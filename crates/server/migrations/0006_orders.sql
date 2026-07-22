-- Orders: one row per order intent that passed risk evaluation and was
-- accepted for submission to an exchange (paper or live).

CREATE TABLE orders (
    id                uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    account_id        uuid NOT NULL REFERENCES accounts(id) ON DELETE RESTRICT,
    client_order_id   text NOT NULL,
    symbol            text NOT NULL,
    side              text NOT NULL CHECK (side IN ('buy', 'sell')),
    order_type        text NOT NULL CHECK (order_type IN ('limit', 'market')),
    quantity          numeric(30,10) NOT NULL CHECK (quantity > 0),
    price             numeric(30,10) CHECK (price IS NULL OR price > 0),
    status            text NOT NULL DEFAULT 'pending' CHECK (status IN (
                          'pending', 'submitted', 'partially_filled', 'filled',
                          'cancel_requested', 'cancelled', 'rejected'
                      )),
    exchange_mode     text NOT NULL CHECK (exchange_mode IN ('paper', 'live')),
    filled_quantity   numeric(30,10) NOT NULL DEFAULT 0 CHECK (filled_quantity >= 0),
    created_at        timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now(),
    CHECK (filled_quantity <= quantity)
);

-- client_order_id must be unique per account so idempotency keys derived
-- from it (submit/cancel) never collide across orders.
CREATE UNIQUE INDEX orders_account_client_order_id ON orders (account_id, client_order_id);
CREATE INDEX orders_by_account_status ON orders (account_id, status);

ALTER TABLE orders ENABLE ROW LEVEL SECURITY;
ALTER TABLE orders FORCE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON orders
    USING (account_id = current_setting('app.account_id', true)::uuid)
    WITH CHECK (account_id = current_setting('app.account_id', true)::uuid);

GRANT SELECT, INSERT, UPDATE ON orders TO confluence_app;
