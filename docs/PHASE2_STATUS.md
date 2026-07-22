# Phase 2 Status

Full plan: [`../PHASE2_PLAN.md`](../PHASE2_PLAN.md). This document reflects what is actually
in the repo, verified by running `cargo test --workspace`, `cargo clippy --workspace
--all-targets`, and live smoke tests against the real Neon database and Binance Spot Testnet
in the session that built this.

**Status: paper trading is complete and verified end-to-end. Live trading is not enabled.**

## Delivered and verified

- `crates/exchange` — Binance Spot adapter + paper trading engine, zero `sqlx` in its
  dependency tree (confirmed via `cargo tree`; the only transitive `ring`/`base64` come from
  `rustls`/`reqwest`, not from a competing TLS/encoding stack).
- Migrations 0006–0011 applied and verified against the live database: orders, order_fills,
  exchange_configs (mode-switch trigger confirmed `SECURITY INVOKER` via `pg_proc.prosecdef`),
  exchange_commands (5-state outbox), extended risk_events whitelist, market_data_state.
- Full order-filter validation: LOT_SIZE/MARKET_LOT_SIZE, PRICE_FILTER, NOTIONAL (both
  min/max, both apply-to-market flags), PERCENT_PRICE and PERCENT_PRICE_BY_SIDE — both
  variants wired into validation, not just parsed.
- Paper matching engine and `PaperAdapter` reconciling from a server-hydrated snapshot only
  (the adapter never reads `order_fills` directly). A per-asset balance reservation ledger
  (`PaperAccount`) exists and is unit-tested, but is **not yet called** from the order
  placement route or the outbox worker — see Known gaps below.
- Binance REST client: HMAC-SHA256 signing, origin-only base URL with every endpoint
  (`exchangeInfo`, `order`, `openOrders`, `account`, `allOrders`, `myTrades`,
  `userDataStream`, `ticker/bookTicker`) appending its own `/api/v3/...` path.
- `BinanceAdapter::reconcile_order` via `GET /api/v3/order?origClientOrderId=`.
- AES-256-GCM credential encryption, AAD-bound to account id, on write in
  `routes_exchange.rs`. Encryption/decryption never happens in `db.rs` or `crates/exchange`.
  No code path decrypts and uses these credentials yet — that's part of the live-adapter
  wiring the enablement gate blocks, not something this repo does today.
- Outbox worker: real claim → hydrate → adapter call → persist pipeline (not a stub) —
  `submit_order` transitions `pending → submitted` and auto-enqueues the first
  `reconcile_order`; `reconcile_order` persists new fills, position deltas, a
  state-machine-validated order-status transition, and risk events, all in one transaction;
  deterministic exchange errors (bad symbol, filter violation, insufficient balance) become
  `permanently_rejected` and flip the order to `rejected`, everything else retries.
- Symbol metadata and market-data both wired live: startup fetch + periodic refresh from
  Binance Spot Testnet, shared between the order-placement route and the paper adapter via
  one `Arc<Mutex<..>>` instance each — confirmed live with a $1 BTC limit order correctly
  rejected against real fetched price bounds, and a valid order accepted end-to-end.
- Fail-closed behavior confirmed by design and by test: unknown/stale symbol metadata,
  stale/missing market data, and a missing adapter for a command's `exchange_mode` all reject
  or escalate rather than proceeding on unverified data. No `.unwrap()` on adapter resolution.
- **100/100 tests passing** (`cargo test --workspace`), including 10 Phase 2 database
  integration tests (fill dedup, terminal-order reconciliation escalation, outbox command
  states, mode-switch guard under both the app layer and the DB trigger — tested as the
  `confluence_app` role under RLS, not just as a superuser — RLS isolation on the new tables,
  restart-recovery discovery, and one full paper order-to-position flow through the real
  outbox worker). `cargo clippy --workspace --all-targets` is clean except one pre-existing
  stylistic lint (a `match` clippy would prefer as `matches!`).

## Documented deviation from the plan

The plan specifies Binance's public WebSocket depth/trade stream as the market-data and
symbol-metadata source. This repo currently uses **periodic REST polling** instead
(`GET /api/v3/exchangeInfo` every 60s, `GET /api/v3/ticker/bookTicker` every 5s per watchlist
symbol) as a **temporary, verifiable-now implementation choice**, not a change to the intended
architecture:

- The WS client code (`crates/exchange/src/binance/ws.rs` — URL builders, public-stream event
  parsing) already exists and is tested.
- Both the REST poller and a future WS listener feed the exact same abstractions
  (`SymbolMetadataStore::replace_all`, `MarketDataProvider::update`), so swapping the transport
  later does not require touching `routes_orders.rs`, `PaperAdapter`, or any validation logic.
- Reason for choosing REST first: a long-lived WS connection with reconnect/backoff logic is
  materially harder to verify as actually working within one working session than a REST call
  whose response can be inspected directly. REST was the mechanism that could be proven live
  against real Binance Spot Testnet endpoints in this session; WS was not exercised.

## Known gaps / not yet done

- **Asset reservations are not wired in.** `PaperAccount` (per-asset free-balance
  reserve/release) is implemented and unit-tested in isolation, but `routes_orders.rs` and
  `outbox_worker.rs` never construct or call it — order placement does not currently reserve
  quote/base asset balance against a paper account, so nothing yet prevents placing more paper
  orders than a paper balance could cover. Risk-engine equity/position checks still run
  independently and are unaffected.
- **Watchlist is hardcoded** (`BTCUSDT`, `ETHUSDT` in `main.rs`) rather than derived from
  account activity or a configurable list.
- **Multi-fill-per-reconcile-pass price attribution is simplified**: `outbox_worker`'s
  position-delta application uses the single new fill's price as the entry price, correct for
  both current adapters (each produces at most one new fill per pass) but would need
  per-symbol attribution if an adapter ever returns multiple fills in one `ReconcileResult`.
- **Position average-price weighting on partial adds is not implemented**: growing an existing
  same-direction position leaves `avg_entry_price` unchanged (it does not incorporate the new
  fill's price at all); a flip or a shrink resets it to the new fill's price instead of
  computing a quantity-weighted average. Fills/positions/events still persist atomically and
  correctly; this only affects the precision of `avg_entry_price`, which Phase 2 does not read
  back for any risk or reconciliation decision.
- **Live trading is explicitly not enabled.** Per the plan's live-enablement gate, all of the
  following remain undone: user authentication/authorization, testnet soak tests (7+ days),
  failure-injection tests (process kill, duplicate messages, timeout, stale feed, DB retry),
  monitoring/alerting/runbook, and explicit written approval. `BinanceAdapter` exists and is
  unit-tested against mapped responses, but nothing in this repo wires a live (non-testnet)
  connection, and no code path should be treated as ready to place real orders.
