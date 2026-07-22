//! Authentication: GitHub OAuth login, sessions, and request authorisation.

pub mod oauth;
pub mod routes;
pub mod session;

pub use oauth::{GithubOAuth, Identity, OAuthProvider};
pub use session::AuthUser;

use axum::routing::{get, post};
use axum::Router;

use crate::state::AppState;

/// All authentication routes, to be merged into the top-level router.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/auth/github/login", get(routes::login))
        .route("/auth/github/callback", get(routes::callback))
        .route("/auth/me", get(routes::me))
        .route("/auth/logout", post(routes::logout))
}
