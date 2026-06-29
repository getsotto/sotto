//! Shared application state injected into every handler.

use std::sync::Arc;

use sqlx::PgPool;

use crate::auth::OAuthProvider;
use crate::config::OAuthConfig;

/// Cloneable handle to the resources every request needs.
///
/// `oauth` and `oauth_config` are present only when OAuth is configured (see
/// [`crate::config::Config`]); auth endpoints return 503 when they are absent.
#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub oauth: Option<Arc<dyn OAuthProvider>>,
    pub oauth_config: Option<OAuthConfig>,
}
