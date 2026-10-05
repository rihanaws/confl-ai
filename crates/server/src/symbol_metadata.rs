//! Shared, refreshable exchange-metadata store. Periodic refresh; fails
//! closed on stale data — a symbol missing or older than the freshness
//! window is treated as absent, not as "use whatever we last had."

use std::sync::Arc;
use std::time::{Duration, Instant};

use confluence_exchange::types::{SymbolMeta, SymbolStatus};
use dashmap::DashMap;

pub struct SymbolMetadataStore {
    symbols: DashMap<String, SymbolMeta>,
    last_refreshed: Arc<std::sync::RwLock<Option<Instant>>>,
    max_age: Duration,
}

impl SymbolMetadataStore {
    pub fn new(max_age: Duration) -> Self {
        Self {
            symbols: DashMap::new(),
            last_refreshed: Arc::new(std::sync::RwLock::new(None)),
            max_age,
        }
    }

    pub fn replace_all(&self, metas: Vec<SymbolMeta>) {
        self.symbols.clear();
        for m in metas {
            self.symbols.insert(m.symbol.clone(), m);
        }
        *self.last_refreshed.write().unwrap() = Some(Instant::now());
    }

    /// Returns `None` if the symbol is unknown OR the store hasn't refreshed
    /// within `max_age` — fail-closed, never serve metadata we can't vouch
    /// for as current.
    pub fn get(&self, symbol: &str) -> Option<SymbolMeta> {
        let refreshed = (*self.last_refreshed.read().unwrap())?;
        if refreshed.elapsed() > self.max_age {
            return None;
        }
        self.symbols.get(symbol).map(|r| r.value().clone())
    }

    pub fn is_stale(&self) -> bool {
        match *self.last_refreshed.read().unwrap() {
            None => true,
            Some(t) => t.elapsed() > self.max_age,
        }
    }

    /// Symbols that can actually pass `validate_order` right now: the same
    /// `status == Trading` gate the validator applies, on fresh metadata
    /// only. Drives what the market-data poller watches, so a symbol never
    /// gets through order validation without a price feed backing it —
    /// and the poller doesn't waste requests on Break/Halt/AuctionMatch
    /// symbols the validator would reject anyway.
    pub fn tradable_symbols(&self) -> Vec<String> {
        if self.is_stale() {
            return vec![];
        }
        self.symbols
            .iter()
            .filter(|e| e.value().status == SymbolStatus::Trading)
            .map(|e| e.key().clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use confluence_exchange::types::{
        LotSizeFilter, NotionalFilter, OrderType, PercentPriceRule, PriceFilter, SymbolStatus,
    };
    use rust_decimal_macros::dec;

    fn meta(symbol: &str) -> SymbolMeta {
        SymbolMeta {
            symbol: symbol.into(),
            base_asset: "BTC".into(),
            quote_asset: "USDT".into(),
            status: SymbolStatus::Trading,
            allowed_order_types: vec![OrderType::Limit, OrderType::Market],
            lot_size: LotSizeFilter {
                min_qty: dec!(0.0001),
                max_qty: dec!(1000),
                step_size: dec!(0.0001),
            },
            market_lot_size: LotSizeFilter {
                min_qty: dec!(0.0001),
                max_qty: dec!(1000),
                step_size: dec!(0.0001),
            },
            price_filter: PriceFilter {
                min_price: dec!(0.01),
                max_price: dec!(1000000),
                tick_size: dec!(0.01),
            },
            notional: NotionalFilter {
                min_notional: Some(dec!(10)),
                max_notional: Some(dec!(9000000)),
                apply_min_to_market: true,
                apply_max_to_market: false,
                avg_price_mins: 5,
            },
            percent_price: PercentPriceRule::BySide {
                bid_multiplier_up: dec!(1.2),
                bid_multiplier_down: dec!(0.8),
                ask_multiplier_up: dec!(1.2),
                ask_multiplier_down: dec!(0.8),
                avg_price_mins: 5,
            },
        }
    }

    #[test]
    fn stale_before_first_refresh() {
        let store = SymbolMetadataStore::new(Duration::from_secs(60));
        assert!(store.is_stale());
        assert!(store.get("BTCUSDT").is_none());
    }

    #[test]
    fn fresh_after_refresh_returns_full_filter_shape() {
        let store = SymbolMetadataStore::new(Duration::from_secs(60));
        store.replace_all(vec![meta("BTCUSDT")]);
        assert!(!store.is_stale());
        let m = store.get("BTCUSDT").expect("present");
        assert_eq!(m.market_lot_size.step_size, dec!(0.0001));
        assert_eq!(m.notional.min_notional, Some(dec!(10)));
        assert!(m.notional.apply_min_to_market);
        assert!(!m.notional.apply_max_to_market);
        match m.percent_price {
            PercentPriceRule::BySide { ask_multiplier_down, .. } => {
                assert_eq!(ask_multiplier_down, dec!(0.8));
            }
            PercentPriceRule::General { .. } => panic!("expected BySide arm to survive store roundtrip"),
        }
    }

    #[test]
    fn unknown_symbol_absent_even_when_fresh() {
        let store = SymbolMetadataStore::new(Duration::from_secs(60));
        store.replace_all(vec![meta("BTCUSDT")]);
        assert!(store.get("ETHUSDT").is_none());
    }

    fn meta_with_status(symbol: &str, status: SymbolStatus) -> SymbolMeta {
        SymbolMeta { status, ..meta(symbol) }
    }

    #[test]
    fn tradable_symbols_excludes_non_trading_status() {
        let store = SymbolMetadataStore::new(Duration::from_secs(60));
        store.replace_all(vec![
            meta_with_status("BTCUSDT", SymbolStatus::Trading),
            meta_with_status("ETHUSDT", SymbolStatus::Halt),
            meta_with_status("BNBUSDT", SymbolStatus::Break),
        ]);
        let mut tradable = store.tradable_symbols();
        tradable.sort();
        assert_eq!(tradable, vec!["BTCUSDT".to_string()]);
    }

    #[test]
    fn tradable_symbols_empty_when_stale() {
        let store = SymbolMetadataStore::new(Duration::from_millis(1));
        store.replace_all(vec![meta_with_status("BTCUSDT", SymbolStatus::Trading)]);
        std::thread::sleep(Duration::from_millis(5));
        assert!(store.tradable_symbols().is_empty());
    }
}
