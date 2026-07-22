-- Per-account exchange configuration: which venue, which mode (paper/live,
-- mutually exclusive), and encrypted API credentials. Credentials are
-- encrypted application-side (AES-256-GCM, crates/exchange::config) before
-- this row is written; this migration never handles plaintext.

CREATE TABLE exchange_configs (
    account_id           uuid PRIMARY KEY REFERENCES accounts(id) ON DELETE RESTRICT,
    exchange             text NOT NULL DEFAULT 'binance' CHECK (exchange IN ('binance')),
    mode                 text NOT NULL DEFAULT 'paper' CHECK (mode IN ('paper', 'live')),
    testnet              boolean NOT NULL DEFAULT true,
    api_key_ciphertext   bytea,
    api_secret_ciphertext bytea,
    version              integer NOT NULL DEFAULT 1,
    created_at           timestamptz NOT NULL DEFAULT now(),
    updated_at           timestamptz NOT NULL DEFAULT now()
);

ALTER TABLE exchange_configs ENABLE ROW LEVEL SECURITY;
ALTER TABLE exchange_configs FORCE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON exchange_configs
    USING (account_id = current_setting('app.account_id', true)::uuid)
    WITH CHECK (account_id = current_setting('app.account_id', true)::uuid);

-- Never SELECT-able ciphertext columns in bulk API responses by accident is
-- an application concern; the grant itself must still allow the row to be
-- read at all (decrypt happens just-in-time in the route handler).
GRANT SELECT, INSERT, UPDATE ON exchange_configs TO confluence_app;

-- Mode-switch guard, layer 2 (DB trigger backstop; layer 1 is the
-- application-level check in put_exchange_config). SECURITY INVOKER so the
-- trigger runs as the calling role and its subqueries stay subject to RLS —
-- a SECURITY DEFINER trigger would silently see across tenants.
CREATE FUNCTION forbid_mode_switch_with_open_activity() RETURNS trigger
LANGUAGE plpgsql SECURITY INVOKER AS $$
BEGIN
    IF NEW.mode = OLD.mode THEN
        RETURN NEW;
    END IF;

    IF EXISTS (
        SELECT 1 FROM orders
        WHERE account_id = NEW.account_id
          AND status NOT IN ('filled', 'cancelled', 'rejected')
    ) THEN
        RAISE EXCEPTION 'cannot switch exchange mode: account % has open orders', NEW.account_id;
    END IF;

    IF EXISTS (
        SELECT 1 FROM positions
        WHERE account_id = NEW.account_id
          AND status = 'open'
          AND quantity > 0
    ) THEN
        RAISE EXCEPTION 'cannot switch exchange mode: account % has open positions', NEW.account_id;
    END IF;

    RETURN NEW;
END;
$$;

CREATE TRIGGER exchange_configs_mode_switch_guard
    BEFORE UPDATE ON exchange_configs
    FOR EACH ROW EXECUTE FUNCTION forbid_mode_switch_with_open_activity();
