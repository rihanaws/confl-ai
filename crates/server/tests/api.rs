//! Integration tests against the real Neon database (APP_DATABASE_URL from
//! the project .env). Each test creates its own account, so tests are
//! isolated and can run in parallel.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

async fn test_pool() -> PgPool {
    dotenvy::dotenv_override().ok();
    let url = std::env::var("APP_DATABASE_URL").expect("APP_DATABASE_URL must be set");
    PgPoolOptions::new()
        .max_connections(3)
        .connect(&url)
        .await
        .expect("connect to test database")
}

async fn call(app: Router, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let req = match body {
        Some(b) => Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from(b.to_string()))
            .unwrap(),
        None => Request::builder()
            .method(method)
            .uri(path)
            .body(Body::empty())
            .unwrap(),
    };
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

async fn create_account(pool: &PgPool, equity: i64) -> Uuid {
    let (status, body) = call(
        confluence_server::app(pool.clone()),
        "POST",
        "/accounts",
        Some(json!({ "equity": equity })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create account: {body}");
    Uuid::parse_str(body["id"].as_str().unwrap()).unwrap()
}

async fn evaluate(pool: &PgPool, id: Uuid, intent: Value) -> (StatusCode, Value) {
    call(
        confluence_server::app(pool.clone()),
        "POST",
        &format!("/accounts/{id}/evaluate"),
        Some(intent),
    )
    .await
}

fn buy(symbol: &str, qty: &str, price: &str) -> Value {
    json!({ "symbol": symbol, "side": "buy", "quantity": qty, "price": price })
}

/// Insert an open position as the tenant (grants allow it; RLS applies).
async fn insert_position(pool: &PgPool, id: Uuid, symbol: &str, qty: &str, price: &str) {
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT set_config('app.account_id', $1, true)")
        .bind(id.to_string())
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO positions (account_id, symbol, side, quantity, avg_entry_price)
         VALUES ($1, $2, 'long', $3::numeric, $4::numeric)",
    )
    .bind(id)
    .bind(symbol)
    .bind(qty)
    .bind(price)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

fn violations(body: &Value) -> Vec<String> {
    body["decision"]["violations"]
        .as_array()
        .map(|vs| {
            vs.iter()
                .map(|v| v["rule"].as_str().unwrap_or("?").to_string())
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn lifecycle_defaults_config_update_and_evaluate() {
    let pool = test_pool().await;
    let id = create_account(&pool, 10_000).await;

    // Default config (schema placeholders), version 1.
    let (status, cfg) = call(
        confluence_server::app(pool.clone()),
        "GET",
        &format!("/accounts/{id}/risk-config"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cfg["version"], 1);
    assert_eq!(cfg["max_concurrent_positions"], 5);

    // Small trade approved.
    let (status, body) = evaluate(&pool, id, buy("BTC", "1", "100")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["decision"]["decision"], "approved");

    // Oversized trade (default max_position_pct_equity = 5% of 10_000 = 500).
    let (_, body) = evaluate(&pool, id, buy("BTC", "1", "501")).await;
    assert_eq!(body["decision"]["decision"], "rejected");
    assert_eq!(violations(&body), vec!["position_size_exceeded"]);

    // Version-checked config update.
    let new_cfg = json!({
        "daily_max_loss_pct": "2.0",
        "max_position_pct_equity": "10",
        "max_concurrent_positions": 3,
        "max_correlated_exposure_pct": "20",
        "version": 1
    });
    let (status, updated) = call(
        confluence_server::app(pool.clone()),
        "PUT",
        &format!("/accounts/{id}/risk-config"),
        Some(new_cfg.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["version"], 2);

    // Stale version -> conflict.
    let (status, _) = call(
        confluence_server::app(pool.clone()),
        "PUT",
        &format!("/accounts/{id}/risk-config"),
        Some(new_cfg),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // 1_000 (10% of 10_000) now allowed at the new limit.
    let (_, body) = evaluate(&pool, id, buy("BTC", "10", "100")).await;
    assert_eq!(body["decision"]["decision"], "approved", "{body}");

    // Invalid config values rejected at the boundary.
    let (status, _) = call(
        confluence_server::app(pool.clone()),
        "PUT",
        &format!("/accounts/{id}/risk-config"),
        Some(json!({
            "daily_max_loss_pct": "0",
            "max_position_pct_equity": "10",
            "max_concurrent_positions": 3,
            "max_correlated_exposure_pct": "20",
            "version": 2
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn kill_switch_engage_release_and_audit_trail() {
    let pool = test_pool().await;
    let id = create_account(&pool, 10_000).await;

    let (status, body) = call(
        confluence_server::app(pool.clone()),
        "POST",
        &format!("/accounts/{id}/kill-switch"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["kill_switch_engaged"], true);
    assert_eq!(body["status"], "killed");

    // Everything rejected, kill switch as the sole violation.
    let (_, body) = evaluate(&pool, id, buy("BTC", "1", "1")).await;
    assert_eq!(body["decision"]["decision"], "rejected");
    assert_eq!(violations(&body), vec!["kill_switch_engaged"]);

    // Idempotent re-engage.
    let (status, _) = call(
        confluence_server::app(pool.clone()),
        "POST",
        &format!("/accounts/{id}/kill-switch"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Release restores trading.
    let (status, body) = call(
        confluence_server::app(pool.clone()),
        "DELETE",
        &format!("/accounts/{id}/kill-switch"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["kill_switch_engaged"], false);
    let (_, body) = evaluate(&pool, id, buy("BTC", "1", "1")).await;
    assert_eq!(body["decision"]["decision"], "approved");

    // Audit trail: engage + release + evaluations all present, exactly once each.
    let (_, events) = call(
        confluence_server::app(pool.clone()),
        "GET",
        &format!("/accounts/{id}/risk-events"),
        None,
    )
    .await;
    let types: Vec<&str> = events
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["event_type"].as_str().unwrap())
        .collect();
    assert_eq!(
        types.iter().filter(|t| **t == "kill_switch_engaged").count(),
        1,
        "idempotent engage must log once: {types:?}"
    );
    assert_eq!(types.iter().filter(|t| **t == "kill_switch_released").count(), 1);
    assert_eq!(types.iter().filter(|t| **t == "trade_evaluated").count(), 2);
    assert_eq!(types.iter().filter(|t| **t == "account_created").count(), 1);
}

#[tokio::test]
async fn circuit_breaker_trips_and_persists() {
    let pool = test_pool().await;
    let id = create_account(&pool, 10_000).await;

    // Book a realized loss at the default daily limit (2% of 10_000 = 200).
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT set_config('app.account_id', $1, true)")
        .bind(id.to_string())
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO daily_pnl (account_id, trading_date, realized_loss)
         VALUES ($1, (now() AT TIME ZONE 'utc')::date, 200)",
    )
    .bind(id)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let (_, body) = evaluate(&pool, id, buy("BTC", "1", "1")).await;
    assert_eq!(body["decision"]["decision"], "rejected");
    assert_eq!(violations(&body), vec!["circuit_breaker_tripped"]);

    // Trip persisted: tripped_at set, audit event written once.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT set_config('app.account_id', $1, true)")
        .bind(id.to_string())
        .execute(&mut *tx)
        .await
        .unwrap();
    let tripped: bool = sqlx::query_scalar(
        "SELECT circuit_breaker_tripped_at IS NOT NULL FROM daily_pnl
         WHERE account_id = $1 AND trading_date = (now() AT TIME ZONE 'utc')::date",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert!(tripped, "circuit_breaker_tripped_at must be persisted");
    tx.commit().await.unwrap();

    // Still halted on the next evaluation; trip event logged exactly once.
    let (_, body) = evaluate(&pool, id, buy("ETH", "1", "1")).await;
    assert_eq!(body["decision"]["decision"], "rejected");
    let (_, events) = call(
        confluence_server::app(pool.clone()),
        "GET",
        &format!("/accounts/{id}/risk-events"),
        None,
    )
    .await;
    let trip_count = events
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["event_type"] == "circuit_breaker_tripped")
        .count();
    assert_eq!(trip_count, 1);
}

#[tokio::test]
async fn concurrent_positions_and_correlated_exposure() {
    let pool = test_pool().await;
    let id = create_account(&pool, 100_000).await;

    for (i, sym) in ["AAA", "BBB", "CCC", "DDD", "EEE"].iter().enumerate() {
        insert_position(&pool, id, sym, "1", &format!("{}", 100 * (i + 1))).await;
    }
    // Default cap: 5 concurrent. A 6th symbol must be rejected.
    let (_, body) = evaluate(&pool, id, buy("FFF", "1", "100")).await;
    assert_eq!(body["decision"]["decision"], "rejected", "{body}");
    assert_eq!(violations(&body), vec!["max_concurrent_positions_exceeded"]);

    // Adding to an existing symbol is not a new position.
    let (_, body) = evaluate(&pool, id, buy("AAA", "1", "100")).await;
    assert_eq!(body["decision"]["decision"], "approved", "{body}");

    // Raise the per-trade cap so the group cap is the binding constraint.
    let (status, cfg) = call(
        confluence_server::app(pool.clone()),
        "PUT",
        &format!("/accounts/{id}/risk-config"),
        Some(json!({
            "daily_max_loss_pct": "2.0",
            "max_position_pct_equity": "15",
            "max_concurrent_positions": 5,
            "max_correlated_exposure_pct": "10",
            "version": 1
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{cfg}");

    // Correlated exposure: AAA+BBB grouped; group cap 10% of 100_000 = 10_000.
    let (status, _) = call(
        confluence_server::app(pool.clone()),
        "PUT",
        &format!("/accounts/{id}/correlation-groups"),
        Some(json!([{ "name": "pair", "symbols": ["AAA", "BBB"] }])),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // Existing group notional: 100 + 200 = 300. 9_700 more is fine (<= 10_000)…
    let (_, body) = evaluate(&pool, id, buy("AAA", "97", "100")).await;
    assert_eq!(body["decision"]["decision"], "approved", "{body}");
    // …but 9_701 breaches the group cap (and only that).
    let (_, body) = evaluate(&pool, id, buy("AAA", "97.01", "100")).await;
    assert_eq!(body["decision"]["decision"], "rejected", "{body}");
    assert_eq!(violations(&body), vec!["correlated_exposure_exceeded"]);
}

#[tokio::test]
async fn rls_blocks_cross_tenant_reads_and_audit_is_append_only() {
    let pool = test_pool().await;
    let a = create_account(&pool, 1_000).await;
    let b = create_account(&pool, 1_000).await;

    // Tenant B's transaction cannot see tenant A's rows at all.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT set_config('app.account_id', $1, true)")
        .bind(b.to_string())
        .execute(&mut *tx)
        .await
        .unwrap();
    let visible: i64 =
        sqlx::query_scalar("SELECT count(*) FROM accounts WHERE id = $1")
            .bind(a)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    assert_eq!(visible, 0, "tenant B must not see tenant A's account");
    let visible_events: i64 =
        sqlx::query_scalar("SELECT count(*) FROM risk_events WHERE account_id = $1")
            .bind(a)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    assert_eq!(visible_events, 0);
    tx.commit().await.unwrap();

    // The app role cannot mutate the audit log even for its own tenant.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT set_config('app.account_id', $1, true)")
        .bind(a.to_string())
        .execute(&mut *tx)
        .await
        .unwrap();
    let err = sqlx::query("UPDATE risk_events SET payload = '{}' WHERE account_id = $1")
        .bind(a)
        .execute(&mut *tx)
        .await
        .expect_err("audit update must fail");
    assert!(
        err.to_string().contains("permission denied"),
        "unexpected error: {err}"
    );
    drop(tx); // poisoned transaction

    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT set_config('app.account_id', $1, true)")
        .bind(a.to_string())
        .execute(&mut *tx)
        .await
        .unwrap();
    let err = sqlx::query("DELETE FROM risk_events WHERE account_id = $1")
        .bind(a)
        .execute(&mut *tx)
        .await
        .expect_err("audit delete must fail");
    assert!(err.to_string().contains("permission denied"));
}

#[tokio::test]
async fn evaluate_rejects_malformed_intents_at_the_boundary() {
    let pool = test_pool().await;
    let id = create_account(&pool, 10_000).await;

    let (status, _) = evaluate(&pool, id, buy("", "1", "1")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _) = evaluate(&pool, id, buy("BTC", "-1", "1")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _) = evaluate(&pool, id, buy("BTC", "1", "-1")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Unknown account -> 404, not a silent approval.
    let ghost = Uuid::now_v7();
    let (status, _) = evaluate(&pool, ghost, buy("BTC", "1", "1")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn equity_updates_are_validated_and_audited() {
    let pool = test_pool().await;
    let id = create_account(&pool, 1_000).await;

    let (status, _) = call(
        confluence_server::app(pool.clone()),
        "PUT",
        &format!("/accounts/{id}/equity"),
        Some(json!({ "equity": "-5000" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let (status, _) = call(
        confluence_server::app(pool.clone()),
        "PUT",
        &format!("/accounts/{id}/equity"),
        Some(json!({ "equity": "2500" })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, account) = call(
        confluence_server::app(pool.clone()),
        "GET",
        &format!("/accounts/{id}"),
        None,
    )
    .await;
    assert_eq!(account["equity"].as_str().unwrap(), "2500.0000000000");

    let (_, events) = call(
        confluence_server::app(pool.clone()),
        "GET",
        &format!("/accounts/{id}/risk-events"),
        None,
    )
    .await;
    let equity_events: Vec<&Value> = events
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["event_type"] == "equity_changed")
        .collect();
    assert_eq!(equity_events.len(), 1, "rejected update must not be audited");
    assert_eq!(equity_events[0]["payload"]["new"], "2500");
}
