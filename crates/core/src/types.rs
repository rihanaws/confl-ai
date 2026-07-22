use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Direction of a proposed trade.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TradeSide {
    Buy,
    Sell,
}

/// Direction of an existing open position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PositionSide {
    Long,
    Short,
}

/// A proposed trade, before any order exists. Quantities and prices are
/// exact decimals; notional value is `quantity * price`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeIntent {
    pub symbol: String,
    pub side: TradeSide,
    pub quantity: Decimal,
    pub price: Decimal,
}

impl TradeIntent {
    pub fn notional(&self) -> Decimal {
        self.quantity * self.price
    }
}

/// An open position as the risk engine needs to see it. `notional` is the
/// current absolute exposure of the position (quantity * mark or entry
/// price), computed by the caller. `quantity` is the position size in units
/// of the asset; flip-vs-reduce classification compares quantities, never
/// notionals, so it cannot be skewed by a divergent limit price.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenPosition {
    pub symbol: String,
    pub side: PositionSide,
    pub quantity: Decimal,
    pub notional: Decimal,
}

/// Per-account risk limits. All `*_pct` values are percentages
/// (e.g. `25` means 25% of equity), not fractions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskConfig {
    /// Daily realized-loss limit as % of equity; reaching it trips the
    /// circuit breaker for the rest of the trading day (UTC).
    pub daily_max_loss_pct: Decimal,
    /// Max notional of a single trade as % of equity.
    pub max_position_pct_equity: Decimal,
    /// Max number of concurrently open positions.
    pub max_concurrent_positions: i32,
    /// Max combined notional exposure within one correlation group as % of equity.
    pub max_correlated_exposure_pct: Decimal,
}

/// Everything the engine needs to know about an account at the moment of
/// evaluation. Assembled by the server from the DB in one transaction.
#[derive(Debug, Clone)]
pub struct AccountSnapshot {
    /// Current account equity. Must be positive for any trade to pass.
    pub equity: Decimal,
    /// Today's realized loss so far, as a non-negative number
    /// (a net profitable day is `0`).
    pub today_realized_loss: Decimal,
    /// User kill switch state. When engaged, everything is rejected.
    pub kill_switch_engaged: bool,
    /// Whether the daily circuit breaker has already been tripped today.
    pub circuit_breaker_tripped: bool,
    pub open_positions: Vec<OpenPosition>,
    /// symbol -> correlation group name. Symbols absent from the map are
    /// uncorrelated and only subject to the per-trade size limit.
    pub correlation_groups: HashMap<String, String>,
}

/// A single risk-rule violation. `Rejected` decisions carry every violation
/// found (not just the first) so the audit log shows the complete picture.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "rule", rename_all = "snake_case")]
pub enum Violation {
    /// User kill switch is engaged. Absolute: when present it is the only
    /// violation reported, since all other evaluation is bypassed.
    KillSwitchEngaged,
    /// Negative quantity or price. The server boundary rejects these before
    /// the engine runs; this is defense in depth, because a negative
    /// notional would otherwise slide through every sizing comparison.
    InvalidIntent { quantity: Decimal, price: Decimal },
    /// Daily max-loss circuit breaker is (or must now be) tripped.
    CircuitBreakerTripped {
        today_realized_loss: Decimal,
        daily_loss_limit: Decimal,
    },
    /// Account equity is zero or negative; no trade can be sized.
    NonPositiveEquity { equity: Decimal },
    /// Trade notional exceeds the per-trade % of equity limit.
    PositionSizeExceeded {
        trade_notional: Decimal,
        limit_notional: Decimal,
        limit_pct: Decimal,
    },
    /// Opening this position would exceed the concurrent-position cap.
    MaxConcurrentPositionsExceeded { open_count: i64, limit: i32 },
    /// Combined exposure in the intent's correlation group would exceed the cap.
    CorrelatedExposureExceeded {
        group: String,
        combined_notional: Decimal,
        limit_notional: Decimal,
        limit_pct: Decimal,
    },
}

/// Outcome of evaluating a trade intent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum Decision {
    Approved,
    Rejected { violations: Vec<Violation> },
}

impl Decision {
    pub fn is_approved(&self) -> bool {
        matches!(self, Decision::Approved)
    }
}
