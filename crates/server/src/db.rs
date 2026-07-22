//! Tenant-scoped data access. Every handler runs inside one transaction that
//! has `app.account_id` set, so Postgres RLS enforces isolation underneath
//! the explicit `WHERE account_id = $1` in each query (defense in depth).

use chrono::Utc;
use confluence_core::{AccountSnapshot, OpenPosition, PositionSide, RiskConfig};
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
