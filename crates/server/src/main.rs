//! The Sotto sync / API backend.
//!
//! Boots the server: connect to Postgres, apply migrations, and serve the zero-knowledge API
//! (health, GitHub OAuth login/sessions, account + secret sync). The router itself lives in
//! [`sotto_server::app`] so the binary and the end-to-end tests share one wiring.

use std::sync::Arc;

use sotto_server::auth::{GithubOAuth, OAuthProvider};
use sotto_server::config::Config;
use sotto_server::db;
use sotto_server::error::{Error, Result};
use sotto_server::state::AppState;

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

    let oauth: Option<Arc<dyn OAuthProvider>> = config.oauth.as_ref().map(|o| {
        Arc::new(GithubOAuth::new(
            o.github_client_id.clone(),
            o.github_client_secret.clone(),
            o.callback_url(),
        )) as Arc<dyn OAuthProvider>
    });
    if oauth.is_none() {
        eprintln!("warning: GITHUB_CLIENT_ID/SECRET unset - OAuth endpoints will return 503");
    }

    let state = AppState {
        pool: pool.clone(),
        oauth,
        oauth_config: config.oauth.clone(),
        billing: config.billing.clone(),
        telemetry_ingest: config.telemetry.ingest_enabled,
    };

    // Default-on telemetry must never be a surprise: say so at boot, with the off switch.
    if config.telemetry.ping_enabled && !config.telemetry.ingest_enabled {
        println!(
            "telemetry: daily anonymous version ping is on (random instance uuid + version + \
             os/arch, nothing else) - set SOTTO_TELEMETRY=off to disable; see README §Telemetry"
        );
    }
    sotto_server::telemetry::spawn(pool, config.telemetry.clone());

    let listener = tokio::net::TcpListener::bind(&config.bind_addr)
        .await
        .map_err(|e| Error::Io(e.to_string()))?;
    println!("sotto-server listening on http://{}", config.bind_addr);
    axum::serve(listener, sotto_server::app(state))
        .await
        .map_err(|e| Error::Io(e.to_string()))
}
