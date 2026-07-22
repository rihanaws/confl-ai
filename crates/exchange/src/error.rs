use rust_decimal::Decimal;

#[derive(Debug, thiserror::Error, Clone, PartialEq)]
pub enum ExchangeError {
    #[error("fill of {fill_qty} exceeds remaining {remaining_qty} on order {order_id}")]
    FillExceedsRemaining {
        order_id: String,
        fill_qty: Decimal,
        remaining_qty: Decimal,
    },
    #[error("insufficient balance: need {need} {asset}, have {available}")]
    InsufficientBalance {
        asset: String,
        need: Decimal,
        available: Decimal,
    },
    #[error("market data error: {0}")]
    MarketDataError(String),
    #[error("invalid order state transition: {from} -> {to}")]
    InvalidTransition { from: String, to: String },
    #[error("symbol not found: {0}")]
    SymbolNotFound(String),
    #[error("filter violation: {0}")]
    FilterViolation(String),
    #[error("encryption error: {0}")]
    EncryptionError(String),
}

pub type Result<T> = std::result::Result<T, ExchangeError>;
