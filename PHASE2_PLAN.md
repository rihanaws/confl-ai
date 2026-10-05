# Confluence Phase 2 — Implementation Plan

## Scope

- **Product:** Binance Spot only (no futures/margin)
- **Modes:** Mutually exclusive per account — `paper` or `live`
- **Runtime:** Same axum binary, tokio tasks supervised per account
- **Paper data:** Binance public WebSocket depth/trade feeds
- **Credentials:** AES-256-GCM at rest, `bytea` in Postgres, decrypted just-in-time in route handlers (never in `db.rs`)
- **ExchangeInfo:** Shared `SymbolMetadataStore` — fetches from correct environment (testnet: `https://testnet.binance.vision`, live: `https://api.binance.com`)
- **No Binance SDK — custom client:** HMAC-SHA256 signing via `hmac` + `sha2`, `reqwest` with `rustls-tls`, `tokio-tungstenite` with `rustls-tls-native-roots`. No `ring`, no `openssl`, no `base64`.

---

## Architecture

```
crates/exchange/                          # No sqlx, no I/O to DB
  src/
    lib.rs
    types.rs            → SymbolMeta, LotSizeFilter, MinNotionalFilter, PercentPriceFilter,
                          OrderStatus, Fill, ConservativePriceEstimate, PriceSource, etc.
    error.rs            → FillExceedsRemaining, InsufficientBalance, MarketDataError — typed errors, no assert!
    adapter.rs          → ExchangeAdapter trait (reconcile_order, reconcile_account, submit, cancel, start, shutdown)
    config.rs           → EncryptionKey — encrypt/decrypt only here. No base64.
    order_state.rs      → OrderStateMachine with validated transitions
    outbox.rs           → CommandDispatcher trait only — no sqlx
    paper/
      mod.rs            → PaperAdapter. reconcile_order uses local fill ledger only. Never calls Binance.
      paper_account.rs  → Free balances per asset, equity kept separate. SymbolMeta-based.
      matching_engine.rs→ Price-time priority, Decimal everywhere, quantization helpers
      market_data.rs    → Binance WS depth/trade feed, ConservativePriceEstimate, gap/stale detection
    binance/
      mod.rs            → BinanceAdapter. reconcile_order uses GET /api/v3/order?origClientOrderId=
      rest.rs           → HMAC-SHA256, ExchangeInfo fetch, market data provider
      ws.rs             → tokio-tungstenite rustls, public + user data streams
      reconcile.rs      → Order lookup, openOrders, account balances (no positions endpoint)

crates/server/
  src/
    routes/
      mod.rs            → AppState { pool, supervisors, symbol_metadata }
      accounts.rs       → Existing handlers (moved)
      orders.rs         → ExchangeInfo validation + market data price estimate + evaluate → order creation → command
      exchange.rs       → Config CRUD (decrypt here, not db.rs), sync
    outbox_worker.rs    → SKIP LOCKED claim loop, routes by command.exchange_mode to correct adapter
    symbol_metadata.rs  → Shared refreshable store, periodic refresh, fail-closed on stale
    db.rs               → Extended queries — no decrypt, no base64
    error.rs            → Extended ApiError
    main.rs             → SymbolMetadataStore init, supervisor lifecycle, startup reconcile
  migrations/
    0006_orders.sql         → + exchange_mode column (paper|live)
    0007_order_fills.sql    → exchange_trade_id unique per account
    0008_exchange_configs.sql → + mode-switch trigger, SECURITY INVOKER
    0009_exchange_commands.sql → + exchange_mode, two failure states (reconciliation_required / permanently_rejected)
    0010_risk_event_types.sql
    0011_market_data_state.sql
```

---

## Fixes Applied to Original Plan

### Fix 1: Consistent Testnet URL Construction

Store `base_url` **without the `/api` suffix**, and all REST endpoints prepend `/api` in the path:

```
Testnet: base_url = "https://testnet.binance.vision"
Live:    base_url = "https://api.binance.com"

let url = format!("{base_url}/api/v3/exchangeInfo");       // testnet → https://testnet.binance.vision/api/v3/exchangeInfo
let url = format!("{base_url}/api/v3/order");              // live    → https://api.binance.com/api/v3/order
```

Applied to all REST calls: `exchangeInfo`, `order`, `openOrders`, `account`, `allOrders`, `myTrades`, user data stream `listenKey`.

### Fix 2: Paper Reconciliation Respects Crate Boundaries

`crates/exchange` (no sqlx) **never** reads `order_fills` or any durable store. Reconciliation is split:

```
┌─ crates/server (owns sqlx) ───────────────────────┐
│  RecService::reconcile_order(account_id, order_id) │
│    BEGIN tenant_tx                                  │
│    SELECT orders, order_fills FOR UPDATE            │
│    Build PaperLedgerSnapshot {                      │
│      order, fills, positions                        │
│    }                                                │
│    → paper_adapter.reconcile(snapshot)              │
│      // adapter compares snapshot with in-mem state │
│      // returns ReconcileResult {                   │
│      //   new_status, fills_to_add,                 │
│      //   positions_delta, events                   │
│      // }                                           │
│    INSERT/UPDATE as needed (fills, orders,           │
│      positions, risk_events, commands)               │
│    COMMIT                                           │
└────────────────────────────────────────────────────┘
```

The adapter trait gains:

```rust
// crates/exchange/src/adapter.rs
pub struct PaperLedgerSnapshot {
    pub order: OrderSnapshot,
    pub fills: Vec<FillSnapshot>,
    pub positions: Vec<PositionSnapshot>,
}

pub struct ReconcileResult {
    pub reconciled_status: OrderStatus,
    pub new_fills: Vec<Fill>,
    pub position_deltas: Vec<(String, Decimal)>,  // asset, delta
    pub events: Vec<RiskEvent>,
    pub command: Option<ExchangeCommand>,
}
```

Live reconciliation (`BinanceAdapter`) calls Binance REST (`GET /api/v3/order?origClientOrderId=`) directly, then returns a `ReconcileResult` for the server to persist — no sqlx in the adapter.

### Fix 3: Complete Notional and Percent-Price Filters

```rust
// Replaces the incomplete MinNotionalFilter
pub struct NotionalFilter {
    pub min_notional: Option<Decimal>,
    pub max_notional: Option<Decimal>,
    pub apply_min_to_market: bool,
    pub apply_max_to_market: bool,
    pub avg_price_mins: u32,
}

// Replaces a single up/down pair
pub enum PercentPriceRule {
    General {
        multiplier_up: Decimal,
        multiplier_down: Decimal,
        avg_price_mins: u32,
    },
    BySide {
        bid_multiplier_up: Decimal,
        bid_multiplier_down: Decimal,
        ask_multiplier_up: Decimal,
        ask_multiplier_down: Decimal,
        avg_price_mins: u32,
    },
}
```

These are stored on `SymbolMeta` and applied in validation as follows:

- **Limit buy**: check ask-side percent bounds if `BySide`, else use `General` bidirectionally.
- **Limit sell**: check bid-side percent bounds if `BySide`, else use `General` bidirectionally.
- **Market buy/sell**: apply `notional.apply_max_to_market` / `apply_min_to_market` flags.

### Fix 4: Separate Validation Functions

```rust
fn validate_order(intent: &OrderIntent, symbol: &SymbolMeta, price_est: Option<PriceEstimate>) -> Result<()> {
    // 1. Symbol trading status, allowed order types, base/quote asset format
    // 2. Common lot filter: LOT_SIZE or MARKET_LOT_SIZE depending on order type
    // 3. MIN_NOTIONAL check
    match intent.order_type {
        OrderType::Limit => validate_limit(intent, symbol, price_est),
        OrderType::Market => validate_market(intent, symbol, price_est),
    }
}

fn validate_limit(intent: &OrderIntent, symbol: &SymbolMeta, price_est: PriceEstimate) -> Result<()> {
    // PRICE_FILTER (tick_size, min/max price)
    // PERCENT_PRICE or PERCENT_PRICE_BY_SIDE against reference price
}

fn validate_market(intent: &OrderIntent, symbol: &SymbolMeta, price_est: PriceEstimate) -> Result<()> {
    // require fresh price estimate — stale feed → 422
    // notional max/min market flags
    // reserve quote (buy) or base (sell) with slippage/fee buffer
}
```

`validate_order()` is called in the route handler **before** `tenant_tx` — filter validation fails fast without a DB trip. The risk engine `evaluate()` runs inside the transaction after reservation.

---

## Cargo Dependencies (exchange crate)

```toml
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }
tokio-tungstenite = { version = "0.24", default-features = false, features = ["connect", "rustls-tls-native-roots"] }
aes-gcm = "0.10"
hmac = "0.12"
sha2 = "0.10"
tokio = { version = "1", features = ["macros", "rt-multi-thread", "sync", "time"] }
futures = "0.3"
async-trait = "0.1"
rust_decimal.workspace = true
uuid.workspace = true
chrono.workspace = true
serde.workspace = true
serde_json.workspace = true
thiserror.workspace = true
tracing = "0.1"
dashmap = "6"
```

No `sqlx`, no `ring`, no `openssl`, no `base64`.

---

## Supervisor: Single Adapter per Mode

Each supervisor owns **one adapter** (matching its configured mode). When dispatching a command:

```
command.exchange_mode → resolve adapter:
  paper → supervisor.paper_adapter  (Some)
  live  → supervisor.live_adapter   (Some)
  None  → fail closed: insert reconciliation_required command, log event
```

No `.unwrap()`. If the resolved adapter is `None`, the command enters `reconciliation_required` and a manual review event is logged.

---

## Fill Processing Order

```
1. BEGIN tenant_tx, set app.account_id
2. SELECT ... FROM orders WHERE id = $1 AND account_id = $2 FOR UPDATE
   → 404 if not found
3. Validate fill matches order (symbol, side)
4. SELECT 1 FROM order_fills WHERE account_id = $1 AND exchange_trade_id = $2
   → If EXISTS: return Duplicate — true no-op, no event, no reconciliation
5. If NOT EXISTS and order.status is terminal:
   → Novel fill for terminal order → state divergence
   → INSERT exchange_commands (command_type='reconcile_order', status='pending',
        idempotency_key='{oid}:reconcile:novel_fill:{tid}')
     ON CONFLICT (account_id, idempotency_key) DO NOTHING
   → INSERT risk_event('exchange_reconciliation_required')
   → return Ok
6. If NOT EXISTS and order is non-terminal:
   → Validate fill ≤ remaining (typed FillExceedsRemaining error, no assert!)
   → INSERT order_fills ON CONFLICT (account_id, exchange_trade_id) DO NOTHING
   → Compute new status, update order, positions, reservations
   → INSERT risk_event
7. COMMIT
```

---

## Order Placement Flow

```
POST /accounts/{id}/orders
  1. Validate symbol via SymbolMetadataStore (exists, spot allowed, order type allowed)
  2. Validate quantity/price against market filters:
     - LOT_SIZE / MARKET_LOT_SIZE (min_qty, max_qty, step_size via is_aligned)
     - PriceFilter (min_price, max_price, tick_size)
     - MIN_NOTIONAL (respecting applyToMarket flag)
     - PERCENT_PRICE / PERCENT_PRICE_BY_SIDE (for limit orders)
  3. For market orders: estimate notional from MarketDataProvider conservative price.
     If feed stale → reject.
  4. BEGIN tenant_tx, lock_account FOR UPDATE
  5. Load risk_config + load_snapshot_with_reservations (includes pending orders)
  6. Asset-specific balance check:
     - Buy:  reserve quote_asset at limit_notional + fee buffer
     - Sell: reserve base_asset quantity
  7. confluence_core::evaluate(intent, snapshot, risk_config)
  8. If rejected → INSERT risk_event('order_rejected') → COMMIT → return 422
  9. If approved:
     - client_order_id = order_id.to_string().replace('-', '')  // 32 hex chars
     - INSERT orders (status='pending', exchange_mode=config.mode)
     - INSERT exchange_commands (command_type='submit_order', status='pending',
         exchange_mode=config.mode, idempotency_key='{oid}:submit')
     - INSERT risk_event('order_placed')
     - COMMIT
     - return 201
```

---

## Asset Reservations

```
available_base(asset)  = exchange_or_paper_free_balance(asset)  - Σ sell_reservations(asset)
available_quote(asset) = exchange_or_paper_free_balance(asset)  - Σ buy_reservations(asset)

accounts.equity        = total portfolio value (unchanged, used only by evaluate())

Reservation for cancel_requested: HELD. Released only on confirmed cancelled.
Reserved capacity releases atomically in the same TX as: filled, cancelled, rejected.
```

---

## Exchange Commands — States

```
pending                → claimable. Lease expiry returns to pending with backoff.
leased                 → in-flight. Expires after timeout.
delivered              → terminal success.
reconciliation_required → non-claimable. Ambiguous — a separate reconcile_order command handles it.
permanently_rejected   → non-claimable. Binance 4xx deterministic rejection. Order → rejected.

reconcile_order commands: always inserted in pending with idempotency_key + ON CONFLICT DO NOTHING.
```

---

## Mode-Switch Guard (Two Layers)

1. **Application layer** (in `put_exchange_config`): Count non-terminal orders + open positions before update.
2. **DB trigger** (`exchange_configs` `BEFORE UPDATE`):
   ```sql
   IF EXISTS (SELECT 1 FROM orders WHERE account_id = NEW.account_id
              AND status NOT IN ('filled','cancelled','rejected'))
   OR EXISTS (SELECT 1 FROM positions WHERE account_id = NEW.account_id
              AND status = 'open' AND quantity > 0)
   → RAISE EXCEPTION
   ```
   `SECURITY INVOKER`. Tested via `APP_DATABASE_URL` with `app.account_id` set.

---

## Decimal Quantization (no `%` operator)

```rust
fn is_aligned(value: Decimal, step: Decimal) -> bool {
    if step.is_zero() { return false; }
    (value / step).fract().is_zero()
}

fn quantize_down(value: Decimal, step: Decimal) -> Decimal {
    ((value / step).floor()) * step  // error messages only — never silently alter order
}
```

---

## Test Plan (26 items)

| # | Test | Key Validation |
|---|------|----------------|
| 1 | Encryption roundtrip | AES-256-GCM, `bytea`, AAD binding, no plaintext |
| 2 | Order state transitions | All legal/illegal paths, terminal immutability |
| 3 | Fill locking + duplicate ordering | FOR UPDATE before insert, duplicate check before escalation |
| 4 | Duplicate fill → true no-op | Same `exchange_trade_id` replayed → no state change |
| 5 | Novel fill for terminal order → reconcile | Unique fill for filled order → `reconcile_order` command, event logged |
| 6 | Paper reconciliation (local ledger) | Snapshot passed by server; adapter compares in-mem state; server persists result |
| 7 | Live reconciliation (REST) | `origClientOrderId` query → state synced |
| 8 | Exchange mode on order/command | Row has `exchange_mode` set. Dispatch routed by mode. |
| 9 | Expanded SymbolMeta filters | `market_lot_size`, `notional` (min+max+market flags), `percent_price` (BySide variant) all parsed |
| 10 | Market order price estimation | Fresh ask → estimate. Stale feed → 422. |
| 11 | Market order MIN_NOTIONAL | `apply_to_market=true` + below min → 422 |
| 12 | Limit order PERCENT_PRICE | Outside multiplier range → 422; BySide tested per side |
| 13 | Decimal alignment (no `%`) | `is_aligned(1.5, 0.1)=true`, `(1.55, 0.1)=false` |
| 14 | Quantize down (error only) | Never silently alter. Error shows step + example. |
| 15 | Symbol metadata — testnet URL | Store initialized with correct origin-only base URL; `/api/v3/...` paths |
| 16 | Paper matching engine | Decimal arithmetic only. Zero floats. |
| 17 | Paper stale data → no fills | Degraded → market order rejected |
| 18 | Paper full flow | Evaluate → order (mode=paper) → command → fill → positions → events |
| 19 | Paper risk rejection | 422, no order row |
| 20 | Outbox permanent rejection | 4xx → `permanently_rejected` → not claimable |
| 21 | Outbox timeout → reconciliation | Lease expires → `reconciliation_required` + `reconcile_order` pending |
| 22 | Mode switch app-layer | Open order → 409 |
| 23 | Mode switch trigger (app role + RLS) | Direct SQL via `APP_DATABASE_URL` → trigger blocks |
| 24 | RLS tenant isolation | Cross-tenant → empty results |
| 25 | Restart recovery | Non-terminal at shutdown → reconcile on restart |
| 26 | No float literals | Zero `f64`/`f32` in tests, zero float arithmetic for money |

---

## Non-Negotiable Rules (all preserved)

- `confluence_core` remains pure, zero-I/O
- `Decimal` ↔ `NUMERIC(30,10)` — no floats
- Every query: `tenant_tx` + `WHERE account_id = $1` + RLS backstop
- Every state change: `risk_events` row in the same transaction
- `risk_events` is append-only (trigger + grants, unchanged)
- Credentials never logged, returned, or committed
- `dotenvy::dotenv_override()` in all binaries
- sqlx 0.8 (no 0.9 driver-crate split)
- No remote migrations (reads `DATABASE_URL` from `.env`)
- No commit/push without explicit approval

---

## Implementation Order

1. Workspace deps + `crates/exchange` member (no sqlx, no ring, no base64)
2. Migrations 0006–0011 (with exchange_mode columns, mode-switch trigger)
3. `types.rs`, `error.rs`, `adapter.rs`, `config.rs`, `order_state.rs`, `outbox.rs` (no sqlx)
4. `symbol_metadata.rs` — `SymbolMeta`, `SymbolMetadataStore`, filter parsing (including `NotionalFilter`, `PercentPriceRule::BySide`)
5. Paper matching engine + paper_account + market_data + `PaperAdapter` (reconciliation via snapshot)
6. Paper integration tests (full flow)
7. Binance REST client (HMAC-SHA256, ExchangeInfo fetch, market data) — origin-only base URL + `/api/v3/...` paths
8. Binance WS (public + private streams)
9. Binance `reconcile.rs` — `origClientOrderId` lookup, balance reconciliation (returns `ReconcileResult`, no sqlx)
10. `routes/orders.rs` (split `validate_order`/`validate_limit`/`validate_market`), `routes/exchange.rs`, `outbox_worker.rs` (single adapter resolution)
11. Supervisor lifecycle in `main.rs`
12. Full integration test suite (26 tests, Decimal-only, app-role trigger test)

---

## Live Enablement Gate

Production live trading requires before enablement:

- User authentication + authorization implemented
- Testnet soak tests pass (7+ days)
- All failure injection tests pass (process kill, duplicate messages, timeout, stale feed, DB retry)
- Monitoring, alerting, key rotation, incident runbook reviewed
- Explicit written approval
