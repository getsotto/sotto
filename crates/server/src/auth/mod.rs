//! Authentication: GitHub OAuth login, sessions, and request authorisation.

#[cfg(feature = "e2e-mock-oauth")]
pub mod mock_oauth;
pub mod oauth;
pub mod routes;
pub mod session;

#[cfg(feature = "e2e-mock-oauth")]
pub use mock_oauth::MockOAuth;
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
