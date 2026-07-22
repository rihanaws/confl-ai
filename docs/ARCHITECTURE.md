# Architecture

## Crates

```
crates/core       confluence-core    — pure risk engine, zero I/O
crates/server     confluence-server  — axum API, sqlx/Postgres, outbox worker
crates/exchange   confluence-exchange — Binance Spot adapter + paper trading engine, zero sqlx
```

### `confluence-core`

`evaluate()` is the only entry point. Takes a `TradeIntent`, an `AccountSnapshot`, and a
`RiskConfig`; returns `Decision::Approved` or `Decision::Rejected { violations }`. No I/O,
no async, no database types — fully unit-testable in isolation. Five controls: kill switch,
daily max-loss circuit breaker, per-trade size % of equity, max concurrent positions,
correlated exposure.

### `confluence-exchange`

Binance Spot adapter (REST + WS client) and a local paper-trading engine, unified behind one
`ExchangeAdapter` trait. **Zero `sqlx` in this crate's dependency tree, by design** — every
file compiles and is testable without a database connection. The server hydrates plain-data
snapshots (`OrderSnapshot`, `ReconcileSnapshot`, `FillSnapshot`, ...) from Postgres, passes
them across the trait boundary, and persists whatever `ReconcileResult` comes back. Neither
`PaperAdapter` nor `BinanceAdapter` ever touches sqlx.

```
adapter.rs        ExchangeAdapter trait + plain-data snapshot/result types
types.rs          SymbolMeta, filters (LOT_SIZE, NOTIONAL, PERCENT_PRICE[_BY_SIDE]), OrderStatus, Fill
order_state.rs    OrderStateMachine — validated status transitions, terminal states immutable
config.rs         AES-256-GCM credential encryption (EncryptionKey), AAD-bound to account id
quantize.rs       is_aligned()/qdown() — Decimal step/tick alignment, never `%`
outbox.rs         CommandDispatcher trait (dispatch contract only; the server owns the queue)

paper/
  matching_engine.rs  price-time-priority fill logic, Decimal only
  paper_account.rs    per-asset free-balance reservation ledger
  market_data.rs      MarketDataProvider — book-top cache + staleness detection
  mod.rs              PaperAdapter — reconciles from a passed-in snapshot only

binance/
  rest.rs         BinanceRestClient — HMAC-SHA256 signing, origin-only base URL + /api/v3/* paths
  ws.rs           WS URL builders + public-stream event parsing (built, not yet the live feed — see PHASE2_STATUS.md)
  reconcile.rs    BinanceAdapter — GET /api/v3/order?origClientOrderId= based reconciliation
```

### `confluence-server`

Axum HTTP API + Postgres persistence + a background outbox worker.

```
routes.rs          route table, AppState (pool, symbol metadata store, shared market data)
routes_orders.rs   order placement: filter validation -> risk evaluation -> order + command insert
routes_exchange.rs exchange config CRUD: mode-switch guard (app layer), credential encryption
db.rs              all sqlx queries — tenant_tx, orders, fills, positions, exchange_commands
outbox_worker.rs   claim -> hydrate -> adapter call -> persist (fills/positions/status/events)
symbol_metadata.rs SymbolMetadataStore — fail-closed cache of exchange filter metadata
validators.rs      validate_order / validate_limit / validate_market (pre-transaction checks)
main.rs            wires pools, refresh loops, supervisor, outbox worker, axum server
```

## Request flow: placing an order

1. `POST /accounts/{id}/orders` — filter validation runs **before any transaction opens**:
   symbol status/type, LOT_SIZE/MARKET_LOT_SIZE, PRICE_FILTER, NOTIONAL, PERCENT_PRICE(_BY_SIDE).
   A market order additionally requires a fresh conservative price estimate; stale or missing
   market data rejects the order (422) before any DB round trip.
2. One transaction: lock account, load risk config + snapshot, run `confluence_core::evaluate()`.
3. Rejected → `order_rejected` risk event, transaction commits, 422 returned. No order row.
4. Approved → insert `orders` row (status `pending`), insert an `exchange_commands` row
   (`submit_order`, `idempotency_key = "{order_id}:submit"`), insert `order_placed` risk event.
   Commit. 201 returned.

## Outbox worker

Runs in a background tokio task on a fixed interval. Each pass:

1. Claims pending/expired-lease `exchange_commands` rows across **all tenants** in one
   `SKIP LOCKED` batch (see [DATABASE.md](DATABASE.md) for why this needs a separate pool).
2. Resolves the adapter for the command's `exchange_mode` (paper/live). No adapter configured
   → fail closed: `reconciliation_required` + a manual-review `risk_events` row. Never `.unwrap()`.
3. Dispatches:
   - `submit_order` → adapter `submit()`, then `pending → submitted`, then auto-enqueues the
     first `reconcile_order` for that order.
   - `reconcile_order` → hydrates a `ReconcileSnapshot` from `orders`/`order_fills`, calls
     `adapter.reconcile_order()`, persists the result (new fills, position deltas via
     `apply_position_delta`, order status via a validated `OrderStateMachine::transition`,
     risk events) in one transaction.
   - `cancel_order` → adapter `cancel()`.
4. Deterministic exchange errors (bad symbol, filter violation, insufficient balance) →
   `permanently_rejected` + order flips to `rejected`. Everything else → retried with backoff.

## Symbol metadata & market data

`SymbolMetadataStore` and `MarketDataProvider` are both fail-closed caches: a symbol/price
absent or older than the freshness window returns `None`, and callers treat that as "reject,"
never "use whatever we last had." Current refresh mechanism is periodic REST polling against
Binance Spot Testnet (`GET /api/v3/exchangeInfo`, `GET /api/v3/ticker/bookTicker`) — see
[PHASE2_STATUS.md](PHASE2_STATUS.md) for why, and for the WS path that already exists as a
drop-in replacement behind the same abstractions.

## Money and multi-tenancy rules

- All money/quantity values are `rust_decimal::Decimal` ↔ Postgres `NUMERIC(30,10)`. No floats
  anywhere in money-handling code or its tests.
- Every tenant-scoped query runs inside `db::tenant_tx`, which sets `app.account_id` for that
  transaction, **and** includes an explicit `WHERE account_id = $1`. Postgres RLS is the
  backstop, not the only gate — see [DATABASE.md](DATABASE.md).
- Every state change (order placed/filled/rejected/cancelled, mode switch, credential rotation)
  writes a `risk_events` row in the same transaction as the change.
