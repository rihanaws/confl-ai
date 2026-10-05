use rust_decimal::Decimal;

use crate::types::{ConservativePriceEstimate, Fill, OrderSide, OrderType};

/// Matches one paper order intent against a market-price reference, price-
/// time priority collapsed to a single conservative fill (paper trading has
/// no real order book depth to walk — it fills fully at the conservative
/// estimate, immediately, in submission order). Decimal arithmetic only.
pub struct MatchingEngine;

impl MatchingEngine {
    /// Returns `None` when the order cannot fill now (stale feed for a
    /// market order, or a limit price that hasn't been touched).
    pub fn try_fill(
        order_type: OrderType,
        side: OrderSide,
        quantity: Decimal,
        limit_price: Option<Decimal>,
        estimate: Option<ConservativePriceEstimate>,
        trade_id: String,
    ) -> Option<Fill> {
        match order_type {
            OrderType::Market => {
                let est = estimate?;
                if !est.is_fresh {
                    return None;
                }
                Some(Fill {
                    exchange_trade_id: trade_id,
                    quantity,
                    price: est.price,
                    fee: Decimal::ZERO,
                    fee_asset: String::new(),
                })
            }
            OrderType::Limit => {
                let est = estimate?;
                if !est.is_fresh {
                    return None;
                }
                let limit = limit_price?;
                let touched = match side {
                    OrderSide::Buy => est.price <= limit,
                    OrderSide::Sell => est.price >= limit,
                };
                if !touched {
                    return None;
                }
                Some(Fill {
                    exchange_trade_id: trade_id,
                    quantity,
                    price: est.price,
                    fee: Decimal::ZERO,
                    fee_asset: String::new(),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn fresh(price: Decimal) -> ConservativePriceEstimate {
        ConservativePriceEstimate { price, is_fresh: true }
    }

    fn stale(price: Decimal) -> ConservativePriceEstimate {
        ConservativePriceEstimate { price, is_fresh: false }
    }

    #[test]
    fn market_order_fills_at_conservative_price() {
        let fill = MatchingEngine::try_fill(
            OrderType::Market,
            OrderSide::Buy,
            dec!(1),
            None,
            Some(fresh(dec!(101))),
            "t1".into(),
        )
        .unwrap();
        assert_eq!(fill.price, dec!(101));
        assert_eq!(fill.quantity, dec!(1));
    }

    #[test]
    fn market_order_stale_feed_no_fill() {
        let fill = MatchingEngine::try_fill(
            OrderType::Market,
            OrderSide::Buy,
            dec!(1),
            None,
            Some(stale(dec!(101))),
            "t1".into(),
        );
        assert!(fill.is_none());
    }

    #[test]
    fn limit_buy_fills_when_price_at_or_below_limit() {
        let fill = MatchingEngine::try_fill(
            OrderType::Limit,
            OrderSide::Buy,
            dec!(1),
            Some(dec!(100)),
            Some(fresh(dec!(99))),
            "t1".into(),
        );
        assert!(fill.is_some());
        assert_eq!(fill.unwrap().price, dec!(99));
    }

    #[test]
    fn limit_buy_no_fill_when_price_above_limit() {
        let fill = MatchingEngine::try_fill(
            OrderType::Limit,
            OrderSide::Buy,
            dec!(1),
            Some(dec!(100)),
            Some(fresh(dec!(101))),
            "t1".into(),
        );
        assert!(fill.is_none());
    }

    #[test]
    fn limit_sell_fills_when_price_at_or_above_limit() {
        let fill = MatchingEngine::try_fill(
            OrderType::Limit,
            OrderSide::Sell,
            dec!(1),
            Some(dec!(100)),
            Some(fresh(dec!(101))),
            "t1".into(),
        );
        assert_eq!(fill.unwrap().price, dec!(101));
    }

    #[test]
    fn no_market_data_no_fill() {
        let fill = MatchingEngine::try_fill(
            OrderType::Market,
            OrderSide::Buy,
            dec!(1),
            None,
            None,
            "t1".into(),
        );
        assert!(fill.is_none());
    }
}
