-- Row-level tenant isolation. The application role (confluence_app, created
-- outside migrations by scripts/bootstrap_app_role.sql because CREATE ROLE
-- needs a password from the environment) is subject to these policies; the
-- migration/owner role is not the role the app connects as.
--
-- The app sets the tenant per transaction:
--     SET LOCAL app.account_id = '<uuid>';
-- A missing setting yields NULL and every policy evaluates false-ish:
-- no rows visible, no rows insertable.

ALTER TABLE accounts                 ENABLE ROW LEVEL SECURITY;
ALTER TABLE risk_configs             ENABLE ROW LEVEL SECURITY;
ALTER TABLE correlation_groups       ENABLE ROW LEVEL SECURITY;
ALTER TABLE correlation_group_assets ENABLE ROW LEVEL SECURITY;
ALTER TABLE positions                ENABLE ROW LEVEL SECURITY;
ALTER TABLE daily_pnl                ENABLE ROW LEVEL SECURITY;
ALTER TABLE risk_events              ENABLE ROW LEVEL SECURITY;

-- Also bind table owners, so a future misconfigured connection as the owner
-- doesn't silently bypass isolation.
ALTER TABLE accounts                 FORCE ROW LEVEL SECURITY;
ALTER TABLE risk_configs             FORCE ROW LEVEL SECURITY;
ALTER TABLE correlation_groups       FORCE ROW LEVEL SECURITY;
ALTER TABLE correlation_group_assets FORCE ROW LEVEL SECURITY;
ALTER TABLE positions                FORCE ROW LEVEL SECURITY;
ALTER TABLE daily_pnl                FORCE ROW LEVEL SECURITY;
ALTER TABLE risk_events              FORCE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON accounts
    USING (id = current_setting('app.account_id', true)::uuid)
    WITH CHECK (id = current_setting('app.account_id', true)::uuid);

CREATE POLICY tenant_isolation ON risk_configs
    USING (account_id = current_setting('app.account_id', true)::uuid)
    WITH CHECK (account_id = current_setting('app.account_id', true)::uuid);

CREATE POLICY tenant_isolation ON correlation_groups
    USING (account_id = current_setting('app.account_id', true)::uuid)
    WITH CHECK (account_id = current_setting('app.account_id', true)::uuid);

CREATE POLICY tenant_isolation ON correlation_group_assets
    USING (account_id = current_setting('app.account_id', true)::uuid)
    WITH CHECK (account_id = current_setting('app.account_id', true)::uuid);

CREATE POLICY tenant_isolation ON positions
    USING (account_id = current_setting('app.account_id', true)::uuid)
    WITH CHECK (account_id = current_setting('app.account_id', true)::uuid);

CREATE POLICY tenant_isolation ON daily_pnl
    USING (account_id = current_setting('app.account_id', true)::uuid)
    WITH CHECK (account_id = current_setting('app.account_id', true)::uuid);

CREATE POLICY tenant_isolation ON risk_events
    USING (account_id = current_setting('app.account_id', true)::uuid)
    WITH CHECK (account_id = current_setting('app.account_id', true)::uuid);

-- Application role grants: least privilege. No UPDATE/DELETE on risk_events
-- (append-only; the trigger in 0001 backstops even roles that do have it).
GRANT USAGE ON SCHEMA public TO confluence_app;
GRANT SELECT, INSERT, UPDATE ON accounts, risk_configs, correlation_groups,
    correlation_group_assets, positions, daily_pnl TO confluence_app;
GRANT DELETE ON correlation_groups, correlation_group_assets TO confluence_app;
GRANT SELECT, INSERT ON risk_events TO confluence_app;
