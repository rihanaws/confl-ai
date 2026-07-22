-- Tracks last-seen market data per symbol so a stale/gapped feed can be
-- detected and market orders rejected rather than sized on old data.
-- Not tenant-scoped: symbols/feeds are shared infrastructure, not per-account.

CREATE TABLE market_data_state (
    symbol              text PRIMARY KEY,
    last_price          numeric(30,10),
    last_bid            numeric(30,10),
    last_ask            numeric(30,10),
    last_update_at      timestamptz,
    is_stale            boolean NOT NULL DEFAULT true,
    updated_at          timestamptz NOT NULL DEFAULT now()
);

GRANT SELECT, INSERT, UPDATE ON market_data_state TO confluence_app;
