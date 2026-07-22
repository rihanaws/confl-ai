use std::sync::{Arc, Mutex};
use std::time::Duration;

use confluence_exchange::binance::rest::{BinanceConfig, BinanceRestClient};
use confluence_exchange::paper::market_data::MarketDataProvider;
use confluence_exchange::paper::PaperAdapter;
use confluence_server::outbox_worker::{self, Supervisor};
use confluence_server::routes::AppState;
use confluence_server::symbol_metadata::SymbolMetadataStore;
use sqlx::postgres::PgPoolOptions;

/// Symbols this deployment trades in paper mode. A later iteration can pull
/// this from account risk-config symbols instead of a fixed watchlist; kept
/// static here so both the metadata and market-data refresh loops have a
/// concrete driving set without over-fetching all of Binance's symbols.
const WATCHLIST: &[&str] = &["BTCUSDT", "ETHUSDT"];

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Override: the project .env is authoritative; a DATABASE_URL inherited
    // from the shell must not silently win.
    dotenvy::dotenv_override().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let url = std::env::var("APP_DATABASE_URL")?;
    let pool = PgPoolOptions::new().max_connections(5).connect(&url).await?;

    // The outbox worker claims exchange_commands across every tenant in one
    // batch UPDATE; exchange_commands' tenant_isolation RLS policy would
    // otherwise block that (RLS has no "the worker" identity). The owner
    // role from DATABASE_URL has BYPASSRLS, so the claim/status-update half
    // of the worker uses this pool; every per-command audit write still
    // goes through `pool` (APP_DATABASE_URL) with tenant_tx.
    let owner_url = std::env::var("DATABASE_URL")?;
    let owner_pool = PgPoolOptions::new().max_connections(2).connect(&owner_url).await?;

    let symbols = Arc::new(SymbolMetadataStore::new(Duration::from_secs(120)));
    let market_data = Arc::new(Mutex::new(MarketDataProvider::new(Duration::from_secs(10))));

    // Public Binance Spot Testnet REST endpoints (exchangeInfo, bookTicker)
    // are unauthenticated, so an empty key/secret is correct here — this
    // client is never used for signed (order) calls. Chosen over the WS
    // feed in `crates/exchange::binance::ws` as the verifiable-now default;
    // that WS path exists and can replace this polling loop later without
    // touching route logic (both feed the same SymbolMetadataStore /
    // MarketDataProvider abstractions).
    let market_client = BinanceRestClient::new(BinanceConfig::testnet(String::new(), String::new()));

    // Startup fetch: block until the first hydration succeeds so the
    // server never serves requests against an empty, permanently-stale
    // metadata store.
    match market_client.fetch_exchange_info().await {
        Ok(metas) => {
            let count = metas.len();
            symbols.replace_all(metas);
            tracing::info!(count, "symbol metadata hydrated at startup");
        }
        Err(e) => tracing::error!(error = %e, "startup exchangeInfo fetch failed; symbols store stays stale/fail-closed"),
    }

    let refresh_symbols = symbols.clone();
    let refresh_client_a = BinanceRestClient::new(BinanceConfig::testnet(String::new(), String::new()));
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            match refresh_client_a.fetch_exchange_info().await {
                Ok(metas) => {
                    let count = metas.len();
                    refresh_symbols.replace_all(metas);
                    tracing::debug!(count, "symbol metadata refreshed");
                }
                Err(e) => tracing::warn!(error = %e, "symbol metadata refresh failed; store will go stale and fail closed"),
            }
        }
    });

    let refresh_market_data = market_data.clone();
    tokio::spawn(async move {
        loop {
            for symbol in WATCHLIST {
                match market_client.fetch_book_ticker(symbol).await {
                    Ok((bid, ask)) => {
                        let mut md = refresh_market_data.lock().unwrap();
                        md.update(symbol, bid, ask, std::time::Instant::now());
                    }
                    Err(e) => tracing::warn!(symbol, error = %e, "book ticker refresh failed; feed will go stale and fail closed"),
                }
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });

    // Paper trading is always available; live requires the enablement gate
    // in PHASE2_PLAN.md (auth, testnet soak, failure injection, monitoring,
    // written approval) and is not wired here.
    let paper_adapter: Arc<dyn confluence_exchange::adapter::ExchangeAdapter> =
        Arc::new(PaperAdapter::with_shared_market_data(market_data.clone()));
    let supervisor = Arc::new(Supervisor {
        paper_adapter: Some(paper_adapter),
        live_adapter: None,
    });

    let worker_owner_pool = owner_pool.clone();
    let worker_app_pool = pool.clone();
    let worker_supervisor = supervisor.clone();
    tokio::spawn(async move {
        loop {
            match outbox_worker::run_once(&worker_owner_pool, &worker_app_pool, &worker_supervisor, 20).await {
                Ok(n) if n > 0 => tracing::debug!(processed = n, "outbox worker pass"),
                Ok(_) => {}
                Err(e) => tracing::error!(error = %e, "outbox worker pass failed"),
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });

    let state = AppState { pool, symbols, market_data };
    let addr = std::env::var("LISTEN_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(%addr, "confluence-server listening");
    axum::serve(listener, confluence_server::routes::app_with_state(state)).await?;
    Ok(())
}
