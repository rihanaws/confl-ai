use async_trait::async_trait;
use rust_decimal::Decimal;
use serde_json::Value;
use uuid::Uuid;

use crate::error::Result;
use crate::types::{ExchangeMode, Fill, OrderSide, OrderStatus, OrderType};

/// Plain-data view of an order row, passed in from the server (which owns
/// sqlx) so this crate never touches the database directly.
#[derive(Debug, Clone)]
pub struct OrderSnapshot {
    pub order_id: Uuid,
    pub client_order_id: String,
    pub symbol: String,
    pub side: OrderSide,
    pub order_type: OrderType,
    pub quantity: Decimal,
    pub price: Option<Decimal>,
    pub status: OrderStatus,
}

#[derive(Debug, Clone)]
pub struct FillSnapshot {
    pub exchange_trade_id: String,
    pub quantity: Decimal,
    pub price: Decimal,
}

#[derive(Debug, Clone)]
pub struct PositionSnapshot {
    pub symbol: String,
    pub quantity: Decimal,
}

/// Everything an adapter needs to reconcile one order, assembled by the
/// server inside its own transaction (Fix 2: this crate never reads
/// `order_fills` or any durable store directly).
#[derive(Debug, Clone)]
pub struct ReconcileSnapshot {
    pub order: OrderSnapshot,
    pub fills: Vec<FillSnapshot>,
    pub positions: Vec<PositionSnapshot>,
}

#[derive(Debug, Clone)]
pub struct PositionDelta {
    pub symbol: String,
    pub quantity_delta: Decimal,
}

#[derive(Debug, Clone)]
pub struct RiskEvent {
    pub event_type: String,
    pub payload: Value,
}

/// A follow-up command the server should enqueue as a result of
/// reconciliation (e.g. a `reconcile_order` retry).
#[derive(Debug, Clone)]
pub struct ExchangeCommand {
    pub command_type: String,
    pub idempotency_key: String,
    pub payload: Value,
}

/// Result of reconciling one order. The server persists this — the adapter
/// never writes to a database itself.
#[derive(Debug, Clone, Default)]
pub struct ReconcileResult {
    pub new_status: Option<OrderStatus>,
    pub fills_to_add: Vec<Fill>,
    pub positions_delta: Vec<PositionDelta>,
    pub events: Vec<RiskEvent>,
    pub command: Option<ExchangeCommand>,
}

#[derive(Debug, Clone)]
pub struct SubmitResult {
    pub accepted: bool,
    pub events: Vec<RiskEvent>,
}

/// Adapter over one exchange venue (paper or live Binance). Zero sqlx in any
/// implementation — the server owns all persistence and passes/receives
/// plain data.
#[async_trait]
pub trait ExchangeAdapter: Send + Sync {
    fn mode(&self) -> ExchangeMode;

    async fn submit(&self, order: &OrderSnapshot) -> Result<SubmitResult>;

    async fn cancel(&self, order: &OrderSnapshot) -> Result<()>;

    async fn reconcile_order(&self, snapshot: &ReconcileSnapshot) -> Result<ReconcileResult>;

    async fn reconcile_account(&self, account_id: Uuid) -> Result<()>;

    async fn start(&self) -> Result<()>;

    async fn shutdown(&self) -> Result<()>;
}
