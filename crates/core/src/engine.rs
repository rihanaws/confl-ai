use rust_decimal::Decimal;

use crate::types::{
    AccountSnapshot, Decision, PositionSide, RiskConfig, TradeIntent, TradeSide, Violation,
};

const HUNDRED: Decimal = Decimal::ONE_HUNDRED;

/// True when today's realized loss has reached the daily max-loss limit and
/// the circuit breaker must trip (halting all trading for the rest of the
/// UTC trading day). The comparison is `>=`: reaching the limit exactly trips.
pub fn circuit_breaker_should_trip(snapshot: &AccountSnapshot, cfg: &RiskConfig) -> bool {
    if snapshot.equity <= Decimal::ZERO {
        // With no positive equity there is nothing left to protect by
        // letting trading continue; treat as tripped.
        return snapshot.today_realized_loss > Decimal::ZERO;
    }
    snapshot.today_realized_loss >= snapshot.equity * cfg.daily_max_loss_pct / HUNDRED
}

/// Evaluate a proposed trade against every risk control.
///
/// Ordering and semantics:
/// 1. Kill switch — absolute. If engaged, the trade is rejected with
///    `KillSwitchEngaged` as the sole violation and nothing else runs.
/// 2. Circuit breaker — if already tripped, or today's realized loss has
///    reached the daily limit, all trading (including closes) is halted.
/// 3. Per-trade checks — position size, concurrent positions, correlated
///    exposure. All are evaluated and every violation is reported, so the
///    audit log records the complete picture. Trades that purely reduce an
///    existing opposite-side position are exempt from these three checks
///    (reducing risk is never blocked by sizing rules).
pub fn evaluate(intent: &TradeIntent, snapshot: &AccountSnapshot, cfg: &RiskConfig) -> Decision {
    if snapshot.kill_switch_engaged {
        return Decision::Rejected {
            violations: vec![Violation::KillSwitchEngaged],
        };
    }

    if intent.quantity < Decimal::ZERO || intent.price < Decimal::ZERO {
        return Decision::Rejected {
            violations: vec![Violation::InvalidIntent {
                quantity: intent.quantity,
                price: intent.price,
            }],
        };
    }

    let mut violations = Vec::new();

    let daily_loss_limit = if snapshot.equity > Decimal::ZERO {
        snapshot.equity * cfg.daily_max_loss_pct / HUNDRED
    } else {
        Decimal::ZERO
    };
    if snapshot.circuit_breaker_tripped || circuit_breaker_should_trip(snapshot, cfg) {
        violations.push(Violation::CircuitBreakerTripped {
            today_realized_loss: snapshot.today_realized_loss,
            daily_loss_limit,
        });
    }

    if !reduces_existing_position(intent, snapshot) {
        check_position_size(intent, snapshot, cfg, &mut violations);
        check_concurrent_positions(intent, snapshot, cfg, &mut violations);
        check_correlated_exposure(intent, snapshot, cfg, &mut violations);
    }

    if violations.is_empty() {
        Decision::Approved
    } else {
        Decision::Rejected { violations }
    }
}

/// A trade purely reduces risk when it goes against an existing open
/// position on the same symbol and its quantity does not exceed that
/// position's quantity (i.e. it cannot flip the position into new exposure).
/// Quantities, not notionals: a limit price differing from the mark price
/// must never turn a flip into a "reduce".
fn reduces_existing_position(intent: &TradeIntent, snapshot: &AccountSnapshot) -> bool {
    snapshot.open_positions.iter().any(|p| {
        p.symbol == intent.symbol
            && opposes(intent.side, p.side)
            && intent.quantity <= p.quantity
    })
}

fn opposes(trade: TradeSide, position: PositionSide) -> bool {
    matches!(
        (trade, position),
        (TradeSide::Sell, PositionSide::Long) | (TradeSide::Buy, PositionSide::Short)
    )
}

fn check_position_size(
    intent: &TradeIntent,
    snapshot: &AccountSnapshot,
    cfg: &RiskConfig,
    violations: &mut Vec<Violation>,
) {
    if snapshot.equity <= Decimal::ZERO {
        violations.push(Violation::NonPositiveEquity {
            equity: snapshot.equity,
        });
        return;
    }
    let limit_notional = snapshot.equity * cfg.max_position_pct_equity / HUNDRED;
    let trade_notional = intent.notional();
    if trade_notional > limit_notional {
        violations.push(Violation::PositionSizeExceeded {
            trade_notional,
            limit_notional,
            limit_pct: cfg.max_position_pct_equity,
        });
    }
}

fn check_concurrent_positions(
    intent: &TradeIntent,
    snapshot: &AccountSnapshot,
    cfg: &RiskConfig,
    violations: &mut Vec<Violation>,
) {
    let already_open = snapshot
        .open_positions
        .iter()
        .any(|p| p.symbol == intent.symbol);
    if already_open {
        // Adding to an existing position does not raise the position count.
        return;
    }
    let open_count = snapshot.open_positions.len() as i64;
    if open_count >= i64::from(cfg.max_concurrent_positions) {
        violations.push(Violation::MaxConcurrentPositionsExceeded {
            open_count,
            limit: cfg.max_concurrent_positions,
        });
    }
}

fn check_correlated_exposure(
    intent: &TradeIntent,
    snapshot: &AccountSnapshot,
    cfg: &RiskConfig,
    violations: &mut Vec<Violation>,
) {
    let Some(group) = snapshot.correlation_groups.get(&intent.symbol) else {
        return; // uncorrelated symbol: only the per-trade size limit applies
    };
    if snapshot.equity <= Decimal::ZERO {
        return; // NonPositiveEquity already reported by the size check
    }
    let existing: Decimal = snapshot
        .open_positions
        .iter()
        .filter(|p| snapshot.correlation_groups.get(&p.symbol) == Some(group))
        .map(|p| p.notional)
        .sum();
    let combined = existing + intent.notional();
    let limit_notional = snapshot.equity * cfg.max_correlated_exposure_pct / HUNDRED;
    if combined > limit_notional {
        violations.push(Violation::CorrelatedExposureExceeded {
            group: group.clone(),
            combined_notional: combined,
            limit_notional,
            limit_pct: cfg.max_correlated_exposure_pct,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    use std::collections::HashMap;

    fn cfg() -> RiskConfig {
        RiskConfig {
            daily_max_loss_pct: dec!(5),
            max_position_pct_equity: dec!(10),
            max_concurrent_positions: 3,
            max_correlated_exposure_pct: dec!(20),
        }
    }

    fn snapshot() -> AccountSnapshot {
        AccountSnapshot {
            equity: dec!(10000),
            today_realized_loss: Decimal::ZERO,
            kill_switch_engaged: false,
            circuit_breaker_tripped: false,
            open_positions: vec![],
            correlation_groups: HashMap::new(),
        }
    }

    fn intent(symbol: &str, side: TradeSide, qty: Decimal, price: Decimal) -> TradeIntent {
        TradeIntent {
            symbol: symbol.into(),
            side,
            quantity: qty,
            price,
        }
    }

    fn buy(symbol: &str, notional: Decimal) -> TradeIntent {
        intent(symbol, TradeSide::Buy, notional, dec!(1))
    }

    // Test positions default to price 1, so quantity == notional.
    fn long(symbol: &str, notional: Decimal) -> OpenPosition {
        OpenPosition {
            symbol: symbol.into(),
            side: PositionSide::Long,
            quantity: notional,
            notional,
        }
    }

    use crate::types::{OpenPosition, PositionSide};

    // --- kill switch ---

    #[test]
    fn kill_switch_rejects_everything_and_reports_nothing_else() {
        let mut s = snapshot();
        s.kill_switch_engaged = true;
        s.circuit_breaker_tripped = true; // would also violate
        let d = evaluate(&buy("BTC", dec!(999999)), &s, &cfg()); // would also violate size
        assert_eq!(
            d,
            Decision::Rejected {
                violations: vec![Violation::KillSwitchEngaged]
            }
        );
    }

    #[test]
    fn kill_switch_blocks_even_pure_position_closes() {
        let mut s = snapshot();
        s.kill_switch_engaged = true;
        s.open_positions = vec![long("BTC", dec!(500))];
        let d = evaluate(&intent("BTC", TradeSide::Sell, dec!(500), dec!(1)), &s, &cfg());
        assert!(!d.is_approved());
    }

    // --- circuit breaker ---

    #[test]
    fn circuit_breaker_trips_exactly_at_limit() {
        let mut s = snapshot();
        s.today_realized_loss = dec!(500); // 5% of 10_000 exactly
        assert!(circuit_breaker_should_trip(&s, &cfg()));
        let d = evaluate(&buy("BTC", dec!(100)), &s, &cfg());
        assert_eq!(
            d,
            Decision::Rejected {
                violations: vec![Violation::CircuitBreakerTripped {
                    today_realized_loss: dec!(500),
                    daily_loss_limit: dec!(500),
                }]
            }
        );
    }

    #[test]
    fn circuit_breaker_does_not_trip_just_below_limit() {
        let mut s = snapshot();
        s.today_realized_loss = dec!(499.99);
        assert!(!circuit_breaker_should_trip(&s, &cfg()));
        assert!(evaluate(&buy("BTC", dec!(100)), &s, &cfg()).is_approved());
    }

    #[test]
    fn already_tripped_flag_halts_trading_even_if_loss_recomputes_below() {
        let mut s = snapshot();
        s.circuit_breaker_tripped = true;
        s.today_realized_loss = Decimal::ZERO;
        let d = evaluate(&buy("BTC", dec!(100)), &s, &cfg());
        assert!(!d.is_approved());
    }

    #[test]
    fn circuit_breaker_halts_closes_too() {
        let mut s = snapshot();
        s.circuit_breaker_tripped = true;
        s.open_positions = vec![long("BTC", dec!(500))];
        let d = evaluate(&intent("BTC", TradeSide::Sell, dec!(500), dec!(1)), &s, &cfg());
        assert!(!d.is_approved());
    }

    // --- position size ---

    #[test]
    fn position_size_at_exact_limit_is_allowed() {
        // 10% of 10_000 = 1_000: allowed at exactly the limit, blocked above.
        assert!(evaluate(&buy("BTC", dec!(1000)), &snapshot(), &cfg()).is_approved());
    }

    #[test]
    fn position_size_above_limit_is_rejected() {
        let d = evaluate(&buy("BTC", dec!(1000.01)), &snapshot(), &cfg());
        assert_eq!(
            d,
            Decision::Rejected {
                violations: vec![Violation::PositionSizeExceeded {
                    trade_notional: dec!(1000.01),
                    limit_notional: dec!(1000),
                    limit_pct: dec!(10),
                }]
            }
        );
    }

    #[test]
    fn zero_equity_rejects_any_trade() {
        let mut s = snapshot();
        s.equity = Decimal::ZERO;
        let d = evaluate(&buy("BTC", dec!(1)), &s, &cfg());
        assert_eq!(
            d,
            Decision::Rejected {
                violations: vec![Violation::NonPositiveEquity {
                    equity: Decimal::ZERO
                }]
            }
        );
    }

    #[test]
    fn negative_equity_rejects_any_trade() {
        let mut s = snapshot();
        s.equity = dec!(-50);
        assert!(!evaluate(&buy("BTC", dec!(1)), &s, &cfg()).is_approved());
    }

    #[test]
    fn zero_quantity_trade_is_approved_when_within_limits() {
        // Zero notional violates nothing; rejecting it is not a risk rule's job.
        assert!(evaluate(&buy("BTC", dec!(0)), &snapshot(), &cfg()).is_approved());
    }

    // --- max concurrent positions ---

    #[test]
    fn opening_beyond_max_concurrent_is_rejected() {
        let mut s = snapshot();
        s.open_positions = vec![
            long("BTC", dec!(100)),
            long("ETH", dec!(100)),
            long("SOL", dec!(100)),
        ];
        let d = evaluate(&buy("AVAX", dec!(100)), &s, &cfg());
        assert_eq!(
            d,
            Decision::Rejected {
                violations: vec![Violation::MaxConcurrentPositionsExceeded {
                    open_count: 3,
                    limit: 3
                }]
            }
        );
    }

    #[test]
    fn adding_to_existing_position_does_not_count_as_new() {
        let mut s = snapshot();
        s.open_positions = vec![
            long("BTC", dec!(100)),
            long("ETH", dec!(100)),
            long("SOL", dec!(100)),
        ];
        assert!(evaluate(&buy("BTC", dec!(100)), &s, &cfg()).is_approved());
    }

    #[test]
    fn opening_at_one_below_limit_is_allowed() {
        let mut s = snapshot();
        s.open_positions = vec![long("BTC", dec!(100)), long("ETH", dec!(100))];
        assert!(evaluate(&buy("SOL", dec!(100)), &s, &cfg()).is_approved());
    }

    // --- correlated exposure ---

    fn correlated_snapshot() -> AccountSnapshot {
        let mut s = snapshot();
        s.correlation_groups = HashMap::from([
            ("BTC".to_string(), "majors".to_string()),
            ("ETH".to_string(), "majors".to_string()),
            ("XYZ".to_string(), "alts".to_string()),
        ]);
        s.open_positions = vec![long("BTC", dec!(1000))];
        s
    }

    #[test]
    fn correlated_exposure_at_exact_limit_is_allowed() {
        // limit: 20% of 10_000 = 2_000. Existing 1_000 + 1_000 = exactly 2_000.
        let s = correlated_snapshot();
        assert!(evaluate(&buy("ETH", dec!(1000)), &s, &cfg()).is_approved());
    }

    #[test]
    fn correlated_exposure_above_limit_is_rejected() {
        let s = correlated_snapshot();
        let d = evaluate(&buy("ETH", dec!(1000.01)), &s, &cfg());
        match d {
            Decision::Rejected { violations } => {
                assert!(violations.iter().any(|v| matches!(
                    v,
                    Violation::CorrelatedExposureExceeded { group, .. } if group == "majors"
                )));
            }
            Decision::Approved => panic!("expected rejection"),
        }
    }

    #[test]
    fn uncorrelated_symbol_ignores_group_exposure() {
        let s = correlated_snapshot();
        // DOGE has no group; only the 10% per-trade limit applies.
        assert!(evaluate(&buy("DOGE", dec!(1000)), &s, &cfg()).is_approved());
    }

    #[test]
    fn different_group_not_affected_by_majors_exposure() {
        let s = correlated_snapshot();
        assert!(evaluate(&buy("XYZ", dec!(900)), &s, &cfg()).is_approved());
    }

    // --- multiple violations reported together ---

    #[test]
    fn all_violations_reported_not_just_first() {
        let mut s = correlated_snapshot();
        s.open_positions = vec![
            long("BTC", dec!(1000)),
            long("SOL", dec!(100)),
            long("DOGE", dec!(100)),
        ];
        s.today_realized_loss = dec!(600); // breaker: over 5%
        // ETH buy: too big (>10%), 4th position, and blows the majors cap.
        let d = evaluate(&buy("ETH", dec!(1500)), &s, &cfg());
        match d {
            Decision::Rejected { violations } => {
                assert_eq!(violations.len(), 4, "violations: {violations:?}");
                assert!(violations
                    .iter()
                    .any(|v| matches!(v, Violation::CircuitBreakerTripped { .. })));
                assert!(violations
                    .iter()
                    .any(|v| matches!(v, Violation::PositionSizeExceeded { .. })));
                assert!(violations
                    .iter()
                    .any(|v| matches!(v, Violation::MaxConcurrentPositionsExceeded { .. })));
                assert!(violations
                    .iter()
                    .any(|v| matches!(v, Violation::CorrelatedExposureExceeded { .. })));
            }
            Decision::Approved => panic!("expected rejection"),
        }
    }

    // --- risk-reducing trades ---

    #[test]
    fn pure_close_is_exempt_from_sizing_rules() {
        let mut s = snapshot();
        // Position bigger than the 10% per-trade cap: closing it must be allowed.
        s.open_positions = vec![long("BTC", dec!(5000))];
        let d = evaluate(&intent("BTC", TradeSide::Sell, dec!(5000), dec!(1)), &s, &cfg());
        assert!(d.is_approved());
    }

    #[test]
    fn partial_reduce_is_exempt_from_sizing_rules() {
        let mut s = snapshot();
        s.open_positions = vec![long("BTC", dec!(5000))];
        // Selling 2_000 of a 5_000 long: still over the 10% per-trade cap by
        // notional, but purely risk-reducing, so approved.
        let d = evaluate(&intent("BTC", TradeSide::Sell, dec!(2000), dec!(1)), &s, &cfg());
        assert!(d.is_approved());
    }

    #[test]
    fn flip_detection_uses_quantity_not_notional() {
        let mut s = snapshot();
        // Long 100 BTC marked at 50: quantity 100, notional 5_000.
        s.open_positions = vec![OpenPosition {
            symbol: "BTC".into(),
            side: PositionSide::Long,
            quantity: dec!(100),
            notional: dec!(5000),
        }];
        // SELL 150 @ limit 30: intent notional 4_500 <= 5_000, but quantity
        // 150 > 100 flips into a 50-BTC short — must NOT be exempt.
        let d = evaluate(&intent("BTC", TradeSide::Sell, dec!(150), dec!(30)), &s, &cfg());
        assert!(!d.is_approved(), "price-divergent flip must not pass as a reduce");
        // Full close at a price above the mark is still a reduce by quantity.
        let d = evaluate(&intent("BTC", TradeSide::Sell, dec!(100), dec!(80)), &s, &cfg());
        assert!(d.is_approved(), "full close must be exempt regardless of price");
    }

    // --- intent boundary ---

    #[test]
    fn negative_quantity_or_price_is_rejected() {
        for (q, p) in [(dec!(-1), dec!(1)), (dec!(1), dec!(-1))] {
            let d = evaluate(&intent("BTC", TradeSide::Buy, q, p), &snapshot(), &cfg());
            assert_eq!(
                d,
                Decision::Rejected {
                    violations: vec![Violation::InvalidIntent { quantity: q, price: p }]
                }
            );
        }
    }

    #[test]
    fn oversized_reversal_is_not_exempt() {
        let mut s = snapshot();
        s.open_positions = vec![long("BTC", dec!(500))];
        // Selling 6_000 against a 500 long would flip into a 5_500 short:
        // treated as new exposure, so the 10% cap applies.
        let d = evaluate(&intent("BTC", TradeSide::Sell, dec!(6000), dec!(1)), &s, &cfg());
        assert!(!d.is_approved());
    }
}
