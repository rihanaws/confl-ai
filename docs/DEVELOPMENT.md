# Development

## Prerequisites

- Rust (edition 2021 toolchain; workspace pins `sqlx` to `0.8.x` deliberately — the 0.9
  driver-crate split is not adopted for money-handling code)
- A Postgres database (this project develops against Neon)
- `psql` (for `scripts/bootstrap_app_role.sql`)

## Environment

Create a `.env` at repo root (git-ignored, never commit) with:

```bash
DATABASE_URL="postgresql://<owner>:<password>@<host>/<db>?sslmode=require"
APP_DATABASE_URL="postgresql://confluence_app:<password>@<host>/<db>?sslmode=require"

# Optional
LISTEN_ADDR="127.0.0.1:8080"          # default if unset
EXCHANGE_ENCRYPTION_KEY="<64 hex chars>"  # required only for the exchange-config route (AES-256-GCM key, 32 bytes)
```

Every binary calls `dotenvy::dotenv_override()`, so this file always wins over anything your
shell already exports — important if your shell profile sets a global `DATABASE_URL` pointing
at an unrelated local database. Pass explicit URLs in one-off shell commands too:
`grep '^DATABASE_URL' .env | cut -d'"' -f2`.

## First-time setup on a fresh database

```bash
# 1. Create the confluence_app login role (cluster-wide; needs the owner connection + a password)
psql "$DATABASE_URL" -v app_password='<password>' -f scripts/bootstrap_app_role.sql

# 2. Apply all migrations
cargo run -p confluence-server --bin migrate
```

## Running

```bash
cargo run -p confluence-server          # serves on LISTEN_ADDR (default 127.0.0.1:8080)
```

On startup the server:
- connects both the app pool (`APP_DATABASE_URL`) and the owner pool (`DATABASE_URL`, used only
  by the outbox worker's cross-tenant command claim — see `docs/DATABASE.md`)
- fetches Binance Spot Testnet `exchangeInfo` to hydrate the symbol metadata store, and errors
  loudly (but doesn't crash) if that fails — the store then stays stale and every order is
  fail-closed rejected until a refresh succeeds
- starts a periodic market-data refresh (bookTicker poll) and the outbox worker loop

## Tests

```bash
cargo test --workspace              # unit + integration; integration tests need a real .env
cargo clippy --workspace --all-targets
```

Test layout:
- `crates/core` — pure unit tests, no I/O, no `.env` needed.
- `crates/exchange` — unit tests (matching engine, filters, encryption, state machine) plus a
  handful of tests that make real network calls to Binance Spot Testnet's public REST endpoints
  (`fetch_exchange_info`, `fetch_book_ticker`) — these need outbound internet access.
- `crates/server/tests/api.rs` — Phase 1 integration tests against the real database.
- `crates/server/tests/phase2_exchange.rs` — Phase 2 integration tests: fill dedup, terminal-
  order reconciliation escalation, outbox command states, mode-switch guard (both layers), RLS
  isolation on the new tables, restart-recovery discovery, and one full paper-trading flow
  through the real outbox worker (order → submit → fill → position → risk event).

Each integration test creates its own account(s), so tests are isolated and safe to run in
parallel against the same database.

## Conventions

- Money is always `rust_decimal::Decimal`. Never `f32`/`f64` in money-handling code or tests.
- Every tenant query goes through `db::tenant_tx` and carries an explicit `WHERE account_id`.
- Every state change writes a `risk_events` row in the same transaction.
- Don't commit or push without explicit review — see `CLAUDE.md`.
