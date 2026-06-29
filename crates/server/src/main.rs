//! The Sotto sync / API backend.
//!
//! M3 PR2: connect to Postgres, apply migrations, and serve health + GitHub OAuth login/sessions.
//! The zero-knowledge sync endpoints (snapshot, versioned writes, …) land in later PRs.

use std::sync::Arc;

use axum::{routing::get, Router};

use sotto_server::auth::{self, GithubOAuth, OAuthProvider};
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
        eprintln!("warning: GITHUB_CLIENT_ID/SECRET unset — OAuth endpoints will return 503");
    }

    let state = AppState {
        pool,
        oauth,
        oauth_config: config.oauth.clone(),
    };

    let app = Router::new()
        .route("/health", get(health))
        .merge(auth::router())
        .with_state(state);

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
