//! Sotto sync / API server.
//!
//! The server is **zero-knowledge**: it stores ciphertext (secret names/values/data-keys, wrapped
//! vault keys) plus structural metadata, and never sees plaintext or keys.
//!
//! - [`config`] — server configuration from the environment
//! - [`db`] — Postgres connection pool + migrations
//! - [`auth`] — GitHub OAuth login, sessions, and the [`auth::AuthUser`] request extractor
//! - [`account`] — account crypto-material sync (KDF params, public key, sealed private keys, …)
//! - [`sync`] — projects, environments, and the secret snapshot/batch hot path
//! - [`state`] — shared application state ([`state::AppState`])
//! - [`error`] — server error type

pub mod account;
pub mod auth;
pub mod config;
pub mod db;
pub mod encoding;
pub mod error;
pub mod state;
pub mod sync;

use axum::routing::get;
use axum::Router;

use crate::state::AppState;

/// Build the full application router (health + auth + account + sync) over the shared state. Shared
/// by the binary and the end-to-end tests so they exercise the same wiring.
pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .merge(auth::router())
        .merge(account::router())
        .merge(sync::router())
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}
