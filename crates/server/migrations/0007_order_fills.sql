-- Individual fills against an order. `exchange_trade_id` dedupes replayed
-- fill notifications (unique per account, since ids are per-venue).

CREATE TABLE order_fills (
    id                  uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    account_id          uuid NOT NULL REFERENCES accounts(id) ON DELETE RESTRICT,
    order_id            uuid NOT NULL REFERENCES orders(id) ON DELETE RESTRICT,
    exchange_trade_id   text NOT NULL,
    quantity            numeric(30,10) NOT NULL CHECK (quantity > 0),
    price               numeric(30,10) NOT NULL CHECK (price > 0),
    fee                 numeric(30,10) NOT NULL DEFAULT 0 CHECK (fee >= 0),
    fee_asset           text NOT NULL DEFAULT '',
    created_at          timestamptz NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX order_fills_account_trade_id ON order_fills (account_id, exchange_trade_id);
CREATE INDEX order_fills_by_order ON order_fills (order_id);

ALTER TABLE order_fills ENABLE ROW LEVEL SECURITY;
ALTER TABLE order_fills FORCE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON order_fills
    USING (account_id = current_setting('app.account_id', true)::uuid)
    WITH CHECK (account_id = current_setting('app.account_id', true)::uuid);

-- Fills are append-only, like risk_events: a recorded fill must never be
-- edited or removed once written.
GRANT SELECT, INSERT ON order_fills TO confluence_app;
