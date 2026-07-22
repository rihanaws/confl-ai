-- Integrity fixes from the schema verification pass.

-- 1. FK checks bypass RLS, so correlation_group_assets.group_id could point
--    at another tenant's group. Composite FK pins the referenced group to
--    the same account.
ALTER TABLE correlation_groups
    ADD CONSTRAINT correlation_groups_id_account_unique UNIQUE (id, account_id);
ALTER TABLE correlation_group_assets
    DROP CONSTRAINT correlation_group_assets_group_id_fkey,
    ADD CONSTRAINT correlation_group_assets_group_fk
        FOREIGN KEY (group_id, account_id)
        REFERENCES correlation_groups (id, account_id) ON DELETE CASCADE;

-- 2. Audit rows are immutable, so a cascading account delete would be
--    blocked by the append-only trigger anyway. Make that explicit:
--    accounts with audit history are intentionally undeletable.
ALTER TABLE risk_events
    DROP CONSTRAINT risk_events_account_id_fkey,
    ADD CONSTRAINT risk_events_account_fk
        FOREIGN KEY (account_id) REFERENCES accounts (id) ON DELETE RESTRICT;
