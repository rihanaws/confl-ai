-- Outbox for exchange-bound commands (submit_order, cancel_order,
-- reconcile_order). Worker claims rows with SKIP LOCKED and dispatches to
-- the adapter resolved by exchange_mode.

CREATE TABLE exchange_commands (
    id                uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    account_id        uuid NOT NULL REFERENCES accounts(id) ON DELETE RESTRICT,
    command_type      text NOT NULL CHECK (command_type IN (
                          'submit_order', 'cancel_order', 'reconcile_order'
                      )),
    exchange_mode     text NOT NULL CHECK (exchange_mode IN ('paper', 'live')),
    status            text NOT NULL DEFAULT 'pending' CHECK (status IN (
                          'pending', 'leased', 'delivered',
                          'reconciliation_required', 'permanently_rejected'
                      )),
    idempotency_key   text NOT NULL,
    payload           jsonb NOT NULL DEFAULT '{}'::jsonb,
    attempts          integer NOT NULL DEFAULT 0,
    leased_until      timestamptz,
    created_at        timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now()
);

-- Idempotency key uniqueness is the ON CONFLICT DO NOTHING target for
-- reconcile_order inserts, and prevents duplicate submit/cancel dispatch.
CREATE UNIQUE INDEX exchange_commands_account_idempotency
    ON exchange_commands (account_id, idempotency_key);

-- Claim query: pending rows, oldest first, per account isolation.
CREATE INDEX exchange_commands_claimable
    ON exchange_commands (status, created_at) WHERE status = 'pending';

ALTER TABLE exchange_commands ENABLE ROW LEVEL SECURITY;
ALTER TABLE exchange_commands FORCE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON exchange_commands
    USING (account_id = current_setting('app.account_id', true)::uuid)
    WITH CHECK (account_id = current_setting('app.account_id', true)::uuid);

GRANT SELECT, INSERT, UPDATE ON exchange_commands TO confluence_app;
