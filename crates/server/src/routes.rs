use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::{Json, Router};
use axum::routing::{get, post};
use chrono::{DateTime, Utc};
use confluence_core::{circuit_breaker_should_trip, evaluate, Decision, TradeIntent};
use confluence_exchange::paper::market_data::MarketDataProvider;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{PgPool, Row};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

use crate::db;
use crate::error::ApiError;
use crate::symbol_metadata::SymbolMetadataStore;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub symbols: Arc<SymbolMetadataStore>,
    /// Shared with whatever `PaperAdapter` the outbox worker's supervisor
    /// uses, via `PaperAdapter::with_shared_market_data` — same feed backs
    /// both this route's pre-trade estimate and the adapter's fills.
    pub market_data: Arc<Mutex<MarketDataProvider>>,
}

pub fn app(pool: PgPool) -> Router {
    let symbols = Arc::new(SymbolMetadataStore::new(std::time::Duration::from_secs(60)));
    let market_data = Arc::new(Mutex::new(MarketDataProvider::new(std::time::Duration::from_secs(10))));
    app_with_state(AppState { pool, symbols, market_data })
}

pub fn app_with_state(state: AppState) -> Router {
    Router::new()
        .route("/accounts", post(create_account))
        .route("/accounts/{id}", get(get_account))
        .route("/accounts/{id}/equity", axum::routing::put(put_equity))
        .route(
            "/accounts/{id}/risk-config",
            get(get_risk_config).put(put_risk_config),
        )
        .route(
            "/accounts/{id}/correlation-groups",
            axum::routing::put(put_correlation_groups),
        )
        .route(
            "/accounts/{id}/kill-switch",
            post(engage_kill_switch).delete(release_kill_switch),
        )
        .route("/accounts/{id}/evaluate", post(evaluate_trade))
        .route("/accounts/{id}/risk-events", get(list_risk_events))
        .route("/accounts/{id}/orders", post(crate::routes_orders::place_order))
        .route(
            "/accounts/{id}/exchange-config",
            get(crate::routes_exchange::get_exchange_config)
                .put(crate::routes_exchange::put_exchange_config),
        )
        .with_state(state)
}

// ---------- accounts ----------

#[derive(Deserialize, Default)]
pub struct CreateAccount {
    #[serde(default)]
    equity: Option<Decimal>,
}

#[derive(Serialize)]
pub struct AccountView {
    id: Uuid,
    status: String,
    equity: Decimal,
    kill_switch_engaged: bool,
}

async fn create_account(
    State(state): State<AppState>,
    body: Option<Json<CreateAccount>>,
) -> Result<(StatusCode, Json<AccountView>), ApiError> {
    let pool = state.pool.clone();
    let equity = body.and_then(|b| b.0.equity).unwrap_or(Decimal::ZERO);
    if equity < Decimal::ZERO {
        return Err(ApiError::Invalid("equity must be >= 0".into()));
    }
    let id = Uuid::now_v7();
    let mut tx = db::tenant_tx(&pool, id).await?;
    sqlx::query("INSERT INTO accounts (id, equity) VALUES ($1, $2)")
        .bind(id)
        .bind(equity)
        .execute(&mut *tx)
        .await?;
    // Risk config rows are born with the placeholder defaults from the schema.
    sqlx::query("INSERT INTO risk_configs (account_id) VALUES ($1)")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    db::insert_risk_event(&mut tx, id, "account_created", &json!({ "equity": equity })).await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(AccountView {
            id,
            status: "active".into(),
            equity,
            kill_switch_engaged: false,
        }),
    ))
}

async fn get_account(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<AccountView>, ApiError> {
    let pool = state.pool.clone();
    let mut tx = db::tenant_tx(&pool, id).await?;
    let row = sqlx::query(
        "SELECT status, equity, kill_switch_engaged_at IS NOT NULL AS ks FROM accounts WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(ApiError::NotFound)?;
    Ok(Json(AccountView {
        id,
        status: row.get("status"),
        equity: row.get("equity"),
        kill_switch_engaged: row.get("ks"),
    }))
}

#[derive(Deserialize)]
pub struct PutEquity {
    equity: Decimal,
}

async fn put_equity(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(body): Json<PutEquity>,
) -> Result<StatusCode, ApiError> {
    let pool = state.pool.clone();
    if body.equity < Decimal::ZERO {
        return Err(ApiError::Invalid("equity must be >= 0".into()));
    }
    let mut tx = db::tenant_tx(&pool, id).await?;
    let account = db::lock_account(&mut tx, id).await?;
    sqlx::query("UPDATE accounts SET equity = $2, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(body.equity)
        .execute(&mut *tx)
        .await?;
    db::insert_risk_event(
        &mut tx,
        id,
        "equity_changed",
        &json!({ "old": account.equity, "new": body.equity }),
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------- risk config ----------

#[derive(Serialize, Deserialize)]
pub struct RiskConfigView {
    daily_max_loss_pct: Decimal,
    max_position_pct_equity: Decimal,
    max_concurrent_positions: i32,
    max_correlated_exposure_pct: Decimal,
    version: i32,
}

async fn get_risk_config(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<RiskConfigView>, ApiError> {
    let pool = state.pool.clone();
    let mut tx = db::tenant_tx(&pool, id).await?;
    let (cfg, version) = db::load_risk_config(&mut tx, id).await?;
    Ok(Json(RiskConfigView {
        daily_max_loss_pct: cfg.daily_max_loss_pct,
        max_position_pct_equity: cfg.max_position_pct_equity,
        max_concurrent_positions: cfg.max_concurrent_positions,
        max_correlated_exposure_pct: cfg.max_correlated_exposure_pct,
        version,
    }))
}

async fn put_risk_config(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(body): Json<RiskConfigView>,
) -> Result<Json<RiskConfigView>, ApiError> {
    let pool = state.pool.clone();
    let pct = |name: &str, v: Decimal| -> Result<(), ApiError> {
        if v <= Decimal::ZERO || v > Decimal::ONE_HUNDRED {
            Err(ApiError::Invalid(format!("{name} must be in (0, 100]")))
        } else {
            Ok(())
        }
    };
    pct("daily_max_loss_pct", body.daily_max_loss_pct)?;
    pct("max_position_pct_equity", body.max_position_pct_equity)?;
    pct("max_correlated_exposure_pct", body.max_correlated_exposure_pct)?;
    if body.max_concurrent_positions <= 0 {
        return Err(ApiError::Invalid("max_concurrent_positions must be > 0".into()));
    }

    let mut tx = db::tenant_tx(&pool, id).await?;
    let (old, current_version) = db::load_risk_config(&mut tx, id).await?;
    if current_version != body.version {
        return Err(ApiError::Conflict(format!(
            "risk config version is {current_version}, request had {}",
            body.version
        )));
    }
    let new_version = current_version + 1;
    let updated = sqlx::query(
        "UPDATE risk_configs SET daily_max_loss_pct = $2, max_position_pct_equity = $3,
             max_concurrent_positions = $4, max_correlated_exposure_pct = $5,
             version = $6, updated_at = now()
         WHERE account_id = $1 AND version = $7",
    )
    .bind(id)
    .bind(body.daily_max_loss_pct)
    .bind(body.max_position_pct_equity)
    .bind(body.max_concurrent_positions)
    .bind(body.max_correlated_exposure_pct)
    .bind(new_version)
    .bind(current_version)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    // A concurrent writer can commit between our version read and this
    // UPDATE (READ COMMITTED re-evaluates the WHERE). Zero rows means we
    // lost the race: report conflict, never log an audit event for a
    // change that did not happen.
    if updated != 1 {
        return Err(ApiError::Conflict(
            "risk config was modified concurrently; re-read and retry".into(),
        ));
    }
    db::insert_risk_event(
        &mut tx,
        id,
        "risk_config_changed",
        &json!({
            "old": {
                "daily_max_loss_pct": old.daily_max_loss_pct,
                "max_position_pct_equity": old.max_position_pct_equity,
                "max_concurrent_positions": old.max_concurrent_positions,
                "max_correlated_exposure_pct": old.max_correlated_exposure_pct,
                "version": current_version,
            },
            "new": {
                "daily_max_loss_pct": body.daily_max_loss_pct,
                "max_position_pct_equity": body.max_position_pct_equity,
                "max_concurrent_positions": body.max_concurrent_positions,
                "max_correlated_exposure_pct": body.max_correlated_exposure_pct,
                "version": new_version,
            },
        }),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(RiskConfigView {
        version: new_version,
        ..body
    }))
}

// ---------- correlation groups ----------

#[derive(Deserialize, Serialize)]
pub struct CorrelationGroupDef {
    name: String,
    symbols: Vec<String>,
}

/// Declarative replace of all correlation groups for the account.
async fn put_correlation_groups(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(groups): Json<Vec<CorrelationGroupDef>>,
) -> Result<StatusCode, ApiError> {
    let pool = state.pool.clone();
    let mut seen = std::collections::HashSet::new();
    for g in &groups {
        if g.name.trim().is_empty() {
            return Err(ApiError::Invalid("group name must not be empty".into()));
        }
        for s in &g.symbols {
            if s.trim().is_empty() {
                return Err(ApiError::Invalid("symbol must not be empty".into()));
            }
            if !seen.insert(s.clone()) {
                return Err(ApiError::Invalid(format!(
                    "symbol {s} appears in more than one group"
                )));
            }
        }
    }
    let mut tx = db::tenant_tx(&pool, id).await?;
    db::lock_account(&mut tx, id).await?;
    sqlx::query("DELETE FROM correlation_groups WHERE account_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    for g in &groups {
        let gid = Uuid::now_v7();
        sqlx::query("INSERT INTO correlation_groups (id, account_id, name) VALUES ($1, $2, $3)")
            .bind(gid)
            .bind(id)
            .bind(&g.name)
            .execute(&mut *tx)
            .await?;
        for s in &g.symbols {
            sqlx::query(
                "INSERT INTO correlation_group_assets (group_id, account_id, symbol) VALUES ($1, $2, $3)",
            )
            .bind(gid)
            .bind(id)
            .bind(s)
            .execute(&mut *tx)
            .await?;
        }
    }
    db::insert_risk_event(
        &mut tx,
        id,
        "risk_config_changed",
        &json!({ "correlation_groups": groups }),
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------- kill switch ----------

async fn engage_kill_switch(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<AccountView>, ApiError> {
    let pool = state.pool.clone();
    let mut tx = db::tenant_tx(&pool, id).await?;
    let account = db::lock_account(&mut tx, id).await?;
    if !account.kill_switch_engaged {
        sqlx::query(
            "UPDATE accounts SET status = 'killed', kill_switch_engaged_at = now(),
                 updated_at = now() WHERE id = $1",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
        db::insert_risk_event(&mut tx, id, "kill_switch_engaged", &json!({})).await?;
    }
    tx.commit().await?;
    Ok(Json(AccountView {
        id,
        status: "killed".into(),
        equity: account.equity,
        kill_switch_engaged: true,
    }))
}

async fn release_kill_switch(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<AccountView>, ApiError> {
    let pool = state.pool.clone();
    let mut tx = db::tenant_tx(&pool, id).await?;
    let account = db::lock_account(&mut tx, id).await?;
    let status = if account.kill_switch_engaged {
        sqlx::query(
            "UPDATE accounts SET status = 'active', kill_switch_engaged_at = NULL,
                 updated_at = now() WHERE id = $1",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
        db::insert_risk_event(&mut tx, id, "kill_switch_released", &json!({})).await?;
        "active".to_string()
    } else {
        account.status.clone()
    };
    tx.commit().await?;
    Ok(Json(AccountView {
        id,
        status,
        equity: account.equity,
        kill_switch_engaged: false,
    }))
}

// ---------- evaluate ----------

#[derive(Serialize)]
pub struct EvaluateResponse {
    decision: Decision,
}

async fn evaluate_trade(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(intent): Json<TradeIntent>,
) -> Result<Json<EvaluateResponse>, ApiError> {
    let pool = state.pool.clone();
    if intent.symbol.trim().is_empty() {
        return Err(ApiError::Invalid("symbol must not be empty".into()));
    }
    if intent.quantity < Decimal::ZERO || intent.price < Decimal::ZERO {
        return Err(ApiError::Invalid("quantity and price must be >= 0".into()));
    }

    let mut tx = db::tenant_tx(&pool, id).await?;
    let account = db::lock_account(&mut tx, id).await?;
    let (cfg, _) = db::load_risk_config(&mut tx, id).await?;
    let snapshot = db::load_snapshot(&mut tx, id, &account).await?;

    let decision = evaluate(&intent, &snapshot, &cfg);

    // Persist a newly-detected breaker trip so the halt outlives this call.
    if !snapshot.circuit_breaker_tripped && circuit_breaker_should_trip(&snapshot, &cfg) {
        db::trip_circuit_breaker(&mut tx, id).await?;
        db::insert_risk_event(
            &mut tx,
            id,
            "circuit_breaker_tripped",
            &json!({
                "today_realized_loss": snapshot.today_realized_loss,
                "equity": snapshot.equity,
                "daily_max_loss_pct": cfg.daily_max_loss_pct,
            }),
        )
        .await?;
    }

    db::insert_risk_event(
        &mut tx,
        id,
        "trade_evaluated",
        &json!({
            "intent": intent,
            "decision": decision,
            "snapshot": {
                "equity": snapshot.equity,
                "today_realized_loss": snapshot.today_realized_loss,
                "kill_switch_engaged": snapshot.kill_switch_engaged,
                "circuit_breaker_tripped": snapshot.circuit_breaker_tripped,
                "open_position_count": snapshot.open_positions.len(),
            },
        }),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(EvaluateResponse { decision }))
}

// ---------- audit log ----------

#[derive(Deserialize)]
pub struct EventsQuery {
    limit: Option<i64>,
    /// Return events strictly older than this event id (cursor pagination).
    before: Option<Uuid>,
}

#[derive(Serialize)]
pub struct RiskEventView {
    id: Uuid,
    event_type: String,
    payload: serde_json::Value,
    created_at: DateTime<Utc>,
}

async fn list_risk_events(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(q): Query<EventsQuery>,
) -> Result<Json<Vec<RiskEventView>>, ApiError> {
    let pool = state.pool.clone();
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    let mut tx = db::tenant_tx(&pool, id).await?;
    // UUIDv7 ids are time-ordered, so id is a stable pagination cursor.
    let rows = sqlx::query(
        "SELECT id, event_type, payload, created_at FROM risk_events
         WHERE account_id = $1 AND ($2::uuid IS NULL OR id < $2)
         ORDER BY id DESC LIMIT $3",
    )
    .bind(id)
    .bind(q.before)
    .bind(limit)
    .fetch_all(&mut *tx)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| RiskEventView {
                id: r.get("id"),
                event_type: r.get("event_type"),
                payload: r.get("payload"),
                created_at: r.get("created_at"),
            })
            .collect(),
    ))
}
