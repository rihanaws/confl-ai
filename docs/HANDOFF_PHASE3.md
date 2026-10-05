# Handoff: Phase 2 complete → next phase

Date: 2026-10-05. Read `../CLAUDE.md` (working rules) and `PHASE2_STATUS.md` first; this file only records what changed after those were written and what to do next.

## State

- PR #1 (`review/phase2-greptile` → `phase2-base`) merged 2026-10-05, merge commit `6587c32`.
- **`main` does NOT have the review fixes yet.** `main` is still at `24a555f`. `phase2-base` has them. Landing them on `main` needs a PR/merge `phase2-base` → `main` (not done; needs user go-ahead).
- Greptile final score: 4/5 (subscription ended, no further automated review). 101 tests pass, 2 `#[ignore]`d (live Binance Testnet), clippy `-D warnings` clean.
- Live trading still disabled. Gate unchanged (see `PHASE2_STATUS.md`).

## Review fixes applied (commits `7ba5b23`, `6c40069`, `bf420b0`)

| Area | Fix |
|---|---|
| `exchange/config.rs`, `routes_exchange.rs` | AES-GCM nonce now random per encryption (`encrypt_fresh`). Old deterministic nonce reused the same key+nonce on every credential rotation. `encrypt()` with explicit nonce kept for tests only. Existing ciphertexts still decrypt (nonce is prefixed). |
| `outbox_worker.rs` | Successful `cancel_order` moves order to `CancelRequested`; reconcile finalises it. |
| `binance/reconcile.rs` | Fill price = `cummulativeQuoteQty / executedQty` (order `price` is 0 for market orders). |
| `validators.rs` | `PERCENT_PRICE_BY_SIDE`: buy uses bid multipliers, sell uses ask (was swapped). |
| `paper/matching_engine.rs` | Marketable limit fills at the quote, not the limit price. |
| `symbol_metadata.rs`, `main.rs` | Market-data poller now polls `SymbolMetadataStore::tradable_symbols()` (fresh, `status == Trading`) instead of the hardcoded 2-symbol `WATCHLIST` (removed). |
| `db.rs` `apply_position_delta` | Same-direction change (add or partial close) keeps `avg_entry_price`; only a flip resets it. |
| `paper/mod.rs`, `binance/reconcile.rs` | Fill `exchange_trade_id` = `{client_order_id}:{paper\|reconcile}:{snapshot.fills.len()}` (was random UUID) so lease-expiry retries dedupe via `ON CONFLICT`. |
| `routes_exchange.rs` | PUT response `has_credentials` reads `api_key_ciphertext IS NOT NULL` from the row. |

## Docs now stale (fix when touching them)

- `PHASE2_STATUS.md`: says "100/100 tests" (now 101); says watchlist hardcoded (now derived from tradable symbols); says growing a position leaves avg price unchanged and a shrink resets it (shrink now preserves it; add still does not weight).
- `CLAUDE.md` mentions nothing about `encrypt_fresh`; fine, but never reintroduce caller-chosen nonces in production paths.

## Known gaps / candidate next work

1. **Polling scale.** Poller does one `bookTicker` request per tradable symbol every 5s (sequential). With all Trading symbols this is likely many hundreds of requests per cycle and can exceed the 5s window or Binance rate limits. Fix: one `GET /api/v3/ticker/bookTicker` call without `symbol` (returns all), or the WS stream already scaffolded in `binance/ws.rs`. Highest-priority follow-up.
2. **Trade-ID ordinal is a stopgap.** `GET /api/v3/order` has no per-trade IDs; for live, use `myTrades` (`tradeId`) for true dedup. Also assumes at most one new fill per reconcile pass.
3. **`avg_entry_price` not weighted on adds** (unchanged from Phase 2 status); no realized-P&L calculation on partial close.
4. **Paper balance reservations (`PaperAccount`) still not wired** into order placement or the worker.
5. **`CancelRequested` is still "open"** for `has_open_activity`; it only clears once reconcile reaches a terminal state. Make sure a reconcile is always enqueued after a cancel (verify; not covered by a test).
6. **Flaky test:** `leased_command_past_expiry_becomes_claimable_again` (`crates/server/tests/phase2_exchange.rs`) failed once under full parallel run, passes alone. Timing-sensitive lease test; stabilise before relying on CI.
7. **No new tests** were added for: partial-close avg price, `has_credentials`, reconcile-retry dedup, cancel→`CancelRequested`. Add DB integration tests for these.
8. REST→WS market-data swap, live-enablement gate (auth, 7+ day testnet soak, failure injection, monitoring, written approval), and Phase 3 (LLM worker) remain unstarted. Start only on explicit request per `CLAUDE.md`.

## Gotchas for the next session

- Shell exports a global `DATABASE_URL` to `claude_cache_db`; project `.env` must win (`dotenv_override`). Never touch that DB.
- Cargo builds/tests are slow (1–6 min); run in background.
- Do not commit or push without explicit user go-ahead (project rule). `.claude/` is untracked and should stay out of commits.
