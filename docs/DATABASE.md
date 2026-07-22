# Database

Postgres (Neon). Migrations live in `crates/server/migrations/`, applied via
`cargo run -p confluence-server --bin migrate` (uses `DATABASE_URL`).

## Roles

- Owner role (`DATABASE_URL`) — runs migrations. Has `BYPASSRLS`.
- `confluence_app` (`APP_DATABASE_URL`) — what the server (and every HTTP handler) connects
  as. Subject to Row-Level Security on every tenant table. No UPDATE/DELETE on `risk_events`.
  Created once via `scripts/bootstrap_app_role.sql` (run as owner before first migrate on a
  fresh database — role/password creation is cluster-wide and kept out of sqlx migrations).

**Runtime exception:** the outbox worker's command-claim/status-update calls use the owner
pool, because `exchange_commands`' tenant-isolation RLS policy has no notion of "the worker"
and would otherwise block its cross-tenant batch claim. That pool is scoped to
`exchange_commands` claim/`mark_*`/`requeue` operations only — never exposed to `AppState`,
never reachable from an HTTP handler, never used for account/order/fill/position writes. Any
new runtime use of the owner role outside that narrow surface is a regression.

## Tenant isolation

Every tenant table has RLS enabled and **forced** (so even the table owner is subject to it),
plus a `tenant_isolation` policy comparing `account_id` against
`current_setting('app.account_id', true)`. The app sets this per-transaction via
`db::tenant_tx`, which does `SELECT set_config('app.account_id', $1, true)` — `is_local = true`
so the setting dies with the transaction. Every query additionally carries an explicit
`WHERE account_id = $1`: RLS is the backstop, not the only gate.

## Migrations

| # | File | Adds |
|---|------|------|
| 0001 | `schema.sql` | accounts, risk_configs, correlation_groups(+assets), positions, daily_pnl, risk_events (append-only trigger) |
| 0002 | `rls.sql` | RLS + tenant_isolation policy on all Phase 1 tables, confluence_app grants |
| 0003 | `account_equity.sql` | `accounts.equity` |
| 0004 | `integrity_fixes.sql` | composite FK pinning correlation_group_assets to its account; risk_events FK changed to `ON DELETE RESTRICT` |
| 0005 | `equity_audit.sql` | equity non-negative check; adds `equity_changed` to risk_events whitelist |
| 0006 | `orders.sql` | `orders` table (status, exchange_mode, filled_quantity), RLS |
| 0007 | `order_fills.sql` | `order_fills` (append-only, unique per `account_id, exchange_trade_id`), RLS |
| 0008 | `exchange_configs.sql` | `exchange_configs` (mode, testnet, encrypted credentials); mode-switch guard trigger |
| 0009 | `exchange_commands.sql` | outbox table — 5 states, idempotency-key uniqueness, claimable index |
| 0010 | `risk_event_types.sql` | extends risk_events whitelist for Phase 2 event types |
| 0011 | `market_data_state.sql` | `market_data_state` (not tenant-scoped — shared market infra) |

## Append-only audit log

`risk_events` cannot be UPDATEd or DELETEd: enforced by a `BEFORE UPDATE OR DELETE` trigger
(`forbid_risk_event_mutation`) **and** by revoked grants on the app role. Accounts with audit
history are undeletable by design (`risk_events_account_fk` is `ON DELETE RESTRICT`).

## Mode-switch guard (paper ⇄ live)

Two independent layers, not redundant:

1. **Application layer** — `routes_exchange::put_exchange_config` counts non-terminal orders
   and open positions before allowing a mode change; rejects with 409 if any exist.
2. **DB trigger** — `exchange_configs_mode_switch_guard` (`BEFORE UPDATE`), `SECURITY INVOKER`
   (not `DEFINER`, so it runs as the calling role and its subqueries stay subject to RLS).
   Backstops any code path that bypasses the application layer. Verified under the app role
   with `app.account_id` set (not just as a superuser) — see
   `crates/server/tests/phase2_exchange.rs`.

## Outbox states

`exchange_commands.status`: `pending` (claimable) → `leased` (in-flight, lease expiry returns
it to pending) → `delivered` (terminal success), or `reconciliation_required` (ambiguous,
non-claimable, needs a `reconcile_order` follow-up) / `permanently_rejected` (deterministic
4xx-style rejection, non-claimable, order flips to `rejected`).

## Money types

Every money/quantity column is `NUMERIC(30,10)`, mapped to `rust_decimal::Decimal`. Percent
limits are `NUMERIC(8,4)` expressed as percentages (`2.0` = 2%), not fractions. No floats
anywhere in this schema or the Rust code that reads/writes it.

## Placeholder values

Risk-limit defaults in `0001_schema.sql` (`daily_max_loss_pct`, `max_position_pct_equity`,
`max_concurrent_positions`, `max_correlated_exposure_pct`) are engineering placeholders, not
production numbers — production values are a pending financial decision.
