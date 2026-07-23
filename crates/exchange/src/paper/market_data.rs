use std::collections::HashMap;
use std::time::{Duration, Instant};

use rust_decimal::Decimal;

use crate::types::ConservativePriceEstimate;

#[derive(Debug, Clone, Copy)]
struct BookTop {
    bid: Decimal,
    ask: Decimal,
    last_update: Instant,
}

/// In-memory view of the latest public book top per symbol, fed by the
/// Binance public WS depth/trade stream. Used only to price paper fills —
/// paper orders are never sent to Binance.
pub struct MarketDataProvider {
    books: HashMap<String, BookTop>,
    max_age: Duration,
}

impl MarketDataProvider {
    pub fn new(max_age: Duration) -> Self {
        Self {
            books: HashMap::new(),
            max_age,
        }
    }

    pub fn update(&mut self, symbol: &str, bid: Decimal, ask: Decimal, at: Instant) {
        self.books.insert(
            symbol.to_string(),
            BookTop {
                bid,
                ask,
                last_update: at,
            },
        );
    }

    fn top(&self, symbol: &str) -> Option<&BookTop> {
        self.books.get(symbol)
    }

    fn is_fresh(&self, top: &BookTop, now: Instant) -> bool {
        now.duration_since(top.last_update) <= self.max_age
    }

    /// Conservative estimate: ask for buys (worst case you pay more), bid
    /// for sells (worst case you receive less) — so reservations never
    /// under-cover the eventual fill. `is_fresh = false` when the feed is
    /// stale or gapped; callers must reject market orders in that case.
    pub fn conservative_estimate(
        &self,
        symbol: &str,
        is_buy: bool,
        now: Instant,
    ) -> Option<ConservativePriceEstimate> {
        let top = self.top(symbol)?;
        let price = if is_buy { top.ask } else { top.bid };
        Some(ConservativePriceEstimate {
            price,
            is_fresh: self.is_fresh(top, now),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn fresh_feed_gives_fresh_estimate() {
        let mut md = MarketDataProvider::new(Duration::from_secs(5));
        let t0 = Instant::now();
        md.update("BTCUSDT", dec!(100), dec!(101), t0);
        let est = md.conservative_estimate("BTCUSDT", true, t0).unwrap();
        assert_eq!(est.price, dec!(101)); // buy uses ask
        assert!(est.is_fresh);
        let est = md.conservative_estimate("BTCUSDT", false, t0).unwrap();
        assert_eq!(est.price, dec!(100)); // sell uses bid
    }

    #[test]
    fn stale_feed_flagged_not_fresh() {
        let mut md = MarketDataProvider::new(Duration::from_millis(1));
        let t0 = Instant::now();
        md.update("BTCUSDT", dec!(100), dec!(101), t0);
        std::thread::sleep(Duration::from_millis(5));
        let est = md.conservative_estimate("BTCUSDT", true, Instant::now()).unwrap();
        assert!(!est.is_fresh);
    }

    #[test]
    fn unknown_symbol_has_no_estimate() {
        let md = MarketDataProvider::new(Duration::from_secs(5));
        assert!(md.conservative_estimate("XYZ", true, Instant::now()).is_none());
    }
}
