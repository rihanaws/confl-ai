pub mod matching_engine;
pub mod market_data;
pub mod paper_account;

use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use serde_json::json;
use uuid::Uuid;

use crate::adapter::{
    ExchangeAdapter, OrderSnapshot, PositionDelta, ReconcileResult, ReconcileSnapshot, RiskEvent,
    SubmitResult,
};
use crate::error::Result;
use crate::types::{ExchangeMode, OrderStatus};
use matching_engine::MatchingEngine;
use market_data::MarketDataProvider;

/// Paper-trading adapter. Never calls Binance for order placement — fills
/// are matched locally against public market data only. `reconcile_order`
/// operates purely on the `ReconcileSnapshot` passed in by the server (Fix
/// 2): this crate has no sqlx and never reads `order_fills` directly.
///
/// `market_data` is shared (`Arc<Mutex<..>>`) rather than owned, so the same
/// live feed backs both this adapter's fills and the order-placement
/// route's pre-trade price estimate — one refresh loop, one source of
/// truth, no risk of the route validating against data the adapter never
/// sees.
pub struct PaperAdapter {
    market_data: Arc<Mutex<MarketDataProvider>>,
}

impl PaperAdapter {
    pub fn new(market_data: MarketDataProvider) -> Self {
        Self {
            market_data: Arc::new(Mutex::new(market_data)),
        }
    }

    pub fn with_shared_market_data(market_data: Arc<Mutex<MarketDataProvider>>) -> Self {
        Self { market_data }
    }
}

#[async_trait]
impl ExchangeAdapter for PaperAdapter {
    fn mode(&self) -> ExchangeMode {
        ExchangeMode::Paper
    }

    async fn submit(&self, _order: &OrderSnapshot) -> Result<SubmitResult> {
        // Paper orders are always locally accepted; matching happens on the
        // next reconcile_order pass against current market data.
        Ok(SubmitResult {
            accepted: true,
            events: vec![],
        })
    }

    async fn cancel(&self, _order: &OrderSnapshot) -> Result<()> {
        Ok(())
    }

    async fn reconcile_order(&self, snapshot: &ReconcileSnapshot) -> Result<ReconcileResult> {
        let order = &snapshot.order;
        if order.status.is_terminal() {
            return Ok(ReconcileResult::default());
        }

        let already_filled: rust_decimal::Decimal =
            snapshot.fills.iter().map(|f| f.quantity).sum();
        let remaining = order.quantity - already_filled;
        if remaining <= rust_decimal::Decimal::ZERO {
            return Ok(ReconcileResult::default());
        }

        let is_buy = matches!(order.side, crate::types::OrderSide::Buy);
        let estimate = {
            let md = self.market_data.lock().unwrap();
            md.conservative_estimate(&order.symbol, is_buy, Instant::now())
        };

        // Content-addressed, not random: a lease-expiry re-claim replays
        // this same reconcile pass against the same `snapshot.fills`, so
        // the ordinal (fill count so far) must be stable across retries —
        // that's what lets `insert_fill`'s `ON CONFLICT (account_id,
        // exchange_trade_id) DO NOTHING` dedupe the retry instead of
        // double-counting the fill and position delta.
        let trade_id = format!("{}:paper:{}", order.client_order_id, snapshot.fills.len());
        let fill = MatchingEngine::try_fill(
            order.order_type,
            order.side,
            remaining,
            order.price,
            estimate,
            trade_id,
        );

        let Some(fill) = fill else {
            return Ok(ReconcileResult::default());
        };

        let new_filled = already_filled + fill.quantity;
        let new_status = if new_filled >= order.quantity {
            OrderStatus::Filled
        } else {
            OrderStatus::PartiallyFilled
        };

        let position_qty_delta = if is_buy { fill.quantity } else { -fill.quantity };

        Ok(ReconcileResult {
            new_status: Some(new_status),
            fills_to_add: vec![fill],
            positions_delta: vec![PositionDelta {
                symbol: order.symbol.clone(),
                quantity_delta: position_qty_delta,
            }],
            events: vec![RiskEvent {
                event_type: if new_status == OrderStatus::Filled {
                    "order_filled".into()
                } else {
                    "order_partially_filled".into()
                },
                payload: json!({ "order_id": order.order_id, "new_status": format!("{new_status:?}") }),
            }],
            command: None,
        })
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
    use crate::adapter::{FillSnapshot, OrderSnapshot};
    use crate::types::{OrderSide, OrderType};
    use rust_decimal_macros::dec;
    use std::time::Duration;

    fn order(status: OrderStatus, qty: rust_decimal::Decimal) -> OrderSnapshot {
        OrderSnapshot {
            order_id: Uuid::now_v7(),
            client_order_id: "abc123".into(),
            symbol: "BTCUSDT".into(),
            side: OrderSide::Buy,
            order_type: OrderType::Market,
            quantity: qty,
            price: None,
            status,
        }
    }

    #[tokio::test]
    async fn market_order_fills_from_fresh_market_data() {
        let mut md = MarketDataProvider::new(Duration::from_secs(5));
        md.update("BTCUSDT", dec!(100), dec!(101), Instant::now());
        let adapter = PaperAdapter::new(md);
        let snap = ReconcileSnapshot {
            order: order(OrderStatus::Submitted, dec!(1)),
            fills: vec![],
            positions: vec![],
        };
        let result = adapter.reconcile_order(&snap).await.unwrap();
        assert_eq!(result.new_status, Some(OrderStatus::Filled));
        assert_eq!(result.fills_to_add.len(), 1);
        assert_eq!(result.fills_to_add[0].price, dec!(101));
        assert_eq!(result.positions_delta[0].quantity_delta, dec!(1));
    }

    #[tokio::test]
    async fn stale_market_data_yields_no_fill() {
        let md = MarketDataProvider::new(Duration::from_secs(5));
        let adapter = PaperAdapter::new(md);
        let snap = ReconcileSnapshot {
            order: order(OrderStatus::Submitted, dec!(1)),
            fills: vec![],
            positions: vec![],
        };
        let result = adapter.reconcile_order(&snap).await.unwrap();
        assert_eq!(result.new_status, None);
        assert!(result.fills_to_add.is_empty());
    }

    #[tokio::test]
    async fn terminal_order_never_reconciled() {
        let mut md = MarketDataProvider::new(Duration::from_secs(5));
        md.update("BTCUSDT", dec!(100), dec!(101), Instant::now());
        let adapter = PaperAdapter::new(md);
        let snap = ReconcileSnapshot {
            order: order(OrderStatus::Filled, dec!(1)),
            fills: vec![FillSnapshot {
                exchange_trade_id: "x".into(),
                quantity: dec!(1),
                price: dec!(100),
            }],
            positions: vec![],
        };
        let result = adapter.reconcile_order(&snap).await.unwrap();
        assert_eq!(result.new_status, None);
    }
}
