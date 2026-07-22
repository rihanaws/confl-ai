//! Binance Spot exchange adapter + paper trading engine. Zero `sqlx`, zero
//! database I/O anywhere in this crate — the server owns all persistence and
//! passes plain data across the `ExchangeAdapter` trait boundary.

pub mod adapter;
pub mod config;
pub mod error;
pub mod order_state;
pub mod outbox;
pub mod quantize;
pub mod types;

pub mod paper;
pub mod binance;

pub use adapter::ExchangeAdapter;
pub use error::{ExchangeError, Result};
