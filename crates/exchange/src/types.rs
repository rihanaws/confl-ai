use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderSide {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderType {
    Limit,
    Market,
}

/// Order lifecycle state. See `order_state.rs` for legal transitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    Pending,
    Submitted,
    PartiallyFilled,
    Filled,
    CancelRequested,
    Cancelled,
    Rejected,
}

impl OrderStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            OrderStatus::Filled | OrderStatus::Cancelled | OrderStatus::Rejected
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExchangeMode {
    Paper,
    Live,
}

/// A single fill (trade execution) against an order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fill {
    pub exchange_trade_id: String,
    pub quantity: Decimal,
    pub price: Decimal,
    pub fee: Decimal,
    pub fee_asset: String,
}

/// Minimum lot-size style filter (LOT_SIZE or MARKET_LOT_SIZE).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LotSizeFilter {
    pub min_qty: Decimal,
    pub max_qty: Decimal,
    pub step_size: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceFilter {
    pub min_price: Decimal,
    pub max_price: Decimal,
    pub tick_size: Decimal,
}

/// Full Binance NOTIONAL filter shape (Fix 3): both bounds, both
/// apply-to-market flags, independently settable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotionalFilter {
    pub min_notional: Option<Decimal>,
    pub max_notional: Option<Decimal>,
    pub apply_min_to_market: bool,
    pub apply_max_to_market: bool,
    pub avg_price_mins: u32,
}

/// Full PERCENT_PRICE / PERCENT_PRICE_BY_SIDE shape (Fix 3). Both variants
/// must be handled by validation, not just `General`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PercentPriceRule {
    General {
        multiplier_up: Decimal,
        multiplier_down: Decimal,
        avg_price_mins: u32,
    },
    BySide {
        bid_multiplier_up: Decimal,
        bid_multiplier_down: Decimal,
        ask_multiplier_up: Decimal,
        ask_multiplier_down: Decimal,
        avg_price_mins: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SymbolStatus {
    Trading,
    Break,
    Halt,
    AuctionMatch,
}

/// Exchange metadata for one trading symbol, as needed for order validation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolMeta {
    pub symbol: String,
    pub base_asset: String,
    pub quote_asset: String,
    pub status: SymbolStatus,
    pub allowed_order_types: Vec<OrderType>,
    pub lot_size: LotSizeFilter,
    pub market_lot_size: LotSizeFilter,
    pub price_filter: PriceFilter,
    pub notional: NotionalFilter,
    pub percent_price: PercentPriceRule,
}

/// A price estimate for market-order notional checks, with a freshness
/// signal so stale feeds fail closed rather than silently sizing on old data.
#[derive(Debug, Clone, Copy)]
pub struct PriceEstimate {
    pub price: Decimal,
    pub source: PriceSource,
    pub is_fresh: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriceSource {
    BestBid,
    BestAsk,
    LastTrade,
}

/// A conservative estimate used for reservation/notional sizing: worst-case
/// side of the book (ask for buys, bid for sells) so reservations never
/// under-cover the eventual fill.
#[derive(Debug, Clone, Copy)]
pub struct ConservativePriceEstimate {
    pub price: Decimal,
    pub is_fresh: bool,
}

/// An intent to place an order, prior to any DB row existing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderIntent {
    pub symbol: String,
    pub side: OrderSide,
    pub order_type: OrderType,
    pub quantity: Decimal,
    /// Required for `Limit`, ignored for `Market`.
    pub price: Option<Decimal>,
}
