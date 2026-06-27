//! The Sotto sync / API backend.
//!
//! M3 PR1: connect to Postgres, apply migrations, and serve a health check. The zero-knowledge
//! sync endpoints (snapshot, versioned writes, …) land in later PRs.

use axum::{routing::get, Router};

use sotto_server::config::Config;
use sotto_server::db;
use sotto_server::error::{Error, Result};

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let config = Config::from_env()?;
    let pool = db::connect(&config.database_url).await?;
    db::migrate(&pool).await?;

    let app = Router::new().route("/health", get(health)).with_state(pool);

    let listener = tokio::net::TcpListener::bind(&config.bind_addr)
        .await
        .map_err(|e| Error::Io(e.to_string()))?;
    println!("sotto-server listening on http://{}", config.bind_addr);
    axum::serve(listener, app)
        .await
        .map_err(|e| Error::Io(e.to_string()))
}

async fn health() -> &'static str {
    "ok"
}
