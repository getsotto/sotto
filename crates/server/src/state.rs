//! Shared application state injected into every handler.

use std::sync::Arc;

use sqlx::PgPool;

use crate::auth::OAuthProvider;
use crate::config::{BillingConfig, OAuthConfig};

/// Cloneable handle to the resources every request needs.
///
/// `oauth`/`oauth_config` and `billing` are present only when configured (see
/// [`crate::config::Config`]); their endpoints return 503 when absent.
#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub oauth: Option<Arc<dyn OAuthProvider>>,
    pub oauth_config: Option<OAuthConfig>,
    pub billing: Option<BillingConfig>,
}
