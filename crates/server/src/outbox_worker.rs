//! SKIP LOCKED claim loop. Routes each claimed command to the adapter that
//! matches its `exchange_mode` — never `.unwrap()` the resolution; a
//! missing adapter is a fail-closed path (`reconciliation_required` +
//! manual-review audit event), not a panic.

use std::sync::Arc;

use confluence_exchange::adapter::{ExchangeAdapter, FillSnapshot, OrderSnapshot, ReconcileSnapshot};
use confluence_exchange::types::ExchangeMode;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use crate::db;

pub struct Supervisor {
    pub paper_adapter: Option<Arc<dyn ExchangeAdapter>>,
    pub live_adapter: Option<Arc<dyn ExchangeAdapter>>,
}

impl Supervisor {
    fn resolve(&self, mode: ExchangeMode) -> Option<Arc<dyn ExchangeAdapter>> {
        match mode {
            ExchangeMode::Paper => self.paper_adapter.clone(),
            ExchangeMode::Live => self.live_adapter.clone(),
        }
    }
}

/// One claim-and-dispatch pass. `owner_pool` connects as the RLS-bypassing
/// owner role (`DATABASE_URL`) and is used only for claiming/updating
/// `exchange_commands` rows across all tenants — that table's
/// `tenant_isolation` policy would otherwise block a cross-tenant batch
/// claim outright (RLS has no notion of "the worker's own tenant"). Every
/// per-command audit write still goes through `app_pool` with
/// `tenant_tx`, so RLS keeps gating that half as designed.
pub async fn run_once(
    owner_pool: &PgPool,
    app_pool: &PgPool,
    supervisor: &Supervisor,
    batch_size: i64,
) -> Result<usize, sqlx::Error> {
    let claimed = db::claim_pending_commands(owner_pool, batch_size, 30)
        .await
        .map_err(|e| match e {
            crate::error::ApiError::Db(e) => e,
            other => sqlx::Error::Protocol(other.to_string()),
        })?;

    let mut processed = 0usize;
    for cmd in claimed {
        processed += 1;
        let Some(adapter) = supervisor.resolve(cmd.exchange_mode) else {
            // Fail-closed: no adapter configured for this mode. Not an
            // error to propagate as a panic — enters reconciliation_required
            // and a manual-review risk_events row.
            let mut owner_conn = owner_pool.acquire().await?;
            let _ = db::mark_command_reconciliation_required(&mut owner_conn, cmd.id).await;
            if let Ok(mut tx) = db::tenant_tx(app_pool, cmd.account_id).await {
                let _ = db::insert_risk_event(
                    &mut tx,
                    cmd.account_id,
                    "exchange_reconciliation_required",
                    &json!({
                        "command_id": cmd.id,
                        "reason": "no adapter configured for resolved exchange_mode",
                    }),
                )
                .await;
                let _ = tx.commit().await;
            }
            continue;
        };

        match dispatch(&cmd, &adapter, app_pool).await {
            Ok(()) => {
                let mut owner_conn = owner_pool.acquire().await?;
                let _ = db::mark_command_delivered(&mut owner_conn, cmd.id).await;
                // A delivered submit_order needs a follow-up poll to learn
                // whether/how it filled — enqueue the first reconcile_order
                // pass now; reconcile_order itself does not self-repeat
                // here (a scheduled periodic sweep is the production
                // mechanism for repeated polling of still-open orders).
                if cmd.command_type == "submit_order" {
                    if let Some(order_id) = extract_order_id(&cmd) {
                        let _ = db::insert_exchange_command(
                            &mut owner_conn,
                            cmd.account_id,
                            "reconcile_order",
                            cmd.exchange_mode,
                            &format!("{order_id}:reconcile:initial"),
                            &json!({ "order_id": order_id }),
                        )
                        .await;
                    }
                }
            }
            Err(DispatchOutcome::Retry) => {
                let mut owner_conn = owner_pool.acquire().await?;
                let _ = db::requeue_command(&mut owner_conn, cmd.id).await;
            }
            Err(DispatchOutcome::PermanentlyRejected) => {
                let mut owner_conn = owner_pool.acquire().await?;
                let _ = db::mark_command_permanently_rejected(&mut owner_conn, cmd.id).await;
                // <exchange_command_states>: a deterministic 4xx-style
                // rejection also flips the order itself to rejected.
                if let Some(order_id) = extract_order_id(&cmd) {
                    if let Ok(mut tx) = db::tenant_tx(app_pool, cmd.account_id).await {
                        if let Ok(locked) = db::lock_order(&mut tx, cmd.account_id, order_id).await {
                            let legal = confluence_exchange::order_state::OrderStateMachine::transition(
                                locked.status,
                                confluence_exchange::types::OrderStatus::Rejected,
                            )
                            .is_ok();
                            if legal {
                                let _ = db::update_order_status(
                                    &mut tx,
                                    cmd.account_id,
                                    order_id,
                                    confluence_exchange::types::OrderStatus::Rejected,
                                    locked.filled_quantity,
                                )
                                .await;
                            }
                        }
                        let _ = db::insert_risk_event(
                            &mut tx,
                            cmd.account_id,
                            "order_rejected",
                            &json!({ "command_id": cmd.id, "order_id": order_id }),
                        )
                        .await;
                        let _ = tx.commit().await;
                    }
                }
            }
            Err(DispatchOutcome::ReconciliationRequired) => {
                let mut owner_conn = owner_pool.acquire().await?;
                let _ = db::mark_command_reconciliation_required(&mut owner_conn, cmd.id).await;
                if let Ok(mut tx) = db::tenant_tx(app_pool, cmd.account_id).await {
                    let _ = db::insert_risk_event(
                        &mut tx,
                        cmd.account_id,
                        "exchange_reconciliation_required",
                        &json!({ "command_id": cmd.id }),
                    )
                    .await;
                    let _ = tx.commit().await;
                }
            }
        }
    }
    Ok(processed)
}

enum DispatchOutcome {
    Retry,
    PermanentlyRejected,
    ReconciliationRequired,
}

/// Deterministic exchange rejections (bad symbol, filter violation,
/// insufficient balance) are never fixed by retrying the same request, so
/// they become `permanently_rejected`. Everything else (network/parse
/// errors, transient market-data gaps) is retried with backoff.
fn classify_exchange_error(err: confluence_exchange::ExchangeError) -> DispatchOutcome {
    use confluence_exchange::ExchangeError;
    match err {
        ExchangeError::InsufficientBalance { .. }
        | ExchangeError::FilterViolation(_)
        | ExchangeError::SymbolNotFound(_) => DispatchOutcome::PermanentlyRejected,
        _ => DispatchOutcome::Retry,
    }
}

fn extract_order_id(cmd: &db::ClaimedCommand) -> Option<Uuid> {
    cmd.payload
        .get("order_id")
        .and_then(|v| v.as_str())
        .and_then(|s| Uuid::parse_str(s).ok())
}

/// Loads an order + its recorded fills into the plain-data snapshot the
/// exchange crate's adapter trait expects (Fix 2: the adapter never touches
/// sqlx; the server hydrates and persists on its behalf).
async fn hydrate_snapshot(
    app_pool: &PgPool,
    account_id: Uuid,
    order_id: Uuid,
) -> Result<ReconcileSnapshot, DispatchOutcome> {
    let mut tx = db::tenant_tx(app_pool, account_id)
        .await
        .map_err(|_| DispatchOutcome::Retry)?;
    let order = db::lock_order(&mut tx, account_id, order_id)
        .await
        .map_err(|_| DispatchOutcome::ReconciliationRequired)?;
    let fills = db::load_fills_for_order(&mut tx, account_id, order_id)
        .await
        .map_err(|_| DispatchOutcome::Retry)?;
    let _ = tx.commit().await;

    Ok(ReconcileSnapshot {
        order: OrderSnapshot {
            order_id: order.id,
            client_order_id: order.client_order_id,
            symbol: order.symbol,
            side: order.side,
            order_type: order.order_type,
            quantity: order.quantity,
            price: order.price,
            status: order.status,
        },
        fills: fills
            .into_iter()
            .map(|f| FillSnapshot {
                exchange_trade_id: f.exchange_trade_id,
                quantity: f.quantity,
                price: f.price,
            })
            .collect(),
        positions: vec![],
    })
}

/// Persists a `ReconcileResult` atomically: new order status, new fill
/// rows, position deltas, and audit events, all in one transaction — the
/// same pattern the route layer uses for order placement.
async fn persist_reconcile_result(
    app_pool: &PgPool,
    account_id: Uuid,
    order_id: Uuid,
    result: confluence_exchange::adapter::ReconcileResult,
) -> Result<(), DispatchOutcome> {
    let mut tx = db::tenant_tx(app_pool, account_id)
        .await
        .map_err(|_| DispatchOutcome::Retry)?;

    // Step 2 of <fill_processing_order>: lock the order row before writing
    // fills against it, so a concurrent reconcile pass on the same order
    // serializes rather than racing on filled_quantity.
    let locked = db::lock_order(&mut tx, account_id, order_id)
        .await
        .map_err(|_| DispatchOutcome::Retry)?;

    for fill in &result.fills_to_add {
        db::insert_fill(&mut tx, account_id, order_id, fill)
            .await
            .map_err(|_| DispatchOutcome::Retry)?;
    }

    // Both adapters (paper matching, Binance reconcile) produce at most one
    // new fill per reconcile pass, so its price is the entry price for any
    // resulting position delta. A future multi-fill-per-pass adapter would
    // need per-symbol price attribution here.
    let fill_price = result
        .fills_to_add
        .first()
        .map(|f| f.price)
        .unwrap_or(rust_decimal::Decimal::ZERO);
    for delta in &result.positions_delta {
        db::apply_position_delta(&mut tx, account_id, &delta.symbol, delta.quantity_delta, fill_price)
            .await
            .map_err(|_| DispatchOutcome::Retry)?;
    }

    if let Some(new_status) = result.new_status {
        // Validated transition: a reconcile result that would jump to an
        // illegal state (e.g. a terminal order somehow reopened) is a bug
        // in the adapter, not something to persist silently.
        if confluence_exchange::order_state::OrderStateMachine::transition(locked.status, new_status).is_ok() {
            let new_fill_qty: rust_decimal::Decimal = result.fills_to_add.iter().map(|f| f.quantity).sum();
            let filled_qty = locked.filled_quantity + new_fill_qty;
            db::update_order_status(&mut tx, account_id, order_id, new_status, filled_qty)
                .await
                .map_err(|_| DispatchOutcome::Retry)?;
        } else {
            return Err(DispatchOutcome::ReconciliationRequired);
        }
    }

    for event in &result.events {
        let _ = db::insert_risk_event(&mut tx, account_id, &event.event_type, &event.payload).await;
    }

    tx.commit().await.map_err(|_| DispatchOutcome::Retry)?;
    Ok(())
}

async fn dispatch(
    cmd: &db::ClaimedCommand,
    adapter: &Arc<dyn ExchangeAdapter>,
    app_pool: &PgPool,
) -> Result<(), DispatchOutcome> {
    match cmd.command_type.as_str() {
        "submit_order" => {
            let Some(order_id) = extract_order_id(cmd) else {
                return Err(DispatchOutcome::ReconciliationRequired);
            };
            let snapshot = hydrate_snapshot(app_pool, cmd.account_id, order_id).await?;
            adapter
                .submit(&snapshot.order)
                .await
                .map_err(classify_exchange_error)?;

            // Accepted for submission: pending -> submitted, so the next
            // reconcile_order pass sees a legal predecessor state for the
            // fill transitions in order_state.rs.
            let mut tx = db::tenant_tx(app_pool, cmd.account_id)
                .await
                .map_err(|_| DispatchOutcome::Retry)?;
            let locked = db::lock_order(&mut tx, cmd.account_id, order_id)
                .await
                .map_err(|_| DispatchOutcome::Retry)?;
            if confluence_exchange::order_state::OrderStateMachine::transition(
                locked.status,
                confluence_exchange::types::OrderStatus::Submitted,
            )
            .is_ok()
            {
                db::update_order_status(
                    &mut tx,
                    cmd.account_id,
                    order_id,
                    confluence_exchange::types::OrderStatus::Submitted,
                    locked.filled_quantity,
                )
                .await
                .map_err(|_| DispatchOutcome::Retry)?;
            }
            tx.commit().await.map_err(|_| DispatchOutcome::Retry)?;
            Ok(())
        }
        "reconcile_order" => {
            let Some(order_id) = extract_order_id(cmd) else {
                return Err(DispatchOutcome::ReconciliationRequired);
            };
            let snapshot = hydrate_snapshot(app_pool, cmd.account_id, order_id).await?;
            let result = adapter
                .reconcile_order(&snapshot)
                .await
                .map_err(classify_exchange_error)?;
            persist_reconcile_result(app_pool, cmd.account_id, order_id, result).await
        }
        "cancel_order" => {
            let Some(order_id) = extract_order_id(cmd) else {
                return Err(DispatchOutcome::ReconciliationRequired);
            };
            let snapshot = hydrate_snapshot(app_pool, cmd.account_id, order_id).await?;
            adapter
                .cancel(&snapshot.order)
                .await
                .map_err(|_| DispatchOutcome::Retry)?;
            Ok(())
        }
        _ => Err(DispatchOutcome::ReconciliationRequired),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use confluence_exchange::adapter::{OrderSnapshot, ReconcileResult, ReconcileSnapshot, SubmitResult};
    use confluence_exchange::error::Result as ExResult;

    struct StubAdapter(ExchangeMode);

    #[async_trait]
    impl ExchangeAdapter for StubAdapter {
        fn mode(&self) -> ExchangeMode {
            self.0
        }
        async fn submit(&self, _order: &OrderSnapshot) -> ExResult<SubmitResult> {
            Ok(SubmitResult { accepted: true, events: vec![] })
        }
        async fn cancel(&self, _order: &OrderSnapshot) -> ExResult<()> {
            Ok(())
        }
        async fn reconcile_order(&self, _snapshot: &ReconcileSnapshot) -> ExResult<ReconcileResult> {
            Ok(ReconcileResult::default())
        }
        async fn reconcile_account(&self, _account_id: uuid::Uuid) -> ExResult<()> {
            Ok(())
        }
        async fn start(&self) -> ExResult<()> {
            Ok(())
        }
        async fn shutdown(&self) -> ExResult<()> {
            Ok(())
        }
    }

    #[test]
    fn resolves_paper_and_live_independently() {
        let sup = Supervisor {
            paper_adapter: Some(Arc::new(StubAdapter(ExchangeMode::Paper))),
            live_adapter: None,
        };
        assert!(sup.resolve(ExchangeMode::Paper).is_some());
        assert!(sup.resolve(ExchangeMode::Live).is_none());
    }

    #[test]
    fn missing_adapter_resolves_to_none_not_panic() {
        let sup = Supervisor {
            paper_adapter: None,
            live_adapter: None,
        };
        assert!(sup.resolve(ExchangeMode::Paper).is_none());
        assert!(sup.resolve(ExchangeMode::Live).is_none());
    }
}
