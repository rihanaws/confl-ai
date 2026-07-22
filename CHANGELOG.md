# Changelog

Entries are grouped by phase, not by release version or date — this project has not yet cut a
versioned release.

## Phase 2 — Paper Trading

Adds Binance Spot exchange integration and an end-to-end paper-trading order lifecycle.

Added:
- `confluence-exchange` crate: Binance Spot REST/WS client, paper-trading matching engine,
  `ExchangeAdapter` trait unifying both, zero `sqlx` in this crate's dependency tree.
- Order placement API with full pre-trade filter validation (lot size, price, notional,
  percent-price — both `PERCENT_PRICE` and `PERCENT_PRICE_BY_SIDE` variants).
- Exchange config API: paper/live mode selection, AES-256-GCM encrypted credentials, two-layer
  mode-switch guard (application check + `SECURITY INVOKER` database trigger).
- Outbox worker: claim → hydrate → adapter call → persist pipeline for order submission,
  reconciliation, and cancellation, with a 5-state command lifecycle and fail-closed adapter
  resolution.
- Symbol metadata and market-data refresh from Binance Spot Testnet, feeding both order
  validation and the paper matching engine from one shared source per data type.
- 6 new migrations: orders, order fills, exchange configs, exchange commands (outbox),
  extended audit-event types, market-data state.
- 10 new database-integration tests covering fill dedup, reconciliation escalation, outbox
  command states, mode-switch guard (app layer + trigger under RLS), tenant isolation on the
  new tables, restart recovery, and a full paper order-to-position flow.

Known deviation from plan:
- Symbol metadata and market-data refresh use periodic REST polling rather than the planned
  WebSocket feed, as a temporary and swappable implementation choice — see
  `docs/PHASE2_STATUS.md`.

Not included:
- Live trading remains disabled. No authentication, no testnet soak testing, no
  failure-injection testing, no monitoring, no written approval — the live-enablement gate is
  untouched.

## Phase 1 — Core Risk Engine

Initial implementation: safety-critical foundation only, no exchange connectivity.

Added:
- `confluence-core` crate: pure Rust risk engine (`evaluate()`), zero I/O. Five controls: kill
  switch, daily max-loss circuit breaker, per-trade size limit (% of equity), max concurrent
  positions, correlated exposure.
- `confluence-server` crate: axum API + sqlx/Postgres persistence.
- Multi-tenant Postgres schema with Row-Level Security on every tenant table and an
  append-only `risk_events` audit log (enforced by both a trigger and revoked grants).
- Account, risk-config, correlation-group, kill-switch, and trade-evaluation endpoints, all
  writing their audit trail in the same transaction as the state change.
