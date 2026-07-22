# Confluence

Non-custodial, AI-assisted multi-asset trading platform.

![Rust](https://img.shields.io/badge/rust-2021-orange)
![Axum](https://img.shields.io/badge/axum-0.8-informational)
![SQLx](https://img.shields.io/badge/sqlx-0.8-informational)
![Postgres](https://img.shields.io/badge/postgres-Neon-blue)
![Binance Spot](https://img.shields.io/badge/binance-spot%20testnet-yellow)
![License: TBD](https://img.shields.io/badge/license-TBD-lightgrey)
![Phase 2: Paper Complete](https://img.shields.io/badge/phase%202-paper%20trading%20complete-brightgreen)

> ⚠️ **Live trading is disabled.** This repo implements paper trading against Binance Spot
> Testnet end-to-end. Real-money (live) order placement is not wired up, not authenticated,
> not soak-tested, and not approved for use — see [docs/PHASE2_STATUS.md](docs/PHASE2_STATUS.md).

## What this is

A safety-first trading platform built in layers:

- **Phase 1** — a pure Rust risk engine (kill switch, daily max-loss circuit breaker,
  per-trade size limits, max concurrent positions, correlated exposure) and a multi-tenant
  Postgres schema with Row-Level Security and an append-only audit log.
- **Phase 2** (current) — a Binance Spot exchange adapter and a paper-trading engine on top of
  Phase 1, with the same money-handling and auditability guarantees.

## Architecture, in short

Three crates: `confluence-core` (the risk engine, zero I/O), `confluence-exchange` (Binance
adapter + paper trading, zero `sqlx`), `confluence-server` (axum API + Postgres + a background
outbox worker that dispatches order commands to whichever adapter matches the account's mode).
Every tenant query is RLS-scoped and carries an explicit `account_id` filter; every state
change is written to an append-only audit table in the same transaction. All money is
`rust_decimal::Decimal` — never a float.

Full detail: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Getting started

```bash
# .env (git-ignored): DATABASE_URL, APP_DATABASE_URL — see docs/DEVELOPMENT.md
psql "$DATABASE_URL" -v app_password='<password>' -f scripts/bootstrap_app_role.sql
cargo run -p confluence-server --bin migrate
cargo run -p confluence-server
```

```bash
cargo test --workspace
cargo clippy --workspace --all-targets
```

Full setup, env vars, and test layout: [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md).

## Documentation

See [docs/INDEX.md](docs/INDEX.md) for the full set: architecture, database/schema, local
development, and Phase 2 status/release notes.

## License

TBD (not yet chosen).
