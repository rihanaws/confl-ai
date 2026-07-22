//! Applies embedded sqlx migrations as the database owner.
//! Usage: `cargo run -p confluence-server --bin migrate` (reads DATABASE_URL).

use sqlx::postgres::PgPoolOptions;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Override: the project .env is authoritative; a DATABASE_URL inherited
    // from the shell (e.g. a local dev DB) must not silently win.
    dotenvy::dotenv_override().ok();
    let url = std::env::var("DATABASE_URL")?;
    let pool = PgPoolOptions::new().max_connections(1).connect(&url).await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    println!("migrations applied");
    Ok(())
}
