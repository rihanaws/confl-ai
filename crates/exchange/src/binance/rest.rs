use hmac::{Hmac, Mac};
use rust_decimal::Decimal;
use serde_json::Value;
use sha2::Sha256;
use std::str::FromStr;

use crate::error::{ExchangeError, Result};
use crate::types::{
    LotSizeFilter, NotionalFilter, OrderType, PercentPriceRule, PriceFilter, SymbolMeta,
    SymbolStatus,
};

type HmacSha256 = Hmac<Sha256>;

/// Binance REST client config. `base_url` is stored **without** the `/api`
/// suffix (Fix 1) — every endpoint prepends `/api` itself, so testnet vs.
/// live only ever differs by origin.
///
/// ```text
/// Testnet: base_url = "https://testnet.binance.vision"
/// Live:    base_url = "https://api.binance.com"
/// ```
#[derive(Debug, Clone)]
pub struct BinanceConfig {
    pub base_url: String,
    pub api_key: String,
    pub api_secret: String,
}

impl BinanceConfig {
    pub fn testnet(api_key: String, api_secret: String) -> Self {
        Self {
            base_url: "https://testnet.binance.vision".into(),
            api_key,
            api_secret,
        }
    }

    pub fn live(api_key: String, api_secret: String) -> Self {
        Self {
            base_url: "https://api.binance.com".into(),
            api_key,
            api_secret,
        }
    }
}

/// HMAC-SHA256 signature over a query string, hex-encoded, per Binance's
/// signed-endpoint convention.
pub fn sign_query(secret: &str, query: &str) -> Result<String> {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .map_err(|e| ExchangeError::MarketDataError(format!("hmac key error: {e}")))?;
    mac.update(query.as_bytes());
    Ok(hex_encode(&mac.finalize().into_bytes()))
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub struct BinanceRestClient {
    config: BinanceConfig,
    http: reqwest::Client,
}

impl BinanceRestClient {
    pub fn new(config: BinanceConfig) -> Self {
        Self {
            config,
            http: reqwest::Client::new(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}/api{}", self.config.base_url, path)
    }

    pub fn exchange_info_url(&self) -> String {
        self.url("/v3/exchangeInfo")
    }

    pub fn order_url(&self) -> String {
        self.url("/v3/order")
    }

    pub fn open_orders_url(&self) -> String {
        self.url("/v3/openOrders")
    }

    pub fn account_url(&self) -> String {
        self.url("/v3/account")
    }

    pub fn all_orders_url(&self) -> String {
        self.url("/v3/allOrders")
    }

    pub fn my_trades_url(&self) -> String {
        self.url("/v3/myTrades")
    }

    pub fn user_data_stream_url(&self) -> String {
        self.url("/v3/userDataStream")
    }

    pub fn book_ticker_url(&self, symbol: &str) -> String {
        format!("{}?symbol={symbol}", self.url("/v3/ticker/bookTicker"))
    }

    pub fn api_key(&self) -> &str {
        &self.config.api_key
    }

    pub fn sign(&self, query: &str) -> Result<String> {
        sign_query(&self.config.api_secret, query)
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// Fetches and parses `GET /api/v3/exchangeInfo` for every symbol
    /// returned (unauthenticated — exchangeInfo is public). Used by the
    /// server's periodic `SymbolMetadataStore` refresh; `crates/exchange`
    /// stays sqlx-free, this is a plain network call.
    pub async fn fetch_exchange_info(&self) -> Result<Vec<SymbolMeta>> {
        let resp = self
            .http
            .get(self.exchange_info_url())
            .send()
            .await
            .map_err(|e| ExchangeError::MarketDataError(format!("exchangeInfo request failed: {e}")))?;
        let body: Value = resp
            .json()
            .await
            .map_err(|e| ExchangeError::MarketDataError(format!("bad exchangeInfo body: {e}")))?;
        let symbols = body
            .get("symbols")
            .and_then(|v| v.as_array())
            .ok_or_else(|| ExchangeError::MarketDataError("exchangeInfo missing symbols[]".into()))?;
        symbols.iter().map(parse_symbol_meta).collect()
    }

    /// Fetches `GET /api/v3/ticker/bookTicker` for one symbol (public, no
    /// signing) and returns (bid, ask).
    pub async fn fetch_book_ticker(&self, symbol: &str) -> Result<(Decimal, Decimal)> {
        let resp = self
            .http
            .get(self.book_ticker_url(symbol))
            .send()
            .await
            .map_err(|e| ExchangeError::MarketDataError(format!("bookTicker request failed: {e}")))?;
        let body: Value = resp
            .json()
            .await
            .map_err(|e| ExchangeError::MarketDataError(format!("bad bookTicker body: {e}")))?;
        let bid = dec_field(&body, "bidPrice")?;
        let ask = dec_field(&body, "askPrice")?;
        Ok((bid, ask))
    }
}

fn dec_field(v: &Value, key: &str) -> Result<Decimal> {
    let s = v
        .get(key)
        .and_then(|x| x.as_str())
        .ok_or_else(|| ExchangeError::FilterViolation(format!("missing field {key}")))?;
    Decimal::from_str(s)
        .map_err(|e| ExchangeError::FilterViolation(format!("bad decimal for {key}: {e}")))
}

/// Parses one entry of Binance `GET /api/v3/exchangeInfo`'s `symbols[]` array
/// into our internal `SymbolMeta`, including the full NOTIONAL and
/// PERCENT_PRICE/PERCENT_PRICE_BY_SIDE filter shapes (Fix 3) — not just the
/// first filter variant encountered.
pub fn parse_symbol_meta(entry: &Value) -> Result<SymbolMeta> {
    let symbol = entry
        .get("symbol")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ExchangeError::FilterViolation("missing symbol".into()))?
        .to_string();
    let base_asset = entry
        .get("baseAsset")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let quote_asset = entry
        .get("quoteAsset")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let status = match entry.get("status").and_then(|v| v.as_str()) {
        Some("TRADING") => SymbolStatus::Trading,
        Some("BREAK") => SymbolStatus::Break,
        Some("HALT") => SymbolStatus::Halt,
        Some("AUCTION_MATCH") => SymbolStatus::AuctionMatch,
        _ => SymbolStatus::Break,
    };
    let allowed_order_types = entry
        .get("orderTypes")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|t| match t.as_str() {
                    Some("LIMIT") => Some(OrderType::Limit),
                    Some("MARKET") => Some(OrderType::Market),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();

    let filters = entry
        .get("filters")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut lot_size = LotSizeFilter {
        min_qty: Decimal::ZERO,
        max_qty: Decimal::MAX,
        step_size: Decimal::ZERO,
    };
    let mut market_lot_size = lot_size.clone();
    let mut price_filter = PriceFilter {
        min_price: Decimal::ZERO,
        max_price: Decimal::MAX,
        tick_size: Decimal::ZERO,
    };
    let mut notional = NotionalFilter {
        min_notional: None,
        max_notional: None,
        apply_min_to_market: false,
        apply_max_to_market: false,
        avg_price_mins: 0,
    };
    let mut percent_price = PercentPriceRule::General {
        multiplier_up: Decimal::MAX,
        multiplier_down: Decimal::ZERO,
        avg_price_mins: 0,
    };

    for f in &filters {
        match f.get("filterType").and_then(|v| v.as_str()) {
            Some("LOT_SIZE") => {
                lot_size = LotSizeFilter {
                    min_qty: dec_field(f, "minQty")?,
                    max_qty: dec_field(f, "maxQty")?,
                    step_size: dec_field(f, "stepSize")?,
                };
            }
            Some("MARKET_LOT_SIZE") => {
                market_lot_size = LotSizeFilter {
                    min_qty: dec_field(f, "minQty")?,
                    max_qty: dec_field(f, "maxQty")?,
                    step_size: dec_field(f, "stepSize")?,
                };
            }
            Some("PRICE_FILTER") => {
                price_filter = PriceFilter {
                    min_price: dec_field(f, "minPrice")?,
                    max_price: dec_field(f, "maxPrice")?,
                    tick_size: dec_field(f, "tickSize")?,
                };
            }
            Some("NOTIONAL") => {
                notional = NotionalFilter {
                    min_notional: f.get("minNotional").and_then(|v| v.as_str()).and_then(|s| Decimal::from_str(s).ok()),
                    max_notional: f.get("maxNotional").and_then(|v| v.as_str()).and_then(|s| Decimal::from_str(s).ok()),
                    apply_min_to_market: f
                        .get("applyMinToMarket")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                    apply_max_to_market: f
                        .get("applyMaxToMarket")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                    avg_price_mins: f
                        .get("avgPriceMins")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as u32,
                };
            }
            Some("PERCENT_PRICE_BY_SIDE") => {
                percent_price = PercentPriceRule::BySide {
                    bid_multiplier_up: dec_field(f, "bidMultiplierUp")?,
                    bid_multiplier_down: dec_field(f, "bidMultiplierDown")?,
                    ask_multiplier_up: dec_field(f, "askMultiplierUp")?,
                    ask_multiplier_down: dec_field(f, "askMultiplierDown")?,
                    avg_price_mins: f
                        .get("avgPriceMins")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as u32,
                };
            }
            Some("PERCENT_PRICE") => {
                percent_price = PercentPriceRule::General {
                    multiplier_up: dec_field(f, "multiplierUp")?,
                    multiplier_down: dec_field(f, "multiplierDown")?,
                    avg_price_mins: f
                        .get("avgPriceMins")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as u32,
                };
            }
            _ => {}
        }
    }

    Ok(SymbolMeta {
        symbol,
        base_asset,
        quote_asset,
        status,
        allowed_order_types,
        lot_size,
        market_lot_size,
        price_filter,
        notional,
        percent_price,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn testnet_and_live_urls_are_origin_only() {
        let testnet = BinanceRestClient::new(BinanceConfig::testnet("k".into(), "s".into()));
        assert_eq!(
            testnet.exchange_info_url(),
            "https://testnet.binance.vision/api/v3/exchangeInfo"
        );
        assert_eq!(testnet.order_url(), "https://testnet.binance.vision/api/v3/order");
        assert_eq!(
            testnet.open_orders_url(),
            "https://testnet.binance.vision/api/v3/openOrders"
        );
        assert_eq!(testnet.account_url(), "https://testnet.binance.vision/api/v3/account");
        assert_eq!(
            testnet.all_orders_url(),
            "https://testnet.binance.vision/api/v3/allOrders"
        );
        assert_eq!(testnet.my_trades_url(), "https://testnet.binance.vision/api/v3/myTrades");
        assert_eq!(
            testnet.user_data_stream_url(),
            "https://testnet.binance.vision/api/v3/userDataStream"
        );

        let live = BinanceRestClient::new(BinanceConfig::live("k".into(), "s".into()));
        assert_eq!(live.exchange_info_url(), "https://api.binance.com/api/v3/exchangeInfo");
        assert_eq!(live.order_url(), "https://api.binance.com/api/v3/order");
    }

    #[test]
    fn hmac_signature_is_deterministic_hex() {
        let sig1 = sign_query("secret", "symbol=BTCUSDT&side=BUY").unwrap();
        let sig2 = sign_query("secret", "symbol=BTCUSDT&side=BUY").unwrap();
        assert_eq!(sig1, sig2);
        assert!(sig1.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(sig1.len(), 64); // SHA256 -> 32 bytes -> 64 hex chars
    }

    #[test]
    fn different_queries_produce_different_signatures() {
        let sig1 = sign_query("secret", "symbol=BTCUSDT").unwrap();
        let sig2 = sign_query("secret", "symbol=ETHUSDT").unwrap();
        assert_ne!(sig1, sig2);
    }

    fn sample_entry(percent_price_filter: Value) -> Value {
        serde_json::json!({
            "symbol": "BTCUSDT",
            "baseAsset": "BTC",
            "quoteAsset": "USDT",
            "status": "TRADING",
            "orderTypes": ["LIMIT", "MARKET"],
            "filters": [
                { "filterType": "LOT_SIZE", "minQty": "0.00001000", "maxQty": "9000.00000000", "stepSize": "0.00001000" },
                { "filterType": "MARKET_LOT_SIZE", "minQty": "0.00001000", "maxQty": "100.00000000", "stepSize": "0.00001000" },
                { "filterType": "PRICE_FILTER", "minPrice": "0.01000000", "maxPrice": "1000000.00000000", "tickSize": "0.01000000" },
                {
                    "filterType": "NOTIONAL",
                    "minNotional": "5.00000000",
                    "maxNotional": "9000000.00000000",
                    "applyMinToMarket": true,
                    "applyMaxToMarket": false,
                    "avgPriceMins": 5
                },
                percent_price_filter,
            ]
        })
    }

    #[test]
    fn parses_market_lot_size_and_full_notional_shape() {
        let entry = sample_entry(serde_json::json!({
            "filterType": "PERCENT_PRICE",
            "multiplierUp": "1.2000",
            "multiplierDown": "0.8000",
            "avgPriceMins": 5
        }));
        let meta = parse_symbol_meta(&entry).unwrap();
        assert_eq!(meta.market_lot_size.step_size, rust_decimal_macros::dec!(0.00001));
        assert_eq!(meta.notional.min_notional, Some(rust_decimal_macros::dec!(5)));
        assert_eq!(meta.notional.max_notional, Some(rust_decimal_macros::dec!(9000000)));
        assert!(meta.notional.apply_min_to_market);
        assert!(!meta.notional.apply_max_to_market);
    }

    #[test]
    fn parses_percent_price_by_side_variant_specifically() {
        let entry = sample_entry(serde_json::json!({
            "filterType": "PERCENT_PRICE_BY_SIDE",
            "bidMultiplierUp": "1.2000",
            "bidMultiplierDown": "0.8000",
            "askMultiplierUp": "1.2000",
            "askMultiplierDown": "0.8000",
            "avgPriceMins": 5
        }));
        let meta = parse_symbol_meta(&entry).unwrap();
        match meta.percent_price {
            PercentPriceRule::BySide { ask_multiplier_down, .. } => {
                assert_eq!(ask_multiplier_down, rust_decimal_macros::dec!(0.8));
            }
            PercentPriceRule::General { .. } => panic!("expected BySide, PERCENT_PRICE_BY_SIDE filter was present"),
        }
    }

    /// Live testnet network call. Requires internet access; run with the
    /// rest of `cargo test --workspace` in this environment where testnet
    /// connectivity is available.
    #[tokio::test]
    async fn fetch_exchange_info_against_real_testnet() {
        let client = BinanceRestClient::new(BinanceConfig::testnet(String::new(), String::new()));
        let symbols = client.fetch_exchange_info().await.expect("testnet exchangeInfo reachable");
        assert!(!symbols.is_empty());
        let btc = symbols.iter().find(|s| s.symbol == "BTCUSDT").expect("BTCUSDT listed on testnet");
        assert!(btc.lot_size.step_size > Decimal::ZERO);
        assert!(matches!(btc.percent_price, PercentPriceRule::BySide { .. } | PercentPriceRule::General { .. }));
    }

    #[tokio::test]
    async fn fetch_book_ticker_against_real_testnet() {
        let client = BinanceRestClient::new(BinanceConfig::testnet(String::new(), String::new()));
        let (bid, ask) = client.fetch_book_ticker("BTCUSDT").await.expect("testnet bookTicker reachable");
        assert!(bid > Decimal::ZERO);
        assert!(ask >= bid);
    }
}
