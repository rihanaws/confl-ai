use crate::error::{ExchangeError, Result};
use crate::types::OrderStatus;

/// Validated order lifecycle transitions. Terminal states
/// (`Filled`/`Cancelled`/`Rejected`) accept no further transitions.
pub struct OrderStateMachine;

impl OrderStateMachine {
    pub fn transition(from: OrderStatus, to: OrderStatus) -> Result<OrderStatus> {
        let legal = matches!(
            (from, to),
            (OrderStatus::Pending, OrderStatus::Submitted)
                | (OrderStatus::Pending, OrderStatus::Rejected)
                | (OrderStatus::Pending, OrderStatus::CancelRequested)
                | (OrderStatus::Submitted, OrderStatus::PartiallyFilled)
                | (OrderStatus::Submitted, OrderStatus::Filled)
                | (OrderStatus::Submitted, OrderStatus::Rejected)
                | (OrderStatus::Submitted, OrderStatus::CancelRequested)
                | (OrderStatus::PartiallyFilled, OrderStatus::PartiallyFilled)
                | (OrderStatus::PartiallyFilled, OrderStatus::Filled)
                | (OrderStatus::PartiallyFilled, OrderStatus::CancelRequested)
                | (OrderStatus::CancelRequested, OrderStatus::Cancelled)
                | (OrderStatus::CancelRequested, OrderStatus::Filled)
                | (OrderStatus::CancelRequested, OrderStatus::PartiallyFilled)
        );
        if legal {
            Ok(to)
        } else {
            Err(ExchangeError::InvalidTransition {
                from: format!("{from:?}"),
                to: format!("{to:?}"),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use OrderStatus::*;

    #[test]
    fn legal_happy_path() {
        assert!(OrderStateMachine::transition(Pending, Submitted).is_ok());
        assert!(OrderStateMachine::transition(Submitted, PartiallyFilled).is_ok());
        assert!(OrderStateMachine::transition(PartiallyFilled, Filled).is_ok());
    }

    #[test]
    fn terminal_states_reject_everything() {
        for terminal in [Filled, Cancelled, Rejected] {
            for target in [Pending, Submitted, PartiallyFilled, Filled, CancelRequested, Cancelled, Rejected] {
                if terminal == target {
                    continue;
                }
                assert!(
                    OrderStateMachine::transition(terminal, target).is_err(),
                    "{terminal:?} -> {target:?} must be illegal"
                );
            }
        }
    }

    #[test]
    fn cancel_requested_can_still_resolve_to_fill_or_partial() {
        assert!(OrderStateMachine::transition(CancelRequested, Filled).is_ok());
        assert!(OrderStateMachine::transition(CancelRequested, PartiallyFilled).is_ok());
        assert!(OrderStateMachine::transition(CancelRequested, Cancelled).is_ok());
    }

    #[test]
    fn pending_cannot_jump_to_filled() {
        assert!(OrderStateMachine::transition(Pending, Filled).is_err());
    }

    #[test]
    fn cancelled_is_immutable() {
        assert!(OrderStateMachine::transition(Cancelled, Pending).is_err());
        assert!(OrderStateMachine::transition(Cancelled, Filled).is_err());
    }
}
