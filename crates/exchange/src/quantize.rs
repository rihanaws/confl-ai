use rust_decimal::Decimal;

/// True when `value` sits exactly on a `step` boundary. Uses division +
/// `.fract()`, never `%`, since `Decimal`'s remainder semantics are not what
/// step-alignment needs here.
pub fn is_aligned(value: Decimal, step: Decimal) -> bool {
    if step.is_zero() {
        return false;
    }
    (value / step).fract().is_zero()
}

/// Nearest valid quantity at or below `value`. For error messages only
/// ("nearest valid quantity is X") — never used to silently coerce a
/// submitted order's quantity. A misaligned order is rejected, not
/// auto-corrected.
pub fn qdown(value: Decimal, step: Decimal) -> Decimal {
    if step.is_zero() {
        return Decimal::ZERO;
    }
    (value / step).floor() * step
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn aligned_values_detected() {
        assert!(is_aligned(dec!(1.5), dec!(0.1)));
        assert!(!is_aligned(dec!(1.55), dec!(0.1)));
    }

    #[test]
    fn qdown_rounds_toward_nearest_valid_step() {
        assert_eq!(qdown(dec!(1.55), dec!(0.1)), dec!(1.5));
        assert_eq!(qdown(dec!(1.5), dec!(0.1)), dec!(1.5));
    }

    #[test]
    fn zero_step_never_aligned() {
        assert!(!is_aligned(dec!(1), dec!(0)));
    }
}
