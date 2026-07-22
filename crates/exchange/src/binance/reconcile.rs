use async_trait::async_trait;
use rust_decimal::Decimal;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::adapter::{
    ExchangeAdapter, FillSnapshot, OrderSnapshot, PositionDelta, ReconcileResult,
    ReconcileSnapshot, RiskEvent, SubmitResult,
};
use crate::error::{ExchangeError, Result};
use crate::types::{ExchangeMode, OrderStatus};

use super::rest::BinanceRestClient;

/// Live Binance adapter. `reconcile_order` calls Binance's REST order-status
/// endpoint directly (`GET /api/v3/order?origClientOrderId=`), then returns
/// a `ReconcileResult` for the server to persist — no sqlx enters this
/// adapter either, matching `PaperAdapter`'s crate-boundary rule.
pub struct BinanceAdapter {
    client: BinanceRestClient,
}

impl BinanceAdapter {
    pub fn new(client: BinanceRestClient) -> Self {
        Self { client }
    }

    /// Builds the signed query for `GET /api/v3/order` keyed on
    /// `origClientOrderId`, as required by Fix 1/Fix 2 (never by exchange
    /// order id, which the server doesn't track as primary key).
    fn order_status_query(&self, symbol: &str, client_order_id: &str, timestamp: i64) -> Result<String> {
        let query = format!(
            "symbol={symbol}&origClientOrderId={client_order_id}&timestamp={timestamp}"
        );
        let sig = self.client.sign(&query)?;
        Ok(format!("{query}&signature={sig}"))
    }

    /// Maps a Binance order-status REST response into a `ReconcileResult`.
    /// Split out from `reconcile_order` so tests can exercise the mapping
    /// without a live HTTP call.
    fn map_order_status_response(
        &self,
        snapshot: &ReconcileSnapshot,
        resp: &Value,
    ) -> Result<ReconcileResult> {
        let order = &snapshot.order;
        let status_str = resp
            .get("status")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ExchangeError::MarketDataError("missing status in order response".into()))?;

        let new_status = match status_str {
            "NEW" => OrderStatus::Submitted,
            "PARTIALLY_FILLED" => OrderStatus::PartiallyFilled,
            "FILLED" => OrderStatus::Filled,
            "CANCELED" | "EXPIRED" => OrderStatus::Cancelled,
            "REJECTED" => OrderStatus::Rejected,
            "PENDING_CANCEL" => OrderStatus::CancelRequested,
            other => {
                return Err(ExchangeError::MarketDataError(format!(
                    "unrecognized Binance order status: {other}"
                )))
            }
        };

        if new_status == order.status {
            return Ok(ReconcileResult::default());
        }

        let executed_qty: Decimal = resp
            .get("executedQty")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok())
            .unwrap_or(Decimal::ZERO);
        let already_recorded: Decimal = snapshot.fills.iter().map(|f: &FillSnapshot| f.quantity).sum();
        let delta = executed_qty - already_recorded;

        let mut fills_to_add = vec![];
        if delta > Decimal::ZERO {
            let price: Decimal = resp
                .get("price")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse().ok())
                .unwrap_or(Decimal::ZERO);
            fills_to_add.push(crate::types::Fill {
                exchange_trade_id: format!("{}:reconcile:{}", order.client_order_id, Uuid::now_v7()),
                quantity: delta,
                price,
                fee: Decimal::ZERO,
                fee_asset: String::new(),
            });
        }

        let positions_delta = if delta > Decimal::ZERO {
            let signed = if matches!(order.side, crate::types::OrderSide::Buy) {
                delta
            } else {
                -delta
            };
            vec![PositionDelta {
                symbol: order.symbol.clone(),
                quantity_delta: signed,
            }]
        } else {
            vec![]
        };

        Ok(ReconcileResult {
            new_status: Some(new_status),
            fills_to_add,
            positions_delta,
            events: vec![RiskEvent {
                event_type: match new_status {
                    OrderStatus::Filled => "order_filled".into(),
                    OrderStatus::PartiallyFilled => "order_partially_filled".into(),
                    OrderStatus::Cancelled => "order_cancelled".into(),
                    OrderStatus::Rejected => "order_rejected".into(),
                    _ => "exchange_reconciliation_required".into(),
                },
                payload: json!({ "order_id": order.order_id, "binance_status": status_str }),
            }],
            command: None,
        })
    }
}

#[async_trait]
impl ExchangeAdapter for BinanceAdapter {
    fn mode(&self) -> ExchangeMode {
        ExchangeMode::Live
    }

    async fn submit(&self, order: &OrderSnapshot) -> Result<SubmitResult> {
        let _ = order;
        Ok(SubmitResult {
            accepted: true,
            events: vec![],
        })
    }

    async fn cancel(&self, _order: &OrderSnapshot) -> Result<()> {
        Ok(())
    }

    async fn reconcile_order(&self, snapshot: &ReconcileSnapshot) -> Result<ReconcileResult> {
        if snapshot.order.status.is_terminal() {
            return Ok(ReconcileResult::default());
        }
        let timestamp = chrono::Utc::now().timestamp_millis();
        let query = self.order_status_query(
            &snapshot.order.symbol,
            &snapshot.order.client_order_id,
            timestamp,
        )?;
        let url = format!("{}?{}", self.client.order_url(), query);
        let resp = self
            .client
            .http()
            .get(&url)
            .header("X-MBX-APIKEY", self.client.api_key())
            .send()
            .await
            .map_err(|e| ExchangeError::MarketDataError(format!("order status request failed: {e}")))?;
        let body: Value = resp
            .json()
            .await
            .map_err(|e| ExchangeError::MarketDataError(format!("bad order status body: {e}")))?;
        self.map_order_status_response(snapshot, &body)
    }

    async fn reconcile_account(&self, _account_id: Uuid) -> Result<()> {
        Ok(())
    }

    async fn start(&self) -> Result<()> {
        Ok(())
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binance::rest::BinanceConfig;
    use crate::types::{OrderSide, OrderType};
    use rust_decimal_macros::dec;
    use serde_json::json;

    fn adapter() -> BinanceAdapter {
        BinanceAdapter::new(BinanceRestClient::new(BinanceConfig::testnet("k".into(), "s".into())))
    }

    fn snapshot(status: OrderStatus) -> ReconcileSnapshot {
        ReconcileSnapshot {
            order: OrderSnapshot {
                order_id: Uuid::now_v7(),
                client_order_id: "abc123".into(),
                symbol: "BTCUSDT".into(),
                side: OrderSide::Buy,
                order_type: OrderType::Limit,
                quantity: dec!(1),
                price: Some(dec!(100)),
                status,
            },
            fills: vec![],
            positions: vec![],
        }
    }

    #[test]
    fn origclientorderid_used_as_lookup_key() {
        let a = adapter();
        let q = a.order_status_query("BTCUSDT", "abc123", 1000).unwrap();
        assert!(q.contains("origClientOrderId=abc123"));
        assert!(q.contains("signature="));
    }

    #[test]
    fn filled_status_maps_to_fill_and_position_delta() {
        let a = adapter();
        let resp = json!({ "status": "FILLED", "executedQty": "1.00000000", "price": "100.00000000" });
        let result = a
            .map_order_status_response(&snapshot(OrderStatus::Submitted), &resp)
            .unwrap();
        assert_eq!(result.new_status, Some(OrderStatus::Filled));
        assert_eq!(result.fills_to_add.len(), 1);
        assert_eq!(result.fills_to_add[0].quantity, dec!(1));
        assert_eq!(result.positions_delta[0].quantity_delta, dec!(1));
    }

    #[test]
    fn unchanged_status_is_a_noop() {
        let a = adapter();
        let resp = json!({ "status": "NEW", "executedQty": "0", "price": "100" });
        let result = a
            .map_order_status_response(&snapshot(OrderStatus::Submitted), &resp)
            .unwrap();
        assert_eq!(result.new_status, None);
        assert!(result.fills_to_add.is_empty());
    }

    #[test]
    fn rejected_status_maps_to_rejected() {
        let a = adapter();
        let resp = json!({ "status": "REJECTED", "executedQty": "0", "price": "100" });
        let result = a
            .map_order_status_response(&snapshot(OrderStatus::Submitted), &resp)
            .unwrap();
        assert_eq!(result.new_status, Some(OrderStatus::Rejected));
    }

    #[tokio::test]
    async fn terminal_order_short_circuits_before_any_http_call() {
        let a = adapter();
        let result = a.reconcile_order(&snapshot(OrderStatus::Filled)).await.unwrap();
        assert_eq!(result.new_status, None);
    }
}
