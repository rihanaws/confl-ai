//! Confluence core: pure, deterministic risk-rule engine.
//!
//! No I/O lives here. The server layer assembles an [`AccountSnapshot`] from
//! the database inside a single transaction, calls [`evaluate`], persists the
//! resulting decision to the audit log, and enforces it.

pub mod engine;
pub mod types;

pub use engine::{circuit_breaker_should_trip, evaluate};
pub use types::{
    AccountSnapshot, Decision, OpenPosition, PositionSide, RiskConfig, TradeIntent, TradeSide,
    Violation,
};
