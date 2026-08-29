//! Sotto sync / API server.
//!
//! The server is **zero-knowledge**: it stores ciphertext (secret names/values/data-keys, wrapped
//! vault keys) plus structural metadata, and never sees plaintext or keys.
//!
//! - [`config`] - server configuration from the environment
//! - [`db`] - Postgres connection pool + migrations
//! - [`auth`] - GitHub OAuth login, sessions, and the [`auth::AuthUser`] request extractor
//! - [`account`] - account crypto-material sync (KDF params, public key, sealed private keys, …)
//! - [`org`] - organisations, memberships, and roles (the team-RBAC substrate)
//! - [`billing`] - Stripe checkout/portal/webhook, assigning the tier entitlements enforce
//! - [`machine`] - per-environment machine tokens (CI / service access)
//! - [`sync`] - projects, environments, and the secret snapshot/batch hot path
//! - [`share`] - one-time / expiring share links (the viral funnel)
//! - [`telemetry`] - anonymous opt-out version ping (sender + hosted ingest)
//! - [`state`] - shared application state ([`state::AppState`])
//! - [`error`] - server error type

pub mod account;
pub mod audit;
pub mod auth;
pub mod billing;
pub mod config;
pub mod db;
pub mod encoding;
pub mod entitlements;
pub mod error;
pub mod machine;
pub mod org;
// The lifecycle seam, HTTP adapter, and worker remain doc-hidden: deletion is enabled per
// deployment rather than presented as a stable public API surface, and the internal seam is not
// something an embedder should call directly.
#[doc(hidden)]
pub mod org_deletion;
#[doc(hidden)]
pub mod org_deletion_api;
#[doc(hidden)]
pub mod org_deletion_worker;
// Operational deletion controls stay behind their dedicated bearer token and remain separate from
// the user-facing API. Metrics and operator observations use different secrets by design.
#[doc(hidden)]
pub mod org_deletion_metrics;
#[doc(hidden)]
pub mod org_deletion_ops;
pub mod share;
pub mod state;
pub mod sync;
pub mod telemetry;

use axum::routing::get;
use axum::Router;

use crate::state::AppState;

/// Build the full application router (health + auth + account + org + sync + share) over the shared
/// state. Shared by the binary and the end-to-end tests so they exercise the same wiring.
///
/// The destructive organisation-deletion routes are the one conditional part of this wiring. They
/// are registered only when [`AppState::organisation_deletion_enabled`] is set, which follows the
/// worker opt-in; a deployment that has not opted in serves `404` for them, exactly as it did
/// before the routes existed. Gating registration rather than checking inside each handler means a
/// route added to [`org_deletion_api`] later cannot become reachable by forgetting the check.
pub fn app(state: AppState) -> Router {
    // Built before `with_state` so the branch is on the flag, not on a per-request extraction.
    let organisation_deletion = if state.organisation_deletion_enabled {
        org_deletion_api::router()
    } else {
        Router::new()
    };
    Router::new()
        .route("/health", get(health))
        .merge(auth::router())
        .merge(audit::router())
        .merge(entitlements::router())
        .merge(account::router())
        .merge(org::router())
        .merge(billing::router())
        .merge(machine::router())
        .merge(sync::router())
        .merge(share::router())
        .merge(telemetry::router())
        .merge(org_deletion_metrics::router())
        .merge(org_deletion_ops::router())
        .merge(organisation_deletion)
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}
