-- Equity changes are the basis of every %-of-equity limit, so they belong in
-- the audit log, and equity itself must be non-negative until a later phase
-- deliberately introduces margin accounting.
ALTER TABLE accounts ADD CONSTRAINT accounts_equity_nonnegative CHECK (equity >= 0);

ALTER TABLE risk_events DROP CONSTRAINT risk_events_event_type_check;
ALTER TABLE risk_events ADD CONSTRAINT risk_events_event_type_check CHECK (event_type IN (
    'trade_evaluated',
    'circuit_breaker_tripped',
    'kill_switch_engaged',
    'kill_switch_released',
    'risk_config_changed',
    'equity_changed',
    'account_created'
));
