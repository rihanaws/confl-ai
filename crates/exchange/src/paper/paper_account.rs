use std::collections::HashMap;

use rust_decimal::Decimal;

use crate::error::{ExchangeError, Result};

/// Free (unreserved) balances per asset for one paper account. Equity is
/// tracked separately by the server (Phase 1 `accounts.equity`) — this
/// struct only tracks per-asset spendable balance for reservation checks.
#[derive(Debug, Clone, Default)]
pub struct PaperAccount {
    free: HashMap<String, Decimal>,
}

impl PaperAccount {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_balance(mut self, asset: &str, amount: Decimal) -> Self {
        self.free.insert(asset.to_string(), amount);
        self
    }

    pub fn free_balance(&self, asset: &str) -> Decimal {
        *self.free.get(asset).unwrap_or(&Decimal::ZERO)
    }

    pub fn reserve(&mut self, asset: &str, amount: Decimal) -> Result<()> {
        let available = self.free_balance(asset);
        if amount > available {
            return Err(ExchangeError::InsufficientBalance {
                asset: asset.to_string(),
                need: amount,
                available,
            });
        }
        self.free.insert(asset.to_string(), available - amount);
        Ok(())
    }

    pub fn release(&mut self, asset: &str, amount: Decimal) {
        let bal = self.free_balance(asset);
        self.free.insert(asset.to_string(), bal + amount);
    }

    pub fn credit(&mut self, asset: &str, amount: Decimal) {
        let bal = self.free_balance(asset);
        self.free.insert(asset.to_string(), bal + amount);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn reserve_reduces_free_balance() {
        let mut acct = PaperAccount::new().with_balance("USDT", dec!(1000));
        acct.reserve("USDT", dec!(300)).unwrap();
        assert_eq!(acct.free_balance("USDT"), dec!(700));
    }

    #[test]
    fn reserve_beyond_available_fails() {
        let mut acct = PaperAccount::new().with_balance("USDT", dec!(100));
        let err = acct.reserve("USDT", dec!(200)).unwrap_err();
        assert!(matches!(err, ExchangeError::InsufficientBalance { .. }));
    }

    #[test]
    fn release_restores_balance() {
        let mut acct = PaperAccount::new().with_balance("USDT", dec!(1000));
        acct.reserve("USDT", dec!(300)).unwrap();
        acct.release("USDT", dec!(300));
        assert_eq!(acct.free_balance("USDT"), dec!(1000));
    }
}
