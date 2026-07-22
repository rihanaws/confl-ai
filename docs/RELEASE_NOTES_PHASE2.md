# Phase 2 Release Notes — Paper Trading

Adds Binance Spot exchange integration and a fully working paper-trading order lifecycle on
top of Phase 1's risk engine and multi-tenant schema. Full detail in
[`PHASE2_STATUS.md`](PHASE2_STATUS.md); this is the short version.

## What's new

- **New crate `confluence-exchange`** — Binance Spot adapter + paper-trading engine, with no
  `sqlx` dependency, so exchange logic is testable without a database.
- **Order placement API** (`POST /accounts/{id}/orders`) — full pre-trade filter validation
  (lot size, price, notional, percent-price) ahead of the Phase 1 risk engine, atomic
  order + outbox-command + audit-event insert on approval.
- **Exchange config API** (`GET`/`PUT /accounts/{id}/exchange-config`) — paper/live mode
  selection, AES-256-GCM encrypted API credentials, two-layer mode-switch guard (application
  check + a `SECURITY INVOKER` database trigger).
- **Outbox worker** — background command dispatch with a 5-state lifecycle
  (`pending`/`leased`/`delivered`/`reconciliation_required`/`permanently_rejected`), full
  hydrate → adapter call → persist pipeline for `submit_order`/`reconcile_order`/`cancel_order`.
- **Paper trading engine** — local price-time-priority matching against live Binance Spot
  Testnet market data, position tracking. A per-asset balance reservation ledger exists and is
  unit-tested but is not yet wired into order placement — see `docs/PHASE2_STATUS.md`.
- **6 new migrations** (0006–0011) — orders, fills, exchange configs, outbox commands,
  extended audit-event types, market-data state, all under the same RLS + append-only-audit
  model as Phase 1.

## What changed under the hood

- `AppState` now carries a shared symbol-metadata store and a shared market-data feed (both
  fail-closed) alongside the database pool.
- The outbox worker's cross-tenant command claim uses a second, narrowly-scoped database pool
  connected as the owner role (`BYPASSRLS`) — documented in [`DATABASE.md`](DATABASE.md) as an
  explicit, bounded exception to "app connections use `APP_DATABASE_URL`."

## Verified in this session

100/100 tests passing across the workspace, including live network calls to Binance Spot
Testnet (`exchangeInfo`, `bookTicker`) and 10 database-integration tests against the real
Neon instance. A live server process was started and smoke-tested: account creation, a limit
order correctly rejected against real fetched price bounds, a valid limit order accepted
end-to-end with the expected audit trail.

## Known deviation from plan

Symbol metadata and market data refresh via **periodic REST polling**, not the planned
WebSocket feed. Temporary and swappable — see [`PHASE2_STATUS.md`](PHASE2_STATUS.md) for
detail. The WS client code exists and is tested but is not the live path yet.

## Not included

**Live trading remains disabled.** No authentication/authorization, no testnet soak, no
failure-injection testing, no monitoring, no written sign-off — the plan's live-enablement
gate is untouched by design. Nothing in this release should be interpreted as ready to place
real-money orders.
