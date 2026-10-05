# Confluence

Non-custodial AI-assisted multi-asset trading platform (TechSci Inc.). Phase 1 (safety-critical core: pure Rust risk engine, per-account risk config, multi-tenant Postgres schema with append-only audit log) and Phase 2 (Binance Spot exchange adapter + paper trading, end-to-end) are both done. Live trading is explicitly disabled — see `docs/PHASE2_STATUS.md`. No auth, no LLM worker yet (Phase 3+).

Full docs: `docs/INDEX.md`. Don't re-explain architecture/status here beyond what an agent needs to avoid regressing it — those docs are the source of truth, this file is working rules.

## Layout

- `crates/core` — pure risk engine (`evaluate()`), zero I/O. All five controls: kill switch, daily max-loss circuit breaker, per-trade size % equity, max concurrent positions, correlated exposure.
- `crates/exchange` — Binance Spot adapter + paper trading engine, zero `sqlx` in its dependency tree by design. See `docs/ARCHITECTURE.md`.
- `crates/server` — axum API + sqlx (Postgres) + outbox worker. Migrations in `crates/server/migrations/`.
- `scripts/bootstrap_app_role.sql` — one-time creation of `confluence_app` login role (run as owner before first migrate on a fresh DB).
- `.github/workflows/ci.yml` — build + clippy (`-D warnings`) + full test suite against a postgres service container, on every push/PR to `main`.
- `.github/workflows/network-tests.yml` — the 2 `#[ignore]`d live-Binance-Testnet tests, manual/weekly only (kept out of PR CI).

## Commands

```bash
cargo test --workspace                          # unit + integration (integration needs .env); 2 tests are #[ignore]d (live network)
cargo test --workspace -- --ignored             # the 2 live Binance Testnet tests
cargo clippy --workspace --all-targets -- -D warnings
cargo run -p confluence-server --bin migrate    # apply migrations (uses DATABASE_URL from .env)
cargo run -p confluence-server                  # serve on 127.0.0.1:8080 (uses APP_DATABASE_URL)
```

## Database

Neon project `confluence` (id `sweet-math-60573043`), branch `main`, DB `neondb`. Two connection strings in `.env` (git-ignored, never commit):
- `DATABASE_URL` — owner role (`BYPASSRLS`). Migrations, and one narrow runtime exception: the outbox worker's claim/status-update pool in `main.rs`/`outbox_worker.rs`, because `exchange_commands`' tenant RLS policy has no notion of "the worker" and would otherwise block its cross-tenant batch claim. That pool is scoped to `exchange_commands` claim/`mark_*`/`requeue` calls only — never passed into `AppState`, never reachable from an HTTP handler, never used for account/order/fill/position writes. Any new use of `DATABASE_URL` at runtime outside that narrow surface is a regression; flag it.
- `APP_DATABASE_URL` — `confluence_app` role, what the server (and all HTTP handlers) use. Subject to RLS; no UPDATE/DELETE on `risk_events`.

Rules that must not regress:
- Money is `rust_decimal::Decimal` ↔ `NUMERIC(30,10)`. Never floats.
- Every tenant query: explicit `WHERE account_id` AND `set_config('app.account_id', ..., true)` in the same transaction (see `db::tenant_tx`). RLS is the backstop, not the only gate.
- Every state change writes its `risk_events` row in the same transaction.
- `risk_events` is append-only (grants + trigger). Accounts with audit history are undeletable by design.
- Risk-limit defaults in `0001_schema.sql` are PLACEHOLDERS — production numbers are a pending financial decision, do not pick them.

## Environment gotchas

- The developer shell exports a global `DATABASE_URL` pointing at a local `claude_cache_db`. This project must NEVER touch that DB. All binaries use `dotenvy::dotenv_override()` so the project `.env` wins — keep it that way, and pass explicit URLs in shell commands (`grep '^DATABASE_URL' .env | cut -d'"' -f2`).
- sqlx pinned to 0.8 deliberately (0.9 driver-crate split not adopted for money code).

## Process

- Do not commit or push without the user's explicit go-ahead.
- Phase 3 (LLM worker) and any live-trading enablement work only when explicitly requested. Live enablement additionally requires the full gate in `docs/PHASE2_STATUS.md` (auth, testnet soak, failure injection, monitoring, written approval) — do not treat any subset of that as sufficient.
