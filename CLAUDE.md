# Confluence — Phase 1

Non-custodial AI-assisted multi-asset trading platform (TechSci Inc.). Phase 1 = safety-critical core only: pure Rust risk engine, per-account risk config, multi-tenant Postgres schema with append-only audit log. No exchange adapter, no LLM worker, no auth yet (Phase 2+).

## Layout

- `crates/core` — pure risk engine (`evaluate()`), zero I/O. All five controls: kill switch, daily max-loss circuit breaker, per-trade size % equity, max concurrent positions, correlated exposure.
- `crates/server` — axum API + sqlx (Postgres). Migrations in `crates/server/migrations/`.
- `scripts/bootstrap_app_role.sql` — one-time creation of `confluence_app` login role (run as owner before first migrate on a fresh DB).

## Commands

```bash
cargo test --workspace                          # unit + integration (integration needs .env)
cargo clippy --workspace --all-targets
cargo run -p confluence-server --bin migrate    # apply migrations (uses DATABASE_URL from .env)
cargo run -p confluence-server                  # serve on 127.0.0.1:8080 (uses APP_DATABASE_URL)
```

## Database

Neon project `confluence` (id `sweet-math-60573043`), branch `main`, DB `neondb`. Two connection strings in `.env` (git-ignored, never commit):
- `DATABASE_URL` — owner role, migrations only.
- `APP_DATABASE_URL` — `confluence_app` role, what the server uses. Subject to RLS; no UPDATE/DELETE on `risk_events`.

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
- Phase 2 (Binance adapter, paper trading) and Phase 3 (LLM worker) only when explicitly requested.
