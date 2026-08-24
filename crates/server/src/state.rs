//! Shared application state injected into every handler.

use std::sync::Arc;

use sqlx::PgPool;

use crate::auth::OAuthProvider;
use crate::billing::BillingState;
use crate::config::OAuthConfig;

/// Cloneable handle to the resources every request needs.
///
/// `oauth`/`oauth_config` and `billing` are present only when configured (see
/// [`crate::config::Config`]); their endpoints return 503 when absent.
#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub oauth: Option<Arc<dyn OAuthProvider>>,
    pub oauth_config: Option<OAuthConfig>,
    pub billing: Option<BillingState>,
    /// Whether this instance accepts telemetry pings (`SOTTO_TELEMETRY_INGEST=1`, hosted only).
    pub telemetry_ingest: bool,
    /// Recovery window applied to new organisation-deletion requests.
    pub organisation_deletion_retention_days: i64,
    /// Bearer token for the protected organisation-deletion metrics exporter.
    pub organisation_deletion_metrics_token: Option<String>,
    /// Bearer token for the protected operator-observation endpoint.
    pub organisation_deletion_operator_token: Option<String>,
}
