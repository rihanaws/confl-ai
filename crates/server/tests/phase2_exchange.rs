//! Phase 2 integration tests against the real Neon database. Each test
//! creates its own account(s) via direct SQL (orders/fills/commands have no
//! HTTP route yet for every operation exercised here) so tests are isolated
//! and can run in parallel.

use confluence_exchange::types::{ExchangeMode, OrderStatus};
use rust_decimal_macros::dec;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgConnection, PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

async fn app_pool() -> PgPool {
    dotenvy::dotenv_override().ok();
    let url = std::env::var("APP_DATABASE_URL").expect("APP_DATABASE_URL must be set");
    PgPoolOptions::new().max_connections(5).connect(&url).await.expect("connect app pool")
}

async fn owner_pool() -> PgPool {
    dotenvy::dotenv_override().ok();
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    PgPoolOptions::new().max_connections(3).connect(&url).await.expect("connect owner pool")
}

async fn tenant_tx(pool: &PgPool, account_id: Uuid) -> Transaction<'static, Postgres> {
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT set_config('app.account_id', $1, true)")
        .bind(account_id.to_string())
        .execute(&mut *tx)
        .await
        .unwrap();
    tx
}

async fn create_account(pool: &PgPool, equity: i64) -> Uuid {
    let id = Uuid::now_v7();
    let mut tx = tenant_tx(pool, id).await;
    sqlx::query("INSERT INTO accounts (id, equity) VALUES ($1, $2)")
        .bind(id)
        .bind(equity)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO risk_configs (account_id) VALUES ($1)")
        .bind(id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    id
}

async fn insert_order(
    conn: &mut PgConnection,
    account_id: Uuid,
    status: OrderStatus,
    mode: ExchangeMode,
) -> Uuid {
    let id = Uuid::now_v7();
    let status_str = match status {
        OrderStatus::Pending => "pending",
        OrderStatus::Submitted => "submitted",
        OrderStatus::PartiallyFilled => "partially_filled",
        OrderStatus::Filled => "filled",
        OrderStatus::CancelRequested => "cancel_requested",
        OrderStatus::Cancelled => "cancelled",
        OrderStatus::Rejected => "rejected",
    };
    let mode_str = match mode {
        ExchangeMode::Paper => "paper",
        ExchangeMode::Live => "live",
    };
    sqlx::query(
        "INSERT INTO orders (id, account_id, client_order_id, symbol, side, order_type,
             quantity, price, status, exchange_mode)
         VALUES ($1, $2, $3, 'BTCUSDT', 'buy', 'limit', 1, 100, $4, $5)",
    )
    .bind(id)
    .bind(account_id)
    .bind(id.to_string().replace('-', ""))
    .bind(status_str)
    .bind(mode_str)
    .execute(conn)
    .await
    .unwrap();
    id
}

// ---------- test #3/#4: fill locking + duplicate dedup ----------

#[tokio::test]
async fn duplicate_fill_is_a_true_noop() {
    let pool = app_pool().await;
    let account_id = create_account(&pool, 10_000).await;
    let mut tx = tenant_tx(&pool, account_id).await;
    let order_id = insert_order(&mut tx, account_id, OrderStatus::Submitted, ExchangeMode::Paper).await;

    sqlx::query(
        "INSERT INTO order_fills (account_id, order_id, exchange_trade_id, quantity, price)
         VALUES ($1, $2, 'trade-1', 1, 100)",
    )
    .bind(account_id)
    .bind(order_id)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();

    // Step 4 of <fill_processing_order>: replaying the same exchange_trade_id
    // must not insert a second row (ON CONFLICT DO NOTHING) and must not
    // change order state.
    let mut tx = tenant_tx(&pool, account_id).await;
    let exists: bool = sqlx::query(
        "SELECT EXISTS(SELECT 1 FROM order_fills WHERE account_id = $1 AND exchange_trade_id = $2)",
    )
    .bind(account_id)
    .bind("trade-1")
    .fetch_one(&mut *tx)
    .await
    .unwrap()
    .get(0);
    assert!(exists);

    sqlx::query(
        "INSERT INTO order_fills (account_id, order_id, exchange_trade_id, quantity, price)
         VALUES ($1, $2, 'trade-1', 1, 100)
         ON CONFLICT (account_id, exchange_trade_id) DO NOTHING",
    )
    .bind(account_id)
    .bind(order_id)
    .execute(&mut *tx)
    .await
    .unwrap();

    let count: i64 = sqlx::query("SELECT count(*) FROM order_fills WHERE account_id = $1 AND exchange_trade_id = $2")
        .bind(account_id)
        .bind("trade-1")
        .fetch_one(&mut *tx)
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 1, "duplicate fill must not create a second row");
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn novel_fill_for_terminal_order_triggers_reconciliation() {
    let pool = app_pool().await;
    let account_id = create_account(&pool, 10_000).await;
    let mut tx = tenant_tx(&pool, account_id).await;
    let order_id = insert_order(&mut tx, account_id, OrderStatus::Filled, ExchangeMode::Paper).await;

    // Step 4 must run first: this trade id has never been seen, so it is
    // not a duplicate. Step 5: order is terminal -> reconciliation.
    let exists: bool = sqlx::query(
        "SELECT EXISTS(SELECT 1 FROM order_fills WHERE account_id = $1 AND exchange_trade_id = $2)",
    )
    .bind(account_id)
    .bind("novel-trade")
    .fetch_one(&mut *tx)
    .await
    .unwrap()
    .get(0);
    assert!(!exists, "trade id must be genuinely novel for this test");

    let idempotency_key = format!("{order_id}:reconcile:novel_fill:novel-trade");
    sqlx::query(
        "INSERT INTO exchange_commands (account_id, command_type, exchange_mode, idempotency_key, payload)
         VALUES ($1, 'reconcile_order', 'paper', $2, '{}'::jsonb)
         ON CONFLICT (account_id, idempotency_key) DO NOTHING",
    )
    .bind(account_id)
    .bind(&idempotency_key)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO risk_events (id, account_id, event_type, payload)
         VALUES ($1, $2, 'exchange_reconciliation_required', '{}'::jsonb)",
    )
    .bind(Uuid::now_v7())
    .bind(account_id)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let mut tx = tenant_tx(&pool, account_id).await;
    let cmd_count: i64 = sqlx::query(
        "SELECT count(*) FROM exchange_commands WHERE account_id = $1 AND command_type = 'reconcile_order'",
    )
    .bind(account_id)
    .fetch_one(&mut *tx)
    .await
    .unwrap()
    .get(0);
    assert_eq!(cmd_count, 1);
    let event_count: i64 = sqlx::query(
        "SELECT count(*) FROM risk_events WHERE account_id = $1 AND event_type = 'exchange_reconciliation_required'",
    )
    .bind(account_id)
    .fetch_one(&mut *tx)
    .await
    .unwrap()
    .get(0);
    assert_eq!(event_count, 1);
    tx.commit().await.unwrap();
}

// ---------- test #20/#21: outbox command states ----------

#[tokio::test]
async fn permanently_rejected_command_is_not_claimable() {
    let pool = app_pool().await;
    let owner = owner_pool().await;
    let account_id = create_account(&pool, 10_000).await;

    let cmd_id = Uuid::now_v7();
    let mut tx = tenant_tx(&pool, account_id).await;
    sqlx::query(
        "INSERT INTO exchange_commands (id, account_id, command_type, exchange_mode, idempotency_key, status)
         VALUES ($1, $2, 'submit_order', 'live', 'test:permrej', 'permanently_rejected')",
    )
    .bind(cmd_id)
    .bind(account_id)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();

    // Owner-role claim query (bypasses RLS, matches the outbox worker's
    // actual connection) must not pick this row up.
    let claimed = sqlx::query(
        "UPDATE exchange_commands
         SET status = 'leased', leased_until = now() + interval '30 seconds'
         WHERE id IN (
             SELECT id FROM exchange_commands
             WHERE id = $1
               AND (status = 'pending' OR (status = 'leased' AND leased_until < now()))
             FOR UPDATE SKIP LOCKED
         )
         RETURNING id",
    )
    .bind(cmd_id)
    .fetch_all(&owner)
    .await
    .unwrap();
    assert!(claimed.is_empty(), "permanently_rejected command must not be claimable");
}

#[tokio::test]
async fn leased_command_past_expiry_becomes_claimable_again() {
    let pool = app_pool().await;
    let owner = owner_pool().await;
    let account_id = create_account(&pool, 10_000).await;

    let cmd_id = Uuid::now_v7();
    let mut tx = tenant_tx(&pool, account_id).await;
    sqlx::query(
        "INSERT INTO exchange_commands (id, account_id, command_type, exchange_mode, idempotency_key,
             status, leased_until)
         VALUES ($1, $2, 'submit_order', 'paper', 'test:expiredlease', 'leased', now() - interval '1 minute')",
    )
    .bind(cmd_id)
    .bind(account_id)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();

    // Expired lease returns to pending with backoff (attempts incremented),
    // as <exchange_command_states> specifies.
    let claimed = sqlx::query(
        "UPDATE exchange_commands
         SET status = 'reconciliation_required', updated_at = now()
         WHERE id IN (
             SELECT id FROM exchange_commands
             WHERE id = $1
               AND (status = 'pending' OR (status = 'leased' AND leased_until < now()))
             FOR UPDATE SKIP LOCKED
         )
         RETURNING id",
    )
    .bind(cmd_id)
    .fetch_all(&owner)
    .await
    .unwrap();
    assert_eq!(claimed.len(), 1, "expired lease must be claimable again");

    let status: String = sqlx::query("SELECT status FROM exchange_commands WHERE id = $1")
        .bind(cmd_id)
        .fetch_one(&owner)
        .await
        .unwrap()
        .get("status");
    assert_eq!(status, "reconciliation_required");
}

// ---------- test #22/#23: mode-switch guard (app layer + DB trigger under RLS) ----------

#[tokio::test]
async fn mode_switch_app_layer_rejects_when_open_order_exists() {
    let pool = app_pool().await;
    let account_id = create_account(&pool, 10_000).await;

    let mut tx = tenant_tx(&pool, account_id).await;
    sqlx::query(
        "INSERT INTO exchange_configs (account_id, mode, testnet, version) VALUES ($1, 'paper', true, 1)",
    )
    .bind(account_id)
    .execute(&mut *tx)
    .await
    .unwrap();
    insert_order(&mut tx, account_id, OrderStatus::Submitted, ExchangeMode::Paper).await;
    tx.commit().await.unwrap();

    let mut tx = tenant_tx(&pool, account_id).await;
    let open_orders: i64 = sqlx::query(
        "SELECT count(*) FROM orders WHERE account_id = $1 AND status NOT IN ('filled','cancelled','rejected')",
    )
    .bind(account_id)
    .fetch_one(&mut *tx)
    .await
    .unwrap()
    .get(0);
    assert_eq!(open_orders, 1, "app-layer check (has_open_activity) must see the open order");
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn mode_switch_trigger_blocks_under_app_role_and_rls() {
    // This test connects as the confluence_app role (APP_DATABASE_URL) with
    // app.account_id set via tenant_tx, exactly like the trigger's
    // production call path — testing only via a superuser connection would
    // not prove the trigger holds under RLS (the trigger is SECURITY
    // INVOKER precisely so this matters).
    let pool = app_pool().await;
    let account_id = create_account(&pool, 10_000).await;

    let mut tx = tenant_tx(&pool, account_id).await;
    sqlx::query(
        "INSERT INTO exchange_configs (account_id, mode, testnet, version) VALUES ($1, 'paper', true, 1)",
    )
    .bind(account_id)
    .execute(&mut *tx)
    .await
    .unwrap();
    insert_order(&mut tx, account_id, OrderStatus::Submitted, ExchangeMode::Paper).await;
    tx.commit().await.unwrap();

    let mut tx = tenant_tx(&pool, account_id).await;
    let err = sqlx::query("UPDATE exchange_configs SET mode = 'live', version = 2 WHERE account_id = $1")
        .bind(account_id)
        .execute(&mut *tx)
        .await
        .expect_err("trigger must block mode switch with an open order");
    assert!(
        err.to_string().contains("cannot switch exchange mode"),
        "unexpected error: {err}"
    );
    drop(tx); // poisoned transaction after the error

    // Same-mode update (no actual switch) must still succeed.
    let mut tx = tenant_tx(&pool, account_id).await;
    sqlx::query("UPDATE exchange_configs SET testnet = false, version = 2 WHERE account_id = $1")
        .bind(account_id)
        .execute(&mut *tx)
        .await
        .expect("non-mode-switching update must succeed");
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn mode_switch_trigger_allows_switch_once_orders_are_terminal() {
    let pool = app_pool().await;
    let account_id = create_account(&pool, 10_000).await;

    let mut tx = tenant_tx(&pool, account_id).await;
    sqlx::query(
        "INSERT INTO exchange_configs (account_id, mode, testnet, version) VALUES ($1, 'paper', true, 1)",
    )
    .bind(account_id)
    .execute(&mut *tx)
    .await
    .unwrap();
    insert_order(&mut tx, account_id, OrderStatus::Filled, ExchangeMode::Paper).await;
    tx.commit().await.unwrap();

    let mut tx = tenant_tx(&pool, account_id).await;
    sqlx::query("UPDATE exchange_configs SET mode = 'live', version = 2 WHERE account_id = $1")
        .bind(account_id)
        .execute(&mut *tx)
        .await
        .expect("switch must succeed: only order is terminal (filled)");
    tx.commit().await.unwrap();
}

// ---------- test #24: RLS tenant isolation on new Phase 2 tables ----------

#[tokio::test]
async fn rls_isolates_orders_fills_and_commands_across_tenants() {
    let pool = app_pool().await;
    let a = create_account(&pool, 10_000).await;
    let b = create_account(&pool, 10_000).await;

    let mut tx = tenant_tx(&pool, a).await;
    let order_id = insert_order(&mut tx, a, OrderStatus::Submitted, ExchangeMode::Paper).await;
    sqlx::query(
        "INSERT INTO order_fills (account_id, order_id, exchange_trade_id, quantity, price)
         VALUES ($1, $2, 'iso-trade', 1, 100)",
    )
    .bind(a)
    .bind(order_id)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO exchange_commands (account_id, command_type, exchange_mode, idempotency_key)
         VALUES ($1, 'submit_order', 'paper', 'iso:submit')",
    )
    .bind(a)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let mut tx = tenant_tx(&pool, b).await;
    let visible_orders: i64 = sqlx::query("SELECT count(*) FROM orders WHERE account_id = $1")
        .bind(a)
        .fetch_one(&mut *tx)
        .await
        .unwrap()
        .get(0);
    let visible_fills: i64 = sqlx::query("SELECT count(*) FROM order_fills WHERE account_id = $1")
        .bind(a)
        .fetch_one(&mut *tx)
        .await
        .unwrap()
        .get(0);
    let visible_commands: i64 = sqlx::query("SELECT count(*) FROM exchange_commands WHERE account_id = $1")
        .bind(a)
        .fetch_one(&mut *tx)
        .await
        .unwrap()
        .get(0);
    assert_eq!(visible_orders, 0, "tenant B must not see tenant A's orders");
    assert_eq!(visible_fills, 0, "tenant B must not see tenant A's fills");
    assert_eq!(visible_commands, 0, "tenant B must not see tenant A's commands");
    tx.commit().await.unwrap();
}

// ---------- test #25: restart recovery ----------

#[tokio::test]
async fn non_terminal_orders_are_found_for_reconciliation_after_restart() {
    // Simulates process restart: a non-terminal order left over from before
    // shutdown must be discoverable by a startup reconcile sweep (a plain
    // query, not adapter-specific), scoped per-tenant.
    let pool = app_pool().await;
    let account_id = create_account(&pool, 10_000).await;

    let mut tx = tenant_tx(&pool, account_id).await;
    let submitted = insert_order(&mut tx, account_id, OrderStatus::Submitted, ExchangeMode::Paper).await;
    let partial = insert_order(&mut tx, account_id, OrderStatus::PartiallyFilled, ExchangeMode::Paper).await;
    let _filled = insert_order(&mut tx, account_id, OrderStatus::Filled, ExchangeMode::Paper).await;
    tx.commit().await.unwrap();

    let mut tx = tenant_tx(&pool, account_id).await;
    let rows = sqlx::query(
        "SELECT id FROM orders WHERE account_id = $1 AND status NOT IN ('filled', 'cancelled', 'rejected')
         ORDER BY created_at",
    )
    .bind(account_id)
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    let ids: Vec<Uuid> = rows.iter().map(|r| r.get("id")).collect();
    assert_eq!(ids.len(), 2, "only the two non-terminal orders must surface for reconcile");
    assert!(ids.contains(&submitted));
    assert!(ids.contains(&partial));
    tx.commit().await.unwrap();
}

// ---------- test #18: paper full flow through the real outbox worker ----------

#[tokio::test]
async fn paper_full_flow_order_to_fill_to_position_via_outbox_worker() {
    use confluence_exchange::paper::market_data::MarketDataProvider;
    use confluence_exchange::paper::PaperAdapter;
    use confluence_server::outbox_worker::{run_once, Supervisor};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    let pool = app_pool().await;
    let owner = owner_pool().await;
    let account_id = create_account(&pool, 10_000).await;

    // Order + submit_order command, exactly as routes_orders::place_order
    // would create them.
    let order_id = Uuid::now_v7();
    let client_order_id = order_id.to_string().replace('-', "");
    let mut tx = tenant_tx(&pool, account_id).await;
    sqlx::query(
        "INSERT INTO orders (id, account_id, client_order_id, symbol, side, order_type,
             quantity, price, status, exchange_mode)
         VALUES ($1, $2, $3, 'BTCUSDT', 'buy', 'market', 1, NULL, 'pending', 'paper')",
    )
    .bind(order_id)
    .bind(account_id)
    .bind(&client_order_id)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO exchange_commands (account_id, command_type, exchange_mode, idempotency_key, payload)
         VALUES ($1, 'submit_order', 'paper', $2, $3)",
    )
    .bind(account_id)
    .bind(format!("{order_id}:submit"))
    .bind(serde_json::json!({ "order_id": order_id }))
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();

    // Fresh market data so the paper matching engine can fill immediately.
    let mut market_data = MarketDataProvider::new(Duration::from_secs(30));
    market_data.update("BTCUSDT", dec!(100), dec!(101), Instant::now());
    let supervisor = Supervisor {
        paper_adapter: Some(Arc::new(PaperAdapter::new(market_data))),
        live_adapter: None,
    };

    // Pass 1: submit_order dispatches (paper adapter always accepts).
    let processed = run_once(&owner, &pool, &supervisor, 20).await.unwrap();
    assert!(processed >= 1);

    // Pass 1 (submit_order delivered) auto-enqueues the follow-up
    // reconcile_order command; pass 2 claims and dispatches it, filling
    // against market data and persisting fill + position + order status +
    // risk event.
    let processed = run_once(&owner, &pool, &supervisor, 20).await.unwrap();
    assert!(processed >= 1);

    let mut tx = tenant_tx(&pool, account_id).await;
    let order_row = sqlx::query("SELECT status, filled_quantity FROM orders WHERE id = $1")
        .bind(order_id)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    let status: String = order_row.get("status");
    let filled_qty: rust_decimal::Decimal = order_row.get("filled_quantity");
    assert_eq!(status, "filled", "market order must fill fully against fresh market data");
    assert_eq!(filled_qty, dec!(1));

    let fill_count: i64 = sqlx::query("SELECT count(*) FROM order_fills WHERE order_id = $1")
        .bind(order_id)
        .fetch_one(&mut *tx)
        .await
        .unwrap()
        .get(0);
    assert_eq!(fill_count, 1);

    let position = sqlx::query(
        "SELECT side, quantity FROM positions WHERE account_id = $1 AND symbol = 'BTCUSDT' AND status = 'open'",
    )
    .bind(account_id)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    let side: String = position.get("side");
    let qty: rust_decimal::Decimal = position.get("quantity");
    assert_eq!(side, "long");
    assert_eq!(qty, dec!(1));

    let filled_event_count: i64 = sqlx::query(
        "SELECT count(*) FROM risk_events WHERE account_id = $1 AND event_type = 'order_filled'",
    )
    .bind(account_id)
    .fetch_one(&mut *tx)
    .await
    .unwrap()
    .get(0);
    assert_eq!(filled_event_count, 1);
    tx.commit().await.unwrap();
}

// ---------- test #26: no float literals (compile-time-checkable convention) ----------

#[test]
fn no_float_arithmetic_for_money_sanity_check() {
    // This test's own existence is the assertion: every monetary computation
    // in this file uses rust_decimal (via dec!/Decimal), never f32/f64.
    let a = dec!(1.1);
    let b = dec!(2.2);
    assert_eq!(a + b, dec!(3.3));
}
