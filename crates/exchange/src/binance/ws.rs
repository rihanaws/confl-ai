use futures::{SinkExt, StreamExt};
use serde_json::Value;
use tokio_tungstenite::tungstenite::Message;

use crate::error::{ExchangeError, Result};

/// Binance WS origin. Public combined-stream and user-data-stream URLs are
/// built from this the same way REST base URLs are (Fix 1 pattern): origin
/// only, path appended per use.
pub fn ws_base_url(testnet: bool) -> &'static str {
    if testnet {
        "wss://testnet.binance.vision"
    } else {
        "wss://stream.binance.com:9443"
    }
}

pub fn public_stream_url(testnet: bool, streams: &[String]) -> String {
    format!("{}/stream?streams={}", ws_base_url(testnet), streams.join("/"))
}

pub fn user_data_stream_url(testnet: bool, listen_key: &str) -> String {
    format!("{}/ws/{}", ws_base_url(testnet), listen_key)
}

/// One parsed depth-update or trade event off the public combined stream.
#[derive(Debug, Clone)]
pub enum PublicEvent {
    BookTicker {
        symbol: String,
        bid: rust_decimal::Decimal,
        ask: rust_decimal::Decimal,
    },
    Trade {
        symbol: String,
        price: rust_decimal::Decimal,
    },
    Unrecognized,
}

pub fn parse_public_event(raw: &str) -> Result<PublicEvent> {
    let v: Value = serde_json::from_str(raw)
        .map_err(|e| ExchangeError::MarketDataError(format!("invalid ws json: {e}")))?;
    let data = v.get("data").unwrap_or(&v);
    let event_type = data.get("e").and_then(|x| x.as_str());
    let is_book_ticker_shape = data.get("b").is_some() && data.get("a").is_some();
    match event_type {
        Some("bookTicker") => parse_book_ticker(data),
        Some("trade") => parse_trade(data),
        None if is_book_ticker_shape => parse_book_ticker(data),
        _ => Ok(PublicEvent::Unrecognized),
    }
}

fn parse_book_ticker(data: &Value) -> Result<PublicEvent> {
    let symbol = data
        .get("s")
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string();
    let bid = parse_decimal_field(data, "b")?;
    let ask = parse_decimal_field(data, "a")?;
    Ok(PublicEvent::BookTicker { symbol, bid, ask })
}

fn parse_trade(data: &Value) -> Result<PublicEvent> {
    let symbol = data
        .get("s")
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string();
    let price = parse_decimal_field(data, "p")?;
    Ok(PublicEvent::Trade { symbol, price })
}

fn parse_decimal_field(v: &Value, key: &str) -> Result<rust_decimal::Decimal> {
    let s = v
        .get(key)
        .and_then(|x| x.as_str())
        .ok_or_else(|| ExchangeError::MarketDataError(format!("missing field {key}")))?;
    std::str::FromStr::from_str(s)
        .map_err(|e| ExchangeError::MarketDataError(format!("bad decimal for {key}: {e}")))
}

/// Connects to a WS URL over rustls and returns nothing — callers drive the
/// stream themselves via `tokio_tungstenite::connect_async` directly in
/// production code; this wrapper exists so tests can exercise URL/parse
/// logic without a live socket.
pub async fn connect(
    url: &str,
) -> Result<
    tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
> {
    let (stream, _) = tokio_tungstenite::connect_async(url)
        .await
        .map_err(|e| ExchangeError::MarketDataError(format!("ws connect failed: {e}")))?;
    Ok(stream)
}

pub async fn send_ping(
    stream: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Result<()> {
    stream
        .send(Message::Ping(Vec::new()))
        .await
        .map_err(|e| ExchangeError::MarketDataError(format!("ws ping failed: {e}")))
}

#[allow(dead_code)]
async fn _drain_one(
    stream: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Option<Message> {
    stream.next().await.transpose().ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_stream_url_uses_origin_only_base() {
        let url = public_stream_url(true, &["btcusdt@bookTicker".to_string()]);
        assert_eq!(url, "wss://testnet.binance.vision/stream?streams=btcusdt@bookTicker");
        let url = public_stream_url(false, &["btcusdt@bookTicker".to_string()]);
        assert_eq!(url, "wss://stream.binance.com:9443/stream?streams=btcusdt@bookTicker");
    }

    #[test]
    fn user_data_stream_url_uses_origin_only_base() {
        let url = user_data_stream_url(true, "abc123");
        assert_eq!(url, "wss://testnet.binance.vision/ws/abc123");
    }

    #[test]
    fn parses_book_ticker_event() {
        let raw = r#"{"data":{"e":"bookTicker","s":"BTCUSDT","b":"100.00","a":"101.00"}}"#;
        let evt = parse_public_event(raw).unwrap();
        match evt {
            PublicEvent::BookTicker { symbol, bid, ask } => {
                assert_eq!(symbol, "BTCUSDT");
                assert_eq!(bid, rust_decimal_macros::dec!(100));
                assert_eq!(ask, rust_decimal_macros::dec!(101));
            }
            _ => panic!("expected BookTicker"),
        }
    }

    #[test]
    fn parses_trade_event() {
        let raw = r#"{"data":{"e":"trade","s":"BTCUSDT","p":"100.50"}}"#;
        let evt = parse_public_event(raw).unwrap();
        match evt {
            PublicEvent::Trade { symbol, price } => {
                assert_eq!(symbol, "BTCUSDT");
                assert_eq!(price, rust_decimal_macros::dec!(100.50));
            }
            _ => panic!("expected Trade"),
        }
    }
}
