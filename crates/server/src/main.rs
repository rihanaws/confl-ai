use sqlx::postgres::PgPoolOptions;

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

    let addr = std::env::var("LISTEN_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(%addr, "confluence-server listening");
    axum::serve(listener, confluence_server::app(pool)).await?;
    Ok(())
}
