-- Extend the append-only audit log's event_type whitelist for Phase 2
-- (orders, fills, reconciliation, mode switches, credential rotation).
ALTER TABLE risk_events DROP CONSTRAINT risk_events_event_type_check;
ALTER TABLE risk_events ADD CONSTRAINT risk_events_event_type_check CHECK (event_type IN (
    'trade_evaluated',
    'circuit_breaker_tripped',
    'kill_switch_engaged',
    'kill_switch_released',
    'risk_config_changed',
    'equity_changed',
    'account_created',
    'order_placed',
    'order_rejected',
    'order_filled',
    'order_partially_filled',
    'order_cancelled',
    'exchange_reconciliation_required',
    'exchange_mode_switched',
    'exchange_credentials_rotated'
));
