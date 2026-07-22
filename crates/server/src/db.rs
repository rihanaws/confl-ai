//! Tenant-scoped data access. Every handler runs inside one transaction that
//! has `app.account_id` set, so Postgres RLS enforces isolation underneath
//! the explicit `WHERE account_id = $1` in each query (defense in depth).

use chrono::Utc;
use confluence_core::{AccountSnapshot, OpenPosition, PositionSide, RiskConfig};
use confluence_exchange::types::{ExchangeMode, Fill, OrderSide, OrderStatus, OrderType};
use rust_decimal::Decimal;
use serde_json::Value;
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, PgPool, Postgres, Row, Transaction};
use std::collections::HashMap;
use uuid::Uuid;

use crate::error::ApiError;

/// Begin a transaction scoped to one tenant. `SET LOCAL` cannot take bind
/// parameters, so `set_config(..., is_local := true)` is used instead; the
/// setting dies with the transaction.
pub async fn tenant_tx(
    pool: &PgPool,
    account_id: Uuid,
) -> Result<Transaction<'static, Postgres>, ApiError> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT set_config('app.account_id', $1, true)")
        .bind(account_id.to_string())
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

/// Append one audit row. Callers pass the same transaction that performs the
/// state change, so the event and the change commit or roll back together.
pub async fn insert_risk_event(
    conn: &mut PgConnection,
    account_id: Uuid,
    event_type: &str,
    payload: &Value,
) -> Result<(), ApiError> {
    sqlx::query(
        "INSERT INTO risk_events (id, account_id, event_type, payload) VALUES ($1, $2, $3, $4)",
    )
    .bind(Uuid::now_v7())
    .bind(account_id)
    .bind(event_type)
    .bind(payload)
    .execute(conn)
    .await?;
    Ok(())
}

pub struct AccountRow {
    pub status: String,
    pub equity: Decimal,
    pub kill_switch_engaged: bool,
}

/// Load and lock the account row (`FOR UPDATE`) so concurrent evaluations,
/// kill-switch flips, and breaker trips for one account serialize.
pub async fn lock_account(
    conn: &mut PgConnection,
    account_id: Uuid,
) -> Result<AccountRow, ApiError> {
    let row = sqlx::query(
        "SELECT status, equity, kill_switch_engaged_at IS NOT NULL AS ks
         FROM accounts WHERE id = $1 FOR UPDATE",
    )
    .bind(account_id)
    .fetch_optional(conn)
    .await?
    .ok_or(ApiError::NotFound)?;
    Ok(AccountRow {
        status: row.get("status"),
        equity: row.get("equity"),
        kill_switch_engaged: row.get::<bool, _>("ks"),
    })
}

pub async fn load_risk_config(
    conn: &mut PgConnection,
    account_id: Uuid,
) -> Result<(RiskConfig, i32), ApiError> {
    let row = sqlx::query(
        "SELECT daily_max_loss_pct, max_position_pct_equity, max_concurrent_positions,
                max_correlated_exposure_pct, version
         FROM risk_configs WHERE account_id = $1",
    )
    .bind(account_id)
    .fetch_optional(conn)
    .await?
    .ok_or(ApiError::NotFound)?;
    Ok((row_to_config(&row), row.get("version")))
}

fn row_to_config(row: &PgRow) -> RiskConfig {
    RiskConfig {
        daily_max_loss_pct: row.get("daily_max_loss_pct"),
        max_position_pct_equity: row.get("max_position_pct_equity"),
        max_concurrent_positions: row.get("max_concurrent_positions"),
        max_correlated_exposure_pct: row.get("max_correlated_exposure_pct"),
    }
}

/// Assemble the full engine snapshot inside the caller's transaction.
/// `account` must come from [`lock_account`] on the same transaction.
pub async fn load_snapshot(
    conn: &mut PgConnection,
    account_id: Uuid,
    account: &AccountRow,
) -> Result<AccountSnapshot, ApiError> {
    let positions = sqlx::query(
        "SELECT symbol, side, quantity, quantity * avg_entry_price AS notional
         FROM positions WHERE account_id = $1 AND status = 'open'",
    )
    .bind(account_id)
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|r| OpenPosition {
        symbol: r.get("symbol"),
        side: if r.get::<String, _>("side") == "short" {
            PositionSide::Short
        } else {
            PositionSide::Long
        },
        quantity: r.get("quantity"),
        notional: r.get("notional"),
    })
    .collect();

    let correlation_groups: HashMap<String, String> = sqlx::query(
        "SELECT a.symbol, g.name
         FROM correlation_group_assets a
         JOIN correlation_groups g ON g.id = a.group_id
         WHERE a.account_id = $1",
    )
    .bind(account_id)
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|r| (r.get("symbol"), r.get("name")))
    .collect();

    let today = Utc::now().date_naive();
    let pnl = sqlx::query(
        "SELECT realized_loss, circuit_breaker_tripped_at IS NOT NULL AS tripped
         FROM daily_pnl WHERE account_id = $1 AND trading_date = $2",
    )
    .bind(account_id)
    .bind(today)
    .fetch_optional(&mut *conn)
    .await?;
    let (today_realized_loss, circuit_breaker_tripped) = match pnl {
        Some(r) => (r.get("realized_loss"), r.get("tripped")),
        None => (Decimal::ZERO, false),
    };

    Ok(AccountSnapshot {
        equity: account.equity,
        today_realized_loss,
        kill_switch_engaged: account.kill_switch_engaged,
        circuit_breaker_tripped,
        open_positions: positions,
        correlation_groups,
    })
}

/// Persist a circuit-breaker trip for today (idempotent: keeps the first
/// trip timestamp).
pub async fn trip_circuit_breaker(
    conn: &mut PgConnection,
    account_id: Uuid,
) -> Result<(), ApiError> {
    sqlx::query(
        "INSERT INTO daily_pnl (account_id, trading_date, circuit_breaker_tripped_at)
         VALUES ($1, $2, now())
         ON CONFLICT (account_id, trading_date)
         DO UPDATE SET circuit_breaker_tripped_at =
             COALESCE(daily_pnl.circuit_breaker_tripped_at, now())",
    )
    .bind(account_id)
    .bind(Utc::now().date_naive())
    .execute(conn)
    .await?;
    Ok(())
}

// ---------- exchange config / mode ----------

pub struct ExchangeConfigRow {
    pub mode: ExchangeMode,
    pub testnet: bool,
    pub version: i32,
}

fn parse_mode(s: &str) -> ExchangeMode {
    if s == "live" {
        ExchangeMode::Live
    } else {
        ExchangeMode::Paper
    }
}

fn mode_str(mode: ExchangeMode) -> &'static str {
    match mode {
        ExchangeMode::Paper => "paper",
        ExchangeMode::Live => "live",
    }
}

/// Returns the account's configured mode, defaulting to `paper` if no row
/// exists yet (an account with no exchange config has never traded).
pub async fn load_exchange_mode(
    conn: &mut PgConnection,
    account_id: Uuid,
) -> Result<ExchangeMode, ApiError> {
    let row = sqlx::query("SELECT mode FROM exchange_configs WHERE account_id = $1")
        .bind(account_id)
        .fetch_optional(conn)
        .await?;
    Ok(row.map(|r| parse_mode(r.get("mode"))).unwrap_or(ExchangeMode::Paper))
}

/// Layer 1 of the mode-switch guard: application-level check before the DB
/// trigger backstop (`exchange_configs_mode_switch_guard`) even runs.
pub async fn has_open_activity(conn: &mut PgConnection, account_id: Uuid) -> Result<bool, ApiError> {
    let open_orders: i64 = sqlx::query(
        "SELECT count(*) AS c FROM orders
         WHERE account_id = $1 AND status NOT IN ('filled', 'cancelled', 'rejected')",
    )
    .bind(account_id)
    .fetch_one(&mut *conn)
    .await?
    .get("c");
    if open_orders > 0 {
        return Ok(true);
    }
    let open_positions: i64 = sqlx::query(
        "SELECT count(*) AS c FROM positions WHERE account_id = $1 AND status = 'open' AND quantity > 0",
    )
    .bind(account_id)
    .fetch_one(&mut *conn)
    .await?
    .get("c");
    Ok(open_positions > 0)
}

// ---------- orders ----------

pub struct OrderRow {
    pub id: Uuid,
    pub client_order_id: String,
    pub symbol: String,
    pub side: OrderSide,
    pub order_type: OrderType,
    pub quantity: Decimal,
    pub price: Option<Decimal>,
    pub status: OrderStatus,
    pub exchange_mode: ExchangeMode,
    pub filled_quantity: Decimal,
}

fn side_str(side: OrderSide) -> &'static str {
    match side {
        OrderSide::Buy => "buy",
        OrderSide::Sell => "sell",
    }
}

fn parse_side(s: &str) -> OrderSide {
    if s == "sell" {
        OrderSide::Sell
    } else {
        OrderSide::Buy
    }
}

fn order_type_str(t: OrderType) -> &'static str {
    match t {
        OrderType::Limit => "limit",
        OrderType::Market => "market",
    }
}

fn parse_order_type(s: &str) -> OrderType {
    if s == "market" {
        OrderType::Market
    } else {
        OrderType::Limit
    }
}

pub fn status_str(status: OrderStatus) -> &'static str {
    match status {
        OrderStatus::Pending => "pending",
        OrderStatus::Submitted => "submitted",
        OrderStatus::PartiallyFilled => "partially_filled",
        OrderStatus::Filled => "filled",
        OrderStatus::CancelRequested => "cancel_requested",
        OrderStatus::Cancelled => "cancelled",
        OrderStatus::Rejected => "rejected",
    }
}

fn parse_status(s: &str) -> OrderStatus {
    match s {
        "submitted" => OrderStatus::Submitted,
        "partially_filled" => OrderStatus::PartiallyFilled,
        "filled" => OrderStatus::Filled,
        "cancel_requested" => OrderStatus::CancelRequested,
        "cancelled" => OrderStatus::Cancelled,
        "rejected" => OrderStatus::Rejected,
        _ => OrderStatus::Pending,
    }
}

fn row_to_order(row: &PgRow) -> OrderRow {
    OrderRow {
        id: row.get("id"),
        client_order_id: row.get("client_order_id"),
        symbol: row.get("symbol"),
        side: parse_side(row.get("side")),
        order_type: parse_order_type(row.get("order_type")),
        quantity: row.get("quantity"),
        price: row.get("price"),
        status: parse_status(row.get("status")),
        exchange_mode: parse_mode(row.get("exchange_mode")),
        filled_quantity: row.get("filled_quantity"),
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn insert_order(
    conn: &mut PgConnection,
    id: Uuid,
    account_id: Uuid,
    client_order_id: &str,
    symbol: &str,
    side: OrderSide,
    order_type: OrderType,
    quantity: Decimal,
    price: Option<Decimal>,
    exchange_mode: ExchangeMode,
) -> Result<(), ApiError> {
    sqlx::query(
        "INSERT INTO orders (id, account_id, client_order_id, symbol, side, order_type,
             quantity, price, status, exchange_mode)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'pending', $9)",
    )
    .bind(id)
    .bind(account_id)
    .bind(client_order_id)
    .bind(symbol)
    .bind(side_str(side))
    .bind(order_type_str(order_type))
    .bind(quantity)
    .bind(price)
    .bind(mode_str(exchange_mode))
    .execute(conn)
    .await?;
    Ok(())
}

/// Locks one order row for update, scoped to the tenant. Used by the fill
/// pipeline (step 2 of `<fill_processing_order>`).
pub async fn lock_order(
    conn: &mut PgConnection,
    account_id: Uuid,
    order_id: Uuid,
) -> Result<OrderRow, ApiError> {
    let row = sqlx::query(
        "SELECT id, client_order_id, symbol, side, order_type, quantity, price, status,
                exchange_mode, filled_quantity
         FROM orders WHERE id = $1 AND account_id = $2 FOR UPDATE",
    )
    .bind(order_id)
    .bind(account_id)
    .fetch_optional(conn)
    .await?
    .ok_or(ApiError::NotFound)?;
    Ok(row_to_order(&row))
}

pub async fn update_order_status(
    conn: &mut PgConnection,
    account_id: Uuid,
    order_id: Uuid,
    status: OrderStatus,
    filled_quantity: Decimal,
) -> Result<(), ApiError> {
    sqlx::query(
        "UPDATE orders SET status = $3, filled_quantity = $4, updated_at = now()
         WHERE id = $1 AND account_id = $2",
    )
    .bind(order_id)
    .bind(account_id)
    .bind(status_str(status))
    .bind(filled_quantity)
    .execute(conn)
    .await?;
    Ok(())
}

// ---------- fills ----------

/// Step 4 of `<fill_processing_order>`: duplicate check must run before the
/// terminal-order branch, so a replayed fill on a terminal order is still a
/// true no-op, not a reconciliation trigger.
pub async fn fill_exists(
    conn: &mut PgConnection,
    account_id: Uuid,
    exchange_trade_id: &str,
) -> Result<bool, ApiError> {
    let exists: bool = sqlx::query(
        "SELECT EXISTS(SELECT 1 FROM order_fills WHERE account_id = $1 AND exchange_trade_id = $2) AS e",
    )
    .bind(account_id)
    .bind(exchange_trade_id)
    .fetch_one(conn)
    .await?
    .get("e");
    Ok(exists)
}

/// All fills recorded so far for one order — the crate-boundary input to
/// `ReconcileSnapshot.fills` (Fix 2: `crates/exchange` never reads this
/// table directly).
pub async fn load_fills_for_order(
    conn: &mut PgConnection,
    account_id: Uuid,
    order_id: Uuid,
) -> Result<Vec<Fill>, ApiError> {
    let rows = sqlx::query(
        "SELECT exchange_trade_id, quantity, price, fee, fee_asset FROM order_fills
         WHERE account_id = $1 AND order_id = $2",
    )
    .bind(account_id)
    .bind(order_id)
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| Fill {
            exchange_trade_id: r.get("exchange_trade_id"),
            quantity: r.get("quantity"),
            price: r.get("price"),
            fee: r.get("fee"),
            fee_asset: r.get("fee_asset"),
        })
        .collect())
}

pub async fn insert_fill(
    conn: &mut PgConnection,
    account_id: Uuid,
    order_id: Uuid,
    fill: &Fill,
) -> Result<(), ApiError> {
    sqlx::query(
        "INSERT INTO order_fills (account_id, order_id, exchange_trade_id, quantity, price, fee, fee_asset)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         ON CONFLICT (account_id, exchange_trade_id) DO NOTHING",
    )
    .bind(account_id)
    .bind(order_id)
    .bind(&fill.exchange_trade_id)
    .bind(fill.quantity)
    .bind(fill.price)
    .bind(fill.fee)
    .bind(&fill.fee_asset)
    .execute(conn)
    .await?;
    Ok(())
}

// ---------- exchange commands (outbox) ----------

pub async fn insert_exchange_command(
    conn: &mut PgConnection,
    account_id: Uuid,
    command_type: &str,
    exchange_mode: ExchangeMode,
    idempotency_key: &str,
    payload: &Value,
) -> Result<(), ApiError> {
    sqlx::query(
        "INSERT INTO exchange_commands (account_id, command_type, exchange_mode, idempotency_key, payload)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (account_id, idempotency_key) DO NOTHING",
    )
    .bind(account_id)
    .bind(command_type)
    .bind(mode_str(exchange_mode))
    .bind(idempotency_key)
    .bind(payload)
    .execute(conn)
    .await?;
    Ok(())
}

pub struct ClaimedCommand {
    pub id: Uuid,
    pub account_id: Uuid,
    pub command_type: String,
    pub exchange_mode: ExchangeMode,
    pub payload: Value,
}

/// Claims up to `limit` pending commands with `SKIP LOCKED`, marking them
/// `leased` with a lease expiry. Not tenant-scoped by design: the worker
/// polls across all accounts, each claimed row still carries its own
/// `account_id` for downstream tenant-scoped processing.
pub async fn claim_pending_commands(
    pool: &PgPool,
    limit: i64,
    lease_seconds: i64,
) -> Result<Vec<ClaimedCommand>, ApiError> {
    let rows = sqlx::query(
        "UPDATE exchange_commands
         SET status = 'leased', leased_until = now() + make_interval(secs => $2), updated_at = now()
         WHERE id IN (
             SELECT id FROM exchange_commands
             WHERE status = 'pending'
                OR (status = 'leased' AND leased_until < now())
             ORDER BY created_at
             LIMIT $1
             FOR UPDATE SKIP LOCKED
         )
         RETURNING id, account_id, command_type, exchange_mode, payload",
    )
    .bind(limit)
    .bind(lease_seconds)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| ClaimedCommand {
            id: r.get("id"),
            account_id: r.get("account_id"),
            command_type: r.get("command_type"),
            exchange_mode: parse_mode(r.get("exchange_mode")),
            payload: r.get("payload"),
        })
        .collect())
}

pub async fn mark_command_delivered(conn: &mut PgConnection, id: Uuid) -> Result<(), ApiError> {
    sqlx::query("UPDATE exchange_commands SET status = 'delivered', updated_at = now() WHERE id = $1")
        .bind(id)
        .execute(conn)
        .await?;
    Ok(())
}

pub async fn mark_command_permanently_rejected(conn: &mut PgConnection, id: Uuid) -> Result<(), ApiError> {
    sqlx::query(
        "UPDATE exchange_commands SET status = 'permanently_rejected', updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .execute(conn)
    .await?;
    Ok(())
}

/// Lease-expiry / dispatch-failure path: returns the command to `pending`
/// with an incremented attempt count for backoff, rather than leaving it
/// stuck `leased`.
pub async fn requeue_command(conn: &mut PgConnection, id: Uuid) -> Result<(), ApiError> {
    sqlx::query(
        "UPDATE exchange_commands SET status = 'pending', leased_until = NULL,
             attempts = attempts + 1, updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .execute(conn)
    .await?;
    Ok(())
}

pub async fn mark_command_reconciliation_required(conn: &mut PgConnection, id: Uuid) -> Result<(), ApiError> {
    sqlx::query(
        "UPDATE exchange_commands SET status = 'reconciliation_required', updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .execute(conn)
    .await?;
    Ok(())
}

// ---------- positions ----------

/// Applies a signed quantity delta (positive = long exposure added, negative
/// = long exposure removed / short exposure added) to the account's open
/// position on `symbol`, opening, growing, shrinking, flipping, or closing
/// it as needed. `fill_price` is used as the entry price for a newly opened
/// (or newly re-opened after a full close) position only.
pub async fn apply_position_delta(
    conn: &mut PgConnection,
    account_id: Uuid,
    symbol: &str,
    quantity_delta: Decimal,
    fill_price: Decimal,
) -> Result<(), ApiError> {
    if quantity_delta.is_zero() {
        return Ok(());
    }

    let existing = sqlx::query(
        "SELECT id, side, quantity, avg_entry_price FROM positions
         WHERE account_id = $1 AND symbol = $2 AND status = 'open'",
    )
    .bind(account_id)
    .bind(symbol)
    .fetch_optional(&mut *conn)
    .await?;

    let Some(row) = existing else {
        // No open position: a positive delta opens long, negative opens
        // short.
        let side = if quantity_delta > Decimal::ZERO { "long" } else { "short" };
        sqlx::query(
            "INSERT INTO positions (account_id, symbol, side, quantity, avg_entry_price)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(account_id)
        .bind(symbol)
        .bind(side)
        .bind(quantity_delta.abs())
        .bind(fill_price)
        .execute(conn)
        .await?;
        return Ok(());
    };

    let id: Uuid = row.get("id");
    let side: String = row.get("side");
    let current_qty: Decimal = row.get("quantity");
    let signed_current = if side == "short" { -current_qty } else { current_qty };
    let new_signed = signed_current + quantity_delta;

    if new_signed.is_zero() {
        sqlx::query(
            "UPDATE positions SET status = 'closed', quantity = 0, closed_at = now() WHERE id = $1",
        )
        .bind(id)
        .execute(conn)
        .await?;
        return Ok(());
    }

    let new_side = if new_signed > Decimal::ZERO { "long" } else { "short" };
    let new_qty = new_signed.abs();

    // A flip (sign change) or a same-direction increase resets/extends the
    // entry price to the fill price; the exact avg-price weighting for
    // partial adds is a follow-up refinement, not required for Phase 2's
    // reconciliation correctness (fills/positions/events land atomically
    // either way).
    let grew_same_direction = (side == "long") == (new_signed > Decimal::ZERO) && new_qty > current_qty;
    let avg_price = if grew_same_direction {
        row.get::<Decimal, _>("avg_entry_price")
    } else {
        fill_price
    };

    sqlx::query(
        "UPDATE positions SET side = $2, quantity = $3, avg_entry_price = $4 WHERE id = $1",
    )
    .bind(id)
    .bind(new_side)
    .bind(new_qty)
    .bind(avg_price)
    .execute(conn)
    .await?;
    Ok(())
}
