-- Phase 1 schema: accounts, risk configs, correlation groups, positions,
-- daily PnL (circuit breaker state), and the append-only risk_events audit log.
--
-- Conventions:
--  * All UUIDs are generated application-side (UUIDv7); gen_random_uuid() is
--    only a fallback default.
--  * Money/quantity columns are NUMERIC(30,10) to round-trip rust_decimal
--    exactly. Percent limits are NUMERIC(8,4), expressed as percentages
--    (2.0 = 2%), not fractions.
--  * Enum-ish columns are TEXT + CHECK, not Postgres enums, so future values
--    are a plain migration instead of an enum surgery.

CREATE TABLE accounts (
    id                      uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    status                  text NOT NULL DEFAULT 'active'
                            CHECK (status IN ('active', 'killed', 'suspended')),
    kill_switch_engaged_at  timestamptz,
    created_at              timestamptz NOT NULL DEFAULT now(),
    updated_at              timestamptz NOT NULL DEFAULT now(),
    -- status and timestamp must agree; the kill switch is a single state.
    CHECK ((status = 'killed') = (kill_switch_engaged_at IS NOT NULL))
);

-- PLACEHOLDER DEFAULTS: the numeric limit defaults below are engineering
-- placeholders, NOT production risk values. Production values are a
-- financial decision pending explicit sign-off.
CREATE TABLE risk_configs (
    account_id                   uuid PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    daily_max_loss_pct           numeric(8,4) NOT NULL DEFAULT 2.0
                                 CHECK (daily_max_loss_pct > 0 AND daily_max_loss_pct <= 100),
    max_position_pct_equity      numeric(8,4) NOT NULL DEFAULT 5.0
                                 CHECK (max_position_pct_equity > 0 AND max_position_pct_equity <= 100),
    max_concurrent_positions     integer NOT NULL DEFAULT 5
                                 CHECK (max_concurrent_positions > 0),
    max_correlated_exposure_pct  numeric(8,4) NOT NULL DEFAULT 10.0
                                 CHECK (max_correlated_exposure_pct > 0 AND max_correlated_exposure_pct <= 100),
    version                      integer NOT NULL DEFAULT 1,
    updated_at                   timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE correlation_groups (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    account_id  uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name        text NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    UNIQUE (account_id, name)
);

CREATE TABLE correlation_group_assets (
    group_id    uuid NOT NULL REFERENCES correlation_groups(id) ON DELETE CASCADE,
    account_id  uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    symbol      text NOT NULL,
    PRIMARY KEY (group_id, symbol),
    -- One group per symbol per account: the engine maps symbol -> group.
    UNIQUE (account_id, symbol)
);

CREATE TABLE positions (
    id               uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    account_id       uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    symbol           text NOT NULL,
    side             text NOT NULL CHECK (side IN ('long', 'short')),
    quantity         numeric(30,10) NOT NULL CHECK (quantity > 0),
    avg_entry_price  numeric(30,10) NOT NULL CHECK (avg_entry_price >= 0),
    status           text NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'closed')),
    realized_pnl     numeric(30,10) NOT NULL DEFAULT 0,
    opened_at        timestamptz NOT NULL DEFAULT now(),
    closed_at        timestamptz,
    CHECK ((status = 'closed') = (closed_at IS NOT NULL))
);

CREATE UNIQUE INDEX positions_one_open_per_symbol
    ON positions (account_id, symbol) WHERE status = 'open';
CREATE INDEX positions_by_account_status ON positions (account_id, status);

CREATE TABLE daily_pnl (
    account_id                  uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    trading_date                date NOT NULL,  -- UTC trading day
    realized_loss               numeric(30,10) NOT NULL DEFAULT 0 CHECK (realized_loss >= 0),
    circuit_breaker_tripped_at  timestamptz,
    PRIMARY KEY (account_id, trading_date)
);

CREATE TABLE risk_events (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    account_id  uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    event_type  text NOT NULL CHECK (event_type IN (
                    'trade_evaluated',
                    'circuit_breaker_tripped',
                    'kill_switch_engaged',
                    'kill_switch_released',
                    'risk_config_changed',
                    'account_created'
                )),
    payload     jsonb NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX risk_events_by_account_time ON risk_events (account_id, created_at DESC);

-- Audit log is append-only at the database level, independent of grants.
CREATE FUNCTION forbid_risk_event_mutation() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'risk_events is append-only: % not allowed', TG_OP;
END;
$$;

CREATE TRIGGER risk_events_append_only
    BEFORE UPDATE OR DELETE ON risk_events
    FOR EACH ROW EXECUTE FUNCTION forbid_risk_event_mutation();
