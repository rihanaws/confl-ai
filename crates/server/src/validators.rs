//! Order-intent validation against exchange filters. Runs in the route
//! handler **before** any transaction opens (fail fast, no DB round trip for
//! invalid input); `confluence_core::evaluate()` runs later, inside the
//! transaction, after reservation.

use confluence_exchange::types::{
    NotionalFilter, OrderIntent, OrderSide, OrderType, PercentPriceRule, PriceEstimate, SymbolMeta,
    SymbolStatus,
};
use confluence_exchange::quantize::{is_aligned, qdown};
use rust_decimal::Decimal;

#[derive(Debug, Clone, PartialEq)]
pub struct ValidationError(pub String);

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

type Result<T> = std::result::Result<T, ValidationError>;

fn reject(msg: impl Into<String>) -> ValidationError {
    ValidationError(msg.into())
}

/// Top-level entry point: symbol status/type checks, common lot/notional
/// filters, then dispatches to the order-type-specific validator.
pub fn validate_order(
    intent: &OrderIntent,
    symbol: &SymbolMeta,
    price_est: Option<PriceEstimate>,
) -> Result<()> {
    if symbol.status != SymbolStatus::Trading {
        return Err(reject(format!(
            "symbol {} is not currently trading (status: {:?})",
            symbol.symbol, symbol.status
        )));
    }
    if !symbol.allowed_order_types.contains(&intent.order_type) {
        return Err(reject(format!(
            "order type {:?} not allowed for {}",
            intent.order_type, symbol.symbol
        )));
    }

    let lot = match intent.order_type {
        OrderType::Market => &symbol.market_lot_size,
        OrderType::Limit => &symbol.lot_size,
    };
    if intent.quantity < lot.min_qty || intent.quantity > lot.max_qty {
        return Err(reject(format!(
            "quantity {} outside [{}, {}] for {}",
            intent.quantity, lot.min_qty, lot.max_qty, symbol.symbol
        )));
    }
    if !is_aligned(intent.quantity, lot.step_size) {
        return Err(reject(format!(
            "quantity {} not aligned to step size {}; nearest valid quantity is {}",
            intent.quantity,
            lot.step_size,
            qdown(intent.quantity, lot.step_size)
        )));
    }

    match intent.order_type {
        OrderType::Limit => validate_limit(intent, symbol, price_est),
        OrderType::Market => validate_market(intent, symbol, price_est),
    }
}

fn validate_limit(
    intent: &OrderIntent,
    symbol: &SymbolMeta,
    price_est: Option<PriceEstimate>,
) -> Result<()> {
    let price = intent
        .price
        .ok_or_else(|| reject("limit order requires a price"))?;

    let pf = &symbol.price_filter;
    if price < pf.min_price || price > pf.max_price {
        return Err(reject(format!(
            "price {} outside [{}, {}] for {}",
            price, pf.min_price, pf.max_price, symbol.symbol
        )));
    }
    if !is_aligned(price, pf.tick_size) {
        return Err(reject(format!(
            "price {} not aligned to tick size {}; nearest valid price is {}",
            price,
            pf.tick_size,
            qdown(price, pf.tick_size)
        )));
    }

    check_percent_price(intent.side, price, &symbol.percent_price, price_est)?;
    check_notional(intent.side, intent.quantity, price, false, &symbol.notional)?;
    Ok(())
}

fn validate_market(
    intent: &OrderIntent,
    symbol: &SymbolMeta,
    price_est: Option<PriceEstimate>,
) -> Result<()> {
    let est = price_est.ok_or_else(|| reject("no price estimate available for market order"))?;
    if !est.is_fresh {
        return Err(reject("market data feed is stale; market order rejected"));
    }
    check_notional(intent.side, intent.quantity, est.price, true, &symbol.notional)?;
    Ok(())
}

/// PERCENT_PRICE / PERCENT_PRICE_BY_SIDE (Fix 3): both variants handled.
/// Limit buy checks bid-side bounds under `BySide`, else `General`
/// bidirectionally; limit sell checks ask-side bounds under `BySide`.
fn check_percent_price(
    side: OrderSide,
    price: Decimal,
    rule: &PercentPriceRule,
    price_est: Option<PriceEstimate>,
) -> Result<()> {
    let Some(est) = price_est else {
        // No reference price available: cannot bound-check, but a limit
        // order does not require freshness the way a market order does.
        return Ok(());
    };
    let reference = est.price;

    match rule {
        PercentPriceRule::General {
            multiplier_up,
            multiplier_down,
            ..
        } => {
            let upper = reference * multiplier_up;
            let lower = reference * multiplier_down;
            if price > upper || price < lower {
                return Err(reject(format!(
                    "price {price} outside PERCENT_PRICE bounds [{lower}, {upper}]"
                )));
            }
        }
        PercentPriceRule::BySide {
            bid_multiplier_up,
            bid_multiplier_down,
            ask_multiplier_up,
            ask_multiplier_down,
            ..
        } => {
            let (up, down) = match side {
                OrderSide::Buy => (bid_multiplier_up, bid_multiplier_down),
                OrderSide::Sell => (ask_multiplier_up, ask_multiplier_down),
            };
            let upper = reference * up;
            let lower = reference * down;
            if price > upper || price < lower {
                return Err(reject(format!(
                    "price {price} outside PERCENT_PRICE_BY_SIDE bounds [{lower}, {upper}] for {side:?}"
                )));
            }
        }
    }
    Ok(())
}

/// NOTIONAL (Fix 3): both min/max bounds, both apply-to-market flags.
fn check_notional(
    _side: OrderSide,
    quantity: Decimal,
    price: Decimal,
    is_market: bool,
    filter: &NotionalFilter,
) -> Result<()> {
    let notional = quantity * price;
    if let Some(min) = filter.min_notional {
        let applies = !is_market || filter.apply_min_to_market;
        if applies && notional < min {
            return Err(reject(format!("notional {notional} below MIN_NOTIONAL {min}")));
        }
    }
    if let Some(max) = filter.max_notional {
        let applies = !is_market || filter.apply_max_to_market;
        if applies && notional > max {
            return Err(reject(format!("notional {notional} above MAX_NOTIONAL {max}")));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use confluence_exchange::types::{LotSizeFilter, PriceFilter, PriceSource};
    use rust_decimal_macros::dec;

    fn meta() -> SymbolMeta {
        SymbolMeta {
            symbol: "BTCUSDT".into(),
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
                max_notional: Some(dec!(1000000)),
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

    fn est(price: Decimal, is_fresh: bool) -> PriceEstimate {
        PriceEstimate {
            price,
            source: PriceSource::BestAsk,
            is_fresh,
        }
    }

    fn limit_intent(side: OrderSide, qty: Decimal, price: Decimal) -> OrderIntent {
        OrderIntent {
            symbol: "BTCUSDT".into(),
            side,
            order_type: OrderType::Limit,
            quantity: qty,
            price: Some(price),
        }
    }

    fn market_intent(side: OrderSide, qty: Decimal) -> OrderIntent {
        OrderIntent {
            symbol: "BTCUSDT".into(),
            side,
            order_type: OrderType::Market,
            quantity: qty,
            price: None,
        }
    }

    #[test]
    fn valid_limit_order_passes() {
        let intent = limit_intent(OrderSide::Buy, dec!(1), dec!(100));
        assert!(validate_order(&intent, &meta(), Some(est(dec!(100), true))).is_ok());
    }

    #[test]
    fn non_trading_symbol_rejected() {
        let mut m = meta();
        m.status = SymbolStatus::Halt;
        let intent = limit_intent(OrderSide::Buy, dec!(1), dec!(100));
        assert!(validate_order(&intent, &m, None).is_err());
    }

    #[test]
    fn misaligned_quantity_rejected() {
        let intent = limit_intent(OrderSide::Buy, dec!(1.00005), dec!(100));
        assert!(validate_order(&intent, &meta(), Some(est(dec!(100), true))).is_err());
    }

    #[test]
    fn market_order_requires_fresh_estimate() {
        let intent = market_intent(OrderSide::Buy, dec!(1));
        let err = validate_order(&intent, &meta(), Some(est(dec!(100), false))).unwrap_err();
        assert!(err.0.contains("stale"));
    }

    #[test]
    fn market_order_no_estimate_rejected() {
        let intent = market_intent(OrderSide::Buy, dec!(1));
        assert!(validate_order(&intent, &meta(), None).is_err());
    }

    #[test]
    fn market_order_min_notional_applies_when_flag_set() {
        let intent = market_intent(OrderSide::Buy, dec!(0.0001)); // notional ~0.01, below min 10
        let err = validate_order(&intent, &meta(), Some(est(dec!(100), true))).unwrap_err();
        assert!(err.0.contains("MIN_NOTIONAL"));
    }

    #[test]
    fn market_order_max_notional_not_applied_when_flag_unset() {
        let mut m = meta();
        m.market_lot_size.max_qty = dec!(100000);
        let intent = market_intent(OrderSide::Buy, dec!(90000)); // notional 9,000,000 > max 1,000,000
        // apply_max_to_market is false in meta(), so market orders are exempt.
        assert!(validate_order(&intent, &m, Some(est(dec!(100), true))).is_ok());
    }

    #[test]
    fn limit_order_percent_price_by_side_buy_uses_ask_bounds() {
        // reference 100, ask bounds [80, 120]; buy at 125 is outside.
        let intent = limit_intent(OrderSide::Buy, dec!(1), dec!(125));
        let err = validate_order(&intent, &meta(), Some(est(dec!(100), true))).unwrap_err();
        assert!(err.0.contains("PERCENT_PRICE_BY_SIDE"));
    }

    #[test]
    fn limit_order_percent_price_by_side_sell_uses_bid_bounds() {
        let intent = limit_intent(OrderSide::Sell, dec!(1), dec!(70));
        let err = validate_order(&intent, &meta(), Some(est(dec!(100), true))).unwrap_err();
        assert!(err.0.contains("PERCENT_PRICE_BY_SIDE"));
    }

    #[test]
    fn limit_order_percent_price_general_variant_checked_bidirectionally() {
        let mut m = meta();
        m.percent_price = PercentPriceRule::General {
            multiplier_up: dec!(1.1),
            multiplier_down: dec!(0.9),
            avg_price_mins: 5,
        };
        let intent = limit_intent(OrderSide::Buy, dec!(1), dec!(115)); // > 110 upper bound
        assert!(validate_order(&intent, &m, Some(est(dec!(100), true))).is_err());
        let intent = limit_intent(OrderSide::Sell, dec!(1), dec!(85)); // < 90 lower bound
        assert!(validate_order(&intent, &m, Some(est(dec!(100), true))).is_err());
    }

    #[test]
    fn disallowed_order_type_rejected() {
        let mut m = meta();
        m.allowed_order_types = vec![OrderType::Limit];
        let intent = market_intent(OrderSide::Buy, dec!(1));
        assert!(validate_order(&intent, &m, Some(est(dec!(100), true))).is_err());
    }
}
