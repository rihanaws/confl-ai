-- Account equity, the base for every %-of-equity risk limit. Phase 1 has no
-- exchange sync, so equity is set explicitly (account creation / equity
-- endpoint); Phase 2's exchange adapter becomes the writer later.
ALTER TABLE accounts ADD COLUMN equity numeric(30,10) NOT NULL DEFAULT 0;
