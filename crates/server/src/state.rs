//! Shared application state injected into every handler.

use std::sync::Arc;

use sqlx::PgPool;

use crate::auth::OAuthProvider;
use crate::billing::BillingState;
use crate::config::{DeploymentMode, OAuthConfig};
use crate::contact_verification::ContactVerificationService;

/// Cloneable handle to the resources every request needs.
///
/// `oauth`/`oauth_config` and `billing` are present only when configured (see
/// [`crate::config::Config`]); their endpoints return 503 when absent.
#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub deployment_mode: DeploymentMode,
    pub oauth: Option<Arc<dyn OAuthProvider>>,
    pub oauth_config: Option<OAuthConfig>,
    pub billing: Option<BillingState>,
    /// Contact enrolment and verification transport. `None` keeps the feature unavailable.
    pub contact_verification: Option<Arc<ContactVerificationService>>,
    /// Whether bounded Cloud lifecycle email dispatch and its Resend callback are enabled.
    pub cloud_notice_dispatch_enabled: bool,
    /// Resend Svix signing secret; absent keeps provider feedback unavailable.
    pub resend_webhook_secret: Option<String>,
    /// Whether this instance accepts telemetry pings (`SOTTO_TELEMETRY_INGEST=1`, hosted only).
    pub telemetry_ingest: bool,
    /// Whether the organisation-deletion API is registered on this instance.
    ///
    /// Sourced from the same `SOTTO_ORGANISATION_DELETION_WORKER_ENABLED` opt-in that starts the
    /// lifecycle worker, so a deployment can never accept deletion requests that no worker will
    /// ever advance. [`crate::app`] leaves the routes unregistered when this is false.
    pub organisation_deletion_enabled: bool,
    /// Recovery window applied to new organisation-deletion requests.
    pub organisation_deletion_retention_days: i64,
    /// Bearer token for the protected organisation-deletion metrics exporter.
    pub organisation_deletion_metrics_token: Option<String>,
    /// Dedicated bearer credential for protected, aggregate Cloud lifecycle diagnostics.
    pub cloud_operations_metrics_token: Option<String>,
    /// Separate write credential for provider-backed Cloud coverage repair.
    pub cloud_coverage_repair_token: Option<String>,
    pub provider_refresh_ingest_enabled: bool,
    pub provider_refresh_worker_enabled: bool,
    pub provider_refresh_reconciliation_enabled: bool,
    pub provider_authority_revalidation_enabled: bool,
    pub cloud_sales_enabled: bool,
    pub founding_schedule_worker_enabled: bool,
    pub founding_price_notices_enabled: bool,
    pub billing_recovery_worker_enabled: bool,
    pub sponsored_billing_recovery_worker_enabled: bool,
    /// Bearer token for the protected operator-observation endpoint.
    pub organisation_deletion_operator_token: Option<String>,
    /// Dedicated bearer token for protected payer-transfer manual review controls.
    pub billing_transfer_operator_token: Option<String>,
    /// Whether the dormant human hosted action policy rejects ineligible requests.
    pub cloud_action_enforcement_enabled: bool,
    /// Whether Cloud eligibility requires a fresh, complete provider authority receipt.
    pub cloud_coverage_authority_enforcement_enabled: bool,
    /// Whether authority freshness participates in non-blocking shadow decisions.
    pub cloud_coverage_authority_shadow_enabled: bool,
    /// Whether hosted machine retrieval and creation apply the accountable beneficiary policy.
    pub machine_eligibility_enforcement_enabled: bool,
    /// Whether consented payer-transfer API routes are registered.
    pub billing_transfers_enabled: bool,
    /// Whether the opt-in worker may make Stripe changes for already accepted transfers.
    pub billing_transfer_execution_enabled: bool,
    /// Whether a beneficiary may create a Checkout setup session for a personal transfer.
    pub billing_transfer_setup_enabled: bool,
    /// Whether the dormant Cloud retention candidate and dry-run worker is started.
    pub cloud_retention_dry_run_enabled: bool,
    /// Whether the shared organisation retention scanner and dry-run worker is started.
    pub cloud_shared_retention_dry_run_enabled: bool,
    /// Dedicated bearer token for Cloud retention operator routes.
    pub cloud_retention_operator_token: Option<String>,
    /// Whether a separately approved, bounded Cloud retention purge worker is active.
    pub cloud_retention_purge_enabled: bool,
    /// Destructive credential, independent from protected inspection controls.
    pub cloud_retention_destructive_token: Option<String>,
    /// HMAC key used to verify the protected restore evidence attestation.
    pub cloud_retention_evidence_hmac_key: Option<String>,
    /// Release identity bound to verified restore evidence.
    pub cloud_retention_build_sha: Option<String>,
}
