//! Stripe billing: subscription checkout, the customer portal, and the webhook that assigns tiers.
//!
//! Deliberately thin: entitlements ([`crate::entitlements`]) already gate everything on
//! `organizations.tier`, so this module's only real job is flipping that column in response to
//! **signature-verified** Stripe webhooks. Checkout and the portal are Stripe-hosted pages - the
//! server hands the browser a redirect URL and never touches card data.
//!
//! Ships dark: without the `STRIPE_*` environment variables every endpoint returns 503 (the OAuth
//! pattern). Zero-knowledge is unaffected - Stripe learns an org *id* and whatever the payer types
//! into Stripe's own pages; org names, membership, and vault data never leave the server.

use std::fmt;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
#[cfg(feature = "e2e-mock-billing")]
use axum::response::Html;
use axum::routing::{get, post};
use axum::{Json, Router};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Row, Transaction};

#[cfg(feature = "e2e-mock-billing")]
use url::Url;

use crate::auth::AuthUser;
use crate::billing_catalogue::{BillingOffer, BillingPriceIds};
use crate::billing_operations::{self, BeginOperation, BillingOperationState};
use crate::billing_refunds::{self, CorrectionReason, PayerKind};
use crate::billing_transfers::{self, TransferPayer, TransferState};
use crate::cloud_provider::{
    accept_provider_invalidation, record_verified_event, register_verified_allocation,
    reject_verified_event, reopen_rejected_event_for_repair, AllocationState,
    PayerKind as ProviderPayerKind, ProviderContext, ProviderEnvironment, VerifiedAllocation,
};
use crate::cloud_provider_stripe::StripeCoverageConfig;
use crate::cloud_provider_stripe::{
    decode_paid_invoice_from_authenticated_read_with_observation,
    decode_paid_invoice_with_observation, StripeAllocationBinding,
};
use crate::cloud_provider_stripe_event_repair::StripeEventRepairDisposition;
use crate::cloud_provider_stripe_http::{AuthenticatedStripeEvent, StripeReadClient};
use crate::cloud_provider_stripe_sponsored::{
    compose_sponsored_coverage, sponsored_predecessor_evidence_reference,
    SponsoredAllocationInterval, SponsoredAllocationManifest, SponsoredCoverageResult,
    SponsoredStripeCoverageConfig,
};
use crate::cloud_provider_stripe_sponsored_store::{
    end_sponsored_allocation, record_sponsored_allocation_term,
};
use crate::config::BillingConfig;
use crate::error::{Error, Result};
use crate::founding_allocator::{self, FoundingDate, FoundingOffer};
use crate::founding_schedule;
use crate::personal_billing;
use crate::sponsored_billing::{self, SponsoredSeatAction, SponsoredSeatRequest};
use crate::state::AppState;
use crate::{audit, org};

/// Reject webhook timestamps further than this from now (replay protection).
const SIGNATURE_TOLERANCE_SECS: i64 = 300;
/// The version this server *sends* on every outbound request, so response shapes cannot drift
/// underneath billing or deletion decisions.
pub const STRIPE_API_VERSION: &str = "2026-07-29.dahlia";

/// The versions this server will *accept* an inbound webhook at.
///
/// Deliberately a set rather than the constant above, because the two directions are not the same
/// problem. Outbound, we choose the version and pin it. Inbound, Stripe chooses: a webhook
/// endpoint's `api_version` is fixed when the endpoint is created and cannot be edited afterwards,
/// and an account's default only moves forwards. So an endpoint created before this code existed
/// renders events at a version this code can never be configured to ask for.
///
/// That is not hypothetical. In September 2026 the live endpoint was rendering
/// `2026-06-24.dahlia`, the account default had moved on to `2026-08-26.dahlia`, and this server
/// accepted only `2026-07-29.dahlia`: a version reachable from neither. Every live
/// `checkout.session.completed` was dropped for twelve days, and only an unrelated test failure
/// surfaced it.
///
/// The legacy handlers touch `client_reference_id`, `customer`, `subscription`, `status`,
/// `metadata.org_id` and `id`, and [`Event`] says as much: everything else is ignored. Provider
/// coverage adapters consume their own version-sensitive resource shapes and must carry separate
/// fixture or sandbox evidence before treating an allowlisted version as compatible. Add a version
/// here when Stripe moves and the legacy fields above still mean what they meant; do not infer
/// coverage compatibility from this allowlist.
pub const ACCEPTED_WEBHOOK_API_VERSIONS: &[&str] = &[
    "2026-06-24.dahlia",
    "2026-07-29.dahlia",
    "2026-08-26.dahlia",
];

/// Whether an inbound webhook's API version is one this server understands.
pub fn webhook_version_accepted(version: Option<&str>) -> bool {
    version.is_some_and(|v| ACCEPTED_WEBHOOK_API_VERSIONS.contains(&v))
}

/// Provider failures retain the machine-readable classification needed by retry and deletion
/// policy. Human-readable provider text never crosses the HTTP or persistence boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderErrorKind {
    Authentication,
    ResourceMissing,
    Retryable,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderError {
    pub status: Option<u16>,
    pub code: Option<String>,
    pub kind: ProviderErrorKind,
}

impl ProviderError {
    fn http(status: u16, code: Option<String>) -> Self {
        let kind = if matches!(status, 401 | 403) {
            ProviderErrorKind::Authentication
        } else if code.as_deref() == Some("resource_missing") {
            ProviderErrorKind::ResourceMissing
        } else if status == 429
            || status >= 500
            || matches!(code.as_deref(), Some("rate_limit_error" | "api_error"))
        {
            ProviderErrorKind::Retryable
        } else {
            ProviderErrorKind::Unknown
        };
        Self {
            status: Some(status),
            code,
            kind,
        }
    }

    fn transport() -> Self {
        Self {
            status: None,
            code: Some("transport_error".into()),
            kind: ProviderErrorKind::Retryable,
        }
    }

    fn malformed_response() -> Self {
        Self {
            status: None,
            code: Some("malformed_response".into()),
            kind: ProviderErrorKind::Unknown,
        }
    }

    fn unsupported(operation: &str) -> Self {
        Self {
            status: None,
            code: Some(format!("unsupported_{operation}")),
            kind: ProviderErrorKind::Unknown,
        }
    }

    pub(crate) fn into_error(self) -> Error {
        let detail = self.code.unwrap_or_else(|| "unknown_error".into());
        Error::Upstream(format!("stripe provider error: {detail}"))
    }
}

/// A provider 4xx response proves that the requested sponsored mutation was rejected. A 5xx,
/// transport failure or malformed success response can follow an applied mutation, so its
/// reservation must remain until the provider outcome is reconciled.
fn provider_failure_is_definitive(error: &ProviderError) -> bool {
    matches!(error.status, Some(400 | 401 | 403 | 404))
        && error.kind != ProviderErrorKind::Retryable
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let code = self.code.as_deref().unwrap_or("unknown_error");
        match self.status {
            Some(status) => write!(f, "stripe {status} {code}"),
            None => write!(f, "stripe {code}"),
        }
    }
}

pub type ProviderResult<T> = std::result::Result<T, ProviderError>;

/// Stripe subscription states are deliberately separate from entitlement states. A status that is
/// free for the product can still be resumable at Stripe and therefore must block a purge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubscriptionStatus {
    Active,
    Trialing,
    PastDue,
    Incomplete,
    Paused,
    Unpaid,
    Canceled,
    IncompleteExpired,
    Unknown(String),
}

impl SubscriptionStatus {
    pub fn parse(value: &str) -> Self {
        match value {
            "active" => Self::Active,
            "trialing" => Self::Trialing,
            "past_due" => Self::PastDue,
            "incomplete" => Self::Incomplete,
            "paused" => Self::Paused,
            "unpaid" => Self::Unpaid,
            "canceled" => Self::Canceled,
            "incomplete_expired" => Self::IncompleteExpired,
            other => Self::Unknown(other.to_string()),
        }
    }

    pub fn purge_gate(&self) -> PurgeGate {
        match self {
            Self::Canceled | Self::IncompleteExpired => PurgeGate::Terminal,
            Self::Unknown(_) => PurgeGate::Unknown,
            _ => PurgeGate::Blocking,
        }
    }

    pub(crate) fn entitlement_tier(&self) -> &'static str {
        match self {
            Self::Active | Self::Trialing | Self::PastDue => "team",
            _ => "free",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PurgeGate {
    Blocking,
    Terminal,
    Missing,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubscriptionSnapshot {
    pub id: String,
    pub status: SubscriptionStatus,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubscriptionObservation {
    Current(SubscriptionSnapshot),
    Missing,
}

/// One price-class quantity on the organisation's sponsored subscription.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SponsoredSubscriptionItem {
    pub provider_item_id: String,
    pub provider_price_id: String,
    pub quantity: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SponsoredSubscriptionItemRequest {
    pub provider_price_id: String,
    pub quantity: i64,
}

/// A bounded or open-ended future phase for the organisation's sponsored subscription.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SponsoredSubscriptionPhase {
    pub start_at_epoch: i64,
    pub end_at_epoch: Option<i64>,
    pub items: Vec<SponsoredSubscriptionItemRequest>,
}

/// Provider state needed to reconcile a sponsored subscription mutation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SponsoredSubscriptionSnapshot {
    pub customer_id: Option<String>,
    pub default_payment_method_id: Option<String>,
    pub subscription_id: String,
    pub status: SubscriptionStatus,
    pub cancel_at_period_end: bool,
    pub cancel_at: Option<i64>,
    pub current_period_start: Option<i64>,
    pub current_period_end: Option<i64>,
    pub schedule_id: Option<String>,
    pub items: Vec<SponsoredSubscriptionItem>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PersonalSubscriptionPeriod {
    pub start_epoch: i64,
    pub end_epoch: i64,
}

/// The durable identifiers returned while creating the initial hosted subscription.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SponsoredCheckoutSession {
    pub session_id: String,
    pub checkout_url: String,
    pub customer_id: Option<String>,
    pub subscription_id: Option<String>,
}

/// The provider resource created for one personal Checkout operation. The session id is kept
/// separately from its browser URL so recovery and webhooks never have to parse the URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersonalCheckoutSession {
    pub session_id: Option<String>,
    pub checkout_url: String,
}

/// Runner-owned Stripe customer details for the isolated, real-provider rehearsal build.
#[cfg(feature = "e2e-stripe-test-clock")]
#[derive(Debug, Clone, Deserialize)]
struct E2eTestClockCustomer {
    customer_id: String,
    clock_id: String,
    run_id: String,
}

/// Stripe's current status for a previously-created personal Checkout session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PersonalCheckoutStatus {
    Open,
    Complete,
    ExpiredUnpaid,
}

/// A beneficiary-authorised Checkout session for saving a payment method before a transfer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransferSetupSession {
    pub session_id: String,
    pub checkout_url: Option<String>,
    pub customer_id: String,
    pub setup_intent_id: Option<String>,
    pub status: String,
    pub expires_at_epoch: Option<i64>,
}

/// Verified SetupIntent fields needed to create a later subscription without taking payment now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransferSetupIntent {
    pub setup_intent_id: String,
    pub transfer_id: String,
    pub customer_id: String,
    pub payment_method_id: String,
    pub status: String,
    pub off_session: bool,
}

/// A future personal subscription obligation created from an authorised SetupIntent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransferSubscriptionSchedule {
    pub schedule_id: String,
    pub customer_id: String,
    pub subscription_id: Option<String>,
    pub start_at_epoch: i64,
    pub expected_period_end_epoch: i64,
    pub status: String,
}

/// Provider evidence for a refund request. A refund response is not an access decision; the
/// durable correction state decides whether the paid term remains or ends early.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefundReceipt {
    pub refund_id: String,
    pub status: String,
    pub payment_reference: String,
    pub charge_id: Option<String>,
    pub amount_pence: i64,
    pub currency: String,
}

impl SubscriptionObservation {
    pub fn purge_gate(&self) -> PurgeGate {
        match self {
            Self::Current(snapshot) => snapshot.status.purge_gate(),
            Self::Missing => PurgeGate::Missing,
        }
    }
}

/// The small interface between billing handlers and an external payment provider.
#[allow(clippy::double_must_use)]
#[async_trait]
pub trait SubscriptionProvider: Send + Sync {
    async fn create_transfer_customer(
        &self,
        _transfer_id: &str,
        _beneficiary_id: &str,
        _idempotency_key: &str,
    ) -> ProviderResult<String> {
        Err(ProviderError::unsupported("transfer_customer_create"))
    }

    async fn create_transfer_setup_session(
        &self,
        _transfer_id: &str,
        _customer_id: &str,
        _idempotency_key: &str,
        _success_url: &str,
        _cancel_url: &str,
    ) -> ProviderResult<TransferSetupSession> {
        Err(ProviderError::unsupported("transfer_setup_session_create"))
    }

    async fn get_transfer_setup_session(
        &self,
        _session_id: &str,
    ) -> ProviderResult<TransferSetupSession> {
        Err(ProviderError::unsupported("transfer_setup_session_lookup"))
    }

    async fn get_transfer_setup_intent(
        &self,
        _setup_intent_id: &str,
    ) -> ProviderResult<TransferSetupIntent> {
        Err(ProviderError::unsupported("transfer_setup_intent_lookup"))
    }

    #[allow(clippy::too_many_arguments)]
    async fn create_personal_transfer_schedule(
        &self,
        _transfer_id: &str,
        _beneficiary_id: &str,
        _customer_id: &str,
        _payment_method_id: &str,
        _offer: BillingOffer,
        _price_id: &str,
        _allocation_reference: &str,
        _start_at_epoch: i64,
        _idempotency_key: &str,
    ) -> ProviderResult<TransferSubscriptionSchedule> {
        Err(ProviderError::unsupported(
            "personal_transfer_schedule_create",
        ))
    }

    /// Preview the upcoming sponsored subscription invoice after its schedule has been updated.
    /// The returned line period is Stripe's billing boundary, including the organisation's
    /// existing anchor day.
    async fn preview_transfer_period_end(
        &self,
        _subscription_id: &str,
        _price_id: &str,
        _period_start: i64,
    ) -> ProviderResult<i64> {
        Err(ProviderError::unsupported("transfer_period_preview"))
    }

    #[allow(clippy::too_many_arguments)]
    async fn create_personal_source_restoration_subscription(
        &self,
        _transfer_id: &str,
        _beneficiary_id: &str,
        _customer_id: &str,
        _payment_method_id: &str,
        _offer: BillingOffer,
        _price_id: &str,
        _allocation_reference: &str,
        _operation_id: &str,
        _idempotency_key: &str,
    ) -> ProviderResult<SponsoredSubscriptionSnapshot> {
        Err(ProviderError::unsupported(
            "personal_source_restoration_subscription_create",
        ))
    }

    #[allow(clippy::too_many_arguments)]
    async fn find_personal_source_restoration_subscription(
        &self,
        _operation_id: &str,
        _transfer_id: &str,
        _beneficiary_id: &str,
        _customer_id: &str,
        _payment_method_id: &str,
        _price_id: &str,
        _allocation_reference: &str,
    ) -> ProviderResult<Option<SponsoredSubscriptionSnapshot>> {
        Err(ProviderError::unsupported(
            "personal_source_restoration_subscription_lookup",
        ))
    }

    async fn create_checkout(
        &self,
        org_id: &str,
        customer: Option<&str>,
        success_url: &str,
        cancel_url: &str,
    ) -> ProviderResult<String>;

    /// Create a personal checkout using a server-selected catalogue price. The default keeps
    /// existing provider adapters source-compatible; hosted Stripe overrides it with the personal
    /// metadata and selected price.
    #[allow(clippy::too_many_arguments)]
    async fn create_personal_checkout(
        &self,
        user_id: &str,
        customer: Option<&str>,
        price_id: &str,
        operation_id: &str,
        allocation_reference: &str,
        idempotency_key: &str,
        expires_at_epoch: i64,
        success_url: &str,
        cancel_url: &str,
    ) -> ProviderResult<PersonalCheckoutSession> {
        let _ = (
            price_id,
            operation_id,
            allocation_reference,
            idempotency_key,
            expires_at_epoch,
        );
        let checkout_url = self
            .create_checkout(user_id, customer, success_url, cancel_url)
            .await?;
        Ok(PersonalCheckoutSession {
            session_id: None,
            checkout_url,
        })
    }

    /// Verify and claim a runner-created test-clock Customer for one mock OAuth identity.
    #[cfg(feature = "e2e-stripe-test-clock")]
    async fn claim_e2e_test_clock_customer(
        &self,
        _customer_id: &str,
        _clock_id: &str,
        _run_id: &str,
        _user_id: &str,
    ) -> ProviderResult<()> {
        Err(ProviderError::unsupported("e2e_test_clock_customer"))
    }

    /// Read a previously-created personal Checkout session before releasing its pending account.
    /// Implementations must bind the returned resource to both identifiers.
    async fn get_personal_checkout_status(
        &self,
        _session_id: &str,
        _operation_id: &str,
    ) -> ProviderResult<PersonalCheckoutStatus> {
        Err(ProviderError::unsupported("personal_checkout_lookup"))
    }

    async fn get_sponsored_checkout_status(
        &self,
        _session_id: &str,
        _operation_id: &str,
    ) -> ProviderResult<PersonalCheckoutStatus> {
        Err(ProviderError::unsupported("sponsored_checkout_lookup"))
    }

    /// Create the initial named sponsored subscription checkout. All price classes are sent in
    /// one Checkout Session so an organisation receives one subscription and one invoice.
    #[allow(clippy::too_many_arguments)]
    async fn create_sponsored_checkout(
        &self,
        _organization_id: &str,
        _customer: Option<&str>,
        _items: &[SponsoredSubscriptionItemRequest],
        _operation_id: &str,
        _idempotency_key: &str,
        _expires_at_epoch: i64,
        _success_url: &str,
        _cancel_url: &str,
    ) -> ProviderResult<SponsoredCheckoutSession> {
        Err(ProviderError::unsupported("sponsored_checkout"))
    }

    /// Apply the desired named-seat quantities at the next billing boundary. The provider
    /// implementation uses an explicit subscription schedule so local effective dates and
    /// Stripe's future invoice agree.
    async fn update_sponsored_subscription(
        &self,
        _organization_id: &str,
        _subscription_id: &str,
        _phases: &[SponsoredSubscriptionPhase],
        _idempotency_key: &str,
    ) -> ProviderResult<SponsoredSubscriptionSnapshot> {
        Err(ProviderError::unsupported("sponsored_subscription_update"))
    }

    /// Confirm whether an earlier sponsored schedule mutation is visible at the provider. A
    /// positive result is independently read back; `None` means it is not proven applied.
    async fn verify_sponsored_subscription_update(
        &self,
        _subscription_id: &str,
        _expected_period_start: i64,
        _expected_period_end: i64,
        _phases: &[SponsoredSubscriptionPhase],
    ) -> ProviderResult<Option<SponsoredSubscriptionSnapshot>> {
        Ok(None)
    }

    /// Schedule a personal subscription's founding-to-standard transition without adding
    /// organisation metadata to its Stripe subscription.
    async fn update_personal_subscription(
        &self,
        _subscription_id: &str,
        _phases: &[SponsoredSubscriptionPhase],
        _idempotency_key: &str,
    ) -> ProviderResult<SponsoredSubscriptionSnapshot> {
        Err(ProviderError::unsupported("personal_subscription_schedule"))
    }

    async fn get_sponsored_subscription(
        &self,
        _subscription_id: &str,
    ) -> ProviderResult<SponsoredSubscriptionSnapshot> {
        Err(ProviderError::unsupported("sponsored_subscription_lookup"))
    }

    async fn cancel_sponsored_subscription(
        &self,
        _organization_id: &str,
        _subscription_id: &str,
        _idempotency_key: &str,
    ) -> ProviderResult<SponsoredSubscriptionSnapshot> {
        Err(ProviderError::unsupported(
            "sponsored_subscription_cancellation",
        ))
    }

    /// Validate the configured Stripe price before quoting or creating a personal checkout.
    /// Providers that cannot authenticate their price catalogue keep this route disabled.
    async fn validate_personal_price(
        &self,
        _price_id: &str,
        _offer: BillingOffer,
    ) -> ProviderResult<()> {
        Err(ProviderError::unsupported("personal_price_validation"))
    }

    async fn validate_cloud_portal_configuration(
        &self,
        configuration_id: &str,
    ) -> ProviderResult<()> {
        let _ = configuration_id;
        Ok(())
    }

    async fn create_portal(
        &self,
        customer: &str,
        return_url: &str,
        configuration_id: Option<&str>,
    ) -> ProviderResult<String>;

    /// Create a refund against a verified payment reference. The caller owns the durable
    /// correction transaction and must record the receipt before changing any entitlement.
    async fn create_refund(
        &self,
        payment_reference: &str,
        amount_pence: Option<i64>,
        idempotency_key: &str,
    ) -> ProviderResult<RefundReceipt> {
        let _ = (payment_reference, amount_pence, idempotency_key);
        Err(ProviderError::unsupported("refund"))
    }

    /// Read a previously created refund. A stored provider refund ID must be reconciled with a
    /// read before another create call is attempted.
    async fn get_refund(&self, _provider_refund_id: &str) -> ProviderResult<RefundReceipt> {
        Err(ProviderError::unsupported("refund_lookup"))
    }

    /// Read the amount available immediately before a successful refund. Stripe's charge
    /// aggregate includes the successful refund being checked, so subtract its amount first.
    async fn get_refundable_amount(
        &self,
        _payment_reference: &str,
        _charge_id: &str,
        _refund_amount_pence: i64,
    ) -> ProviderResult<(i64, String)> {
        Err(ProviderError::unsupported("payment_lookup"))
    }

    async fn get_subscription(
        &self,
        subscription_id: &str,
    ) -> ProviderResult<SubscriptionObservation>;

    /// Read the provider's current period end for a newly paid personal subscription.
    async fn personal_subscription_period(
        &self,
        _subscription_id: &str,
    ) -> ProviderResult<Option<PersonalSubscriptionPeriod>> {
        Ok(None)
    }

    async fn cancel_subscription(
        &self,
        subscription_id: &str,
        idempotency_key: &str,
        org_id: &str,
    ) -> ProviderResult<SubscriptionObservation>;

    /// Request cancellation at the end of the paid term. Existing test adapters inherit the
    /// legacy method until they opt into the personal lifecycle explicitly.
    async fn cancel_personal_subscription(
        &self,
        subscription_id: &str,
        idempotency_key: &str,
        user_id: &str,
    ) -> ProviderResult<SubscriptionObservation> {
        self.cancel_subscription(subscription_id, idempotency_key, user_id)
            .await
    }

    /// Cancel a transfer source only at the paid-through boundary accepted in the quote. The
    /// default adapter verifies the current period first; provider implementations should also
    /// bind the write itself to the explicit timestamp where the API supports it.
    async fn cancel_personal_subscription_at_boundary(
        &self,
        subscription_id: &str,
        boundary_epoch: i64,
        idempotency_key: &str,
        user_id: &str,
    ) -> ProviderResult<SponsoredSubscriptionSnapshot> {
        let current = self.get_sponsored_subscription(subscription_id).await?;
        if current.current_period_end != Some(boundary_epoch) {
            return Err(ProviderError::malformed_response());
        }
        match self
            .cancel_personal_subscription(subscription_id, idempotency_key, user_id)
            .await?
        {
            SubscriptionObservation::Current(_) => {
                let updated = self.get_sponsored_subscription(subscription_id).await?;
                if updated.current_period_end != Some(boundary_epoch)
                    || (!updated.cancel_at_period_end
                        && updated.cancel_at != Some(boundary_epoch)
                        && updated.status != SubscriptionStatus::Canceled)
                {
                    return Err(ProviderError::malformed_response());
                }
                Ok(updated)
            }
            SubscriptionObservation::Missing => Err(ProviderError::malformed_response()),
        }
    }

    /// Immediately terminate a personal subscription after an approved full refund. The
    /// correction workflow keeps its request in `provider_pending` until this step and the
    /// refund both succeed, so a retry can safely repeat the same provider idempotency key.
    async fn terminate_personal_subscription(
        &self,
        _subscription_id: &str,
        _idempotency_key: &str,
        _user_id: &str,
    ) -> ProviderResult<SubscriptionObservation> {
        Err(ProviderError::unsupported(
            "personal_subscription_termination",
        ))
    }
}

/// Compatibility name for callers that still refer to the pre-provider billing trait.
pub use SubscriptionProvider as BillingProvider;

/// Billing resources shared by handlers. The provider is swappable for the browser E2E build,
/// while webhook verification keeps its own secret regardless of which checkout adapter runs.
#[derive(Clone)]
pub struct BillingState {
    provider: Arc<dyn SubscriptionProvider>,
    webhook_secret: String,
    return_url: String,
    price_catalogue: Option<BillingPriceIds>,
    cloud_sales_enabled: bool,
    founding_schedule_worker_enabled: bool,
    cloud_portal_configuration_id: Option<String>,
    personal_coverage: Option<PersonalCoverageRuntime>,
}

#[derive(Clone)]
struct PersonalCoverageRuntime {
    config: StripeCoverageConfig,
    client: StripeReadClient,
    ingest_enabled: bool,
}

impl BillingState {
    /// Share the provider with the deletion worker without exposing billing credentials.
    pub fn provider(&self) -> Arc<dyn SubscriptionProvider> {
        Arc::clone(&self.provider)
    }

    pub fn price_catalogue(&self) -> Option<&BillingPriceIds> {
        self.price_catalogue.as_ref()
    }

    pub fn cloud_sales_enabled(&self) -> bool {
        self.cloud_sales_enabled
    }

    pub fn return_url(&self) -> &str {
        &self.return_url
    }

    pub fn founding_schedule_worker_enabled(&self) -> bool {
        self.founding_schedule_worker_enabled
    }

    pub fn cloud_portal_configuration_id(&self) -> Option<&str> {
        self.cloud_portal_configuration_id.as_deref()
    }

    pub async fn validate_cloud_portal_configuration(&self) -> ProviderResult<()> {
        let configuration_id = self
            .cloud_portal_configuration_id()
            .ok_or_else(ProviderError::malformed_response)?;
        self.provider
            .validate_cloud_portal_configuration(configuration_id)
            .await
    }

    pub fn with_founding_schedule_worker_enabled(mut self, enabled: bool) -> Self {
        self.founding_schedule_worker_enabled = enabled;
        self
    }

    pub fn from_config(config: BillingConfig) -> Self {
        let provider = StripeBilling {
            api_key: config.api_key.clone(),
            price_id: config.price_id,
            api_origin: "https://api.stripe.com".into(),
        };
        Self {
            provider: Arc::new(provider),
            webhook_secret: config.webhook_secret,
            return_url: config.return_url,
            price_catalogue: config.price_catalogue,
            cloud_portal_configuration_id: config.cloud_portal_configuration_id,
            cloud_sales_enabled: config.cloud_sales_enabled,
            founding_schedule_worker_enabled: false,
            personal_coverage: None,
        }
    }

    pub fn with_personal_coverage(
        mut self,
        config: StripeCoverageConfig,
        client: StripeReadClient,
        ingest_enabled: bool,
    ) -> Self {
        self.personal_coverage = Some(PersonalCoverageRuntime {
            config,
            client,
            ingest_enabled,
        });
        self
    }

    pub fn with_price_catalogue(mut self, catalogue: BillingPriceIds) -> Self {
        self.price_catalogue = Some(catalogue);
        self
    }

    pub(crate) fn personal_coverage(&self) -> Option<(&StripeCoverageConfig, &StripeReadClient)> {
        self.personal_coverage
            .as_ref()
            .map(|runtime| (&runtime.config, &runtime.client))
    }

    pub(crate) fn personal_coverage_ingest_enabled(&self) -> bool {
        self.personal_coverage
            .as_ref()
            .is_some_and(|runtime| runtime.ingest_enabled)
    }

    pub fn with_provider(
        provider: Arc<dyn SubscriptionProvider>,
        webhook_secret: String,
        return_url: String,
    ) -> Self {
        Self::with_provider_and_cloud_sales(provider, webhook_secret, return_url, false)
    }

    /// Construct billing with an injected provider and an explicit purchase decision.
    pub fn with_provider_and_cloud_sales(
        provider: Arc<dyn SubscriptionProvider>,
        webhook_secret: String,
        return_url: String,
        cloud_sales_enabled: bool,
    ) -> Self {
        Self {
            provider,
            webhook_secret,
            return_url,
            price_catalogue: None,
            cloud_sales_enabled,
            founding_schedule_worker_enabled: true,
            cloud_portal_configuration_id: cloud_sales_enabled.then(|| "bpc_test".into()),
            personal_coverage: None,
        }
    }

    #[cfg(feature = "e2e-mock-billing")]
    pub fn with_e2e_provider(config: BillingConfig, provider_origin: String) -> Self {
        Self {
            provider: Arc::new(E2eBilling { provider_origin }),
            webhook_secret: config.webhook_secret,
            return_url: config.return_url,
            price_catalogue: config.price_catalogue,
            cloud_sales_enabled: config.cloud_sales_enabled,
            founding_schedule_worker_enabled: false,
            cloud_portal_configuration_id: config.cloud_portal_configuration_id,
            personal_coverage: None,
        }
    }
}

struct StripeBilling {
    api_key: String,
    price_id: String,
    api_origin: String,
}

#[async_trait]
impl SubscriptionProvider for StripeBilling {
    #[cfg(feature = "e2e-stripe-test-clock")]
    async fn claim_e2e_test_clock_customer(
        &self,
        customer_id: &str,
        clock_id: &str,
        run_id: &str,
        user_id: &str,
    ) -> ProviderResult<()> {
        if !self.api_key.starts_with("rk_test_")
            || !valid_stripe_id(customer_id, "cus_")
            || !valid_stripe_id(clock_id, "clock_")
            || !valid_run_id(run_id)
            || !valid_e2e_customer_user_id(user_id)
        {
            return Err(ProviderError::malformed_response());
        }

        let customer = stripe_get_at(
            &self.api_key,
            &self.api_origin,
            &format!("customers/{customer_id}"),
        )
        .await?;
        if !e2e_test_clock_customer_matches(&customer, customer_id, clock_id, run_id, None) {
            return Err(ProviderError::malformed_response());
        }
        let claimed_user = customer["metadata"]["sotto_lifecycle_user_id"].as_str();
        if claimed_user.is_some_and(|claimed_user| claimed_user != user_id) {
            return Err(ProviderError::malformed_response());
        }

        if claimed_user.is_none() {
            let subscriptions = stripe_get_at(
                &self.api_key,
                &self.api_origin,
                &format!("subscriptions?customer={customer_id}&status=all&limit=1"),
            )
            .await?;
            if subscriptions["data"]
                .as_array()
                .is_none_or(|rows| !rows.is_empty())
            {
                return Err(ProviderError::malformed_response());
            }
            let updated = stripe_post_with_idempotency_at(
                &self.api_key,
                &self.api_origin,
                &format!("customers/{customer_id}"),
                &[(
                    "metadata[sotto_lifecycle_user_id]".into(),
                    user_id.to_owned(),
                )],
                &format!("sotto-lifecycle:{run_id}:claim:{customer_id}"),
            )
            .await?;
            if !e2e_test_clock_customer_matches(
                &updated,
                customer_id,
                clock_id,
                run_id,
                Some(user_id),
            ) {
                return Err(ProviderError::malformed_response());
            }
        }
        Ok(())
    }

    async fn create_transfer_customer(
        &self,
        transfer_id: &str,
        beneficiary_id: &str,
        idempotency_key: &str,
    ) -> ProviderResult<String> {
        if transfer_id.trim().is_empty() || beneficiary_id.trim().is_empty() {
            return Err(ProviderError::malformed_response());
        }
        let form = vec![
            ("metadata[sotto_transfer_id]".into(), transfer_id.into()),
            (
                "metadata[sotto_beneficiary_id]".into(),
                beneficiary_id.into(),
            ),
        ];
        let customer = stripe_post_with_idempotency_at(
            &self.api_key,
            &self.api_origin,
            "customers",
            &form,
            idempotency_key,
        )
        .await?;
        customer["id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .ok_or_else(ProviderError::malformed_response)
    }

    async fn create_transfer_setup_session(
        &self,
        transfer_id: &str,
        customer_id: &str,
        idempotency_key: &str,
        success_url: &str,
        cancel_url: &str,
    ) -> ProviderResult<TransferSetupSession> {
        if transfer_id.trim().is_empty() || customer_id.trim().is_empty() {
            return Err(ProviderError::malformed_response());
        }
        let form = transfer_setup_checkout_form(transfer_id, customer_id, success_url, cancel_url);
        let value = stripe_post_with_idempotency_at(
            &self.api_key,
            &self.api_origin,
            "checkout/sessions",
            &form,
            idempotency_key,
        )
        .await?;
        let session = transfer_setup_session_from_object(&value, None, Some(transfer_id))?;
        if session.status != "open" || session.checkout_url.is_none() {
            return Err(ProviderError::malformed_response());
        }
        Ok(session)
    }

    async fn get_transfer_setup_session(
        &self,
        session_id: &str,
    ) -> ProviderResult<TransferSetupSession> {
        let value = stripe_get_at(
            &self.api_key,
            &self.api_origin,
            &format!("checkout/sessions/{session_id}"),
        )
        .await?;
        transfer_setup_session_from_object(&value, Some(session_id), None)
    }

    async fn get_transfer_setup_intent(
        &self,
        setup_intent_id: &str,
    ) -> ProviderResult<TransferSetupIntent> {
        if setup_intent_id.trim().is_empty() {
            return Err(ProviderError::malformed_response());
        }
        let value = stripe_get_at(
            &self.api_key,
            &self.api_origin,
            &format!("setup_intents/{setup_intent_id}"),
        )
        .await?;
        transfer_setup_intent_from_object(&value, setup_intent_id)
    }

    #[allow(clippy::too_many_arguments)]
    async fn create_personal_transfer_schedule(
        &self,
        transfer_id: &str,
        beneficiary_id: &str,
        customer_id: &str,
        payment_method_id: &str,
        offer: BillingOffer,
        price_id: &str,
        allocation_reference: &str,
        start_at_epoch: i64,
        idempotency_key: &str,
    ) -> ProviderResult<TransferSubscriptionSchedule> {
        if transfer_id.trim().is_empty()
            || beneficiary_id.trim().is_empty()
            || customer_id.trim().is_empty()
            || payment_method_id.trim().is_empty()
            || price_id.trim().is_empty()
            || allocation_reference.trim().is_empty()
            || start_at_epoch <= 0
        {
            return Err(ProviderError::malformed_response());
        }
        let form = personal_transfer_schedule_form(
            transfer_id,
            beneficiary_id,
            customer_id,
            payment_method_id,
            offer,
            price_id,
            allocation_reference,
            start_at_epoch,
        );
        let created = stripe_post_with_idempotency_at(
            &self.api_key,
            &self.api_origin,
            "subscription_schedules",
            &form,
            idempotency_key,
        )
        .await?;
        let schedule_id = created["id"]
            .as_str()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(ProviderError::malformed_response)?;
        let observed = stripe_get_at(
            &self.api_key,
            &self.api_origin,
            &format!("subscription_schedules/{schedule_id}"),
        )
        .await?;
        transfer_schedule_from_object(
            &observed,
            transfer_id,
            customer_id,
            payment_method_id,
            beneficiary_id,
            offer,
            price_id,
            allocation_reference,
            start_at_epoch,
        )
    }

    async fn preview_transfer_period_end(
        &self,
        subscription_id: &str,
        price_id: &str,
        period_start: i64,
    ) -> ProviderResult<i64> {
        if subscription_id.trim().is_empty() || price_id.trim().is_empty() || period_start <= 0 {
            return Err(ProviderError::malformed_response());
        }
        let invoice = stripe_post_at(
            &self.api_key,
            &self.api_origin,
            "invoices/create_preview",
            &[("subscription".into(), subscription_id.into())],
        )
        .await?;
        preview_subscription_period_end_from_invoice(
            &invoice,
            subscription_id,
            price_id,
            period_start,
        )
    }

    #[allow(clippy::too_many_arguments)]
    async fn create_personal_source_restoration_subscription(
        &self,
        transfer_id: &str,
        beneficiary_id: &str,
        customer_id: &str,
        payment_method_id: &str,
        _offer: BillingOffer,
        price_id: &str,
        allocation_reference: &str,
        operation_id: &str,
        idempotency_key: &str,
    ) -> ProviderResult<SponsoredSubscriptionSnapshot> {
        if transfer_id.trim().is_empty()
            || beneficiary_id.trim().is_empty()
            || customer_id.trim().is_empty()
            || payment_method_id.trim().is_empty()
            || price_id.trim().is_empty()
            || allocation_reference.trim().is_empty()
            || operation_id.trim().is_empty()
        {
            return Err(ProviderError::malformed_response());
        }
        let form = personal_source_restoration_form(
            transfer_id,
            beneficiary_id,
            customer_id,
            payment_method_id,
            price_id,
            allocation_reference,
            operation_id,
        );
        let created = stripe_post_with_idempotency_at(
            &self.api_key,
            &self.api_origin,
            "subscriptions",
            &form,
            idempotency_key,
        )
        .await?;
        let subscription_id = created["id"]
            .as_str()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(ProviderError::malformed_response)?;
        let observed = stripe_get_at(
            &self.api_key,
            &self.api_origin,
            &format!("subscriptions/{subscription_id}"),
        )
        .await?;
        let snapshot = sponsored_subscription_snapshot(&observed, subscription_id)?;
        if snapshot.customer_id.as_deref() != Some(customer_id)
            || snapshot.default_payment_method_id.as_deref() != Some(payment_method_id)
            || snapshot.items.len() != 1
            || snapshot.items[0].provider_price_id != price_id
            || snapshot.items[0].quantity != 1
            || observed["metadata"]["personal_user_id"].as_str() != Some(beneficiary_id)
            || observed["metadata"]["operation_id"].as_str() != Some(operation_id)
            || observed["metadata"]["sotto_allocation_reference"].as_str()
                != Some(allocation_reference)
            || observed["metadata"]["sotto_transfer_id"].as_str() != Some(transfer_id)
        {
            return Err(ProviderError::malformed_response());
        }
        Ok(snapshot)
    }

    #[allow(clippy::too_many_arguments)]
    async fn find_personal_source_restoration_subscription(
        &self,
        operation_id: &str,
        transfer_id: &str,
        beneficiary_id: &str,
        customer_id: &str,
        payment_method_id: &str,
        price_id: &str,
        allocation_reference: &str,
    ) -> ProviderResult<Option<SponsoredSubscriptionSnapshot>> {
        if operation_id.trim().is_empty()
            || transfer_id.trim().is_empty()
            || beneficiary_id.trim().is_empty()
            || customer_id.trim().is_empty()
            || payment_method_id.trim().is_empty()
            || price_id.trim().is_empty()
            || allocation_reference.trim().is_empty()
        {
            return Err(ProviderError::malformed_response());
        }
        let query = format!("metadata['operation_id']:'{operation_id}'");
        let response = stripe_client()
            .get(format!(
                "{}/v1/subscriptions/search",
                self.api_origin.trim_end_matches('/')
            ))
            .bearer_auth(&self.api_key)
            .headers(stripe_headers(None)?)
            .query(&[("query", query.as_str()), ("limit", "2")])
            .send()
            .await
            .map_err(|_| ProviderError::transport())?;
        let value = stripe_response(response).await?;
        let rows = value["data"]
            .as_array()
            .filter(|rows| rows.len() <= 1)
            .ok_or_else(ProviderError::malformed_response)?;
        let Some(object) = rows.first() else {
            return Ok(None);
        };
        let subscription_id = object["id"]
            .as_str()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(ProviderError::malformed_response)?;
        let snapshot = sponsored_subscription_snapshot(object, subscription_id)?;
        if snapshot.customer_id.as_deref() != Some(customer_id)
            || snapshot.default_payment_method_id.as_deref() != Some(payment_method_id)
            || snapshot.items.len() != 1
            || snapshot.items[0].provider_price_id != price_id
            || snapshot.items[0].quantity != 1
            || object["metadata"]["personal_user_id"].as_str() != Some(beneficiary_id)
            || object["metadata"]["operation_id"].as_str() != Some(operation_id)
            || object["metadata"]["sotto_allocation_reference"].as_str()
                != Some(allocation_reference)
            || object["metadata"]["sotto_transfer_id"].as_str() != Some(transfer_id)
        {
            return Err(ProviderError::malformed_response());
        }
        Ok(Some(snapshot))
    }

    async fn create_checkout(
        &self,
        org_id: &str,
        customer: Option<&str>,
        success_url: &str,
        cancel_url: &str,
    ) -> ProviderResult<String> {
        let mut form = vec![
            ("mode".to_string(), "subscription".to_string()),
            ("line_items[0][price]".to_string(), self.price_id.clone()),
            ("line_items[0][quantity]".to_string(), "1".to_string()),
            ("client_reference_id".to_string(), org_id.to_string()),
            // Mirrored onto the subscription so its lifecycle webhooks name the org even if they
            // arrive before (or without) the checkout-completed event.
            (
                "subscription_data[metadata][org_id]".to_string(),
                org_id.to_string(),
            ),
            ("success_url".to_string(), success_url.to_string()),
            ("cancel_url".to_string(), cancel_url.to_string()),
        ];
        if let Some(customer) = customer {
            form.push(("customer".to_string(), customer.to_string()));
        }

        let session = stripe_post(&self.api_key, "checkout/sessions", &form).await?;
        session["url"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(ProviderError::malformed_response)
    }

    #[allow(clippy::too_many_arguments)]
    async fn create_personal_checkout(
        &self,
        user_id: &str,
        customer: Option<&str>,
        price_id: &str,
        operation_id: &str,
        allocation_reference: &str,
        idempotency_key: &str,
        expires_at_epoch: i64,
        success_url: &str,
        cancel_url: &str,
    ) -> ProviderResult<PersonalCheckoutSession> {
        if price_id.is_empty() {
            return Err(ProviderError::malformed_response());
        }
        let mut form = vec![
            ("mode".to_string(), "subscription".to_string()),
            ("line_items[0][price]".to_string(), price_id.to_string()),
            ("line_items[0][quantity]".to_string(), "1".to_string()),
            (
                "client_reference_id".to_string(),
                format!("personal:{operation_id}"),
            ),
            (
                "subscription_data[metadata][personal_user_id]".to_string(),
                user_id.to_string(),
            ),
            (
                "subscription_data[metadata][operation_id]".to_string(),
                operation_id.to_string(),
            ),
            (
                "subscription_data[metadata][sotto_allocation_reference]".to_string(),
                allocation_reference.to_string(),
            ),
            ("success_url".to_string(), success_url.to_string()),
            ("cancel_url".to_string(), cancel_url.to_string()),
            ("expires_at".to_string(), expires_at_epoch.to_string()),
        ];
        if let Some(customer) = customer {
            form.push(("customer".to_string(), customer.to_string()));
        }
        let session = stripe_post_with_idempotency(
            &self.api_key,
            "checkout/sessions",
            &form,
            idempotency_key,
        )
        .await?;
        let session_id = session["id"]
            .as_str()
            .filter(|value| !value.is_empty())
            .ok_or_else(ProviderError::malformed_response)?;
        let checkout_url = session["url"]
            .as_str()
            .filter(|value| !value.is_empty())
            .ok_or_else(ProviderError::malformed_response)?;
        Ok(PersonalCheckoutSession {
            session_id: Some(session_id.to_string()),
            checkout_url: checkout_url.to_string(),
        })
    }

    async fn get_personal_checkout_status(
        &self,
        session_id: &str,
        operation_id: &str,
    ) -> ProviderResult<PersonalCheckoutStatus> {
        if session_id.trim().is_empty() || operation_id.trim().is_empty() {
            return Err(ProviderError::malformed_response());
        }
        let value = stripe_get_at(
            &self.api_key,
            &self.api_origin,
            &format!("checkout/sessions/{session_id}"),
        )
        .await?;
        personal_checkout_status_from_object(&value, session_id, operation_id)
    }

    async fn get_sponsored_checkout_status(
        &self,
        session_id: &str,
        operation_id: &str,
    ) -> ProviderResult<PersonalCheckoutStatus> {
        if session_id.trim().is_empty() || operation_id.trim().is_empty() {
            return Err(ProviderError::malformed_response());
        }
        let value = stripe_get_at(
            &self.api_key,
            &self.api_origin,
            &format!("checkout/sessions/{session_id}"),
        )
        .await?;
        checkout_status_from_object(&value, session_id, &format!("sponsored:{operation_id}"))
    }

    #[allow(clippy::too_many_arguments)]
    async fn create_sponsored_checkout(
        &self,
        organization_id: &str,
        customer: Option<&str>,
        items: &[SponsoredSubscriptionItemRequest],
        operation_id: &str,
        idempotency_key: &str,
        expires_at_epoch: i64,
        success_url: &str,
        cancel_url: &str,
    ) -> ProviderResult<SponsoredCheckoutSession> {
        if items.is_empty()
            || items
                .iter()
                .any(|item| item.provider_price_id.is_empty() || item.quantity <= 0)
        {
            return Err(ProviderError::malformed_response());
        }
        let mut form = vec![
            ("mode".to_string(), "subscription".to_string()),
            (
                ("client_reference_id").to_string(),
                format!("sponsored:{operation_id}"),
            ),
            (
                "subscription_data[metadata][sponsored_operation_id]".to_string(),
                operation_id.to_string(),
            ),
            (
                "subscription_data[metadata][organization_id]".to_string(),
                organization_id.to_string(),
            ),
            ("success_url".to_string(), success_url.to_string()),
            ("cancel_url".to_string(), cancel_url.to_string()),
            ("expires_at".to_string(), expires_at_epoch.to_string()),
        ];
        for (index, item) in items.iter().enumerate() {
            form.push((
                format!("line_items[{index}][price]"),
                item.provider_price_id.clone(),
            ));
            form.push((
                format!("line_items[{index}][quantity]"),
                item.quantity.to_string(),
            ));
        }
        if let Some(customer) = customer {
            form.push(("customer".to_string(), customer.to_string()));
        }
        let session = stripe_post_with_idempotency_at(
            &self.api_key,
            &self.api_origin,
            "checkout/sessions",
            &form,
            idempotency_key,
        )
        .await?;
        let session_id = session["id"]
            .as_str()
            .filter(|value| !value.is_empty())
            .ok_or_else(ProviderError::malformed_response)?;
        let checkout_url = session["url"]
            .as_str()
            .filter(|value| !value.is_empty())
            .ok_or_else(ProviderError::malformed_response)?;
        Ok(SponsoredCheckoutSession {
            session_id: session_id.to_string(),
            checkout_url: checkout_url.to_string(),
            customer_id: session["customer"].as_str().map(str::to_string),
            subscription_id: session["subscription"].as_str().map(str::to_string),
        })
    }

    async fn update_sponsored_subscription(
        &self,
        organization_id: &str,
        subscription_id: &str,
        phases: &[SponsoredSubscriptionPhase],
        idempotency_key: &str,
    ) -> ProviderResult<SponsoredSubscriptionSnapshot> {
        if subscription_id.is_empty()
            || phases.is_empty()
            || phases.iter().any(|phase| {
                phase.start_at_epoch < 0
                    || phase
                        .end_at_epoch
                        .is_some_and(|end| end <= phase.start_at_epoch)
                    || phase
                        .items
                        .iter()
                        .any(|item| item.provider_price_id.is_empty() || item.quantity < 0)
            })
            || phases.windows(2).any(|window| {
                window[0]
                    .end_at_epoch
                    .map(|end| window[1].start_at_epoch < end)
                    .unwrap_or(true)
            })
        {
            return Err(ProviderError::malformed_response());
        }
        let current = self.get_sponsored_subscription(subscription_id).await?;
        let schedule_id = if let Some(schedule_id) = current.schedule_id.clone() {
            schedule_id
        } else {
            let schedule = stripe_post_with_idempotency(
                &self.api_key,
                "subscription_schedules",
                &[("from_subscription".into(), subscription_id.into())],
                &format!("{idempotency_key}-schedule"),
            )
            .await?;
            schedule["id"]
                .as_str()
                .filter(|value| !value.is_empty())
                .ok_or_else(ProviderError::malformed_response)?
                .to_string()
        };
        let current_start = current
            .current_period_start
            .ok_or_else(ProviderError::malformed_response)?;
        let current_end = current
            .current_period_end
            .ok_or_else(ProviderError::malformed_response)?;
        let phase_at_end = phases
            .iter()
            .find(|phase| {
                phase.start_at_epoch <= current_end
                    && phase.end_at_epoch.is_none_or(|end| current_end < end)
            })
            .or_else(|| {
                phases
                    .iter()
                    .find(|phase| phase.start_at_epoch == current_end)
            })
            .ok_or_else(ProviderError::malformed_response)?;
        let mut normalized = Vec::with_capacity(phases.len());
        normalized.push(SponsoredSubscriptionPhase {
            start_at_epoch: current_end,
            end_at_epoch: phase_at_end.end_at_epoch,
            items: phase_at_end.items.clone(),
        });
        normalized.extend(
            phases
                .iter()
                .filter(|phase| phase.start_at_epoch > current_end)
                .cloned(),
        );
        if normalized.windows(2).any(|window| {
            window[0]
                .end_at_epoch
                .is_some_and(|end| end != window[1].start_at_epoch)
        }) {
            return Err(ProviderError::malformed_response());
        }
        let final_empty = normalized
            .last()
            .is_some_and(|phase| phase.items.is_empty());
        if normalized
            .iter()
            .take(if final_empty {
                normalized.len().saturating_sub(1)
            } else {
                normalized.len()
            })
            .any(|phase| phase.items.is_empty())
        {
            return Err(ProviderError::malformed_response());
        }
        if final_empty {
            normalized.pop();
        }
        let mut form = vec![
            (
                "end_behavior".into(),
                if final_empty { "cancel" } else { "release" }.into(),
            ),
            ("proration_behavior".into(), "none".into()),
            ("metadata[organization_id]".into(), organization_id.into()),
            ("phases[0][start_date]".into(), current_start.to_string()),
            ("phases[0][end_date]".into(), current_end.to_string()),
        ];
        for (index, item) in current.items.iter().enumerate() {
            form.push((
                format!("phases[0][items][{index}][price]"),
                item.provider_price_id.clone(),
            ));
            form.push((
                format!("phases[0][items][{index}][quantity]"),
                item.quantity.to_string(),
            ));
        }
        for (phase_index, phase) in normalized.iter().enumerate() {
            let index = phase_index + 1;
            form.push((
                format!("phases[{index}][start_date]"),
                phase.start_at_epoch.to_string(),
            ));
            if let Some(end) = phase.end_at_epoch {
                form.push((format!("phases[{index}][end_date]"), end.to_string()));
            }
            for (item_index, item) in phase.items.iter().enumerate() {
                form.push((
                    format!("phases[{index}][items][{item_index}][price]"),
                    item.provider_price_id.clone(),
                ));
                form.push((
                    format!("phases[{index}][items][{item_index}][quantity]"),
                    item.quantity.to_string(),
                ));
            }
        }
        stripe_post_with_idempotency(
            &self.api_key,
            &format!("subscription_schedules/{schedule_id}"),
            &form,
            &schedule_update_idempotency_key(idempotency_key, &normalized),
        )
        .await?;
        let schedule = stripe_get_at(
            &self.api_key,
            &self.api_origin,
            &format!("subscription_schedules/{schedule_id}"),
        )
        .await?;
        if !stripe_schedule_contains_phases(
            &schedule,
            current_start,
            current_end,
            &current.items,
            &normalized,
        ) {
            return Err(ProviderError::malformed_response());
        }
        self.get_sponsored_subscription(subscription_id).await
    }

    async fn verify_sponsored_subscription_update(
        &self,
        subscription_id: &str,
        expected_period_start: i64,
        expected_period_end: i64,
        phases: &[SponsoredSubscriptionPhase],
    ) -> ProviderResult<Option<SponsoredSubscriptionSnapshot>> {
        let current = self.get_sponsored_subscription(subscription_id).await?;
        if current.current_period_start != Some(expected_period_start)
            || current.current_period_end != Some(expected_period_end)
        {
            return Ok(None);
        }
        let Some(schedule_id) = current.schedule_id.as_deref() else {
            return Ok(None);
        };
        let schedule = stripe_get_at(
            &self.api_key,
            &self.api_origin,
            &format!("subscription_schedules/{schedule_id}"),
        )
        .await?;
        let phase_at_end = phases
            .iter()
            .find(|phase| {
                phase.start_at_epoch <= expected_period_end
                    && phase
                        .end_at_epoch
                        .is_none_or(|end| expected_period_end < end)
            })
            .or_else(|| {
                phases
                    .iter()
                    .find(|phase| phase.start_at_epoch == expected_period_end)
            })
            .ok_or_else(ProviderError::malformed_response)?;
        let mut normalized = vec![SponsoredSubscriptionPhase {
            start_at_epoch: expected_period_end,
            end_at_epoch: phase_at_end.end_at_epoch,
            items: phase_at_end.items.clone(),
        }];
        normalized.extend(
            phases
                .iter()
                .filter(|phase| phase.start_at_epoch > expected_period_end)
                .cloned(),
        );
        if normalized.windows(2).any(|window| {
            window[0]
                .end_at_epoch
                .is_some_and(|end| end != window[1].start_at_epoch)
        }) {
            return Err(ProviderError::malformed_response());
        }
        let final_empty = normalized
            .last()
            .is_some_and(|phase| phase.items.is_empty());
        if final_empty {
            normalized.pop();
        }
        if normalized.iter().any(|phase| phase.items.is_empty()) {
            return Err(ProviderError::malformed_response());
        }
        let current_start = stripe_current_schedule_phase_start(
            &schedule,
            schedule_id,
            subscription_id,
            expected_period_start,
            expected_period_end,
        )?;
        if stripe_schedule_contains_phases(
            &schedule,
            current_start,
            expected_period_end,
            &current.items,
            &normalized,
        ) && schedule["end_behavior"].as_str()
            == Some(if final_empty { "cancel" } else { "release" })
        {
            Ok(Some(current))
        } else {
            Ok(None)
        }
    }

    async fn get_sponsored_subscription(
        &self,
        subscription_id: &str,
    ) -> ProviderResult<SponsoredSubscriptionSnapshot> {
        let value = stripe_get_at(
            &self.api_key,
            &self.api_origin,
            &format!("subscriptions/{subscription_id}"),
        )
        .await?;
        sponsored_subscription_snapshot(&value, subscription_id)
    }

    async fn update_personal_subscription(
        &self,
        subscription_id: &str,
        phases: &[SponsoredSubscriptionPhase],
        idempotency_key: &str,
    ) -> ProviderResult<SponsoredSubscriptionSnapshot> {
        if subscription_id.is_empty()
            || phases.is_empty()
            || phases.iter().any(|phase| {
                phase.start_at_epoch < 0
                    || phase
                        .end_at_epoch
                        .is_some_and(|end| end <= phase.start_at_epoch)
                    || phase.items.len() != 1
                    || phase
                        .items
                        .iter()
                        .any(|item| item.provider_price_id.is_empty() || item.quantity != 1)
            })
            || phases
                .windows(2)
                .any(|window| window[0].end_at_epoch != Some(window[1].start_at_epoch))
        {
            return Err(ProviderError::malformed_response());
        }
        let current = self.get_sponsored_subscription(subscription_id).await?;
        let schedule_id = match current.schedule_id.clone() {
            Some(schedule_id) => schedule_id,
            None => {
                let schedule = stripe_post_with_idempotency(
                    &self.api_key,
                    "subscription_schedules",
                    &[("from_subscription".into(), subscription_id.into())],
                    &format!("{idempotency_key}-schedule"),
                )
                .await?;
                schedule["id"]
                    .as_str()
                    .filter(|value| !value.is_empty())
                    .ok_or_else(ProviderError::malformed_response)?
                    .to_string()
            }
        };
        let current_period_start = current
            .current_period_start
            .ok_or_else(ProviderError::malformed_response)?;
        let current_end = current
            .current_period_end
            .ok_or_else(ProviderError::malformed_response)?;
        let existing_schedule = stripe_get_at(
            &self.api_key,
            &self.api_origin,
            &format!("subscription_schedules/{schedule_id}"),
        )
        .await?;
        let current_start = stripe_current_schedule_phase_start(
            &existing_schedule,
            &schedule_id,
            subscription_id,
            current_period_start,
            current_end,
        )?;
        let phase_at_end = phases
            .iter()
            .find(|phase| {
                phase.start_at_epoch <= current_end
                    && phase.end_at_epoch.is_none_or(|end| current_end < end)
            })
            .or_else(|| {
                phases
                    .iter()
                    .find(|phase| phase.start_at_epoch == current_end)
            })
            .ok_or_else(ProviderError::malformed_response)?;
        let mut normalized = vec![SponsoredSubscriptionPhase {
            start_at_epoch: current_end,
            end_at_epoch: phase_at_end.end_at_epoch,
            items: phase_at_end.items.clone(),
        }];
        normalized.extend(
            phases
                .iter()
                .filter(|phase| phase.start_at_epoch > current_end)
                .cloned(),
        );
        if normalized
            .windows(2)
            .any(|window| window[0].end_at_epoch != Some(window[1].start_at_epoch))
        {
            return Err(ProviderError::malformed_response());
        }
        let mut form = vec![
            ("end_behavior".into(), "release".into()),
            ("proration_behavior".into(), "none".into()),
            ("phases[0][start_date]".into(), current_start.to_string()),
            ("phases[0][end_date]".into(), current_end.to_string()),
        ];
        for (index, item) in current.items.iter().enumerate() {
            form.push((
                format!("phases[0][items][{index}][price]"),
                item.provider_price_id.clone(),
            ));
            form.push((
                format!("phases[0][items][{index}][quantity]"),
                item.quantity.to_string(),
            ));
        }
        for (phase_index, phase) in normalized.iter().enumerate() {
            let index = phase_index + 1;
            form.push((
                format!("phases[{index}][start_date]"),
                phase.start_at_epoch.to_string(),
            ));
            if let Some(end) = phase.end_at_epoch {
                form.push((format!("phases[{index}][end_date]"), end.to_string()));
            }
            for (item_index, item) in phase.items.iter().enumerate() {
                form.push((
                    format!("phases[{index}][items][{item_index}][price]"),
                    item.provider_price_id.clone(),
                ));
                form.push((
                    format!("phases[{index}][items][{item_index}][quantity]"),
                    item.quantity.to_string(),
                ));
            }
        }
        stripe_post_with_idempotency_at(
            &self.api_key,
            &self.api_origin,
            &format!("subscription_schedules/{schedule_id}"),
            &form,
            &schedule_update_idempotency_key(idempotency_key, &normalized),
        )
        .await?;
        let schedule = stripe_get_at(
            &self.api_key,
            &self.api_origin,
            &format!("subscription_schedules/{schedule_id}"),
        )
        .await?;
        if !stripe_schedule_contains_phases(
            &schedule,
            current_start,
            current_end,
            &current.items,
            &normalized,
        ) {
            return Err(ProviderError::malformed_response());
        }
        self.get_sponsored_subscription(subscription_id).await
    }

    async fn cancel_sponsored_subscription(
        &self,
        organization_id: &str,
        subscription_id: &str,
        idempotency_key: &str,
    ) -> ProviderResult<SponsoredSubscriptionSnapshot> {
        let form = vec![
            ("invoice_now".into(), "false".into()),
            ("prorate".into(), "false".into()),
            (
                "cancellation_details[comment]".into(),
                format!("Sotto sponsored billing for organisation {organization_id}"),
            ),
        ];
        stripe_delete(
            &self.api_key,
            &format!("subscriptions/{subscription_id}"),
            idempotency_key,
            &form,
        )
        .await?;
        self.get_sponsored_subscription(subscription_id).await
    }

    async fn validate_personal_price(
        &self,
        price_id: &str,
        offer: BillingOffer,
    ) -> ProviderResult<()> {
        let value = stripe_get(&self.api_key, &format!("prices/{price_id}")).await?;
        validate_stripe_personal_price(
            &value,
            price_id,
            offer,
            self.api_key.starts_with("sk_live_") || self.api_key.starts_with("rk_live_"),
        )
    }

    async fn validate_cloud_portal_configuration(
        &self,
        configuration_id: &str,
    ) -> ProviderResult<()> {
        let value = stripe_get(
            &self.api_key,
            &format!("billing_portal/configurations/{configuration_id}"),
        )
        .await?;
        validate_cloud_portal_configuration(
            &value,
            configuration_id,
            self.api_key.starts_with("sk_live_") || self.api_key.starts_with("rk_live_"),
        )
    }

    async fn create_portal(
        &self,
        customer: &str,
        return_url: &str,
        configuration_id: Option<&str>,
    ) -> ProviderResult<String> {
        if let Some(configuration_id) = configuration_id {
            self.validate_cloud_portal_configuration(configuration_id)
                .await?;
        }
        let form = portal_session_form(customer, return_url, configuration_id);
        let session = stripe_post(&self.api_key, "billing_portal/sessions", &form).await?;
        session["url"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(ProviderError::malformed_response)
    }

    async fn create_refund(
        &self,
        payment_reference: &str,
        amount_pence: Option<i64>,
        idempotency_key: &str,
    ) -> ProviderResult<RefundReceipt> {
        if !payment_reference.starts_with("pi_") || idempotency_key.trim().is_empty() {
            return Err(ProviderError::malformed_response());
        }
        if amount_pence.is_some_and(|amount| amount <= 0) {
            return Err(ProviderError::malformed_response());
        }
        let mut form = vec![("payment_intent".to_string(), payment_reference.to_string())];
        if let Some(amount) = amount_pence {
            form.push(("amount".to_string(), amount.to_string()));
        }
        let value =
            stripe_post_with_idempotency(&self.api_key, "refunds", &form, idempotency_key).await?;
        let refund_id = value["id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or_else(ProviderError::malformed_response)?;
        let status = value["status"]
            .as_str()
            .filter(|status| !status.is_empty())
            .ok_or_else(ProviderError::malformed_response)?;
        let payment_reference = value["payment_intent"]
            .as_str()
            .filter(|payment_reference| !payment_reference.is_empty())
            .ok_or_else(ProviderError::malformed_response)?;
        let amount_pence = value["amount"]
            .as_i64()
            .filter(|amount| *amount > 0)
            .ok_or_else(ProviderError::malformed_response)?;
        let currency = value["currency"]
            .as_str()
            .filter(|currency| !currency.is_empty())
            .ok_or_else(ProviderError::malformed_response)?;
        let charge_id = parse_refund_charge_id(&value)?;
        Ok(RefundReceipt {
            refund_id: refund_id.to_string(),
            status: status.to_string(),
            payment_reference: payment_reference.to_string(),
            charge_id,
            amount_pence,
            currency: currency.to_string(),
        })
    }

    async fn get_refund(&self, provider_refund_id: &str) -> ProviderResult<RefundReceipt> {
        if !provider_refund_id.starts_with("re_") {
            return Err(ProviderError::malformed_response());
        }
        let value = stripe_get_at(
            &self.api_key,
            &self.api_origin,
            &format!("refunds/{provider_refund_id}"),
        )
        .await?;
        let refund_id = value["id"]
            .as_str()
            .filter(|id| *id == provider_refund_id)
            .ok_or_else(ProviderError::malformed_response)?;
        let status = value["status"]
            .as_str()
            .filter(|status| !status.is_empty())
            .ok_or_else(ProviderError::malformed_response)?;
        let payment_reference = value["payment_intent"]
            .as_str()
            .filter(|payment_reference| !payment_reference.is_empty())
            .ok_or_else(ProviderError::malformed_response)?;
        let amount_pence = value["amount"]
            .as_i64()
            .filter(|amount| *amount > 0)
            .ok_or_else(ProviderError::malformed_response)?;
        let currency = value["currency"]
            .as_str()
            .filter(|currency| !currency.is_empty())
            .ok_or_else(ProviderError::malformed_response)?;
        let charge_id = parse_refund_charge_id(&value)?;
        Ok(RefundReceipt {
            refund_id: refund_id.to_string(),
            status: status.to_string(),
            payment_reference: payment_reference.to_string(),
            charge_id,
            amount_pence,
            currency: currency.to_string(),
        })
    }

    async fn get_refundable_amount(
        &self,
        payment_reference: &str,
        expected_charge_id: &str,
        refund_amount_pence: i64,
    ) -> ProviderResult<(i64, String)> {
        if !payment_reference.starts_with("pi_")
            || !expected_charge_id.starts_with("ch_")
            || refund_amount_pence <= 0
        {
            return Err(ProviderError::malformed_response());
        }
        let payment = stripe_get_at(
            &self.api_key,
            &self.api_origin,
            &format!("payment_intents/{payment_reference}"),
        )
        .await?;
        if payment["id"].as_str() != Some(payment_reference)
            || payment["status"].as_str() != Some("succeeded")
        {
            return Err(ProviderError::malformed_response());
        }
        let amount_received = payment["amount_received"]
            .as_i64()
            .filter(|amount| *amount > 0)
            .ok_or_else(ProviderError::malformed_response)?;
        let currency = payment["currency"]
            .as_str()
            .filter(|currency| !currency.is_empty())
            .ok_or_else(ProviderError::malformed_response)?;
        let charge_id = payment["latest_charge"]
            .as_str()
            .filter(|id| id.starts_with("ch_"))
            .ok_or_else(ProviderError::malformed_response)?;
        if charge_id != expected_charge_id {
            return Err(ProviderError::malformed_response());
        }
        let charge = stripe_get_at(
            &self.api_key,
            &self.api_origin,
            &format!("charges/{charge_id}"),
        )
        .await?;
        if charge["id"].as_str() != Some(charge_id)
            || charge["payment_intent"].as_str() != Some(payment_reference)
            || charge["currency"].as_str() != Some(currency)
        {
            return Err(ProviderError::malformed_response());
        }
        let captured_amount = charge["amount_captured"]
            .as_i64()
            .filter(|amount| *amount > 0)
            .ok_or_else(ProviderError::malformed_response)?;
        let amount_refunded = charge["amount_refunded"]
            .as_i64()
            .filter(|amount| *amount >= refund_amount_pence)
            .ok_or_else(ProviderError::malformed_response)?;
        if captured_amount != amount_received || amount_refunded > captured_amount {
            return Err(ProviderError::malformed_response());
        }
        let refundable = amount_received
            .checked_sub(amount_refunded - refund_amount_pence)
            .filter(|amount| *amount > 0)
            .ok_or_else(ProviderError::malformed_response)?;
        Ok((refundable, currency.to_string()))
    }

    async fn get_subscription(
        &self,
        subscription_id: &str,
    ) -> ProviderResult<SubscriptionObservation> {
        let path = format!("subscriptions/{subscription_id}");
        match stripe_get(&self.api_key, &path).await {
            Ok(subscription) => Ok(SubscriptionObservation::Current(subscription_snapshot(
                &subscription,
                subscription_id,
            )?)),
            Err(error) if error.kind == ProviderErrorKind::ResourceMissing => {
                Ok(SubscriptionObservation::Missing)
            }
            Err(error) => Err(error),
        }
    }

    async fn personal_subscription_period(
        &self,
        subscription_id: &str,
    ) -> ProviderResult<Option<PersonalSubscriptionPeriod>> {
        let object = stripe_get(&self.api_key, &format!("subscriptions/{subscription_id}")).await?;
        if object["id"].as_str() != Some(subscription_id) {
            return Err(ProviderError::malformed_response());
        }
        personal_subscription_period_from_object(&object)
    }

    async fn cancel_subscription(
        &self,
        subscription_id: &str,
        idempotency_key: &str,
        org_id: &str,
    ) -> ProviderResult<SubscriptionObservation> {
        let current = self.get_subscription(subscription_id).await?;
        let SubscriptionObservation::Current(snapshot) = current else {
            return Ok(SubscriptionObservation::Missing);
        };
        // Terminal and missing snapshots already satisfy the purge gate; only a blocking
        // subscription needs the destructive provider call.
        if !matches!(snapshot.status.purge_gate(), PurgeGate::Blocking) {
            return Ok(SubscriptionObservation::Current(snapshot));
        }

        let form = cancellation_form(org_id);
        // Stripe may accept the cancellation while the request times out. A fresh lookup is the
        // source of truth, so a successful terminal observation wins over the original error.
        let cancellation = stripe_delete(
            &self.api_key,
            &format!("subscriptions/{subscription_id}"),
            idempotency_key,
            &form,
        )
        .await;
        let fresh = self.get_subscription(subscription_id).await;
        cancellation_outcome(cancellation.map(|_| ()), fresh)
    }

    async fn cancel_personal_subscription(
        &self,
        subscription_id: &str,
        idempotency_key: &str,
        _user_id: &str,
    ) -> ProviderResult<SubscriptionObservation> {
        let current = self.get_sponsored_subscription(subscription_id).await?;
        if let Some(schedule_id) = current.schedule_id.as_deref() {
            let current_period_start = current
                .current_period_start
                .filter(|start| *start >= 0)
                .ok_or_else(ProviderError::malformed_response)?;
            let current_end = current
                .current_period_end
                .filter(|end| *end > current_period_start)
                .ok_or_else(ProviderError::malformed_response)?;
            if current.items.len() != 1
                || current.items[0].quantity != 1
                || current.items[0].provider_price_id.is_empty()
            {
                return Err(ProviderError::malformed_response());
            }
            let existing_schedule = stripe_get_at(
                &self.api_key,
                &self.api_origin,
                &format!("subscription_schedules/{schedule_id}"),
            )
            .await?;
            let current_start = stripe_current_schedule_phase_start(
                &existing_schedule,
                schedule_id,
                subscription_id,
                current_period_start,
                current_end,
            )?;
            let form =
                personal_schedule_cancellation_form(current_start, current_end, &current.items);
            // Each route request or worker lease gets its own key. Reusing a key across the route
            // and worker could replay a response from before the worker's schedule update.
            let provider_key = personal_schedule_cancellation_idempotency_key(
                subscription_id,
                current_end,
                idempotency_key,
            );
            stripe_post_with_idempotency_at(
                &self.api_key,
                &self.api_origin,
                &format!("subscription_schedules/{schedule_id}"),
                &form,
                &provider_key,
            )
            .await?;
            let schedule = stripe_get_at(
                &self.api_key,
                &self.api_origin,
                &format!("subscription_schedules/{schedule_id}"),
            )
            .await?;
            if schedule["id"].as_str() != Some(schedule_id)
                || stripe_schedule_subscription_id(&schedule) != Some(subscription_id)
                || schedule["end_behavior"].as_str() != Some("cancel")
                || !stripe_schedule_contains_phases(
                    &schedule,
                    current_start,
                    current_end,
                    &current.items,
                    &[],
                )
            {
                return Err(ProviderError::malformed_response());
            }
            let response = stripe_get_at(
                &self.api_key,
                &self.api_origin,
                &format!("subscriptions/{subscription_id}"),
            )
            .await?;
            return Ok(SubscriptionObservation::Current(subscription_snapshot(
                &response,
                subscription_id,
            )?));
        }
        let form = vec![("cancel_at_period_end".to_string(), "true".to_string())];
        let response = stripe_post_with_idempotency_at(
            &self.api_key,
            &self.api_origin,
            &format!("subscriptions/{subscription_id}"),
            &form,
            idempotency_key,
        )
        .await?;
        Ok(SubscriptionObservation::Current(subscription_snapshot(
            &response,
            subscription_id,
        )?))
    }

    async fn cancel_personal_subscription_at_boundary(
        &self,
        subscription_id: &str,
        boundary_epoch: i64,
        idempotency_key: &str,
        _user_id: &str,
    ) -> ProviderResult<SponsoredSubscriptionSnapshot> {
        if boundary_epoch <= 0 || idempotency_key.trim().is_empty() {
            return Err(ProviderError::malformed_response());
        }
        let current = self.get_sponsored_subscription(subscription_id).await?;
        if current.subscription_id != subscription_id
            || current.current_period_end != Some(boundary_epoch)
        {
            return Err(ProviderError::malformed_response());
        }
        if current.status == SubscriptionStatus::Canceled {
            return Ok(current);
        }
        if current.cancel_at.is_some_and(|end| end != boundary_epoch) {
            return Err(ProviderError::malformed_response());
        }

        if let Some(schedule_id) = current.schedule_id.as_deref() {
            let period_start = current
                .current_period_start
                .filter(|start| *start >= 0)
                .ok_or_else(ProviderError::malformed_response)?;
            let period_end = current
                .current_period_end
                .filter(|end| *end > period_start && *end == boundary_epoch)
                .ok_or_else(ProviderError::malformed_response)?;
            if current.items.len() != 1
                || current.items[0].quantity != 1
                || current.items[0].provider_price_id.is_empty()
            {
                return Err(ProviderError::malformed_response());
            }
            let existing_schedule = stripe_get_at(
                &self.api_key,
                &self.api_origin,
                &format!("subscription_schedules/{schedule_id}"),
            )
            .await?;
            let phase_start = stripe_current_schedule_phase_start(
                &existing_schedule,
                schedule_id,
                subscription_id,
                period_start,
                period_end,
            )?;
            let form =
                personal_schedule_cancellation_form(phase_start, boundary_epoch, &current.items);
            let provider_key = personal_schedule_cancellation_idempotency_key(
                subscription_id,
                boundary_epoch,
                idempotency_key,
            );
            stripe_post_with_idempotency_at(
                &self.api_key,
                &self.api_origin,
                &format!("subscription_schedules/{schedule_id}"),
                &form,
                &provider_key,
            )
            .await?;
            let schedule = stripe_get_at(
                &self.api_key,
                &self.api_origin,
                &format!("subscription_schedules/{schedule_id}"),
            )
            .await?;
            if schedule["id"].as_str() != Some(schedule_id)
                || stripe_schedule_subscription_id(&schedule) != Some(subscription_id)
                || schedule["end_behavior"].as_str() != Some("cancel")
                || !stripe_schedule_contains_phases(
                    &schedule,
                    phase_start,
                    boundary_epoch,
                    &current.items,
                    &[],
                )
            {
                return Err(ProviderError::malformed_response());
            }
        } else if current.cancel_at != Some(boundary_epoch) && !current.cancel_at_period_end {
            // An explicit timestamp prevents Stripe from silently moving this cancellation to
            // the next period if the billing boundary rolls between the read and the write.
            let form = vec![("cancel_at".to_string(), boundary_epoch.to_string())];
            stripe_post_with_idempotency_at(
                &self.api_key,
                &self.api_origin,
                &format!("subscriptions/{subscription_id}"),
                &form,
                idempotency_key,
            )
            .await?;
        }

        let updated = self.get_sponsored_subscription(subscription_id).await?;
        if updated.subscription_id != subscription_id
            || updated.current_period_end != Some(boundary_epoch)
            || (!updated.cancel_at_period_end
                && updated.cancel_at != Some(boundary_epoch)
                && updated.status != SubscriptionStatus::Canceled)
        {
            return Err(ProviderError::malformed_response());
        }
        Ok(updated)
    }

    async fn terminate_personal_subscription(
        &self,
        subscription_id: &str,
        idempotency_key: &str,
        user_id: &str,
    ) -> ProviderResult<SubscriptionObservation> {
        let current = self.get_subscription(subscription_id).await?;
        let SubscriptionObservation::Current(snapshot) = current else {
            return Ok(current);
        };
        if !matches!(snapshot.status.purge_gate(), PurgeGate::Blocking) {
            return Ok(SubscriptionObservation::Current(snapshot));
        }
        let form = vec![
            ("invoice_now".into(), "false".into()),
            ("prorate".into(), "false".into()),
            (
                "cancellation_details[comment]".into(),
                format!("Sotto personal billing correction {user_id}"),
            ),
        ];
        let cancellation = stripe_delete(
            &self.api_key,
            &format!("subscriptions/{subscription_id}"),
            idempotency_key,
            &form,
        )
        .await;
        let fresh = self.get_subscription(subscription_id).await;
        cancellation_outcome(cancellation.map(|_| ()), fresh)
    }
}

#[cfg(feature = "e2e-mock-billing")]
struct E2eBilling {
    provider_origin: String,
}

#[cfg(feature = "e2e-mock-billing")]
#[async_trait]
impl SubscriptionProvider for E2eBilling {
    async fn create_checkout(
        &self,
        _org_id: &str,
        _customer: Option<&str>,
        success_url: &str,
        cancel_url: &str,
    ) -> ProviderResult<String> {
        self.page_url(
            "checkout",
            &[("success_url", success_url), ("cancel_url", cancel_url)],
        )
    }

    async fn create_portal(
        &self,
        _customer: &str,
        return_url: &str,
        _configuration_id: Option<&str>,
    ) -> ProviderResult<String> {
        self.page_url("portal", &[("return_url", return_url)])
    }

    async fn validate_personal_price(
        &self,
        _price_id: &str,
        _offer: BillingOffer,
    ) -> ProviderResult<()> {
        Ok(())
    }

    async fn get_subscription(
        &self,
        _subscription_id: &str,
    ) -> ProviderResult<SubscriptionObservation> {
        Err(ProviderError::unsupported("subscription_lookup"))
    }

    async fn cancel_subscription(
        &self,
        _subscription_id: &str,
        _idempotency_key: &str,
        _org_id: &str,
    ) -> ProviderResult<SubscriptionObservation> {
        Err(ProviderError::unsupported("subscription_cancellation"))
    }
}

#[cfg(feature = "e2e-mock-billing")]
impl E2eBilling {
    fn page_url(&self, page: &str, params: &[(&str, &str)]) -> ProviderResult<String> {
        let base = format!(
            "{}/e2e/billing/{page}",
            self.provider_origin.trim_end_matches('/')
        );
        let mut url = Url::parse(&base).map_err(|_| ProviderError::malformed_response())?;
        url.query_pairs_mut().extend_pairs(params.iter().copied());
        Ok(url.to_string())
    }
}

pub fn router() -> Router<AppState> {
    let router = Router::new()
        .route("/orgs/{org_id}/billing/checkout", post(create_checkout))
        .route("/orgs/{org_id}/billing/portal", post(create_portal))
        .route(
            "/orgs/{org_id}/billing/sponsored/seats",
            get(sponsored_seats),
        )
        .route(
            "/orgs/{org_id}/billing/sponsored/quote",
            post(sponsored_quote),
        )
        .route(
            "/orgs/{org_id}/billing/sponsored/checkout",
            post(sponsored_checkout),
        )
        .route(
            "/orgs/{org_id}/billing/sponsored/operations/{operation_id}",
            get(sponsored_operation),
        )
        .route("/billing/personal/quote", get(personal_quote))
        .route("/billing/personal/checkout", post(personal_checkout))
        .route(
            "/billing/personal/operations/{operation_id}",
            get(personal_operation),
        )
        .route("/billing/personal/lifecycle", get(personal_lifecycle))
        .route("/billing/personal/portal", post(personal_portal))
        .route("/billing/personal/cancel", post(personal_cancel))
        .route("/billing/personal/refunds", post(personal_refund_request))
        .route(
            "/billing/personal/refunds/{request_id}",
            get(personal_refund_status),
        )
        .route(
            "/billing/personal/refunds/{request_id}/confirm-early",
            post(personal_refund_confirm_early),
        )
        .route(
            "/billing/refunds/{request_id}/review",
            post(operator_review_refund),
        )
        .route(
            "/billing/refunds/{request_id}/resolve",
            post(operator_resolve_refund),
        )
        .route(
            "/billing/refunds/{request_id}/process",
            post(operator_process_refund),
        )
        .route("/billing/webhook", post(webhook));

    #[cfg(feature = "e2e-mock-billing")]
    let router = router
        .route("/e2e/billing/checkout", get(e2e_checkout))
        .route("/e2e/billing/portal", get(e2e_portal));

    router
}

fn billing_config(state: &AppState) -> Result<&BillingState> {
    state
        .billing
        .as_ref()
        .ok_or_else(|| Error::NotConfigured("billing is not configured".into()))
}

fn cloud_sales_config(state: &AppState) -> Result<&BillingState> {
    let billing = billing_config(state)?;
    ensure_cloud_sales_enabled(billing.cloud_sales_enabled())?;
    Ok(billing)
}

fn ensure_cloud_sales_for_operation(sales_enabled: bool, operation_exists: bool) -> Result<()> {
    if operation_exists {
        Ok(())
    } else {
        ensure_cloud_sales_enabled(sales_enabled)
    }
}

fn ensure_cloud_sales_enabled(enabled: bool) -> Result<()> {
    if enabled {
        Ok(())
    } else {
        Err(Error::NotConfigured(
            "hosted Cloud purchases are not enabled".into(),
        ))
    }
}

const SPONSORED_BILLING_ENABLED_ENV: &str = "SOTTO_SPONSORED_BILLING_ENABLED";
const BILLING_CORRECTIONS_ENABLED_ENV: &str = "SOTTO_BILLING_CORRECTIONS_ENABLED";
const BILLING_OPERATOR_TOKEN_ENV: &str = "SOTTO_BILLING_OPERATOR_TOKEN";
const BILLING_CORRECTION_POLICY_VERSION: &str = "2026-10-04";

fn sponsored_billing_enabled() -> bool {
    std::env::var(SPONSORED_BILLING_ENABLED_ENV).as_deref() == Ok("1")
}

fn billing_corrections_enabled() -> bool {
    std::env::var(BILLING_CORRECTIONS_ENABLED_ENV).as_deref() == Ok("1")
}

fn sponsored_billing_config(state: &AppState) -> Result<&BillingState> {
    if !sponsored_billing_enabled() {
        return Err(Error::NotConfigured(
            "hosted sponsored billing is not enabled".into(),
        ));
    }
    billing_config(state)
}

fn sponsored_sales_config(state: &AppState) -> Result<&BillingState> {
    let billing = sponsored_billing_config(state)?;
    ensure_cloud_sales_enabled(billing.cloud_sales_enabled())?;
    Ok(billing)
}

/// Billing is admin+: the same bar as membership management, and a non-member sees a 404.
async fn require_billing_admin(
    tx: &mut Transaction<'_, Postgres>,
    org_id: &str,
    user_id: &str,
) -> Result<()> {
    let access = org::access_for_update(tx, org_id, user_id).await?;
    access.require_write()?;
    if !access.role().can_manage_members() {
        return Err(Error::Forbidden(
            "managing billing requires the admin or owner role".into(),
        ));
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct PersonalQuoteQuery {
    offer: String,
}

#[derive(Debug, Serialize)]
struct PersonalQuoteView {
    offer: String,
    amount_pence: i64,
    currency: &'static str,
    interval: &'static str,
    tax_treatment: &'static str,
    quote_version: i64,
    quote_expires_at_epoch: i64,
    founding: bool,
    founding_remaining_places: Option<i64>,
    founding_term: Option<&'static str>,
    next_renewal_amount_pence: i64,
}

#[derive(Debug, Deserialize)]
struct PersonalCheckoutRequest {
    offer: String,
    idempotency_key: String,
    quote_version: i64,
    quote_expires_at_epoch: i64,
    return_url: String,
    #[cfg(feature = "e2e-stripe-test-clock")]
    #[serde(default)]
    test_clock_customer: Option<E2eTestClockCustomer>,
}

#[derive(Debug, Serialize)]
struct PersonalCheckoutView {
    operation_id: String,
    state: String,
    checkout_url: Option<String>,
}

#[derive(Debug, Serialize)]
struct PersonalOperationView {
    operation_id: String,
    offer: String,
    state: String,
    provider_operation_id: Option<String>,
    checkout_url: Option<String>,
}

fn billing_offer(value: &str) -> Result<BillingOffer> {
    BillingOffer::ALL
        .into_iter()
        .find(|offer| offer.as_str() == value)
        .ok_or_else(|| Error::BadRequest("unsupported billing offer".into()))
}

fn parse_founding_offer(value: &str) -> Result<FoundingOffer> {
    match value {
        "founding_monthly" => Ok(FoundingOffer::Monthly),
        "founding_annual" => Ok(FoundingOffer::Annual),
        _ => Err(Error::Internal("founding award offer is corrupt".into())),
    }
}

#[derive(Debug, Deserialize)]
struct SponsoredQuoteRequest {
    action: String,
    offer: String,
    beneficiary_ids: Vec<String>,
}

#[derive(Debug, Serialize)]
struct SponsoredQuoteView {
    action: String,
    offer: String,
    seat_count: i64,
    amount_pence: i64,
    currency: &'static str,
    interval: &'static str,
    quote_version: i64,
    quote_expires_at_epoch: i64,
    founding: bool,
    founding_remaining_places: Option<i64>,
    founding_term: Option<&'static str>,
    next_renewal_amount_pence: i64,
}

#[derive(Debug, Serialize)]
struct SponsoredSeatView {
    seat_id: String,
    beneficiary_id: String,
    offer: String,
    effective_from: i64,
    effective_until: Option<i64>,
    state: String,
    founding_price_end_epoch: Option<i64>,
    standard_renewal_amount_pence: Option<i64>,
    founding_transition_status: Option<String>,
    founding_transition_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SponsoredCheckoutRequest {
    action: String,
    offer: String,
    beneficiary_id: String,
    replacement_beneficiary_id: Option<String>,
    quote_version: i64,
    quote_expires_at_epoch: i64,
    effective_from: i64,
    effective_until: Option<i64>,
    idempotency_key: String,
    return_url: String,
    #[cfg(feature = "e2e-stripe-test-clock")]
    #[serde(default)]
    test_clock_customer: Option<E2eTestClockCustomer>,
}

#[derive(Debug, Serialize)]
struct SponsoredOperationView {
    operation_id: String,
    action: String,
    state: String,
    provider_checkout_url: Option<String>,
}

async fn sponsored_seats(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<String>,
) -> Result<Json<Vec<SponsoredSeatView>>> {
    sponsored_billing_config(&state)?;
    let seats = sponsored_billing::list_seats(&state.pool, &org_id, &user.user_id)
        .await
        .map_err(Error::from)?;
    Ok(Json(
        seats
            .into_iter()
            .map(|seat| SponsoredSeatView {
                seat_id: seat.seat_id,
                beneficiary_id: seat.beneficiary_id,
                offer: seat.offer.as_str().into(),
                effective_from: seat.effective_from,
                effective_until: seat.effective_until,
                state: seat.state.as_str().into(),
                founding_price_end_epoch: seat.founding_price_end_epoch,
                standard_renewal_amount_pence: seat.founding_offer.and_then(|offer| {
                    FoundingOffer::from_billing_offer(offer)
                        .map(FoundingOffer::standard_billing_offer)
                        .map(BillingOffer::expected_amount_pence)
                }),
                founding_transition_status: seat.founding_transition_status,
                founding_transition_reason: seat.founding_transition_reason,
            })
            .collect(),
    ))
}

async fn sponsored_quote(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<String>,
    Json(request): Json<SponsoredQuoteRequest>,
) -> Result<Json<SponsoredQuoteView>> {
    let action = SponsoredSeatAction::parse(&request.action).map_err(Error::from)?;
    if request.beneficiary_ids.len() != 1
        || request
            .beneficiary_ids
            .iter()
            .any(|beneficiary| beneficiary.trim().is_empty())
        || request
            .beneficiary_ids
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != request.beneficiary_ids.len()
    {
        return Err(Error::BadRequest(
            "a sponsored quote must name exactly one unique beneficiary account".into(),
        ));
    }
    let requested_offer = billing_offer(&request.offer)?;
    let billing = sponsored_sales_config(&state)?;
    let catalogue = billing
        .price_catalogue()
        .ok_or_else(|| Error::NotConfigured("hosted sponsored billing is not configured".into()))?;
    let access = org::access(&state.pool, &org_id, &user.user_id).await?;
    access.require_write()?;
    if !access.role().can_manage_members() {
        return Err(Error::Forbidden(
            "managing sponsored billing requires the admin or owner role".into(),
        ));
    }
    for beneficiary_id in &request.beneficiary_ids {
        let exists: Option<String> = sqlx::query_scalar("SELECT id FROM users WHERE id = $1")
            .bind(beneficiary_id)
            .fetch_optional(&state.pool)
            .await?;
        if exists.is_none() {
            return Err(Error::NotFound("sponsored beneficiary not found".into()));
        }
    }
    billing
        .validate_cloud_portal_configuration()
        .await
        .map_err(ProviderError::into_error)?;
    validate_personal_catalogue(billing, catalogue).await?;
    let now = sponsored_billing::current_epoch();
    let (offer, founding_remaining_places) = if action == SponsoredSeatAction::Remove {
        (requested_offer, None)
    } else {
        let mut tx = state.pool.begin().await?;
        let selected = select_founding_aware_offer(
            &mut tx,
            &request.beneficiary_ids[0],
            FoundingOffer::for_billing_offer_interval(requested_offer),
            None,
            now,
            billing.founding_schedule_worker_enabled(),
        )
        .await
        .map_err(|error| Error::Internal(error.to_string()))?;
        let founding_offer = FoundingOffer::from_billing_offer(selected);
        let remaining = if let Some(founding_offer) = founding_offer {
            let status = founding_allocator::quote_status(&mut tx, founding_offer, now)
                .await
                .map_err(|error| Error::Internal(error.to_string()))?;
            let subscription_period_end: Option<Option<i64>> = sqlx::query_scalar(
                "SELECT current_period_end FROM billing_sponsored_subscriptions \
                 WHERE organization_id = $1 AND provider_subscription_id IS NOT NULL \
                   AND status IN ('pending', 'active', 'past_due')",
            )
            .bind(&org_id)
            .fetch_optional(&mut *tx)
            .await?;
            if subscription_period_end.is_some_and(|period_end| period_end.is_none()) {
                return Err(Error::Conflict(
                    "sponsored subscription period is unavailable; founding quote cannot be issued"
                        .into(),
                ));
            }
            Some(status.remaining_places)
        } else {
            None
        };
        tx.rollback().await?;
        (selected, remaining)
    };
    let quote = sponsored_billing::quote(action, offer, request.beneficiary_ids.len(), now)
        .map_err(Error::from)?;
    Ok(Json(SponsoredQuoteView {
        action: quote.action.as_str().into(),
        offer: quote.offer.as_str().into(),
        seat_count: quote.seat_count,
        amount_pence: quote.amount_pence,
        currency: quote.currency,
        interval: quote.interval,
        quote_version: quote.quote_version,
        quote_expires_at_epoch: quote.quote_expires_at_epoch,
        founding: FoundingOffer::from_billing_offer(offer).is_some(),
        founding_remaining_places,
        founding_term: FoundingOffer::from_billing_offer(offer).map(|offer| match offer {
            FoundingOffer::Monthly => "for the first 12 monthly billing periods",
            FoundingOffer::Annual => "for the first annual billing period",
        }),
        next_renewal_amount_pence: match FoundingOffer::from_billing_offer(offer) {
            Some(FoundingOffer::Monthly) => BillingOffer::StandardMonthly.expected_amount_pence(),
            Some(FoundingOffer::Annual) => BillingOffer::StandardAnnual.expected_amount_pence(),
            None => offer.expected_amount_pence(),
        },
    }))
}

async fn sponsored_checkout(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<String>,
    Json(input): Json<SponsoredCheckoutRequest>,
) -> Result<Json<SponsoredOperationView>> {
    let billing = sponsored_billing_config(&state)?;
    #[cfg(feature = "e2e-stripe-test-clock")]
    let e2e_test_clock_customer = input.test_clock_customer.clone();
    let existing_operation = if billing.cloud_sales_enabled() {
        false
    } else {
        sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM billing_sponsored_operations \
             WHERE organization_id = $1 AND actor_user_id = $2 AND idempotency_key = $3)",
        )
        .bind(&org_id)
        .bind(&user.user_id)
        .bind(&input.idempotency_key)
        .fetch_one(&state.pool)
        .await?
    };
    ensure_cloud_sales_for_operation(billing.cloud_sales_enabled(), existing_operation)?;
    let catalogue = billing
        .price_catalogue()
        .ok_or_else(|| Error::NotConfigured("hosted sponsored billing is not configured".into()))?;
    let action = SponsoredSeatAction::parse(&input.action).map_err(Error::from)?;
    let offer = billing_offer(&input.offer)?;
    if billing.cloud_sales_enabled() {
        billing
            .validate_cloud_portal_configuration()
            .await
            .map_err(ProviderError::into_error)?;
    }
    validate_personal_catalogue(billing, catalogue).await?;
    let request = SponsoredSeatRequest {
        action,
        beneficiary_id: input.beneficiary_id,
        replacement_beneficiary_id: input.replacement_beneficiary_id,
        offer,
        quote_version: input.quote_version,
        quote_expires_at_epoch: input.quote_expires_at_epoch,
        effective_from: input.effective_from,
        effective_until: input.effective_until,
        idempotency_key: input.idempotency_key,
    };
    billing_operations::validate_return_url(&billing.return_url, &input.return_url)
        .map_err(Error::from)?;
    #[cfg(feature = "e2e-stripe-test-clock")]
    if let Some(binding) = e2e_test_clock_customer.as_ref() {
        if action != SponsoredSeatAction::Add {
            return Err(Error::BadRequest(
                "test-clock customer can only be used for a first sponsored checkout".into(),
            ));
        }
        if std::env::var("SOTTO_E2E_TEST_CLOCK_CHECKOUT_ENABLED")
            .ok()
            .as_deref()
            != Some("1")
        {
            return Err(Error::NotConfigured(
                "test-clock checkout binding is disabled".into(),
            ));
        }
    }
    let now = sponsored_billing::current_epoch();
    let mut tx = state.pool.begin().await?;
    // Keep the allocator mutex ahead of organisation, seat, operation and subscription locks.
    // Invoice settlement and expiry use the same order.
    founding_allocator::lock_capacity(&mut tx)
        .await
        .map_err(|error| Error::Internal(error.to_string()))?;
    let mut transfer_beneficiaries = vec![request.beneficiary_id.as_str()];
    if let Some(replacement) = request.replacement_beneficiary_id.as_deref() {
        transfer_beneficiaries.push(replacement);
    }
    billing_transfers::lock_and_check_no_live_transfer(&mut tx, &transfer_beneficiaries, now)
        .await
        .map_err(Error::from)?;
    let incoming_beneficiary = request
        .replacement_beneficiary_id
        .as_deref()
        .unwrap_or(&request.beneficiary_id);
    sponsored_billing::cancel_expired_pending_checkouts(
        &mut tx,
        &org_id,
        incoming_beneficiary,
        now,
    )
    .await
    .map_err(Error::from)?;
    let operation =
        sponsored_billing::begin_operation(&mut tx, &org_id, &user.user_id, &request, now)
            .await
            .map_err(Error::from)?;
    if !matches!(operation.state.as_str(), "pending" | "unknown") {
        tx.rollback().await?;
        return Ok(Json(SponsoredOperationView {
            operation_id: operation.operation_id,
            action: operation.action.as_str().into(),
            state: operation.state,
            provider_checkout_url: operation.provider_checkout_url,
        }));
    }
    if request.action != SponsoredSeatAction::Remove {
        let selected_offer = select_founding_aware_offer(
            &mut tx,
            incoming_beneficiary,
            FoundingOffer::for_billing_offer_interval(offer),
            Some(&operation.operation_id),
            now,
            billing.founding_schedule_worker_enabled(),
        )
        .await
        .map_err(|error| Error::Internal(error.to_string()))?;
        if selected_offer != offer {
            return Err(Error::Conflict(
                "sponsored billing quote no longer matches founding availability; refresh the quote".into(),
            ));
        }
    }
    let existing_subscription: Option<(Option<String>, Option<String>, Option<i64>)> =
        sqlx::query_as(
            "SELECT provider_customer_id, provider_subscription_id, current_period_end \
         FROM billing_sponsored_subscriptions \
         WHERE organization_id = $1 AND provider_subscription_id IS NOT NULL \
           AND status IN ('pending', 'active', 'past_due') FOR UPDATE",
        )
        .bind(&org_id)
        .fetch_optional(&mut *tx)
        .await?;
    let existing_customer: Option<String> =
        sqlx::query_scalar("SELECT stripe_customer_id FROM organizations WHERE id = $1")
            .bind(&org_id)
            .fetch_one(&mut *tx)
            .await?;

    #[cfg(feature = "e2e-stripe-test-clock")]
    if let Some(binding) = e2e_test_clock_customer.as_ref() {
        if existing_subscription.is_some() {
            return Err(Error::BadRequest(
                "test-clock customer can only be used for a first sponsored checkout".into(),
            ));
        }
        if existing_customer
            .as_deref()
            .is_some_and(|existing| existing != binding.customer_id)
        {
            return Err(Error::Conflict(
                "test-clock customer is not available for this organisation".into(),
            ));
        }
        let attached = sqlx::query(
            "UPDATE organizations SET stripe_customer_id = $2 \
             WHERE id = $1 AND (stripe_customer_id IS NULL OR stripe_customer_id = $2)",
        )
        .bind(&org_id)
        .bind(&binding.customer_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if attached != 1 {
            return Err(Error::Conflict(
                "test-clock customer is not available for this organisation".into(),
            ));
        }
    }

    if request.action != SponsoredSeatAction::Remove {
        if let Some(founding_offer) = FoundingOffer::from_billing_offer(offer) {
            let reservation_expires_at = if let Some((_, Some(_), period_end)) =
                existing_subscription.as_ref()
            {
                let period_end = period_end.ok_or_else(|| {
                    Error::Conflict(
                        "sponsored subscription period is unavailable; founding quote cannot be reserved".into(),
                    )
                })?;
                period_end
                    .checked_add(founding_allocator::SETTLEMENT_RECOVERY_SECONDS)
                    .ok_or_else(|| Error::Internal("billing clock overflow".into()))?
                    .max(request.quote_expires_at_epoch)
            } else {
                personal_checkout_expiry(request.quote_expires_at_epoch, now)?
            };
            let reservation = founding_allocator::reserve_with_payer_kind_until(
                &mut tx,
                &format!("founding:{}", operation.operation_id),
                &operation.operation_id,
                incoming_beneficiary,
                founding_allocator::FoundingPayerKind::Sponsor,
                &org_id,
                founding_offer,
                request.quote_version,
                request.quote_expires_at_epoch,
                reservation_expires_at,
                now,
            )
            .await
            .map_err(|error| Error::Conflict(error.to_string()))?;
            if matches!(reservation, founding_allocator::ReservationOutcome::Full) {
                return Err(Error::Conflict(
                    "founding hosted places are no longer available; refresh the quote".into(),
                ));
            }
        }
    }
    if let Some((_, Some(subscription_id), _)) = existing_subscription.as_ref() {
        // Persist the target before the provider call. If Stripe ends the subscription while an
        // update is in flight, the terminal webhook can fail this operation and release its
        // founding reservation instead of leaving an orphaned pending seat behind.
        sqlx::query(
            "UPDATE billing_sponsored_operations SET provider_subscription_id = $2, updated_at = now() \
             WHERE operation_id = $1 AND state = 'pending'",
        )
        .bind(&operation.operation_id)
        .bind(subscription_id)
        .execute(&mut *tx)
        .await?;
    }

    #[cfg(feature = "e2e-stripe-test-clock")]
    if let Some(binding) = e2e_test_clock_customer.as_ref() {
        // `begin_operation` has already checked the actor's organisation authority. Claiming
        // earlier would let an unauthorised caller bind the runner Customer before rejection.
        billing
            .provider
            .claim_e2e_test_clock_customer(
                &binding.customer_id,
                &binding.clock_id,
                &binding.run_id,
                &user.user_id,
            )
            .await
            .map_err(ProviderError::into_error)?;
    }
    tx.commit().await?;

    #[cfg(feature = "e2e-stripe-test-clock")]
    let checkout_customer_id = e2e_test_clock_customer
        .as_ref()
        .map(|binding| binding.customer_id.clone())
        .or(existing_customer.clone());
    #[cfg(not(feature = "e2e-stripe-test-clock"))]
    let checkout_customer_id = existing_customer.clone();

    let provider_request = if let Some(value) = operation.provider_request.clone() {
        if operation.provider_request_version != Some(1) {
            return Err(Error::Conflict(
                "sponsored billing operation needs recovery review".into(),
            ));
        }
        let saved = serde_json::from_value::<sponsored_billing::SponsoredProviderRequest>(value)
            .map_err(|_| Error::Internal("saved sponsored provider request is invalid".into()))?;
        #[cfg(feature = "e2e-stripe-test-clock")]
        if let Some(binding) = e2e_test_clock_customer.as_ref() {
            match &saved {
                sponsored_billing::SponsoredProviderRequest::Checkout { customer_id, .. }
                    if customer_id.as_deref() == Some(binding.customer_id.as_str()) => {}
                _ => {
                    return Err(Error::Conflict(
                        "test-clock customer does not match the saved checkout request".into(),
                    ));
                }
            }
        }
        saved
    } else if operation.state == "pending" {
        let (success_url, cancel_url) = checkout_return_urls(&billing.return_url);
        if let Some((_, Some(subscription_id), _)) = existing_subscription.as_ref() {
            let phases = sponsored_provider_phases(
                &state.pool,
                &org_id,
                catalogue,
                sponsored_billing::current_epoch(),
            )
            .await?;
            let snapshot = billing
                .provider
                .get_sponsored_subscription(subscription_id)
                .await
                .map_err(ProviderError::into_error)?;
            if snapshot.subscription_id != *subscription_id {
                return Err(Error::Conflict(
                    "sponsored subscription binding changed; operation needs review".into(),
                ));
            }
            sponsored_billing::SponsoredProviderRequest::ScheduleUpdate {
                organization_id: org_id.clone(),
                subscription_id: subscription_id.clone(),
                expected_period_start: snapshot.current_period_start.ok_or_else(|| {
                    Error::Conflict("sponsored subscription period is unavailable".into())
                })?,
                expected_period_end: snapshot.current_period_end.ok_or_else(|| {
                    Error::Conflict("sponsored subscription period is unavailable".into())
                })?,
                phases,
            }
        } else if existing_subscription.is_none() && action == SponsoredSeatAction::Add {
            sponsored_billing::SponsoredProviderRequest::Checkout {
                organization_id: org_id.clone(),
                customer_id: checkout_customer_id,
                items: sponsored_provider_items(&state.pool, &org_id, catalogue, &operation)
                    .await?,
                expires_at_epoch: personal_checkout_expiry(
                    operation.quote_expires_at_epoch,
                    sponsored_billing::current_epoch(),
                )?,
                success_url,
                cancel_url,
            }
        } else {
            return Err(Error::Conflict(
                "sponsored subscription is missing for this change".into(),
            ));
        }
    } else {
        return Err(Error::Conflict(
            "sponsored billing operation needs recovery review".into(),
        ));
    };
    if operation.provider_request.is_none() {
        let mut request_tx = state.pool.begin().await?;
        sponsored_billing::persist_provider_request(
            &mut request_tx,
            &operation.operation_id,
            &provider_request,
        )
        .await
        .map_err(Error::from)?;
        request_tx.commit().await?;
    }
    recover_sponsored_billing_operation_by_id(&state.pool, billing, &operation.operation_id)
        .await?;
    let operation = sponsored_billing::load_operation_for_actor(
        &state.pool,
        &operation.operation_id,
        &user.user_id,
    )
    .await
    .map_err(Error::from)?
    .ok_or_else(|| Error::NotFound("sponsored billing operation not found".into()))?;
    Ok(Json(SponsoredOperationView {
        operation_id: operation.operation_id,
        action: operation.action.as_str().into(),
        state: operation.state,
        provider_checkout_url: operation.provider_checkout_url,
    }))
}

async fn sponsored_operation(
    State(state): State<AppState>,
    user: AuthUser,
    Path((org_id, operation_id)): Path<(String, String)>,
) -> Result<Json<SponsoredOperationView>> {
    sponsored_billing_config(&state)?;
    let operation =
        sponsored_billing::load_operation_for_actor(&state.pool, &operation_id, &user.user_id)
            .await
            .map_err(Error::from)?
            .ok_or_else(|| Error::NotFound("sponsored billing operation not found".into()))?;
    if operation.organization_id != org_id {
        return Err(Error::NotFound(
            "sponsored billing operation not found".into(),
        ));
    }
    Ok(Json(SponsoredOperationView {
        operation_id: operation.operation_id,
        action: operation.action.as_str().into(),
        state: operation.state,
        provider_checkout_url: operation.provider_checkout_url,
    }))
}

async fn validate_personal_catalogue(
    billing: &BillingState,
    catalogue: &BillingPriceIds,
) -> Result<()> {
    for offer in BillingOffer::ALL {
        billing
            .provider
            .validate_personal_price(catalogue.id_for(offer), offer)
            .await
            .map_err(ProviderError::into_error)?;
    }
    Ok(())
}

async fn sponsored_provider_items(
    pool: &sqlx::PgPool,
    organization_id: &str,
    catalogue: &BillingPriceIds,
    operation: &sponsored_billing::SponsoredOperation,
) -> Result<Vec<SponsoredSubscriptionItemRequest>> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT offer, count(*)::BIGINT FROM billing_sponsored_seats \
         WHERE organization_id = $1 AND state IN ('active', 'pending') \
           AND NOT ($2 IN ('remove', 'replace') AND beneficiary_id = $3) \
         GROUP BY offer ORDER BY offer",
    )
    .bind(organization_id)
    .bind(operation.action.as_str())
    .bind(&operation.beneficiary_id)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|(offer, quantity)| {
            let offer = billing_offer(&offer)?;
            Ok(SponsoredSubscriptionItemRequest {
                provider_price_id: catalogue.id_for(offer).to_string(),
                quantity,
            })
        })
        .collect()
}

/// Build the complete future seat schedule from Sotto's interval ledger. Stripe's current
/// quantities describe only the provider's present phase; rebuilding a schedule from them loses
/// an earlier removal or a later finite seat boundary whenever two changes are pending together.
type SponsoredScheduleSeatRow = (
    String,
    String,
    String,
    i64,
    Option<i64>,
    Option<i64>,
    Option<String>,
);
type SponsoredSchedulePendingChange = (String, String, Option<i64>);

pub(crate) async fn sponsored_provider_phases(
    pool: &sqlx::PgPool,
    organization_id: &str,
    catalogue: &BillingPriceIds,
    now_epoch: i64,
) -> Result<Vec<SponsoredSubscriptionPhase>> {
    let rows: Vec<SponsoredScheduleSeatRow> = sqlx::query_as(
        "SELECT seat.beneficiary_id, seat.offer, seat.state, seat.effective_from, seat.effective_until, \
                award.original_end_epoch, award.offer \
         FROM billing_sponsored_seats AS seat \
         LEFT JOIN billing_founding_awards AS award ON award.beneficiary_id = seat.beneficiary_id \
           AND award.payer_kind = 'sponsor' AND award.payer_id = seat.organization_id \
         WHERE seat.organization_id = $1 AND seat.state IN ('active', 'pending', 'scheduled_removal') \
         ORDER BY seat.effective_from, seat.seat_id",
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await?;
    let pending_changes: Vec<SponsoredSchedulePendingChange> = sqlx::query_as(
        "SELECT beneficiary_id, action, effective_until \
         FROM billing_sponsored_operations \
         WHERE organization_id = $1 \
           AND state IN ('pending', 'checkout_created', 'provider_pending', 'unknown') \
           AND action IN ('remove', 'replace') ORDER BY created_at",
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await?;
    compile_sponsored_provider_phases(rows, &pending_changes, catalogue, now_epoch)
}

fn compile_sponsored_provider_phases(
    mut rows: Vec<SponsoredScheduleSeatRow>,
    pending_changes: &[SponsoredSchedulePendingChange],
    catalogue: &BillingPriceIds,
    now_epoch: i64,
) -> Result<Vec<SponsoredSubscriptionPhase>> {
    if rows
        .iter()
        .any(|(_, offer, state, _, _, founding_end, award_offer)| {
            let unawarded_pending_founding = state == "pending"
                && award_offer.is_none()
                && billing_offer(offer)
                    .ok()
                    .and_then(FoundingOffer::from_billing_offer)
                    .is_some();
            !unawarded_pending_founding
                && (award_offer.is_some()
                    || billing_offer(offer)
                        .ok()
                        .and_then(FoundingOffer::from_billing_offer)
                        .is_some())
                && founding_end.is_none()
        })
    {
        return Err(Error::Internal(
            "founding sponsored seat has no verified transition boundary".into(),
        ));
    }

    // Removal and replacement rows are not marked scheduled_removal until the provider accepts
    // their schedule. Apply every still-live operation in memory so a second mutation cannot
    // rebuild Stripe's schedule without an earlier pending change.
    for (beneficiary_id, _, effective_until) in pending_changes {
        for row in &mut rows {
            if row.0 == *beneficiary_id {
                row.4 = *effective_until;
            }
        }
    }

    let mut boundaries = std::collections::BTreeSet::from([now_epoch]);
    for (_, _, _, effective_from, effective_until, founding_end, award_offer) in &rows {
        if *effective_from > now_epoch {
            boundaries.insert(*effective_from);
        }
        if let Some(effective_until) = effective_until.filter(|until| *until > now_epoch) {
            boundaries.insert(effective_until);
        }
        if award_offer.is_some() {
            if let Some(founding_end) = founding_end {
                let founding_end = founding_price_boundary(*founding_end)?;
                if founding_end > now_epoch {
                    boundaries.insert(founding_end);
                }
            }
        }
    }
    let boundaries: Vec<i64> = boundaries.into_iter().collect();
    let mut phases = Vec::with_capacity(boundaries.len());
    for (index, start_at_epoch) in boundaries.iter().copied().enumerate() {
        let end_at_epoch = boundaries.get(index + 1).copied();
        let mut quantities = std::collections::BTreeMap::<String, i64>::new();
        for (_, offer, state, effective_from, effective_until, founding_end, award_offer) in &rows {
            if *effective_from <= start_at_epoch
                && effective_until
                    .map(|effective_until| start_at_epoch < effective_until)
                    .unwrap_or(true)
            {
                let stored_offer = billing_offer(offer)?;
                let founding_offer = award_offer
                    .as_deref()
                    .map(parse_founding_offer)
                    .transpose()?;
                let offer = if *state == "pending"
                    && founding_offer.is_none()
                    && FoundingOffer::from_billing_offer(stored_offer).is_some()
                {
                    stored_offer
                } else {
                    expected_founding_invoice_offer(
                        stored_offer,
                        founding_offer,
                        founding_end.map(founding_price_boundary).transpose()?,
                        start_at_epoch,
                    )
                    .ok_or_else(|| {
                        Error::Internal("founding sponsored seat has no transition boundary".into())
                    })?
                };
                let quantity = quantities.entry(offer.as_str().to_string()).or_default();
                *quantity = quantity
                    .checked_add(1)
                    .ok_or_else(|| Error::BadRequest("sponsored seat count is too large".into()))?;
            }
        }
        let items = quantities
            .into_iter()
            .map(|(offer, quantity)| {
                let offer = billing_offer(&offer)?;
                Ok(SponsoredSubscriptionItemRequest {
                    provider_price_id: catalogue.id_for(offer).to_string(),
                    quantity,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        phases.push(SponsoredSubscriptionPhase {
            start_at_epoch,
            end_at_epoch,
            items,
        });
    }
    Ok(phases)
}

pub(crate) fn founding_price_boundary(value: i64) -> Result<i64> {
    if value < 0 {
        return Err(Error::Internal("founding price boundary is corrupt".into()));
    }
    Ok(value)
}

#[cfg(test)]
fn founding_transition_offer(
    offer: BillingOffer,
    founding_end_epoch: i64,
    phase_start_epoch: i64,
) -> BillingOffer {
    if phase_start_epoch < founding_end_epoch {
        return offer;
    }
    FoundingOffer::from_billing_offer(offer)
        .map(FoundingOffer::standard_billing_offer)
        .unwrap_or(offer)
}

fn expected_founding_invoice_offer(
    stored_offer: BillingOffer,
    founding_offer: Option<FoundingOffer>,
    founding_end_epoch: Option<i64>,
    period_start_epoch: i64,
) -> Option<BillingOffer> {
    match founding_offer.or_else(|| FoundingOffer::from_billing_offer(stored_offer)) {
        Some(founding) => {
            let boundary = founding_end_epoch?;
            Some(if period_start_epoch < boundary {
                founding.billing_offer()
            } else {
                founding.standard_billing_offer()
            })
        }
        None => Some(stored_offer),
    }
}

fn expected_sponsored_invoice_offer(
    stored_offer: BillingOffer,
    founding_offer: Option<FoundingOffer>,
    founding_end_epoch: Option<i64>,
    seat_state: &str,
    period_start_epoch: i64,
) -> Option<BillingOffer> {
    if seat_state == "pending"
        && founding_offer.is_none()
        && FoundingOffer::from_billing_offer(stored_offer).is_some()
    {
        // The first paid invoice is what creates the award boundary. Until it does, a pending
        // founding seat must be matched against the founding price it requested.
        Some(stored_offer)
    } else {
        expected_founding_invoice_offer(
            stored_offer,
            founding_offer,
            founding_end_epoch,
            period_start_epoch,
        )
    }
}

async fn select_founding_aware_offer(
    tx: &mut Transaction<'_, Postgres>,
    beneficiary_id: &str,
    requested_interval: FoundingOffer,
    operation_id: Option<&str>,
    now_epoch: i64,
    transitions_enabled: bool,
) -> std::result::Result<BillingOffer, founding_allocator::FoundingAllocatorError> {
    if !transitions_enabled {
        return Ok(requested_interval.standard_billing_offer());
    }
    founding_allocator::select_offer(
        tx,
        beneficiary_id,
        requested_interval,
        operation_id,
        now_epoch,
    )
    .await
}

fn billing_epoch() -> i64 {
    crate::clock::business_epoch_seconds()
}

fn personal_billing_error(error: personal_billing::PersonalBillingError) -> Error {
    match error {
        personal_billing::PersonalBillingError::AccountExists => {
            Error::Conflict("a personal billing account already exists".into())
        }
        personal_billing::PersonalBillingError::CorruptState => {
            Error::Internal("personal billing state is corrupt".into())
        }
        personal_billing::PersonalBillingError::SettlementConflict => {
            Error::Conflict("personal settlement conflicts with stored evidence".into())
        }
        personal_billing::PersonalBillingError::Database(error) => Error::Db(error),
    }
}

/// `GET /billing/personal/quote` - return a server-selected, short-lived person quote.
async fn personal_quote(
    State(state): State<AppState>,
    user: AuthUser,
    Query(query): Query<PersonalQuoteQuery>,
) -> Result<Json<PersonalQuoteView>> {
    let billing = cloud_sales_config(&state)?;
    let catalogue = billing
        .price_catalogue()
        .ok_or_else(|| Error::NotConfigured("hosted personal billing is not configured".into()))?;
    let requested_offer = billing_offer(&query.offer)?;
    billing
        .validate_cloud_portal_configuration()
        .await
        .map_err(ProviderError::into_error)?;
    let requested_interval = FoundingOffer::for_billing_offer_interval(requested_offer);
    validate_personal_catalogue(billing, catalogue).await?;
    reconcile_expired_personal_checkout_once(&state.pool, billing, Some(&user.user_id))
        .await
        .map_err(|error| Error::Internal(error.to_string()))?;
    let now = billing_epoch();
    let expires = now
        .checked_add(founding_allocator::RESERVATION_SECONDS)
        .ok_or_else(|| Error::Internal("billing clock overflow".into()))?;
    let mut tx = state.pool.begin().await?;
    if let Some(account) = personal_billing::load_account(&mut tx, &user.user_id)
        .await
        .map_err(personal_billing_error)?
    {
        let reusable = personal_quote_account_is_reusable(account.state);
        if !reusable {
            return Err(Error::Conflict(
                "personal billing is already active or pending".into(),
            ));
        }
    }
    let sponsored: Option<String> = sqlx::query_scalar(
        "SELECT allocation_id FROM cloud_provider_allocations \
         WHERE beneficiary_id = $1 AND state IN ('pending', 'active') LIMIT 1",
    )
    .bind(&user.user_id)
    .fetch_optional(&mut *tx)
    .await?;
    if sponsored.is_some() {
        return Err(Error::Conflict(
            "this person already has sponsored hosted coverage".into(),
        ));
    }
    let offer = select_founding_aware_offer(
        &mut tx,
        &user.user_id,
        requested_interval,
        None,
        now,
        billing.founding_schedule_worker_enabled(),
    )
    .await
    .map_err(|error| Error::Internal(error.to_string()))?;
    let amount_pence = offer.expected_amount_pence();
    let interval = offer.expected_interval();
    let founding = FoundingOffer::from_billing_offer(offer);
    let (remaining, founding_term, next_renewal) = if let Some(founding_offer) = founding {
        let status = founding_allocator::quote_status(&mut tx, founding_offer, now)
            .await
            .map_err(|error| Error::Internal(error.to_string()))?;
        (
            Some(status.remaining_places),
            Some(match founding_offer {
                FoundingOffer::Monthly => "for the first 12 monthly billing periods",
                FoundingOffer::Annual => "for the first annual billing period",
            }),
            status.standard_amount_pence,
        )
    } else {
        (None, None, amount_pence)
    };
    tx.rollback().await?;
    Ok(Json(PersonalQuoteView {
        offer: offer.as_str().into(),
        amount_pence,
        currency: "gbp",
        interval: interval.as_str(),
        tax_treatment: "shown_at_checkout",
        quote_version: 1,
        quote_expires_at_epoch: expires,
        founding: founding.is_some(),
        founding_remaining_places: remaining,
        founding_term,
        next_renewal_amount_pence: next_renewal,
    }))
}

/// `POST /billing/personal/checkout` - persist the operation before asking Stripe for a hosted
/// page. The provider result is the creation of the checkout session; coverage waits for the
/// verified paid webhook handled below.
const SAFE_CHECKOUT_IDEMPOTENCY_WINDOW_SECONDS: i64 = 23 * 60 * 60;

#[cfg(feature = "e2e-test-clock")]
fn test_checkout_customer_matches(configured: Option<&str>, stored: Option<&str>) -> bool {
    configured.is_none_or(|customer_id| Some(customer_id) == stored)
}

async fn personal_checkout(
    State(state): State<AppState>,
    user: AuthUser,
    Json(request): Json<PersonalCheckoutRequest>,
) -> Result<Json<PersonalCheckoutView>> {
    let billing = billing_config(&state)?;
    let existing_operation = if billing.cloud_sales_enabled() {
        false
    } else {
        sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM billing_operations \
             WHERE actor_user_id = $1 AND idempotency_key = $2)",
        )
        .bind(&user.user_id)
        .bind(&request.idempotency_key)
        .fetch_one(&state.pool)
        .await?
    };
    ensure_cloud_sales_for_operation(billing.cloud_sales_enabled(), existing_operation)?;
    let catalogue = billing
        .price_catalogue()
        .ok_or_else(|| Error::NotConfigured("hosted personal billing is not configured".into()))?;
    let offer = billing_offer(&request.offer)?;
    if billing.cloud_sales_enabled() {
        billing
            .validate_cloud_portal_configuration()
            .await
            .map_err(ProviderError::into_error)?;
    }
    validate_personal_catalogue(billing, catalogue).await?;
    let price_id = catalogue.id_for(offer).to_string();
    let (success_url, cancel_url) = checkout_return_urls(&billing.return_url);
    #[cfg(feature = "e2e-test-clock")]
    let test_clock_customer_id = crate::clock::test_clock_customer_id();
    billing_operations::validate_return_url(&billing.return_url, &request.return_url)
        .map_err(Error::from)?;
    #[cfg(feature = "e2e-stripe-test-clock")]
    let e2e_customer_id = if let Some(binding) = request.test_clock_customer.as_ref() {
        if std::env::var("SOTTO_E2E_TEST_CLOCK_CHECKOUT_ENABLED")
            .ok()
            .as_deref()
            != Some("1")
        {
            return Err(Error::NotConfigured(
                "test-clock checkout binding is disabled".into(),
            ));
        }
        Some(binding.customer_id.clone())
    } else {
        None
    };
    reconcile_expired_personal_checkout_once(&state.pool, billing, Some(&user.user_id))
        .await
        .map_err(|error| Error::Internal(error.to_string()))?;
    let mut tx = state.pool.begin().await?;
    founding_allocator::lock_capacity(&mut tx)
        .await
        .map_err(|error| Error::Internal(error.to_string()))?;
    billing_transfers::lock_and_check_no_live_transfer(&mut tx, &[&user.user_id], billing_epoch())
        .await
        .map_err(Error::from)?;
    let operation = billing_operations::begin_personal_operation(
        &mut tx,
        &user.user_id,
        &request.idempotency_key,
        offer,
        request.quote_version,
        request.quote_expires_at_epoch,
        &billing.return_url,
        &request.return_url,
    )
    .await
    .map_err(Error::from)?;
    let mut checkout_expires_at_epoch = None;
    let operation = match operation {
        BeginOperation::Created(operation) => {
            let selected_offer = select_founding_aware_offer(
                &mut tx,
                &user.user_id,
                FoundingOffer::for_billing_offer_interval(offer),
                Some(&operation.operation_id),
                billing_epoch(),
                billing.founding_schedule_worker_enabled(),
            )
            .await
            .map_err(|error| Error::Internal(error.to_string()))?;
            if selected_offer != offer {
                return Err(Error::Conflict(
                    "billing quote no longer matches founding availability; refresh the quote"
                        .into(),
                ));
            }
            let expiry = personal_checkout_expiry(request.quote_expires_at_epoch, billing_epoch())?;
            checkout_expires_at_epoch = Some(expiry);
            personal_billing::begin_account(
                &mut tx,
                &user.user_id,
                &operation.operation_id,
                offer,
                expiry,
                billing_epoch(),
            )
            .await
            .map_err(personal_billing_error)?;
            #[cfg(feature = "e2e-test-clock")]
            if let Some(customer_id) = test_clock_customer_id.as_deref() {
                let result = sqlx::query(
                    "UPDATE billing_personal_accounts SET stripe_customer_id = $2, updated_at = now() \
                     WHERE user_id = $1 AND operation_id = $3 AND state = 'pending' \
                       AND (stripe_customer_id IS NULL OR stripe_customer_id = $2)",
                )
                .bind(&user.user_id)
                .bind(customer_id)
                .bind(&operation.operation_id)
                .execute(&mut *tx)
                .await?;
                if result.rows_affected() != 1 {
                    return Err(Error::Conflict(
                        "billing operation is bound to a different test customer".into(),
                    ));
                }
            }
            let operation = billing_operations::record_personal_checkout_request(
                &mut tx,
                &operation.operation_id,
                &price_id,
                &success_url,
                &cancel_url,
            )
            .await
            .map_err(Error::from)?;
            if let Some(founding_offer) = FoundingOffer::from_billing_offer(offer) {
                let reservation = founding_allocator::reserve_with_payer_kind_until(
                    &mut tx,
                    &format!("founding:{}", operation.operation_id),
                    &operation.operation_id,
                    &user.user_id,
                    founding_allocator::FoundingPayerKind::Personal,
                    &user.user_id,
                    founding_offer,
                    request.quote_version,
                    request.quote_expires_at_epoch,
                    expiry,
                    billing_epoch(),
                )
                .await
                .map_err(|error| Error::Conflict(error.to_string()))?;
                if matches!(reservation, founding_allocator::ReservationOutcome::Full) {
                    return Err(Error::Conflict(
                        "founding hosted places are no longer available".into(),
                    ));
                }
            }
            tx.commit().await?;
            operation
        }
        BeginOperation::AlreadyExists(operation)
            if !matches!(operation.state, BillingOperationState::Pending)
                || operation.provider_checkout_url.is_some() =>
        {
            tx.rollback().await?;
            return Ok(Json(PersonalCheckoutView {
                operation_id: operation.operation_id,
                state: operation.state.as_str().into(),
                checkout_url: operation.provider_checkout_url,
            }));
        }
        BeginOperation::AlreadyExists(operation) => {
            // The original provider request may have timed out after Stripe accepted it. Reuse
            // the stored provider idempotency key and ask again only while the operation is still
            // pending and has no recorded checkout URL.
            tx.rollback().await?;
            if billing_epoch().saturating_sub(operation.created_at_epoch)
                >= SAFE_CHECKOUT_IDEMPOTENCY_WINDOW_SECONDS
            {
                if !billing_operations::park_personal_checkout_outside_idempotency_window(
                    &state.pool,
                    &operation.operation_id,
                )
                .await
                .map_err(Error::from)?
                {
                    return Err(Error::Conflict(
                        "billing operation changed while recovery was being reviewed".into(),
                    ));
                }
                let operation = billing_operations::load_operation_for_actor(
                    &state.pool,
                    &operation.operation_id,
                    &user.user_id,
                )
                .await
                .map_err(Error::from)?
                .ok_or_else(|| Error::NotFound("billing operation not found".into()))?;
                return Ok(Json(PersonalCheckoutView {
                    operation_id: operation.operation_id,
                    state: operation.state.as_str().into(),
                    checkout_url: operation.provider_checkout_url,
                }));
            }
            operation
        }
    };
    #[cfg(feature = "e2e-stripe-test-clock")]
    if let Some(customer_id) = e2e_customer_id.as_deref() {
        let mut customer_tx = state.pool.begin().await?;
        let bound = sqlx::query(
            "UPDATE billing_personal_accounts SET stripe_customer_id = $2, updated_at = now() \
             WHERE operation_id = $1 AND state = 'pending' AND stripe_subscription_id IS NULL \
             AND payment_reference IS NULL AND (stripe_customer_id IS NULL OR stripe_customer_id = $2)",
        )
        .bind(&operation.operation_id)
        .bind(customer_id)
        .execute(&mut *customer_tx)
        .await?
        .rows_affected();
        if bound != 1 {
            let existing_customer: Option<String> = sqlx::query_scalar(
                "SELECT stripe_customer_id FROM billing_personal_accounts WHERE operation_id = $1",
            )
            .bind(&operation.operation_id)
            .fetch_optional(&mut *customer_tx)
            .await?
            .flatten();
            if existing_customer.as_deref() != Some(customer_id) {
                return Err(Error::Conflict(
                    "test-clock customer is not available for this checkout".into(),
                ));
            }
        }
        customer_tx.commit().await?;
    }
    let checkout_expires_at_epoch = match checkout_expires_at_epoch {
        Some(expiry) => expiry,
        None => personal_checkout_expiry_for_retry(
            sqlx::query_scalar(
                "SELECT pending_expires_at_epoch FROM billing_personal_accounts WHERE operation_id = $1",
            )
            .bind(&operation.operation_id)
            .fetch_one(&state.pool)
            .await?,
            billing_epoch(),
        )?,
    };
    let (allocation_reference, customer_id): (String, Option<String>) = sqlx::query_as(
        "SELECT allocation_reference, stripe_customer_id FROM billing_personal_accounts WHERE operation_id = $1",
    )
    .bind(&operation.operation_id)
    .fetch_one(&state.pool)
    .await?;
    #[cfg(feature = "e2e-test-clock")]
    if !test_checkout_customer_matches(test_clock_customer_id.as_deref(), customer_id.as_deref()) {
        return Err(Error::Conflict(
            "billing operation is bound to a different test customer".into(),
        ));
    }
    let stored_price_id = operation
        .provider_checkout_price_id
        .as_deref()
        .ok_or_else(|| Error::Conflict("billing operation needs recovery review".into()))?;
    let stored_success_url = operation
        .provider_checkout_success_url
        .as_deref()
        .ok_or_else(|| Error::Conflict("billing operation needs recovery review".into()))?;
    let stored_cancel_url = operation
        .provider_checkout_cancel_url
        .as_deref()
        .ok_or_else(|| Error::Conflict("billing operation needs recovery review".into()))?;
    #[cfg(feature = "e2e-stripe-test-clock")]
    if let Some(binding) = request.test_clock_customer.as_ref() {
        // The caller's quote and pending billing operation are validated before this provider
        // metadata write, so a rejected checkout cannot claim the runner Customer.
        billing
            .provider
            .claim_e2e_test_clock_customer(
                &binding.customer_id,
                &binding.clock_id,
                &binding.run_id,
                &user.user_id,
            )
            .await
            .map_err(ProviderError::into_error)?;
    }
    let checkout = billing
        .provider
        .create_personal_checkout(
            &user.user_id,
            customer_id.as_deref(),
            stored_price_id,
            &operation.operation_id,
            &allocation_reference,
            &operation.provider_idempotency_key,
            checkout_expires_at_epoch,
            stored_success_url,
            stored_cancel_url,
        )
        .await
        .map_err(ProviderError::into_error)?;
    let mut result_tx = state.pool.begin().await?;
    personal_billing::record_checkout_url(
        &mut result_tx,
        &operation.operation_id,
        checkout.session_id.as_deref(),
        &checkout.checkout_url,
    )
    .await
    .map_err(personal_billing_error)?;
    let result = billing_operations::record_provider_result(
        &mut result_tx,
        &operation.operation_id,
        BillingOperationState::Succeeded,
        checkout.session_id.as_deref(),
        Some("checkout_created"),
    )
    .await
    .map_err(Error::from)?;
    result_tx.commit().await?;
    Ok(Json(PersonalCheckoutView {
        operation_id: result.operation_id,
        state: result.state.as_str().into(),
        checkout_url: Some(checkout.checkout_url),
    }))
}

/// Resolve one interrupted personal Checkout creation. The operation's original key and exact
/// stored request facts are reused only inside a conservative 23-hour window; beyond it the
/// operation is parked rather than risking Stripe pruning the idempotency key.
pub async fn recover_personal_billing_operation_once(
    pool: &sqlx::PgPool,
    billing: &BillingState,
) -> std::result::Result<bool, billing_operations::BillingRecoveryError> {
    let mut candidates =
        billing_operations::load_personal_reconciliation_candidates(pool, 1).await?;
    let Some(candidate) = candidates.pop() else {
        return reconcile_expired_personal_checkout_once(pool, billing, None).await;
    };
    let claim_token = format!("billing-recovery:{}", uuid::Uuid::new_v4());
    let operation =
        match billing_operations::claim_reconciliation(pool, &candidate.operation_id, &claim_token)
            .await
        {
            Ok(operation) => operation,
            Err(billing_operations::BillingRecoveryError::InProgress) => return Ok(true),
            Err(error) => return Err(error),
        };
    if !matches!(
        operation.state,
        BillingOperationState::Pending | BillingOperationState::Unknown
    ) {
        return Ok(true);
    }
    let account = sqlx::query(
        "SELECT user_id, allocation_reference, offer, stripe_customer_id, state \
         FROM billing_personal_accounts WHERE operation_id = $1",
    )
    .bind(&operation.operation_id)
    .fetch_optional(pool)
    .await
    .map_err(|_| {
        billing_operations::BillingRecoveryError::Provider("database_read_failed".into())
    })?;
    let Some(account) = account else {
        billing_operations::park_reconciliation_for_review(
            pool,
            &operation.operation_id,
            &claim_token,
            "personal_binding_missing",
        )
        .await?;
        return Ok(true);
    };
    let account_state: String = account.try_get("state").map_err(|_| {
        billing_operations::BillingRecoveryError::Provider("personal_binding_invalid".into())
    })?;
    if account_state == "refund_required" {
        billing_operations::park_reconciliation_for_review(
            pool,
            &operation.operation_id,
            &claim_token,
            "paid_checkout_requires_refund",
        )
        .await
        .map_err(|_| {
            billing_operations::BillingRecoveryError::Provider(
                "recovery_review_update_failed".into(),
            )
        })?;
        return Ok(true);
    }
    if matches!(account_state.as_str(), "active" | "past_due" | "unpaid") {
        let mut tx = pool.begin().await.map_err(|_| {
            billing_operations::BillingRecoveryError::Provider("database_write_failed".into())
        })?;
        billing_operations::record_claimed_provider_result(
            &mut tx,
            &operation.operation_id,
            &claim_token,
            BillingOperationState::Succeeded,
            operation.provider_operation_id.as_deref(),
            Some("checkout_settled"),
        )
        .await?;
        tx.commit().await.map_err(|_| {
            billing_operations::BillingRecoveryError::Provider("database_commit_failed".into())
        })?;
        return Ok(true);
    }
    let now = billing_epoch();
    if now.saturating_sub(operation.created_at_epoch) >= SAFE_CHECKOUT_IDEMPOTENCY_WINDOW_SECONDS {
        billing_operations::park_reconciliation_for_review(
            pool,
            &operation.operation_id,
            &claim_token,
            "provider_idempotency_window_elapsed",
        )
        .await?;
        return Ok(true);
    }
    let Some(price_id) = operation.provider_checkout_price_id.as_deref() else {
        billing_operations::park_reconciliation_for_review(
            pool,
            &operation.operation_id,
            &claim_token,
            "checkout_price_missing",
        )
        .await?;
        return Ok(true);
    };
    let (Some(success_url), Some(cancel_url)) = (
        operation.provider_checkout_success_url.as_deref(),
        operation.provider_checkout_cancel_url.as_deref(),
    ) else {
        billing_operations::park_reconciliation_for_review(
            pool,
            &operation.operation_id,
            &claim_token,
            "checkout_return_urls_missing",
        )
        .await?;
        return Ok(true);
    };
    let user_id: String = account.try_get("user_id").map_err(|_| {
        billing_operations::BillingRecoveryError::Provider("personal_binding_invalid".into())
    })?;
    let allocation_reference: String = account.try_get("allocation_reference").map_err(|_| {
        billing_operations::BillingRecoveryError::Provider("personal_binding_invalid".into())
    })?;
    let customer_id: Option<String> = account.try_get("stripe_customer_id").map_err(|_| {
        billing_operations::BillingRecoveryError::Provider("personal_binding_invalid".into())
    })?;
    let expires_at_epoch: i64 = sqlx::query_scalar(
        "SELECT pending_expires_at_epoch FROM billing_personal_accounts WHERE operation_id = $1",
    )
    .bind(&operation.operation_id)
    .fetch_one(pool)
    .await
    .map_err(|_| {
        billing_operations::BillingRecoveryError::Provider("personal_binding_invalid".into())
    })?;
    let result = billing
        .provider
        .create_personal_checkout(
            &user_id,
            customer_id.as_deref(),
            price_id,
            &operation.operation_id,
            &allocation_reference,
            &operation.provider_idempotency_key,
            expires_at_epoch,
            success_url,
            cancel_url,
        )
        .await;
    match result {
        Ok(session) => {
            if let Some(session_id) = session.session_id.as_deref() {
                let mut tx = pool.begin().await.map_err(|_| {
                    billing_operations::BillingRecoveryError::Provider(
                        "database_write_failed".into(),
                    )
                })?;
                billing_operations::record_claimed_checkout_result(
                    &mut tx,
                    &operation.operation_id,
                    &claim_token,
                    session_id,
                    &session.checkout_url,
                )
                .await?;
                tx.commit().await.map_err(|_| {
                    billing_operations::BillingRecoveryError::Provider(
                        "database_commit_failed".into(),
                    )
                })?;
            } else {
                billing_operations::park_reconciliation_for_review(
                    pool,
                    &operation.operation_id,
                    &claim_token,
                    "provider_session_id_missing",
                )
                .await?;
            }
        }
        Err(error)
            if provider_failure_is_definitive(&error)
                && definitive_checkout_status(error.status) =>
        {
            let mut tx = pool.begin().await.map_err(|_| {
                billing_operations::BillingRecoveryError::Provider("database_write_failed".into())
            })?;
            founding_allocator::release_reservation(&mut tx, &operation.operation_id)
                .await
                .map_err(|_| {
                    billing_operations::BillingRecoveryError::Provider(
                        "reservation_release_failed".into(),
                    )
                })?;
            billing_operations::record_claimed_provider_result(
                &mut tx,
                &operation.operation_id,
                &claim_token,
                BillingOperationState::Failed,
                None,
                Some("provider_rejected"),
            )
            .await?;
            sqlx::query(
                "UPDATE billing_personal_accounts SET state='canceled', updated_at=now() \
                 WHERE operation_id=$1 AND state='pending'",
            )
            .bind(&operation.operation_id)
            .execute(&mut *tx)
            .await
            .map_err(|_| {
                billing_operations::BillingRecoveryError::Provider(
                    "personal_account_update_failed".into(),
                )
            })?;
            tx.commit().await.map_err(|_| {
                billing_operations::BillingRecoveryError::Provider("database_commit_failed".into())
            })?;
        }
        Err(error) if matches!(error.kind, ProviderErrorKind::Authentication) => {
            billing_operations::park_reconciliation_for_review(
                pool,
                &operation.operation_id,
                &claim_token,
                "provider_credentials_unavailable",
            )
            .await?;
        }
        Err(_) => {
            billing_operations::release_reconciliation_failure(
                pool,
                &operation.operation_id,
                &claim_token,
                "provider_outcome_uncertain",
            )
            .await?;
        }
    }
    Ok(true)
}

/// Resolve one sponsored provider operation from its immutable request record. Provider writes
/// are never made while a database transaction is open.
pub async fn recover_sponsored_billing_operation_once(
    pool: &sqlx::PgPool,
    billing: &BillingState,
) -> Result<bool> {
    if sponsored_billing::park_unprepared_legacy_operations(pool, 8).await? > 0 {
        return Ok(true);
    }
    let Some(operation_id) = sponsored_billing::next_recovery_candidate(pool).await? else {
        return Ok(false);
    };
    recover_sponsored_billing_operation_by_id(pool, billing, &operation_id).await?;
    Ok(true)
}

#[doc(hidden)]
pub async fn recover_sponsored_billing_operation_by_id(
    pool: &sqlx::PgPool,
    billing: &BillingState,
    operation_id: &str,
) -> Result<()> {
    let lease_token = format!("sponsored-recovery:{}", uuid::Uuid::new_v4());
    let Some(operation) = sponsored_billing::claim_recovery(pool, operation_id, &lease_token)
        .await
        .map_err(Error::from)?
    else {
        return Ok(());
    };
    let Some(request_value) = operation.provider_request.clone() else {
        sponsored_billing::park_recovery_for_review(
            pool,
            &operation.operation_id,
            &lease_token,
            "provider_request_missing",
        )
        .await
        .map_err(Error::from)?;
        return Ok(());
    };
    if operation.provider_request_version != Some(1) {
        sponsored_billing::park_recovery_for_review(
            pool,
            &operation.operation_id,
            &lease_token,
            "provider_request_version_unsupported",
        )
        .await
        .map_err(Error::from)?;
        return Ok(());
    }
    let request: sponsored_billing::SponsoredProviderRequest =
        match serde_json::from_value(request_value) {
            Ok(request) => request,
            Err(_) => {
                sponsored_billing::park_recovery_for_review(
                    pool,
                    &operation.operation_id,
                    &lease_token,
                    "provider_request_invalid",
                )
                .await
                .map_err(Error::from)?;
                return Ok(());
            }
        };
    let now = sponsored_billing::current_epoch();
    let first_sent = operation.provider_first_sent_epoch.unwrap_or(now);
    let horizon_elapsed =
        now.saturating_sub(first_sent) >= sponsored_billing::MAX_PROVIDER_REPLAY_SECONDS;

    match request {
        sponsored_billing::SponsoredProviderRequest::Checkout {
            organization_id,
            customer_id,
            items,
            expires_at_epoch,
            success_url,
            cancel_url,
        } => {
            if organization_id != operation.organization_id || items.is_empty() {
                sponsored_billing::park_recovery_for_review(
                    pool,
                    &operation.operation_id,
                    &lease_token,
                    "provider_request_binding_invalid",
                )
                .await
                .map_err(Error::from)?;
                return Ok(());
            }
            if let Some(session_id) = operation.provider_checkout_session_id.as_deref() {
                match billing
                    .provider
                    .get_sponsored_checkout_status(session_id, &operation.operation_id)
                    .await
                {
                    Ok(PersonalCheckoutStatus::ExpiredUnpaid) => {
                        let mut tx = pool.begin().await?;
                        founding_allocator::lock_capacity(&mut tx)
                            .await
                            .map_err(|error| Error::Internal(error.to_string()))?;
                        sponsored_billing::cancel_claimed_expired_checkout(
                            &mut tx,
                            &operation.operation_id,
                            &lease_token,
                        )
                        .await
                        .map_err(Error::from)?;
                        tx.commit().await?;
                    }
                    Ok(PersonalCheckoutStatus::Open | PersonalCheckoutStatus::Complete) => {
                        if horizon_elapsed {
                            sponsored_billing::park_recovery_for_review(
                                pool,
                                &operation.operation_id,
                                &lease_token,
                                "checkout_settlement_unresolved",
                            )
                            .await
                            .map_err(Error::from)?;
                        } else {
                            sponsored_billing::defer_recovery(
                                pool,
                                &operation.operation_id,
                                &lease_token,
                                if now < expires_at_epoch {
                                    expires_at_epoch
                                } else {
                                    now.saturating_add(300)
                                },
                            )
                            .await
                            .map_err(Error::from)?;
                        }
                    }
                    Err(error) => {
                        sponsored_billing::record_recovery_failure(
                            pool,
                            &operation.operation_id,
                            &lease_token,
                            sponsored_recovery_error_code(&error),
                        )
                        .await
                        .map_err(Error::from)?;
                    }
                }
                return Ok(());
            }
            if horizon_elapsed {
                sponsored_billing::park_recovery_for_review(
                    pool,
                    &operation.operation_id,
                    &lease_token,
                    "provider_idempotency_window_elapsed",
                )
                .await
                .map_err(Error::from)?;
                return Ok(());
            }
            match billing
                .provider
                .create_sponsored_checkout(
                    &organization_id,
                    customer_id.as_deref(),
                    &items,
                    &operation.operation_id,
                    &operation.provider_idempotency_key,
                    expires_at_epoch,
                    &success_url,
                    &cancel_url,
                )
                .await
            {
                Ok(session) => {
                    let mut tx = pool.begin().await?;
                    sponsored_billing::record_claimed_checkout(
                        &mut tx,
                        &operation.operation_id,
                        &lease_token,
                        &session.checkout_url,
                        &session.session_id,
                        session.customer_id.as_deref(),
                        session.subscription_id.as_deref(),
                    )
                    .await
                    .map_err(Error::from)?;
                    tx.commit().await?;
                }
                Err(error) => {
                    record_sponsored_provider_failure(pool, &operation, &lease_token, &error)
                        .await?;
                }
            }
        }
        sponsored_billing::SponsoredProviderRequest::ScheduleUpdate {
            organization_id,
            subscription_id,
            expected_period_start,
            expected_period_end,
            phases,
        } => {
            if organization_id != operation.organization_id
                || Some(&subscription_id) != operation.provider_subscription_id.as_ref()
                || phases.is_empty()
            {
                sponsored_billing::park_recovery_for_review(
                    pool,
                    &operation.operation_id,
                    &lease_token,
                    "provider_request_binding_invalid",
                )
                .await
                .map_err(Error::from)?;
                return Ok(());
            }
            with_sponsored_subscription_write_lock(pool, &subscription_id, async {
                match billing
                    .provider
                    .verify_sponsored_subscription_update(
                        &subscription_id,
                        expected_period_start,
                        expected_period_end,
                        &phases,
                    )
                    .await
                {
                    Ok(Some(snapshot)) => {
                        record_claimed_sponsored_update(pool, &operation, &lease_token, &snapshot)
                            .await?;
                        return Ok(());
                    }
                    Ok(None) => {}
                    Err(error) => {
                        sponsored_billing::record_recovery_failure(
                            pool,
                            &operation.operation_id,
                            &lease_token,
                            sponsored_recovery_error_code(&error),
                        )
                        .await
                        .map_err(Error::from)?;
                        return Ok(());
                    }
                }
                let snapshot = match billing
                    .provider
                    .get_sponsored_subscription(&subscription_id)
                    .await
                {
                    Ok(snapshot) if snapshot.subscription_id == subscription_id => snapshot,
                    Ok(_) => {
                        sponsored_billing::park_recovery_for_review(
                            pool,
                            &operation.operation_id,
                            &lease_token,
                            "subscription_binding_changed",
                        )
                        .await
                        .map_err(Error::from)?;
                        return Ok(());
                    }
                    Err(error) => {
                        record_sponsored_provider_failure(pool, &operation, &lease_token, &error)
                            .await?;
                        return Ok(());
                    }
                };
                let Some(current_period_start) = snapshot.current_period_start else {
                    sponsored_billing::park_recovery_for_review(
                        pool,
                        &operation.operation_id,
                        &lease_token,
                        "subscription_period_changed",
                    )
                    .await
                    .map_err(Error::from)?;
                    return Ok(());
                };
                let Some(current_period_end) = snapshot.current_period_end else {
                    sponsored_billing::park_recovery_for_review(
                        pool,
                        &operation.operation_id,
                        &lease_token,
                        "subscription_period_changed",
                    )
                    .await
                    .map_err(Error::from)?;
                    return Ok(());
                };
                let Some(catalogue) = billing.price_catalogue() else {
                    sponsored_billing::park_recovery_for_review(
                        pool,
                        &operation.operation_id,
                        &lease_token,
                        "price_catalogue_missing",
                    )
                    .await
                    .map_err(Error::from)?;
                    return Ok(());
                };
                let current_phases = match sponsored_provider_phases(
                    pool,
                    &organization_id,
                    catalogue,
                    sponsored_billing::current_epoch(),
                )
                .await
                {
                    Ok(phases) if !phases.is_empty() => phases,
                    Ok(_) | Err(_) => {
                        sponsored_billing::park_recovery_for_review(
                            pool,
                            &operation.operation_id,
                            &lease_token,
                            "local_schedule_evidence_invalid",
                        )
                        .await
                        .map_err(Error::from)?;
                        return Ok(());
                    }
                };
                let Some(current_schedule) =
                    normalized_sponsored_phases_at(&current_phases, current_period_end)
                else {
                    sponsored_billing::park_recovery_for_review(
                        pool,
                        &operation.operation_id,
                        &lease_token,
                        "local_schedule_evidence_invalid",
                    )
                    .await
                    .map_err(Error::from)?;
                    return Ok(());
                };
                let Some(saved_schedule) =
                    normalized_sponsored_phases_at(&phases, current_period_end)
                else {
                    sponsored_billing::park_recovery_for_review(
                        pool,
                        &operation.operation_id,
                        &lease_token,
                        "provider_request_invalid",
                    )
                    .await
                    .map_err(Error::from)?;
                    return Ok(());
                };
                if current_schedule != saved_schedule {
                    match billing
                        .provider
                        .verify_sponsored_subscription_update(
                            &subscription_id,
                            current_period_start,
                            current_period_end,
                            &current_phases,
                        )
                        .await
                    {
                        Ok(Some(snapshot)) => {
                            record_claimed_sponsored_update(
                                pool,
                                &operation,
                                &lease_token,
                                &snapshot,
                            )
                            .await?;
                        }
                        Ok(None) if horizon_elapsed => {
                            sponsored_billing::park_recovery_for_review(
                                pool,
                                &operation.operation_id,
                                &lease_token,
                                "superseding_schedule_not_confirmed",
                            )
                            .await
                            .map_err(Error::from)?;
                        }
                        Ok(None) => {
                            sponsored_billing::defer_recovery(
                                pool,
                                &operation.operation_id,
                                &lease_token,
                                sponsored_billing::current_epoch().saturating_add(300),
                            )
                            .await
                            .map_err(Error::from)?;
                        }
                        Err(error) => {
                            sponsored_billing::record_recovery_failure(
                                pool,
                                &operation.operation_id,
                                &lease_token,
                                sponsored_recovery_error_code(&error),
                            )
                            .await
                            .map_err(Error::from)?;
                        }
                    }
                    return Ok(());
                }
                if current_period_start != expected_period_start
                    || current_period_end != expected_period_end
                {
                    sponsored_billing::park_recovery_for_review(
                        pool,
                        &operation.operation_id,
                        &lease_token,
                        "subscription_period_changed",
                    )
                    .await
                    .map_err(Error::from)?;
                    return Ok(());
                }
                if horizon_elapsed {
                    sponsored_billing::park_recovery_for_review(
                        pool,
                        &operation.operation_id,
                        &lease_token,
                        "provider_idempotency_window_elapsed",
                    )
                    .await
                    .map_err(Error::from)?;
                    return Ok(());
                }
                match billing
                    .provider
                    .update_sponsored_subscription(
                        &organization_id,
                        &subscription_id,
                        &phases,
                        &operation.provider_idempotency_key,
                    )
                    .await
                {
                    Ok(snapshot) => {
                        record_claimed_sponsored_update(pool, &operation, &lease_token, &snapshot)
                            .await?;
                    }
                    Err(error) => {
                        record_sponsored_provider_failure(pool, &operation, &lease_token, &error)
                            .await?;
                    }
                }
                Ok(())
            })
            .await?;
        }
    }
    Ok(())
}

pub(crate) async fn with_sponsored_subscription_write_lock<T, F>(
    pool: &sqlx::PgPool,
    subscription_id: &str,
    operation: F,
) -> Result<T>
where
    F: std::future::Future<Output = Result<T>>,
{
    let lock_name = format!("sponsored-subscription-write:{subscription_id}");
    let mut connection = pool.acquire().await?;
    // Closing this dedicated connection on drop also releases the session lock if the task is
    // cancelled while a bounded provider request is in flight.
    connection.close_on_drop();
    sqlx::query("SELECT pg_advisory_lock(hashtextextended($1, 0))")
        .bind(&lock_name)
        .execute(&mut *connection)
        .await?;

    let result = operation.await;
    let unlocked =
        sqlx::query_scalar::<_, bool>("SELECT pg_advisory_unlock(hashtextextended($1, 0))")
            .bind(&lock_name)
            .fetch_one(&mut *connection)
            .await;
    match (result, unlocked) {
        (Err(error), _) => Err(error),
        (Ok(_), Ok(false)) => Err(Error::Internal(
            "sponsored subscription write lock was not held".into(),
        )),
        (Ok(_), Err(error)) => Err(error.into()),
        (Ok(value), Ok(true)) => Ok(value),
    }
}

fn normalized_sponsored_phases_at(
    phases: &[SponsoredSubscriptionPhase],
    boundary: i64,
) -> Option<Vec<SponsoredSubscriptionPhase>> {
    let phase_at_boundary = phases
        .iter()
        .find(|phase| {
            phase.start_at_epoch <= boundary && phase.end_at_epoch.is_none_or(|end| boundary < end)
        })
        .or_else(|| phases.iter().find(|phase| phase.start_at_epoch == boundary))?;
    let mut normalized = vec![SponsoredSubscriptionPhase {
        start_at_epoch: boundary,
        end_at_epoch: phase_at_boundary.end_at_epoch,
        items: phase_at_boundary.items.clone(),
    }];
    normalized.extend(
        phases
            .iter()
            .filter(|phase| phase.start_at_epoch > boundary)
            .cloned(),
    );
    if normalized
        .last()
        .is_some_and(|phase| phase.items.is_empty())
    {
        normalized.pop();
    }
    Some(normalized)
}

async fn record_claimed_sponsored_update(
    pool: &sqlx::PgPool,
    operation: &sponsored_billing::SponsoredOperation,
    lease_token: &str,
    snapshot: &SponsoredSubscriptionSnapshot,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    founding_allocator::lock_capacity(&mut tx)
        .await
        .map_err(|error| Error::Internal(error.to_string()))?;
    sponsored_billing::record_claimed_provider_update(
        &mut tx,
        &operation.operation_id,
        lease_token,
        snapshot
            .customer_id
            .as_deref()
            .or(operation.provider_customer_id.as_deref()),
        &snapshot.subscription_id,
        snapshot.schedule_id.as_deref(),
        snapshot.current_period_end,
        None,
    )
    .await
    .map_err(Error::from)?;
    crate::notifications::enqueue_sponsorship_change_in_tx(&mut tx, &operation.operation_id)
        .await
        .map_err(notification_error)?;
    tx.commit().await?;
    Ok(())
}

async fn record_sponsored_provider_failure(
    pool: &sqlx::PgPool,
    operation: &sponsored_billing::SponsoredOperation,
    lease_token: &str,
    error: &ProviderError,
) -> Result<()> {
    if operation.state == "pending"
        && operation.recovery_attempt_count == 0
        && provider_failure_is_definitive(error)
    {
        let mut tx = pool.begin().await?;
        founding_allocator::lock_capacity(&mut tx)
            .await
            .map_err(|failure| Error::Internal(failure.to_string()))?;
        sponsored_billing::cancel_claimed_rejected_operation(
            &mut tx,
            &operation.operation_id,
            lease_token,
            "provider_rejected",
        )
        .await
        .map_err(Error::from)?;
        tx.commit().await?;
    } else {
        sponsored_billing::record_recovery_failure(
            pool,
            &operation.operation_id,
            lease_token,
            sponsored_recovery_error_code(error),
        )
        .await
        .map_err(Error::from)?;
    }
    Ok(())
}

fn sponsored_recovery_error_code(error: &ProviderError) -> &'static str {
    match error.kind {
        ProviderErrorKind::Authentication => "provider_authentication",
        ProviderErrorKind::ResourceMissing => "provider_resource_missing",
        ProviderErrorKind::Retryable => "provider_retryable",
        ProviderErrorKind::Unknown => "provider_unknown",
    }
}

/// Reconcile an expired local pending account against Stripe's current session state. This also
/// repairs accounts whose signed expiry event was acknowledged by an older server or exhausted
/// Stripe's webhook retry window.
async fn reconcile_expired_personal_checkout_once(
    pool: &sqlx::PgPool,
    billing: &BillingState,
    user_id: Option<&str>,
) -> std::result::Result<bool, billing_operations::BillingRecoveryError> {
    let lease_token = format!("billing-expiry-recovery:{}", uuid::Uuid::new_v4());
    let Some(candidate) =
        billing_operations::claim_expired_personal_checkout(pool, &lease_token, user_id).await?
    else {
        return Ok(false);
    };
    let status = billing
        .provider
        .get_personal_checkout_status(&candidate.session_id, &candidate.operation_id)
        .await;
    match status {
        Ok(PersonalCheckoutStatus::ExpiredUnpaid) => {
            let mut tx = pool.begin().await.map_err(|_| {
                billing_operations::BillingRecoveryError::Provider("database_write_failed".into())
            })?;
            founding_allocator::lock_capacity(&mut tx)
                .await
                .map_err(|_| {
                    billing_operations::BillingRecoveryError::Provider(
                        "capacity_lock_failed".into(),
                    )
                })?;
            let account = sqlx::query(
                "SELECT state, pending_expires_at_epoch, user_id \
                 FROM billing_personal_accounts WHERE operation_id = $1 FOR UPDATE",
            )
            .bind(&candidate.operation_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|_| {
                billing_operations::BillingRecoveryError::Provider("database_read_failed".into())
            })?;
            let Some(account) = account else {
                tx.rollback().await.ok();
                billing_operations::release_expired_personal_checkout_check(
                    pool,
                    &candidate.operation_id,
                    &candidate.lease_token,
                )
                .await?;
                return Ok(true);
            };
            let state: String = account.try_get("state").map_err(|_| {
                billing_operations::BillingRecoveryError::Provider(
                    "personal_binding_invalid".into(),
                )
            })?;
            let expires_at: i64 = account.try_get("pending_expires_at_epoch").map_err(|_| {
                billing_operations::BillingRecoveryError::Provider(
                    "personal_binding_invalid".into(),
                )
            })?;
            let user_id: String = account.try_get("user_id").map_err(|_| {
                billing_operations::BillingRecoveryError::Provider(
                    "personal_binding_invalid".into(),
                )
            })?;
            if state != "pending" || expires_at > billing_epoch() {
                tx.rollback().await.ok();
                billing_operations::release_expired_personal_checkout_check(
                    pool,
                    &candidate.operation_id,
                    &candidate.lease_token,
                )
                .await?;
                return Ok(true);
            }
            let operation = billing_operations::record_checkout_expired(
                &mut tx,
                &candidate.operation_id,
                &candidate.session_id,
            )
            .await
            .map_err(|_| {
                billing_operations::BillingRecoveryError::Provider("operation_update_failed".into())
            })?;
            sqlx::query(
                "UPDATE billing_personal_accounts SET state='canceled', updated_at=now() \
                 WHERE operation_id=$1 AND state='pending'",
            )
            .bind(&candidate.operation_id)
            .execute(&mut *tx)
            .await
            .map_err(|_| {
                billing_operations::BillingRecoveryError::Provider(
                    "personal_account_update_failed".into(),
                )
            })?;
            founding_allocator::release_reservation(&mut tx, &candidate.operation_id)
                .await
                .map_err(|_| {
                    billing_operations::BillingRecoveryError::Provider(
                        "reservation_release_failed".into(),
                    )
                })?;
            crate::notifications::enqueue_checkout_outcome_in_tx(
                &mut tx,
                &user_id,
                &candidate.operation_id,
                crate::notifications::CheckoutOutcome::Expired,
            )
            .await
            .map_err(|_| {
                billing_operations::BillingRecoveryError::Provider(
                    "notification_enqueue_failed".into(),
                )
            })?;
            tx.commit().await.map_err(|_| {
                billing_operations::BillingRecoveryError::Provider("database_commit_failed".into())
            })?;
            let _ = operation;
        }
        Ok(PersonalCheckoutStatus::Open | PersonalCheckoutStatus::Complete) => {
            billing_operations::release_expired_personal_checkout_check(
                pool,
                &candidate.operation_id,
                &candidate.lease_token,
            )
            .await?;
        }
        Err(_) => {
            billing_operations::release_expired_personal_checkout_check(
                pool,
                &candidate.operation_id,
                &candidate.lease_token,
            )
            .await?;
        }
    }
    Ok(true)
}

fn personal_checkout_expiry(quote_expires_at_epoch: i64, now_epoch: i64) -> Result<i64> {
    let latest_quote_expiry = now_epoch
        .checked_add(founding_allocator::RESERVATION_SECONDS)
        .ok_or_else(|| Error::Internal("billing clock overflow".into()))?;
    if quote_expires_at_epoch <= now_epoch {
        return Err(Error::Conflict("billing quote expired".into()));
    }
    if quote_expires_at_epoch > latest_quote_expiry {
        return Err(Error::BadRequest("billing quote expiry is invalid".into()));
    }
    quote_expires_at_epoch
        .checked_add(founding_allocator::RESERVATION_SECONDS)
        .ok_or_else(|| Error::Internal("billing clock overflow".into()))
}

fn personal_quote_account_is_reusable(state: personal_billing::PersonalBillingState) -> bool {
    matches!(state, personal_billing::PersonalBillingState::Canceled)
}

fn personal_checkout_expiry_for_retry(pending_expiry_epoch: i64, now_epoch: i64) -> Result<i64> {
    if pending_expiry_epoch <= now_epoch {
        return Err(Error::Conflict(
            "personal Checkout has expired; wait for its signed expiry event".into(),
        ));
    }
    Ok(pending_expiry_epoch)
}

fn definitive_checkout_status(status: Option<u16>) -> bool {
    matches!(status, Some(400 | 404))
}

async fn personal_operation(
    State(state): State<AppState>,
    user: AuthUser,
    Path(operation_id): Path<String>,
) -> Result<Json<PersonalOperationView>> {
    let operation =
        billing_operations::load_operation_for_actor(&state.pool, &operation_id, &user.user_id)
            .await
            .map_err(Error::from)?
            .ok_or_else(|| Error::NotFound("billing operation not found".into()))?;
    Ok(Json(PersonalOperationView {
        operation_id: operation.operation_id,
        offer: operation.offer,
        state: operation.state.as_str().into(),
        provider_operation_id: operation.provider_operation_id,
        checkout_url: operation.provider_checkout_url,
    }))
}

#[derive(Debug, Serialize)]
struct PersonalLifecycleView {
    state: String,
    stripe_subscription_id: Option<String>,
    paid_through_date: Option<String>,
    cancel_at_period_end: bool,
    portal_url: Option<String>,
    founding_price_end_epoch: Option<i64>,
    standard_renewal_amount_pence: Option<i64>,
    founding_transition_status: Option<String>,
    founding_transition_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PersonalCancelRequest {
    idempotency_key: String,
}

#[derive(Debug, Deserialize)]
struct PersonalRefundRequest {
    reason: String,
    amount_pence: Option<i64>,
    full_refund: bool,
    idempotency_key: String,
}

#[derive(Debug, Deserialize)]
struct PersonalRefundEarlyConfirmation {
    effective_at_epoch: i64,
}

#[derive(Debug, Deserialize)]
struct BillingRefundReviewRequest {
    approve: bool,
    result_code: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BillingRefundResolutionRequest {
    provider_refund_id: String,
}

#[derive(Debug, Serialize)]
struct PersonalRefundView {
    request_id: String,
    state: String,
    reason: String,
    amount_pence: Option<i64>,
    full_refund_requested: bool,
    preserve_paid_term: bool,
    early_termination_confirmed_at_epoch: Option<i64>,
    effective_at_epoch: Option<i64>,
    result_code: Option<String>,
}

fn correction_reason(value: &str) -> Result<CorrectionReason> {
    match value {
        "duplicate_charge" => Ok(CorrectionReason::DuplicateCharge),
        "billing_error" => Ok(CorrectionReason::BillingError),
        "accidental_renewal" => Ok(CorrectionReason::AccidentalRenewal),
        "legal_requirement" => Ok(CorrectionReason::LegalRequirement),
        _ => Err(Error::BadRequest(
            "unsupported billing correction reason".into(),
        )),
    }
}

fn personal_refund_view(request: billing_refunds::BillingRefundRequest) -> PersonalRefundView {
    PersonalRefundView {
        request_id: request.request_id,
        state: request.state.as_str().into(),
        reason: request.reason,
        amount_pence: request.amount_pence,
        full_refund_requested: request.full_refund_requested,
        preserve_paid_term: request.preserve_paid_term,
        early_termination_confirmed_at_epoch: request.early_termination_confirmed_at_epoch,
        effective_at_epoch: request.effective_at_epoch,
        result_code: request.result_code,
    }
}

fn require_billing_corrections() -> Result<()> {
    if billing_corrections_enabled() {
        Ok(())
    } else {
        Err(Error::NotConfigured(
            "billing corrections are not enabled".into(),
        ))
    }
}

fn require_billing_operator(headers: &HeaderMap) -> Result<()> {
    let configured = std::env::var(BILLING_OPERATOR_TOKEN_ENV)
        .ok()
        .filter(|token| !token.trim().is_empty())
        .ok_or_else(|| {
            Error::NotConfigured("billing operator controls are not configured".into())
        })?;
    let provided = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    if provided != Some(configured.as_str()) {
        return Err(Error::Forbidden(
            "billing operator authorisation failed".into(),
        ));
    }
    Ok(())
}

async fn personal_refund_request(
    State(state): State<AppState>,
    user: AuthUser,
    Json(request): Json<PersonalRefundRequest>,
) -> Result<Json<PersonalRefundView>> {
    require_billing_corrections()?;
    billing_config(&state)?;
    if request.idempotency_key.trim().is_empty() {
        return Err(Error::BadRequest(
            "idempotency_key must not be empty".into(),
        ));
    }
    let reason = correction_reason(&request.reason)?;
    let mut tx = state.pool.begin().await?;
    billing_transfers::lock_and_check_no_live_transfer(&mut tx, &[&user.user_id], billing_epoch())
        .await
        .map_err(Error::from)?;
    let account = personal_billing::load_account(&mut tx, &user.user_id)
        .await
        .map_err(personal_billing_error)?
        .ok_or_else(|| Error::NotFound("personal billing account not found".into()))?;
    let payment_reference = account.payment_reference.ok_or_else(|| {
        Error::Conflict("personal billing has no settled payment to correct".into())
    })?;
    let subscription_id = account.stripe_subscription_id.ok_or_else(|| {
        Error::Conflict("personal billing has no settled subscription to correct".into())
    })?;
    let (_, correction) = billing_refunds::create_request(
        &mut tx,
        &billing_refunds::CorrectionRequest {
            requester_user_id: user.user_id.clone(),
            beneficiary_id: user.user_id,
            organization_id: None,
            payer_kind: PayerKind::Personal,
            payment_reference,
            subscription_id,
            amount_pence: request.amount_pence,
            reason,
            policy_version: BILLING_CORRECTION_POLICY_VERSION.into(),
            idempotency_key: request.idempotency_key,
            full_refund_requested: request.full_refund,
        },
    )
    .await
    .map_err(Error::from)?;
    tx.commit().await?;
    Ok(Json(personal_refund_view(correction)))
}

async fn personal_refund_status(
    State(state): State<AppState>,
    user: AuthUser,
    Path(request_id): Path<String>,
) -> Result<Json<PersonalRefundView>> {
    require_billing_corrections()?;
    let mut tx = state.pool.begin().await?;
    let request = billing_refunds::load_for_requester(&mut tx, &request_id, &user.user_id)
        .await
        .map_err(Error::from)?
        .ok_or_else(|| Error::NotFound("billing correction request not found".into()))?;
    tx.rollback().await?;
    Ok(Json(personal_refund_view(request)))
}

async fn personal_refund_confirm_early(
    State(state): State<AppState>,
    user: AuthUser,
    Path(request_id): Path<String>,
    Json(request): Json<PersonalRefundEarlyConfirmation>,
) -> Result<Json<PersonalRefundView>> {
    require_billing_corrections()?;
    let mut tx = state.pool.begin().await?;
    billing_transfers::lock_and_check_no_live_transfer(&mut tx, &[&user.user_id], billing_epoch())
        .await
        .map_err(Error::from)?;
    let request = billing_refunds::confirm_early_termination(
        &mut tx,
        &request_id,
        &user.user_id,
        request.effective_at_epoch,
    )
    .await
    .map_err(Error::from)?;
    tx.commit().await?;
    Ok(Json(personal_refund_view(request)))
}

async fn operator_review_refund(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(request_id): Path<String>,
    Json(review): Json<BillingRefundReviewRequest>,
) -> Result<Json<PersonalRefundView>> {
    require_billing_corrections()?;
    require_billing_operator(&headers)?;
    let mut tx = state.pool.begin().await?;
    let request = billing_refunds::review_request(
        &mut tx,
        &request_id,
        review.approve,
        review.result_code.as_deref(),
    )
    .await
    .map_err(Error::from)?;
    tx.commit().await?;
    Ok(Json(personal_refund_view(request)))
}

async fn operator_resolve_refund(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(request_id): Path<String>,
    Json(resolution): Json<BillingRefundResolutionRequest>,
) -> Result<Json<PersonalRefundView>> {
    require_billing_corrections()?;
    require_billing_operator(&headers)?;
    let billing = billing_config(&state)?;
    let mut tx = state.pool.begin().await?;
    let current = billing_refunds::load_for_operator(&mut tx, &request_id)
        .await
        .map_err(Error::from)?;
    if current.state != billing_refunds::CorrectionState::Unknown
        || current.provider_refund_id.is_some()
        || current.result_code.as_deref() != Some("refund_create_horizon_expired")
    {
        return Err(Error::Conflict(
            "billing refund is not awaiting ambiguous-create resolution".into(),
        ));
    }
    tx.rollback().await?;

    let receipt = billing
        .provider
        .get_refund(&resolution.provider_refund_id)
        .await
        .map_err(ProviderError::into_error)?;
    if !validate_refund_receipt(&*billing.provider, &current, &receipt).await? {
        return Err(Error::Conflict(
            "provider refund amount does not match the approved correction".into(),
        ));
    }

    let mut tx = state.pool.begin().await?;
    let request = match receipt.status.as_str() {
        "succeeded" => billing_refunds::record_provider_refund(
            &mut tx,
            &request_id,
            &receipt.refund_id,
            true,
            None,
        )
        .await
        .map_err(Error::from)?,
        "pending" | "requires_action" => {
            billing_refunds::record_provider_pending(&mut tx, &request_id, &receipt.refund_id)
                .await
                .map_err(Error::from)?
        }
        status @ ("failed" | "canceled") => billing_refunds::record_provider_refund(
            &mut tx,
            &request_id,
            &receipt.refund_id,
            false,
            Some(status),
        )
        .await
        .map_err(Error::from)?,
        _ => billing_refunds::record_provider_unknown(
            &mut tx,
            &request_id,
            &receipt.refund_id,
            "provider_status_unrecognised",
        )
        .await
        .map_err(Error::from)?,
    };
    tx.commit().await?;
    Ok(Json(personal_refund_view(request)))
}

async fn validate_refund_receipt(
    provider: &dyn SubscriptionProvider,
    request: &billing_refunds::BillingRefundRequest,
    receipt: &RefundReceipt,
) -> Result<bool> {
    let refundable_amount = if request.full_refund_requested && receipt.status == "succeeded" {
        let Some(charge_id) = receipt.charge_id.as_deref() else {
            return Ok(false);
        };
        if !refund_receipt_matches_charge(receipt, charge_id) {
            return Ok(false);
        }
        Some(
            provider
                .get_refundable_amount(&request.payment_reference, charge_id, receipt.amount_pence)
                .await
                .map_err(ProviderError::into_error)?,
        )
    } else {
        None
    };
    if request.full_refund_requested && receipt.status != "succeeded" {
        return Ok(
            receipt.payment_reference == request.payment_reference && receipt.currency == "gbp"
        );
    }
    Ok(refund_receipt_matches_approved_correction(
        request,
        receipt,
        refundable_amount.as_ref(),
    ))
}

fn refund_receipt_matches_charge(receipt: &RefundReceipt, expected_charge_id: &str) -> bool {
    receipt.charge_id.as_deref() == Some(expected_charge_id)
        && expected_charge_id.starts_with("ch_")
}

fn parse_refund_charge_id(value: &serde_json::Value) -> ProviderResult<Option<String>> {
    match value.get("charge") {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(charge) => stripe_reference(charge)
            .filter(|charge_id| charge_id.starts_with("ch_"))
            .map(Some)
            .ok_or_else(ProviderError::malformed_response),
    }
}

fn refund_receipt_matches_approved_correction(
    request: &billing_refunds::BillingRefundRequest,
    receipt: &RefundReceipt,
    refundable_amount: Option<&(i64, String)>,
) -> bool {
    if receipt.payment_reference != request.payment_reference || receipt.currency != "gbp" {
        return false;
    }
    let partial_amount = request
        .amount_pence
        .map(|amount| (amount, "gbp".to_string()));
    let expected_amount = if request.full_refund_requested {
        refundable_amount
    } else {
        partial_amount.as_ref()
    };
    expected_amount.is_some_and(|(amount, currency)| {
        currency == &receipt.currency && *amount == receipt.amount_pence
    })
}

async fn operator_process_refund(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(request_id): Path<String>,
) -> Result<Json<PersonalRefundView>> {
    require_billing_corrections()?;
    require_billing_operator(&headers)?;
    let billing = billing_config(&state)?;
    let mut tx = state.pool.begin().await?;
    let current = billing_refunds::load_for_operator(&mut tx, &request_id)
        .await
        .map_err(Error::from)?;
    billing_transfers::lock_and_check_no_live_transfer(
        &mut tx,
        &[current.beneficiary_id.as_str()],
        sponsored_billing::current_epoch(),
    )
    .await
    .map_err(Error::from)?;
    let current = if current.state == billing_refunds::CorrectionState::Approved {
        let next = billing_refunds::begin_provider_refund(&mut tx, &request_id)
            .await
            .map_err(Error::from)?;
        next
    } else {
        current
    };
    let create_claim_token = if current.state == billing_refunds::CorrectionState::ProviderPending
        && current.provider_refund_id.is_none()
    {
        let claim_token = uuid::Uuid::new_v4();
        match billing_refunds::claim_or_park_provider_refund_create(
            &mut tx,
            &request_id,
            claim_token,
        )
        .await
        .map_err(Error::from)?
        {
            billing_refunds::RefundCreateClaim::Claimed { token, .. } => {
                tx.commit().await?;
                let mut check_tx = state.pool.begin().await?;
                let still_current = billing_refunds::provider_refund_create_claim_is_current(
                    &mut check_tx,
                    &request_id,
                    token,
                )
                .await
                .map_err(Error::from)?;
                let latest = billing_refunds::load_for_operator(&mut check_tx, &request_id)
                    .await
                    .map_err(Error::from)?;
                check_tx.rollback().await?;
                if !still_current {
                    return Ok(Json(personal_refund_view(latest)));
                }
                Some(token)
            }
            billing_refunds::RefundCreateClaim::InFlight(request)
            | billing_refunds::RefundCreateClaim::Parked(request) => {
                tx.commit().await?;
                return Ok(Json(personal_refund_view(request)));
            }
        }
    } else {
        tx.rollback().await?;
        None
    };
    let current = if create_claim_token.is_some() {
        let mut read_tx = state.pool.begin().await?;
        let current = billing_refunds::load_for_operator(&mut read_tx, &request_id)
            .await
            .map_err(Error::from)?;
        read_tx.rollback().await?;
        current
    } else {
        current
    };
    if current.state == billing_refunds::CorrectionState::TerminationPending {
        let provider_key = format!("sotto-refund:{request_id}:terminate");
        billing
            .provider
            .terminate_personal_subscription(
                &current.subscription_id,
                &provider_key,
                &current.beneficiary_id,
            )
            .await
            .map_err(ProviderError::into_error)?;
        let mut tx = state.pool.begin().await?;
        let request = billing_refunds::record_provider_termination(&mut tx, &request_id)
            .await
            .map_err(Error::from)?;
        tx.commit().await?;
        return Ok(Json(personal_refund_view(request)));
    }
    let has_known_refund = current.provider_refund_id.is_some();
    if current.state != billing_refunds::CorrectionState::ProviderPending
        && !(current.state == billing_refunds::CorrectionState::Unknown && has_known_refund)
    {
        return Ok(Json(personal_refund_view(current)));
    }
    let provider_key = format!("sotto-refund:{request_id}");
    let receipt = if let Some(provider_refund_id) = current.provider_refund_id.as_deref() {
        billing
            .provider
            .get_refund(provider_refund_id)
            .await
            .map_err(ProviderError::into_error)?
    } else if create_claim_token.is_some() {
        billing
            .provider
            .create_refund(
                &current.payment_reference,
                current.amount_pence,
                &provider_key,
            )
            .await
            .map_err(ProviderError::into_error)?
    } else {
        return Ok(Json(personal_refund_view(current)));
    };
    if !validate_refund_receipt(&*billing.provider, &current, &receipt).await? {
        let mut tx = state.pool.begin().await?;
        let request = billing_refunds::record_provider_unknown(
            &mut tx,
            &request_id,
            &receipt.refund_id,
            "provider_refund_evidence_mismatch",
        )
        .await
        .map_err(Error::from)?;
        tx.commit().await?;
        return Ok(Json(personal_refund_view(request)));
    }
    let mut tx = state.pool.begin().await?;
    let request = match receipt.status.as_str() {
        "succeeded" => billing_refunds::record_provider_refund(
            &mut tx,
            &request_id,
            &receipt.refund_id,
            true,
            None,
        )
        .await
        .map_err(Error::from)?,
        "pending" | "requires_action" => {
            billing_refunds::record_provider_pending(&mut tx, &request_id, &receipt.refund_id)
                .await
                .map_err(Error::from)?
        }
        status @ ("failed" | "canceled") => billing_refunds::record_provider_refund(
            &mut tx,
            &request_id,
            &receipt.refund_id,
            false,
            Some(status),
        )
        .await
        .map_err(Error::from)?,
        _ => billing_refunds::record_provider_unknown(
            &mut tx,
            &request_id,
            &receipt.refund_id,
            "provider_status_unrecognised",
        )
        .await
        .map_err(Error::from)?,
    };
    tx.commit().await?;
    if request.state == billing_refunds::CorrectionState::TerminationPending {
        billing
            .provider
            .terminate_personal_subscription(
                &request.subscription_id,
                &format!("{provider_key}:terminate"),
                &request.beneficiary_id,
            )
            .await
            .map_err(ProviderError::into_error)?;
        let mut tx = state.pool.begin().await?;
        let request = billing_refunds::record_provider_termination(&mut tx, &request_id)
            .await
            .map_err(Error::from)?;
        tx.commit().await?;
        return Ok(Json(personal_refund_view(request)));
    }
    Ok(Json(personal_refund_view(request)))
}

async fn personal_portal(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<PersonalLifecycleView>> {
    let billing = billing_config(&state)?;
    let mut tx = state.pool.begin().await?;
    let account = personal_billing::load_account(&mut tx, &user.user_id)
        .await
        .map_err(personal_billing_error)?
        .ok_or_else(|| Error::NotFound("personal billing account not found".into()))?;
    let customer = account.stripe_customer_id.clone().ok_or_else(|| {
        Error::Conflict("personal billing has not received a paid customer yet".into())
    })?;
    let founding = personal_founding_transition(&mut tx, &user.user_id).await?;
    tx.rollback().await?;
    let portal_url = billing
        .provider
        .create_portal(
            &customer,
            &app_url(&billing.return_url),
            billing.cloud_portal_configuration_id(),
        )
        .await
        .map_err(ProviderError::into_error)?;
    Ok(Json(PersonalLifecycleView {
        state: account.state.as_str().into(),
        stripe_subscription_id: account.stripe_subscription_id,
        paid_through_date: account.paid_through_date,
        cancel_at_period_end: account.cancel_at_period_end,
        portal_url: Some(portal_url),
        founding_price_end_epoch: founding.end_epoch,
        standard_renewal_amount_pence: founding.standard_amount_pence,
        founding_transition_status: founding.status,
        founding_transition_reason: founding.reason,
    }))
}

async fn personal_lifecycle(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<PersonalLifecycleView>> {
    let mut tx = state.pool.begin().await?;
    let account = personal_billing::load_account(&mut tx, &user.user_id)
        .await
        .map_err(personal_billing_error)?
        .ok_or_else(|| Error::NotFound("personal billing account not found".into()))?;
    let founding = personal_founding_transition(&mut tx, &user.user_id).await?;
    tx.commit().await?;
    Ok(Json(PersonalLifecycleView {
        state: account.state.as_str().into(),
        stripe_subscription_id: account.stripe_subscription_id,
        paid_through_date: account.paid_through_date,
        cancel_at_period_end: account.cancel_at_period_end,
        portal_url: None,
        founding_price_end_epoch: founding.end_epoch,
        standard_renewal_amount_pence: founding.standard_amount_pence,
        founding_transition_status: founding.status,
        founding_transition_reason: founding.reason,
    }))
}

struct FoundingTransitionView {
    end_epoch: Option<i64>,
    standard_amount_pence: Option<i64>,
    status: Option<String>,
    reason: Option<String>,
}

async fn personal_founding_transition(
    tx: &mut Transaction<'_, Postgres>,
    beneficiary_id: &str,
) -> Result<FoundingTransitionView> {
    let row = sqlx::query(
        "SELECT award.offer, award.original_end_epoch, \
                CASE WHEN account.cancel_at_period_end OR account.state = 'canceled' \
                     THEN 'not_required' ELSE COALESCE(schedule.state, 'unresolved') END AS schedule_state, \
                CASE WHEN account.cancel_at_period_end OR account.state = 'canceled' THEN NULL \
                     WHEN schedule.state = 'unresolved' THEN schedule.last_error_code \
                     WHEN schedule.job_id IS NULL THEN 'local_evidence_invalid' \
                     ELSE NULL END AS schedule_reason \
         FROM billing_founding_awards AS award \
         LEFT JOIN billing_founding_schedule_jobs AS schedule ON schedule.award_id = award.award_id \
         LEFT JOIN billing_personal_accounts AS account ON account.user_id = award.beneficiary_id \
         WHERE award.beneficiary_id = $1",
    )
    .bind(beneficiary_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row else {
        return Ok(FoundingTransitionView {
            end_epoch: None,
            standard_amount_pence: None,
            status: None,
            reason: None,
        });
    };
    let offer = parse_founding_offer(&row.try_get::<String, _>("offer")?)?;
    let end_epoch: Option<i64> = row.try_get("original_end_epoch")?;
    let status: Option<String> = row.try_get("schedule_state")?;
    let reason: Option<String> = row.try_get("schedule_reason")?;
    Ok(FoundingTransitionView {
        end_epoch,
        standard_amount_pence: Some(offer.standard_billing_offer().expected_amount_pence()),
        status: Some(status.unwrap_or_else(|| "unresolved".into())),
        reason,
    })
}

async fn personal_cancel(
    State(state): State<AppState>,
    user: AuthUser,
    Json(request): Json<PersonalCancelRequest>,
) -> Result<Json<PersonalLifecycleView>> {
    if request.idempotency_key.trim().is_empty() {
        return Err(Error::BadRequest(
            "idempotency_key must not be empty".into(),
        ));
    }
    let billing = billing_config(&state)?;
    let mut tx = state.pool.begin().await?;
    billing_transfers::lock_and_check_no_live_transfer(&mut tx, &[&user.user_id], billing_epoch())
        .await
        .map_err(Error::from)?;
    let account = personal_billing::load_account(&mut tx, &user.user_id)
        .await
        .map_err(personal_billing_error)?
        .ok_or_else(|| Error::NotFound("personal billing account not found".into()))?;
    if account.cancel_at_period_end {
        // A webhook or an earlier cancellation attempt may have set the account flag before the
        // founding transition schedule was reconciled. Repeat the provider operation on retries:
        // it is idempotent and changes an attached schedule to end at the paid boundary.
        if account.state != personal_billing::PersonalBillingState::Canceled {
            if let Some(subscription_id) = account.stripe_subscription_id.as_deref() {
                founding_schedule::request_personal_cancellation(
                    &mut tx,
                    &user.user_id,
                    subscription_id,
                )
                .await?;
                tx.commit().await?;
                billing
                    .provider
                    .cancel_personal_subscription(
                        subscription_id,
                        &request.idempotency_key,
                        &user.user_id,
                    )
                    .await
                    .map_err(ProviderError::into_error)?;
                founding_schedule::record_personal_cancellation(
                    &state.pool,
                    &user.user_id,
                    subscription_id,
                )
                .await?;
                let mut update_tx = state.pool.begin().await?;
                let founding = personal_founding_transition(&mut update_tx, &user.user_id).await?;
                update_tx.commit().await?;
                return Ok(Json(PersonalLifecycleView {
                    state: account.state.as_str().into(),
                    stripe_subscription_id: Some(subscription_id.to_owned()),
                    paid_through_date: account.paid_through_date,
                    cancel_at_period_end: true,
                    portal_url: None,
                    founding_price_end_epoch: founding.end_epoch,
                    standard_renewal_amount_pence: founding.standard_amount_pence,
                    founding_transition_status: founding.status,
                    founding_transition_reason: founding.reason,
                }));
            }
        }
        let founding = personal_founding_transition(&mut tx, &user.user_id).await?;
        tx.rollback().await?;
        return Ok(Json(PersonalLifecycleView {
            state: account.state.as_str().into(),
            stripe_subscription_id: account.stripe_subscription_id,
            paid_through_date: account.paid_through_date,
            cancel_at_period_end: true,
            portal_url: None,
            founding_price_end_epoch: founding.end_epoch,
            standard_renewal_amount_pence: founding.standard_amount_pence,
            founding_transition_status: founding.status,
            founding_transition_reason: founding.reason,
        }));
    }
    let subscription_id = account.stripe_subscription_id.clone().ok_or_else(|| {
        Error::Conflict("personal billing has not received a paid subscription yet".into())
    })?;
    founding_schedule::request_personal_cancellation(&mut tx, &user.user_id, &subscription_id)
        .await?;
    tx.commit().await?;
    billing
        .provider
        .cancel_personal_subscription(&subscription_id, &request.idempotency_key, &user.user_id)
        .await
        .map_err(ProviderError::into_error)?;
    founding_schedule::record_personal_cancellation(&state.pool, &user.user_id, &subscription_id)
        .await?;
    let mut update_tx = state.pool.begin().await?;
    let founding = personal_founding_transition(&mut update_tx, &user.user_id).await?;
    update_tx.commit().await?;
    Ok(Json(PersonalLifecycleView {
        state: account.state.as_str().into(),
        stripe_subscription_id: Some(subscription_id),
        paid_through_date: account.paid_through_date,
        cancel_at_period_end: true,
        portal_url: None,
        founding_price_end_epoch: founding.end_epoch,
        standard_renewal_amount_pence: founding.standard_amount_pence,
        founding_transition_status: founding.status,
        founding_transition_reason: founding.reason,
    }))
}

/// A provider-hosted page for the browser to navigate to.
#[derive(Serialize)]
struct RedirectView {
    url: String,
}

/// `POST /orgs/{org_id}/billing/checkout` - start a Team subscription (admin+). Returns the URL of
/// a checkout page; the tier flips when the `checkout.session.completed` webhook arrives.
async fn create_checkout(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<String>,
) -> Result<Json<RedirectView>> {
    let billing = cloud_sales_config(&state)?;
    let mut tx = state.pool.begin().await?;
    // Keep the organisation lock through provider session creation so deletion cannot transition
    // between the lifecycle check and this billing side effect. The bounded provider timeout
    // briefly pins a pool connection and serialises this organisation's writes; that is the
    // deliberate trade-off for closing the race.
    require_billing_admin(&mut tx, &org_id, &user.user_id).await?;
    billing
        .validate_cloud_portal_configuration()
        .await
        .map_err(ProviderError::into_error)?;

    // Reuse the org's Stripe customer if one exists, so a cancel/resubscribe doesn't fork billing
    // history; otherwise Checkout creates one and the webhook records it.
    let customer: Option<String> =
        sqlx::query_scalar("SELECT stripe_customer_id FROM organizations WHERE id = $1")
            .bind(&org_id)
            .fetch_optional(&mut *tx)
            .await?
            .flatten();

    let (success_url, cancel_url) = checkout_return_urls(&billing.return_url);
    let url = billing
        .provider
        .create_checkout(&org_id, customer.as_deref(), &success_url, &cancel_url)
        .await
        .map_err(ProviderError::into_error)?;
    tx.commit().await?;
    Ok(Json(RedirectView { url }))
}

/// `POST /orgs/{org_id}/billing/portal` - manage/cancel the subscription (admin+) via Stripe's
/// hosted customer portal.
async fn create_portal(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<String>,
) -> Result<Json<RedirectView>> {
    let billing = billing_config(&state)?;
    let mut tx = state.pool.begin().await?;
    // Keep the organisation lock through provider session creation so deletion cannot transition
    // between the lifecycle check and this billing side effect. The bounded provider timeout
    // briefly pins a pool connection and serialises this organisation's writes; that is the
    // deliberate trade-off for closing the race.
    require_billing_admin(&mut tx, &org_id, &user.user_id).await?;

    let customer: Option<String> =
        sqlx::query_scalar("SELECT stripe_customer_id FROM organizations WHERE id = $1")
            .bind(&org_id)
            .fetch_optional(&mut *tx)
            .await?
            .flatten();
    let customer = customer.ok_or_else(|| {
        Error::BadRequest("this organisation has no billing account yet - subscribe first".into())
    })?;

    let url = billing
        .provider
        .create_portal(
            &customer,
            &app_url(&billing.return_url),
            billing.cloud_portal_configuration_id(),
        )
        .await
        .map_err(ProviderError::into_error)?;
    tx.commit().await?;
    Ok(Json(RedirectView { url }))
}

#[cfg(feature = "e2e-mock-billing")]
#[derive(Deserialize)]
struct E2eCheckoutQuery {
    success_url: String,
    cancel_url: String,
}

#[cfg(feature = "e2e-mock-billing")]
#[derive(Deserialize)]
struct E2ePortalQuery {
    return_url: String,
}

#[cfg(feature = "e2e-mock-billing")]
async fn e2e_checkout(Query(query): Query<E2eCheckoutQuery>) -> Html<String> {
    Html(e2e_provider_page(
        "Test checkout",
        "Complete payment",
        &query.success_url,
        "Cancel payment",
        &query.cancel_url,
    ))
}

#[cfg(feature = "e2e-mock-billing")]
async fn e2e_portal(Query(query): Query<E2ePortalQuery>) -> Html<String> {
    Html(e2e_provider_page(
        "Test billing portal",
        "Return to app",
        &query.return_url,
        "Return to app",
        &query.return_url,
    ))
}

#[cfg(feature = "e2e-mock-billing")]
fn e2e_provider_page(
    title: &str,
    primary_label: &str,
    primary_url: &str,
    secondary_label: &str,
    secondary_url: &str,
) -> String {
    format!(
        "<!doctype html><html><head><title>{}</title></head><body>\
         <h1>{}</h1><p><a href=\"{}\">{}</a></p><p><a href=\"{}\">{}</a></p>\
         </body></html>",
        escape_html(title),
        escape_html(title),
        safe_href(primary_url),
        escape_html(primary_label),
        safe_href(secondary_url),
        escape_html(secondary_label),
    )
}

#[cfg(feature = "e2e-mock-billing")]
fn safe_href(value: &str) -> String {
    // Query parameters become links in this test-only page, so reject script/data schemes if a
    // mock-billing build is ever exposed outside its intended local environment.
    match Url::parse(value) {
        Ok(url) if matches!(url.scheme(), "http" | "https") => escape_html(value),
        _ => "#".into(),
    }
}

#[cfg(feature = "e2e-mock-billing")]
fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The vault app's address: the site root serves the marketing page, the app lives under `/app`.
fn app_url(base: &str) -> String {
    format!("{}/app", base.trim_end_matches('/'))
}

/// Where the browser lands after Stripe Checkout. Both land in the vault app, which reads the
/// `billing` query parameter to explain the outcome (the tier itself flips via the webhook).
fn checkout_return_urls(base: &str) -> (String, String) {
    let app = app_url(base);
    (
        format!("{app}?billing=success"),
        format!("{app}?billing=cancelled"),
    )
}

/// The process-wide Stripe HTTP client, built once and reused (reqwest pools connections behind an
/// `Arc`, so cloning/sharing is cheap). Bounded by the same timeouts as the GitHub OAuth client
/// (`auth::oauth`): a stalled Stripe - slow DNS/TLS, a hung connection - must not tie up the request
/// task and its socket indefinitely.
fn stripe_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("reqwest client with static config builds")
    })
}

fn stripe_headers(idempotency_key: Option<&str>) -> ProviderResult<reqwest::header::HeaderMap> {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        "Stripe-Version",
        reqwest::header::HeaderValue::from_static(STRIPE_API_VERSION),
    );
    if let Some(idempotency_key) = idempotency_key {
        let value = reqwest::header::HeaderValue::from_str(idempotency_key)
            .map_err(|_| ProviderError::malformed_response())?;
        headers.insert("Idempotency-Key", value);
    }
    Ok(headers)
}

/// One form-encoded call to the Stripe API.
async fn stripe_post(
    api_key: &str,
    path: &str,
    form: &[(String, String)],
) -> ProviderResult<serde_json::Value> {
    stripe_post_at(api_key, "https://api.stripe.com", path, form).await
}

async fn stripe_post_at(
    api_key: &str,
    api_origin: &str,
    path: &str,
    form: &[(String, String)],
) -> ProviderResult<serde_json::Value> {
    let response = stripe_client()
        .post(format!("{}/v1/{path}", api_origin.trim_end_matches('/')))
        .bearer_auth(api_key)
        .headers(stripe_headers(None)?)
        .form(form)
        .send()
        .await
        .map_err(|_| ProviderError::transport())?;
    stripe_response(response).await
}

async fn stripe_post_with_idempotency(
    api_key: &str,
    path: &str,
    form: &[(String, String)],
    idempotency_key: &str,
) -> ProviderResult<serde_json::Value> {
    stripe_post_with_idempotency_at(
        api_key,
        "https://api.stripe.com",
        path,
        form,
        idempotency_key,
    )
    .await
}

async fn stripe_post_with_idempotency_at(
    api_key: &str,
    api_origin: &str,
    path: &str,
    form: &[(String, String)],
    idempotency_key: &str,
) -> ProviderResult<serde_json::Value> {
    let response = stripe_client()
        .post(format!("{}/v1/{path}", api_origin.trim_end_matches('/')))
        .bearer_auth(api_key)
        .headers(stripe_headers(Some(idempotency_key))?)
        .form(form)
        .send()
        .await
        .map_err(|_| ProviderError::transport())?;
    stripe_response(response).await
}

/// Fetch one Stripe resource with the pinned API version and structured error classification.
async fn stripe_get(api_key: &str, path: &str) -> ProviderResult<serde_json::Value> {
    stripe_get_at(api_key, "https://api.stripe.com", path).await
}

async fn stripe_get_at(
    api_key: &str,
    api_origin: &str,
    path: &str,
) -> ProviderResult<serde_json::Value> {
    let response = stripe_client()
        .get(format!("{}/v1/{path}", api_origin.trim_end_matches('/')))
        .bearer_auth(api_key)
        .headers(stripe_headers(None)?)
        .send()
        .await
        .map_err(|_| ProviderError::transport())?;
    stripe_response(response).await
}

fn stripe_schedule_contains_phases(
    schedule: &serde_json::Value,
    current_start: i64,
    current_end: i64,
    current_items: &[SponsoredSubscriptionItem],
    expected_phases: &[SponsoredSubscriptionPhase],
) -> bool {
    let Some(actual) = schedule["phases"].as_array() else {
        return false;
    };
    if actual.iter().any(|phase| {
        let Some(start) = phase["start_date"].as_i64() else {
            return true;
        };
        let Some(end) = phase["end_date"].as_i64() else {
            return true;
        };
        end <= start
    }) {
        return false;
    }
    // Stripe may return completed schedule phases, though it can omit them. Compare the active
    // phase and future plan, excluding only history whose end is at or before this period starts.
    let current_phases: Vec<_> = actual
        .iter()
        .filter(|phase| {
            phase["end_date"]
                .as_i64()
                .is_some_and(|end| end > current_start)
        })
        .collect();
    let active = SponsoredSubscriptionPhase {
        start_at_epoch: current_start,
        end_at_epoch: Some(current_end),
        items: current_items
            .iter()
            .map(|item| SponsoredSubscriptionItemRequest {
                provider_price_id: item.provider_price_id.clone(),
                quantity: item.quantity,
            })
            .collect(),
    };
    current_phases.len() == expected_phases.len() + 1
        && stripe_schedule_phase_matches(current_phases[0], &active, None)
        && expected_phases.iter().enumerate().all(|(index, expected)| {
            let next_start = expected_phases
                .get(index + 1)
                .map(|phase| phase.start_at_epoch);
            stripe_schedule_phase_matches(current_phases[index + 1], expected, next_start)
        })
}

fn transfer_setup_checkout_form(
    transfer_id: &str,
    customer_id: &str,
    success_url: &str,
    cancel_url: &str,
) -> Vec<(String, String)> {
    vec![
        ("mode".into(), "setup".into()),
        ("customer".into(), customer_id.into()),
        (
            "client_reference_id".into(),
            format!("sotto-transfer:{transfer_id}"),
        ),
        ("metadata[sotto_transfer_id]".into(), transfer_id.into()),
        (
            "setup_intent_data[metadata][sotto_transfer_id]".into(),
            transfer_id.into(),
        ),
        ("setup_intent_data[usage]".into(), "off_session".into()),
        ("success_url".into(), success_url.into()),
        ("cancel_url".into(), cancel_url.into()),
    ]
}

fn transfer_setup_session_from_object(
    object: &serde_json::Value,
    expected_session_id: Option<&str>,
    expected_transfer_id: Option<&str>,
) -> ProviderResult<TransferSetupSession> {
    let session_id = object["id"]
        .as_str()
        .filter(|value| !value.is_empty() && expected_session_id.is_none_or(|id| *value == id))
        .ok_or_else(ProviderError::malformed_response)?;
    if object["object"].as_str() != Some("checkout.session")
        || object["mode"].as_str() != Some("setup")
        || expected_transfer_id.is_some_and(|transfer_id| {
            object["client_reference_id"].as_str()
                != Some(format!("sotto-transfer:{transfer_id}").as_str())
                || object["metadata"]["sotto_transfer_id"].as_str() != Some(transfer_id)
        })
    {
        return Err(ProviderError::malformed_response());
    }
    let customer_id = object["customer"]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(ProviderError::malformed_response)?;
    let status = object["status"]
        .as_str()
        .filter(|value| matches!(*value, "open" | "complete" | "expired"))
        .ok_or_else(ProviderError::malformed_response)?;
    let checkout_url = object["url"]
        .as_str()
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let setup_intent_id = object["setup_intent"]
        .as_str()
        .or_else(|| object["setup_intent"]["id"].as_str())
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let expires_at_epoch = object["expires_at"].as_i64();
    if object["url"].is_null() && status == "open" {
        return Err(ProviderError::malformed_response());
    }
    Ok(TransferSetupSession {
        session_id: session_id.to_string(),
        checkout_url,
        customer_id: customer_id.to_string(),
        setup_intent_id,
        status: status.to_string(),
        expires_at_epoch,
    })
}

fn personal_checkout_status_from_object(
    object: &serde_json::Value,
    expected_session_id: &str,
    expected_operation_id: &str,
) -> ProviderResult<PersonalCheckoutStatus> {
    checkout_status_from_object(
        object,
        expected_session_id,
        &format!("personal:{expected_operation_id}"),
    )
}

fn checkout_status_from_object(
    object: &serde_json::Value,
    expected_session_id: &str,
    expected_client_reference_id: &str,
) -> ProviderResult<PersonalCheckoutStatus> {
    if object["object"].as_str() != Some("checkout.session")
        || object["id"].as_str() != Some(expected_session_id)
        || object["mode"].as_str() != Some("subscription")
        || object["client_reference_id"].as_str() != Some(expected_client_reference_id)
    {
        return Err(ProviderError::malformed_response());
    }
    match (object["status"].as_str(), object["payment_status"].as_str()) {
        (Some("open"), Some("unpaid")) => Ok(PersonalCheckoutStatus::Open),
        (Some("complete"), Some("unpaid" | "paid" | "no_payment_required")) => {
            Ok(PersonalCheckoutStatus::Complete)
        }
        (Some("expired"), Some("unpaid")) => Ok(PersonalCheckoutStatus::ExpiredUnpaid),
        _ => Err(ProviderError::malformed_response()),
    }
}

fn transfer_setup_intent_from_object(
    object: &serde_json::Value,
    expected_setup_intent_id: &str,
) -> ProviderResult<TransferSetupIntent> {
    let setup_intent_id = object["id"]
        .as_str()
        .filter(|value| *value == expected_setup_intent_id)
        .ok_or_else(ProviderError::malformed_response)?;
    let transfer_id = object["metadata"]["sotto_transfer_id"]
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(ProviderError::malformed_response)?;
    let customer_id = object["customer"]
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(ProviderError::malformed_response)?;
    let payment_method_id = object["payment_method"]
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(ProviderError::malformed_response)?;
    let status = object["status"]
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(ProviderError::malformed_response)?;
    let off_session = object["usage"].as_str() == Some("off_session");
    if object["object"].as_str() != Some("setup_intent") || !off_session {
        return Err(ProviderError::malformed_response());
    }
    Ok(TransferSetupIntent {
        setup_intent_id: setup_intent_id.to_string(),
        transfer_id: transfer_id.to_string(),
        customer_id: customer_id.to_string(),
        payment_method_id: payment_method_id.to_string(),
        status: status.to_string(),
        off_session,
    })
}

#[allow(clippy::too_many_arguments)]
fn personal_transfer_schedule_form(
    transfer_id: &str,
    beneficiary_id: &str,
    customer_id: &str,
    payment_method_id: &str,
    offer: BillingOffer,
    price_id: &str,
    allocation_reference: &str,
    start_at_epoch: i64,
) -> Vec<(String, String)> {
    let operation_id = format!("{transfer_id}:destination");
    let mut form: Vec<(String, String)> = vec![
        ("customer".into(), customer_id.into()),
        ("start_date".into(), start_at_epoch.to_string()),
        ("end_behavior".into(), "release".into()),
        (
            "default_settings[default_payment_method]".into(),
            payment_method_id.into(),
        ),
        ("metadata[sotto_transfer_id]".into(), transfer_id.into()),
        ("metadata[personal_user_id]".into(), beneficiary_id.into()),
        ("metadata[operation_id]".into(), operation_id.clone()),
        (
            "metadata[sotto_allocation_reference]".into(),
            allocation_reference.into(),
        ),
        ("metadata[sotto_offer]".into(), offer.as_str().into()),
        (
            "phases[0][default_payment_method]".into(),
            payment_method_id.into(),
        ),
        ("phases[0][proration_behavior]".into(), "none".into()),
        ("phases[0][items][0][price]".into(), price_id.into()),
        ("phases[0][items][0][quantity]".into(), "1".into()),
        (
            "phases[0][metadata][sotto_transfer_id]".into(),
            transfer_id.into(),
        ),
        (
            "phases[0][metadata][personal_user_id]".into(),
            beneficiary_id.into(),
        ),
        ("phases[0][metadata][operation_id]".into(), operation_id),
        (
            "phases[0][metadata][sotto_allocation_reference]".into(),
            allocation_reference.into(),
        ),
        (
            "phases[0][metadata][sotto_offer]".into(),
            offer.as_str().into(),
        ),
    ];
    form.sort_by(|left, right| left.0.cmp(&right.0));
    form
}

fn personal_source_restoration_form(
    transfer_id: &str,
    beneficiary_id: &str,
    customer_id: &str,
    payment_method_id: &str,
    price_id: &str,
    allocation_reference: &str,
    operation_id: &str,
) -> Vec<(String, String)> {
    vec![
        ("customer".into(), customer_id.into()),
        ("items[0][price]".into(), price_id.into()),
        ("default_payment_method".into(), payment_method_id.into()),
        ("collection_method".into(), "charge_automatically".into()),
        ("payment_behavior".into(), "allow_incomplete".into()),
        ("off_session".into(), "true".into()),
        ("metadata[personal_user_id]".into(), beneficiary_id.into()),
        ("metadata[operation_id]".into(), operation_id.into()),
        (
            "metadata[sotto_allocation_reference]".into(),
            allocation_reference.into(),
        ),
        ("metadata[sotto_transfer_id]".into(), transfer_id.into()),
    ]
}

#[allow(clippy::too_many_arguments)]
fn transfer_schedule_from_object(
    object: &serde_json::Value,
    transfer_id: &str,
    customer_id: &str,
    payment_method_id: &str,
    beneficiary_id: &str,
    offer: BillingOffer,
    price_id: &str,
    allocation_reference: &str,
    start_at_epoch: i64,
) -> ProviderResult<TransferSubscriptionSchedule> {
    let operation_id = format!("{transfer_id}:destination");
    let schedule_id = object["id"]
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(ProviderError::malformed_response)?;
    let observed_customer = object["customer"]
        .as_str()
        .filter(|value| *value == customer_id)
        .ok_or_else(ProviderError::malformed_response)?;
    let status = object["status"]
        .as_str()
        .filter(|value| matches!(*value, "not_started" | "active"))
        .ok_or_else(ProviderError::malformed_response)?;
    let phases = object["phases"]
        .as_array()
        .filter(|phases| phases.len() == 1)
        .ok_or_else(ProviderError::malformed_response)?;
    let phase = &phases[0];
    let items = phase["items"]
        .as_array()
        .filter(|items| items.len() == 1)
        .ok_or_else(ProviderError::malformed_response)?;
    let phase_payment_method = stripe_reference(&phase["default_payment_method"]);
    let default_payment_method =
        stripe_reference(&object["default_settings"]["default_payment_method"]);
    if object["object"].as_str() != Some("subscription_schedule")
        || object["start_date"].as_i64() != Some(start_at_epoch)
        || phase["start_date"].as_i64() != Some(start_at_epoch)
        || object["end_behavior"].as_str() != Some("release")
        || phase["proration_behavior"].as_str() != Some("none")
        || phase_payment_method.as_deref() != Some(payment_method_id)
        || default_payment_method.as_deref() != Some(payment_method_id)
        || object["metadata"]["sotto_transfer_id"].as_str() != Some(transfer_id)
        || object["metadata"]["personal_user_id"].as_str() != Some(beneficiary_id)
        || object["metadata"]["operation_id"].as_str() != Some(operation_id.as_str())
        || object["metadata"]["sotto_allocation_reference"].as_str() != Some(allocation_reference)
        || object["metadata"]["sotto_offer"].as_str() != Some(offer.as_str())
        || items[0]["price"].as_str() != Some(price_id)
        || items[0]["quantity"].as_i64() != Some(1)
        || phase["metadata"]["sotto_transfer_id"].as_str() != Some(transfer_id)
        || phase["metadata"]["personal_user_id"].as_str() != Some(beneficiary_id)
        || phase["metadata"]["operation_id"].as_str() != Some(operation_id.as_str())
        || phase["metadata"]["sotto_allocation_reference"].as_str() != Some(allocation_reference)
        || phase["metadata"]["sotto_offer"].as_str() != Some(offer.as_str())
    {
        return Err(ProviderError::malformed_response());
    }
    let subscription_id = object["subscription"]
        .as_str()
        .or_else(|| object["subscription"]["id"].as_str())
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string);
    Ok(TransferSubscriptionSchedule {
        schedule_id: schedule_id.to_string(),
        customer_id: observed_customer.to_string(),
        subscription_id,
        start_at_epoch,
        expected_period_end_epoch: FoundingDate::anchored_term_end_epoch(
            start_at_epoch,
            FoundingOffer::for_billing_offer_interval(offer),
            1,
        )
        .map_err(|_| ProviderError::malformed_response())?,
        status: status.to_string(),
    })
}

fn personal_schedule_cancellation_form(
    current_start: i64,
    current_end: i64,
    current_items: &[SponsoredSubscriptionItem],
) -> Vec<(String, String)> {
    let mut form = vec![
        ("end_behavior".into(), "cancel".into()),
        ("proration_behavior".into(), "none".into()),
        ("phases[0][start_date]".into(), current_start.to_string()),
        ("phases[0][end_date]".into(), current_end.to_string()),
    ];
    for (index, item) in current_items.iter().enumerate() {
        form.push((
            format!("phases[0][items][{index}][price]"),
            item.provider_price_id.clone(),
        ));
        form.push((
            format!("phases[0][items][{index}][quantity]"),
            item.quantity.to_string(),
        ));
    }
    form
}

fn stripe_schedule_subscription_id(schedule: &serde_json::Value) -> Option<&str> {
    schedule["subscription"]
        .as_str()
        .or_else(|| schedule["subscription"]["id"].as_str())
}

fn personal_schedule_cancellation_idempotency_key(
    subscription_id: &str,
    current_end: i64,
    request_key: &str,
) -> String {
    let digest = Sha256::digest(request_key.as_bytes());
    let request_fingerprint = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("sotto-personal-cancel:{subscription_id}:{current_end}:{request_fingerprint}")
}

fn stripe_current_schedule_phase_start(
    schedule: &serde_json::Value,
    schedule_id: &str,
    subscription_id: &str,
    period_start: i64,
    period_end: i64,
) -> ProviderResult<i64> {
    if schedule["id"].as_str() != Some(schedule_id)
        || stripe_schedule_subscription_id(schedule) != Some(subscription_id)
    {
        return Err(ProviderError::malformed_response());
    }
    let start = schedule["current_phase"]["start_date"]
        .as_i64()
        .filter(|start| *start >= 0)
        .ok_or_else(ProviderError::malformed_response)?;
    let end = schedule["current_phase"]["end_date"]
        .as_i64()
        .filter(|end| *end > start)
        .ok_or_else(ProviderError::malformed_response)?;
    if start > period_start || end < period_end {
        return Err(ProviderError::malformed_response());
    }
    Ok(start)
}

fn stripe_schedule_phase_matches(
    actual: &serde_json::Value,
    expected: &SponsoredSubscriptionPhase,
    next_phase_start: Option<i64>,
) -> bool {
    actual["start_date"].as_i64() == Some(expected.start_at_epoch)
        && match expected.end_at_epoch {
            Some(end) => actual["end_date"].as_i64() == Some(end),
            None => actual["end_date"]
                .as_i64()
                .is_some_and(|end| match next_phase_start {
                    Some(next_start) => end == next_start,
                    None => end > expected.start_at_epoch,
                }),
        }
        && stripe_schedule_items_match(&actual["items"], &expected.items)
}

fn stripe_schedule_items_match(
    actual: &serde_json::Value,
    expected: &[SponsoredSubscriptionItemRequest],
) -> bool {
    let Some(actual) = actual.as_array() else {
        return false;
    };
    actual.len() == expected.len()
        && expected.iter().all(|expected| {
            actual.iter().any(|item| {
                let price_id = item["price"]
                    .as_str()
                    .or_else(|| item["price"]["id"].as_str());
                price_id == Some(expected.provider_price_id.as_str())
                    && item["quantity"].as_i64() == Some(expected.quantity)
            })
        })
}

fn schedule_update_idempotency_key(base: &str, phases: &[SponsoredSubscriptionPhase]) -> String {
    let encoded = serde_json::to_vec(phases).expect("schedule phases serialize");
    let digest = Sha256::digest(encoded);
    format!("{base}:update:{digest:x}")
}

fn validate_stripe_personal_price(
    value: &serde_json::Value,
    price_id: &str,
    offer: BillingOffer,
    expected_livemode: bool,
) -> ProviderResult<()> {
    if value["id"].as_str() != Some(price_id)
        || value["active"].as_bool() != Some(true)
        || value["livemode"].as_bool() != Some(expected_livemode)
        || value["currency"].as_str() != Some("gbp")
        || value["unit_amount"].as_i64() != Some(offer.expected_amount_pence())
    {
        return Err(ProviderError::malformed_response());
    }
    let recurring = value["recurring"]
        .as_object()
        .ok_or_else(ProviderError::malformed_response)?;
    if recurring
        .get("interval")
        .and_then(serde_json::Value::as_str)
        != Some(offer.expected_interval().as_str())
        || recurring
            .get("interval_count")
            .and_then(serde_json::Value::as_i64)
            != Some(1)
        || recurring
            .get("usage_type")
            .and_then(serde_json::Value::as_str)
            != Some("licensed")
    {
        return Err(ProviderError::malformed_response());
    }
    Ok(())
}

fn validate_cloud_portal_configuration(
    value: &serde_json::Value,
    configuration_id: &str,
    expected_livemode: bool,
) -> ProviderResult<()> {
    if value["id"].as_str() != Some(configuration_id)
        || value["object"].as_str() != Some("billing_portal.configuration")
        || value["active"].as_bool() != Some(true)
        || value["livemode"].as_bool() != Some(expected_livemode)
        || value["features"]["subscription_update"]["enabled"].as_bool() != Some(false)
        || value["features"]["payment_method_update"]["enabled"].as_bool() != Some(true)
        || value["features"]["subscription_cancel"]["enabled"].as_bool() != Some(true)
        || value["features"]["subscription_cancel"]["mode"].as_str() != Some("at_period_end")
        || value["features"]["subscription_cancel"]["proration_behavior"].as_str() != Some("none")
    {
        return Err(ProviderError::malformed_response());
    }
    Ok(())
}

fn portal_session_form(
    customer: &str,
    return_url: &str,
    configuration_id: Option<&str>,
) -> Vec<(String, String)> {
    let mut form = vec![
        ("customer".to_string(), customer.to_string()),
        ("return_url".to_string(), return_url.to_string()),
    ];
    if let Some(configuration_id) = configuration_id {
        form.push(("configuration".to_string(), configuration_id.to_string()));
    }
    form
}

/// Delete one Stripe resource with an idempotency key and the explicit cancellation form.
async fn stripe_delete(
    api_key: &str,
    path: &str,
    idempotency_key: &str,
    form: &[(String, String)],
) -> ProviderResult<serde_json::Value> {
    let response = stripe_client()
        .delete(format!("https://api.stripe.com/v1/{path}"))
        .bearer_auth(api_key)
        .headers(stripe_headers(Some(idempotency_key))?)
        .form(form)
        .send()
        .await
        .map_err(|_| ProviderError::transport())?;
    stripe_response(response).await
}

/// Preserve Stripe's status and machine-readable error code before the HTTP boundary sanitises it.
async fn stripe_response(response: reqwest::Response) -> ProviderResult<serde_json::Value> {
    let status = response.status().as_u16();
    let body = response
        .text()
        .await
        .map_err(|_| ProviderError::transport())?;
    if status >= 400 {
        let code = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|value| value["error"]["code"].as_str().map(str::to_string));
        return Err(ProviderError::http(status, code));
    }
    let value = if body.trim().is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_str(&body).map_err(|_| ProviderError::malformed_response())?
    };
    Ok(value)
}

fn subscription_snapshot(
    object: &serde_json::Value,
    requested_id: &str,
) -> ProviderResult<SubscriptionSnapshot> {
    // A mismatched provider ID could otherwise gate deletion using another subscription's state.
    let id = object["id"]
        .as_str()
        .filter(|&id| id == requested_id)
        .ok_or_else(ProviderError::malformed_response)?;
    let status = object["status"]
        .as_str()
        .ok_or_else(ProviderError::malformed_response)?;
    Ok(SubscriptionSnapshot {
        id: id.to_string(),
        status: SubscriptionStatus::parse(status),
    })
}

fn sponsored_subscription_snapshot(
    object: &serde_json::Value,
    requested_id: &str,
) -> ProviderResult<SponsoredSubscriptionSnapshot> {
    let base = subscription_snapshot(object, requested_id)?;
    let cancel_at_period_end = object["cancel_at_period_end"]
        .as_bool()
        .ok_or_else(ProviderError::malformed_response)?;
    let cancel_at = match object.get("cancel_at") {
        None | Some(serde_json::Value::Null) => None,
        Some(value) => Some(
            value
                .as_i64()
                .filter(|timestamp| *timestamp >= 0)
                .ok_or_else(ProviderError::malformed_response)?,
        ),
    };
    let items = object["items"]["data"]
        .as_array()
        .ok_or_else(ProviderError::malformed_response)?
        .iter()
        .map(|item| {
            let provider_item_id = item["id"]
                .as_str()
                .filter(|value| !value.is_empty())
                .ok_or_else(ProviderError::malformed_response)?;
            let provider_price_id = item["price"]["id"]
                .as_str()
                .filter(|value| !value.is_empty())
                .ok_or_else(ProviderError::malformed_response)?;
            let quantity = item["quantity"]
                .as_i64()
                .filter(|value| *value >= 0)
                .ok_or_else(ProviderError::malformed_response)?;
            Ok(SponsoredSubscriptionItem {
                provider_item_id: provider_item_id.to_string(),
                provider_price_id: provider_price_id.to_string(),
                quantity,
            })
        })
        .collect::<ProviderResult<Vec<_>>>()?;
    Ok(SponsoredSubscriptionSnapshot {
        customer_id: object["customer"].as_str().map(str::to_string),
        default_payment_method_id: stripe_reference(&object["default_payment_method"]),
        subscription_id: base.id,
        status: base.status,
        cancel_at_period_end,
        cancel_at,
        current_period_start: subscription_period_start(object),
        current_period_end: subscription_period_end(object),
        schedule_id: object["schedule"].as_str().map(str::to_string),
        items,
    })
}

fn cancellation_form(org_id: &str) -> Vec<(String, String)> {
    vec![
        ("invoice_now".into(), "false".into()),
        ("prorate".into(), "false".into()),
        (
            "cancellation_details[comment]".into(),
            format!("Sotto organisation {org_id} deleted"),
        ),
    ]
}

fn cancellation_outcome(
    cancellation: ProviderResult<()>,
    fresh: ProviderResult<SubscriptionObservation>,
) -> ProviderResult<SubscriptionObservation> {
    match fresh {
        Ok(observation) => {
            if matches!(
                observation.purge_gate(),
                PurgeGate::Terminal | PurgeGate::Missing
            ) {
                Ok(observation)
            } else {
                // A still-blocking fresh observation must retain a failed or timed-out cancel;
                // returning it as success would let the deletion worker advance unsafely.
                cancellation.map(|_| observation)
            }
        }
        Err(error) => Err(cancellation.err().unwrap_or(error)),
    }
}

// --- webhook -------------------------------------------------------------------------------------

/// The slice of a Stripe event we act on; everything else in the payload is ignored.
#[derive(Deserialize)]
struct Event {
    id: String,
    created: i64,
    api_version: Option<String>,
    #[serde(rename = "type")]
    kind: String,
    data: EventData,
}

#[derive(Deserialize)]
struct EventData {
    object: serde_json::Value,
}

#[derive(Clone, Debug)]
struct VerifiedTransferSetup {
    transfer_id: String,
    customer_id: String,
    session_id: String,
    setup_intent_id: String,
}

/// `POST /billing/webhook` - Stripe's event delivery. Signature-verified against the endpoint's
/// signing secret; unhandled event types are acknowledged and ignored (so the endpoint can be
/// subscribed broadly in the dashboard without breaking).
async fn webhook(State(state): State<AppState>, headers: HeaderMap, body: String) -> Result<()> {
    let billing = billing_config(&state)?;
    let signature = headers
        .get("Stripe-Signature")
        .and_then(|v| v.to_str().ok())
        .ok_or(Error::Unauthorized)?;
    let now = crate::clock::wall_epoch_seconds();
    if !verify_signature(&billing.webhook_secret, signature, &body, now) {
        return Err(Error::Unauthorized);
    }

    let event: Event =
        serde_json::from_str(&body).map_err(|_| Error::BadRequest("malformed event".into()))?;
    if !webhook_version_accepted(event.api_version.as_deref()) {
        // Recorded, but deliberately NOT marked processed, and answered with a failure.
        //
        // The previous version of this did the opposite of both, and the combination was the
        // trap: a 200 tells Stripe the event was handled, so it is never retried, and marking it
        // processed means a manual resend is skipped as a duplicate. An event arriving one
        // version too new was therefore lost the instant it arrived, silently, having reported
        // success. A paid subscription that never applies is not a thing to be quiet about.
        //
        // Failing instead buys Stripe's retry schedule, which is hours: long enough to add the
        // version to the list above, ship, and have the backlog delivered rather than recovered
        // by hand from a dashboard.
        let mut tx = state.pool.begin().await?;
        // Pruned here as well as on the accepted path, because this is the one path that can run
        // for days on its own. Refusing leaves each receipt pending on purpose so a redelivery
        // can still do the work, and the retention policy already covers that case: pending rows
        // older than a day go. Without this call a version mismatch would be the only state in
        // which nothing ever prunes, which is precisely the state it has to survive.
        prune_webhook_events(&mut tx).await?;
        let inserted = record_webhook_receipt(&mut tx, &event, None).await?;
        tx.commit().await?;
        if inserted {
            eprintln!(
                "error: refusing Stripe webhook {} sent at unsupported API version {}; \
                 add it to ACCEPTED_WEBHOOK_API_VERSIONS if the payload is compatible",
                event.id,
                event.api_version.as_deref().unwrap_or("missing")
            );
        }
        return Err(Error::Internal(format!(
            "unsupported Stripe API version {}",
            event.api_version.as_deref().unwrap_or("missing")
        )));
    }

    // Make the signed billing fact visible even if the separate coverage-evidence read below
    // needs Stripe to retry. The receipt stays unprocessed until the ordinary webhook path has
    // applied all billing and coverage work; this transaction only records the customer notice.
    if event.kind == "invoice.payment_failed"
        && record_personal_renewal_failure_notice(&state.pool, &event).await?
    {
        return Ok(());
    }

    let processed_duplicate =
        if event.kind == "invoice.paid" && billing.personal_coverage_ingest_enabled() {
            sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM stripe_webhook_events \
             WHERE event_id = $1 AND processed_at IS NOT NULL)",
            )
            .bind(&event.id)
            .fetch_one(&state.pool)
            .await?
        } else {
            false
        };
    // Disabling setup stops new Checkout sessions, not settlement of sessions already created.
    // Recovery must continue when either transfer intake or the setup route is switched off.
    let transfer_setup = if event.kind == "checkout.session.completed"
        && object_is_transfer_setup(&event.data.object)
    {
        let object = &event.data.object;
        let transfer_id = transfer_setup_transfer_id(object)
            .ok_or_else(|| Error::BadRequest("malformed transfer setup session".into()))?;
        let session_id = object["id"]
            .as_str()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| Error::BadRequest("transfer setup session has no id".into()))?;
        let customer_id = object["customer"]
            .as_str()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| Error::BadRequest("transfer setup session has no customer".into()))?;
        let setup_intent_id = object["setup_intent"]
            .as_str()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| Error::BadRequest("transfer setup session has no SetupIntent".into()))?;
        let setup_intent = billing
            .provider
            .get_transfer_setup_intent(setup_intent_id)
            .await
            .map_err(ProviderError::into_error)?;
        if setup_intent.transfer_id != transfer_id
            || setup_intent.setup_intent_id != setup_intent_id
            || setup_intent.customer_id != customer_id
            || setup_intent.status != "succeeded"
            || !setup_intent.off_session
        {
            return Err(Error::Conflict(
                "transfer payment setup evidence does not match the session".into(),
            ));
        }
        Some(VerifiedTransferSetup {
            transfer_id: transfer_id.to_string(),
            customer_id: customer_id.to_string(),
            session_id: session_id.to_string(),
            setup_intent_id: setup_intent_id.to_string(),
        })
    } else {
        None
    };
    let accepted_failure_duplicate =
        if event.kind == "invoice.payment_failed" && billing.personal_coverage_ingest_enabled() {
            if let Some((coverage, _)) = billing.personal_coverage() {
                repair_event_already_completed(
                    &state.pool,
                    &coverage.account_id,
                    coverage.environment,
                    &event,
                )
                .await?
            } else {
                false
            }
        } else {
            false
        };
    let (personal_coverage, coverage_rejection) = if event.kind == "invoice.paid"
        && billing.personal_coverage_ingest_enabled()
        && !processed_duplicate
    {
        match prepare_personal_invoice_coverage(
            &state.pool,
            billing,
            &body,
            PersonalInvoiceEventSource::Signed { signature, now },
        )
        .await?
        {
            PersonalInvoiceCoveragePreparation::NotApplicable => (None, None),
            PersonalInvoiceCoveragePreparation::Prepared(coverage) => (Some(*coverage), None),
            PersonalInvoiceCoveragePreparation::Rejected(reason) => (None, Some(reason)),
        }
    } else {
        (None, None)
    };
    let mut sponsored_rejection = None;
    // A transfer destination subscription may not yet be linked to the personal account. Do not
    // persist that association from webhook metadata alone: wait until the invoice and its
    // payment have been read back from Stripe and their period, price and binding all agree.
    if let Some(coverage) = personal_coverage.as_ref() {
        if let Some(transfer_id) = coverage.transfer_id.as_deref() {
            link_prepared_personal_transfer_invoice(&state.pool, coverage, transfer_id).await?;
        }
    }
    let personal_renewal_failure = if event.kind == "invoice.payment_failed"
        && billing.personal_coverage_ingest_enabled()
        && !accepted_failure_duplicate
    {
        prepare_signed_personal_renewal_failure(&state.pool, billing, &body, signature, now).await?
    } else {
        None
    };
    let sponsored_renewal_failure = if event.kind == "invoice.payment_failed"
        && billing.personal_coverage_ingest_enabled()
        && sponsored_billing_enabled()
        && !accepted_failure_duplicate
    {
        Some(prepare_sponsored_renewal_failure(&state.pool, billing, &body, &event).await?)
    } else {
        None
    };
    let sponsored_coverage = if event.kind == "invoice.paid"
        && billing.personal_coverage_ingest_enabled()
        && sponsored_billing_enabled()
        && !processed_duplicate
    {
        Some(prepare_sponsored_invoice_coverage(&state.pool, billing, &body, &event).await?)
    } else {
        None
    };

    let mut tx = state.pool.begin().await?;
    prune_webhook_events(&mut tx).await?;
    let disposition = record_webhook_event(&mut tx, &event).await?;
    if disposition == EventDisposition::Ignore {
        tx.commit().await?;
        return Ok(());
    }

    let object = &event.data.object;
    let subscription_id = event_subscription_id(&event);
    let personal_period = if is_personal_checkout_event(&event) {
        if let Some(subscription_id) = subscription_id.as_deref() {
            billing
                .provider
                .personal_subscription_period(subscription_id)
                .await
                .map_err(ProviderError::into_error)?
        } else {
            None
        }
    } else {
        None
    };
    let sponsored_snapshot = if sponsored_billing_enabled() && is_sponsored_checkout_event(&event) {
        if let Some(subscription_id) = subscription_id.as_deref() {
            Some(
                billing
                    .provider
                    .get_sponsored_subscription(subscription_id)
                    .await
                    .map_err(ProviderError::into_error)?,
            )
        } else {
            None
        }
    } else {
        None
    };
    if disposition == EventDisposition::Reconcile {
        if is_checkout_expired_event(&event) {
            sponsored_checkout_expired(&mut tx, object).await?;
            mark_webhook_event_processed(&mut tx, &event.id).await?;
            tx.commit().await?;
            return Ok(());
        }
        if let Some(setup) = transfer_setup.as_ref() {
            accept_transfer_setup(&mut tx, setup).await?;
            mark_webhook_event_processed(&mut tx, &event.id).await?;
            tx.commit().await?;
            return Ok(());
        }
        if is_personal_checkout_event(&event) || is_sponsored_checkout_event(&event) {
            // A personal checkout has no organisation tier to reconcile. Re-apply the verified
            // settlement instead; the personal account store makes this equal-timestamp replay
            // idempotent while still requiring the paid webhook evidence.
            checkout_completed(
                &mut tx,
                object,
                event.created,
                event.kind == "checkout.session.async_payment_succeeded",
                personal_period,
                sponsored_snapshot.as_ref(),
            )
            .await?;
            if let Some(subscription_id) = subscription_id {
                update_subscription_watermark(&mut tx, &event, &subscription_id).await?;
            }
            mark_webhook_event_processed(&mut tx, &event.id).await?;
            tx.commit().await?;
            return Ok(());
        }
        if let Some(failure) = personal_renewal_failure.as_ref() {
            accept_prepared_personal_renewal_failure(&mut tx, failure).await?;
            mark_webhook_event_processed(&mut tx, &event.id).await?;
            tx.commit().await?;
            return Ok(());
        }
        if let Some(failure) = sponsored_renewal_failure.as_ref() {
            apply_sponsored_renewal_failure(
                &mut tx,
                failure,
                crate::cloud_provider_stripe_renewal_store::StripeRenewalFailureProvenance::SignedWebhook,
            )
            .await?;
            mark_webhook_event_processed(&mut tx, &event.id).await?;
            tx.commit().await?;
            return Ok(());
        }
        let subscription_id = subscription_id.ok_or_else(|| {
            Error::Config("stripe reconciliation event has no subscription id".into())
        })?;
        // Equal-timestamp events must not fetch snapshots concurrently: a slower, older lookup
        // could otherwise overwrite a newer provider observation after the event tie-break.
        // Holding this short transaction's advisory lock across the bounded provider call trades
        // one pool connection for a cross-instance ordering guarantee.
        let observation = billing
            .provider
            .get_subscription(&subscription_id)
            .await
            .map_err(ProviderError::into_error)?;
        reconcile_subscription(
            &mut tx,
            observation,
            &subscription_id,
            event_org_hint(&event),
            event_is_sponsored_subscription(&event),
        )
        .await?;
        update_subscription_watermark(&mut tx, &event, &subscription_id).await?;
        mark_webhook_event_processed(&mut tx, &event.id).await?;
        tx.commit().await?;
        return Ok(());
    }

    if matches!(disposition, EventDisposition::Apply) {
        if let Some(setup) = transfer_setup.as_ref() {
            accept_transfer_setup(&mut tx, setup).await?;
            if let Some(subscription_id) = subscription_id.as_deref() {
                update_subscription_watermark(&mut tx, &event, subscription_id).await?;
            }
            mark_webhook_event_processed(&mut tx, &event.id).await?;
            tx.commit().await?;
            return Ok(());
        }
    }

    match disposition {
        EventDisposition::Apply => match event.kind.as_str() {
            "checkout.session.completed" | "checkout.session.async_payment_succeeded" => {
                checkout_completed(
                    &mut tx,
                    object,
                    event.created,
                    event.kind == "checkout.session.async_payment_succeeded",
                    personal_period,
                    sponsored_snapshot.as_ref(),
                )
                .await?
            }
            "checkout.session.expired" => sponsored_checkout_expired(&mut tx, object).await?,
            "customer.subscription.updated" => subscription_updated(&mut tx, object).await?,
            "customer.subscription.deleted" => subscription_deleted(&mut tx, object).await?,
            "invoice.paid" => {
                sponsored_rejection = invoice_paid(
                    &mut tx,
                    object,
                    event.created,
                    billing.price_catalogue(),
                    personal_coverage.as_ref(),
                    sponsored_coverage.as_ref(),
                )
                .await?;
                if let Some(coverage) = &personal_coverage {
                    accept_personal_invoice_coverage(
                        &mut tx,
                        &coverage.context,
                        &coverage.event,
                        &coverage.allocation,
                        coverage.previous_provider_item_id.as_deref(),
                        coverage.provider_item_rebind_allowed,
                        true,
                    )
                    .await?;
                    record_transfer_invoice_payment(
                        &mut tx,
                        &coverage.operation_id,
                        &coverage.allocation.beneficiary_id,
                        TransferPayer::Personal,
                        None,
                        &coverage.offer,
                        &coverage.allocation.provider_customer_id,
                        subscription_id.as_deref().unwrap_or_default(),
                        &coverage.payment_reference,
                        coverage.period_start,
                        coverage.period_end,
                    )
                    .await?;
                }
                if let Some(SponsoredInvoiceCoveragePreparation::Prepared(prepared)) =
                    sponsored_coverage.as_ref()
                {
                    let customer_id = object
                        .get("customer")
                        .and_then(stripe_reference)
                        .ok_or_else(|| {
                            Error::Config("paid sponsored invoice has no customer".into())
                        })?;
                    let subscription_id = invoice_subscription_id(object).ok_or_else(|| {
                        Error::Config("paid sponsored invoice has no subscription".into())
                    })?;
                    for seat in &prepared.seats {
                        record_transfer_invoice_payment(
                            &mut tx,
                            &seat.operation_id,
                            &seat.allocation.beneficiary_id,
                            TransferPayer::Sponsor,
                            Some(&prepared.organization_id),
                            seat.offer.as_str(),
                            &customer_id,
                            subscription_id,
                            &prepared.payment_reference,
                            seat.period_start,
                            seat.period_end,
                        )
                        .await?;
                    }
                }
            }
            "invoice.payment_failed" => {
                if let Some(failure) = personal_renewal_failure.as_ref() {
                    accept_prepared_personal_renewal_failure(&mut tx, failure).await?;
                }
                if let Some(failure) = sponsored_renewal_failure.as_ref() {
                    apply_sponsored_renewal_failure(
                        &mut tx,
                        failure,
                        crate::cloud_provider_stripe_renewal_store::StripeRenewalFailureProvenance::SignedWebhook,
                    )
                    .await?;
                }
            }
            _ => {}
        },
        EventDisposition::Reconcile | EventDisposition::Ignore => {
            return Err(Error::Config("invalid webhook disposition".into()));
        }
    }
    if event_updates_subscription_watermark(&event) {
        if let Some(subscription_id) = subscription_id {
            update_subscription_watermark(&mut tx, &event, &subscription_id).await?;
        }
    }
    mark_webhook_event_processed(&mut tx, &event.id).await?;
    tx.commit().await?;
    if let Some(reason) = coverage_rejection {
        if let Some((coverage, _)) = billing.personal_coverage() {
            if let Err(error) = crate::cloud_operations::record_coverage_rejection(
                &state.pool,
                &coverage.account_id,
                coverage.environment,
                &event.id,
                "personal",
                reason,
            )
            .await
            {
                eprintln!(
                    "error: failed to persist personal coverage rejection diagnostic: {error}"
                );
            }
        }
        eprintln!(
            "error: accepted Stripe invoice event {} for billing without personal Cloud coverage: {}",
            event.id, reason
        );
    }
    let sponsored_rejection = sponsored_rejection.or(match sponsored_coverage.as_ref() {
        Some(SponsoredInvoiceCoveragePreparation::Rejected { reason, .. }) => Some(*reason),
        _ => None,
    });
    if let Some(reason) = sponsored_rejection {
        if let Some((coverage, _)) = billing.personal_coverage() {
            if let Err(error) = crate::cloud_operations::record_coverage_rejection(
                &state.pool,
                &coverage.account_id,
                coverage.environment,
                &event.id,
                "sponsored",
                reason,
            )
            .await
            {
                eprintln!(
                    "error: failed to persist sponsored coverage rejection diagnostic: {error}"
                );
            }
        }
        eprintln!(
            "error: accepted Stripe invoice event {} for billing without sponsored Cloud coverage: {}",
            event.id, reason
        );
    }
    Ok(())
}

async fn link_prepared_personal_transfer_invoice(
    pool: &PgPool,
    coverage: &PreparedPersonalInvoiceCoverage,
    transfer_id: &str,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    founding_allocator::lock_capacity(&mut tx)
        .await
        .map_err(|error| Error::Internal(error.to_string()))?;
    sqlx::query("SELECT id FROM users WHERE id = $1 FOR UPDATE")
        .bind(&coverage.allocation.beneficiary_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::NotFound("billing beneficiary not found".into()))?;
    billing_transfers::link_personal_destination_subscription(
        &mut tx,
        transfer_id,
        &coverage.allocation.beneficiary_id,
        &coverage.operation_id,
        &coverage.allocation.external_allocation_reference,
        &coverage.allocation.provider_customer_id,
        &coverage.allocation.subscription_id,
        &coverage.offer,
        coverage.period_start,
    )
    .await
    .map_err(Error::from)?;
    tx.commit().await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn record_transfer_invoice_payment(
    tx: &mut Transaction<'_, Postgres>,
    operation_id: &str,
    beneficiary_id: &str,
    destination_kind: billing_transfers::TransferPayer,
    destination_organization_id: Option<&str>,
    offer: &str,
    customer_id: &str,
    subscription_id: &str,
    payment_reference: &str,
    period_start: i64,
    period_end: i64,
) -> Result<()> {
    let restoration = sqlx::query(
        "SELECT transfer_id, beneficiary_id, source_kind, source_organization_id, offer, state, \
                source_restoration_provider_subscription_id, source_restoration_payment_reference, \
                effective_from, (SELECT account.offer FROM billing_personal_accounts account \
                  WHERE account.user_id = billing_transfer_intents.beneficiary_id \
                    AND account.operation_id = billing_transfer_intents.source_restoration_operation_id \
                    AND account.stripe_customer_id = $2 \
                    AND account.stripe_subscription_id = billing_transfer_intents.source_restoration_provider_subscription_id) \
                    AS account_offer \
         FROM billing_transfer_intents WHERE source_restoration_operation_id = $1 FOR UPDATE",
    )
    .bind(operation_id)
    .bind(customer_id)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some(row) = restoration {
        let transfer_id: String = row.try_get("transfer_id")?;
        let source_kind: String = row.try_get("source_kind")?;
        let source_organization_id: Option<String> = row.try_get("source_organization_id")?;
        let restored_subscription: Option<String> =
            row.try_get("source_restoration_provider_subscription_id")?;
        let recorded_payment: Option<String> =
            row.try_get("source_restoration_payment_reference")?;
        let state: String = row.try_get("state")?;
        let effective_from: i64 = row.try_get("effective_from")?;
        let restoration_offer: String = row.try_get("offer")?;
        let account_offer: Option<String> = row.try_get("account_offer")?;
        if row.try_get::<String, _>("beneficiary_id")? != beneficiary_id
            || source_kind != destination_kind.as_str()
            || source_organization_id.as_deref() != destination_organization_id
            || account_offer.is_none()
            || restored_subscription.as_deref() != Some(subscription_id)
        {
            return Err(Error::Conflict(
                "paid invoice does not match its source restoration".into(),
            ));
        }
        // The restoration operation id remains attached to the account after recovery. Only
        // its first paid invoice is transfer evidence; later renewals belong to normal billing.
        if recorded_payment.is_some()
            || !matches!(state.as_str(), "source_restoration_pending" | "unknown")
        {
            return Ok(());
        }
        if restoration_offer != offer
            || account_offer.as_deref() != Some(offer)
            || period_start < effective_from
            || period_end <= period_start
        {
            return Err(Error::Conflict(
                "paid invoice does not match its source restoration".into(),
            ));
        }
        billing_transfers::record_source_restoration_payment(
            tx,
            &transfer_id,
            operation_id,
            subscription_id,
            payment_reference,
        )
        .await
        .map_err(Error::from)?;
        return Ok(());
    }

    let row = sqlx::query(
        "SELECT transfer_id, beneficiary_id, destination_kind, destination_organization_id, \
                destination_customer_id, destination_provider_subscription_id, offer, effective_from, \
                state, destination_payment_reference \
         FROM billing_transfer_intents WHERE destination_operation_id = $1 FOR UPDATE",
    )
    .bind(operation_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row else {
        return Ok(());
    };
    let transfer_id: String = row.try_get("transfer_id")?;
    let stored_kind: String = row.try_get("destination_kind")?;
    let stored_organization: Option<String> = row.try_get("destination_organization_id")?;
    let stored_customer: Option<String> = row.try_get("destination_customer_id")?;
    let stored_subscription: Option<String> =
        row.try_get("destination_provider_subscription_id")?;
    let effective_from: i64 = row.try_get("effective_from")?;
    let state: String = row.try_get("state")?;
    let recorded_payment: Option<String> = row.try_get("destination_payment_reference")?;
    if row.try_get::<String, _>("beneficiary_id")? != beneficiary_id
        || stored_kind != destination_kind.as_str()
        || stored_organization.as_deref() != destination_organization_id
        || stored_customer.as_deref() != Some(customer_id)
        || stored_subscription.as_deref() != Some(subscription_id)
    {
        return Err(Error::Conflict(
            "paid invoice does not match its payer transfer".into(),
        ));
    }
    // Destination operation ids stay on the account/seat after the transfer completes. Do
    // not reinterpret later renewals as transfer evidence or compare them to the boundary
    // invoice's payment reference.
    if recorded_payment.is_some()
        || !matches!(
            state.as_str(),
            "destination_prepared" | "source_adjustment_pending" | "unknown"
        )
    {
        return Ok(());
    }
    // An invoice for the paid term ending at the transfer boundary is still the source term.
    // A period that overlaps or follows the boundary is relevant: record_destination_paid
    // will park a wrong start/end for operator review instead of aborting invoice settlement.
    if period_end <= effective_from {
        return Ok(());
    }
    if row.try_get::<String, _>("offer")? != offer {
        return Err(Error::Conflict(
            "paid invoice does not match its payer transfer".into(),
        ));
    }
    billing_transfers::record_destination_paid(
        tx,
        &transfer_id,
        payment_reference,
        period_start,
        period_end,
    )
    .await
    .map_err(Error::from)?;
    Ok(())
}

async fn repair_event_already_completed(
    pool: &sqlx::PgPool,
    account_id: &str,
    environment: ProviderEnvironment,
    event: &Event,
) -> Result<bool> {
    let processed: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM stripe_webhook_events \
         WHERE event_id = $1 AND processed_at IS NOT NULL)",
    )
    .bind(&event.id)
    .fetch_one(pool)
    .await?;
    if !processed {
        return Ok(false);
    }
    match event.kind.as_str() {
        "invoice.paid" => {
            // Billing may already be committed even though permanent coverage rejection left a
            // durable diagnostic open. Let the authenticated repair path re-read that invoice and
            // retry only coverage; never replay or roll back the paid billing transition.
            let repairable: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM cloud_coverage_rejection_incidents \
                 WHERE provider_account_id = $1 AND provider_environment = $2 \
                   AND event_id = $3 AND state IN ('open','repairing'))",
            )
            .bind(account_id)
            .bind(environment.as_str())
            .bind(&event.id)
            .fetch_one(pool)
            .await?;
            if !should_skip_processed_paid_invoice(true, repairable) {
                return Ok(false);
            }
            Ok(sqlx::query_scalar(
                "SELECT EXISTS( \
                 SELECT 1 FROM cloud_provider_event_receipts receipt \
                 WHERE receipt.provider_namespace = 'stripe' \
                   AND receipt.provider_account_id = $1 \
                   AND receipt.provider_environment = $2 \
                   AND receipt.event_id = $3 \
                   AND receipt.status IN ('applied', 'rejected') \
             )",
            )
            .bind(account_id)
            .bind(environment.as_str())
            .bind(&event.id)
            .fetch_one(pool)
            .await?)
        }
        "invoice.payment_failed" => Ok(sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM cloud_provider_stripe_renewal_failures \
             WHERE provider_namespace = 'stripe' AND provider_account_id = $1 \
               AND provider_environment = $2 AND event_id = $3)",
        )
        .bind(account_id)
        .bind(environment.as_str())
        .bind(&event.id)
        .fetch_one(pool)
        .await?),
        _ => Ok(false),
    }
}

fn should_skip_processed_paid_invoice(processed: bool, coverage_repair_open: bool) -> bool {
    processed && !coverage_repair_open
}

pub(crate) async fn repair_authenticated_stripe_event(
    pool: &sqlx::PgPool,
    billing: &BillingState,
    authenticated: &AuthenticatedStripeEvent,
    now_epoch: i64,
) -> Result<StripeEventRepairDisposition> {
    let event: Event = serde_json::from_value(authenticated.payload().clone())
        .map_err(|_| Error::BadRequest("malformed authenticated Stripe event".into()))?;
    if event.id != authenticated.event_id() || event.created < 0 {
        return Ok(StripeEventRepairDisposition::Unresolved(
            "event_identity_mismatch",
        ));
    }
    if !webhook_version_accepted(event.api_version.as_deref()) {
        return Ok(StripeEventRepairDisposition::Unresolved(
            "unsupported_api_version",
        ));
    }
    if !matches!(
        event.kind.as_str(),
        "invoice.paid" | "invoice.payment_failed"
    ) {
        return Ok(StripeEventRepairDisposition::Unrelated);
    }
    if repair_event_already_completed(
        pool,
        authenticated.account_id(),
        authenticated.environment(),
        &event,
    )
    .await?
    {
        return Ok(StripeEventRepairDisposition::AlreadyApplied);
    }
    if event.kind == "invoice.payment_failed" {
        if sponsored_billing_enabled() {
            let sponsored: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM billing_sponsored_subscriptions WHERE provider_subscription_id = $1)",
            )
            .bind(invoice_subscription_id_claim(&event.data.object).unwrap_or_default())
            .fetch_one(pool)
            .await?;
            if sponsored {
                return repair_authenticated_sponsored_renewal_failure(
                    pool,
                    billing,
                    authenticated,
                    &event,
                )
                .await;
            }
        }
        return repair_authenticated_personal_renewal_failure(
            pool,
            billing,
            authenticated,
            now_epoch,
        )
        .await;
    }
    let Some(subscription_id) = invoice_subscription_id_claim(&event.data.object) else {
        return Ok(StripeEventRepairDisposition::Unrelated);
    };
    let (personal_subscription, sponsored_subscription): (bool, bool) = sqlx::query_as(
        "SELECT EXISTS(SELECT 1 FROM billing_personal_accounts \
                          WHERE stripe_subscription_id = $1), \
                EXISTS(SELECT 1 FROM billing_sponsored_subscriptions \
                          WHERE provider_subscription_id = $1)",
    )
    .bind(&subscription_id)
    .fetch_one(pool)
    .await?;
    let settlement_expected =
        personal_subscription || (sponsored_billing_enabled() && sponsored_subscription);

    let raw_event = serde_json::to_string(authenticated.payload())
        .map_err(|_| Error::BadRequest("malformed authenticated Stripe event".into()))?;
    let (personal_coverage, personal_rejection) = if billing.personal_coverage().is_some() {
        match prepare_personal_invoice_coverage(
            pool,
            billing,
            &raw_event,
            PersonalInvoiceEventSource::AuthenticatedRead(authenticated),
        )
        .await?
        {
            PersonalInvoiceCoveragePreparation::NotApplicable => (None, None),
            PersonalInvoiceCoveragePreparation::Prepared(coverage) => (Some(*coverage), None),
            PersonalInvoiceCoveragePreparation::Rejected(reason) => (None, Some(reason)),
        }
    } else {
        (None, None)
    };
    let sponsored_coverage = if sponsored_billing_enabled() && billing.personal_coverage().is_some()
    {
        Some(prepare_sponsored_invoice_coverage(pool, billing, &raw_event, &event).await?)
    } else {
        None
    };
    let prepared_period_end = match personal_coverage.as_ref() {
        Some(PreparedPersonalInvoiceCoverage { period_end, .. }) => Some(*period_end),
        _ => None,
    };
    if let Some(reason) = paid_invoice_repair_shape_error(
        &event.data.object,
        prepared_period_end,
        settlement_expected,
    ) {
        return Ok(StripeEventRepairDisposition::Unresolved(reason));
    }

    let mut tx = pool.begin().await?;
    prune_webhook_events(&mut tx).await?;
    let disposition = record_webhook_event(&mut tx, &event).await?;
    if disposition == EventDisposition::Ignore {
        if let Some(sponsored) = sponsored_coverage.as_ref() {
            let evidence = match sponsored {
                SponsoredInvoiceCoveragePreparation::Prepared(prepared) => {
                    Some((&prepared.context, &prepared.event))
                }
                SponsoredInvoiceCoveragePreparation::Rejected { context, event, .. } => {
                    Some((context, event.as_ref()))
                }
                SponsoredInvoiceCoveragePreparation::NotApplicable => None,
            };
            if let Some((context, evidence)) = evidence {
                reopen_rejected_event_for_repair(&mut tx, context, evidence)
                    .await
                    .map_err(|_| {
                        Error::Internal("sponsored provider repair receipt changed".into())
                    })?;
            }
        }
        if let Some(coverage) = &personal_coverage {
            accept_personal_invoice_coverage(
                &mut tx,
                &coverage.context,
                &coverage.event,
                &coverage.allocation,
                coverage.previous_provider_item_id.as_deref(),
                coverage.provider_item_rebind_allowed,
                true,
            )
            .await?;
        }
        let mut sponsored_rejection = None;
        if let Some(SponsoredInvoiceCoveragePreparation::Prepared(prepared)) =
            sponsored_coverage.as_ref()
        {
            if !accept_sponsored_invoice_coverage(&mut tx, prepared).await? {
                sponsored_rejection = Some("sponsored_invoice_no_eligible_seats");
            }
        }
        if let Some(SponsoredInvoiceCoveragePreparation::Rejected {
            context,
            event,
            reason,
        }) = sponsored_coverage.as_ref()
        {
            reject_sponsored_invoice_coverage(&mut tx, context, event, reason).await?;
            sponsored_rejection = Some(*reason);
        }
        tx.commit().await?;
        if (personal_coverage.is_some()
            || matches!(
                sponsored_coverage.as_ref(),
                Some(SponsoredInvoiceCoveragePreparation::Prepared(_))
            ))
            && personal_rejection.is_none()
            && sponsored_rejection.is_none()
            && !matches!(
                sponsored_coverage.as_ref(),
                Some(SponsoredInvoiceCoveragePreparation::Rejected { .. })
            )
        {
            if let Err(error) = crate::cloud_operations::resolve_coverage_rejections(
                pool,
                authenticated.account_id(),
                authenticated.environment(),
                authenticated.event_id(),
            )
            .await
            {
                eprintln!("error: failed to resolve coverage rejection diagnostic: {error}");
            }
        }
        if let Some(reason) = personal_rejection {
            if let Err(error) = crate::cloud_operations::record_coverage_rejection(
                pool,
                authenticated.account_id(),
                authenticated.environment(),
                authenticated.event_id(),
                "personal",
                reason,
            )
            .await
            {
                eprintln!(
                    "error: failed to persist personal coverage rejection diagnostic: {error}"
                );
            }
            return Ok(StripeEventRepairDisposition::Rejected(reason));
        }
        if let Some(reason) = sponsored_rejection {
            if let Err(error) = crate::cloud_operations::record_coverage_rejection(
                pool,
                authenticated.account_id(),
                authenticated.environment(),
                authenticated.event_id(),
                "sponsored",
                reason,
            )
            .await
            {
                eprintln!(
                    "error: failed to persist sponsored coverage rejection diagnostic: {error}"
                );
            }
            return Ok(StripeEventRepairDisposition::Rejected(reason));
        }
        if personal_coverage.is_some()
            || matches!(
                sponsored_coverage.as_ref(),
                Some(SponsoredInvoiceCoveragePreparation::Prepared(_))
            )
        {
            return Ok(StripeEventRepairDisposition::Applied);
        }
        return Ok(StripeEventRepairDisposition::AlreadyApplied);
    }
    if disposition != EventDisposition::Apply {
        return Err(Error::Internal(
            "authenticated invoice event has an invalid disposition".into(),
        ));
    }
    let sponsored_rejection = invoice_paid(
        &mut tx,
        &event.data.object,
        event.created,
        billing.price_catalogue(),
        personal_coverage.as_ref(),
        sponsored_coverage.as_ref(),
    )
    .await?;
    if let Some(coverage) = &personal_coverage {
        accept_personal_invoice_coverage(
            &mut tx,
            &coverage.context,
            &coverage.event,
            &coverage.allocation,
            coverage.previous_provider_item_id.as_deref(),
            coverage.provider_item_rebind_allowed,
            true,
        )
        .await?;
    }
    complete_repaired_invoice_event(&mut tx, &event).await?;
    tx.commit().await?;

    if (personal_coverage.is_some()
        || matches!(
            sponsored_coverage.as_ref(),
            Some(SponsoredInvoiceCoveragePreparation::Prepared(_))
        ))
        && personal_rejection.is_none()
        && sponsored_rejection.is_none()
        && !matches!(
            sponsored_coverage.as_ref(),
            Some(SponsoredInvoiceCoveragePreparation::Rejected { .. })
        )
    {
        if let Err(error) = crate::cloud_operations::resolve_coverage_rejections(
            pool,
            authenticated.account_id(),
            authenticated.environment(),
            authenticated.event_id(),
        )
        .await
        {
            eprintln!("error: failed to resolve coverage rejection diagnostic: {error}");
        }
    }

    if let Some(reason) = personal_rejection {
        if let Err(error) = crate::cloud_operations::record_coverage_rejection(
            pool,
            authenticated.account_id(),
            authenticated.environment(),
            authenticated.event_id(),
            "personal",
            reason,
        )
        .await
        {
            eprintln!("error: failed to persist personal coverage rejection diagnostic: {error}");
        }
        eprintln!(
            "error: repaired Stripe invoice settled without personal Cloud coverage: {reason}"
        );
        return Ok(StripeEventRepairDisposition::Rejected(reason));
    }
    if let Some(reason) = sponsored_rejection {
        if let Err(error) = crate::cloud_operations::record_coverage_rejection(
            pool,
            authenticated.account_id(),
            authenticated.environment(),
            authenticated.event_id(),
            "sponsored",
            reason,
        )
        .await
        {
            eprintln!("error: failed to persist sponsored coverage rejection diagnostic: {error}");
        }
        return Ok(StripeEventRepairDisposition::Rejected(reason));
    }
    Ok(StripeEventRepairDisposition::Applied)
}

async fn repair_authenticated_personal_renewal_failure(
    pool: &sqlx::PgPool,
    billing: &BillingState,
    authenticated: &AuthenticatedStripeEvent,
    now_epoch: i64,
) -> Result<StripeEventRepairDisposition> {
    let Some((coverage_config, stripe)) = billing.personal_coverage() else {
        return Ok(StripeEventRepairDisposition::Unresolved(
            "personal_coverage_not_configured",
        ));
    };
    let payload = authenticated.payload();
    let invoice = payload
        .get("data")
        .and_then(|data| data.get("object"))
        .ok_or_else(|| Error::BadRequest("malformed authenticated Stripe event".into()))?;
    let Some(allocation_reference) = invoice_allocation_reference_claim(invoice) else {
        return Ok(StripeEventRepairDisposition::Unresolved(
            "allocation_reference_missing",
        ));
    };
    let row = sqlx::query(
        "SELECT allocation.allocation_id, allocation.payer_id, allocation.beneficiary_id, \
                allocation.provider_subscription_id, allocation.provider_item_id, \
                allocation.external_allocation_reference, allocation.coverage_source_id, \
                allocation.effective_from, allocation.effective_until, allocation.state, \
                allocation.ownership_evidence_reference, allocation.provider_account_id, \
                allocation.provider_environment, payer.provider_customer_id, payer.payer_kind \
         FROM cloud_provider_allocations AS allocation \
         JOIN cloud_provider_payers AS payer ON payer.payer_id = allocation.payer_id \
         WHERE allocation.provider_namespace = 'stripe' \
           AND allocation.provider_account_id = $1 AND allocation.provider_environment = $2 \
           AND allocation.external_allocation_reference = $3",
    )
    .bind(&coverage_config.account_id)
    .bind(coverage_config.environment.as_str())
    .bind(&allocation_reference)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(StripeEventRepairDisposition::Unrelated);
    };
    if row.try_get::<String, _>("payer_kind")? != "personal" {
        return Ok(StripeEventRepairDisposition::Unresolved(
            "sponsored_failure_unsupported",
        ));
    }
    let customer_id: String = row.try_get("provider_customer_id")?;
    let subscription_id: String = row.try_get("provider_subscription_id")?;
    let item_id: String = row.try_get("provider_item_id")?;
    let binding = StripeAllocationBinding::new(
        allocation_reference,
        customer_id.clone(),
        subscription_id.clone(),
        item_id,
        ProviderPayerKind::Personal,
    )
    .map_err(|_| Error::Internal("stored Stripe allocation binding is invalid".into()))?;
    let allocation = VerifiedAllocation::new(
        row.try_get::<String, _>("allocation_id")?,
        row.try_get::<String, _>("payer_id")?,
        customer_id,
        ProviderPayerKind::Personal,
        row.try_get::<String, _>("beneficiary_id")?,
        subscription_id,
        binding.provider_item_id().to_owned(),
        row.try_get::<String, _>("external_allocation_reference")?,
        row.try_get::<String, _>("coverage_source_id")?,
        row.try_get::<i64, _>("effective_from")?,
        row.try_get::<Option<i64>, _>("effective_until")?,
        parse_allocation_state(&row.try_get::<String, _>("state")?)?,
        row.try_get::<String, _>("ownership_evidence_reference")?,
    )
    .map_err(|_| Error::Internal("stored Stripe allocation is invalid".into()))?;
    if row.try_get::<String, _>("provider_account_id")? != coverage_config.account_id
        || row.try_get::<String, _>("provider_environment")? != coverage_config.environment.as_str()
    {
        return Ok(StripeEventRepairDisposition::Unresolved(
            "allocation_context_mismatch",
        ));
    }

    let mut session = stripe.session();
    let history_result = stripe
        .personal_invoice_history(&mut session, &binding)
        .await
        .map_err(|_| Error::Internal("Stripe renewal history read failed".into()))?;
    let history = match history_result {
        crate::cloud_provider_stripe_http::StripePersonalInvoiceHistoryResult::Observed(
            history,
        ) => history,
        crate::cloud_provider_stripe_http::StripePersonalInvoiceHistoryResult::NeedsEvidence(_) => {
            return Ok(StripeEventRepairDisposition::Unresolved(
                "invoice_history_incomplete",
            ));
        }
    };
    let candidate = match crate::cloud_provider_stripe_repair::decode_authenticated_repair_event(
        payload,
        now_epoch,
        coverage_config,
        &binding,
    ) {
        Ok(candidate) => candidate,
        Err(crate::cloud_provider_stripe_repair::StripeRepairError::Unsupported(
            "personal allocation required",
        )) => {
            return Ok(StripeEventRepairDisposition::Unresolved(
                "sponsored_failure_unsupported",
            ));
        }
        Err(crate::cloud_provider_stripe_repair::StripeRepairError::Unsupported(_)) => {
            return Ok(StripeEventRepairDisposition::Rejected(
                "renewal_event_unsupported",
            ));
        }
        Err(crate::cloud_provider_stripe_repair::StripeRepairError::OwnershipMismatch) => {
            return Ok(StripeEventRepairDisposition::Unresolved(
                "renewal_event_binding_mismatch",
            ));
        }
        Err(crate::cloud_provider_stripe_repair::StripeRepairError::UnsupportedApiVersion(_)) => {
            return Ok(StripeEventRepairDisposition::Unresolved(
                "unsupported_api_version",
            ));
        }
        Err(crate::cloud_provider_stripe_repair::StripeRepairError::Read(error)) => {
            return Err(Error::Internal(format!(
                "Stripe renewal repair read failed: {error}"
            )));
        }
        Err(_) => {
            return Ok(StripeEventRepairDisposition::Rejected(
                "renewal_event_malformed",
            ));
        }
    };
    let failure_invoice_is_open = stripe
        .personal_renewal_failure_invoice_is_open_and_unpaid(
            &mut session,
            candidate.invoice_id(),
            &binding,
        )
        .await
        .map_err(|_| Error::Internal("Stripe failed invoice detail read failed".into()))?;
    if !failure_invoice_is_open {
        return Ok(StripeEventRepairDisposition::Unresolved(
            "renewal_invoice_not_open_or_unpaid",
        ));
    }
    let linked = crate::cloud_provider_stripe_renewals::link_repaired_candidate(
        &candidate,
        coverage_config,
        &binding,
        &history,
    )
    .map_err(|_| Error::Internal("Stripe renewal history could not be linked".into()))?;
    let evidence = match linked {
        crate::cloud_provider_stripe_renewals::StripeRenewalFailureResult::Linked(evidence) => {
            *evidence
        }
        crate::cloud_provider_stripe_renewals::StripeRenewalFailureResult::NeedsEvidence(
            reason,
        ) => {
            return Ok(StripeEventRepairDisposition::Unresolved(match reason {
                crate::cloud_provider_stripe_renewals::StripeRenewalFailureNeedsEvidence::TruncatedInvoiceLines => "renewal_history_truncated",
                crate::cloud_provider_stripe_renewals::StripeRenewalFailureNeedsEvidence::MissingProrationProof => "renewal_proration_unproven",
                crate::cloud_provider_stripe_renewals::StripeRenewalFailureNeedsEvidence::NoMatchingPaidPredecessor => "renewal_predecessor_missing",
                crate::cloud_provider_stripe_renewals::StripeRenewalFailureNeedsEvidence::AmbiguousPaidPredecessor => "renewal_predecessor_ambiguous",
            }));
        }
    };
    let context = coverage_config
        .provider_context()
        .map_err(|_| Error::Internal("Stripe provider context is invalid".into()))?;
    let mut tx = pool.begin().await?;
    let subscription_id = Some(evidence.subscription_id());
    lock_subscription(&mut tx, evidence.subscription_id()).await?;
    record_webhook_receipt(
        &mut tx,
        &Event {
            id: evidence.event_id().to_owned(),
            created: evidence.event_created_at(),
            api_version: payload
                .get("api_version")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            kind: "invoice.payment_failed".into(),
            data: EventData {
                object: payload["data"]["object"].clone(),
            },
        },
        subscription_id,
    )
    .await?;
    crate::cloud_provider_stripe_renewal_store::accept_personal_renewal_failure_with_provenance(
        &mut tx,
        &context,
        &allocation,
        &evidence,
        crate::cloud_provider_stripe_renewal_store::StripeRenewalFailureProvenance::AuthenticatedApiRead,
        true,
    )
    .await
    .map_err(|_| Error::Internal("personal renewal failure could not be recorded".into()))?;
    mark_webhook_event_processed(&mut tx, evidence.event_id()).await?;
    tx.commit().await?;
    Ok(StripeEventRepairDisposition::Applied)
}

async fn repair_authenticated_sponsored_renewal_failure(
    pool: &PgPool,
    billing: &BillingState,
    authenticated: &AuthenticatedStripeEvent,
    event: &Event,
) -> Result<StripeEventRepairDisposition> {
    let raw_event = serde_json::to_string(authenticated.payload())
        .map_err(|_| Error::BadRequest("malformed authenticated Stripe event".into()))?;
    let prepared = prepare_sponsored_renewal_failure(pool, billing, &raw_event, event).await?;
    if matches!(&prepared, SponsoredRenewalFailurePreparation::NotApplicable) {
        return Ok(StripeEventRepairDisposition::Unrelated);
    }
    let rejection = match &prepared {
        SponsoredRenewalFailurePreparation::Rejected { reason, .. } => Some(*reason),
        _ => None,
    };
    let subscription_id = invoice_subscription_id_claim(&event.data.object);
    let mut tx = pool.begin().await?;
    if let Some(subscription_id) = subscription_id.as_deref() {
        lock_subscription(&mut tx, subscription_id).await?;
    }
    record_webhook_receipt(&mut tx, event, subscription_id.as_deref()).await?;
    apply_sponsored_renewal_failure(
        &mut tx,
        &prepared,
        crate::cloud_provider_stripe_renewal_store::StripeRenewalFailureProvenance::AuthenticatedApiRead,
    )
    .await?;
    mark_webhook_event_processed(&mut tx, &event.id).await?;
    tx.commit().await?;
    Ok(rejection.map_or(
        StripeEventRepairDisposition::Applied,
        StripeEventRepairDisposition::Rejected,
    ))
}

struct PreparedPersonalInvoiceCoverage {
    context: ProviderContext,
    event: crate::cloud_provider::VerifiedProviderEvent,
    allocation: VerifiedAllocation,
    operation_id: String,
    transfer_id: Option<String>,
    offer: String,
    payment_reference: String,
    period_start: i64,
    period_end: i64,
    previous_provider_item_id: Option<String>,
    provider_item_rebind_allowed: bool,
}

enum PersonalInvoiceCoveragePreparation {
    NotApplicable,
    Prepared(Box<PreparedPersonalInvoiceCoverage>),
    Rejected(&'static str),
}

enum PersonalInvoiceEventSource<'a> {
    Signed { signature: &'a str, now: i64 },
    AuthenticatedRead(&'a AuthenticatedStripeEvent),
}

struct PreparedPersonalRenewalFailure {
    context: ProviderContext,
    allocation: VerifiedAllocation,
    evidence: crate::cloud_provider_stripe_renewals::StripeRenewalFailureEvidence,
    previous_provider_item_id: Option<String>,
    provider_item_rebind_allowed: bool,
}

struct PreparedSponsoredRenewalFailure {
    context: ProviderContext,
    event: crate::cloud_provider::VerifiedProviderEvent,
    organization_id: String,
    subscription_id: String,
    invoice_id: String,
    affected: Vec<(
        VerifiedAllocation,
        crate::cloud_provider_stripe_renewals::StripeRenewalFailureEvidence,
    )>,
}

enum SponsoredRenewalFailurePreparation {
    NotApplicable,
    Prepared(Box<PreparedSponsoredRenewalFailure>),
    Rejected {
        context: ProviderContext,
        event: Box<crate::cloud_provider::VerifiedProviderEvent>,
        organization_id: String,
        subscription_id: String,
        invoice_id: String,
        period_start: i64,
        reason: &'static str,
    },
}

struct SponsoredFailurePredecessor {
    settlement: crate::cloud_provider_stripe_sponsored::SponsoredInvoiceSettlement,
    manifest: SponsoredAllocationManifest,
}

fn sponsored_invoice_lines_can_be_predecessors(
    predecessor_lines: &[crate::cloud_provider_stripe_http::StripeInvoiceLineResource],
    failed_lines: &[crate::cloud_provider_stripe_http::StripeInvoiceLineResource],
) -> bool {
    failed_lines.iter().any(|failed_line| {
        let (Some(item_id), Some(price_id), Some(start)) = (
            failed_line.subscription_item_id.as_deref(),
            failed_line.price_id.as_deref(),
            failed_line.period_start,
        ) else {
            return false;
        };
        predecessor_lines.iter().any(|line| {
            (line
                .subscription_item_id
                .as_deref()
                .or(line.legacy_subscription_item_id.as_deref())
                == Some(item_id))
                && line.price_id.as_deref() == Some(price_id)
                && line.period_end == Some(start)
        })
    })
}

async fn prepare_signed_personal_renewal_failure(
    pool: &sqlx::PgPool,
    billing: &BillingState,
    raw_event: &str,
    signature: &str,
    now_epoch: i64,
) -> Result<Option<PreparedPersonalRenewalFailure>> {
    let Some((coverage_config, stripe)) = billing.personal_coverage() else {
        return Ok(None);
    };
    let payload: serde_json::Value = serde_json::from_str(raw_event)
        .map_err(|_| Error::BadRequest("malformed signed Stripe event".into()))?;
    let invoice = &payload["data"]["object"];
    let Some(allocation_reference) = invoice_allocation_reference_claim(invoice) else {
        return Ok(None);
    };
    let Some(failed_line) = invoice
        .get("lines")
        .and_then(|lines| lines.get("data"))
        .and_then(serde_json::Value::as_array)
        .filter(|lines| lines.len() == 1)
        .and_then(|lines| lines.first())
    else {
        return Ok(None);
    };
    let Some(event_item_id) = failed_line
        .get("parent")
        .and_then(|parent| parent.get("subscription_item_details"))
        .and_then(|details| details.get("subscription_item"))
        .and_then(stripe_reference)
    else {
        return Ok(None);
    };
    let Some(failed_period_start) = failed_line
        .get("period")
        .and_then(|period| period.get("start"))
        .and_then(serde_json::Value::as_i64)
    else {
        return Ok(None);
    };
    let row = sqlx::query(
        "SELECT allocation.allocation_id, allocation.payer_id, allocation.beneficiary_id, \
                allocation.provider_subscription_id, allocation.provider_item_id, \
                allocation.external_allocation_reference, allocation.coverage_source_id, \
                allocation.effective_from, allocation.effective_until, allocation.state, \
                allocation.ownership_evidence_reference, allocation.provider_account_id, \
                allocation.provider_environment, payer.provider_customer_id, payer.payer_kind, \
                (SELECT paid_through_epoch FROM billing_personal_accounts \
                 WHERE user_id = allocation.beneficiary_id) AS personal_paid_through_epoch \
         FROM cloud_provider_allocations AS allocation \
         JOIN cloud_provider_payers AS payer ON payer.payer_id = allocation.payer_id \
         WHERE allocation.provider_namespace = 'stripe' \
           AND allocation.provider_account_id = $1 AND allocation.provider_environment = $2 \
           AND allocation.external_allocation_reference = $3",
    )
    .bind(&coverage_config.account_id)
    .bind(coverage_config.environment.as_str())
    .bind(&allocation_reference)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    if row.try_get::<String, _>("payer_kind")? != "personal" {
        return Ok(None);
    }
    let customer_id: String = row.try_get("provider_customer_id")?;
    let subscription_id: String = row.try_get("provider_subscription_id")?;
    let previous_provider_item_id: String = row.try_get("provider_item_id")?;
    let item_changed = previous_provider_item_id != event_item_id;
    let provider_item_rebind_allowed = item_changed
        && row.try_get::<Option<i64>, _>("personal_paid_through_epoch")?
            == Some(failed_period_start);
    if item_changed && !provider_item_rebind_allowed {
        return Ok(None);
    }
    let binding = StripeAllocationBinding::new(
        allocation_reference,
        customer_id.clone(),
        subscription_id.clone(),
        event_item_id.clone(),
        ProviderPayerKind::Personal,
    )
    .map_err(|_| Error::Internal("stored Stripe allocation binding is invalid".into()))?;
    let allocation = VerifiedAllocation::new(
        row.try_get::<String, _>("allocation_id")?,
        row.try_get::<String, _>("payer_id")?,
        customer_id,
        ProviderPayerKind::Personal,
        row.try_get::<String, _>("beneficiary_id")?,
        subscription_id.clone(),
        event_item_id,
        row.try_get::<String, _>("external_allocation_reference")?,
        row.try_get::<String, _>("coverage_source_id")?,
        row.try_get::<i64, _>("effective_from")?,
        row.try_get::<Option<i64>, _>("effective_until")?,
        parse_allocation_state(&row.try_get::<String, _>("state")?)?,
        row.try_get::<String, _>("ownership_evidence_reference")?,
    )
    .map_err(|_| Error::Internal("stored Stripe allocation is invalid".into()))?;
    if row.try_get::<String, _>("provider_account_id")? != coverage_config.account_id
        || row.try_get::<String, _>("provider_environment")? != coverage_config.environment.as_str()
    {
        return Ok(None);
    }

    let mut session = stripe.session();
    let Some(invoice_id) = invoice.get("id").and_then(stripe_reference) else {
        return Ok(None);
    };
    let history = match stripe
        .personal_invoice_history(&mut session, &binding)
        .await
        .map_err(|_| Error::Internal("Stripe renewal history read failed".into()))?
    {
        crate::cloud_provider_stripe_http::StripePersonalInvoiceHistoryResult::Observed(
            history,
        ) => history,
        crate::cloud_provider_stripe_http::StripePersonalInvoiceHistoryResult::NeedsEvidence(_) => {
            return Ok(None);
        }
    };
    let failure_invoice_is_open = stripe
        .personal_renewal_failure_invoice_is_open_and_unpaid(&mut session, &invoice_id, &binding)
        .await
        .map_err(|_| Error::Internal("Stripe failed invoice detail read failed".into()))?;
    if !failure_invoice_is_open {
        return Ok(None);
    }
    let result = crate::cloud_provider_stripe_renewals::decode_personal_renewal_failure(
        raw_event.as_bytes(),
        signature,
        &billing.webhook_secret,
        now_epoch,
        coverage_config,
        &binding,
        &history,
    );
    let evidence = match result {
        Ok(crate::cloud_provider_stripe_renewals::StripeRenewalFailureResult::Linked(evidence)) => {
            *evidence
        }
        Ok(crate::cloud_provider_stripe_renewals::StripeRenewalFailureResult::NeedsEvidence(_)) => {
            return Ok(None);
        }
        Err(error) => {
            eprintln!(
                "warning: signed Stripe renewal failure did not produce coverage evidence: {error}"
            );
            return Ok(None);
        }
    };
    let context = coverage_config
        .provider_context()
        .map_err(|_| Error::Internal("Stripe provider context is invalid".into()))?;
    Ok(Some(PreparedPersonalRenewalFailure {
        context,
        allocation,
        evidence,
        previous_provider_item_id: item_changed.then_some(previous_provider_item_id),
        provider_item_rebind_allowed,
    }))
}

async fn accept_prepared_personal_renewal_failure(
    tx: &mut Transaction<'_, Postgres>,
    failure: &PreparedPersonalRenewalFailure,
) -> Result<()> {
    if failure.provider_item_rebind_allowed {
        let previous_item_id = failure
            .previous_provider_item_id
            .as_deref()
            .ok_or_else(|| {
                Error::Internal(
                    "personal renewal item transition lacks its previous binding".into(),
                )
            })?;
        crate::cloud_provider::rebind_personal_allocation_item(
            tx,
            &failure.context,
            &failure.allocation,
            previous_item_id,
        )
        .await
        .map_err(|_| {
            Error::Internal("personal renewal item binding could not be advanced".into())
        })?;
    }
    crate::cloud_provider_stripe_renewal_store::accept_personal_renewal_failure(
        tx,
        &failure.context,
        &failure.allocation,
        &failure.evidence,
        true,
    )
    .await
    .map_err(|_| Error::Internal("personal renewal failure could not be recorded".into()))?;
    Ok(())
}

/// Read and link one grouped sponsor failure before opening the webhook transaction. Stripe reads
/// are bounded and all local manifests are loaded independently, so no database lock is held
/// across the network boundary. An incomplete historical chain is retained as a fixed rejection
/// outcome rather than a partial set of beneficiary evidence.
async fn prepare_sponsored_renewal_failure(
    pool: &PgPool,
    billing: &BillingState,
    raw_event: &str,
    event: &Event,
) -> Result<SponsoredRenewalFailurePreparation> {
    let Some((personal_config, stripe)) = billing.personal_coverage() else {
        return Ok(SponsoredRenewalFailurePreparation::NotApplicable);
    };
    let Some(catalogue) = billing.price_catalogue() else {
        return Ok(SponsoredRenewalFailurePreparation::NotApplicable);
    };
    let invoice = &event.data.object;
    let (Some(invoice_id), Some(subscription_id), Some(customer_id)) = (
        invoice.get("id").and_then(stripe_reference),
        invoice_subscription_id_claim(invoice),
        invoice.get("customer").and_then(stripe_reference),
    ) else {
        return Ok(SponsoredRenewalFailurePreparation::NotApplicable);
    };
    let subscription = sqlx::query(
        "SELECT organization_id, provider_customer_id FROM billing_sponsored_subscriptions \
         WHERE provider_subscription_id = $1",
    )
    .bind(&subscription_id)
    .fetch_optional(pool)
    .await?;
    let Some(subscription) = subscription else {
        return Ok(SponsoredRenewalFailurePreparation::NotApplicable);
    };
    let organization_id: String = subscription.try_get("organization_id")?;
    if subscription
        .try_get::<Option<String>, _>("provider_customer_id")?
        .as_deref()
        != Some(customer_id.as_str())
    {
        let context = personal_config
            .provider_context()
            .map_err(|_| Error::Internal("Stripe provider context is invalid".into()))?;
        let identity = sponsored_verified_event(event, raw_event, &subscription_id, &invoice_id)?;
        return Ok(SponsoredRenewalFailurePreparation::Rejected {
            context,
            event: Box::new(identity),
            organization_id,
            subscription_id,
            invoice_id,
            period_start: invoice_period_start(invoice).unwrap_or(event.created),
            reason: "sponsored_renewal_binding_mismatch",
        });
    }
    let context = personal_config
        .provider_context()
        .map_err(|_| Error::Internal("Stripe provider context is invalid".into()))?;
    let sponsored_config = SponsoredStripeCoverageConfig::new(
        personal_config.account_id.clone(),
        personal_config.environment,
        catalogue.standard_monthly.clone(),
        catalogue.standard_annual.clone(),
        catalogue.founding_monthly.clone(),
        catalogue.founding_annual.clone(),
    )
    .map_err(|_| Error::Internal("sponsored Stripe coverage prices are invalid".into()))?;

    let mut session = stripe.session();
    let failure_result = stripe
        .sponsored_failure_invoice(&mut session, &invoice_id, &customer_id, &subscription_id)
        .await;
    let (failure_invoice, failed_lines) = match failure_result {
        Ok(Some(result)) => result,
        Ok(None) => return Ok(SponsoredRenewalFailurePreparation::NotApplicable),
        Err(error) if error.is_evidence_rejection() => {
            return Ok(SponsoredRenewalFailurePreparation::Rejected {
                context,
                event: Box::new(sponsored_verified_event(
                    event,
                    raw_event,
                    &subscription_id,
                    &invoice_id,
                )?),
                organization_id,
                subscription_id,
                invoice_id,
                period_start: invoice_period_start(invoice).unwrap_or(event.created),
                reason: "sponsored_renewal_invoice_unusable",
            });
        }
        Err(_) => {
            return Err(Error::Internal(
                "sponsored renewal invoice read failed".into(),
            ));
        }
    };
    if failure_invoice.id != invoice_id || failed_lines.is_empty() {
        return Err(Error::Internal(
            "sponsored renewal invoice identity changed".into(),
        ));
    }
    let invoices = stripe
        .subscription_invoices_bounded(&mut session, &subscription_id, Some(&customer_id), 64)
        .await
        .map_err(|_| Error::Internal("sponsored renewal history read failed".into()))?;
    let mut predecessors = Vec::new();
    for paid_invoice in invoices
        .iter()
        .filter(|invoice| invoice.status.as_deref() == Some("paid"))
    {
        let predecessor_lines = if paid_invoice.lines_complete {
            paid_invoice.line_summaries.clone()
        } else {
            match stripe.invoice_lines(&mut session, &paid_invoice.id).await {
                Ok(lines) => lines,
                Err(error) if error.is_evidence_rejection() => {
                    return Ok(SponsoredRenewalFailurePreparation::Rejected {
                        context,
                        event: Box::new(sponsored_verified_event(
                            event,
                            raw_event,
                            &subscription_id,
                            &invoice_id,
                        )?),
                        organization_id,
                        subscription_id: subscription_id.clone(),
                        invoice_id,
                        period_start: invoice_period_start(invoice).unwrap_or(event.created),
                        reason: "sponsored_renewal_history_unusable",
                    });
                }
                Err(_) => {
                    return Err(Error::Internal(
                        "sponsored renewal history read failed".into(),
                    ));
                }
            }
        };
        let could_be_predecessor =
            sponsored_invoice_lines_can_be_predecessors(&predecessor_lines, &failed_lines);
        if !could_be_predecessor {
            continue;
        }
        let settlement = match stripe
            .sponsored_invoice_settlement(
                &mut session,
                &paid_invoice.id,
                &customer_id,
                &subscription_id,
                &sponsored_config,
            )
            .await
        {
            Ok(settlement) => settlement,
            Err(error) if error.is_evidence_rejection() => {
                return Ok(SponsoredRenewalFailurePreparation::Rejected {
                    context,
                    event: Box::new(sponsored_verified_event(
                        event,
                        raw_event,
                        &subscription_id,
                        &invoice_id,
                    )?),
                    organization_id,
                    subscription_id: subscription_id.clone(),
                    invoice_id,
                    period_start: invoice_period_start(invoice).unwrap_or(event.created),
                    reason: "sponsored_renewal_history_unusable",
                });
            }
            Err(_) => {
                return Err(Error::Internal(
                    "sponsored renewal settlement read failed".into(),
                ));
            }
        };
        let manifest =
            match crate::cloud_provider_stripe_sponsored_store::load_sponsored_allocation_manifest(
                pool,
                &context,
                &customer_id,
                &subscription_id,
                settlement.period_start(),
                settlement.period_end(),
                crate::cloud_provider_stripe_sponsored_store::SponsoredManifestLoadLimits::default(
                ),
            )
            .await
            {
                Ok(manifest) => manifest,
                Err(crate::cloud_provider_stripe_sponsored_store::SponsoredManifestLoadError::Database(_)) => {
                    return Err(Error::Internal(
                        "sponsored renewal manifest read failed".into(),
                    ));
                }
                Err(_) => {
                    return Ok(SponsoredRenewalFailurePreparation::Rejected {
                        context,
                        event: Box::new(sponsored_verified_event(
                            event,
                            raw_event,
                            &subscription_id,
                            &invoice_id,
                        )?),
                        organization_id,
                        subscription_id: subscription_id.clone(),
                        invoice_id,
                        period_start: invoice_period_start(invoice).unwrap_or(event.created),
                        reason: "sponsored_renewal_manifest_unusable",
                    });
                }
            };
        match compose_sponsored_coverage(&sponsored_config, &manifest, &settlement) {
            Ok(SponsoredCoverageResult::Candidate(_)) => {
                predecessors.push(SponsoredFailurePredecessor {
                    settlement,
                    manifest,
                })
            }
            _ => {
                return Ok(SponsoredRenewalFailurePreparation::Rejected {
                    context,
                    event: Box::new(sponsored_verified_event(
                        event,
                        raw_event,
                        &subscription_id,
                        &invoice_id,
                    )?),
                    organization_id,
                    subscription_id: subscription_id.clone(),
                    invoice_id,
                    period_start: invoice_period_start(invoice).unwrap_or(event.created),
                    reason: "sponsored_renewal_predecessor_unusable",
                });
            }
        }
    }

    let mut affected = Vec::new();
    let mut used = std::collections::BTreeSet::new();
    let mut rejected_reason = None;
    let mut notice_period_start = i64::MAX;
    for failed_line in &failed_lines {
        let (Some(item_id), Some(price_id), Some(start), Some(end), Some(line_id)) = (
            failed_line.subscription_item_id.as_deref(),
            failed_line.price_id.as_deref(),
            failed_line.period_start,
            failed_line.period_end,
            Some(failed_line.id.as_str()),
        ) else {
            rejected_reason = Some("sponsored_renewal_line_incomplete");
            break;
        };
        notice_period_start = notice_period_start.min(start);
        let Some(interval) = sponsored_config.stripe_interval_for_price(price_id) else {
            rejected_reason = Some("sponsored_renewal_price_unsupported");
            break;
        };
        let matches = predecessors
            .iter()
            .filter(|predecessor| {
                predecessor.settlement.lines().iter().any(|line| {
                    line.provider_item_id() == item_id
                        && line.price_id() == price_id
                        && line.period_end() == start
                })
            })
            .collect::<Vec<_>>();
        if matches.len() != 1 {
            rejected_reason = Some("sponsored_renewal_predecessor_missing_or_ambiguous");
            break;
        }
        let predecessor = matches[0];
        let predecessor_line = predecessor
            .settlement
            .lines()
            .iter()
            .find(|line| {
                line.provider_item_id() == item_id
                    && line.price_id() == price_id
                    && line.period_end() == start
            })
            .expect("matched predecessor line exists");
        let failure_manifest =
            match crate::cloud_provider_stripe_sponsored_store::load_sponsored_allocation_manifest(
                pool,
                &context,
                &customer_id,
                &subscription_id,
                start,
                end,
                crate::cloud_provider_stripe_sponsored_store::SponsoredManifestLoadLimits::default(
                ),
            )
            .await
            {
                Ok(manifest) => manifest,
                Err(crate::cloud_provider_stripe_sponsored_store::SponsoredManifestLoadError::Database(_)) => {
                    return Err(Error::Internal(
                        "sponsored renewal manifest read failed".into(),
                    ));
                }
                Err(_) => {
                    rejected_reason = Some("sponsored_renewal_manifest_unusable");
                    break;
                }
            };
        let continuing = failure_manifest
            .allocations()
            .iter()
            .filter(|allocation| {
                allocation.provider_item_id() == item_id
                    && allocation.price_id() == price_id
                    && allocation.effective_from() <= start
                    && allocation
                        .effective_until()
                        .is_none_or(|until| until >= end)
            })
            .collect::<Vec<_>>();
        if failed_line.quantity.is_none_or(|quantity| quantity <= 0)
            || continuing.len() as i64 != failed_line.quantity.unwrap_or_default()
        {
            rejected_reason = Some("sponsored_renewal_quantity_mismatch");
            break;
        }
        let predecessor_reference = sponsored_predecessor_evidence_reference(
            &predecessor.manifest,
            &predecessor.settlement,
        );
        for current in continuing {
            let old = predecessor
                .manifest
                .allocations()
                .iter()
                .find(|allocation| {
                    allocation.allocation_reference() == current.allocation_reference()
                        && allocation.source_id() == current.source_id()
                        && allocation.beneficiary_id() == current.beneficiary_id()
                        && allocation.provider_item_id() == item_id
                        && allocation.price_id() == price_id
                        && allocation.effective_from() <= predecessor.settlement.period_start()
                        && allocation
                            .effective_until()
                            .is_none_or(|until| until >= predecessor.settlement.period_end())
                });
            let Some(_) = old else { continue };
            let allocation = match load_sponsored_failure_allocation(
                pool,
                &context,
                &customer_id,
                &subscription_id,
                current.allocation_reference(),
                current.source_id(),
                current.beneficiary_id(),
                item_id,
            )
            .await
            {
                Ok(Some(allocation)) => allocation,
                Ok(None) => {
                    rejected_reason = Some("sponsored_renewal_allocation_unusable");
                    break;
                }
                Err(error) => return Err(error),
            };
            if !used.insert((allocation.allocation_id.clone(), line_id.to_owned())) {
                rejected_reason = Some("sponsored_renewal_line_ambiguous");
                break;
            }
            let evidence = crate::cloud_provider_stripe_renewals::StripeRenewalFailureEvidence::from_sponsored_parts(
                &event.id,
                &invoice_id,
                line_id,
                predecessor.settlement.invoice_id(),
                &predecessor_reference,
                &context.account_id,
                context.environment,
                current.allocation_reference(),
                &customer_id,
                &subscription_id,
                item_id,
                price_id,
                predecessor_line.period_start(),
                predecessor_line.period_end(),
                start,
                end,
                event.created,
                interval,
            )
            .map_err(|_| Error::Internal("sponsored renewal evidence is invalid".into()))?;
            affected.push((allocation, evidence));
            if affected.len() > 128 {
                rejected_reason = Some("sponsored_renewal_scope_exceeded");
                break;
            }
        }
        if rejected_reason.is_some() {
            break;
        }
    }
    let period_start = if notice_period_start == i64::MAX {
        invoice_period_start(invoice).unwrap_or(event.created)
    } else {
        notice_period_start
    };
    if let Some(reason) = rejected_reason.or_else(|| {
        affected
            .is_empty()
            .then_some("sponsored_renewal_no_continuing_allocations")
    }) {
        return Ok(SponsoredRenewalFailurePreparation::Rejected {
            context,
            event: Box::new(sponsored_verified_event(
                event,
                raw_event,
                &subscription_id,
                &invoice_id,
            )?),
            organization_id,
            subscription_id,
            invoice_id,
            period_start,
            reason,
        });
    }
    let verified_event = affected[0]
        .1
        .verified_event()
        .map_err(|_| Error::Internal("sponsored renewal event identity is invalid".into()))?;
    Ok(SponsoredRenewalFailurePreparation::Prepared(Box::new(
        PreparedSponsoredRenewalFailure {
            context,
            event: verified_event,
            organization_id,
            subscription_id,
            invoice_id,
            affected,
        },
    )))
}

#[allow(clippy::too_many_arguments)]
async fn load_sponsored_failure_allocation(
    pool: &PgPool,
    context: &ProviderContext,
    customer_id: &str,
    subscription_id: &str,
    allocation_reference: &str,
    source_id: &str,
    beneficiary_id: &str,
    item_id: &str,
) -> Result<Option<VerifiedAllocation>> {
    let row = sqlx::query(
        "SELECT allocation.allocation_id, allocation.payer_id, allocation.beneficiary_id, \
                allocation.provider_subscription_id, allocation.provider_item_id, \
                allocation.external_allocation_reference, allocation.coverage_source_id, \
                allocation.effective_from, allocation.effective_until, allocation.state, \
                allocation.ownership_evidence_reference, allocation.provider_account_id, \
                allocation.provider_environment, payer.provider_customer_id, payer.payer_kind \
         FROM cloud_provider_allocations allocation \
         JOIN cloud_provider_payers payer ON payer.payer_id = allocation.payer_id \
         WHERE allocation.provider_namespace = $1 AND allocation.provider_account_id = $2 \
           AND allocation.provider_environment = $3 \
           AND allocation.provider_subscription_id = $4 AND allocation.provider_item_id = $5 \
           AND allocation.external_allocation_reference = $6 \
           AND allocation.coverage_source_id = $7 AND allocation.beneficiary_id = $8",
    )
    .bind(&context.namespace)
    .bind(&context.account_id)
    .bind(context.environment.as_str())
    .bind(subscription_id)
    .bind(item_id)
    .bind(allocation_reference)
    .bind(source_id)
    .bind(beneficiary_id)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else { return Ok(None) };
    if row.try_get::<String, _>("payer_kind")? != "sponsor"
        || row.try_get::<String, _>("provider_customer_id")? != customer_id
        || row.try_get::<String, _>("provider_account_id")? != context.account_id
        || row.try_get::<String, _>("provider_environment")? != context.environment.as_str()
    {
        return Ok(None);
    }
    let allocation = VerifiedAllocation::new(
        row.try_get::<String, _>("allocation_id")?,
        row.try_get::<String, _>("payer_id")?,
        customer_id.to_owned(),
        ProviderPayerKind::Sponsor,
        beneficiary_id.to_owned(),
        subscription_id.to_owned(),
        item_id.to_owned(),
        allocation_reference.to_owned(),
        source_id.to_owned(),
        row.try_get::<i64, _>("effective_from")?,
        row.try_get::<Option<i64>, _>("effective_until")?,
        parse_allocation_state(&row.try_get::<String, _>("state")?)?,
        row.try_get::<String, _>("ownership_evidence_reference")?,
    )
    .map_err(|_| Error::Internal("stored sponsored allocation is invalid".into()))?;
    Ok(Some(allocation))
}

async fn apply_sponsored_renewal_failure(
    tx: &mut Transaction<'_, Postgres>,
    preparation: &SponsoredRenewalFailurePreparation,
    provenance: crate::cloud_provider_stripe_renewal_store::StripeRenewalFailureProvenance,
) -> Result<()> {
    // Callers hold the subscription advisory lock before reaching this point. A paid invoice may
    // have committed after Stripe preparation observed the failed invoice as open; its durable
    // processed receipt is authoritative and prevents late failure evidence from reviving it.
    let paid_invoice = match preparation {
        SponsoredRenewalFailurePreparation::Prepared(prepared) => {
            Some((&prepared.subscription_id, &prepared.invoice_id))
        }
        SponsoredRenewalFailurePreparation::Rejected {
            subscription_id,
            invoice_id,
            ..
        } => Some((subscription_id, invoice_id)),
        SponsoredRenewalFailurePreparation::NotApplicable => None,
    };
    if let Some((subscription_id, invoice_id)) = paid_invoice {
        if sponsored_renewal_paid_receipt_exists(tx, subscription_id, invoice_id).await? {
            return Ok(());
        }
    }
    match preparation {
        SponsoredRenewalFailurePreparation::NotApplicable => {}
        SponsoredRenewalFailurePreparation::Prepared(prepared) => {
            sqlx::query(
                "UPDATE billing_sponsored_subscriptions SET status = 'past_due', updated_at = now() \
                 WHERE organization_id = $1 AND provider_subscription_id = $2",
            )
            .bind(&prepared.organization_id)
            .bind(&prepared.subscription_id)
            .execute(&mut **tx)
            .await?;
            for (allocation, evidence) in &prepared.affected {
                crate::cloud_provider_stripe_renewal_store::accept_sponsored_renewal_failure(
                    tx,
                    &prepared.context,
                    allocation,
                    evidence,
                    &prepared.event,
                    provenance,
                    true,
                )
                .await
                .map_err(|_| {
                    Error::Internal("sponsored renewal evidence could not be recorded".into())
                })?;
                enqueue_sponsored_renewal_failure_notices(
                    tx,
                    &prepared.organization_id,
                    &prepared.invoice_id,
                    &allocation.beneficiary_id,
                    evidence.predecessor_period_end(),
                    false,
                )
                .await?;
            }
            let period_start = prepared
                .affected
                .first()
                .map(|(_, evidence)| evidence.predecessor_period_end())
                .ok_or_else(|| Error::Internal("sponsored renewal has no affected seats".into()))?;
            enqueue_sponsored_renewal_failure_notices(
                tx,
                &prepared.organization_id,
                &prepared.invoice_id,
                "",
                period_start,
                true,
            )
            .await?;
        }
        SponsoredRenewalFailurePreparation::Rejected {
            context,
            event,
            organization_id,
            subscription_id,
            invoice_id,
            period_start,
            reason,
        } => {
            reject_verified_event(tx, context, event, reason)
                .await
                .map_err(|_| {
                    Error::Internal("sponsored renewal rejection could not be recorded".into())
                })?;
            sqlx::query(
                "UPDATE billing_sponsored_subscriptions SET status = 'past_due', updated_at = now() \
                 WHERE organization_id = $1 AND provider_subscription_id = $2",
            )
            .bind(organization_id)
            .bind(subscription_id)
            .execute(&mut **tx)
            .await?;
            enqueue_sponsored_renewal_failure_notices(
                tx,
                organization_id,
                invoice_id,
                "",
                *period_start,
                true,
            )
            .await?;
        }
    }
    Ok(())
}

async fn sponsored_renewal_paid_receipt_exists(
    tx: &mut Transaction<'_, Postgres>,
    subscription_id: &str,
    invoice_id: &str,
) -> Result<bool> {
    if subscription_id.is_empty() || invoice_id.trim().is_empty() {
        return Ok(false);
    }
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM stripe_webhook_events \
         WHERE event_type = 'invoice.paid' AND subscription_id = $1 \
           AND provider_object_id = $2 AND processed_at IS NOT NULL)",
    )
    .bind(subscription_id)
    .bind(invoice_id)
    .fetch_one(&mut **tx)
    .await?)
}

struct PreparedSponsoredInvoiceSeat {
    seat_id: String,
    operation_id: String,
    offer: BillingOffer,
    registered_offer: BillingOffer,
    provider_item_id: String,
    price_id: String,
    period_start: i64,
    period_end: i64,
    desired_effective_until: Option<i64>,
    ending_evidence_reference: String,
    allocation: VerifiedAllocation,
}

struct PreparedSponsoredInvoiceCoverage {
    context: ProviderContext,
    event: crate::cloud_provider::VerifiedProviderEvent,
    organization_id: String,
    invoice_id: String,
    payment_reference: String,
    seats: Vec<PreparedSponsoredInvoiceSeat>,
}

enum SponsoredInvoiceCoveragePreparation {
    NotApplicable,
    Prepared(Box<PreparedSponsoredInvoiceCoverage>),
    Rejected {
        context: ProviderContext,
        event: Box<crate::cloud_provider::VerifiedProviderEvent>,
        reason: &'static str,
    },
}

async fn prepare_sponsored_invoice_coverage(
    pool: &sqlx::PgPool,
    billing: &BillingState,
    raw_event: &str,
    event: &Event,
) -> Result<SponsoredInvoiceCoveragePreparation> {
    let Some((personal_coverage, stripe)) = billing.personal_coverage() else {
        return Err(Error::Internal(
            "sponsored Stripe coverage is not configured".into(),
        ));
    };
    let Some(catalogue) = billing.price_catalogue() else {
        return Err(Error::NotConfigured(
            "sponsored Stripe coverage catalogue is not configured".into(),
        ));
    };
    let invoice = &event.data.object;
    let Some(invoice_id) = invoice.get("id").and_then(stripe_reference) else {
        return Ok(SponsoredInvoiceCoveragePreparation::NotApplicable);
    };
    let Some(subscription_id) = invoice_subscription_id_claim(invoice) else {
        return Ok(SponsoredInvoiceCoveragePreparation::NotApplicable);
    };
    let Some(customer_id) = invoice.get("customer").and_then(stripe_reference) else {
        return Ok(SponsoredInvoiceCoveragePreparation::NotApplicable);
    };
    let context = personal_coverage
        .provider_context()
        .map_err(|_| Error::Internal("Stripe coverage context is invalid".into()))?;

    let subscription = sqlx::query(
        "SELECT organization_id, provider_customer_id, provider_subscription_id \
         FROM billing_sponsored_subscriptions WHERE provider_subscription_id = $1",
    )
    .bind(&subscription_id)
    .fetch_optional(pool)
    .await?;
    let Some(subscription) = subscription else {
        // This metadata is only a hint that the server should retry until the local Checkout
        // binding arrives. It never chooses the owner or grants coverage.
        if let (Some(organization_id), Some(operation_id)) = (
            invoice_metadata_claim(invoice, "organization_id"),
            invoice_metadata_claim(invoice, "sponsored_operation_id"),
        ) {
            let pending_binding: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM billing_sponsored_operations \
                 WHERE operation_id = $1 AND organization_id = $2 \
                   AND state IN ('checkout_created','provider_pending','unknown'))",
            )
            .bind(operation_id)
            .bind(organization_id)
            .fetch_one(pool)
            .await?;
            if pending_binding {
                return Err(Error::Internal(
                    "sponsored invoice arrived before its local subscription binding".into(),
                ));
            }
        }
        return Ok(SponsoredInvoiceCoveragePreparation::NotApplicable);
    };
    let organization_id: String = subscription.try_get("organization_id")?;
    let stored_customer: Option<String> = subscription.try_get("provider_customer_id")?;
    if stored_customer.is_none() {
        return Err(Error::Internal(
            "sponsored invoice arrived before its local customer binding".into(),
        ));
    }
    if stored_customer.as_deref() != Some(customer_id.as_str())
        || subscription.try_get::<String, _>("provider_subscription_id")? != subscription_id
        || invoice_metadata_claim(invoice, "organization_id")
            .is_some_and(|claim| claim != organization_id)
    {
        let event = sponsored_verified_event(event, raw_event, &subscription_id, &invoice_id)?;
        return Ok(SponsoredInvoiceCoveragePreparation::Rejected {
            context,
            event: Box::new(event),
            reason: "sponsored_invoice_binding_mismatch",
        });
    }

    let sponsored = SponsoredStripeCoverageConfig::new(
        personal_coverage.account_id.clone(),
        personal_coverage.environment,
        catalogue.standard_monthly.clone(),
        catalogue.standard_annual.clone(),
        catalogue.founding_monthly.clone(),
        catalogue.founding_annual.clone(),
    )
    .map_err(|_| Error::Internal("sponsored Stripe coverage prices are invalid".into()))?;
    let mut session = stripe.session();
    let settlement = match stripe
        .sponsored_invoice_settlement(
            &mut session,
            &invoice_id,
            &customer_id,
            &subscription_id,
            &sponsored,
        )
        .await
    {
        Ok(settlement) => settlement,
        Err(error) if error.is_evidence_rejection() => {
            let event = sponsored_verified_event(event, raw_event, &subscription_id, &invoice_id)?;
            return Ok(SponsoredInvoiceCoveragePreparation::Rejected {
                context,
                event: Box::new(event),
                reason: "sponsored_invoice_evidence_rejected",
            });
        }
        Err(_) => {
            return Err(Error::Internal(
                "sponsored Stripe invoice read failed".into(),
            ));
        }
    };
    let payment_reference = settlement.payment_intent_id().to_owned();
    let event = sponsored_verified_event(event, raw_event, &subscription_id, &invoice_id)?;
    let seats = sqlx::query(
        "SELECT seat.seat_id, seat.beneficiary_id, seat.offer, award.offer AS founding_offer, \
                award.original_end_epoch, seat.effective_from, \
                seat.effective_until, seat.state, seat.operation_id, seat.provider_item_id, \
                seat.allocation_id, operation.state AS operation_state, operation.offer AS operation_offer, \
                operation.provider_item_id AS operation_provider_item_id \
         FROM billing_sponsored_seats AS seat \
         JOIN billing_sponsored_operations AS operation ON operation.operation_id = seat.operation_id \
         LEFT JOIN billing_founding_awards AS award ON award.beneficiary_id = seat.beneficiary_id \
           AND award.payer_kind = 'sponsor' AND award.payer_id = seat.organization_id \
         WHERE seat.organization_id = $1 \
           AND operation.state IN ('active','provider_pending') \
           AND operation.provider_subscription_id = $4 \
           AND seat.state IN ('pending','active','scheduled_removal','replaced','canceled') \
           AND seat.effective_from < $2 \
           AND (seat.effective_until IS NULL OR seat.effective_until > $3) \
         ORDER BY seat.seat_id COLLATE \"C\" LIMIT 129",
    )
    .bind(&organization_id)
    .bind(settlement.period_end())
    .bind(settlement.period_start())
    .bind(&subscription_id)
    .fetch_all(pool)
    .await?;
    if seats.len() > 128 {
        let event = event.clone();
        return Ok(SponsoredInvoiceCoveragePreparation::Rejected {
            context,
            event: Box::new(event),
            reason: "sponsored_invoice_scope_exceeded",
        });
    }

    let mut prepared_seats = Vec::new();
    let mut intervals = Vec::new();
    let mut seen_seat_ids = std::collections::BTreeSet::new();
    for line in settlement.lines() {
        let Some(offer) = BillingOffer::ALL
            .into_iter()
            .find(|offer| catalogue.id_for(*offer) == line.price_id())
        else {
            let event = event.clone();
            return Ok(SponsoredInvoiceCoveragePreparation::Rejected {
                context,
                event: Box::new(event),
                reason: "sponsored_invoice_price_unsupported",
            });
        };
        for seat in &seats {
            let seat_offer: String = seat.try_get("offer")?;
            let stored_offer = billing_offer(&seat_offer)?;
            let founding_offer: Option<String> = seat.try_get("founding_offer")?;
            let founding_offer = founding_offer
                .as_deref()
                .map(parse_founding_offer)
                .transpose()?;
            let founding_end_epoch: Option<i64> = seat.try_get("original_end_epoch")?;
            let seat_state: String = seat.try_get("state")?;
            let Some(expected_offer) = expected_sponsored_invoice_offer(
                stored_offer,
                founding_offer,
                founding_end_epoch,
                &seat_state,
                line.period_start(),
            ) else {
                let event = event.clone();
                return Ok(SponsoredInvoiceCoveragePreparation::Rejected {
                    context,
                    event: Box::new(event),
                    reason: "founding_invoice_boundary_missing",
                });
            };
            if expected_offer != offer {
                continue;
            }
            let seat_from: i64 = seat.try_get("effective_from")?;
            let seat_until: Option<i64> = seat.try_get("effective_until")?;
            if seat_from >= line.period_end()
                || seat_until.is_some_and(|until| until <= line.period_start())
            {
                continue;
            }
            let seat_item: Option<String> = seat.try_get("provider_item_id")?;
            let operation_item: Option<String> = seat.try_get("operation_provider_item_id")?;
            if seat_item
                .as_deref()
                .zip(operation_item.as_deref())
                .is_some_and(|(seat_item, operation_item)| seat_item != operation_item)
            {
                let event = event.clone();
                return Ok(SponsoredInvoiceCoveragePreparation::Rejected {
                    context,
                    event: Box::new(event),
                    reason: "sponsored_invoice_item_binding_mismatch",
                });
            }
            match seat_item.as_deref().or(operation_item.as_deref()) {
                Some(item_id) if item_id != line.provider_item_id() => continue,
                // A newly scheduled seat is not bound to an item until this invoice is
                // accepted. The query above already scopes it to this subscription, and the
                // composer verifies the complete named-seat count against the line quantity.
                None => {}
                Some(_) => {}
            }
            let seat_id: String = seat.try_get("seat_id")?;
            if !seen_seat_ids.insert(seat_id.clone()) {
                let event = event.clone();
                return Ok(SponsoredInvoiceCoveragePreparation::Rejected {
                    context,
                    event: Box::new(event),
                    reason: "sponsored_invoice_seat_ambiguous",
                });
            }
            let beneficiary_id: String = seat.try_get("beneficiary_id")?;
            let operation_id: String = seat.try_get("operation_id")?;
            let allocation_id = opaque_provider_id("stripe-sponsored-allocation", &[&seat_id]);
            let stored_allocation_id: Option<String> = seat.try_get("allocation_id")?;
            if stored_allocation_id
                .as_deref()
                .is_some_and(|stored| stored != allocation_id)
            {
                let event = event.clone();
                return Ok(SponsoredInvoiceCoveragePreparation::Rejected {
                    context,
                    event: Box::new(event),
                    reason: "sponsored_invoice_allocation_mismatch",
                });
            }
            let source_id = opaque_provider_id("stripe-sponsored-source", &[&seat_id]);
            let external_reference = format!(
                "sponsored-seat:{}",
                opaque_provider_id("stripe-sponsored-reference", &[&seat_id])
            );
            let payer_id = opaque_provider_id(
                "stripe-sponsored-payer",
                &[
                    &context.account_id,
                    context.environment.as_str(),
                    &customer_id,
                ],
            );
            let ownership_reference = format!(
                "stripe:sponsored-seat:{}",
                opaque_provider_id("stripe-sponsored-ownership", &[&organization_id, &seat_id])
            );
            let allocation = VerifiedAllocation::new(
                allocation_id,
                payer_id,
                customer_id.clone(),
                ProviderPayerKind::Sponsor,
                beneficiary_id.clone(),
                subscription_id.clone(),
                line.provider_item_id().to_owned(),
                external_reference.clone(),
                source_id.clone(),
                seat_from,
                None,
                AllocationState::Active,
                ownership_reference,
            )
            .map_err(|_| Error::Internal("sponsored provider allocation is invalid".into()))?;
            let desired_effective_until = seat_until;
            let ending_evidence_reference = format!(
                "stripe:sponsored-seat-end:{}",
                opaque_provider_id(
                    "stripe-sponsored-seat-end",
                    &[
                        &seat_id,
                        &operation_id,
                        &seat_until.unwrap_or_default().to_string()
                    ],
                )
            );
            intervals.push(
                SponsoredAllocationInterval::new(
                    source_id,
                    external_reference,
                    beneficiary_id,
                    line.provider_item_id().to_owned(),
                    line.price_id().to_owned(),
                    seat_from,
                    desired_effective_until,
                )
                .map_err(|_| Error::Internal("sponsored allocation interval is invalid".into()))?,
            );
            prepared_seats.push(PreparedSponsoredInvoiceSeat {
                seat_id,
                operation_id,
                offer,
                registered_offer: stored_offer,
                provider_item_id: line.provider_item_id().to_owned(),
                price_id: line.price_id().to_owned(),
                period_start: line.period_start(),
                period_end: line.period_end(),
                desired_effective_until,
                ending_evidence_reference,
                allocation,
            });
        }
    }
    let manifest = match SponsoredAllocationManifest::new(
        context.account_id.clone(),
        context.environment,
        customer_id,
        subscription_id,
        intervals,
    ) {
        Ok(manifest) => manifest,
        Err(_) => {
            return Ok(SponsoredInvoiceCoveragePreparation::Rejected {
                context,
                event: Box::new(event),
                reason: "sponsored_invoice_manifest_rejected",
            });
        }
    };
    match compose_sponsored_coverage(&sponsored, &manifest, &settlement) {
        Ok(SponsoredCoverageResult::Candidate(_)) => {}
        Ok(SponsoredCoverageResult::NeedsEvidence(_)) => {
            return Ok(SponsoredInvoiceCoveragePreparation::Rejected {
                context,
                event: Box::new(event),
                reason: "sponsored_invoice_manifest_mismatch",
            });
        }
        Err(_) => {
            return Ok(SponsoredInvoiceCoveragePreparation::Rejected {
                context,
                event: Box::new(event),
                reason: "sponsored_invoice_manifest_mismatch",
            });
        }
    }
    if prepared_seats.is_empty() {
        return Ok(SponsoredInvoiceCoveragePreparation::Rejected {
            context,
            event: Box::new(event),
            reason: "sponsored_invoice_has_no_named_seats",
        });
    }
    Ok(SponsoredInvoiceCoveragePreparation::Prepared(Box::new(
        PreparedSponsoredInvoiceCoverage {
            context,
            event,
            organization_id,
            invoice_id,
            payment_reference,
            seats: prepared_seats,
        },
    )))
}

fn sponsored_verified_event(
    event: &Event,
    raw_event: &str,
    subscription_id: &str,
    invoice_id: &str,
) -> Result<crate::cloud_provider::VerifiedProviderEvent> {
    crate::cloud_provider::VerifiedProviderEvent::from_payload_with_object_id(
        event.id.clone(),
        event.kind.clone(),
        event.created,
        Some(subscription_id.to_owned()),
        None,
        invoice_id.to_owned(),
        raw_event.as_bytes(),
    )
    .map_err(|_| Error::Internal("sponsored provider event identity is invalid".into()))
}

async fn prepare_personal_invoice_coverage(
    pool: &sqlx::PgPool,
    billing: &BillingState,
    raw_event: &str,
    source: PersonalInvoiceEventSource<'_>,
) -> Result<PersonalInvoiceCoveragePreparation> {
    let (coverage_config, stripe) = billing
        .personal_coverage()
        .ok_or_else(|| Error::Internal("personal Stripe coverage is not configured".into()))?;
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw_event) else {
        return Ok(PersonalInvoiceCoveragePreparation::Rejected(
            "invoice_payload_malformed",
        ));
    };
    let Some(invoice) = value.get("data").and_then(|data| data.get("object")) else {
        return Ok(PersonalInvoiceCoveragePreparation::Rejected(
            "invoice_payload_malformed",
        ));
    };
    let subscription_claim = invoice_subscription_id_claim(invoice);
    let allocation_reference = invoice_allocation_reference_claim(invoice);
    let subscription_binding_required = if let Some(subscription_id) = subscription_claim.as_deref()
    {
        sqlx::query_scalar::<_, bool>(
            "SELECT stripe_coverage_binding_required FROM billing_personal_accounts \
             WHERE stripe_subscription_id = $1",
        )
        .bind(subscription_id)
        .fetch_optional(pool)
        .await?
    } else {
        None
    };
    let metadata_binding = if let (Some(user_id), Some(operation_id)) = (
        invoice_metadata_claim(invoice, "personal_user_id"),
        invoice_metadata_claim(invoice, "operation_id"),
    ) {
        sqlx::query_scalar::<_, bool>(
            "SELECT stripe_coverage_binding_required FROM billing_personal_accounts \
             WHERE user_id = $1 AND operation_id = $2",
        )
        .bind(user_id)
        .bind(operation_id)
        .fetch_optional(pool)
        .await?
    } else {
        None
    };
    let Some(allocation_reference) = allocation_reference else {
        if subscription_binding_required == Some(true) {
            return Ok(PersonalInvoiceCoveragePreparation::Rejected(
                "allocation_reference_missing",
            ));
        }
        if metadata_binding == Some(true) {
            return Ok(PersonalInvoiceCoveragePreparation::Rejected(
                "allocation_reference_missing",
            ));
        }
        return Ok(PersonalInvoiceCoveragePreparation::NotApplicable);
    };
    if !allocation_reference.starts_with("alloc_") {
        if subscription_binding_required == Some(true) || metadata_binding == Some(true) {
            return Ok(PersonalInvoiceCoveragePreparation::Rejected(
                "allocation_reference_invalid",
            ));
        }
        return Ok(PersonalInvoiceCoveragePreparation::NotApplicable);
    }

    let row = sqlx::query(
        "SELECT user_id, operation_id, allocation_reference, stripe_coverage_binding_required, \
                offer, stripe_customer_id, \
                paid_through_epoch, \
                (SELECT offer FROM billing_founding_awards \
                 WHERE beneficiary_id = billing_personal_accounts.user_id) AS founding_offer, \
                (SELECT original_end_epoch FROM billing_founding_awards \
                 WHERE beneficiary_id = billing_personal_accounts.user_id) AS founding_end_epoch, \
                stripe_subscription_id FROM billing_personal_accounts \
         WHERE allocation_reference = $1",
    )
    .bind(&allocation_reference)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        if subscription_binding_required == Some(true) {
            return Ok(PersonalInvoiceCoveragePreparation::Rejected(
                "allocation_reference_unrecognised",
            ));
        }
        if metadata_binding == Some(true) {
            return Ok(PersonalInvoiceCoveragePreparation::Rejected(
                "allocation_reference_unrecognised",
            ));
        }
        return Ok(PersonalInvoiceCoveragePreparation::NotApplicable);
    };
    if !row.try_get::<bool, _>("stripe_coverage_binding_required")? {
        return Ok(PersonalInvoiceCoveragePreparation::NotApplicable);
    }
    let user_id: String = row.try_get("user_id")?;
    let operation_id: String = row.try_get("operation_id")?;
    let stored_reference: String = row.try_get("allocation_reference")?;
    let offer: String = row.try_get("offer")?;
    let founding_offer: Option<String> = row.try_get("founding_offer")?;
    let founding_offer = founding_offer
        .as_deref()
        .map(parse_founding_offer)
        .transpose()?;
    let founding_end_epoch: Option<i64> = row.try_get("founding_end_epoch")?;
    let account_paid_through_epoch: Option<i64> = row.try_get("paid_through_epoch")?;
    let Some(customer_id) = row.try_get::<Option<String>, _>("stripe_customer_id")? else {
        return Err(Error::Internal(
            "personal invoice checkout is not yet linked".into(),
        ));
    };
    let invoice_subscription_id = subscription_claim
        .as_deref()
        .ok_or_else(|| Error::Config("personal invoice has no subscription binding".into()))?;
    let claimed_transfer_id = invoice_metadata_claim(invoice, "sotto_transfer_id");
    let stored_subscription_id: Option<String> = row.try_get("stripe_subscription_id")?;
    let transfer_link_id = if stored_subscription_id.as_deref() == Some(invoice_subscription_id) {
        None
    } else if stored_subscription_id.is_none() {
        let Some(transfer_id) = claimed_transfer_id else {
            return Err(Error::Internal(
                "personal invoice checkout is not yet linked".into(),
            ));
        };
        let transfer_matches: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM billing_transfer_intents \
             WHERE transfer_id = $1 AND beneficiary_id = $2 AND destination_kind = 'personal' \
               AND destination_operation_id = $3 AND destination_customer_id = $4 \
               AND destination_provider_subscription_id = $5 \
               AND state IN ('destination_prepared','destination_paid', \
                             'source_adjustment_pending','completed','unknown'))",
        )
        .bind(transfer_id)
        .bind(&user_id)
        .bind(&operation_id)
        .bind(&customer_id)
        .bind(invoice_subscription_id)
        .fetch_one(pool)
        .await?;
        if !transfer_matches || operation_id != format!("{transfer_id}:destination") {
            return Ok(PersonalInvoiceCoveragePreparation::Rejected(
                "invoice_binding_mismatch",
            ));
        }
        Some(transfer_id.to_string())
    } else {
        return Ok(PersonalInvoiceCoveragePreparation::Rejected(
            "invoice_binding_mismatch",
        ));
    };
    let subscription_id = invoice_subscription_id.to_string();
    if stored_reference != allocation_reference
        || invoice_metadata_claim(invoice, "personal_user_id").is_some_and(|claim| claim != user_id)
        || invoice_metadata_claim(invoice, "operation_id")
            .is_some_and(|claim| claim != operation_id)
    {
        return Ok(PersonalInvoiceCoveragePreparation::Rejected(
            "invoice_binding_mismatch",
        ));
    }
    let Some(invoice_id) = invoice.get("id").and_then(stripe_reference) else {
        return Ok(PersonalInvoiceCoveragePreparation::Rejected(
            "invoice_id_missing",
        ));
    };
    let Some(customer_claim) = invoice.get("customer").and_then(stripe_reference) else {
        return Ok(PersonalInvoiceCoveragePreparation::Rejected(
            "invoice_customer_missing",
        ));
    };
    let Some(subscription_claim) = invoice_subscription_id_claim(invoice) else {
        return Ok(PersonalInvoiceCoveragePreparation::Rejected(
            "invoice_subscription_missing",
        ));
    };
    if customer_claim != customer_id || subscription_claim != subscription_id {
        return Ok(PersonalInvoiceCoveragePreparation::Rejected(
            "invoice_binding_mismatch",
        ));
    }
    let Some(provider_item_id) = invoice
        .get("lines")
        .and_then(|lines| lines.get("data"))
        .and_then(serde_json::Value::as_array)
        .and_then(|lines| (lines.len() == 1).then(|| &lines[0]))
        .and_then(|line| line.get("parent"))
        .and_then(|parent| parent.get("subscription_item_details"))
        .and_then(|details| details.get("subscription_item"))
        .and_then(stripe_reference)
    else {
        return Ok(PersonalInvoiceCoveragePreparation::Rejected(
            "invoice_item_missing",
        ));
    };
    let binding = match StripeAllocationBinding::new(
        allocation_reference.clone(),
        customer_id.clone(),
        subscription_id.clone(),
        provider_item_id,
        ProviderPayerKind::Personal,
    ) {
        Ok(binding) => binding,
        Err(_) => {
            return Ok(PersonalInvoiceCoveragePreparation::Rejected(
                "invoice_binding_invalid",
            ));
        }
    };
    let mut session = stripe.session();
    let observation = match stripe
        .personal_invoice_observation(&mut session, &invoice_id, &binding)
        .await
    {
        Ok(observation) => observation,
        Err(error) if error.is_evidence_rejection() => {
            return Ok(PersonalInvoiceCoveragePreparation::Rejected(
                "stripe_invoice_evidence_rejected",
            ));
        }
        Err(_) => {
            return Err(Error::Internal(
                "personal Stripe invoice read failed".into(),
            ));
        }
    };
    let payments = match stripe.invoice_payments(&mut session, &invoice_id).await {
        Ok(payments) => payments,
        Err(error) if error.is_evidence_rejection() => {
            return Ok(PersonalInvoiceCoveragePreparation::Rejected(
                "stripe_payment_evidence_rejected",
            ));
        }
        Err(_) => {
            return Err(Error::Internal(
                "personal Stripe payment read failed".into(),
            ));
        }
    };
    if payments.len() != 1 {
        return Ok(PersonalInvoiceCoveragePreparation::Rejected(
            "payment_count_unsupported",
        ));
    }
    let settlement = match payments[0].settlement(coverage_config) {
        Ok(settlement) => settlement,
        Err(_) => {
            return Ok(PersonalInvoiceCoveragePreparation::Rejected(
                "payment_settlement_unsupported",
            ));
        }
    };
    let decoded = match source {
        PersonalInvoiceEventSource::Signed { signature, now } => {
            decode_paid_invoice_with_observation(
                raw_event.as_bytes(),
                signature,
                &billing.webhook_secret,
                now,
                coverage_config,
                &binding,
                &settlement,
                &observation,
            )
        }
        PersonalInvoiceEventSource::AuthenticatedRead(event) => {
            decode_paid_invoice_from_authenticated_read_with_observation(
                event,
                coverage_config,
                &binding,
                &settlement,
                &observation,
            )
        }
    };
    let evidence = match decoded {
        Ok(evidence) => evidence,
        Err(_) => {
            return Ok(PersonalInvoiceCoveragePreparation::Rejected(
                "invoice_evidence_unsupported",
            ));
        }
    };
    let Some(expected_invoice_offer) = expected_founding_invoice_offer(
        billing_offer(&offer)?,
        founding_offer,
        founding_end_epoch,
        evidence.period_start,
    ) else {
        return Ok(PersonalInvoiceCoveragePreparation::Rejected(
            "founding_boundary_missing",
        ));
    };
    if evidence.invoice_id != observation.invoice_id()
        || evidence.customer_id != observation.customer_id()
        || evidence.subscription_id != observation.subscription_id()
        || evidence.provider_item_id != observation.provider_item_id()
        || evidence.allocation_reference != observation.allocation_reference()
        || evidence.payment_intent_id != observation.payment_intent_id()
        || evidence.amount_paid != observation.amount_paid()
        || evidence.interval.to_string() != observation.interval().to_string()
        || evidence.period_start != observation.period_start()
        || evidence.period_end != observation.period_end()
        || evidence.payment_intent_id != settlement.payment_intent_id()
        || !billing_offer_matches_price(
            billing.price_catalogue(),
            expected_invoice_offer.as_str(),
            &evidence.price_id,
        )
    {
        return Ok(PersonalInvoiceCoveragePreparation::Rejected(
            "invoice_evidence_changed",
        ));
    }

    let context = match coverage_config.provider_context() {
        Ok(context) => context,
        Err(_) => {
            return Ok(PersonalInvoiceCoveragePreparation::Rejected(
                "stripe_context_invalid",
            ));
        }
    };
    let stored = sqlx::query(
        "SELECT allocation.allocation_id, allocation.payer_id, allocation.beneficiary_id, \
                payer.provider_customer_id, allocation.provider_subscription_id, \
                allocation.provider_item_id, allocation.coverage_source_id, allocation.effective_from, \
                allocation.effective_until, allocation.state, allocation.ownership_evidence_reference, \
                payer.payer_kind, allocation.provider_account_id, allocation.provider_environment \
         FROM cloud_provider_allocations AS allocation \
         JOIN cloud_provider_payers AS payer ON payer.payer_id = allocation.payer_id \
         WHERE allocation.provider_namespace = $1 AND allocation.provider_account_id = $2 \
           AND allocation.provider_environment = $3 AND allocation.external_allocation_reference = $4",
    )
    .bind(&context.namespace)
    .bind(&context.account_id)
    .bind(context.environment.as_str())
    .bind(&allocation_reference)
    .fetch_optional(pool)
    .await?;
    let allocation = match VerifiedAllocation::new(
        stored
            .as_ref()
            .map(|row| row.try_get::<String, _>("allocation_id"))
            .transpose()?
            .unwrap_or_else(|| {
                opaque_provider_id("stripe-personal-allocation", &[&allocation_reference])
            }),
        stored
            .as_ref()
            .map(|row| row.try_get::<String, _>("payer_id"))
            .transpose()?
            .unwrap_or_else(|| {
                opaque_provider_id(
                    "stripe-personal-payer",
                    &[
                        &context.account_id,
                        context.environment.as_str(),
                        &customer_id,
                    ],
                )
            }),
        customer_id.clone(),
        ProviderPayerKind::Personal,
        stored
            .as_ref()
            .map(|row| row.try_get::<String, _>("beneficiary_id"))
            .transpose()?
            .unwrap_or_else(|| user_id.clone()),
        subscription_id.clone(),
        evidence.provider_item_id.clone(),
        allocation_reference.clone(),
        stored
            .as_ref()
            .map(|row| row.try_get::<String, _>("coverage_source_id"))
            .transpose()?
            .unwrap_or_else(|| {
                opaque_provider_id("stripe-personal-source", &[&allocation_reference])
            }),
        stored
            .as_ref()
            .map(|row| row.try_get::<i64, _>("effective_from"))
            .transpose()?
            .unwrap_or(evidence.period_start),
        stored
            .as_ref()
            .map(|row| row.try_get::<Option<i64>, _>("effective_until"))
            .transpose()?
            .unwrap_or(None),
        stored
            .as_ref()
            .map(|row| row.try_get::<String, _>("state"))
            .transpose()?
            .as_deref()
            .map(parse_allocation_state)
            .transpose()?
            .unwrap_or(AllocationState::Active),
        stored
            .as_ref()
            .map(|row| row.try_get::<String, _>("ownership_evidence_reference"))
            .transpose()?
            .unwrap_or_else(|| format!("stripe:personal-operation:{operation_id}")),
    ) {
        Ok(allocation) => allocation,
        Err(_) => {
            return Ok(PersonalInvoiceCoveragePreparation::Rejected(
                "provider_allocation_invalid",
            ));
        }
    };
    let previous_provider_item_id = stored
        .as_ref()
        .map(|row| row.try_get::<String, _>("provider_item_id"))
        .transpose()?;
    let provider_item_rebind_allowed = previous_provider_item_id
        .as_deref()
        .is_some_and(|previous| previous != allocation.provider_item_id)
        && account_paid_through_epoch == Some(evidence.period_start);
    if stored.as_ref().is_some_and(|row| {
        row.try_get::<String, _>("beneficiary_id").ok().as_deref()
            != Some(allocation.beneficiary_id.as_str())
            || row
                .try_get::<String, _>("provider_customer_id")
                .ok()
                .as_deref()
                != Some(allocation.provider_customer_id.as_str())
            || row
                .try_get::<String, _>("provider_subscription_id")
                .ok()
                .as_deref()
                != Some(allocation.subscription_id.as_str())
            || (row.try_get::<String, _>("provider_item_id").ok().as_deref()
                != Some(allocation.provider_item_id.as_str())
                && !provider_item_rebind_allowed)
            || row.try_get::<String, _>("payer_kind").ok().as_deref() != Some("personal")
            || row
                .try_get::<String, _>("provider_account_id")
                .ok()
                .as_deref()
                != Some(context.account_id.as_str())
            || row
                .try_get::<String, _>("provider_environment")
                .ok()
                .as_deref()
                != Some(context.environment.as_str())
    }) {
        return Ok(PersonalInvoiceCoveragePreparation::Rejected(
            "provider_allocation_mismatch",
        ));
    }
    Ok(PersonalInvoiceCoveragePreparation::Prepared(Box::new(
        PreparedPersonalInvoiceCoverage {
            context,
            event: evidence.event,
            allocation,
            operation_id,
            transfer_id: transfer_link_id,
            offer,
            payment_reference: evidence.payment_intent_id,
            period_start: evidence.period_start,
            period_end: evidence.period_end,
            previous_provider_item_id,
            provider_item_rebind_allowed,
        },
    )))
}

fn parse_allocation_state(value: &str) -> Result<AllocationState> {
    match value {
        "pending" => Ok(AllocationState::Pending),
        "active" => Ok(AllocationState::Active),
        "ended" => Ok(AllocationState::Ended),
        _ => Err(Error::Internal(
            "personal provider allocation state is invalid".into(),
        )),
    }
}

async fn accept_personal_invoice_coverage(
    tx: &mut Transaction<'_, Postgres>,
    context: &ProviderContext,
    event: &crate::cloud_provider::VerifiedProviderEvent,
    allocation: &VerifiedAllocation,
    previous_provider_item_id: Option<&str>,
    provider_item_rebind_allowed: bool,
    ingest_enabled: bool,
) -> Result<()> {
    record_verified_event(tx, context, event)
        .await
        .map_err(|_| Error::Internal("personal provider event receipt failed".into()))?;
    if provider_item_rebind_allowed {
        let previous_item_id = previous_provider_item_id.ok_or_else(|| {
            Error::Internal("personal provider item transition lacks its previous binding".into())
        })?;
        crate::cloud_provider::rebind_personal_allocation_item(
            tx,
            context,
            allocation,
            previous_item_id,
        )
        .await
        .map_err(|_| {
            Error::Internal("personal provider item binding could not be advanced".into())
        })?;
    }
    register_verified_allocation(tx, context, allocation)
        .await
        .map_err(|_| Error::Internal("personal provider source registration failed".into()))?;
    accept_provider_invalidation(tx, context, event, allocation, ingest_enabled)
        .await
        .map_err(|_| Error::Internal("personal provider invalidation failed".into()))?;
    Ok(())
}

async fn accept_sponsored_invoice_coverage(
    tx: &mut Transaction<'_, Postgres>,
    prepared: &PreparedSponsoredInvoiceCoverage,
) -> Result<bool> {
    record_verified_event(tx, &prepared.context, &prepared.event)
        .await
        .map_err(|_| Error::Internal("sponsored provider event receipt failed".into()))?;

    let mut anchors = std::collections::BTreeMap::<String, VerifiedAllocation>::new();
    for seat in &prepared.seats {
        let operation_state: Option<String> = sqlx::query_scalar(
            "SELECT state FROM billing_sponsored_operations WHERE operation_id = $1 FOR UPDATE",
        )
        .bind(&seat.operation_id)
        .fetch_optional(&mut **tx)
        .await?;
        if operation_state.as_deref() != Some("active") {
            continue;
        }
        let mut allocation = if let Some(existing) =
            load_sponsored_allocation(tx, &seat.allocation.allocation_id).await?
        {
            if !same_sponsored_allocation_identity(&existing, &seat.allocation) {
                return Err(Error::Internal(
                    "sponsored allocation identity changed during invoice processing".into(),
                ));
            }
            existing
        } else {
            seat.allocation.clone()
        };
        register_verified_allocation(tx, &prepared.context, &allocation)
            .await
            .map_err(|_| {
                Error::Internal("sponsored provider allocation registration failed".into())
            })?;
        record_sponsored_allocation_term(tx, &prepared.context, &allocation, &seat.price_id)
            .await
            .map_err(|_| Error::Internal("sponsored allocation term registration failed".into()))?;

        match (allocation.effective_until, seat.desired_effective_until) {
            (None, Some(effective_until)) => {
                end_sponsored_allocation(
                    tx,
                    &prepared.context,
                    &allocation,
                    effective_until,
                    &seat.ending_evidence_reference,
                )
                .await
                .map_err(|_| {
                    Error::Internal("sponsored allocation end evidence conflicted".into())
                })?;
                allocation.effective_until = Some(effective_until);
            }
            (Some(stored_until), Some(desired_until)) if stored_until == desired_until => {}
            (None, None) => {}
            _ => {
                return Err(Error::Internal(
                    "sponsored allocation boundary changed during invoice processing".into(),
                ));
            }
        }
        let updated = sqlx::query(
            "UPDATE billing_sponsored_seats \
             SET allocation_id = $2, provider_item_id = $3, offer = $8, updated_at = now() \
             WHERE seat_id = $1 AND beneficiary_id = $4 AND offer = $5 \
               AND effective_from = $6 AND effective_until IS NOT DISTINCT FROM $7 \
               AND (allocation_id IS NULL OR allocation_id = $2) \
               AND state IN ('pending','active','scheduled_removal','replaced','canceled') \
               AND EXISTS(SELECT 1 FROM billing_sponsored_operations operation \
                          WHERE operation.operation_id = billing_sponsored_seats.operation_id \
                            AND operation.state = 'active')",
        )
        .bind(&seat.seat_id)
        .bind(&allocation.allocation_id)
        .bind(&seat.provider_item_id)
        .bind(&allocation.beneficiary_id)
        .bind(seat.registered_offer.as_str())
        .bind(allocation.effective_from)
        .bind(seat.desired_effective_until)
        .bind(seat.offer.as_str())
        .execute(&mut **tx)
        .await?;
        if updated.rows_affected() != 1 {
            return Err(Error::Internal(
                "sponsored seat changed during invoice processing".into(),
            ));
        }
        anchors
            .entry(allocation.beneficiary_id.clone())
            .and_modify(|anchor| {
                if allocation.allocation_id < anchor.allocation_id {
                    *anchor = allocation.clone();
                }
            })
            .or_insert(allocation);
    }

    for allocation in anchors.values() {
        accept_provider_invalidation(tx, &prepared.context, &prepared.event, allocation, true)
            .await
            .map_err(|_| Error::Internal("sponsored provider invalidation failed".into()))?;
    }
    if anchors.is_empty() {
        reject_verified_event(
            tx,
            &prepared.context,
            &prepared.event,
            "sponsored_invoice_no_eligible_seats",
        )
        .await
        .map_err(|_| Error::Internal("sponsored provider evidence rejection failed".into()))?;
        return Ok(false);
    }
    Ok(true)
}

async fn reject_sponsored_invoice_coverage(
    tx: &mut Transaction<'_, Postgres>,
    context: &ProviderContext,
    event: &crate::cloud_provider::VerifiedProviderEvent,
    rejection_code: &str,
) -> Result<()> {
    record_verified_event(tx, context, event)
        .await
        .map_err(|_| Error::Internal("sponsored provider event receipt failed".into()))?;
    reject_verified_event(tx, context, event, rejection_code)
        .await
        .map_err(|_| Error::Internal("sponsored provider evidence rejection failed".into()))?;
    Ok(())
}

async fn load_sponsored_allocation(
    tx: &mut Transaction<'_, Postgres>,
    allocation_id: &str,
) -> Result<Option<VerifiedAllocation>> {
    let row = sqlx::query(
        "SELECT allocation.allocation_id, allocation.payer_id, payer.provider_customer_id, \
                payer.payer_kind, allocation.beneficiary_id, allocation.provider_subscription_id, \
                allocation.provider_item_id, allocation.external_allocation_reference, \
                allocation.coverage_source_id, allocation.effective_from, allocation.effective_until, \
                allocation.state, allocation.ownership_evidence_reference \
         FROM cloud_provider_allocations AS allocation \
         JOIN cloud_provider_payers AS payer ON payer.payer_id = allocation.payer_id \
         WHERE allocation.allocation_id = $1",
    )
    .bind(allocation_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let payer_kind = match row.try_get::<String, _>("payer_kind")?.as_str() {
        "sponsor" => ProviderPayerKind::Sponsor,
        "personal" => ProviderPayerKind::Personal,
        _ => {
            return Err(Error::Internal(
                "sponsored allocation payer is corrupt".into(),
            ))
        }
    };
    let state = parse_allocation_state(&row.try_get::<String, _>("state")?)?;
    Ok(Some(
        VerifiedAllocation::new(
            row.try_get::<String, _>("allocation_id")?,
            row.try_get::<String, _>("payer_id")?,
            row.try_get::<String, _>("provider_customer_id")?,
            payer_kind,
            row.try_get::<String, _>("beneficiary_id")?,
            row.try_get::<String, _>("provider_subscription_id")?,
            row.try_get::<String, _>("provider_item_id")?,
            row.try_get::<String, _>("external_allocation_reference")?,
            row.try_get::<String, _>("coverage_source_id")?,
            row.try_get("effective_from")?,
            row.try_get("effective_until")?,
            state,
            row.try_get::<String, _>("ownership_evidence_reference")?,
        )
        .map_err(|_| Error::Internal("sponsored allocation is corrupt".into()))?,
    ))
}

fn same_sponsored_allocation_identity(
    existing: &VerifiedAllocation,
    expected: &VerifiedAllocation,
) -> bool {
    existing.allocation_id == expected.allocation_id
        && existing.payer_id == expected.payer_id
        && existing.provider_customer_id == expected.provider_customer_id
        && existing.payer_kind == ProviderPayerKind::Sponsor
        && existing.beneficiary_id == expected.beneficiary_id
        && existing.subscription_id == expected.subscription_id
        && existing.provider_item_id == expected.provider_item_id
        && existing.external_allocation_reference == expected.external_allocation_reference
        && existing.source_id == expected.source_id
        && existing.effective_from == expected.effective_from
        && existing.ownership_evidence_reference == expected.ownership_evidence_reference
}

fn invoice_allocation_reference_claim(invoice: &serde_json::Value) -> Option<String> {
    let root = invoice.get("metadata");
    let parent = invoice
        .get("parent")
        .and_then(|parent| parent.get("subscription_details"))
        .and_then(|details| details.get("metadata"));
    let line = invoice
        .get("lines")
        .and_then(|lines| lines.get("data"))
        .and_then(serde_json::Value::as_array)
        .filter(|lines| lines.len() == 1)
        .and_then(|lines| lines.first())
        .and_then(|line| line.get("metadata"));
    [line, parent, root]
        .into_iter()
        .flatten()
        .find_map(|metadata| metadata.get("sotto_allocation_reference"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

fn invoice_metadata_claim<'a>(invoice: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    invoice
        .get("parent")
        .and_then(|parent| parent.get("subscription_details"))
        .and_then(|details| details.get("metadata"))
        .and_then(|metadata| metadata.get(key))
        .and_then(serde_json::Value::as_str)
}

fn invoice_subscription_id_claim(invoice: &serde_json::Value) -> Option<String> {
    invoice
        .get("parent")
        .and_then(|parent| parent.get("subscription_details"))
        .and_then(|details| details.get("subscription"))
        .and_then(stripe_reference)
        .or_else(|| invoice.get("subscription").and_then(stripe_reference))
}

fn stripe_reference(value: &serde_json::Value) -> Option<String> {
    value
        .as_str()
        .or_else(|| value.get("id").and_then(serde_json::Value::as_str))
        .map(str::to_owned)
}

#[cfg(feature = "e2e-stripe-test-clock")]
fn valid_stripe_id(value: &str, prefix: &str) -> bool {
    value.strip_prefix(prefix).is_some_and(|suffix| {
        !suffix.is_empty()
            && suffix
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    })
}

#[cfg(feature = "e2e-stripe-test-clock")]
fn valid_run_id(value: &str) -> bool {
    uuid::Uuid::parse_str(value).is_ok_and(|parsed| parsed.to_string() == value)
}

#[cfg(feature = "e2e-stripe-test-clock")]
fn valid_e2e_customer_user_id(value: &str) -> bool {
    !value.trim().is_empty() && value.chars().count() <= 500 && !value.chars().any(char::is_control)
}

#[cfg(feature = "e2e-stripe-test-clock")]
fn e2e_test_clock_customer_matches(
    customer: &serde_json::Value,
    customer_id: &str,
    clock_id: &str,
    run_id: &str,
    user_id: Option<&str>,
) -> bool {
    customer["id"].as_str() == Some(customer_id)
        && customer["object"].as_str() == Some("customer")
        && customer["livemode"].as_bool() == Some(false)
        && customer["deleted"].as_bool() != Some(true)
        && stripe_reference(&customer["test_clock"]).as_deref() == Some(clock_id)
        && customer["metadata"]["sotto_lifecycle_run_id"].as_str() == Some(run_id)
        && user_id.is_none_or(|user_id| {
            customer["metadata"]["sotto_lifecycle_user_id"].as_str() == Some(user_id)
        })
}

fn billing_offer_matches_price(
    catalogue: Option<&BillingPriceIds>,
    offer: &str,
    price_id: &str,
) -> bool {
    catalogue.is_some_and(|catalogue| {
        BillingOffer::ALL
            .into_iter()
            .find(|candidate| candidate.as_str() == offer)
            .is_some_and(|candidate| catalogue.id_for(candidate) == price_id)
    })
}

#[cfg(all(test, feature = "e2e-stripe-test-clock"))]
mod e2e_stripe_test_clock_tests {
    use super::{
        e2e_test_clock_customer_matches, valid_e2e_customer_user_id, valid_run_id, valid_stripe_id,
    };
    use serde_json::json;

    const RUN_ID: &str = "7e57d004-2b97-4e7f-9f47-68d9f5d9e3c1";
    const USER_ID: &str = "e2e-e2e-invitee";

    fn customer() -> serde_json::Value {
        json!({
            "id": "cus_clock123",
            "object": "customer",
            "livemode": false,
            "test_clock": {"id": "clock_123", "object": "test_helpers.test_clock"},
            "metadata": {"sotto_lifecycle_run_id": RUN_ID}
        })
    }

    #[test]
    fn test_clock_customer_must_match_run_clock_and_test_mode() {
        let value = customer();
        assert!(e2e_test_clock_customer_matches(
            &value,
            "cus_clock123",
            "clock_123",
            RUN_ID,
            None,
        ));
        assert!(!e2e_test_clock_customer_matches(
            &value,
            "cus_other",
            "clock_123",
            RUN_ID,
            None,
        ));
        assert!(!e2e_test_clock_customer_matches(
            &value,
            "cus_clock123",
            "clock_other",
            RUN_ID,
            None,
        ));
        assert!(!e2e_test_clock_customer_matches(
            &value,
            "cus_clock123",
            "clock_123",
            "00000000-0000-0000-0000-000000000000",
            None,
        ));
        let mut live = value.clone();
        live["livemode"] = json!(true);
        assert!(!e2e_test_clock_customer_matches(
            &live,
            "cus_clock123",
            "clock_123",
            RUN_ID,
            None,
        ));
        let mut deleted = value.clone();
        deleted["deleted"] = json!(true);
        assert!(!e2e_test_clock_customer_matches(
            &deleted,
            "cus_clock123",
            "clock_123",
            RUN_ID,
            None,
        ));
        let mut claimed = value;
        claimed["metadata"]["sotto_lifecycle_user_id"] = json!(USER_ID);
        assert!(e2e_test_clock_customer_matches(
            &claimed,
            "cus_clock123",
            "clock_123",
            RUN_ID,
            Some(USER_ID),
        ));
        assert!(!e2e_test_clock_customer_matches(
            &claimed,
            "cus_clock123",
            "clock_123",
            RUN_ID,
            Some("00000000-0000-0000-0000-000000000000"),
        ));
    }

    #[test]
    fn test_clock_binding_identifiers_are_narrowly_validated() {
        assert!(valid_stripe_id("cus_clock123", "cus_"));
        assert!(valid_stripe_id("clock_123", "clock_"));
        assert!(!valid_stripe_id("cus_../customers", "cus_"));
        assert!(!valid_stripe_id("cus_", "cus_"));
        assert!(valid_run_id(RUN_ID));
        assert!(!valid_run_id("../customers"));
        assert!(valid_run_id("e4a7bc0d-7f4b-4f2b-93aa-9d4086cbd11e"));
        assert!(valid_e2e_customer_user_id("e2e-e2e-invitee"));
        assert!(valid_e2e_customer_user_id(
            "e4a7bc0d-7f4b-4f2b-93aa-9d4086cbd11e"
        ));
        assert!(!valid_e2e_customer_user_id(" \t"));
        assert!(!valid_e2e_customer_user_id("user\nwith-control"));
        assert!(!valid_e2e_customer_user_id(&"x".repeat(501)));
    }

    #[tokio::test]
    async fn stripe_claim_requires_an_unused_owned_test_clock_customer() {
        use super::{StripeBilling, SubscriptionProvider};
        use axum::{
            body::{to_bytes, Body},
            extract::State,
            http::{Method, Request, StatusCode},
            response::IntoResponse,
            routing::any,
            Json, Router,
        };
        use serde_json::{json, Value};
        use std::sync::{Arc, Mutex};
        use tokio::net::TcpListener;

        #[derive(Clone, Default)]
        struct FakeState(Arc<Mutex<Vec<(Method, String, String)>>>);

        async fn fake_stripe(
            State(state): State<FakeState>,
            request: Request<Body>,
        ) -> impl IntoResponse {
            let method = request.method().clone();
            let path = request.uri().path().to_owned();
            let body = to_bytes(request.into_body(), 1024)
                .await
                .map(|body| String::from_utf8_lossy(&body).into_owned())
                .unwrap_or_default();
            state.0.lock().expect("fake request log lock").push((
                method.clone(),
                path.clone(),
                body.clone(),
            ));
            let response: Value = match (method, path.as_str()) {
                (Method::GET, "/v1/customers/cus_clock123") => json!({
                    "id": "cus_clock123", "object": "customer", "livemode": false,
                    "test_clock": "clock_123", "metadata": {"sotto_lifecycle_run_id": RUN_ID}
                }),
                (Method::GET, "/v1/subscriptions") => json!({"data": []}),
                (Method::POST, "/v1/customers/cus_clock123")
                    if body
                        .contains(&format!("metadata%5Bsotto_lifecycle_user_id%5D={USER_ID}")) =>
                {
                    json!({
                        "id": "cus_clock123", "object": "customer", "livemode": false,
                        "test_clock": "clock_123", "metadata": {
                            "sotto_lifecycle_run_id": RUN_ID,
                            "sotto_lifecycle_user_id": USER_ID
                        }
                    })
                }
                _ => {
                    return (
                        StatusCode::NOT_FOUND,
                        Json(json!({"error": {"code": "not_found"}})),
                    )
                        .into_response();
                }
            };
            Json(response).into_response()
        }

        let fake_state = FakeState::default();
        let app = Router::new()
            .fallback(any(fake_stripe))
            .with_state(fake_state.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let provider = StripeBilling {
            api_key: "rk_test_private".into(),
            price_id: "price_test".into(),
            api_origin: format!("http://{address}"),
        };

        provider
            .claim_e2e_test_clock_customer("cus_clock123", "clock_123", RUN_ID, USER_ID)
            .await
            .unwrap();
        let calls = fake_state.0.lock().unwrap().clone();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].0, Method::GET);
        assert_eq!(calls[1].1, "/v1/subscriptions");
        assert_eq!(calls[2].0, Method::POST);
        assert!(calls[2]
            .2
            .contains(&format!("metadata%5Bsotto_lifecycle_user_id%5D={USER_ID}")));
        server.abort();
    }
}

fn opaque_provider_id(prefix: &str, parts: &[&str]) -> String {
    let mut digest = Sha256::new();
    for part in parts {
        digest.update((part.len() as u64).to_be_bytes());
        digest.update(part.as_bytes());
    }
    let hash = digest.finalize();
    format!(
        "{prefix}:{}",
        hash.iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

fn personal_renewal_notice_event_key(
    user_id: &str,
    subscription_id: &str,
    invoice_id: &str,
) -> String {
    opaque_provider_id(
        "personal-renewal-v1",
        &[user_id, subscription_id, invoice_id],
    )
}

/// Persist the customer-facing failure notice before any optional Stripe history read. The
/// webhook receipt and notice commit together, but the receipt remains pending until the regular
/// webhook transaction accepts the billing and coverage work.
async fn record_personal_renewal_failure_notice(pool: &PgPool, event: &Event) -> Result<bool> {
    let invoice = &event.data.object;
    let Some(subscription_id) = invoice_subscription_id_claim(invoice) else {
        log_personal_renewal_notice_skip("subscription_missing");
        return Ok(false);
    };
    let Some(invoice_id) = invoice["id"].as_str().filter(|value| !value.is_empty()) else {
        log_personal_renewal_notice_skip("invoice_id_missing");
        return Ok(false);
    };
    let Some(customer_id) = stripe_reference(&invoice["customer"]) else {
        log_personal_renewal_notice_skip("customer_missing");
        return Ok(false);
    };
    // `paid_through_epoch` retains Stripe's time of day, so compare the exact line boundary.
    let Some(period_start) = invoice_period_start(invoice)
        .filter(|value| FoundingDate::from_unix_seconds(*value).is_ok())
    else {
        log_personal_renewal_notice_skip("period_start_missing_or_invalid");
        return Ok(false);
    };
    let shape_skip_reason = if invoice["object"].as_str() != Some("invoice") {
        Some("not_an_invoice")
    } else if invoice["billing_reason"].as_str() != Some("subscription_cycle") {
        Some("not_a_subscription_cycle")
    } else if invoice["collection_method"].as_str() != Some("charge_automatically") {
        Some("not_automatic_collection")
    } else if invoice["status"].as_str() != Some("open") {
        Some("invoice_not_open")
    } else if invoice["amount_remaining"]
        .as_i64()
        .is_none_or(|amount| amount <= 0)
    {
        Some("amount_remaining_not_positive")
    } else {
        None
    };
    if let Some(reason) = shape_skip_reason {
        log_personal_renewal_notice_skip(reason);
        return Ok(false);
    }

    let mut tx = pool.begin().await?;
    prune_webhook_events(&mut tx).await?;
    let disposition = record_webhook_event(&mut tx, event).await?;
    let outcome = if disposition == EventDisposition::Ignore {
        PersonalRenewalNoticeOutcome::WebhookAlreadyProcessed
    } else {
        let account = sqlx::query(
            "SELECT user_id, paid_through_epoch, state FROM billing_personal_accounts \
             WHERE stripe_subscription_id=$1 AND stripe_customer_id=$2 \
               AND state IN ('active','past_due','unpaid') FOR UPDATE",
        )
        .bind(&subscription_id)
        .bind(&customer_id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(account) = account {
            let user_id: String = account.try_get("user_id")?;
            let paid_through: Option<i64> = account.try_get("paid_through_epoch")?;
            if let Some(paid_through) = paid_through {
                if period_start < paid_through {
                    PersonalRenewalNoticeOutcome::Skipped("invoice_period_precedes_paid_through")
                } else {
                    let deadline = paid_through
                        .checked_add(crate::cloud_coverage::RENEWAL_RECOVERY_SECONDS)
                        .ok_or_else(|| {
                            Error::Internal("personal recovery deadline overflowed".into())
                        })?;
                    let due_at_epoch: i64 =
                        sqlx::query_scalar("SELECT floor(extract(epoch FROM now()))::bigint")
                            .fetch_one(&mut *tx)
                            .await?;
                    let intent = crate::notifications::NoticeIntent {
                recipient_user_id: user_id.clone(),
                event_key: personal_renewal_notice_event_key(
                    &user_id,
                    &subscription_id,
                    invoice_id,
                ),
                policy_key: "personal-renewal-failure-v1".into(),
                kind: crate::notifications::NoticeKind::FailedRenewal,
                channel: crate::notifications::NoticeChannel::InApp,
                contact_id: None,
                scope_id: None,
                due_at_epoch,
                content: crate::notifications::NoticeContent {
                    title: "Action needed for your Cloud renewal".into(),
                    detail: "Your renewal payment was unsuccessful. Update your payment method in Cloud billing before the recovery deadline.".into(),
                    effective_at_epoch: Some(paid_through),
                    deadline_epoch: Some(deadline),
                    amount_pence: None,
                    price_interval: None,
                },
            };
                    let outcome = crate::notifications::enqueue_in_tx(&mut tx, &intent)
                        .await
                        .map_err(notification_error)?;
                    match outcome {
                        crate::notifications::EnqueueOutcome::Enqueued => {
                            PersonalRenewalNoticeOutcome::Enqueued
                        }
                        crate::notifications::EnqueueOutcome::AlreadyQueued => {
                            PersonalRenewalNoticeOutcome::AlreadyQueued
                        }
                    }
                }
            } else {
                PersonalRenewalNoticeOutcome::Skipped("paid_through_missing")
            }
        } else {
            PersonalRenewalNoticeOutcome::Skipped("personal_account_binding_not_found")
        }
    };
    tx.commit().await?;
    log_personal_renewal_notice_outcome(outcome);
    Ok(matches!(
        outcome,
        PersonalRenewalNoticeOutcome::WebhookAlreadyProcessed
    ))
}

#[derive(Clone, Copy)]
enum PersonalRenewalNoticeOutcome {
    Skipped(&'static str),
    Enqueued,
    AlreadyQueued,
    WebhookAlreadyProcessed,
}

fn log_personal_renewal_notice_skip(reason: &'static str) {
    eprintln!("warning: skipped personal renewal failure notice: {reason}");
}

fn log_personal_renewal_notice_outcome(outcome: PersonalRenewalNoticeOutcome) {
    match outcome {
        PersonalRenewalNoticeOutcome::Skipped(reason) => log_personal_renewal_notice_skip(reason),
        PersonalRenewalNoticeOutcome::Enqueued => {
            eprintln!("warning: queued personal renewal failure notice");
        }
        PersonalRenewalNoticeOutcome::AlreadyQueued => {
            eprintln!("warning: personal renewal failure notice was already queued");
        }
        PersonalRenewalNoticeOutcome::WebhookAlreadyProcessed => {
            eprintln!("warning: personal renewal failure webhook was already processed");
        }
    }
}

fn notification_error(error: crate::notifications::NoticeError) -> Error {
    match error {
        crate::notifications::NoticeError::Database(error) => Error::Db(error),
        other => Error::Internal(other.to_string()),
    }
}

/// A verified payment only resolves the matching invoice's failure. The invoice identity is
/// hashed before it enters the outbox; the raw provider identifiers never reach the account API.
async fn enqueue_personal_renewal_recovered_notice(
    tx: &mut Transaction<'_, Postgres>,
    invoice: &serde_json::Value,
    subscription_id: &str,
    event_created: i64,
) -> Result<()> {
    if invoice["object"].as_str() != Some("invoice")
        || invoice["billing_reason"].as_str() != Some("subscription_cycle")
        || invoice["status"].as_str() != Some("paid")
    {
        return Ok(());
    }
    let Some(invoice_id) = invoice["id"].as_str().filter(|value| !value.is_empty()) else {
        return Ok(());
    };
    let Some(customer_id) = stripe_reference(&invoice["customer"]) else {
        return Ok(());
    };
    let Some(period_end) = invoice_period_end(invoice)
        .and_then(|value| FoundingDate::from_unix_seconds(value).ok())
        .map(|value| value.to_unix_seconds())
    else {
        return Ok(());
    };
    let account = sqlx::query(
        "SELECT user_id, paid_through_epoch, state FROM billing_personal_accounts \
         WHERE stripe_subscription_id=$1 AND stripe_customer_id=$2 \
           AND state IN ('active','past_due','unpaid','canceled') FOR UPDATE",
    )
    .bind(subscription_id)
    .bind(customer_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(account) = account else {
        return Ok(());
    };
    let user_id: String = account.try_get("user_id")?;
    let paid_through: Option<i64> = account.try_get("paid_through_epoch")?;
    let account_state: String = account.try_get("state")?;
    if paid_through.is_none_or(|paid_through| paid_through < period_end) {
        return Ok(());
    }
    let event_key = personal_renewal_notice_event_key(&user_id, subscription_id, invoice_id);
    let failed_notice_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM cloud_notice_outbox \
         WHERE recipient_user_id=$1 AND event_key=$2 \
           AND policy_key='personal-renewal-failure-v1' AND kind='failed_renewal' \
           AND channel='in_app')",
    )
    .bind(&user_id)
    .bind(&event_key)
    .fetch_one(&mut **tx)
    .await?;
    if !failed_notice_exists {
        return Ok(());
    }
    // A successful retry makes the original warning obsolete. Retire it in the same transaction
    // as the resolution notice so customers never see contradictory advice after this commits.
    sqlx::query(
        "UPDATE cloud_notice_outbox SET status='cancelled', cancelled_at=now(), \
             lease_owner=NULL, lease_expires_at=NULL, updated_at=now() \
         WHERE recipient_user_id=$1 AND event_key=$2 \
           AND policy_key='personal-renewal-failure-v1' AND kind='failed_renewal' \
           AND status <> 'cancelled'",
    )
    .bind(&user_id)
    .bind(&event_key)
    .execute(&mut **tx)
    .await?;
    // A late paid invoice can arrive after cancellation. It clears the stale payment warning,
    // but must not imply that the cancelled subscription has resumed.
    if account_state == "canceled" {
        return Ok(());
    }
    let due_at_epoch: i64 = sqlx::query_scalar("SELECT floor(extract(epoch FROM now()))::bigint")
        .fetch_one(&mut **tx)
        .await?;
    let intent = crate::notifications::NoticeIntent {
        recipient_user_id: user_id,
        event_key,
        policy_key: "personal-renewal-recovered-v1".into(),
        kind: crate::notifications::NoticeKind::RecoveryResolved,
        channel: crate::notifications::NoticeChannel::InApp,
        contact_id: None,
        scope_id: None,
        due_at_epoch,
        content: crate::notifications::NoticeContent {
            title: "Your Cloud renewal is up to date".into(),
            detail: "Your renewal payment was received. No further action is needed.".into(),
            effective_at_epoch: (event_created > 0).then_some(event_created),
            deadline_epoch: None,
            amount_pence: None,
            price_interval: None,
        },
    };
    crate::notifications::enqueue_in_tx(tx, &intent)
        .await
        .map_err(notification_error)?;
    Ok(())
}

async fn enqueue_sponsored_renewal_failure_notices(
    tx: &mut Transaction<'_, Postgres>,
    organization_id: &str,
    invoice_id: &str,
    beneficiary_id: &str,
    period_start: i64,
    include_administrators: bool,
) -> Result<()> {
    let deadline = period_start
        .checked_add(crate::cloud_coverage::RENEWAL_RECOVERY_SECONDS)
        .ok_or_else(|| Error::Internal("sponsored recovery deadline overflowed".into()))?;
    let due_at_epoch: i64 = sqlx::query_scalar("SELECT floor(extract(epoch FROM now()))::bigint")
        .fetch_one(&mut **tx)
        .await?;
    let event_key = opaque_provider_id("sponsored-renewal-v1", &[organization_id, invoice_id]);
    if !beneficiary_id.is_empty() {
        let intent = crate::notifications::NoticeIntent {
            recipient_user_id: beneficiary_id.to_owned(),
            event_key: event_key.clone(),
            policy_key: "sponsored-renewal-failure-v1".into(),
            kind: crate::notifications::NoticeKind::SponsoredRenewalFailure,
            channel: crate::notifications::NoticeChannel::InApp,
            contact_id: None,
            scope_id: Some(organization_id.to_owned()),
            due_at_epoch,
            content: crate::notifications::NoticeContent {
                title: "Your sponsored Cloud renewal needs attention".into(),
                detail: "Your organisation’s Cloud renewal payment was unsuccessful. Ask an organisation billing admin to review the payment before the recovery deadline.".into(),
                effective_at_epoch: Some(period_start),
                deadline_epoch: Some(deadline),
                amount_pence: None,
                price_interval: None,
            },
        };
        crate::notifications::enqueue_in_tx(tx, &intent)
            .await
            .map_err(notification_error)?;
    }
    if !include_administrators {
        return Ok(());
    }
    let administrators: Vec<String> = sqlx::query_scalar(
        "SELECT membership.user_id FROM organization_memberships membership \
         JOIN organizations organization ON organization.id = membership.org_id \
         WHERE membership.org_id = $1 AND membership.role IN ('owner','admin') \
           AND organization.lifecycle_state = 'active' \
         ORDER BY membership.user_id COLLATE \"C\" LIMIT 129",
    )
    .bind(organization_id)
    .fetch_all(&mut **tx)
    .await?;
    if administrators.len() > 128 {
        return Err(Error::Internal(
            "sponsored renewal administrator scope exceeded".into(),
        ));
    }
    for administrator in administrators {
        let intent = crate::notifications::NoticeIntent {
            recipient_user_id: administrator,
            event_key: event_key.clone(),
            policy_key: "sponsored-renewal-action-v1".into(),
            kind: crate::notifications::NoticeKind::SponsoredRenewalAction,
            channel: crate::notifications::NoticeChannel::InApp,
            contact_id: None,
            scope_id: Some(organization_id.to_owned()),
            due_at_epoch,
            content: crate::notifications::NoticeContent {
                title: "Organisation Cloud renewal needs attention".into(),
                detail: "The organisation’s sponsored Cloud renewal payment was unsuccessful. Review the organisation billing panel before the recovery deadline.".into(),
                effective_at_epoch: Some(period_start),
                deadline_epoch: Some(deadline),
                amount_pence: None,
                price_interval: None,
            },
        };
        crate::notifications::enqueue_in_tx(tx, &intent)
            .await
            .map_err(notification_error)?;
    }
    Ok(())
}

/// Resolve only failures for this exact paid invoice. Notice cancellation, resolution rows and
/// replacement notices share the billing transaction, so a retry cannot leave contradictory UI.
async fn resolve_sponsored_renewal_failures(
    tx: &mut Transaction<'_, Postgres>,
    organization_id: &str,
    subscription_id: &str,
    invoice_id: &str,
    event_created: i64,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO cloud_provider_stripe_renewal_resolutions \
         (provider_namespace, provider_account_id, provider_environment, event_id, \
          beneficiary_id, allocation_id, resolution_invoice_id) \
         SELECT failure.provider_namespace, failure.provider_account_id, \
                failure.provider_environment, failure.event_id, failure.beneficiary_id, \
                failure.allocation_id, $2 \
         FROM cloud_provider_stripe_renewal_failures failure \
         JOIN cloud_provider_allocations allocation ON allocation.allocation_id = failure.allocation_id \
         JOIN cloud_provider_payers payer ON payer.payer_id = allocation.payer_id \
         WHERE failure.provider_namespace = 'stripe' AND failure.subscription_id = $1 \
           AND failure.invoice_id = $2 AND payer.payer_kind = 'sponsor' \
           AND NOT EXISTS (SELECT 1 FROM cloud_provider_stripe_renewal_resolutions resolution \
                           WHERE resolution.provider_namespace = failure.provider_namespace \
                             AND resolution.provider_account_id = failure.provider_account_id \
                             AND resolution.provider_environment = failure.provider_environment \
                             AND resolution.event_id = failure.event_id \
                             AND resolution.beneficiary_id = failure.beneficiary_id \
                             AND resolution.allocation_id = failure.allocation_id) \
         ON CONFLICT DO NOTHING",
    )
    .bind(subscription_id)
    .bind(invoice_id)
    .execute(&mut **tx)
    .await?;
    let event_key = opaque_provider_id("sponsored-renewal-v1", &[organization_id, invoice_id]);
    let due_at_epoch: i64 = sqlx::query_scalar("SELECT floor(extract(epoch FROM now()))::bigint")
        .fetch_one(&mut **tx)
        .await?;
    let notices = sqlx::query(
        "SELECT DISTINCT recipient_user_id COLLATE \"C\" AS recipient_user_id, kind \
         FROM cloud_notice_outbox \
         WHERE event_key = $1 AND kind IN ('sponsored_renewal_failure','sponsored_renewal_action') \
           AND policy_key IN ('sponsored-renewal-failure-v1','sponsored-renewal-action-v1') \
         ORDER BY recipient_user_id COLLATE \"C\" LIMIT 257",
    )
    .bind(&event_key)
    .fetch_all(&mut **tx)
    .await?;
    if notices.len() > 256 {
        return Err(Error::Internal(
            "sponsored renewal notice scope exceeded".into(),
        ));
    }
    sqlx::query(
        "UPDATE cloud_notice_outbox SET status='cancelled', cancelled_at=now(), \
             lease_owner=NULL, lease_expires_at=NULL, updated_at=now() \
         WHERE event_key=$1 AND kind IN ('sponsored_renewal_failure','sponsored_renewal_action') \
           AND policy_key IN ('sponsored-renewal-failure-v1','sponsored-renewal-action-v1') \
           AND status <> 'cancelled'",
    )
    .bind(&event_key)
    .execute(&mut **tx)
    .await?;
    for row in notices {
        let recipient: String = row.try_get("recipient_user_id")?;
        let was_admin = row.try_get::<String, _>("kind")? == "sponsored_renewal_action";
        let intent = crate::notifications::NoticeIntent {
            recipient_user_id: recipient,
            event_key: event_key.clone(),
            policy_key: if was_admin {
                "sponsored-renewal-recovered-admin-v1".into()
            } else {
                "sponsored-renewal-recovered-v1".into()
            },
            kind: crate::notifications::NoticeKind::RecoveryResolved,
            channel: crate::notifications::NoticeChannel::InApp,
            contact_id: None,
            scope_id: None,
            due_at_epoch,
            content: crate::notifications::NoticeContent {
                title: "Organisation Cloud renewal recovered".into(),
                detail: "The organisation’s sponsored Cloud renewal payment was received. The matching renewal warning has been resolved.".into(),
                effective_at_epoch: Some(event_created.max(0)),
                deadline_epoch: None,
                amount_pence: None,
                price_interval: None,
            },
        };
        crate::notifications::enqueue_in_tx(tx, &intent)
            .await
            .map_err(notification_error)?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EventDisposition {
    /// The event can apply normally: either it is newer than the watermark or it is a
    /// payment fact whose ordering is guarded by the subscription lock instead.
    Apply,
    /// The event ties the watermark timestamp and must reconcile against Stripe's current state.
    Reconcile,
    /// The event is a duplicate or older than the stored watermark and changes nothing.
    Ignore,
}

async fn record_webhook_event(
    tx: &mut Transaction<'_, Postgres>,
    event: &Event,
) -> Result<EventDisposition> {
    let subscription_id = event_subscription_id(event);
    if let Some(subscription_id) = subscription_id.as_deref() {
        lock_subscription(tx, subscription_id).await?;
    }
    let inserted = record_webhook_receipt(tx, event, subscription_id.as_deref()).await?;
    if !inserted {
        let processed: bool = sqlx::query_scalar(
            "SELECT processed_at IS NOT NULL FROM stripe_webhook_events WHERE event_id = $1",
        )
        .bind(&event.id)
        .fetch_one(&mut **tx)
        .await?;
        if processed {
            return Ok(EventDisposition::Ignore);
        }
    }

    if is_personal_checkout_event(event)
        || is_sponsored_checkout_event(event)
        || is_sponsored_checkout_expired_event(event)
    {
        return Ok(EventDisposition::Apply);
    }
    if event.kind == "invoice.paid" {
        return Ok(EventDisposition::Apply);
    }
    // A failed invoice is a payment fact, not a subscription-state observation. Keep it under
    // the subscription lock for paid-versus-failed ordering, but let the corresponding
    // customer.subscription.updated event own the watermark and status transition.
    if event.kind == "invoice.payment_failed" {
        return Ok(EventDisposition::Apply);
    }

    let Some(subscription_id) = subscription_id else {
        return Ok(EventDisposition::Apply);
    };
    let watermark: Option<(i64, String)> = sqlx::query_as(
        "SELECT stripe_created, event_id FROM stripe_subscription_watermarks \
         WHERE subscription_id = $1 FOR UPDATE",
    )
    .bind(&subscription_id)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(match watermark {
        Some((created, _)) if event.created < created => {
            mark_webhook_event_processed(tx, &event.id).await?;
            EventDisposition::Ignore
        }
        Some((created, event_id)) if event.created == created && event.id <= event_id => {
            mark_webhook_event_processed(tx, &event.id).await?;
            EventDisposition::Ignore
        }
        Some((created, _)) if event.created == created => EventDisposition::Reconcile,
        _ => EventDisposition::Apply,
    })
}

async fn record_webhook_receipt(
    tx: &mut Transaction<'_, Postgres>,
    event: &Event,
    subscription_id: Option<&str>,
) -> Result<bool> {
    let inserted = sqlx::query(
        "INSERT INTO stripe_webhook_events \
         (event_id, event_type, api_version, stripe_created, subscription_id, provider_object_id) \
         VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (event_id) DO NOTHING",
    )
    .bind(&event.id)
    .bind(&event.kind)
    .bind(event.api_version.as_deref().unwrap_or("missing"))
    .bind(event.created)
    .bind(subscription_id)
    .bind(
        event.data.object["id"]
            .as_str()
            .filter(|id| !id.trim().is_empty()),
    )
    .execute(&mut **tx)
    .await?
    .rows_affected()
        == 1;
    Ok(inserted)
}

async fn prune_webhook_events(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    sqlx::query(
        "WITH candidates AS (
             SELECT e.event_id
             FROM stripe_webhook_events e
             WHERE (e.processed_at < now() - interval '30 days'
                    OR (e.processed_at IS NULL
                        AND e.received_at < now() - interval '1 day'))
               AND NOT EXISTS (
                   SELECT 1
                   FROM stripe_subscription_watermarks w
                   WHERE w.event_id = e.event_id
               )
             ORDER BY e.received_at
             LIMIT 500
         )
         DELETE FROM stripe_webhook_events e
         USING candidates
         WHERE e.event_id = candidates.event_id",
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn lock_subscription(
    tx: &mut Transaction<'_, Postgres>,
    subscription_id: &str,
) -> Result<()> {
    // Advisory locking serialises receipt decisions across server instances without locking the
    // organisation row. The stable 64-bit text hash is sufficient here; a collision only causes
    // unrelated subscriptions to wait briefly.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(subscription_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn mark_webhook_event_processed(
    tx: &mut Transaction<'_, Postgres>,
    event_id: &str,
) -> Result<()> {
    sqlx::query("UPDATE stripe_webhook_events SET processed_at = now() WHERE event_id = $1")
        .bind(event_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn update_subscription_watermark(
    tx: &mut Transaction<'_, Postgres>,
    event: &Event,
    subscription_id: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO stripe_subscription_watermarks (subscription_id, stripe_created, event_id) \
         VALUES ($1, $2, $3) \
         ON CONFLICT (subscription_id) DO UPDATE SET stripe_created = EXCLUDED.stripe_created, \
         event_id = EXCLUDED.event_id, updated_at = now() \
         WHERE (EXCLUDED.stripe_created, EXCLUDED.event_id) > \
               (stripe_subscription_watermarks.stripe_created, stripe_subscription_watermarks.event_id)",
    )
    .bind(subscription_id)
    .bind(event.created)
    .bind(&event.id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn complete_repaired_invoice_event(
    tx: &mut Transaction<'_, Postgres>,
    event: &Event,
) -> Result<()> {
    if event_updates_subscription_watermark(event) {
        if let Some(subscription_id) = event_subscription_id(event) {
            update_subscription_watermark(tx, event, &subscription_id).await?;
        }
    }
    mark_webhook_event_processed(tx, &event.id).await
}

fn event_updates_subscription_watermark(event: &Event) -> bool {
    event.kind != "invoice.payment_failed"
}

fn event_subscription_id(event: &Event) -> Option<String> {
    match event.kind.as_str() {
        "checkout.session.completed" | "checkout.session.async_payment_succeeded" => {
            event.data.object["subscription"]
                .as_str()
                .map(str::to_string)
        }
        "invoice.paid" => invoice_subscription_id(&event.data.object).map(str::to_string),
        "invoice.payment_failed" => invoice_subscription_id_claim(&event.data.object),
        "customer.subscription.updated" | "customer.subscription.deleted" => {
            event.data.object["id"].as_str().map(str::to_string)
        }
        _ => None,
    }
}

fn event_org_hint(event: &Event) -> Option<&str> {
    match event.kind.as_str() {
        "checkout.session.completed" | "checkout.session.async_payment_succeeded" => {
            event.data.object["client_reference_id"].as_str()
        }
        "customer.subscription.updated" | "customer.subscription.deleted" => event.data.object
            ["metadata"]["organization_id"]
            .as_str()
            .or_else(|| event.data.object["metadata"]["org_id"].as_str()),
        _ => None,
    }
}

fn event_is_sponsored_subscription(event: &Event) -> bool {
    matches!(
        event.kind.as_str(),
        "customer.subscription.updated" | "customer.subscription.deleted"
    ) && event.data.object["metadata"]["organization_id"]
        .as_str()
        .is_some()
}

fn is_personal_checkout_event(event: &Event) -> bool {
    matches!(
        event.kind.as_str(),
        "checkout.session.completed" | "checkout.session.async_payment_succeeded"
    ) && event.data.object["client_reference_id"]
        .as_str()
        .is_some_and(|reference| reference.starts_with("personal:"))
}

fn is_sponsored_checkout_event(event: &Event) -> bool {
    matches!(
        event.kind.as_str(),
        "checkout.session.completed" | "checkout.session.async_payment_succeeded"
    ) && event.data.object["client_reference_id"]
        .as_str()
        .is_some_and(|reference| reference.starts_with("sponsored:"))
}

fn is_sponsored_checkout_expired_event(event: &Event) -> bool {
    event.kind == "checkout.session.expired"
        && event.data.object["client_reference_id"]
            .as_str()
            .is_some_and(|reference| reference.starts_with("sponsored:"))
}

fn is_checkout_expired_event(event: &Event) -> bool {
    is_sponsored_checkout_expired_event(event)
        || (event.kind == "checkout.session.expired"
            && event.data.object["client_reference_id"]
                .as_str()
                .is_some_and(|reference| reference.starts_with("personal:")))
}

async fn sponsored_checkout_expired(
    tx: &mut Transaction<'_, Postgres>,
    object: &serde_json::Value,
) -> Result<()> {
    let Some(reference) = object["client_reference_id"].as_str() else {
        return Ok(());
    };
    if let Some(operation_id) = reference.strip_prefix("personal:") {
        let session_id = object["id"]
            .as_str()
            .filter(|session_id| !session_id.trim().is_empty())
            .ok_or_else(|| Error::Config("expired Checkout session has no id".into()))?;
        // Match personal checkout's founding-capacity -> operation/account lock order. Stripe's
        // signed expiry event is the evidence that permits the pending account to be reused.
        founding_allocator::lock_capacity(tx)
            .await
            .map_err(|error| Error::Internal(error.to_string()))?;
        let account_state: Option<String> = sqlx::query_scalar(
            "SELECT state FROM billing_personal_accounts WHERE operation_id=$1 FOR UPDATE",
        )
        .bind(operation_id)
        .fetch_optional(&mut **tx)
        .await?;
        if matches!(
            account_state.as_deref(),
            Some("active" | "past_due" | "unpaid" | "refund_required")
        ) {
            // A paid settlement wins over a delayed expiry delivery.
            return Ok(());
        }
        let operation = billing_operations::record_checkout_expired(tx, operation_id, session_id)
            .await
            .map_err(Error::from)?;
        if account_state.as_deref() == Some("pending") {
            sqlx::query(
                "UPDATE billing_personal_accounts SET state='canceled', updated_at=now() \
                 WHERE operation_id=$1 AND state='pending'",
            )
            .bind(operation_id)
            .execute(&mut **tx)
            .await?;
        }
        founding_allocator::release_reservation(tx, operation_id)
            .await
            .map_err(|error| Error::Internal(error.to_string()))?;
        crate::notifications::enqueue_checkout_outcome_in_tx(
            tx,
            &operation.beneficiary_id,
            operation_id,
            crate::notifications::CheckoutOutcome::Expired,
        )
        .await
        .map_err(notification_error)?;
        return Ok(());
    }
    if !sponsored_billing_enabled() {
        return Ok(());
    }
    let Some(operation_id) = reference.strip_prefix("sponsored:") else {
        return Ok(());
    };
    let operation =
        sponsored_billing::cancel_failed_operation(tx, operation_id, "checkout_expired")
            .await
            .map_err(Error::from)?;
    crate::notifications::enqueue_checkout_outcome_in_tx(
        tx,
        &operation.beneficiary_id,
        operation_id,
        crate::notifications::CheckoutOutcome::Expired,
    )
    .await
    .map_err(notification_error)?;
    if operation.actor_user_id != operation.beneficiary_id {
        crate::notifications::enqueue_checkout_outcome_in_tx(
            tx,
            &operation.actor_user_id,
            operation_id,
            crate::notifications::CheckoutOutcome::Expired,
        )
        .await
        .map_err(notification_error)?;
    }
    Ok(())
}

async fn enqueue_sponsored_checkout_outcome(
    tx: &mut Transaction<'_, Postgres>,
    operation_id: &str,
    outcome: crate::notifications::CheckoutOutcome,
) -> Result<()> {
    let recipients: Option<(String, String)> = sqlx::query_as(
        "SELECT beneficiary_id,actor_user_id FROM billing_sponsored_operations WHERE operation_id=$1",
    )
    .bind(operation_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((beneficiary_id, actor_user_id)) = recipients else {
        return Ok(());
    };
    crate::notifications::enqueue_checkout_outcome_in_tx(
        tx,
        &beneficiary_id,
        operation_id,
        outcome,
    )
    .await
    .map_err(notification_error)?;
    if actor_user_id != beneficiary_id {
        crate::notifications::enqueue_checkout_outcome_in_tx(
            tx,
            &actor_user_id,
            operation_id,
            outcome,
        )
        .await
        .map_err(notification_error)?;
    }
    Ok(())
}

/// A paid checkout: record the Stripe ids and grant the Team tier. Idempotent - a redelivered
/// event changes no rows and writes no duplicate audit entry.
async fn checkout_completed(
    tx: &mut Transaction<'_, Postgres>,
    object: &serde_json::Value,
    event_created: i64,
    async_payment_succeeded: bool,
    personal_period: Option<PersonalSubscriptionPeriod>,
    sponsored_snapshot: Option<&SponsoredSubscriptionSnapshot>,
) -> Result<()> {
    if object["mode"].as_str() == Some("setup") {
        // Setup Checkout is fulfilled only by the payer-transfer setup handler. It is never a
        // subscription checkout and must not fall through to legacy organisation billing.
        return Ok(());
    }
    // Sessions this server creates always carry the org id; anything else isn't ours to act on.
    // Said out loud, because ignoring an event and acting on one are indistinguishable from
    // outside: both answer 200, and Stripe records both as delivered. A payment that completes
    // while the tier stays put leaves nothing behind to look at otherwise.
    let Some(org_id) = object["client_reference_id"].as_str() else {
        eprintln!(
            "warning: ignored checkout.session.completed with no client_reference_id; \
             it was not created by this server"
        );
        return Ok(());
    };
    if let Some(operation_id) = org_id.strip_prefix("personal:") {
        return personal_checkout_completed(
            tx,
            object,
            event_created,
            operation_id,
            async_payment_succeeded,
            personal_period,
        )
        .await;
    }
    if let Some(operation_id) = org_id.strip_prefix("sponsored:") {
        if !sponsored_billing_enabled() {
            return Ok(());
        }
        if !async_payment_succeeded && object["payment_status"].as_str() != Some("paid") {
            record_pending_founding_checkout(tx, operation_id, event_created).await?;
            return Ok(());
        }
        if async_payment_succeeded {
            record_async_payment_founding_checkout(tx, operation_id, object).await?;
        }
        let payment_reference = object["payment_intent"]
            .as_str()
            .or_else(|| object["id"].as_str())
            .ok_or_else(|| {
                Error::Config("paid sponsored checkout has no payment reference".into())
            })?;
        let subscription_id = object["subscription"]
            .as_str()
            .or_else(|| sponsored_snapshot.map(|snapshot| snapshot.subscription_id.as_str()))
            .ok_or_else(|| Error::Config("sponsored checkout has no subscription id".into()))?;
        let evidence = sponsored_billing::SponsoredProviderEvidence {
            customer_id: object["customer"]
                .as_str()
                .map(str::to_string)
                .or_else(|| sponsored_snapshot.and_then(|snapshot| snapshot.customer_id.clone())),
            subscription_id: subscription_id.to_string(),
            schedule_id: sponsored_snapshot.and_then(|snapshot| snapshot.schedule_id.clone()),
            checkout_session_id: object["id"].as_str().map(str::to_string),
            payment_reference: payment_reference.to_string(),
            provider_item_id: sponsored_snapshot.and_then(|snapshot| {
                (snapshot.items.len() == 1).then(|| snapshot.items[0].provider_item_id.clone())
            }),
        };
        let period_start = sponsored_snapshot.and_then(|snapshot| snapshot.current_period_start);
        if record_sponsored_founding_award(tx, operation_id, &evidence, period_start, event_created)
            .await?
        {
            enqueue_sponsored_checkout_outcome(
                tx,
                operation_id,
                crate::notifications::CheckoutOutcome::Completed,
            )
            .await?;
            crate::notifications::enqueue_sponsorship_change_in_tx(tx, operation_id)
                .await
                .map_err(notification_error)?;
            return Ok(());
        }
        sponsored_billing::complete_paid_checkout(tx, operation_id, &evidence)
            .await
            .map_err(Error::from)?;
        enqueue_sponsored_checkout_outcome(
            tx,
            operation_id,
            crate::notifications::CheckoutOutcome::Completed,
        )
        .await?;
        crate::notifications::enqueue_sponsorship_change_in_tx(tx, operation_id)
            .await
            .map_err(notification_error)?;
        return Ok(());
    }
    let customer = object["customer"].as_str();
    let subscription = object["subscription"].as_str();

    let changed = sqlx::query(
        "UPDATE organizations \
         SET tier = 'team', stripe_customer_id = $2, stripe_subscription_id = $3 \
         WHERE id = $1 AND lifecycle_state = 'active' AND (tier <> 'team' \
            OR stripe_customer_id IS DISTINCT FROM $2 \
            OR stripe_subscription_id IS DISTINCT FROM $3)",
    )
    .bind(org_id)
    .bind(customer)
    .bind(subscription)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    if changed == 0 {
        // The other silent ending, and the more misleading one: the event was ours, the handler
        // ran, and the `WHERE` matched nothing. An organisation that is already on the tier is a
        // legitimate repeat delivery; one that is missing or not active is a real problem, and
        // the two should not look the same in a log.
        eprintln!(
            "warning: checkout.session.completed for organisation {org_id} changed no rows; \
             it is already on the team tier, or it is absent or not active"
        );
    }
    if changed > 0 {
        audit::record_tx(
            &mut *tx,
            org_id,
            "stripe",
            "billing.subscribed",
            audit::Context {
                detail: Some("tier set to team"),
                ..Default::default()
            },
        )
        .await?;
    }
    Ok(())
}

fn object_is_transfer_setup(object: &serde_json::Value) -> bool {
    transfer_setup_transfer_id(object).is_some()
}

fn transfer_setup_transfer_id(object: &serde_json::Value) -> Option<&str> {
    let transfer_id = object["metadata"]["sotto_transfer_id"].as_str()?;
    let client_reference = object["client_reference_id"].as_str()?;
    (object["mode"].as_str() == Some("setup")
        && !transfer_id.trim().is_empty()
        && client_reference == format!("sotto-transfer:{transfer_id}"))
    .then_some(transfer_id)
}

async fn accept_transfer_setup(
    tx: &mut Transaction<'_, Postgres>,
    setup: &VerifiedTransferSetup,
) -> Result<()> {
    founding_allocator::lock_capacity(tx)
        .await
        .map_err(|error| Error::Internal(error.to_string()))?;
    billing_transfers::record_destination_setup_completion(
        tx,
        &setup.transfer_id,
        &setup.customer_id,
        &setup.session_id,
        &setup.setup_intent_id,
    )
    .await
    .map_err(Error::from)?;
    Ok(())
}

async fn personal_checkout_completed(
    tx: &mut Transaction<'_, Postgres>,
    object: &serde_json::Value,
    event_created: i64,
    operation_id: &str,
    async_payment_succeeded: bool,
    personal_period: Option<PersonalSubscriptionPeriod>,
) -> Result<()> {
    if !async_payment_succeeded && object["payment_status"].as_str() != Some("paid") {
        // A completed Checkout session is not itself authority. The account remains pending until
        // Stripe says the session is paid, so SCA failures and asynchronous payment methods never
        // become a free hosted term. Preserve a founding reservation only if Checkout completed
        // while that offer was still live; delayed payment success is handled separately below.
        record_pending_founding_checkout(tx, operation_id, event_created).await?;
        return Ok(());
    }
    if async_payment_succeeded {
        record_async_payment_founding_checkout(tx, operation_id, object).await?;
    }
    founding_allocator::lock_capacity(tx)
        .await
        .map_err(|error| Error::Internal(error.to_string()))?;
    let customer = object["customer"]
        .as_str()
        .ok_or_else(|| Error::Config("paid personal checkout has no customer".into()))?;
    let subscription = object["subscription"]
        .as_str()
        .ok_or_else(|| Error::Config("paid personal checkout has no subscription".into()))?;
    let payment_reference = object["payment_intent"]
        .as_str()
        .or_else(|| object["id"].as_str())
        .ok_or_else(|| Error::Config("paid personal checkout has no payment reference".into()))?;
    let account = sqlx::query(
        "SELECT user_id, offer FROM billing_personal_accounts WHERE operation_id = $1 FOR UPDATE",
    )
    .bind(operation_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| Error::Config("personal checkout operation is not registered".into()))?;
    let user_id: String = account.try_get("user_id")?;
    let offer = billing_offer(&account.try_get::<String, _>("offer")?)?;
    let period_start_epoch = personal_period
        .map(|period| period.start_epoch)
        .unwrap_or(event_created);
    let paid_on = founding_allocator::FoundingDate::from_unix_seconds(period_start_epoch)
        .map_err(|_| Error::Config("personal checkout timestamp is invalid".into()))?;
    let interval_offer = match offer {
        BillingOffer::StandardMonthly | BillingOffer::FoundingMonthly => FoundingOffer::Monthly,
        BillingOffer::StandardAnnual | BillingOffer::FoundingAnnual => FoundingOffer::Annual,
    };
    let paid_through = if let Some(period) = personal_period {
        founding_allocator::FoundingDate::from_unix_seconds(period.end_epoch)
            .map_err(|_| Error::Config("personal subscription period end is invalid".into()))?
    } else {
        paid_on.add_term(interval_offer)
    };
    if FoundingOffer::from_billing_offer(offer).is_some() {
        if personal_period.is_none() {
            return Err(Error::Config(
                "founding personal checkout has no verified subscription period".into(),
            ));
        }
        let reservation_id = format!("founding:{operation_id}");
        match founding_allocator::confirm_payment_with_period_start(
            tx,
            &reservation_id,
            payment_reference,
            paid_on,
            period_start_epoch,
            event_created,
        )
        .await
        .map_err(|error| Error::Internal(error.to_string()))?
        {
            founding_allocator::ConfirmationOutcome::Awarded(award) => {
                crate::founding_schedule::enqueue(tx, &award, subscription)
                    .await
                    .map_err(|_| {
                        Error::Internal("founding transition could not be queued".into())
                    })?;
            }
            // A replay can carry an obsolete subscription after a payer transfer. The durable
            // job was retargeted with the transfer itself; never retarget it from duplicate
            // provider evidence.
            founding_allocator::ConfirmationOutcome::AlreadyAwarded(_) => {}
            founding_allocator::ConfirmationOutcome::RefundRequired => {
                let refund = personal_billing::record_refund_required(
                    tx,
                    operation_id,
                    customer,
                    subscription,
                    payment_reference,
                )
                .await
                .map_err(personal_billing_error)?;
                if matches!(refund, personal_billing::SettlementDisposition::Applied) {
                    personal_billing::record_event(
                        tx,
                        &user_id,
                        operation_id,
                        "billing.personal_refund_required",
                        Some("founding capacity was consumed after payment"),
                    )
                    .await
                    .map_err(personal_billing_error)?;
                }
                crate::notifications::enqueue_checkout_outcome_in_tx(
                    tx,
                    &user_id,
                    operation_id,
                    crate::notifications::CheckoutOutcome::RefundRequired,
                )
                .await
                .map_err(notification_error)?;
                return Ok(());
            }
        }
    }
    let settlement = personal_billing::record_paid_settlement(
        tx,
        operation_id,
        customer,
        subscription,
        payment_reference,
        paid_through.to_unix_seconds(),
        &paid_through.to_string(),
    )
    .await
    .map_err(personal_billing_error)?;
    if matches!(settlement, personal_billing::SettlementDisposition::Applied) {
        personal_billing::record_event(
            tx,
            &user_id,
            operation_id,
            "billing.personal_paid",
            Some("personal checkout paid and coverage recorded"),
        )
        .await
        .map_err(personal_billing_error)?;
    }
    crate::notifications::enqueue_checkout_outcome_in_tx(
        tx,
        &user_id,
        operation_id,
        crate::notifications::CheckoutOutcome::Completed,
    )
    .await
    .map_err(notification_error)?;
    Ok(())
}

async fn record_pending_founding_checkout(
    tx: &mut Transaction<'_, Postgres>,
    operation_id: &str,
    event_created: i64,
) -> Result<()> {
    founding_allocator::record_pending_checkout_completion(
        tx,
        operation_id,
        event_created,
        billing_epoch(),
    )
    .await
    .map_err(|error| Error::Internal(error.to_string()))?;
    Ok(())
}

async fn record_async_payment_founding_checkout(
    tx: &mut Transaction<'_, Postgres>,
    operation_id: &str,
    object: &serde_json::Value,
) -> Result<()> {
    let Some(session_expires_at_epoch) = object["expires_at"].as_i64() else {
        // A missing deadline cannot extend an expired offer. Settlement will fail closed below.
        return Ok(());
    };
    founding_allocator::record_async_payment_checkout_completion(
        tx,
        operation_id,
        session_expires_at_epoch,
        object["status"].as_str() == Some("complete"),
        billing_epoch(),
    )
    .await
    .map_err(|error| Error::Internal(error.to_string()))?;
    Ok(())
}

/// A subscription lifecycle change: the status decides the tier. Handles late/failed payments
/// (`unpaid` → free) and recoveries (`active` again → team).
fn sponsored_subscription_status(status: SubscriptionStatus) -> Option<(&'static str, bool)> {
    match status {
        SubscriptionStatus::Active | SubscriptionStatus::Trialing => Some(("active", false)),
        SubscriptionStatus::PastDue | SubscriptionStatus::Paused => Some(("past_due", false)),
        SubscriptionStatus::Incomplete => Some(("pending", false)),
        SubscriptionStatus::Unpaid
        | SubscriptionStatus::Canceled
        | SubscriptionStatus::IncompleteExpired => Some(("canceled", true)),
        SubscriptionStatus::Unknown(_) => None,
    }
}

async fn sponsored_subscription_organization(
    tx: &mut Transaction<'_, Postgres>,
    object: &serde_json::Value,
) -> Result<Option<String>> {
    let Some(subscription_id) = object["id"].as_str() else {
        return Ok(None);
    };
    let metadata_organization = object["metadata"]["organization_id"].as_str();
    let stored_organization: Option<String> = sqlx::query_scalar(
        "SELECT organization_id FROM billing_sponsored_subscriptions \
         WHERE provider_subscription_id = $1",
    )
    .bind(subscription_id)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some(stored_organization) = stored_organization {
        return Ok(Some(stored_organization));
    }
    let Some(metadata_organization) = metadata_organization else {
        return Ok(None);
    };
    Ok(sqlx::query_scalar(
        "SELECT organization_id FROM billing_sponsored_subscriptions \
         WHERE organization_id = $1",
    )
    .bind(metadata_organization)
    .fetch_optional(&mut **tx)
    .await?)
}

#[allow(clippy::too_many_arguments)]
async fn apply_sponsored_subscription_state(
    tx: &mut Transaction<'_, Postgres>,
    organization_id: &str,
    subscription_id: &str,
    status: &str,
    terminal: bool,
    customer_id: Option<&str>,
    schedule_id: Option<&str>,
    period_start: Option<i64>,
    period_end: Option<i64>,
) -> Result<()> {
    let transfer_ids = live_transfer_ids_for_subscription(tx, subscription_id).await?;
    lock_sponsored_subscription_beneficiaries_except(tx, organization_id, &transfer_ids).await?;
    sqlx::query(
        "UPDATE billing_sponsored_subscriptions SET \
           provider_customer_id = COALESCE($2, provider_customer_id), \
           provider_subscription_id = CASE WHEN $3 THEN NULL ELSE $4 END, \
           provider_schedule_id = CASE WHEN $3 THEN NULL ELSE COALESCE($5, provider_schedule_id) END, \
           status = $6, current_period_start = COALESCE($7, current_period_start), \
           current_period_end = COALESCE($8, current_period_end), updated_at = now() \
         WHERE organization_id = $1",
    )
    .bind(organization_id)
    .bind(customer_id)
    .bind(terminal)
    .bind(subscription_id)
    .bind(schedule_id)
    .bind(status)
    .bind(period_start)
    .bind(period_end)
    .execute(&mut **tx)
    .await?;
    if terminal {
        let pending_operations: Vec<String> = sqlx::query_scalar(
            "SELECT operation_id FROM billing_sponsored_operations \
             WHERE organization_id = $1 AND provider_subscription_id = $2 \
               AND state IN ('pending', 'checkout_created', 'provider_pending', 'unknown') \
             ORDER BY created_at FOR UPDATE",
        )
        .bind(organization_id)
        .bind(subscription_id)
        .fetch_all(&mut **tx)
        .await?;
        for operation_id in pending_operations {
            sponsored_billing::cancel_failed_operation(tx, &operation_id, "subscription_ended")
                .await
                .map_err(Error::from)?;
        }
        sqlx::query(
            "UPDATE billing_sponsored_seats SET state = 'canceled', \
             effective_until = COALESCE(effective_until, $2, floor(extract(epoch FROM now()))::BIGINT), \
             updated_at = now() \
             WHERE organization_id = $1 AND state IN ('active', 'pending', 'scheduled_removal')",
        )
        .bind(organization_id)
        .bind(period_end)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

async fn live_transfer_ids_for_subscription(
    tx: &mut Transaction<'_, Postgres>,
    subscription_id: &str,
) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT transfer_id FROM billing_transfer_intents \
         WHERE (source_provider_subscription_id = $1 OR destination_provider_subscription_id = $1) \
           AND state IN ('pending','destination_prepared','destination_paid', \
                         'source_adjustment_pending','source_restoration_pending','unknown') \
         ORDER BY transfer_id COLLATE \"C\"",
    )
    .bind(subscription_id)
    .fetch_all(&mut **tx)
    .await?)
}

async fn record_terminal_destination_transfer(
    tx: &mut Transaction<'_, Postgres>,
    subscription_id: &str,
) -> Result<()> {
    founding_allocator::lock_capacity(tx)
        .await
        .map_err(|error| Error::Internal(error.to_string()))?;
    let row = sqlx::query(
        "SELECT transfer_id, state, source_adjustment_reference \
         FROM billing_transfer_intents WHERE destination_provider_subscription_id = $1 \
           AND destination_payment_reference IS NULL \
           AND state IN ('pending','destination_prepared','unknown','source_adjustment_pending') \
         ORDER BY transfer_id COLLATE \"C\" FOR UPDATE",
    )
    .bind(subscription_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row else {
        return Ok(());
    };
    let transfer_id: String = row.try_get("transfer_id")?;
    let state: String = row.try_get("state")?;
    let source_adjustment_reference: Option<String> = row.try_get("source_adjustment_reference")?;
    if state == TransferState::SourceAdjustmentPending.as_str()
        && source_adjustment_reference.is_some()
    {
        billing_transfers::request_source_restoration(
            tx,
            &transfer_id,
            "destination_subscription_ended",
        )
        .await
        .map_err(Error::from)?;
    } else {
        billing_transfers::mark_failed(tx, &transfer_id, "destination_subscription_ended")
            .await
            .map_err(Error::from)?;
    }
    Ok(())
}

async fn lock_sponsored_subscription_beneficiaries_except(
    tx: &mut Transaction<'_, Postgres>,
    organization_id: &str,
    allowed_transfer_ids: &[String],
) -> Result<()> {
    // Subscription lifecycle and invoice settlement can rewrite every seat on the subscription.
    // Take the shared lock before reading the affected set so a transfer cannot be accepted
    // between enumerating beneficiaries and applying the provider event.
    founding_allocator::lock_capacity(tx)
        .await
        .map_err(|error| Error::Internal(error.to_string()))?;
    let beneficiary_ids: Vec<String> = sqlx::query_scalar(
        "SELECT beneficiary_id FROM billing_sponsored_seats \
         WHERE organization_id = $1 AND state IN ('pending', 'active', 'scheduled_removal') \
         UNION \
         SELECT beneficiary_id FROM billing_sponsored_operations \
         WHERE organization_id = $1 AND state IN ('pending', 'checkout_created', 'provider_pending', 'unknown') \
         UNION \
         SELECT replacement_beneficiary_id FROM billing_sponsored_operations \
         WHERE organization_id = $1 AND replacement_beneficiary_id IS NOT NULL \
           AND state IN ('pending', 'checkout_created', 'provider_pending', 'unknown')",
    )
    .bind(organization_id)
    .fetch_all(&mut **tx)
    .await?;
    let beneficiary_refs = beneficiary_ids
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    billing_transfers::lock_and_check_no_provider_transfer_except(
        tx,
        &beneficiary_refs,
        allowed_transfer_ids,
        sponsored_billing::current_epoch(),
    )
    .await
    .map_err(Error::from)
}

async fn sponsored_subscription_updated(
    tx: &mut Transaction<'_, Postgres>,
    object: &serde_json::Value,
) -> Result<bool> {
    let Some(organization_id) = sponsored_subscription_organization(tx, object).await? else {
        return Ok(false);
    };
    let Some(subscription_id) = object["id"].as_str() else {
        return Ok(false);
    };
    let Some(status) = object["status"].as_str() else {
        return Ok(true);
    };
    let Some((mapped_status, terminal)) =
        sponsored_subscription_status(SubscriptionStatus::parse(status))
    else {
        return Ok(true);
    };
    if terminal {
        record_terminal_destination_transfer(tx, subscription_id).await?;
    }
    apply_sponsored_subscription_state(
        tx,
        &organization_id,
        subscription_id,
        mapped_status,
        terminal,
        object["customer"].as_str(),
        object["schedule"].as_str(),
        subscription_period_start(object),
        subscription_period_end(object),
    )
    .await?;
    audit::record_tx(
        &mut *tx,
        &organization_id,
        "stripe",
        "billing.sponsored_updated",
        audit::Context {
            detail: Some("sponsored subscription lifecycle state updated"),
            ..Default::default()
        },
    )
    .await?;
    Ok(true)
}

async fn subscription_updated(
    tx: &mut Transaction<'_, Postgres>,
    object: &serde_json::Value,
) -> Result<()> {
    let Some(status) = object["status"].as_str() else {
        eprintln!("warning: ignored Stripe subscription event without a status");
        return Ok(());
    };
    if let Some(user_id) = personal_user_for_subscription(tx, object).await? {
        let subscription_id = object["id"].as_str().ok_or_else(|| {
            Error::Config("personal subscription event has no subscription id".into())
        })?;
        let transfer_ids = live_transfer_ids_for_subscription(tx, subscription_id).await?;
        billing_transfers::lock_and_check_no_provider_transfer_except(
            tx,
            &[user_id.as_str()],
            &transfer_ids,
            sponsored_billing::current_epoch(),
        )
        .await
        .map_err(Error::from)?;
        let cancel_at_period_end = object["cancel_at_period_end"].as_bool().ok_or_else(|| {
            Error::Config("personal subscription event has no cancellation state".into())
        })?;
        let cancel_at_epoch = object["cancel_at"]
            .as_i64()
            .filter(|timestamp| *timestamp >= 0);
        let will_cancel = cancel_at_period_end || cancel_at_epoch.is_some();
        let was_cancel_at_period_end: bool = sqlx::query_scalar(
            "SELECT cancel_at_period_end FROM billing_personal_accounts \
             WHERE user_id = $1 FOR UPDATE",
        )
        .bind(&user_id)
        .fetch_one(&mut **tx)
        .await?;
        // A schedule-managed cancellation can be represented by `cancel_at` even when Stripe
        // leaves `cancel_at_period_end` false. Only an unscheduled transition from a recorded
        // cancellation back to no cancellation is treated as customer reactivation.
        let reactivated = was_cancel_at_period_end && !will_cancel && object["schedule"].is_null();
        let state = match SubscriptionStatus::parse(status) {
            SubscriptionStatus::Active | SubscriptionStatus::Trialing => "active",
            SubscriptionStatus::PastDue | SubscriptionStatus::Paused => "past_due",
            SubscriptionStatus::Unpaid => "unpaid",
            SubscriptionStatus::Canceled | SubscriptionStatus::IncompleteExpired => "canceled",
            SubscriptionStatus::Incomplete => "pending",
            SubscriptionStatus::Unknown(_) => return Ok(()),
        };
        if matches!(state, "canceled") {
            record_terminal_destination_transfer(tx, subscription_id).await?;
        }
        sqlx::query(
            "UPDATE billing_personal_accounts SET state = CASE \
                 WHEN state = 'pending' AND $2 <> 'canceled' THEN state ELSE $2 END, \
                 cancel_at_period_end = $3, stripe_subscription_id = COALESCE(stripe_subscription_id, $4), \
                 cancellation_requested_at = CASE WHEN $5 THEN NULL ELSE cancellation_requested_at END, \
                 updated_at = now() WHERE user_id = $1",
        )
        .bind(&user_id)
        .bind(state)
        .bind(will_cancel)
        .bind(subscription_id)
        .bind(reactivated)
        .execute(&mut **tx)
        .await?;
        if reactivated {
            founding_schedule::resume_personal_transition(tx, &user_id, subscription_id).await?;
        }
        return Ok(());
    }
    if sponsored_subscription_updated(tx, object).await? {
        return Ok(());
    }
    // A sponsored subscription can emit lifecycle events before its paid checkout creates the
    // durable row. It is not a legacy subscription, so never let this event change the Team tier.
    if object["metadata"]["organization_id"].as_str().is_some() {
        return Ok(());
    }
    let Some(org_id) = org_for_subscription(tx, object).await? else {
        return Ok(());
    };
    let parsed_status = SubscriptionStatus::parse(status);
    if matches!(&parsed_status, SubscriptionStatus::Unknown(_)) {
        eprintln!(
            "warning: ignored Stripe subscription event with unknown status {status} for organisation {org_id}"
        );
        return Ok(());
    }
    let tier = parsed_status.entitlement_tier();

    let changed = sqlx::query(
        "UPDATE organizations SET tier = $2 WHERE id = $1 AND lifecycle_state = 'active' \
         AND tier <> $2",
    )
    .bind(&org_id)
    .bind(tier)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    if changed > 0 {
        audit::record_tx(
            &mut *tx,
            &org_id,
            "stripe",
            "billing.updated",
            audit::Context {
                detail: Some(&format!("subscription {status}; tier set to {tier}")),
                ..Default::default()
            },
        )
        .await?;
    }
    Ok(())
}

/// Since Stripe API 2025-03-31.basil, subscription billing periods live on the subscription
/// items rather than on the subscription object. Personal checkout creates one item, but taking
/// the furthest item end keeps the stored term safe if that shape ever gains another item.
fn subscription_period_start(object: &serde_json::Value) -> Option<i64> {
    object["items"]["data"]
        .as_array()?
        .iter()
        .filter_map(|item| item["current_period_start"].as_i64())
        .filter(|start| *start >= 0)
        .min()
}

fn subscription_period_end(object: &serde_json::Value) -> Option<i64> {
    object["items"]["data"]
        .as_array()?
        .iter()
        .filter_map(|item| item["current_period_end"].as_i64())
        .filter(|end| *end > 0)
        .max()
}

fn personal_subscription_period_from_object(
    object: &serde_json::Value,
) -> ProviderResult<Option<PersonalSubscriptionPeriod>> {
    let Some(start_epoch) = subscription_period_start(object) else {
        return Ok(None);
    };
    let Some(end_epoch) = subscription_period_end(object) else {
        return Ok(None);
    };
    if end_epoch <= start_epoch {
        return Err(ProviderError::malformed_response());
    }
    Ok(Some(PersonalSubscriptionPeriod {
        start_epoch,
        end_epoch,
    }))
}

fn invoice_period_start(object: &serde_json::Value) -> Option<i64> {
    let lines = object["lines"]["data"].as_array()?;
    lines
        .iter()
        .filter(|line| {
            line["parent"]["type"].as_str() == Some("subscription_item_details")
                || line["type"].as_str() == Some("subscription")
        })
        .filter_map(|line| line["period"]["start"].as_i64())
        .filter(|start| *start >= 0)
        .min()
        .or_else(|| {
            lines
                .iter()
                .filter_map(|line| line["period"]["start"].as_i64())
                .filter(|start| *start >= 0)
                .min()
        })
}

/// Invoice.period_end describes the usage period that produced the invoice and therefore looks
/// backwards for subscription invoices. The subscription line's period.end is the service term
/// the paid invoice covers; the top-level field remains a compatibility fallback for old payloads.
fn invoice_period_end(object: &serde_json::Value) -> Option<i64> {
    let lines = object["lines"]["data"].as_array();
    let subscription_line_end = lines.and_then(|lines| {
        lines
            .iter()
            .filter(|line| {
                line["parent"]["type"].as_str() == Some("subscription_item_details")
                    || line["type"].as_str() == Some("subscription")
            })
            .filter_map(|line| line["period"]["end"].as_i64())
            .filter(|end| *end > 0)
            .max()
            .or_else(|| {
                lines
                    .iter()
                    .filter_map(|line| line["period"]["end"].as_i64())
                    .filter(|end| *end > 0)
                    .max()
            })
    });
    subscription_line_end.or_else(|| object["period_end"].as_i64().filter(|end| *end > 0))
}

fn paid_invoice_repair_shape_error(
    object: &serde_json::Value,
    prepared_period_end: Option<i64>,
    settlement_expected: bool,
) -> Option<&'static str> {
    if !settlement_expected {
        return None;
    }
    let period_end = prepared_period_end.or_else(|| invoice_period_end(object));
    if period_end
        .and_then(|period_end| FoundingDate::from_unix_seconds(period_end).ok())
        .is_none()
    {
        return Some("paid_invoice_settlement_shape");
    }
    if prepared_period_end.is_none()
        && object["payment_intent"].as_str().is_none()
        && object["id"].as_str().is_none()
    {
        return Some("paid_invoice_settlement_shape");
    }
    None
}

fn invoice_subscription_id(object: &serde_json::Value) -> Option<&str> {
    object["parent"]["subscription_details"]["subscription"].as_str()
}

fn preview_subscription_period_end_from_invoice(
    invoice: &serde_json::Value,
    subscription_id: &str,
    price_id: &str,
    period_start: i64,
) -> ProviderResult<i64> {
    if invoice["object"].as_str() != Some("invoice")
        || invoice_subscription_id(invoice) != Some(subscription_id)
    {
        return Err(ProviderError::malformed_response());
    }
    let matching_ends: Vec<i64> = invoice["lines"]["data"]
        .as_array()
        .ok_or_else(ProviderError::malformed_response)?
        .iter()
        .filter(|line| {
            line["parent"]["type"].as_str() == Some("subscription_item_details")
                && line["parent"]["subscription_item_details"]["proration"].as_bool() != Some(true)
        })
        .filter(|line| {
            line["price"]["id"].as_str() == Some(price_id)
                || line["pricing"]["price_details"]["price"].as_str() == Some(price_id)
                || line["parent"]["subscription_item_details"]["price"].as_str() == Some(price_id)
        })
        .filter(|line| line["period"]["start"].as_i64() == Some(period_start))
        .filter_map(|line| line["period"]["end"].as_i64())
        .filter(|period_end| *period_end > period_start)
        .collect();
    let [period_end] = matching_ends.as_slice() else {
        return Err(ProviderError::malformed_response());
    };
    Ok(*period_end)
}

async fn invoice_paid(
    tx: &mut Transaction<'_, Postgres>,
    object: &serde_json::Value,
    event_created: i64,
    catalogue: Option<&BillingPriceIds>,
    verified_personal_invoice: Option<&PreparedPersonalInvoiceCoverage>,
    verified_sponsored_invoice: Option<&SponsoredInvoiceCoveragePreparation>,
) -> Result<Option<&'static str>> {
    let Some(subscription_id) = invoice_subscription_id(object) else {
        return Ok(None);
    };
    if sponsored_billing_enabled() {
        let sponsored_organization =
            sponsored_invoice_paid(tx, object, subscription_id, event_created, catalogue).await?;
        if let Some(organization_id) = sponsored_organization {
            let mut coverage_rejection = None;
            match verified_sponsored_invoice {
                Some(SponsoredInvoiceCoveragePreparation::Prepared(prepared)) => {
                    if !accept_sponsored_invoice_coverage(tx, prepared).await? {
                        coverage_rejection = Some("sponsored_invoice_no_eligible_seats");
                    }
                    resolve_sponsored_renewal_failures(
                        tx,
                        &prepared.organization_id,
                        subscription_id,
                        &prepared.invoice_id,
                        event_created,
                    )
                    .await?;
                }
                Some(SponsoredInvoiceCoveragePreparation::Rejected {
                    context,
                    event,
                    reason,
                }) => {
                    reject_sponsored_invoice_coverage(tx, context, event, reason).await?;
                    coverage_rejection = Some(*reason);
                }
                Some(SponsoredInvoiceCoveragePreparation::NotApplicable) | None => {}
            }
            if let Some(catalogue) = catalogue {
                reconcile_sponsored_founding_invoice(
                    tx,
                    &organization_id,
                    subscription_id,
                    object,
                    &sponsored_invoice_items(object),
                    catalogue,
                )
                .await?;
            }
            return Ok(coverage_rejection);
        }
    }
    let personal_account: Option<(String, String)> = sqlx::query_as(
        "SELECT user_id, operation_id FROM billing_personal_accounts \
         WHERE stripe_subscription_id = $1",
    )
    .bind(subscription_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((personal_user_id, operation_id)) = personal_account else {
        return Ok(None);
    };
    let invoice_period_start = verified_personal_invoice
        .map(|evidence| evidence.period_start)
        .or_else(|| invoice_period_start(object));
    let allowed_transfer_ids: Vec<String> = if let Some(period_start) = invoice_period_start {
        sqlx::query_scalar(
            "SELECT transfer_id FROM billing_transfer_intents \
             WHERE ((destination_operation_id = $1 AND destination_kind = 'personal' \
               AND destination_provider_subscription_id = $2 AND beneficiary_id = $3 \
               AND effective_from = $4) OR \
               (source_restoration_operation_id = $1 AND source_kind = 'personal' \
               AND source_restoration_provider_subscription_id = $2 AND beneficiary_id = $3 \
               AND effective_from <= $4)) AND state IN \
                 ('pending','destination_prepared','destination_paid','source_adjustment_pending', \
                  'source_restoration_pending','unknown')",
        )
        .bind(&operation_id)
        .bind(subscription_id)
        .bind(&personal_user_id)
        .bind(period_start)
        .fetch_all(&mut **tx)
        .await?
    } else {
        Vec::new()
    };
    billing_transfers::lock_and_check_no_provider_transfer_except(
        tx,
        &[personal_user_id.as_str()],
        &allowed_transfer_ids,
        sponsored_billing::current_epoch(),
    )
    .await
    .map_err(Error::from)?;
    let period_end = verified_personal_invoice
        .map(|evidence| evidence.period_end)
        .or_else(|| invoice_period_end(object))
        .ok_or_else(|| Error::Config("paid personal invoice has no period end".into()))?;
    let paid_through = founding_allocator::FoundingDate::from_unix_seconds(period_end)
        .map_err(|_| Error::Config("paid personal invoice has invalid period end".into()))?;
    let payment_reference = verified_personal_invoice
        .map(|evidence| evidence.payment_reference.as_str())
        .or_else(|| {
            object["payment_intent"]
                .as_str()
                .or_else(|| object["id"].as_str())
        })
        .ok_or_else(|| Error::Config("paid personal invoice has no payment reference".into()))?;
    let account_state: Option<String> = sqlx::query_scalar(
        "SELECT state FROM billing_personal_accounts WHERE user_id = $1 AND operation_id = $2 FOR UPDATE",
    )
    .bind(&personal_user_id)
    .bind(&operation_id)
    .fetch_optional(&mut **tx)
    .await?;
    if account_state.as_deref() == Some("pending") {
        let evidence = verified_personal_invoice.ok_or_else(|| {
            Error::Internal("pending personal invoice lacks verified payment evidence".into())
        })?;
        if evidence.operation_id != operation_id
            || evidence.allocation.beneficiary_id != personal_user_id
            || stripe_reference(&object["customer"]).as_deref()
                != Some(evidence.allocation.provider_customer_id.as_str())
        {
            return Err(Error::Conflict(
                "pending personal invoice binding changed".into(),
            ));
        }
        personal_billing::record_paid_settlement(
            tx,
            &operation_id,
            &evidence.allocation.provider_customer_id,
            subscription_id,
            payment_reference,
            period_end,
            &paid_through.to_string(),
        )
        .await
        .map_err(personal_billing_error)?;
        billing_operations::record_verified_paid_invoice(tx, &operation_id)
            .await
            .map_err(Error::from)?;
    } else {
        personal_billing::record_invoice_paid(
            tx,
            subscription_id,
            payment_reference,
            period_end,
            &paid_through.to_string(),
        )
        .await
        .map_err(personal_billing_error)?;
    }
    reconcile_personal_founding_invoice(tx, object, subscription_id, catalogue).await?;
    enqueue_personal_renewal_recovered_notice(tx, object, subscription_id, event_created).await?;
    Ok(None)
}

async fn reconcile_personal_founding_invoice(
    tx: &mut Transaction<'_, Postgres>,
    object: &serde_json::Value,
    subscription_id: &str,
    catalogue: Option<&BillingPriceIds>,
) -> Result<()> {
    let Some(row) = sqlx::query(
        "SELECT account.user_id, award.award_id, award.offer AS founding_offer, \
                award.original_end_epoch \
         FROM billing_personal_accounts AS account \
         JOIN billing_founding_awards AS award ON award.beneficiary_id = account.user_id \
         WHERE account.stripe_subscription_id = $1 \
           AND award.payer_kind = 'personal' AND award.payer_id = account.user_id",
    )
    .bind(subscription_id)
    .fetch_optional(&mut **tx)
    .await?
    else {
        return Ok(());
    };
    let award_id: String = row.try_get("award_id")?;
    let failure = if let Some(period_start) = invoice_period_start(object) {
        if let Some(end_epoch) = row.try_get::<Option<i64>, _>("original_end_epoch")? {
            let founding_offer =
                parse_founding_offer(&row.try_get::<String, _>("founding_offer")?)?;
            if let Some(catalogue) = catalogue {
                let expected = if period_start < founding_price_boundary(end_epoch)? {
                    founding_offer.billing_offer()
                } else {
                    founding_offer.standard_billing_offer()
                };
                let line_items = sponsored_invoice_items(object);
                if line_items.len() == 1
                    && line_items[0].1 == catalogue.id_for(expected)
                    && line_items[0].2 == 1
                {
                    let user_id: String = row.try_get("user_id")?;
                    sqlx::query(
                        "UPDATE billing_personal_accounts SET offer = $3, updated_at = now() \
                         WHERE user_id = $1 AND stripe_subscription_id = $2",
                    )
                    .bind(user_id)
                    .bind(subscription_id)
                    .bind(expected.as_str())
                    .execute(&mut **tx)
                    .await?;
                    return Ok(());
                }
                Some("invoice_price_mismatch")
            } else {
                Some("invoice_price_mismatch")
            }
        } else {
            Some("invoice_boundary_missing")
        }
    } else {
        Some("invoice_boundary_missing")
    };
    if let Some(failure) = failure {
        founding_schedule::mark_invoice_unresolved(tx, &award_id, failure)
            .await
            .map_err(|_| {
                Error::Internal("founding transition could not be marked unresolved".into())
            })?;
    }
    Ok(())
}

async fn sponsored_invoice_paid(
    tx: &mut Transaction<'_, Postgres>,
    object: &serde_json::Value,
    subscription_id: &str,
    event_created: i64,
    catalogue: Option<&BillingPriceIds>,
) -> Result<Option<String>> {
    let Some(catalogue) = catalogue else {
        return Err(Error::NotConfigured(
            "sponsored billing price catalogue is not configured".into(),
        ));
    };
    let known_subscription: Option<String> = sqlx::query_scalar(
        "SELECT organization_id FROM billing_sponsored_subscriptions \
         WHERE provider_subscription_id = $1",
    )
    .bind(subscription_id)
    .fetch_optional(&mut **tx)
    .await?;
    if known_subscription.is_none() {
        return Ok(None);
    }
    founding_allocator::lock_capacity(tx)
        .await
        .map_err(|error| Error::Internal(error.to_string()))?;
    let organization_id: Option<String> = sqlx::query_scalar(
        "SELECT organization_id FROM billing_sponsored_subscriptions \
         WHERE provider_subscription_id = $1 FOR UPDATE",
    )
    .bind(subscription_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(organization_id) = organization_id else {
        return Ok(None);
    };
    let period_start = invoice_period_start(object);
    let period_end = invoice_period_end(object)
        .ok_or_else(|| Error::Config("paid sponsored invoice has no period end".into()))?;
    let allowed_transfer_ids: Vec<String> = if let Some(period_start) = period_start {
        sqlx::query_scalar(
            "SELECT transfer.transfer_id FROM billing_transfer_intents AS transfer \
             JOIN billing_sponsored_operations AS operation \
               ON operation.operation_id = transfer.destination_operation_id \
             WHERE transfer.effective_from = $3 AND transfer.state IN \
                 ('pending','destination_prepared','destination_paid','source_adjustment_pending', \
                  'source_restoration_pending','unknown') \
               AND ((transfer.destination_kind = 'sponsor' \
                     AND transfer.destination_organization_id = $1 \
                     AND transfer.destination_provider_subscription_id = $2 \
                     AND operation.organization_id = $1 \
                     AND operation.provider_subscription_id = $2) \
                 OR (transfer.source_kind = 'sponsor' \
                     AND transfer.source_organization_id = $1 \
                     AND transfer.source_provider_subscription_id = $2 \
                     AND transfer.source_schedule_id IS NOT NULL \
                     AND operation.operation_id = transfer.transfer_id || ':destination:source' \
                     AND operation.organization_id = $1 \
                     AND operation.provider_subscription_id = $2)) \
               AND operation.state IN ('provider_pending','active') \
             ORDER BY transfer.transfer_id COLLATE \"C\"",
        )
        .bind(&organization_id)
        .bind(subscription_id)
        .bind(period_start)
        .fetch_all(&mut **tx)
        .await?
    } else {
        Vec::new()
    };
    lock_sponsored_subscription_beneficiaries_except(tx, &organization_id, &allowed_transfer_ids)
        .await?;
    let payment_reference = object["payment_intent"]
        .as_str()
        .or_else(|| object["id"].as_str())
        .ok_or_else(|| Error::Config("paid sponsored invoice has no payment reference".into()))?;
    let paid_at = object["status_transitions"]["paid_at"]
        .as_i64()
        .or_else(|| object["created"].as_i64())
        .unwrap_or(event_created);
    let customer_id = object["customer"].as_str().map(str::to_string);
    sqlx::query(
        "INSERT INTO billing_sponsored_subscriptions \
         (organization_id, provider_customer_id, provider_subscription_id, status, updated_at) \
         VALUES ($1, $2, $3, 'active', now()) \
         ON CONFLICT (organization_id) DO UPDATE SET provider_customer_id = COALESCE(EXCLUDED.provider_customer_id, billing_sponsored_subscriptions.provider_customer_id), \
           provider_subscription_id = EXCLUDED.provider_subscription_id, status = 'active', updated_at = now()",
    )
    .bind(&organization_id)
    .bind(customer_id.as_deref())
    .bind(subscription_id)
    .execute(&mut **tx)
    .await?;
    let line_items = sponsored_invoice_items(object);
    for (item_id, price_id, quantity) in &line_items {
        if let Some(offer) = BillingOffer::ALL
            .into_iter()
            .find(|offer| catalogue.id_for(*offer) == price_id)
        {
            sponsored_billing::record_provider_item(
                tx,
                &organization_id,
                item_id,
                price_id,
                offer,
                *quantity,
            )
            .await
            .map_err(Error::from)?;
        }
    }
    let line_terms = sponsored_invoice_line_terms(object);
    let operations: Vec<(String, String, i64, Option<String>, String)> = sqlx::query_as(
        "SELECT operation_id, offer, effective_from, provider_item_id, action \
         FROM billing_sponsored_operations \
         WHERE organization_id = $1 \
           AND state = 'provider_pending' AND provider_subscription_id = $2 \
           AND effective_from < $3 \
         ORDER BY created_at FOR UPDATE",
    )
    .bind(&organization_id)
    .bind(subscription_id)
    .bind(period_end)
    .fetch_all(&mut **tx)
    .await?;
    for (operation_id, offer, effective_from, recorded_item_id, action) in operations {
        let offer = billing_offer(&offer)?;
        // A shared subscription can have the same price on multiple lines. Only settle this
        // operation when one invoice line identifies both its item and the term that began at
        // (or after) the operation's effective boundary. Invoice-wide max period_end is not
        // enough: a long annual line must not settle a monthly operation early.
        let Some(provider_item_id) = sponsored_invoice_line_item_for_operation(
            &line_terms,
            catalogue.id_for(offer),
            effective_from,
            recorded_item_id.as_deref(),
            action != "remove",
        ) else {
            continue;
        };
        let provider_item_id = Some(provider_item_id);
        let evidence = sponsored_billing::SponsoredProviderEvidence {
            customer_id: customer_id.clone(),
            subscription_id: subscription_id.to_string(),
            schedule_id: None,
            checkout_session_id: None,
            payment_reference: payment_reference.to_string(),
            provider_item_id,
        };
        if record_sponsored_founding_award(tx, &operation_id, &evidence, period_start, paid_at)
            .await?
        {
            enqueue_sponsored_checkout_outcome(
                tx,
                &operation_id,
                crate::notifications::CheckoutOutcome::Completed,
            )
            .await?;
            crate::notifications::enqueue_sponsorship_change_in_tx(tx, &operation_id)
                .await
                .map_err(notification_error)?;
            continue;
        }
        sponsored_billing::complete_paid_checkout(tx, &operation_id, &evidence)
            .await
            .map_err(Error::from)?;
        enqueue_sponsored_checkout_outcome(
            tx,
            &operation_id,
            crate::notifications::CheckoutOutcome::Completed,
        )
        .await?;
        crate::notifications::enqueue_sponsorship_change_in_tx(tx, &operation_id)
            .await
            .map_err(notification_error)?;
    }
    Ok(Some(organization_id))
}

async fn reconcile_sponsored_founding_invoice(
    tx: &mut Transaction<'_, Postgres>,
    organization_id: &str,
    subscription_id: &str,
    invoice: &serde_json::Value,
    line_items: &[(String, String, i64)],
    catalogue: &BillingPriceIds,
) -> Result<()> {
    let Some(period_start) = invoice_period_start(invoice) else {
        return mark_sponsored_founders_unresolved(
            tx,
            organization_id,
            subscription_id,
            "invoice_boundary_missing",
        )
        .await;
    };
    let Some(period_end) = invoice_period_end(invoice) else {
        return mark_sponsored_founders_unresolved(
            tx,
            organization_id,
            subscription_id,
            "invoice_boundary_missing",
        )
        .await;
    };
    type SponsoredInvoiceSeatRow = (
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        Option<i64>,
    );
    let rows: Vec<SponsoredInvoiceSeatRow> = sqlx::query_as(
            "SELECT seat.seat_id, seat.beneficiary_id, seat.offer, award.award_id, \
                    award.offer, award.original_end_epoch \
             FROM billing_sponsored_seats AS seat \
             JOIN billing_sponsored_operations AS operation ON operation.operation_id = seat.operation_id \
             LEFT JOIN billing_founding_awards AS award ON award.beneficiary_id = seat.beneficiary_id \
               AND award.payer_kind = 'sponsor' AND award.payer_id = seat.organization_id \
             WHERE seat.organization_id = $1 AND operation.provider_subscription_id = $2 \
               AND operation.state IN ('active','provider_pending') \
               AND seat.state IN ('pending','active','scheduled_removal','replaced','canceled') \
               AND seat.effective_from < $4 \
               AND (seat.effective_until IS NULL OR seat.effective_until > $3)",
        )
        .bind(organization_id)
        .bind(subscription_id)
        .bind(period_start)
        .bind(period_end)
        .fetch_all(&mut **tx)
        .await?;

    let mut expected_quantities = std::collections::BTreeMap::<String, i64>::new();
    let mut updates = Vec::new();
    let mut boundary_errors = std::collections::BTreeSet::new();
    let mut founder_awards = std::collections::BTreeSet::new();
    for (seat_id, _, stored_offer, award_id, award_offer, end_epoch) in &rows {
        let stored_offer = billing_offer(stored_offer)?;
        let expected_offer = match (award_id, award_offer, end_epoch) {
            (Some(award_id), Some(offer), Some(end_epoch)) => {
                founder_awards.insert(award_id.clone());
                let offer = parse_founding_offer(offer)?;
                Some(expected_founding_invoice_offer(
                    stored_offer,
                    Some(offer),
                    Some(founding_price_boundary(*end_epoch)?),
                    period_start,
                ))
            }
            (Some(award_id), _, _) => {
                founder_awards.insert(award_id.clone());
                boundary_errors.insert(award_id.clone());
                None
            }
            (None, _, _) => Some(Some(stored_offer)),
        };
        let Some(expected_offer) = expected_offer.flatten() else {
            continue;
        };
        let quantity = expected_quantities
            .entry(catalogue.id_for(expected_offer).to_string())
            .or_default();
        *quantity = quantity
            .checked_add(1)
            .ok_or_else(|| Error::BadRequest("sponsored invoice seat count is too large".into()))?;
        if award_id.is_some() {
            updates.push((seat_id.clone(), expected_offer));
        }
    }
    if !founder_awards.is_empty() {
        let mut actual_quantities = std::collections::BTreeMap::<String, i64>::new();
        for (_, price_id, quantity) in line_items {
            let quantity_sum = actual_quantities.entry(price_id.clone()).or_default();
            *quantity_sum = quantity_sum.checked_add(*quantity).ok_or_else(|| {
                Error::BadRequest("sponsored invoice quantity is too large".into())
            })?;
        }
        if boundary_errors.is_empty() && actual_quantities == expected_quantities {
            for (seat_id, offer) in updates {
                sqlx::query("UPDATE billing_sponsored_seats SET offer = $2 WHERE seat_id = $1")
                    .bind(seat_id)
                    .bind(offer.as_str())
                    .execute(&mut **tx)
                    .await?;
            }
        } else {
            for award_id in founder_awards {
                let code = if boundary_errors.contains(&award_id) {
                    "invoice_boundary_missing"
                } else {
                    "invoice_price_mismatch"
                };
                founding_schedule::mark_invoice_unresolved(tx, &award_id, code)
                    .await
                    .map_err(|_| {
                        Error::Internal("founding transition could not be marked unresolved".into())
                    })?;
            }
        }
    }
    Ok(())
}

async fn mark_sponsored_founders_unresolved(
    tx: &mut Transaction<'_, Postgres>,
    organization_id: &str,
    subscription_id: &str,
    error_code: &'static str,
) -> Result<()> {
    let awards: Vec<String> = sqlx::query_scalar(
        "SELECT award.award_id FROM billing_founding_awards AS award \
         JOIN billing_sponsored_seats AS seat ON seat.beneficiary_id = award.beneficiary_id \
         JOIN billing_sponsored_operations AS operation ON operation.operation_id = seat.operation_id \
         WHERE seat.organization_id = $1 AND operation.provider_subscription_id = $2 \
           AND operation.state IN ('active','provider_pending')",
    )
    .bind(organization_id)
    .bind(subscription_id)
    .fetch_all(&mut **tx)
    .await?;
    for award_id in awards {
        founding_schedule::mark_invoice_unresolved(tx, &award_id, error_code)
            .await
            .map_err(|_| {
                Error::Internal("founding transition could not be marked unresolved".into())
            })?;
    }
    Ok(())
}

/// Confirm a sponsor's founding reservation before activating its named seat. Returns true when
/// payment must remain refund-required instead of becoming coverage.
async fn record_sponsored_founding_award(
    tx: &mut Transaction<'_, Postgres>,
    operation_id: &str,
    evidence: &sponsored_billing::SponsoredProviderEvidence,
    period_start_epoch: Option<i64>,
    paid_at_epoch: i64,
) -> Result<bool> {
    founding_allocator::lock_capacity(tx)
        .await
        .map_err(|error| Error::Internal(error.to_string()))?;
    let offer: String = sqlx::query_scalar(
        "SELECT offer FROM billing_sponsored_operations WHERE operation_id = $1 FOR UPDATE",
    )
    .bind(operation_id)
    .fetch_one(&mut **tx)
    .await?;
    let offer = billing_offer(&offer)?;
    if FoundingOffer::from_billing_offer(offer).is_none() {
        return Ok(false);
    }
    let period_start_epoch = period_start_epoch.ok_or_else(|| {
        Error::Config("sponsored founding payment has no verified period start".into())
    })?;
    let paid_on = founding_allocator::FoundingDate::from_unix_seconds(period_start_epoch)
        .map_err(|_| Error::Config("sponsored founding payment timestamp is invalid".into()))?;
    // One organisation invoice can pay for several named founding seats. Keep the allocator's
    // globally unique payment claim specific to this operation while retaining the unmodified
    // provider reference in the sponsored operation for refund handling.
    let allocation_payment_reference = format!("{}:{operation_id}", evidence.payment_reference);
    match founding_allocator::confirm_payment_with_period_start(
        tx,
        &format!("founding:{operation_id}"),
        &allocation_payment_reference,
        paid_on,
        period_start_epoch,
        paid_at_epoch,
    )
    .await
    .map_err(|error| Error::Internal(error.to_string()))?
    {
        founding_allocator::ConfirmationOutcome::Awarded(award) => {
            crate::founding_schedule::enqueue(tx, &award, &evidence.subscription_id)
                .await
                .map_err(|_| Error::Internal("founding transition could not be queued".into()))?;
        }
        founding_allocator::ConfirmationOutcome::AlreadyAwarded(_) => {}
        founding_allocator::ConfirmationOutcome::RefundRequired => {
            sponsored_billing::record_founding_refund_required(tx, operation_id, evidence)
                .await
                .map_err(Error::from)?;
            return Ok(true);
        }
    }
    Ok(false)
}

fn sponsored_invoice_items(object: &serde_json::Value) -> Vec<(String, String, i64)> {
    object["lines"]["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|line| {
            let item_id = line["subscription_item"].as_str().or_else(|| {
                line["parent"]["subscription_item_details"]["subscription_item"].as_str()
            })?;
            let price_id = line["price"]["id"]
                .as_str()
                .or_else(|| line["pricing"]["price_details"]["price"].as_str())
                .or_else(|| line["parent"]["subscription_item_details"]["price"].as_str())?;
            let quantity = line["quantity"].as_i64().unwrap_or(1);
            Some((item_id.to_string(), price_id.to_string(), quantity))
        })
        .collect()
}

fn sponsored_invoice_line_terms(
    object: &serde_json::Value,
) -> Vec<(String, String, i64, i64, i64, bool)> {
    object["lines"]["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|line| {
            let item_id = line["subscription_item"].as_str().or_else(|| {
                line["parent"]["subscription_item_details"]["subscription_item"].as_str()
            })?;
            let price_id = line["price"]["id"]
                .as_str()
                .or_else(|| line["pricing"]["price_details"]["price"].as_str())
                .or_else(|| line["parent"]["subscription_item_details"]["price"].as_str())?;
            let period_start = line["period"]["start"].as_i64()?;
            let period_end = line["period"]["end"].as_i64()?;
            let amount = line["amount"].as_i64()?;
            let proration = line["parent"]["subscription_item_details"]["proration"]
                .as_bool()
                .or_else(|| line["proration"].as_bool())?;
            (period_start >= 0 && period_end > period_start).then(|| {
                (
                    item_id.to_string(),
                    price_id.to_string(),
                    period_start,
                    period_end,
                    amount,
                    proration,
                )
            })
        })
        .collect()
}

fn sponsored_invoice_line_item_for_operation(
    line_terms: &[(String, String, i64, i64, i64, bool)],
    expected_price_id: &str,
    effective_from: i64,
    recorded_item_id: Option<&str>,
    require_positive_amount: bool,
) -> Option<String> {
    let mut matches = line_terms.iter().filter(
        |(item_id, price_id, period_start, period_end, amount, proration)| {
            price_id == expected_price_id
                && effective_from < *period_end
                && (!require_positive_amount || *amount > 0)
                && (effective_from <= *period_start
                    || (*proration && *period_start <= effective_from))
                && recorded_item_id.is_none_or(|recorded_item_id| recorded_item_id == item_id)
        },
    );
    let (item_id, _, _, _, _, _) = matches.next()?;
    matches.next().is_none().then(|| item_id.clone())
}

/// The subscription ended for good: back to the free tier (existing data stays readable - the
/// entitlement gates are creation-time only).
async fn subscription_deleted(
    tx: &mut Transaction<'_, Postgres>,
    object: &serde_json::Value,
) -> Result<()> {
    if let Some(user_id) = personal_user_for_subscription(tx, object).await? {
        let subscription_id = object["id"].as_str().ok_or_else(|| {
            Error::Config("personal subscription event has no subscription id".into())
        })?;
        let transfer_ids = live_transfer_ids_for_subscription(tx, subscription_id).await?;
        billing_transfers::lock_and_check_no_provider_transfer_except(
            tx,
            &[user_id.as_str()],
            &transfer_ids,
            sponsored_billing::current_epoch(),
        )
        .await
        .map_err(Error::from)?;
        sqlx::query(
            "UPDATE billing_personal_accounts SET state = 'canceled', \
             stripe_subscription_id = COALESCE(stripe_subscription_id, $2), updated_at = now() \
             WHERE user_id = $1",
        )
        .bind(user_id)
        .bind(subscription_id)
        .execute(&mut **tx)
        .await?;
        record_terminal_destination_transfer(tx, subscription_id).await?;
        return Ok(());
    }
    if let Some(organization_id) = sponsored_subscription_organization(tx, object).await? {
        let subscription_id = object["id"].as_str().ok_or_else(|| {
            Error::Config("sponsored subscription event has no subscription id".into())
        })?;
        record_terminal_destination_transfer(tx, subscription_id).await?;
        apply_sponsored_subscription_state(
            tx,
            &organization_id,
            subscription_id,
            "canceled",
            true,
            object["customer"].as_str(),
            None,
            subscription_period_start(object),
            subscription_period_end(object),
        )
        .await?;
        audit::record_tx(
            &mut *tx,
            &organization_id,
            "stripe",
            "billing.sponsored_cancelled",
            audit::Context {
                detail: Some("sponsored subscription ended"),
                ..Default::default()
            },
        )
        .await?;
        return Ok(());
    }
    if object["metadata"]["organization_id"].as_str().is_some() {
        return Ok(());
    }
    let Some(org_id) = org_for_subscription(tx, object).await? else {
        return Ok(());
    };
    let changed = sqlx::query(
        "UPDATE organizations SET tier = 'free', stripe_subscription_id = NULL \
         WHERE id = $1 AND lifecycle_state = 'active' \
           AND (tier <> 'free' OR stripe_subscription_id IS NOT NULL)",
    )
    .bind(&org_id)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    if changed > 0 {
        audit::record_tx(
            &mut *tx,
            &org_id,
            "stripe",
            "billing.cancelled",
            audit::Context {
                detail: Some("tier set to free"),
                ..Default::default()
            },
        )
        .await?;
    }
    Ok(())
}

/// Equal-timestamp events cannot be ordered by delivery time. Reconcile against Stripe's current
/// object instead of letting arrival order decide the entitlement.
async fn reconcile_subscription(
    tx: &mut Transaction<'_, Postgres>,
    observation: SubscriptionObservation,
    subscription_id: &str,
    org_hint: Option<&str>,
    sponsored_hint: bool,
) -> Result<()> {
    if sponsored_hint {
        let Some(organization_id) = org_hint else {
            return Ok(());
        };
        let (status, terminal) = match observation {
            SubscriptionObservation::Current(snapshot) => {
                let Some(mapped) = sponsored_subscription_status(snapshot.status) else {
                    return Ok(());
                };
                mapped
            }
            SubscriptionObservation::Missing => ("canceled", true),
        };
        apply_sponsored_subscription_state(
            tx,
            organization_id,
            subscription_id,
            status,
            terminal,
            None,
            None,
            None,
            None,
        )
        .await?;
        return Ok(());
    }
    let org_id = match org_hint {
        Some(org_id) => Some(org_id.to_string()),
        None => {
            sqlx::query_scalar("SELECT id FROM organizations WHERE stripe_subscription_id = $1")
                .bind(subscription_id)
                .fetch_optional(&mut **tx)
                .await?
        }
    };
    let Some(org_id) = org_id else {
        return Ok(());
    };
    let (tier, linked_subscription) = match observation {
        SubscriptionObservation::Current(snapshot) => {
            (snapshot.status.entitlement_tier(), Some(snapshot.id))
        }
        SubscriptionObservation::Missing => ("free", None),
    };
    let changed = sqlx::query(
        "UPDATE organizations SET tier = $2, stripe_subscription_id = $3 \
         WHERE id = $1 AND lifecycle_state = 'active' \
           AND (tier <> $2 OR stripe_subscription_id IS DISTINCT FROM $3)",
    )
    .bind(&org_id)
    .bind(tier)
    .bind(linked_subscription.as_deref())
    .execute(&mut **tx)
    .await?
    .rows_affected();
    if changed > 0 {
        audit::record_tx(
            &mut *tx,
            &org_id,
            "stripe",
            "billing.reconciled",
            audit::Context {
                detail: Some("equal-timestamp webhook reconciled with provider"),
                ..Default::default()
            },
        )
        .await?;
    }
    Ok(())
}

/// Name the org for a subscription event: the metadata stamped at checkout, else the stored
/// subscription id (covers subscriptions relinked by Stripe support), else not ours.
async fn org_for_subscription(
    tx: &mut Transaction<'_, Postgres>,
    object: &serde_json::Value,
) -> Result<Option<String>> {
    if let Some(org_id) = object["metadata"]["org_id"].as_str() {
        return Ok(Some(org_id.to_string()));
    }
    let Some(subscription_id) = object["id"].as_str() else {
        return Ok(None);
    };
    Ok(
        sqlx::query_scalar("SELECT id FROM organizations WHERE stripe_subscription_id = $1")
            .bind(subscription_id)
            .fetch_optional(&mut **tx)
            .await?,
    )
}

async fn personal_user_for_subscription(
    tx: &mut Transaction<'_, Postgres>,
    object: &serde_json::Value,
) -> Result<Option<String>> {
    if let Some(user_id) = object["metadata"]["personal_user_id"].as_str() {
        return Ok(Some(user_id.to_string()));
    }
    let Some(subscription_id) = object["id"].as_str() else {
        return Ok(None);
    };
    Ok(sqlx::query_scalar(
        "SELECT user_id FROM billing_personal_accounts WHERE stripe_subscription_id = $1",
    )
    .bind(subscription_id)
    .fetch_optional(&mut **tx)
    .await?)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SignatureVerificationError {
    Malformed,
    Stale,
    Invalid,
}

/// Verify a `Stripe-Signature` header: `t=<unix>,v1=<hex hmac>[,v1=…]`, where the MAC is
/// HMAC-SHA256 over `"{t}.{payload}"`. Any valid `v1` within the timestamp tolerance passes
/// (Stripe sends multiples during secret rotation); comparison is constant-time via the `hmac`
/// crate's `verify_slice`.
pub(crate) fn verify_signature_detailed(
    secret: &str,
    header: &str,
    payload: &str,
    now: i64,
) -> std::result::Result<(), SignatureVerificationError> {
    let mut timestamp: Option<i64> = None;
    let mut candidates: Vec<Vec<u8>> = Vec::new();
    for part in header.split(',') {
        match part.trim().split_once('=') {
            Some(("t", value)) => timestamp = value.parse().ok(),
            Some(("v1", value)) => {
                if let Some(mac) = decode_hex(value) {
                    candidates.push(mac);
                }
            }
            _ => {}
        }
    }
    let Some(t) = timestamp else {
        return Err(SignatureVerificationError::Malformed);
    };
    let stale = now
        .checked_sub(t)
        .and_then(|delta| delta.checked_abs())
        .is_none_or(|age| age > SIGNATURE_TOLERANCE_SECS);
    if stale || candidates.is_empty() {
        return Err(if candidates.is_empty() {
            SignatureVerificationError::Malformed
        } else {
            SignatureVerificationError::Stale
        });
    }
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(t.to_string().as_bytes());
    mac.update(b".");
    mac.update(payload.as_bytes());
    if candidates
        .into_iter()
        .any(|candidate| mac.clone().verify_slice(&candidate).is_ok())
    {
        Ok(())
    } else {
        Err(SignatureVerificationError::Invalid)
    }
}

pub(crate) fn verify_signature(secret: &str, header: &str, payload: &str, now: i64) -> bool {
    verify_signature_detailed(secret, header, payload, now).is_ok()
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "e2e-test-clock")]
    #[test]
    fn test_clock_checkout_replays_only_with_its_original_customer() {
        assert!(test_checkout_customer_matches(None, None));
        assert!(test_checkout_customer_matches(None, Some("cus_existing")));
        assert!(test_checkout_customer_matches(
            Some("cus_original"),
            Some("cus_original")
        ));
        assert!(!test_checkout_customer_matches(
            Some("cus_original"),
            Some("cus_different")
        ));
        assert!(!test_checkout_customer_matches(Some("cus_original"), None));
    }
    use axum::response::IntoResponse;
    use std::collections::HashSet;
    use std::str::FromStr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use tokio::net::TcpListener;
    use uuid::Uuid;

    #[derive(Clone)]
    struct PreviewHttpState {
        request: Arc<Mutex<Option<String>>>,
    }

    async fn preview_http_mock(
        State(state): State<PreviewHttpState>,
        request: axum::http::Request<axum::body::Body>,
    ) -> axum::response::Response<axum::body::Body> {
        let body = axum::body::to_bytes(request.into_body(), 16 * 1024)
            .await
            .expect("small Stripe preview request");
        *state.request.lock().expect("preview request lock") =
            Some(String::from_utf8(body.to_vec()).expect("form body is UTF-8"));
        axum::Json(serde_json::json!({
            "object": "invoice",
            "parent": {"subscription_details": {"subscription": "sub-preview"}},
            "lines": {"data": [{
                "parent": {"type": "subscription_item_details", "subscription_item_details": {
                    "subscription": "sub-preview", "proration": false
                }},
                "pricing": {"price_details": {"price": "price-monthly"}},
                "period": {"start": 1_802_592_000, "end": 1_805_270_400}
            }]}
        }))
        .into_response()
    }

    async fn preview_http_server() -> (
        String,
        Arc<Mutex<Option<String>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind preview mock");
        let address = listener.local_addr().expect("preview mock address");
        let request = Arc::new(Mutex::new(None));
        let state = PreviewHttpState {
            request: request.clone(),
        };
        let app = axum::Router::new()
            .route(
                "/v1/invoices/create_preview",
                axum::routing::post(preview_http_mock),
            )
            .with_state(state);
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve preview mock");
        });
        (format!("http://{address}"), request, server)
    }

    #[derive(Clone)]
    struct RefundHttpState {
        requests: Arc<Mutex<Vec<String>>>,
    }

    async fn refund_http_mock(
        State(state): State<RefundHttpState>,
        request: axum::http::Request<axum::body::Body>,
    ) -> axum::response::Response<axum::body::Body> {
        let path = request.uri().to_string();
        state
            .requests
            .lock()
            .expect("refund request log lock")
            .push(path.clone());
        match path.as_str() {
            "/v1/refunds/re_verified" => axum::Json(serde_json::json!({
                "id": "re_verified",
                "status": "succeeded",
                "payment_intent": "pi_expected",
                "charge": "ch_expected",
                "amount": 299,
                "currency": "gbp"
            }))
            .into_response(),
            "/v1/payment_intents/pi_expected" => axum::Json(serde_json::json!({
                "id": "pi_expected",
                "status": "succeeded",
                "amount_received": 799,
                "currency": "gbp",
                "latest_charge": "ch_expected"
            }))
            .into_response(),
            "/v1/charges/ch_expected" => axum::Json(serde_json::json!({
                "id": "ch_expected",
                "payment_intent": "pi_expected",
                "currency": "gbp",
                "amount_captured": 799,
                "amount_refunded": 799
            }))
            .into_response(),
            _ => axum::http::StatusCode::NOT_FOUND.into_response(),
        }
    }

    async fn refund_http_server() -> (String, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>)
    {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind refund mock");
        let address = listener.local_addr().expect("refund mock address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let app = axum::Router::new()
            .route("/{*path}", axum::routing::any(refund_http_mock))
            .with_state(RefundHttpState {
                requests: requests.clone(),
            });
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve refund mock");
        });
        (format!("http://{address}"), requests, server)
    }

    type CapturedSponsoredCheckoutRequests = Arc<Mutex<Vec<(Option<String>, String)>>>;

    #[derive(Clone)]
    struct SponsoredCheckoutHttpState {
        requests: CapturedSponsoredCheckoutRequests,
        calls: Arc<AtomicUsize>,
        accepted_keys: Arc<Mutex<HashSet<String>>>,
        remote_creations: Arc<AtomicUsize>,
    }

    async fn sponsored_checkout_http_mock(
        State(state): State<SponsoredCheckoutHttpState>,
        request: axum::http::Request<axum::body::Body>,
    ) -> axum::response::Response<axum::body::Body> {
        let idempotency_key = request
            .headers()
            .get("idempotency-key")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = axum::body::to_bytes(request.into_body(), 16 * 1024)
            .await
            .expect("small sponsored Checkout request");
        state
            .requests
            .lock()
            .expect("sponsored Checkout request lock")
            .push((
                idempotency_key.clone(),
                String::from_utf8(body.to_vec()).expect("form body is UTF-8"),
            ));
        if let Some(key) = idempotency_key.as_ref() {
            let mut accepted_keys = state
                .accepted_keys
                .lock()
                .expect("accepted Checkout idempotency keys");
            if accepted_keys.insert(key.clone()) {
                state.remote_creations.fetch_add(1, Ordering::SeqCst);
            }
        }
        if state.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(serde_json::json!({
                    "error": {"type": "api_error", "code": "api_error"}
                })),
            )
                .into_response();
        }
        axum::Json(serde_json::json!({
            "id": "cs_recovered_http",
            "url": "https://checkout.stripe.test/cs_recovered_http",
            "customer": "cus_recovered_http",
            "subscription": null
        }))
        .into_response()
    }

    async fn sponsored_checkout_http_server() -> (
        String,
        CapturedSponsoredCheckoutRequests,
        Arc<AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind sponsored Checkout mock");
        let address = listener
            .local_addr()
            .expect("sponsored Checkout mock address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let remote_creations = Arc::new(AtomicUsize::new(0));
        let state = SponsoredCheckoutHttpState {
            requests: requests.clone(),
            calls: Arc::new(AtomicUsize::new(0)),
            accepted_keys: Arc::new(Mutex::new(HashSet::new())),
            remote_creations: remote_creations.clone(),
        };
        let app = axum::Router::new()
            .route(
                "/v1/checkout/sessions",
                axum::routing::post(sponsored_checkout_http_mock),
            )
            .with_state(state);
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve sponsored Checkout mock");
        });
        (
            format!("http://{address}"),
            requests,
            remote_creations,
            server,
        )
    }

    #[derive(Clone)]
    struct SponsoredScheduleVerificationHttpState {
        subscription: serde_json::Value,
        schedule: serde_json::Value,
        requests: Arc<Mutex<Vec<String>>>,
    }

    async fn sponsored_schedule_verification_http_mock(
        State(state): State<SponsoredScheduleVerificationHttpState>,
        request: axum::http::Request<axum::body::Body>,
    ) -> axum::response::Response<axum::body::Body> {
        let path = request.uri().path().to_owned();
        state
            .requests
            .lock()
            .expect("schedule verification request lock")
            .push(path.clone());
        match path.as_str() {
            "/v1/subscriptions/sub_recovery_http" => axum::Json(state.subscription).into_response(),
            "/v1/subscription_schedules/sched_recovery_http" => {
                axum::Json(state.schedule).into_response()
            }
            _ => (
                axum::http::StatusCode::NOT_FOUND,
                axum::Json(serde_json::json!({
                    "error": {"type": "invalid_request_error", "code": "resource_missing"}
                })),
            )
                .into_response(),
        }
    }

    async fn sponsored_schedule_verification_http_server(
        schedule: serde_json::Value,
    ) -> (String, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind sponsored schedule verification mock");
        let address = listener
            .local_addr()
            .expect("sponsored schedule verification mock address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let state = SponsoredScheduleVerificationHttpState {
            subscription: serde_json::json!({
                "id": "sub_recovery_http",
                "object": "subscription",
                "customer": "cus_recovery_http",
                "status": "active",
                "cancel_at_period_end": false,
                "cancel_at": null,
                "default_payment_method": "pm_recovery_http",
                "schedule": "sched_recovery_http",
                "items": {"data": [{
                    "id": "si_recovery_http",
                    "price": {"id": "price_current_http"},
                    "quantity": 1,
                    "current_period_start": 1_600_000_000_i64,
                    "current_period_end": 1_700_000_000_i64
                }]}
            }),
            schedule,
            requests: requests.clone(),
        };
        let app = axum::Router::new()
            .fallback(axum::routing::any(
                sponsored_schedule_verification_http_mock,
            ))
            .with_state(state);
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve sponsored schedule verification mock");
        });
        (format!("http://{address}"), requests, server)
    }

    #[derive(Clone)]
    struct CancellationHttpState {
        requests: Arc<Mutex<Vec<(String, String, String)>>>,
        updated_schedules: Arc<Mutex<HashSet<String>>>,
        boundary_cancelled: Arc<Mutex<bool>>,
    }

    async fn cancellation_http_mock(
        State(state): State<CancellationHttpState>,
        request: axum::http::Request<axum::body::Body>,
    ) -> axum::response::Response<axum::body::Body> {
        let method = request.method().to_string();
        let path = request.uri().path().to_owned();
        let body = axum::body::to_bytes(request.into_body(), 16 * 1024)
            .await
            .expect("small Stripe mock body");
        let body = String::from_utf8(body.to_vec()).expect("form body is UTF-8");
        state.requests.lock().expect("request log lock").push((
            method.clone(),
            path.clone(),
            body.clone(),
        ));
        if method == "POST" && path.starts_with("/v1/subscription_schedules/") {
            let schedule_id = path.trim_start_matches("/v1/subscription_schedules/");
            state
                .updated_schedules
                .lock()
                .expect("updated schedule lock")
                .insert(schedule_id.to_owned());
        }
        if method == "POST" && path == "/v1/subscriptions/sub_boundary_direct" {
            *state
                .boundary_cancelled
                .lock()
                .expect("boundary cancellation lock") = true;
        }
        let boundary_cancelled = *state
            .boundary_cancelled
            .lock()
            .expect("boundary cancellation lock");
        let schedule_updated = state
            .updated_schedules
            .lock()
            .expect("updated schedule lock")
            .clone();
        let value = match (method.as_str(), path.as_str()) {
            ("GET", "/v1/subscriptions/search") => serde_json::json!({
                "object":"search_result", "data":[{
                    "id":"sub_restored", "object":"subscription", "customer":"cus_original",
                    "status":"incomplete", "default_payment_method":"pm_saved",
                    "cancel_at_period_end":false, "cancel_at":null,
                    "metadata":{
                        "personal_user_id":"user_restore", "operation_id":"transfer:one:source-restoration",
                        "sotto_allocation_reference":"alloc_restore", "sotto_transfer_id":"transfer:one"
                    },
                    "items":{"data":[{
                        "id":"si_restored", "price":{"id":"price_standard_monthly"}, "quantity":1,
                        "current_period_start":1700000000, "current_period_end":1702592000
                    }]}
                }]
            }),
            ("GET", "/v1/subscriptions/sub_scheduled") => serde_json::json!({
                "id":"sub_scheduled", "customer":"cus_test", "status":"active",
                "cancel_at_period_end":false,
                "cancel_at":if schedule_updated.contains("sched_test") { Some(1707776000) } else { None::<i64> },
                "schedule":"sched_test", "items":{"data":[{
                    "id":"si_test", "price":{"id":"price_founding_monthly"}, "quantity":1,
                    "current_period_start":1705184000, "current_period_end":1707776000
                }]}
            }),
            ("GET", "/v1/subscription_schedules/sched_test")
                if schedule_updated.contains("sched_test") =>
            {
                serde_json::json!({
                    "id":"sched_test", "subscription":"sub_scheduled", "end_behavior":"cancel",
                    "current_phase":{"start_date":1702592000,"end_date":1707776000},
                    "phases":[{"start_date":1702592000,"end_date":1707776000,"items":[{
                        "price":{"id":"price_founding_monthly"},"quantity":1
                    }]}]
                })
            }
            ("GET", "/v1/subscription_schedules/sched_test") => serde_json::json!({
                "id":"sched_test", "subscription":"sub_scheduled", "end_behavior":"cancel",
                "current_phase":{"start_date":1702592000,"end_date":1710368000},
                "phases":[{"start_date":1702592000,"end_date":1710368000,"items":[{
                    "price":{"id":"price_founding_monthly"},"quantity":1
                }]}]
            }),
            ("POST", "/v1/subscription_schedules/sched_test") => serde_json::json!({
                "id":"sched_test", "subscription":"sub_scheduled", "end_behavior":"cancel",
                "current_phase":{"start_date":1702592000,"end_date":1707776000},
                "phases":[{"start_date":1702592000,"end_date":1707776000,"items":[{
                    "price":{"id":"price_founding_monthly"},"quantity":1
                }]}]
            }),
            ("GET", "/v1/subscriptions/sub_transition") => serde_json::json!({
                "id":"sub_transition", "customer":"cus_test", "status":"active",
                "cancel_at_period_end":false, "cancel_at":null,
                "schedule":"sched_transition", "items":{"data":[{
                    "id":"si_transition", "price":{"id":"price_founding_monthly"}, "quantity":1,
                    "current_period_start":1705184000, "current_period_end":1707776000
                }]}
            }),
            ("GET", "/v1/subscription_schedules/sched_transition")
                if schedule_updated.contains("sched_transition") =>
            {
                serde_json::json!({
                    "id":"sched_transition", "subscription":"sub_transition", "end_behavior":"release",
                    "current_phase":{"start_date":1702592000,"end_date":1707776000},
                    "phases":[
                        {"start_date":1702592000,"end_date":1707776000,"items":[{
                            "price":{"id":"price_founding_monthly"},"quantity":1
                        }]},
                        {"start_date":1707776000,"end_date":1710368000,"items":[{
                            "price":{"id":"price_standard_monthly"},"quantity":1
                        }]}
                    ]
                })
            }
            ("GET", "/v1/subscription_schedules/sched_transition") => serde_json::json!({
                "id":"sched_transition", "subscription":"sub_transition", "end_behavior":"release",
                "current_phase":{"start_date":1702592000,"end_date":1710368000},
                "phases":[{"start_date":1702592000,"end_date":1710368000,"items":[{
                    "price":{"id":"price_founding_monthly"},"quantity":1
                }]}]
            }),
            ("POST", "/v1/subscription_schedules/sched_transition") => serde_json::json!({
                "id":"sched_transition", "subscription":"sub_transition", "end_behavior":"release",
                "current_phase":{"start_date":1702592000,"end_date":1707776000},
                "phases":[
                    {"start_date":1702592000,"end_date":1707776000,"items":[{
                        "price":{"id":"price_founding_monthly"},"quantity":1
                    }]},
                    {"start_date":1707776000,"end_date":1710368000,"items":[{
                        "price":{"id":"price_standard_monthly"},"quantity":1
                    }]}
                ]
            }),
            ("GET", "/v1/subscriptions/sub_unscheduled") => serde_json::json!({
                "id":"sub_unscheduled", "customer":"cus_test", "status":"active",
                "cancel_at_period_end":false, "cancel_at":null,
                "items":{"data":[{
                    "id":"si_test", "price":{"id":"price_standard_monthly"}, "quantity":1,
                    "current_period_start":1700000000, "current_period_end":1702592000
                }]}
            }),
            ("GET", "/v1/subscriptions/sub_boundary_direct") => serde_json::json!({
                "id":"sub_boundary_direct", "customer":"cus_test", "status":"active",
                "cancel_at_period_end":false,
                "cancel_at":if boundary_cancelled { Some(1707776000) } else { None::<i64> },
                "items":{"data":[{
                    "id":"si_boundary", "price":{"id":"price_standard_monthly"}, "quantity":1,
                    "current_period_start":1705184000, "current_period_end":1707776000
                }]}
            }),
            ("POST", "/v1/subscriptions/sub_boundary_direct") => serde_json::json!({
                "id":"sub_boundary_direct", "status":"active"
            }),
            ("POST", "/v1/subscriptions/sub_unscheduled") => serde_json::json!({
                "id":"sub_unscheduled", "status":"active"
            }),
            _ => {
                return axum::response::Response::builder()
                    .status(axum::http::StatusCode::NOT_FOUND)
                    .body(axum::body::Body::from("{}"))
                    .expect("mock response")
            }
        };
        axum::response::Response::builder()
            .status(axum::http::StatusCode::OK)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(value.to_string()))
            .expect("mock response")
    }

    async fn cancellation_http_server() -> (
        String,
        Arc<Mutex<Vec<(String, String, String)>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind Stripe HTTP mock");
        let address = listener.local_addr().expect("mock address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let router = Router::new()
            .fallback(axum::routing::any(cancellation_http_mock))
            .with_state(CancellationHttpState {
                requests: requests.clone(),
                updated_schedules: Arc::new(Mutex::new(HashSet::new())),
                boundary_cancelled: Arc::new(Mutex::new(false)),
            });
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (format!("http://{address}"), requests, task)
    }

    #[test]
    fn founding_price_moves_to_standard_at_the_award_boundary() {
        let boundary = FoundingDate::parse("2027-01-31")
            .expect("date is valid")
            .to_unix_seconds();

        assert_eq!(
            founding_transition_offer(BillingOffer::FoundingMonthly, boundary, boundary - 1),
            BillingOffer::FoundingMonthly
        );
        assert_eq!(
            founding_transition_offer(BillingOffer::FoundingMonthly, boundary, boundary),
            BillingOffer::StandardMonthly
        );
        assert_eq!(
            founding_transition_offer(BillingOffer::StandardAnnual, boundary, boundary),
            BillingOffer::StandardAnnual
        );
        assert_eq!(
            expected_founding_invoice_offer(
                BillingOffer::StandardMonthly,
                Some(FoundingOffer::Monthly),
                Some(boundary),
                boundary - 1,
            ),
            Some(BillingOffer::FoundingMonthly)
        );
        assert_eq!(
            expected_founding_invoice_offer(
                BillingOffer::FoundingMonthly,
                Some(FoundingOffer::Monthly),
                Some(boundary),
                boundary,
            ),
            Some(BillingOffer::StandardMonthly)
        );
        assert_eq!(
            expected_founding_invoice_offer(
                BillingOffer::FoundingMonthly,
                Some(FoundingOffer::Monthly),
                None,
                boundary,
            ),
            None
        );
    }

    #[test]
    fn first_sponsored_founding_invoice_uses_the_pending_seats_requested_price() {
        assert_eq!(
            expected_sponsored_invoice_offer(
                BillingOffer::FoundingMonthly,
                None,
                None,
                "pending",
                1_800_000_000,
            ),
            Some(BillingOffer::FoundingMonthly)
        );
        assert_eq!(
            expected_sponsored_invoice_offer(
                BillingOffer::FoundingMonthly,
                None,
                None,
                "active",
                1_800_000_000,
            ),
            None,
            "an active founding seat without an award remains corrupt evidence"
        );
    }

    #[test]
    fn sponsored_phases_keep_independent_founder_boundaries_and_pending_removals() {
        let catalogue = BillingPriceIds {
            standard_monthly: "price_standard_monthly".into(),
            standard_annual: "price_standard_annual".into(),
            founding_monthly: "price_founding_monthly".into(),
            founding_annual: "price_founding_annual".into(),
        };
        let rows = vec![
            (
                "monthly-founder".into(),
                "founding_monthly".into(),
                "active".into(),
                900,
                None,
                Some(2_000),
                Some("founding_monthly".into()),
            ),
            (
                "annual-founder".into(),
                "founding_annual".into(),
                "active".into(),
                900,
                None,
                Some(3_000),
                Some("founding_annual".into()),
            ),
            (
                "standard-seat".into(),
                "standard_monthly".into(),
                "active".into(),
                900,
                None,
                None,
                None,
            ),
            (
                "removing-seat".into(),
                "standard_monthly".into(),
                "active".into(),
                900,
                None,
                None,
                None,
            ),
        ];
        let pending = vec![("removing-seat".into(), "remove".into(), Some(2_500))];
        let phases = compile_sponsored_provider_phases(rows, &pending, &catalogue, 1_000)
            .expect("compile all independent seat boundaries");

        assert_eq!(phases.len(), 4);
        assert_eq!(phases[0].start_at_epoch, 1_000);
        assert_eq!(phases[0].end_at_epoch, Some(2_000));
        assert_eq!(phases[0].items.len(), 3);
        assert!(phases[0].items.contains(&SponsoredSubscriptionItemRequest {
            provider_price_id: "price_founding_monthly".into(),
            quantity: 1,
        }));
        assert!(phases[0].items.contains(&SponsoredSubscriptionItemRequest {
            provider_price_id: "price_founding_annual".into(),
            quantity: 1,
        }));
        assert!(phases[0].items.contains(&SponsoredSubscriptionItemRequest {
            provider_price_id: "price_standard_monthly".into(),
            quantity: 2,
        }));
        assert_eq!(phases[1].start_at_epoch, 2_000);
        assert!(phases[1].items.contains(&SponsoredSubscriptionItemRequest {
            provider_price_id: "price_standard_monthly".into(),
            quantity: 3,
        }));
        assert_eq!(phases[2].start_at_epoch, 2_500);
        assert!(phases[2].items.contains(&SponsoredSubscriptionItemRequest {
            provider_price_id: "price_standard_monthly".into(),
            quantity: 2,
        }));
        assert_eq!(phases[3].start_at_epoch, 3_000);
        assert!(phases[3].items.contains(&SponsoredSubscriptionItemRequest {
            provider_price_id: "price_standard_annual".into(),
            quantity: 1,
        }));
    }

    #[test]
    fn pending_founding_sponsored_seat_keeps_its_requested_price_before_award() {
        let catalogue = BillingPriceIds {
            standard_monthly: "price_standard_monthly".into(),
            standard_annual: "price_standard_annual".into(),
            founding_monthly: "price_founding_monthly".into(),
            founding_annual: "price_founding_annual".into(),
        };
        let rows = vec![(
            "pending-founder".into(),
            "founding_monthly".into(),
            "pending".into(),
            900,
            None,
            None,
            None,
        )];
        let phases = compile_sponsored_provider_phases(rows, &[], &catalogue, 1_000)
            .expect("unpaid pending seat can be scheduled before its award exists");
        assert_eq!(phases.len(), 1);
        assert_eq!(
            phases[0].items,
            vec![SponsoredSubscriptionItemRequest {
                provider_price_id: "price_founding_monthly".into(),
                quantity: 1,
            }]
        );
    }

    #[test]
    fn stripe_schedule_readback_must_match_price_boundary_and_quantity() {
        let current_items = vec![SponsoredSubscriptionItem {
            provider_item_id: "si_1".into(),
            provider_price_id: "price_founding_monthly".into(),
            quantity: 1,
        }];
        let expected = vec![SponsoredSubscriptionPhase {
            start_at_epoch: 1_800_000_000,
            end_at_epoch: None,
            items: vec![SponsoredSubscriptionItemRequest {
                provider_price_id: "price_standard_monthly".into(),
                quantity: 1,
            }],
        }];
        let accepted = serde_json::json!({
            "phases": [
                {
                    "start_date": 1_600_000_000,
                    "end_date": 1_700_000_000,
                    "items": [{"price": "price_standard_monthly", "quantity": 1}]
                },
                {
                    "start_date": 1_700_000_000,
                    "end_date": 1_800_000_000,
                    "items": [{"price": {"id": "price_founding_monthly"}, "quantity": 1}]
                },
                {
                    "start_date": 1_800_000_000,
                    "end_date": 2_100_000_000,
                    "items": [{"price": {"id": "price_standard_monthly"}, "quantity": 1}]
                }
            ]
        });
        assert!(stripe_schedule_contains_phases(
            &accepted,
            1_700_000_000,
            1_800_000_000,
            &current_items,
            &expected
        ));

        let drifted = serde_json::json!({
            "phases": [
                {
                    "start_date": 1_700_000_000,
                    "end_date": 1_800_000_000,
                    "items": [{"price": "price_founding_monthly", "quantity": 1}]
                },
                {
                    "start_date": 1_800_000_000,
                    "end_date": 2_100_000_000,
                    "items": [{"price": "price_founding_monthly", "quantity": 1}]
                }
            ]
        });
        assert!(!stripe_schedule_contains_phases(
            &drifted,
            1_700_000_000,
            1_800_000_000,
            &current_items,
            &expected
        ));

        let unexpected_future = serde_json::json!({
            "phases": [
                {
                    "start_date": 1_700_000_000,
                    "end_date": 1_800_000_000,
                    "items": [{"price": "price_founding_monthly", "quantity": 1}]
                },
                {
                    "start_date": 1_800_000_000,
                    "end_date": 1_900_000_000,
                    "items": [{"price": "price_standard_monthly", "quantity": 1}]
                },
                {
                    "start_date": 1_900_000_000,
                    "end_date": 2_100_000_000,
                    "items": [{"price": "price_founding_monthly", "quantity": 1}]
                }
            ]
        });
        assert!(!stripe_schedule_contains_phases(
            &unexpected_future,
            1_700_000_000,
            1_800_000_000,
            &current_items,
            &expected
        ));

        let expected_with_contiguous_open_phase = vec![
            SponsoredSubscriptionPhase {
                start_at_epoch: 1_800_000_000,
                end_at_epoch: None,
                items: vec![SponsoredSubscriptionItemRequest {
                    provider_price_id: "price_founding_monthly".into(),
                    quantity: 1,
                }],
            },
            SponsoredSubscriptionPhase {
                start_at_epoch: 1_900_000_000,
                end_at_epoch: None,
                items: vec![SponsoredSubscriptionItemRequest {
                    provider_price_id: "price_standard_monthly".into(),
                    quantity: 1,
                }],
            },
        ];
        let gapped_open_phase = serde_json::json!({
            "phases": [
                {
                    "start_date": 1_700_000_000,
                    "end_date": 1_800_000_000,
                    "items": [{"price": "price_founding_monthly", "quantity": 1}]
                },
                {
                    "start_date": 1_800_000_000,
                    "end_date": 1_850_000_000,
                    "items": [{"price": "price_founding_monthly", "quantity": 1}]
                },
                {
                    "start_date": 1_900_000_000,
                    "end_date": 2_100_000_000,
                    "items": [{"price": "price_standard_monthly", "quantity": 1}]
                }
            ]
        });
        assert!(!stripe_schedule_contains_phases(
            &gapped_open_phase,
            1_700_000_000,
            1_800_000_000,
            &current_items,
            &expected_with_contiguous_open_phase
        ));

        let invalid_final_boundary = serde_json::json!({
            "phases": [
                {
                    "start_date": 1_700_000_000,
                    "end_date": 1_800_000_000,
                    "items": [{"price": "price_founding_monthly", "quantity": 1}]
                },
                {
                    "start_date": 1_800_000_000,
                    "end_date": 1_800_000_000,
                    "items": [{"price": "price_standard_monthly", "quantity": 1}]
                }
            ]
        });
        assert!(!stripe_schedule_contains_phases(
            &invalid_final_boundary,
            1_700_000_000,
            1_800_000_000,
            &current_items,
            &expected
        ));
    }

    #[test]
    fn personal_schedule_cancellation_keeps_only_the_paid_phase() {
        let current_items = vec![SponsoredSubscriptionItem {
            provider_item_id: "si_personal".into(),
            provider_price_id: "price_founding_monthly".into(),
            quantity: 1,
        }];
        let form =
            personal_schedule_cancellation_form(1_700_000_000, 1_702_592_000, &current_items);
        assert!(form.contains(&("end_behavior".into(), "cancel".into())));
        assert!(form.contains(&("proration_behavior".into(), "none".into())));
        assert!(form.contains(&("phases[0][start_date]".into(), "1700000000".into())));
        assert!(form.contains(&("phases[0][end_date]".into(), "1702592000".into())));
        assert!(form.contains(&(
            "phases[0][items][0][price]".into(),
            "price_founding_monthly".into()
        )));
        assert!(!form.iter().any(|(key, _)| key.starts_with("phases[1]")));
        assert_eq!(
            personal_schedule_cancellation_idempotency_key(
                "sub_personal_1",
                1_702_592_000,
                "request-1"
            ),
            "sotto-personal-cancel:sub_personal_1:1702592000:19f1064b619d49d35392eac7261cd726"
        );

        let readback = serde_json::json!({
            "id": "sub_sched_1",
            "subscription": {"id": "sub_personal_1"},
            "end_behavior": "cancel",
            "phases": [{
                "start_date": 1_700_000_000,
                "end_date": 1_702_592_000,
                "items": [{"price": {"id": "price_founding_monthly"}, "quantity": 1}]
            }]
        });
        assert_eq!(
            stripe_schedule_subscription_id(&readback),
            Some("sub_personal_1")
        );
        assert_eq!(readback["end_behavior"].as_str(), Some("cancel"));
        assert!(stripe_schedule_contains_phases(
            &readback,
            1_700_000_000,
            1_702_592_000,
            &current_items,
            &[],
        ));
    }

    #[tokio::test]
    async fn stripe_personal_cancellation_updates_schedules_and_direct_subscriptions() {
        let (origin, requests, server) = cancellation_http_server().await;
        let schedule_readback = serde_json::json!({
            "id":"sched_test", "subscription":"sub_scheduled", "end_behavior":"cancel",
            "phases":[{"start_date":1702592000,"end_date":1707776000,"items":[{
                "price":{"id":"price_founding_monthly"},"quantity":1
            }]}]
        });
        assert!(stripe_schedule_contains_phases(
            &schedule_readback,
            1702592000,
            1707776000,
            &[SponsoredSubscriptionItem {
                provider_item_id: "si_test".into(),
                provider_price_id: "price_founding_monthly".into(),
                quantity: 1,
            }],
            &[]
        ));
        let provider = StripeBilling {
            api_key: "rk_test_mock".into(),
            price_id: "price_test".into(),
            api_origin: origin,
        };

        let scheduled = provider
            .cancel_personal_subscription("sub_scheduled", "cancel-request", "user")
            .await;
        assert!(
            scheduled.is_ok(),
            "scheduled cancellation succeeds after read-back: {:?}; requests: {:?}",
            scheduled,
            requests.lock().expect("request log lock")
        );
        assert!(matches!(scheduled, Ok(SubscriptionObservation::Current(_))));
        let unscheduled = provider
            .cancel_personal_subscription("sub_unscheduled", "cancel-unscheduled", "user")
            .await
            .expect("unscheduled cancellation succeeds");
        assert!(matches!(unscheduled, SubscriptionObservation::Current(_)));
        let transitioned = provider
            .update_personal_subscription(
                "sub_transition",
                &[SponsoredSubscriptionPhase {
                    start_at_epoch: 1707776000,
                    end_at_epoch: None,
                    items: vec![SponsoredSubscriptionItemRequest {
                        provider_price_id: "price_standard_monthly".into(),
                        quantity: 1,
                    }],
                }],
                "transition-request",
            )
            .await;
        assert!(
            transitioned.is_ok(),
            "transition update retains the active schedule phase start: {:?}; requests: {:?}",
            transitioned,
            requests.lock().expect("request log lock")
        );
        let transitioned = transitioned.expect("transition update returned snapshot");
        assert_eq!(
            transitioned.schedule_id.as_deref(),
            Some("sched_transition")
        );

        let requests = requests.lock().expect("request log lock").clone();
        assert_eq!(requests.len(), 12);
        assert_eq!(
            requests
                .iter()
                .filter(|(method, path, _)| {
                    method == "GET" && path == "/v1/subscription_schedules/sched_test"
                })
                .count(),
            2,
            "scheduled cancellation verifies the updated schedule"
        );
        assert_eq!(
            requests
                .iter()
                .filter(|(method, path, _)| {
                    method == "GET" && path == "/v1/subscriptions/sub_scheduled"
                })
                .count(),
            2,
            "scheduled cancellation reads the subscription before and after the update"
        );
        let schedule_update = requests
            .iter()
            .find(|(method, path, _)| {
                method == "POST" && path == "/v1/subscription_schedules/sched_test"
            })
            .expect("schedule update request");
        assert!(schedule_update.2.contains("end_behavior=cancel"));
        assert!(schedule_update
            .2
            .contains("phases%5B0%5D%5Bstart_date%5D=1702592000"));
        assert!(schedule_update
            .2
            .contains("phases%5B0%5D%5Bend_date%5D=1707776000"));
        assert!(schedule_update.2.contains("proration_behavior=none"));
        assert!(!schedule_update.2.contains("phases%5B1%5D"));
        let direct_update = requests
            .iter()
            .find(|(method, path, _)| {
                method == "POST" && path == "/v1/subscriptions/sub_unscheduled"
            })
            .expect("direct subscription update request");
        assert!(direct_update.2.contains("cancel_at_period_end=true"));
        let transition_update = requests
            .iter()
            .find(|(method, path, _)| {
                method == "POST" && path == "/v1/subscription_schedules/sched_transition"
            })
            .expect("transition schedule update request");
        assert!(transition_update
            .2
            .contains("phases%5B0%5D%5Bstart_date%5D=1702592000"));
        assert!(transition_update
            .2
            .contains("phases%5B0%5D%5Bend_date%5D=1707776000"));
        server.abort();
    }

    #[tokio::test]
    async fn sponsored_transfer_reads_the_next_period_from_stripe_preview() {
        let (origin, request, server) = preview_http_server().await;
        let provider = StripeBilling {
            api_key: "rk_test_mock".into(),
            price_id: "price_test".into(),
            api_origin: origin,
        };
        let end = provider
            .preview_transfer_period_end("sub-preview", "price-monthly", 1_802_592_000)
            .await
            .expect("Stripe preview returns the sponsor's anchored renewal period");
        assert_eq!(end, 1_805_270_400);
        let request = request.lock().expect("preview request lock");
        let body = request.as_deref().expect("preview request was sent");
        assert!(body.contains("subscription=sub-preview"));
        drop(request);
        server.abort();
    }

    #[tokio::test]
    async fn refund_recovery_uses_charge_totals_without_listing_refunds() {
        let (origin, requests, server) = refund_http_server().await;
        let provider = StripeBilling {
            api_key: "rk_test_mock".into(),
            price_id: "price_test".into(),
            api_origin: origin,
        };
        let refund = provider
            .get_refund("re_verified")
            .await
            .expect("retrieve the existing refund by its Stripe id");
        assert_eq!(refund.refund_id, "re_verified");
        assert_eq!(refund.payment_reference, "pi_expected");
        assert_eq!(refund.charge_id.as_deref(), Some("ch_expected"));
        assert_eq!(refund.amount_pence, 299);
        assert_eq!(refund.currency, "gbp");
        assert_eq!(
            provider
                .get_refundable_amount("pi_expected", "ch_expected", 299)
                .await
                .expect("read the remaining amount for full-refund validation"),
            (299, "gbp".into())
        );
        assert!(provider
            .get_refundable_amount("pi_expected", "ch_other", 299)
            .await
            .is_err());
        assert_eq!(
            *requests.lock().expect("refund request log lock"),
            [
                "/v1/refunds/re_verified".to_string(),
                "/v1/payment_intents/pi_expected".to_string(),
                "/v1/charges/ch_expected".to_string(),
                "/v1/payment_intents/pi_expected".to_string()
            ]
        );
        server.abort();
    }

    #[tokio::test]
    async fn sponsored_failure_fanout_and_resolution_cover_bounded_recipients() {
        if std::env::var("SOTTO_RUN_DB_TESTS").as_deref() != Ok("1") {
            return;
        }
        let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL is required");
        let options =
            sqlx::postgres::PgConnectOptions::from_str(&database_url).expect("parse DATABASE_URL");
        assert!(
            matches!(options.get_host(), "localhost" | "127.0.0.1" | "::1"),
            "refusing to write test fixtures to non-local database: {}",
            options.get_host()
        );
        let pool = crate::db::connect(&database_url)
            .await
            .expect("connect test database");
        crate::db::migrate(&pool)
            .await
            .expect("migrate test database");

        let fixture = Uuid::new_v4().simple().to_string();
        let account_id = format!("acct_sponsor_failure_{fixture}");
        let customer_id = format!("cus_sponsor_failure_{fixture}");
        let subscription_id = format!("sub_sponsor_failure_{fixture}");
        let organization_id = format!("sponsor_failure_org_{fixture}");
        let administrator_id = format!("sponsor_failure_admin_{fixture}");
        let additional_administrator_ids = (0..127)
            .map(|index| format!("sponsor_failure_admin_{fixture}_{index}"))
            .collect::<Vec<_>>();
        let beneficiary_ids = (0..128)
            .map(|index| format!("sponsor_failure_beneficiary_{fixture}_{index}"))
            .collect::<Vec<_>>();
        let event_ids = [
            format!("evt_sponsor_failure_a_{fixture}"),
            format!("evt_sponsor_failure_b_{fixture}"),
        ];
        let invoice_id = format!("in_sponsor_failure_{fixture}");
        let payer_id = format!("payer_sponsor_failure_{fixture}");
        let context = ProviderContext::new("stripe", &account_id, ProviderEnvironment::Test)
            .expect("valid sponsor provider context");

        for user_id in std::iter::once(&administrator_id)
            .chain(additional_administrator_ids.iter())
            .chain(beneficiary_ids.iter())
        {
            sqlx::query(
                "INSERT INTO users (id, oauth_provider, oauth_subject) VALUES ($1, $2, $1)",
            )
            .bind(user_id)
            .bind(format!("sponsor-failure-{fixture}"))
            .execute(&pool)
            .await
            .expect("insert test user");
        }
        sqlx::query("INSERT INTO organizations (id, enc_name, tier) VALUES ($1, $2, 'free')")
            .bind(&organization_id)
            .bind(b"opaque".as_slice())
            .execute(&pool)
            .await
            .expect("insert sponsor organisation");
        for (administrator, role) in std::iter::once((&administrator_id, "owner"))
            .chain(additional_administrator_ids.iter().map(|id| (id, "admin")))
        {
            sqlx::query(
                "INSERT INTO organization_memberships (org_id, user_id, role) VALUES ($1, $2, $3)",
            )
            .bind(&organization_id)
            .bind(administrator)
            .bind(role)
            .execute(&pool)
            .await
            .expect("insert sponsor administrator");
        }
        sqlx::query(
            "INSERT INTO billing_sponsored_subscriptions \
             (organization_id, provider_customer_id, provider_subscription_id, status) \
             VALUES ($1, $2, $3, 'active')",
        )
        .bind(&organization_id)
        .bind(&customer_id)
        .bind(&subscription_id)
        .execute(&pool)
        .await
        .expect("insert sponsored subscription");

        let mut allocations = Vec::new();
        let mut evidence_by_event = [Vec::new(), Vec::new()];
        for (index, beneficiary_id) in beneficiary_ids.iter().enumerate() {
            let allocation_reference = format!("alloc_sponsor_failure_{fixture}_{index}");
            let allocation = VerifiedAllocation::new(
                format!("allocation_sponsor_failure_{fixture}_{index}"),
                &payer_id,
                &customer_id,
                ProviderPayerKind::Sponsor,
                beneficiary_id,
                &subscription_id,
                "si_sponsor_monthly",
                &allocation_reference,
                format!("source_sponsor_failure_{fixture}_{index}"),
                1_000,
                None,
                AllocationState::Active,
                format!("stripe:sponsor-failure-owner:{fixture}:{index}"),
            )
            .expect("valid sponsored allocation");
            allocations.push(allocation);
            for event_index in 0..event_ids.len() {
                evidence_by_event[event_index].push(
                    crate::cloud_provider_stripe_renewals::StripeRenewalFailureEvidence::from_sponsored_parts(
                        &event_ids[event_index],
                        &invoice_id,
                        &format!("il_sponsor_failure_{fixture}_{event_index}_{index}"),
                        &format!("in_sponsor_paid_{fixture}"),
                        &format!("stripe:sponsor-predecessor:{fixture}"),
                        &account_id,
                        ProviderEnvironment::Test,
                        &allocation_reference,
                        &customer_id,
                        &subscription_id,
                        "si_sponsor_monthly",
                        "price_sponsor_monthly",
                        1_000,
                        2_000,
                        2_000,
                        3_000,
                        2_100 + event_index as i64,
                        crate::cloud_provider_stripe::StripeInterval::Month,
                    )
                    .expect("valid sponsored failure evidence"),
                );
            }
        }
        let mut registration = pool.begin().await.expect("begin allocation registration");
        for allocation in &allocations {
            register_verified_allocation(&mut registration, &context, allocation)
                .await
                .expect("register sponsored allocation");
        }
        registration
            .commit()
            .await
            .expect("commit allocation registration");

        let event = evidence_by_event[0][0]
            .verified_event()
            .expect("invoice wide event identity");
        assert_eq!(
            event.normalized_payload_hash,
            evidence_by_event[0][127]
                .verified_event()
                .expect("second invoice wide event identity")
                .normalized_payload_hash
        );
        let preparation = SponsoredRenewalFailurePreparation::Prepared(Box::new(
            PreparedSponsoredRenewalFailure {
                context: context.clone(),
                event,
                organization_id: organization_id.clone(),
                subscription_id: subscription_id.clone(),
                invoice_id: invoice_id.clone(),
                affected: allocations
                    .iter()
                    .cloned()
                    .zip(evidence_by_event[0].clone())
                    .collect(),
            },
        ));
        let second_preparation = SponsoredRenewalFailurePreparation::Prepared(Box::new(
            PreparedSponsoredRenewalFailure {
                context: context.clone(),
                event: evidence_by_event[1][0]
                    .verified_event()
                    .expect("second shared invoice event identity"),
                organization_id: organization_id.clone(),
                subscription_id: subscription_id.clone(),
                invoice_id: invoice_id.clone(),
                affected: allocations
                    .iter()
                    .cloned()
                    .zip(evidence_by_event[1].clone())
                    .collect(),
            },
        ));
        for preparation in [&preparation, &second_preparation] {
            let mut tx = pool
                .begin()
                .await
                .expect("begin grouped failure transaction");
            apply_sponsored_renewal_failure(
                &mut tx,
                preparation,
                crate::cloud_provider_stripe_renewal_store::StripeRenewalFailureProvenance::SignedWebhook,
            )
            .await
            .expect("accept every beneficiary failure atomically");
            tx.commit().await.expect("commit grouped failure");
        }

        let failures: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM cloud_provider_stripe_renewal_failures WHERE event_id = ANY($1)",
        )
        .bind(event_ids.to_vec())
        .fetch_one(&pool)
        .await
        .expect("count per allocation failures");
        let receipts: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM cloud_provider_event_receipts WHERE event_id = ANY($1)",
        )
        .bind(event_ids.to_vec())
        .fetch_one(&pool)
        .await
        .expect("count shared event receipts");
        let notice_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM cloud_notice_outbox WHERE event_key = $1 \
             AND kind IN ('sponsored_renewal_failure','sponsored_renewal_action')",
        )
        .bind(opaque_provider_id(
            "sponsored-renewal-v1",
            &[&organization_id, &invoice_id],
        ))
        .fetch_one(&pool)
        .await
        .expect("count beneficiary and administrator notices");
        let status: String = sqlx::query_scalar(
            "SELECT status FROM billing_sponsored_subscriptions WHERE organization_id = $1",
        )
        .bind(&organization_id)
        .fetch_one(&pool)
        .await
        .expect("sponsored subscription status");
        assert_eq!(failures, 256);
        assert_eq!(receipts, 2);
        assert_eq!(notice_count, 256);
        assert_eq!(status, "past_due");

        let event_key =
            opaque_provider_id("sponsored-renewal-v1", &[&organization_id, &invoice_id]);
        let mut resolution = pool
            .begin()
            .await
            .expect("begin matching paid invoice resolution");
        resolve_sponsored_renewal_failures(
            &mut resolution,
            &organization_id,
            &subscription_id,
            &invoice_id,
            2_500,
        )
        .await
        .expect("resolve only the matching sponsored invoice");
        resolution
            .commit()
            .await
            .expect("commit matching invoice resolution");
        let resolutions: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM cloud_provider_stripe_renewal_resolutions WHERE event_id = ANY($1)",
        )
        .bind(event_ids.to_vec())
        .fetch_one(&pool)
        .await
        .expect("count resolved beneficiary evidence");
        let recovered_notices: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM cloud_notice_outbox WHERE event_key = $1 \
             AND kind = 'recovery_resolved' AND status = 'pending'",
        )
        .bind(&event_key)
        .fetch_one(&pool)
        .await
        .expect("count recovery notices");
        let failure_notices: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM cloud_notice_outbox WHERE event_key = $1 \
             AND kind IN ('sponsored_renewal_failure','sponsored_renewal_action') \
             AND status = 'cancelled'",
        )
        .bind(&event_key)
        .fetch_one(&pool)
        .await
        .expect("count retired failure notices");
        assert_eq!(resolutions, 256);
        assert_eq!(recovered_notices, 256);
        assert_eq!(failure_notices, 256);

        let paid_event = Event {
            id: format!("evt_sponsor_paid_{fixture}"),
            created: 2_600,
            api_version: Some("2026-07-29.dahlia".into()),
            kind: "invoice.paid".into(),
            data: EventData {
                object: serde_json::json!({
                    "id": invoice_id,
                    "subscription": subscription_id,
                }),
            },
        };
        let mut paid_receipt = pool.begin().await.expect("begin paid invoice receipt");
        record_webhook_receipt(&mut paid_receipt, &paid_event, Some(&subscription_id))
            .await
            .expect("record exact paid invoice identity");
        mark_webhook_event_processed(&mut paid_receipt, &paid_event.id)
            .await
            .expect("mark paid invoice receipt complete");
        paid_receipt
            .commit()
            .await
            .expect("commit paid invoice receipt");
        sqlx::query(
            "UPDATE billing_sponsored_subscriptions SET status = 'active' WHERE organization_id = $1",
        )
        .bind(&organization_id)
        .execute(&pool)
        .await
        .expect("reflect the later paid invoice");
        let mut late_failure = pool.begin().await.expect("begin late failure replay");
        let late_failure_event = Event {
            id: event_ids[0].clone(),
            created: 2_100,
            api_version: Some("2026-07-29.dahlia".into()),
            kind: "invoice.payment_failed".into(),
            data: EventData {
                object: serde_json::json!({
                    "id": invoice_id,
                    "subscription": subscription_id,
                }),
            },
        };
        assert_eq!(
            record_webhook_event(&mut late_failure, &late_failure_event)
                .await
                .expect("failure receipt takes the subscription lock"),
            EventDisposition::Apply
        );
        apply_sponsored_renewal_failure(
            &mut late_failure,
            &preparation,
            crate::cloud_provider_stripe_renewal_store::StripeRenewalFailureProvenance::SignedWebhook,
        )
        .await
        .expect("processed paid invoice suppresses stale failure");
        late_failure
            .commit()
            .await
            .expect("commit ignored late failure");
        let failure_receipt: (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT subscription_id, provider_object_id FROM stripe_webhook_events WHERE event_id = $1",
        )
        .bind(&late_failure_event.id)
        .fetch_one(&pool)
        .await
        .expect("load failure receipt identity");
        assert_eq!(failure_receipt.0.as_deref(), Some(subscription_id.as_str()));
        assert_eq!(failure_receipt.1.as_deref(), Some(invoice_id.as_str()));
        let status_after_late_failure: String = sqlx::query_scalar(
            "SELECT status FROM billing_sponsored_subscriptions WHERE organization_id = $1",
        )
        .bind(&organization_id)
        .fetch_one(&pool)
        .await
        .expect("subscription remains current after late failure");
        assert_eq!(status_after_late_failure, "active");

        sqlx::query("DELETE FROM cloud_notice_outbox WHERE event_key = $1")
            .bind(event_key)
            .execute(&pool)
            .await
            .expect("remove test notices");
        sqlx::query(
            "DELETE FROM cloud_provider_stripe_renewal_resolutions WHERE event_id = ANY($1)",
        )
        .bind(event_ids.to_vec())
        .execute(&pool)
        .await
        .expect("remove test resolutions");
        sqlx::query("DELETE FROM cloud_provider_stripe_renewal_failures WHERE event_id = ANY($1)")
            .bind(event_ids.to_vec())
            .execute(&pool)
            .await
            .expect("remove test evidence");
        sqlx::query(
            "DELETE FROM cloud_provider_invalidation_associations WHERE event_id = ANY($1)",
        )
        .bind(event_ids.to_vec())
        .execute(&pool)
        .await
        .expect("remove test invalidations");
        sqlx::query("DELETE FROM cloud_provider_event_receipts WHERE event_id = ANY($1)")
            .bind(event_ids.to_vec())
            .execute(&pool)
            .await
            .expect("remove test event receipt");
        sqlx::query("DELETE FROM stripe_webhook_events WHERE event_id = $1")
            .bind(&paid_event.id)
            .execute(&pool)
            .await
            .expect("remove paid webhook receipt");
        for allocation_index in 0..beneficiary_ids.len() {
            let allocation_id = format!("allocation_sponsor_failure_{fixture}_{allocation_index}");
            let source_id = format!("source_sponsor_failure_{fixture}_{allocation_index}");
            sqlx::query("DELETE FROM cloud_provider_allocations WHERE allocation_id = $1")
                .bind(allocation_id)
                .execute(&pool)
                .await
                .expect("remove test allocation");
            sqlx::query("DELETE FROM cloud_coverage_sources WHERE source_id = $1")
                .bind(source_id)
                .execute(&pool)
                .await
                .expect("remove test source");
        }
        sqlx::query("DELETE FROM cloud_coverage_coordinators WHERE beneficiary_id = ANY($1)")
            .bind(beneficiary_ids.to_vec())
            .execute(&pool)
            .await
            .expect("remove test coordinators");
        sqlx::query("DELETE FROM cloud_provider_payers WHERE payer_id = $1")
            .bind(&payer_id)
            .execute(&pool)
            .await
            .expect("remove test payer");
        sqlx::query("DELETE FROM billing_sponsored_subscriptions WHERE organization_id = $1")
            .bind(&organization_id)
            .execute(&pool)
            .await
            .expect("remove test sponsored subscription");
        for table in [
            "cloud_coverage_heads",
            "cloud_coverage_revision_facts",
            "cloud_coverage_revisions",
        ] {
            sqlx::query(&format!(
                "DELETE FROM {table} WHERE beneficiary_id = ANY($1)"
            ))
            .bind(beneficiary_ids.to_vec())
            .execute(&pool)
            .await
            .expect("remove test coverage projections");
        }
        sqlx::query("DELETE FROM organizations WHERE id = $1")
            .bind(&organization_id)
            .execute(&pool)
            .await
            .expect("remove test organisation");
        for user_id in std::iter::once(&administrator_id)
            .chain(additional_administrator_ids.iter())
            .chain(beneficiary_ids.iter())
        {
            sqlx::query("DELETE FROM users WHERE id = $1")
                .bind(user_id)
                .execute(&pool)
                .await
                .expect("remove test user");
        }
    }

    #[tokio::test]
    async fn transfer_invoice_webhook_ignores_later_renewals_and_reviews_wrong_boundary_start() {
        if std::env::var("SOTTO_RUN_DB_TESTS").as_deref() != Ok("1") {
            return;
        }
        let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL is required");
        let options =
            sqlx::postgres::PgConnectOptions::from_str(&database_url).expect("parse DATABASE_URL");
        assert!(
            matches!(options.get_host(), "localhost" | "127.0.0.1" | "::1"),
            "refusing to write test fixtures to non-local database: {}",
            options.get_host()
        );
        let pool = crate::db::connect(&database_url)
            .await
            .expect("connect test database");
        crate::db::migrate(&pool)
            .await
            .expect("migrate test database");

        let fixture = Uuid::new_v4().simple().to_string();
        let user_id = format!("transfer_invoice_{fixture}");
        let organization_id = format!("transfer_invoice_org_{fixture}");
        let completed_id = format!("transfer_invoice_completed_{fixture}");
        let prepared_id = format!("transfer_invoice_prepared_{fixture}");
        let restoration_id = format!("transfer_invoice_restoration_{fixture}");
        let completed_operation = format!("{completed_id}:destination");
        let prepared_operation = format!("{prepared_id}:destination");
        let restoration_operation = format!("{restoration_id}:source-restoration");
        let boundary = 1_800_000_000_i64;
        let expected_end = 1_802_592_000_i64;

        sqlx::query(
            "INSERT INTO users (id, oauth_provider, oauth_subject) VALUES ($1, 'transfer_invoice_test', $1)",
        )
        .bind(&user_id)
        .execute(&pool)
        .await
        .expect("insert transfer invoice beneficiary");
        sqlx::query(
            "INSERT INTO organizations (id, enc_name, created_by) VALUES ($1, decode('6f7267', 'hex'), $2)",
        )
        .bind(&organization_id)
        .bind(&user_id)
        .execute(&pool)
        .await
        .expect("insert transfer invoice destination organisation");
        sqlx::query(
            "INSERT INTO billing_operations \
             (operation_id, idempotency_key, request_hash, actor_user_id, payer_id, beneficiary_id, \
              offer, quote_version, quote_expires_at_epoch, provider_idempotency_key) \
             VALUES ($1, $1, $1, $2, $2, $2, 'standard_monthly', 1, $3, $1)",
        )
        .bind(&restoration_operation)
        .bind(&user_id)
        .bind(boundary + 900)
        .execute(&pool)
        .await
        .expect("insert restoration billing operation");
        sqlx::query(
            "INSERT INTO billing_personal_accounts \
             (user_id, operation_id, offer, stripe_customer_id, stripe_subscription_id, state, pending_expires_at_epoch) \
             VALUES ($1, $2, 'standard_monthly', 'cus_restored', 'sub_restored', 'active', $3)",
        )
        .bind(&user_id)
        .bind(&restoration_operation)
        .bind(boundary + 900)
        .execute(&pool)
        .await
        .expect("insert restored personal account");
        for (transfer_id, operation_id, state, result_code, payment, period_end) in [
            (
                &completed_id,
                &completed_operation,
                "completed",
                Some("transfer_completed"),
                Some("pi_transfer_boundary"),
                Some(expected_end),
            ),
            (
                &prepared_id,
                &prepared_operation,
                "destination_prepared",
                None,
                None,
                None,
            ),
        ] {
            sqlx::query(
                "INSERT INTO billing_transfer_intents \
                 (transfer_id, actor_user_id, counterparty_user_id, beneficiary_id, source_kind, \
                  destination_kind, destination_organization_id, offer, quote_version, \
                  quote_expires_at_epoch, consent_expires_at_epoch, effective_from, idempotency_key, request_hash, \
                  provider_idempotency_key, destination_operation_id, source_provider_subscription_id, \
                  destination_provider_subscription_id, destination_customer_id, state, result_code, \
                  destination_payment_reference, destination_period_end, destination_expected_period_end) \
                 VALUES ($1, $2, $2, $2, 'personal', 'sponsor', $3, 'standard_monthly', 1, \
                         $4, $4, $5, $6, $6, $6, $7, 'sub_source', 'sub_destination', 'cus_destination', \
                         $8, $9, $10, $11, $12)",
            )
            .bind(transfer_id)
            .bind(&user_id)
            .bind(&organization_id)
            .bind(boundary + 900)
            .bind(boundary)
            .bind(format!("key_{transfer_id}"))
            .bind(operation_id)
            .bind(state)
            .bind(result_code)
            .bind(payment)
            .bind(period_end)
            .bind(expected_end)
            .execute(&pool)
            .await
            .expect("insert transfer invoice fixture");
        }
        sqlx::query(
            "INSERT INTO billing_transfer_intents \
             (transfer_id, actor_user_id, counterparty_user_id, beneficiary_id, source_kind, \
              destination_kind, destination_organization_id, offer, quote_version, \
              quote_expires_at_epoch, consent_expires_at_epoch, effective_from, idempotency_key, request_hash, \
              provider_idempotency_key, source_provider_subscription_id, state, result_code, \
              source_restoration_operation_id, source_restoration_provider_subscription_id, \
              source_restoration_payment_reference) \
             VALUES ($1, $2, $2, $2, 'personal', 'sponsor', $3, 'standard_monthly', 1, \
                     $4, $4, $5, $6, $6, $6, 'sub_source', 'failed', 'destination_initial_payment_failed', \
                     $7, 'sub_restored', 'pi_restoration_boundary')",
        )
        .bind(&restoration_id)
        .bind(&user_id)
        .bind(&organization_id)
        .bind(boundary + 900)
        .bind(boundary)
        .bind(format!("key_{restoration_id}"))
        .bind(&restoration_operation)
        .execute(&pool)
        .await
        .expect("insert restored transfer fixture");

        let mut tx = pool.begin().await.expect("begin later renewal webhook");
        record_transfer_invoice_payment(
            &mut tx,
            &completed_operation,
            &user_id,
            TransferPayer::Sponsor,
            Some(&organization_id),
            "standard_monthly",
            "cus_destination",
            "sub_destination",
            "pi_second_renewal",
            expected_end,
            expected_end + 2_678_400,
        )
        .await
        .expect("a renewal after transfer completion is ordinary invoice billing");
        tx.commit().await.expect("commit later renewal webhook");
        let completed_payment: String = sqlx::query_scalar(
            "SELECT destination_payment_reference FROM billing_transfer_intents WHERE transfer_id = $1",
        )
        .bind(&completed_id)
        .fetch_one(&pool)
        .await
        .expect("read completed transfer payment");
        assert_eq!(completed_payment, "pi_transfer_boundary");

        let mut tx = pool
            .begin()
            .await
            .expect("begin restored source renewal webhook");
        record_transfer_invoice_payment(
            &mut tx,
            &restoration_operation,
            &user_id,
            TransferPayer::Personal,
            None,
            "standard_monthly",
            "cus_restored",
            "sub_restored",
            "pi_restoration_second_renewal",
            expected_end,
            expected_end + 2_678_400,
        )
        .await
        .expect("a renewal after restoration settlement is ordinary invoice billing");
        tx.commit()
            .await
            .expect("commit restored source renewal webhook");
        let restoration_payment: String = sqlx::query_scalar(
            "SELECT source_restoration_payment_reference FROM billing_transfer_intents WHERE transfer_id = $1",
        )
        .bind(&restoration_id)
        .fetch_one(&pool)
        .await
        .expect("read restoration boundary payment");
        assert_eq!(restoration_payment, "pi_restoration_boundary");

        let mut tx = pool.begin().await.expect("begin wrong-boundary webhook");
        record_transfer_invoice_payment(
            &mut tx,
            &prepared_operation,
            &user_id,
            TransferPayer::Sponsor,
            Some(&organization_id),
            "standard_monthly",
            "cus_destination",
            "sub_destination",
            "pi_wrong_boundary",
            boundary + 1,
            expected_end + 1,
        )
        .await
        .expect("a mismatched boundary start is retained for manual review");
        tx.commit().await.expect("commit wrong-boundary webhook");
        let review_code: Option<String> = sqlx::query_scalar(
            "SELECT last_execution_error_code FROM billing_transfer_intents WHERE transfer_id = $1",
        )
        .bind(&prepared_id)
        .fetch_one(&pool)
        .await
        .expect("read mismatch review code");
        assert_eq!(
            review_code.as_deref(),
            Some("destination_period_start_mismatch")
        );

        sqlx::query(
            "UPDATE billing_transfer_intents SET state = 'unknown', result_code = 'provider_outcome_unknown', \
                 destination_payment_reference = NULL, destination_period_start = NULL, destination_period_end = NULL, \
                 execution_manual_review_required = FALSE, last_execution_error_code = 'provider_outcome_unknown', \
                 execution_next_attempt_at = now() WHERE transfer_id = $1",
        )
        .bind(&prepared_id)
        .execute(&pool)
        .await
        .expect("park the prepared transfer in unknown for provider review");
        let mut tx = pool
            .begin()
            .await
            .expect("begin invoice for unknown transfer");
        record_transfer_invoice_payment(
            &mut tx,
            &prepared_operation,
            &user_id,
            TransferPayer::Sponsor,
            Some(&organization_id),
            "standard_monthly",
            "cus_destination",
            "sub_destination",
            "pi_unknown_destination",
            boundary,
            expected_end,
        )
        .await
        .expect("an invoice for an unknown transfer must be retained for review");
        tx.commit()
            .await
            .expect("commit paid invoice evidence for unknown transfer");
        let unknown: (String, bool, Option<String>, Option<String>, bool) = sqlx::query_as(
            "SELECT state, execution_manual_review_required, last_execution_error_code, \
                    destination_payment_reference, execution_next_attempt_at = 'infinity'::timestamptz \
             FROM billing_transfer_intents WHERE transfer_id = $1",
        )
        .bind(&prepared_id)
        .fetch_one(&pool)
        .await
        .expect("read unknown transfer evidence");
        assert_eq!(unknown.0, "unknown", "the evidence must not move the state");
        assert!(unknown.1);
        assert_eq!(
            unknown.2.as_deref(),
            Some("destination_invoice_received_while_unknown")
        );
        assert_eq!(unknown.3.as_deref(), Some("pi_unknown_destination"));
        assert!(
            unknown.4,
            "manual review remains parked until an operator acts"
        );

        sqlx::query("DELETE FROM billing_transfer_intents WHERE transfer_id = ANY($1)")
            .bind(vec![completed_id, prepared_id, restoration_id])
            .execute(&pool)
            .await
            .expect("delete transfer invoice fixtures");
        sqlx::query("DELETE FROM billing_personal_accounts WHERE user_id = $1")
            .bind(&user_id)
            .execute(&pool)
            .await
            .expect("delete restored personal account");
        sqlx::query("DELETE FROM billing_operations WHERE operation_id = $1")
            .bind(&restoration_operation)
            .execute(&pool)
            .await
            .expect("delete restoration billing operation");
        sqlx::query("DELETE FROM organizations WHERE id = $1")
            .bind(&organization_id)
            .execute(&pool)
            .await
            .expect("delete transfer invoice organisation");
        sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(&user_id)
            .execute(&pool)
            .await
            .expect("delete transfer invoice beneficiary");
    }

    #[tokio::test]
    async fn transfer_personal_cancellation_uses_and_verifies_the_quoted_boundary() {
        let (origin, requests, server) = cancellation_http_server().await;
        let provider = StripeBilling {
            api_key: "rk_test_mock".into(),
            price_id: "price_test".into(),
            api_origin: origin,
        };

        let result = provider
            .cancel_personal_subscription_at_boundary(
                "sub_boundary_direct",
                1_707_776_000,
                "transfer-source",
                "beneficiary",
            )
            .await
            .expect("cancellation is read back at the accepted boundary");
        assert_eq!(result.cancel_at, Some(1_707_776_000));
        let requests = requests.lock().expect("request log lock").clone();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].0, "GET");
        assert_eq!(requests[1].0, "POST");
        assert!(requests[1].2.contains("cancel_at=1707776000"));
        assert_eq!(requests[2].0, "GET");
        server.abort();
    }

    #[tokio::test]
    async fn restoration_lookup_finds_only_the_metadata_bound_subscription() {
        let (origin, requests, server) = cancellation_http_server().await;
        let provider = StripeBilling {
            api_key: "rk_test_mock".into(),
            price_id: "price_test".into(),
            api_origin: origin,
        };
        let found = provider
            .find_personal_source_restoration_subscription(
                "transfer:one:source-restoration",
                "transfer:one",
                "user_restore",
                "cus_original",
                "pm_saved",
                "price_standard_monthly",
                "alloc_restore",
            )
            .await
            .expect("search succeeds")
            .expect("metadata returns the restored subscription");
        assert_eq!(found.subscription_id, "sub_restored");
        assert_eq!(found.status, SubscriptionStatus::Incomplete);
        assert_eq!(
            requests.lock().expect("request log lock")[0].1,
            "/v1/subscriptions/search"
        );
        server.abort();
    }

    #[tokio::test]
    async fn transfer_personal_cancellation_refuses_a_changed_boundary_before_write() {
        let (origin, requests, server) = cancellation_http_server().await;
        let provider = StripeBilling {
            api_key: "rk_test_mock".into(),
            price_id: "price_test".into(),
            api_origin: origin,
        };

        let result = provider
            .cancel_personal_subscription_at_boundary(
                "sub_boundary_direct",
                1_707_776_001,
                "transfer-source",
                "beneficiary",
            )
            .await;
        assert!(result.is_err());
        let requests = requests.lock().expect("request log lock").clone();
        assert_eq!(
            requests.len(),
            1,
            "period drift must fail before a provider write"
        );
        assert_eq!(requests[0].0, "GET");
        server.abort();
    }

    #[tokio::test]
    async fn transfer_personal_schedule_cancellation_ends_at_the_quoted_boundary() {
        let (origin, requests, server) = cancellation_http_server().await;
        let provider = StripeBilling {
            api_key: "rk_test_mock".into(),
            price_id: "price_test".into(),
            api_origin: origin,
        };

        let result = provider
            .cancel_personal_subscription_at_boundary(
                "sub_scheduled",
                1_707_776_000,
                "transfer-source",
                "beneficiary",
            )
            .await
            .expect("scheduled source cancellation is read back");
        assert_eq!(result.current_period_end, Some(1_707_776_000));
        assert_eq!(result.cancel_at, Some(1_707_776_000));
        let requests = requests.lock().expect("request log lock").clone();
        let schedule_update = requests
            .iter()
            .find(|(method, path, _)| {
                method == "POST" && path == "/v1/subscription_schedules/sched_test"
            })
            .expect("schedule update request");
        assert!(schedule_update.2.contains("end_behavior=cancel"));
        assert!(schedule_update
            .2
            .contains("phases%5B0%5D%5Bend_date%5D=1707776000"));
        assert!(schedule_update.2.contains("proration_behavior=none"));
        server.abort();
    }

    #[test]
    fn malformed_paid_invoice_shape_becomes_an_unresolved_repair() {
        assert_eq!(
            paid_invoice_repair_shape_error(&serde_json::json!({"id": "in_1"}), None, true),
            Some("paid_invoice_settlement_shape")
        );
        assert_eq!(
            paid_invoice_repair_shape_error(
                &serde_json::json!({"id": "in_1", "period_end": -1}),
                None,
                true
            ),
            Some("paid_invoice_settlement_shape")
        );
        assert_eq!(
            paid_invoice_repair_shape_error(
                &serde_json::json!({"period_end": 1_950_000_000}),
                None,
                true
            ),
            Some("paid_invoice_settlement_shape")
        );
        assert_eq!(
            paid_invoice_repair_shape_error(
                &serde_json::json!({"id": "in_1"}),
                Some(1_950_000_000),
                true
            ),
            None
        );
        assert_eq!(
            paid_invoice_repair_shape_error(
                &serde_json::json!({"id": "in_1", "period_end": 1_950_000_000}),
                None,
                true
            ),
            None
        );
        assert_eq!(
            paid_invoice_repair_shape_error(&serde_json::json!({"id": "in_1"}), None, false),
            None
        );
    }

    #[tokio::test]
    async fn completed_invoice_repairs_skip_reads_without_skipping_unhandled_failures() {
        if std::env::var("SOTTO_RUN_DB_TESTS").as_deref() != Ok("1") {
            return;
        }
        let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL is required");
        let options =
            sqlx::postgres::PgConnectOptions::from_str(&database_url).expect("parse DATABASE_URL");
        assert!(
            matches!(options.get_host(), "localhost" | "127.0.0.1" | "::1"),
            "refusing to write test fixtures to non-local database: {}",
            options.get_host()
        );
        let pool = crate::db::connect(&database_url)
            .await
            .expect("connect test database");
        crate::db::migrate(&pool)
            .await
            .expect("migrate test database");

        let fixture = Uuid::new_v4().simple().to_string();
        let paid_event_id = format!("evt_paid_repair_{fixture}");
        let failed_event_id = format!("evt_failed_repair_{fixture}");
        let account_id = format!("acct_repair_{fixture}");
        for event_id in [&paid_event_id, &failed_event_id] {
            sqlx::query(
                "INSERT INTO stripe_webhook_events \
                 (event_id, event_type, api_version, stripe_created, subscription_id, processed_at) \
                 VALUES ($1, $2, $3, 1, $4, now())",
            )
            .bind(event_id)
            .bind(if event_id == &paid_event_id {
                "invoice.paid"
            } else {
                "invoice.payment_failed"
            })
            .bind(STRIPE_API_VERSION)
            .bind("sub_repair_fixture")
            .execute(&pool)
            .await
            .expect("insert processed webhook event");
        }
        sqlx::query(
            "INSERT INTO cloud_provider_event_receipts \
             (provider_namespace, provider_account_id, provider_environment, event_id, event_type, \
              provider_created_at, allocation_reference, normalized_payload_hash, status, \
              allocation_id, coverage_source_id, projection_revision, processed_at) \
             VALUES ('stripe', $1, 'test', $2, 'invoice.paid', 1, 'allocation_fixture', $3, \
                     'applied', 'allocation_fixture', 'source_fixture', 1, now())",
        )
        .bind(&account_id)
        .bind(&paid_event_id)
        .bind("a".repeat(64))
        .execute(&pool)
        .await
        .expect("insert completed coverage receipt");
        sqlx::query(
            "INSERT INTO cloud_provider_event_scopes \
             (provider_namespace, provider_account_id, provider_environment, event_id, beneficiary_id) \
             VALUES ('stripe', $1, 'test', $2, $3)",
        )
        .bind(&account_id)
        .bind(&paid_event_id)
        .bind(format!("user_repair_{fixture}"))
        .execute(&pool)
        .await
        .expect("insert pending coverage scope");

        let paid = Event {
            id: paid_event_id.clone(),
            created: 1,
            api_version: Some(STRIPE_API_VERSION.into()),
            kind: "invoice.paid".into(),
            data: EventData {
                object: serde_json::json!({}),
            },
        };
        let failed = Event {
            id: failed_event_id.clone(),
            created: 1,
            api_version: Some(STRIPE_API_VERSION.into()),
            kind: "invoice.payment_failed".into(),
            data: EventData {
                object: serde_json::json!({}),
            },
        };
        assert!(repair_event_already_completed(
            &pool,
            &account_id,
            ProviderEnvironment::Test,
            &paid
        )
        .await
        .expect("check completed paid event"));
        let rejection_fingerprint = "b".repeat(64);
        sqlx::query(
            "INSERT INTO cloud_coverage_rejection_incidents \
             (fingerprint, provider_account_id, provider_environment, event_id, event_type, \
              coverage_kind, reason_code) \
             VALUES ($1, $2, 'test', $3, 'invoice.paid', 'personal', 'invoice_price_mismatch')",
        )
        .bind(&rejection_fingerprint)
        .bind(&account_id)
        .bind(&paid_event_id)
        .execute(&pool)
        .await
        .expect("insert open coverage rejection");
        assert!(!repair_event_already_completed(
            &pool,
            &account_id,
            ProviderEnvironment::Test,
            &paid
        )
        .await
        .expect("open diagnostic must make the event repairable"));
        assert!(!repair_event_already_completed(
            &pool,
            &account_id,
            ProviderEnvironment::Test,
            &failed
        )
        .await
        .expect("check webhook-acknowledged failure event"));

        sqlx::query(
            "DELETE FROM cloud_provider_event_receipts \
             WHERE event_id = ANY($1)",
        )
        .bind(vec![paid_event_id.clone()])
        .execute(&pool)
        .await
        .expect("remove coverage fixture");
        sqlx::query("DELETE FROM cloud_coverage_rejection_incidents WHERE fingerprint = $1")
            .bind(rejection_fingerprint)
            .execute(&pool)
            .await
            .expect("remove rejection fixture");
        sqlx::query("DELETE FROM stripe_webhook_events WHERE event_id = ANY($1)")
            .bind(vec![paid_event_id, failed_event_id])
            .execute(&pool)
            .await
            .expect("remove webhook fixtures");
    }

    #[tokio::test]
    async fn repaired_paid_invoice_advances_subscription_event_watermark() {
        if std::env::var("SOTTO_RUN_DB_TESTS").as_deref() != Ok("1") {
            return;
        }
        let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL is required");
        let options =
            sqlx::postgres::PgConnectOptions::from_str(&database_url).expect("parse DATABASE_URL");
        assert!(
            matches!(options.get_host(), "localhost" | "127.0.0.1" | "::1"),
            "refusing to write test fixtures to non-local database: {}",
            options.get_host()
        );
        let pool = crate::db::connect(&database_url)
            .await
            .expect("connect test database");
        crate::db::migrate(&pool)
            .await
            .expect("migrate test database");

        let fixture = Uuid::new_v4().simple().to_string();
        let subscription_id = format!("sub_repaired_invoice_{fixture}");
        let paid = Event {
            id: format!("evt_paid_repair_watermark_{fixture}"),
            created: 100,
            api_version: Some(STRIPE_API_VERSION.into()),
            kind: "invoice.paid".into(),
            data: EventData {
                object: serde_json::json!({
                    "parent": {"subscription_details": {"subscription": subscription_id}}
                }),
            },
        };
        let mut tx = pool
            .begin()
            .await
            .expect("begin repaired event transaction");
        record_webhook_receipt(&mut tx, &paid, Some(&subscription_id))
            .await
            .expect("persist paid webhook receipt");
        complete_repaired_invoice_event(&mut tx, &paid)
            .await
            .expect("advance watermark and complete paid event");
        tx.commit().await.expect("commit repaired paid event");

        let older = Event {
            id: format!("evt_older_subscription_update_{fixture}"),
            created: 99,
            api_version: Some(STRIPE_API_VERSION.into()),
            kind: "customer.subscription.updated".into(),
            data: EventData {
                object: serde_json::json!({"id": subscription_id}),
            },
        };
        let mut tx = pool.begin().await.expect("begin older webhook transaction");
        assert_eq!(
            record_webhook_event(&mut tx, &older)
                .await
                .expect("compare older event with repaired watermark"),
            EventDisposition::Ignore
        );
        tx.commit().await.expect("commit older event receipt");

        let watermark: (i64, String) = sqlx::query_as(
            "SELECT stripe_created, event_id FROM stripe_subscription_watermarks \
             WHERE subscription_id = $1",
        )
        .bind(&subscription_id)
        .fetch_one(&pool)
        .await
        .expect("repaired paid event remains the watermark");
        assert_eq!(watermark.0, paid.created);
        assert_eq!(watermark.1, paid.id);

        sqlx::query("DELETE FROM stripe_subscription_watermarks WHERE subscription_id = $1")
            .bind(&subscription_id)
            .execute(&pool)
            .await
            .expect("remove watermark fixture");
        sqlx::query(
            "DELETE FROM stripe_webhook_events WHERE subscription_id = $1 OR event_id = $2",
        )
        .bind(&subscription_id)
        .bind(&older.id)
        .execute(&pool)
        .await
        .expect("remove webhook fixtures");
    }

    #[tokio::test]
    async fn authenticated_paid_repair_settles_once_and_duplicate_is_harmless() {
        if std::env::var("SOTTO_RUN_DB_TESTS").as_deref() != Ok("1") {
            return;
        }
        let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL is required");
        let options =
            sqlx::postgres::PgConnectOptions::from_str(&database_url).expect("parse DATABASE_URL");
        assert!(
            matches!(options.get_host(), "localhost" | "127.0.0.1" | "::1"),
            "refusing to write test fixtures to non-local database: {}",
            options.get_host()
        );
        let pool = crate::db::connect(&database_url)
            .await
            .expect("connect test database");
        crate::db::migrate(&pool)
            .await
            .expect("migrate test database");

        let fixture = Uuid::new_v4().simple().to_string();
        let user_id = format!("user_repair_paid_{fixture}");
        let operation_id = format!("op_repair_paid_{fixture}");
        let subscription_id = format!("sub_repair_paid_{fixture}");
        let event_id = format!("evt_repair_paid_{fixture}");
        let invoice_id = format!("in_repair_paid_{fixture}");
        sqlx::query(
            "INSERT INTO users (id, oauth_provider, oauth_subject) VALUES ($1, 'github', $1)",
        )
        .bind(&user_id)
        .execute(&pool)
        .await
        .expect("seed personal billing user");
        sqlx::query(
            "INSERT INTO billing_operations \
             (operation_id, idempotency_key, request_hash, actor_user_id, payer_id, beneficiary_id, \
              offer, quote_version, quote_expires_at_epoch, provider_idempotency_key) \
             VALUES ($1, $2, $3, $4, $4, $4, 'standard_monthly', 1, 1, $5)",
        )
        .bind(&operation_id)
        .bind(format!("key_{fixture}"))
        .bind("a".repeat(64))
        .bind(&user_id)
        .bind(format!("provider_key_{fixture}"))
        .execute(&pool)
        .await
        .expect("seed billing operation");
        sqlx::query(
            "INSERT INTO billing_personal_accounts \
             (user_id, operation_id, offer, stripe_subscription_id, pending_expires_at_epoch, state) \
             VALUES ($1, $2, 'standard_monthly', $3, 1, 'active')",
        )
        .bind(&user_id)
        .bind(&operation_id)
        .bind(&subscription_id)
        .execute(&pool)
        .await
        .expect("seed active personal billing subscription");

        let event_created = 1_950_000_000;
        let invoice = serde_json::json!({
            "id": invoice_id,
            "payment_intent": format!("pi_repair_paid_{fixture}"),
            "period_end": 1_980_000_000,
            "parent": {"subscription_details": {"subscription": subscription_id}}
        });
        let authenticated = AuthenticatedStripeEvent::for_test(
            &event_id,
            "acct_repair_test",
            ProviderEnvironment::Test,
            serde_json::json!({
                "id": event_id,
                "created": event_created,
                "api_version": STRIPE_API_VERSION,
                "type": "invoice.paid",
                "data": {"object": invoice}
            }),
        );
        let billing = BillingState::from_config(BillingConfig {
            api_key: "sk_test_repair".into(),
            webhook_secret: "whsec_repair".into(),
            price_id: "price_legacy".into(),
            price_catalogue: None,
            cloud_portal_configuration_id: None,
            cloud_sales_enabled: false,
            return_url: "https://getsotto.test".into(),
        });

        assert_eq!(
            repair_authenticated_stripe_event(&pool, &billing, &authenticated, event_created)
                .await
                .expect("repair paid event through billing handler"),
            StripeEventRepairDisposition::Applied
        );
        assert_eq!(
            repair_authenticated_stripe_event(&pool, &billing, &authenticated, event_created)
                .await
                .expect("replay the already processed paid event"),
            StripeEventRepairDisposition::AlreadyApplied
        );
        let account: (String, i64, String) = sqlx::query_as(
            "SELECT payment_reference, paid_through_epoch, paid_through_date \
             FROM billing_personal_accounts WHERE user_id = $1",
        )
        .bind(&user_id)
        .fetch_one(&pool)
        .await
        .expect("read repaired billing settlement");
        assert_eq!(account.0, format!("pi_repair_paid_{fixture}"));
        assert_eq!(account.1, 1_980_000_000);
        assert_eq!(
            account.2,
            FoundingDate::from_unix_seconds(1_980_000_000)
                .unwrap()
                .to_string()
        );
        let watermark: (i64, String) = sqlx::query_as(
            "SELECT stripe_created, event_id FROM stripe_subscription_watermarks \
             WHERE subscription_id = $1",
        )
        .bind(&subscription_id)
        .fetch_one(&pool)
        .await
        .expect("read repaired event watermark");
        assert_eq!(watermark, (event_created, event_id.clone()));

        sqlx::query("DELETE FROM stripe_subscription_watermarks WHERE subscription_id = $1")
            .bind(&subscription_id)
            .execute(&pool)
            .await
            .expect("remove watermark fixture");
        sqlx::query("DELETE FROM stripe_webhook_events WHERE event_id = $1")
            .bind(&event_id)
            .execute(&pool)
            .await
            .expect("remove webhook fixture");
        sqlx::query("DELETE FROM billing_personal_accounts WHERE user_id = $1")
            .bind(&user_id)
            .execute(&pool)
            .await
            .expect("remove personal billing fixture");
        sqlx::query("DELETE FROM billing_operations WHERE operation_id = $1")
            .bind(&operation_id)
            .execute(&pool)
            .await
            .expect("remove operation fixture");
        sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(&user_id)
            .execute(&pool)
            .await
            .expect("remove user fixture");
    }

    #[test]
    fn sales_gate_blocks_new_purchase_actions_without_blocking_billing_config() {
        assert!(matches!(
            ensure_cloud_sales_enabled(false),
            Err(Error::NotConfigured(_))
        ));
        assert!(ensure_cloud_sales_enabled(true).is_ok());
    }

    #[test]
    fn sales_pause_allows_only_replays_of_persisted_operations() {
        assert!(ensure_cloud_sales_for_operation(false, true).is_ok());
        assert!(matches!(
            ensure_cloud_sales_for_operation(false, false),
            Err(Error::NotConfigured(_))
        ));
        assert!(ensure_cloud_sales_for_operation(true, false).is_ok());
    }

    /// The vault app moved behind `/app` when the site root became the marketing page; a payer
    /// must land back in the app, never on the landing page. This pins that contract.
    #[test]
    fn stripe_return_urls_target_the_vault_app() {
        let (success, cancel) = checkout_return_urls("https://getsotto.test");
        assert_eq!(success, "https://getsotto.test/app?billing=success");
        assert_eq!(cancel, "https://getsotto.test/app?billing=cancelled");
        // A configured base with a trailing slash must not produce a `//app` path.
        assert_eq!(
            app_url("https://getsotto.test/"),
            "https://getsotto.test/app"
        );
    }

    #[cfg(feature = "e2e-mock-billing")]
    #[tokio::test]
    async fn e2e_provider_builds_a_local_checkout_url() {
        let provider = E2eBilling {
            provider_origin: "http://127.0.0.1:8099/".into(),
        };
        let url = provider
            .create_checkout(
                "org-1",
                None,
                "http://127.0.0.1:5199/app?billing=success",
                "http://127.0.0.1:5199/app?billing=cancelled",
            )
            .await
            .unwrap();
        let parsed = Url::parse(&url).unwrap();
        assert_eq!(parsed.path(), "/e2e/billing/checkout");
        assert_eq!(
            parsed
                .query_pairs()
                .find(|(key, _)| key == "success_url")
                .unwrap()
                .1,
            "http://127.0.0.1:5199/app?billing=success"
        );
    }

    #[cfg(feature = "e2e-mock-billing")]
    #[tokio::test]
    async fn e2e_provider_builds_and_serves_a_local_portal_url() {
        let provider = E2eBilling {
            provider_origin: "http://127.0.0.1:8099/".into(),
        };
        let url = provider
            .create_portal("cus-test", "http://127.0.0.1:5199/app", None)
            .await
            .unwrap();
        let parsed = Url::parse(&url).unwrap();
        assert_eq!(parsed.path(), "/e2e/billing/portal");
        assert_eq!(
            parsed
                .query_pairs()
                .find(|(key, _)| key == "return_url")
                .unwrap()
                .1,
            "http://127.0.0.1:5199/app"
        );

        let Html(page) = e2e_portal(Query(E2ePortalQuery {
            return_url: "http://127.0.0.1:5199/app".into(),
        }))
        .await;
        assert!(page.contains("Test billing portal"));
        assert!(page.contains("Return to app"));
    }

    #[cfg(feature = "e2e-mock-billing")]
    #[test]
    fn e2e_provider_page_rejects_unsafe_link_schemes() {
        let page = e2e_provider_page(
            "Test checkout",
            "Complete payment",
            "javascript:alert(1)",
            "Cancel payment",
            "data:text/html,unsafe",
        );
        assert!(page.contains("href=\"#\""));
        assert!(!page.contains("javascript:"));
        assert!(!page.contains("data:text"));
    }

    fn sign(secret: &str, t: i64, payload: &str) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(format!("{t}.{payload}").as_bytes());
        mac.finalize()
            .into_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    #[test]
    fn valid_signature_passes() {
        let header = format!("t=1000,v1={}", sign("whsec_x", 1000, "{}"));
        assert!(verify_signature("whsec_x", &header, "{}", 1000));
    }

    #[test]
    fn wrong_secret_or_tampered_payload_fails() {
        let header = format!("t=1000,v1={}", sign("whsec_x", 1000, "{}"));
        assert!(!verify_signature("whsec_other", &header, "{}", 1000));
        assert!(!verify_signature("whsec_x", &header, "{\"a\":1}", 1000));
    }

    #[test]
    fn stale_or_future_timestamp_fails() {
        let header = format!("t=1000,v1={}", sign("whsec_x", 1000, "{}"));
        assert!(!verify_signature("whsec_x", &header, "{}", 1000 + 301));
        assert!(!verify_signature("whsec_x", &header, "{}", 1000 - 301));
        // ...but anything inside the tolerance passes.
        assert!(verify_signature("whsec_x", &header, "{}", 1000 + 300));
    }

    #[test]
    fn extreme_timestamp_difference_fails_without_overflow() {
        let timestamp = i64::MIN;
        let header = format!("t={timestamp},v1={}", sign("whsec_x", timestamp, "{}"));
        assert!(!verify_signature("whsec_x", &header, "{}", 0));
    }

    #[test]
    fn any_valid_v1_among_several_passes() {
        let good = sign("whsec_x", 1000, "{}");
        let header = format!("t=1000,v1={},v1={good}", "ab".repeat(32));
        assert!(verify_signature("whsec_x", &header, "{}", 1000));
    }

    #[test]
    fn malformed_headers_fail_closed() {
        assert!(!verify_signature("whsec_x", "", "{}", 1000));
        assert!(!verify_signature(
            "whsec_x",
            "t=notanumber,v1=ab",
            "{}",
            1000
        ));
        assert!(!verify_signature("whsec_x", "v1=abcd", "{}", 1000)); // no timestamp
        let header = format!("t=1000,v1={}", "zz".repeat(32)); // non-hex
        assert!(!verify_signature("whsec_x", &header, "{}", 1000));
    }

    #[test]
    fn subscription_statuses_use_the_deletion_gate_not_entitlement_status() {
        let statuses = [
            ("active", PurgeGate::Blocking),
            ("trialing", PurgeGate::Blocking),
            ("past_due", PurgeGate::Blocking),
            ("incomplete", PurgeGate::Blocking),
            ("paused", PurgeGate::Blocking),
            ("unpaid", PurgeGate::Blocking),
            ("canceled", PurgeGate::Terminal),
            ("incomplete_expired", PurgeGate::Terminal),
            ("future_status", PurgeGate::Unknown),
        ];
        for (status, expected) in statuses {
            assert_eq!(SubscriptionStatus::parse(status).purge_gate(), expected);
        }
        assert_eq!(
            SubscriptionStatus::parse("unpaid").entitlement_tier(),
            "free"
        );
        assert_eq!(
            SubscriptionStatus::parse("past_due").entitlement_tier(),
            "team"
        );
    }

    #[test]
    fn sponsored_subscription_terminal_states_clear_the_provider_link() {
        assert_eq!(
            sponsored_subscription_status(SubscriptionStatus::Active),
            Some(("active", false))
        );
        assert_eq!(
            sponsored_subscription_status(SubscriptionStatus::PastDue),
            Some(("past_due", false))
        );
        assert_eq!(
            sponsored_subscription_status(SubscriptionStatus::Unpaid),
            Some(("canceled", true))
        );
        assert_eq!(
            sponsored_subscription_status(SubscriptionStatus::Canceled),
            Some(("canceled", true))
        );
    }

    #[test]
    fn provider_errors_keep_status_and_code_for_retry_policy() {
        let cases = [
            (401, None, ProviderErrorKind::Authentication),
            (
                403,
                Some("invalid_api_key"),
                ProviderErrorKind::Authentication,
            ),
            (
                404,
                Some("resource_missing"),
                ProviderErrorKind::ResourceMissing,
            ),
            (429, Some("rate_limit_error"), ProviderErrorKind::Retryable),
            (500, Some("api_error"), ProviderErrorKind::Retryable),
            (
                400,
                Some("invalid_request_error"),
                ProviderErrorKind::Unknown,
            ),
        ];
        for (status, code, kind) in cases {
            let error = ProviderError::http(status, code.map(str::to_string));
            assert_eq!(error.status, Some(status));
            assert_eq!(error.kind, kind);
        }
        assert_eq!(
            ProviderError::transport().kind,
            ProviderErrorKind::Retryable
        );
        assert!(provider_failure_is_definitive(&ProviderError::http(
            400,
            Some("invalid_request_error".into())
        )));
        assert!(provider_failure_is_definitive(&ProviderError::http(
            401,
            Some("invalid_api_key".into())
        )));
        assert!(!provider_failure_is_definitive(&ProviderError::http(
            409,
            Some("idempotency_key_in_use".into())
        )));
        assert!(!provider_failure_is_definitive(&ProviderError::http(
            500,
            Some("api_error".into())
        )));
        assert!(!provider_failure_is_definitive(&ProviderError::transport()));
        assert!(!provider_failure_is_definitive(
            &ProviderError::malformed_response()
        ));
    }

    #[test]
    fn personal_price_validation_rejects_shape_drift_and_accepts_restricted_live_keys() {
        let price = serde_json::json!({
            "id": "price_founder",
            "active": true,
            "livemode": true,
            "currency": "gbp",
            "unit_amount": 199,
            "recurring": {"interval": "month", "interval_count": 1, "usage_type": "licensed"}
        });
        assert!(validate_stripe_personal_price(
            &price,
            "price_founder",
            BillingOffer::FoundingMonthly,
            true
        )
        .is_ok());
        let mut wrong_amount = price.clone();
        wrong_amount["unit_amount"] = serde_json::json!(299);
        assert!(validate_stripe_personal_price(
            &wrong_amount,
            "price_founder",
            BillingOffer::FoundingMonthly,
            true
        )
        .is_err());
    }

    #[test]
    fn cloud_portal_requires_payment_updates_and_period_end_cancellation_without_plan_changes() {
        let configuration = serde_json::json!({
            "id": "bpc_cloud_safe",
            "object": "billing_portal.configuration",
            "active": true,
            "livemode": false,
            "features": {
                "payment_method_update": {"enabled": true},
                "subscription_cancel": {
                    "enabled": true,
                    "mode": "at_period_end",
                    "proration_behavior": "none"
                },
                "subscription_update": {"enabled": false}
            }
        });
        assert!(
            validate_cloud_portal_configuration(&configuration, "bpc_cloud_safe", false).is_ok()
        );

        for (path, invalid) in [
            ("active", serde_json::json!(false)),
            ("livemode", serde_json::json!(true)),
            (
                "features.subscription_update.enabled",
                serde_json::json!(true),
            ),
            (
                "features.payment_method_update.enabled",
                serde_json::json!(false),
            ),
            (
                "features.subscription_cancel.enabled",
                serde_json::json!(false),
            ),
            (
                "features.subscription_cancel.mode",
                serde_json::json!("immediately"),
            ),
            (
                "features.subscription_cancel.proration_behavior",
                serde_json::json!("always_invoice"),
            ),
        ] {
            let mut candidate = configuration.clone();
            let mut target = &mut candidate;
            let mut segments = path.split('.').peekable();
            while let Some(segment) = segments.next() {
                if segments.peek().is_none() {
                    target[segment] = invalid.clone();
                } else {
                    target = &mut target[segment];
                }
            }
            assert!(
                validate_cloud_portal_configuration(&candidate, "bpc_cloud_safe", false).is_err(),
                "unsafe portal configuration accepted at {path}"
            );
        }
        assert!(validate_cloud_portal_configuration(&configuration, "bpc_other", false).is_err());
    }

    #[test]
    fn cloud_portal_session_uses_the_configured_safe_configuration() {
        let form = portal_session_form("cus_123", "https://app.sotto.test/cloud", Some("bpc_safe"));
        assert!(form.contains(&("configuration".into(), "bpc_safe".into())));
        assert!(form.contains(&("customer".into(), "cus_123".into())));
        assert!(form.contains(&("return_url".into(), "https://app.sotto.test/cloud".into())));
        assert!(
            !portal_session_form("cus_123", "https://app.sotto.test", None)
                .iter()
                .any(|(key, _)| key == "configuration")
        );
    }

    #[test]
    fn cancellation_form_is_explicit_and_traceable() {
        let form = cancellation_form("org-123");
        assert!(form.contains(&("invoice_now".into(), "false".into())));
        assert!(form.contains(&("prorate".into(), "false".into())));
        assert!(form.iter().any(|(key, value)| {
            key == "cancellation_details[comment]" && value.contains("org-123")
        }));
    }

    #[test]
    fn subscription_lookup_must_return_the_requested_id() {
        let object = serde_json::json!({"id": "sub-other", "status": "canceled"});
        let error = subscription_snapshot(&object, "sub-requested").unwrap_err();
        assert_eq!(error.kind, ProviderErrorKind::Unknown);
        assert_eq!(error.code.as_deref(), Some("malformed_response"));
    }

    #[test]
    fn stripe_requests_use_the_pinned_version_and_idempotency_key() {
        let headers = stripe_headers(Some("operation-123")).unwrap();
        assert_eq!(headers.get("Stripe-Version").unwrap(), STRIPE_API_VERSION);
        assert_eq!(headers.get("Idempotency-Key").unwrap(), "operation-123");
        assert!(stripe_headers(Some("bad\nkey")).is_err());
    }

    #[tokio::test]
    async fn sponsored_checkout_retry_reuses_the_exact_http_request_and_idempotency_key() {
        let (origin, requests, remote_creations, server) = sponsored_checkout_http_server().await;
        let provider = StripeBilling {
            api_key: "rk_test_mock".into(),
            price_id: "price_legacy_unused".into(),
            api_origin: origin,
        };
        let items = vec![SponsoredSubscriptionItemRequest {
            provider_price_id: "price_recovery_monthly".into(),
            quantity: 2,
        }];
        let first = provider
            .create_sponsored_checkout(
                "org_recovery",
                Some("cus_recovery"),
                &items,
                "sponsored:operation_recovery",
                "sotto-sponsored:stable-key",
                1_800_000_000,
                "https://app.example.test/success",
                "https://app.example.test/cancel",
            )
            .await;
        assert!(
            matches!(
                &first,
                Err(ProviderError {
                    kind: ProviderErrorKind::Retryable,
                    ..
                })
            ),
            "first provider response should be retryable: {first:?}"
        );

        let replay = provider
            .create_sponsored_checkout(
                "org_recovery",
                Some("cus_recovery"),
                &items,
                "sponsored:operation_recovery",
                "sotto-sponsored:stable-key",
                1_800_000_000,
                "https://app.example.test/success",
                "https://app.example.test/cancel",
            )
            .await
            .expect("same request succeeds after an ambiguous provider response");
        assert_eq!(replay.session_id, "cs_recovered_http");

        let requests = requests.lock().expect("read HTTP requests");
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].0.as_deref(), Some("sotto-sponsored:stable-key"));
        assert_eq!(requests[1].0.as_deref(), Some("sotto-sponsored:stable-key"));
        assert_eq!(requests[0].1, requests[1].1);
        assert!(requests[0]
            .1
            .contains("subscription_data%5Bmetadata%5D%5Bsponsored_operation_id%5D=sponsored%3Aoperation_recovery"));
        assert!(requests[0]
            .1
            .contains("line_items%5B0%5D%5Bprice%5D=price_recovery_monthly"));
        assert!(requests[0].1.contains("line_items%5B0%5D%5Bquantity%5D=2"));
        assert_eq!(
            remote_creations.load(Ordering::SeqCst),
            1,
            "the HTTP mock accepted only one remote Checkout despite losing the first response"
        );
        drop(requests);
        server.abort();
    }

    #[tokio::test]
    async fn sponsored_schedule_recovery_verifies_provider_binding_and_exact_saved_items_over_http()
    {
        let schedule = |subscription_id: &str, future_price: &str| {
            serde_json::json!({
                "id": "sched_recovery_http",
                "object": "subscription_schedule",
                "subscription": subscription_id,
                "end_behavior": "release",
                "current_phase": {
                    "start_date": 1_600_000_000_i64,
                    "end_date": 1_700_000_000_i64
                },
                "phases": [
                    {
                        "start_date": 1_600_000_000_i64,
                        "end_date": 1_700_000_000_i64,
                        "items": [{"price": "price_current_http", "quantity": 1}]
                    },
                    {
                        "start_date": 1_700_000_000_i64,
                        "end_date": 1_800_000_000_i64,
                        "items": [{"price": future_price, "quantity": 2}]
                    }
                ]
            })
        };
        let expected = [SponsoredSubscriptionPhase {
            start_at_epoch: 1_700_000_000,
            end_at_epoch: None,
            items: vec![SponsoredSubscriptionItemRequest {
                provider_price_id: "price_recovery_http".into(),
                quantity: 2,
            }],
        }];

        let (origin, requests, server) = sponsored_schedule_verification_http_server(schedule(
            "sub_recovery_http",
            "price_recovery_http",
        ))
        .await;
        let provider = StripeBilling {
            api_key: "rk_test_mock".into(),
            price_id: "price_legacy_unused".into(),
            api_origin: origin,
        };
        let verified = provider
            .verify_sponsored_subscription_update(
                "sub_recovery_http",
                1_600_000_000,
                1_700_000_000,
                &expected,
            )
            .await
            .expect("read and validate accepted schedule");
        assert!(verified.is_some());
        assert_eq!(
            requests
                .lock()
                .expect("read verification requests")
                .as_slice(),
            [
                "/v1/subscriptions/sub_recovery_http",
                "/v1/subscription_schedules/sched_recovery_http"
            ]
        );
        server.abort();

        let (origin, _, server) = sponsored_schedule_verification_http_server(schedule(
            "sub_recovery_http",
            "price_unexpected_http",
        ))
        .await;
        let provider = StripeBilling {
            api_key: "rk_test_mock".into(),
            price_id: "price_legacy_unused".into(),
            api_origin: origin,
        };
        let mismatched_item = provider
            .verify_sponsored_subscription_update(
                "sub_recovery_http",
                1_600_000_000,
                1_700_000_000,
                &expected,
            )
            .await
            .expect("a valid but different schedule is not a read error");
        assert!(mismatched_item.is_none());
        server.abort();

        let (origin, _, server) = sponsored_schedule_verification_http_server(schedule(
            "sub_other_http",
            "price_recovery_http",
        ))
        .await;
        let provider = StripeBilling {
            api_key: "rk_test_mock".into(),
            price_id: "price_legacy_unused".into(),
            api_origin: origin,
        };
        assert!(provider
            .verify_sponsored_subscription_update(
                "sub_recovery_http",
                1_600_000_000,
                1_700_000_000,
                &expected,
            )
            .await
            .is_err());
        server.abort();
    }

    #[test]
    fn transfer_setup_checkout_is_off_session_and_bound_to_transfer() {
        let form = transfer_setup_checkout_form(
            "transfer:abc",
            "cus_beneficiary",
            "https://app.example.test/success",
            "https://app.example.test/cancel",
        );
        let fields = form
            .into_iter()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(fields.get("mode").map(String::as_str), Some("setup"));
        assert_eq!(
            fields.get("customer").map(String::as_str),
            Some("cus_beneficiary")
        );
        assert_eq!(
            fields.get("setup_intent_data[usage]").map(String::as_str),
            Some("off_session")
        );
        assert_eq!(
            fields
                .get("setup_intent_data[metadata][sotto_transfer_id]")
                .map(String::as_str),
            Some("transfer:abc")
        );
        assert!(!fields.keys().any(|key| key == "payment_method_types"));
    }

    #[test]
    fn personal_source_restoration_charges_the_original_customer_idempotently() {
        let fields = personal_source_restoration_form(
            "transfer:abc",
            "user_123",
            "cus_original",
            "pm_saved",
            "price_standard_monthly",
            "alloc_restore",
            "transfer:abc:source-restoration",
        )
        .into_iter()
        .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(
            fields.get("customer").map(String::as_str),
            Some("cus_original")
        );
        assert_eq!(
            fields.get("default_payment_method").map(String::as_str),
            Some("pm_saved")
        );
        assert_eq!(
            fields.get("items[0][price]").map(String::as_str),
            Some("price_standard_monthly")
        );
        assert_eq!(
            fields.get("collection_method").map(String::as_str),
            Some("charge_automatically")
        );
        assert_eq!(
            fields.get("payment_behavior").map(String::as_str),
            Some("allow_incomplete")
        );
        assert_eq!(fields.get("off_session").map(String::as_str), Some("true"));
        assert_eq!(
            fields
                .get("metadata[sotto_transfer_id]")
                .map(String::as_str),
            Some("transfer:abc")
        );
        assert_eq!(
            fields.get("metadata[operation_id]").map(String::as_str),
            Some("transfer:abc:source-restoration")
        );
        assert_eq!(
            fields
                .get("metadata[sotto_allocation_reference]")
                .map(String::as_str),
            Some("alloc_restore")
        );
    }

    #[test]
    fn transfer_setup_response_requires_expected_customer_and_transfer_metadata() {
        let response = serde_json::json!({
            "id": "cs_test_transfer",
            "object": "checkout.session",
            "mode": "setup",
            "status": "open",
            "url": "https://checkout.stripe.com/c/pay/cs_test_transfer",
            "customer": "cus_transfer",
            "client_reference_id": "sotto-transfer:transfer:abc",
            "metadata": { "sotto_transfer_id": "transfer:abc" },
            "setup_intent": null,
            "expires_at": 1900000000
        });
        let session = transfer_setup_session_from_object(
            &response,
            Some("cs_test_transfer"),
            Some("transfer:abc"),
        )
        .expect("matching Setup Checkout response");
        assert_eq!(session.customer_id, "cus_transfer");
        assert_eq!(session.status, "open");
        assert!(session.checkout_url.is_some());
        assert_eq!(session.expires_at_epoch, Some(1_900_000_000));

        let mut forged = response;
        forged["metadata"]["sotto_transfer_id"] = serde_json::json!("transfer:other");
        assert!(transfer_setup_session_from_object(&forged, None, Some("transfer:abc")).is_err());
    }

    #[test]
    fn transfer_setup_intent_requires_an_off_session_payment_method() {
        let response = serde_json::json!({
            "id": "seti_transfer",
            "object": "setup_intent",
            "status": "succeeded",
            "usage": "off_session",
            "customer": "cus_transfer",
            "payment_method": "pm_transfer",
            "metadata": {"sotto_transfer_id": "transfer:abc"}
        });
        let setup = transfer_setup_intent_from_object(&response, "seti_transfer")
            .expect("off-session SetupIntent belongs to the transfer");
        assert_eq!(setup.transfer_id, "transfer:abc");
        assert_eq!(setup.payment_method_id, "pm_transfer");
        assert!(setup.off_session);

        let mut on_session = response;
        on_session["usage"] = serde_json::json!("on_session");
        assert!(transfer_setup_intent_from_object(&on_session, "seti_transfer").is_err());
    }

    #[test]
    fn personal_transfer_schedule_starts_at_boundary_with_the_authorised_method() {
        let form = personal_transfer_schedule_form(
            "transfer:abc",
            "user-1",
            "cus_transfer",
            "pm_saved",
            BillingOffer::StandardMonthly,
            "price_monthly",
            "alloc_transfer",
            1_900_000_000,
        );
        let fields = form
            .into_iter()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(
            fields.get("start_date").map(String::as_str),
            Some("1900000000")
        );
        assert_eq!(
            fields
                .get("default_settings[default_payment_method]")
                .map(String::as_str),
            Some("pm_saved")
        );
        assert_eq!(
            fields.get("phases[0][items][0][price]").map(String::as_str),
            Some("price_monthly")
        );
        assert_eq!(
            fields
                .get("phases[0][metadata][sotto_allocation_reference]")
                .map(String::as_str),
            Some("alloc_transfer")
        );
        assert_eq!(
            fields.get("end_behavior").map(String::as_str),
            Some("release")
        );
        assert_eq!(
            fields
                .get("phases[0][proration_behavior]")
                .map(String::as_str),
            Some("none")
        );
        assert!(
            !fields.contains_key("phases[0][start_date]"),
            "the top-level schedule start determines its first phase start"
        );
        assert!(!fields
            .keys()
            .any(|key| key.starts_with("payment_intent_data")));
    }

    #[test]
    fn personal_transfer_schedule_readback_must_match_customer_boundary_and_binding() {
        let response = serde_json::json!({
            "id": "sub_sched_transfer",
            "object": "subscription_schedule",
            "customer": "cus_transfer",
            "subscription": null,
            "status": "not_started",
            "start_date": 1900000000,
            "end_behavior": "release",
            "metadata": {
                "sotto_transfer_id": "transfer:abc",
                "personal_user_id": "user-1",
                "operation_id": "transfer:abc:destination",
                "sotto_allocation_reference": "alloc_transfer",
                "sotto_offer": "standard_monthly"
            },
            "default_settings": {"default_payment_method": "pm_saved"},
            "phases": [{
                "start_date": 1900000000,
                "default_payment_method": "pm_saved",
                "proration_behavior": "none",
                "items": [{"price": "price_monthly", "quantity": 1}],
                "metadata": {
                    "sotto_transfer_id": "transfer:abc",
                    "personal_user_id": "user-1",
                    "operation_id": "transfer:abc:destination",
                    "sotto_allocation_reference": "alloc_transfer",
                    "sotto_offer": "standard_monthly"
                }
            }]
        });
        let schedule = transfer_schedule_from_object(
            &response,
            "transfer:abc",
            "cus_transfer",
            "pm_saved",
            "user-1",
            BillingOffer::StandardMonthly,
            "price_monthly",
            "alloc_transfer",
            1_900_000_000,
        )
        .expect("schedule matches its future obligation");
        assert_eq!(schedule.schedule_id, "sub_sched_transfer");
        assert_eq!(schedule.start_at_epoch, 1_900_000_000);
        assert_eq!(
            schedule.expected_period_end_epoch,
            FoundingDate::anchored_term_end_epoch(1_900_000_000, FoundingOffer::Monthly, 1)
                .unwrap()
        );
        assert_eq!(schedule.subscription_id, None);

        let mut changed_boundary = response.clone();
        changed_boundary["phases"][0]["start_date"] = serde_json::json!(1_900_000_001);
        assert!(transfer_schedule_from_object(
            &changed_boundary,
            "transfer:abc",
            "cus_transfer",
            "pm_saved",
            "user-1",
            BillingOffer::StandardMonthly,
            "price_monthly",
            "alloc_transfer",
            1_900_000_000,
        )
        .is_err());

        let mut changed_owner = response;
        changed_owner["phases"][0]["metadata"]["personal_user_id"] =
            serde_json::json!("someone-else");
        assert!(transfer_schedule_from_object(
            &changed_owner,
            "transfer:abc",
            "cus_transfer",
            "pm_saved",
            "user-1",
            BillingOffer::StandardMonthly,
            "price_monthly",
            "alloc_transfer",
            1_900_000_000,
        )
        .is_err());
    }

    #[test]
    fn cancellation_reconciliation_prefers_fresh_terminal_state() {
        let terminal = SubscriptionObservation::Current(SubscriptionSnapshot {
            id: "sub-1".into(),
            status: SubscriptionStatus::Canceled,
        });
        let result =
            cancellation_outcome(Err(ProviderError::transport()), Ok(terminal.clone())).unwrap();
        assert_eq!(result, terminal);

        let blocking = SubscriptionObservation::Current(SubscriptionSnapshot {
            id: "sub-1".into(),
            status: SubscriptionStatus::Unpaid,
        });
        let error = cancellation_outcome(
            Err(ProviderError::http(500, Some("api_error".into()))),
            Ok(blocking),
        )
        .unwrap_err();
        assert_eq!(error.kind, ProviderErrorKind::Retryable);
    }

    #[test]
    fn personal_checkout_expiry_leaves_stripe_a_full_window() {
        let now = 1_700_000_000;
        let quote_expiry = now + founding_allocator::RESERVATION_SECONDS;
        assert_eq!(
            personal_checkout_expiry(quote_expiry, now).unwrap(),
            now + founding_allocator::RESERVATION_SECONDS * 2
        );
        assert!(personal_checkout_expiry(now, now).is_err());
        assert!(personal_checkout_expiry(quote_expiry + 1, now).is_err());
    }

    #[test]
    fn personal_checkout_expiry_requires_the_exact_unpaid_session_binding() {
        let expired = serde_json::json!({
            "object": "checkout.session",
            "id": "cs_personal",
            "mode": "subscription",
            "client_reference_id": "personal:billing:operation",
            "status": "expired",
            "payment_status": "unpaid"
        });
        assert_eq!(
            personal_checkout_status_from_object(&expired, "cs_personal", "billing:operation")
                .unwrap(),
            PersonalCheckoutStatus::ExpiredUnpaid
        );
        let mut paid = expired.clone();
        paid["payment_status"] = serde_json::json!("paid");
        assert!(
            personal_checkout_status_from_object(&paid, "cs_personal", "billing:operation")
                .is_err()
        );
        assert!(
            personal_checkout_status_from_object(&expired, "cs_other", "billing:operation")
                .is_err()
        );
        assert!(
            personal_checkout_status_from_object(&expired, "cs_personal", "billing:other").is_err()
        );
        let mut open = expired.clone();
        open["status"] = serde_json::json!("open");
        assert_eq!(
            personal_checkout_status_from_object(&open, "cs_personal", "billing:operation")
                .unwrap(),
            PersonalCheckoutStatus::Open
        );
        let mut complete = open;
        complete["status"] = serde_json::json!("complete");
        assert_eq!(
            personal_checkout_status_from_object(&complete, "cs_personal", "billing:operation")
                .unwrap(),
            PersonalCheckoutStatus::Complete
        );
        complete["payment_status"] = serde_json::json!("no_payment_required");
        assert_eq!(
            personal_checkout_status_from_object(&complete, "cs_personal", "billing:operation")
                .unwrap(),
            PersonalCheckoutStatus::Complete
        );
    }

    #[test]
    fn sponsored_checkout_status_requires_its_operation_binding() {
        let expired = serde_json::json!({
            "object": "checkout.session",
            "id": "cs_sponsored",
            "mode": "subscription",
            "client_reference_id": "sponsored:sponsored:operation",
            "status": "expired",
            "payment_status": "unpaid"
        });
        assert_eq!(
            checkout_status_from_object(&expired, "cs_sponsored", "sponsored:sponsored:operation")
                .unwrap(),
            PersonalCheckoutStatus::ExpiredUnpaid
        );
        assert!(
            checkout_status_from_object(&expired, "cs_sponsored", "sponsored:other-operation")
                .is_err()
        );
    }

    #[test]
    fn expired_quote_retries_use_the_existing_checkout_deadline() {
        let quote_expiry = 100;
        let now = 110;
        let original_checkout_expiry = 130;
        assert!(quote_expiry < now);
        assert_eq!(
            personal_checkout_expiry_for_retry(original_checkout_expiry, now).unwrap(),
            original_checkout_expiry
        );
        assert!(personal_checkout_expiry_for_retry(now, now).is_err());
    }

    #[test]
    fn expired_pending_personal_accounts_are_not_quote_reusable() {
        assert!(!personal_quote_account_is_reusable(
            personal_billing::PersonalBillingState::Pending
        ));
        assert!(personal_quote_account_is_reusable(
            personal_billing::PersonalBillingState::Canceled
        ));
    }

    #[test]
    fn definitive_checkout_status_matches_http_codes_individually() {
        assert!(definitive_checkout_status(Some(400)));
        assert!(definitive_checkout_status(Some(404)));
        assert!(!definitive_checkout_status(Some(401)));
        assert!(!definitive_checkout_status(Some(500)));
        assert!(!definitive_checkout_status(None));
    }

    #[test]
    fn stripe_period_helpers_use_item_and_line_service_periods() {
        let subscription = serde_json::json!({
            "current_period_end": 1_700_000_001,
            "items": {"data": [
                {"current_period_end": 1_800_000_000},
                {"current_period_end": 1_900_000_000}
            ]}
        });
        assert_eq!(subscription_period_end(&subscription), Some(1_900_000_000));

        let invoice = serde_json::json!({
            "period_end": 1_800_000_000,
            "lines": {"data": [
                {"type": "invoiceitem", "period": {"end": 1_850_000_000}},
                {"parent": {"type": "subscription_item_details"},
                 "period": {"end": 1_950_000_000}}
            ]},
            "parent": {"subscription_details": {"subscription": "sub-1"}}
        });
        assert_eq!(invoice_period_end(&invoice), Some(1_950_000_000));
        assert_eq!(invoice_subscription_id(&invoice), Some("sub-1"));
    }

    #[test]
    fn invoice_payment_failed_events_use_the_subscription_from_the_invoice_parent() {
        let event = Event {
            id: "evt_failed_1".into(),
            created: 1_800_000_000,
            api_version: Some("2026-07-29.dahlia".into()),
            kind: "invoice.payment_failed".into(),
            data: EventData {
                object: serde_json::json!({
                    "id": "in_failed_1",
                    "parent": {
                        "type": "subscription_details",
                        "subscription_details": {"subscription": {"id": "sub_failed_1"}}
                    }
                }),
            },
        };

        assert_eq!(
            event_subscription_id(&event).as_deref(),
            Some("sub_failed_1")
        );
        assert!(!event_updates_subscription_watermark(&event));
    }

    #[test]
    fn sponsored_transfer_period_preview_uses_stripe_line_boundaries() {
        let at = |year, month, day, hour| {
            FoundingDate::new(year, month, day)
                .unwrap()
                .to_unix_seconds()
                + i64::from(hour) * 60 * 60
        };
        let monthly_start = at(2027, 4, 30, 8);
        let monthly_end = at(2027, 5, 31, 8);
        let annual_start = at(2027, 2, 28, 9);
        let annual_end = at(2028, 2, 29, 9);
        let invoice = serde_json::json!({
            "object": "invoice",
            "parent": {"subscription_details": {"subscription": "sub-sponsor"}},
            "lines": {"data": [
                {
                    "parent": {"type": "subscription_item_details", "subscription_item_details": {
                        "subscription": "sub-sponsor", "proration": false
                    }},
                    "pricing": {"price_details": {"price": "price-monthly"}},
                    "period": {"start": monthly_start - 31 * 86_400, "end": monthly_start}
                },
                {
                    "parent": {"type": "subscription_item_details", "subscription_item_details": {
                        "subscription": "sub-sponsor", "proration": false
                    }},
                    "pricing": {"price_details": {"price": "price-monthly"}},
                    "period": {"start": monthly_start, "end": monthly_end}
                },
                {
                    "parent": {"type": "subscription_item_details", "subscription_item_details": {
                        "subscription": "sub-sponsor", "proration": true
                    }},
                    "pricing": {"price_details": {"price": "price-monthly"}},
                    "period": {"start": monthly_start, "end": monthly_start + 100}
                },
                {
                    "parent": {"type": "subscription_item_details", "subscription_item_details": {
                        "subscription": "sub-sponsor", "proration": false
                    }},
                    "pricing": {"price_details": {"price": "price-annual"}},
                    "period": {"start": annual_start, "end": annual_end}
                }
            ]}
        });
        assert_eq!(
            preview_subscription_period_end_from_invoice(
                &invoice,
                "sub-sponsor",
                "price-monthly",
                monthly_start,
            )
            .unwrap(),
            monthly_end,
            "a subscription anchored on the 31st renews on 31 May after 30 April"
        );
        assert_eq!(
            preview_subscription_period_end_from_invoice(
                &invoice,
                "sub-sponsor",
                "price-annual",
                annual_start,
            )
            .unwrap(),
            annual_end,
            "an annual 29 February anchor returns to 29 February in the leap year"
        );
        assert!(preview_subscription_period_end_from_invoice(
            &invoice,
            "another-subscription",
            "price-monthly",
            monthly_start,
        )
        .is_err());
    }

    #[test]
    fn personal_subscription_period_rejects_non_increasing_boundaries() {
        let valid = serde_json::json!({
            "items": {"data": [{"current_period_start": 100, "current_period_end": 200}]}
        });
        assert_eq!(
            personal_subscription_period_from_object(&valid).unwrap(),
            Some(PersonalSubscriptionPeriod {
                start_epoch: 100,
                end_epoch: 200,
            })
        );

        for end in [100, 99] {
            let malformed = serde_json::json!({
                "items": {"data": [{"current_period_start": 100, "current_period_end": end}]}
            });
            assert_eq!(
                personal_subscription_period_from_object(&malformed)
                    .unwrap_err()
                    .kind,
                ProviderErrorKind::Unknown
            );
        }
    }

    #[test]
    fn sponsored_snapshot_and_invoice_items_use_pinned_nested_fields() {
        let subscription = serde_json::json!({
            "id": "sub-sponsored",
            "customer": "cus-sponsored",
            "default_payment_method": "pm_saved",
            "status": "active",
            "cancel_at_period_end": false,
            "cancel_at": null,
            "current_period_start": 1,
            "current_period_end": 2,
            "schedule": "sub_sched",
            "items": {"data": [{
                "id": "si-sponsored",
                "price": {"id": "price-sponsored"},
                "quantity": 3,
                "current_period_start": 1_700_000_000,
                "current_period_end": 1_702_678_400
            }]}
        });
        let snapshot = sponsored_subscription_snapshot(&subscription, "sub-sponsored").unwrap();
        assert_eq!(snapshot.current_period_start, Some(1_700_000_000));
        assert_eq!(snapshot.current_period_end, Some(1_702_678_400));
        assert_eq!(snapshot.schedule_id.as_deref(), Some("sub_sched"));
        assert_eq!(
            snapshot.default_payment_method_id.as_deref(),
            Some("pm_saved")
        );
        assert!(!snapshot.cancel_at_period_end);
        assert_eq!(snapshot.cancel_at, None);

        let mut scheduled_cancellation = subscription.clone();
        scheduled_cancellation["cancel_at_period_end"] = serde_json::json!(false);
        scheduled_cancellation["cancel_at"] = serde_json::json!(1_702_678_400);
        let snapshot =
            sponsored_subscription_snapshot(&scheduled_cancellation, "sub-sponsored").unwrap();
        assert!(snapshot.cancel_at.is_some());

        let malformed_cancellation = serde_json::json!({
            "id":"sub-sponsored", "status":"active", "items":{"data":[]}
        });
        assert_eq!(
            sponsored_subscription_snapshot(&malformed_cancellation, "sub-sponsored")
                .unwrap_err()
                .kind,
            ProviderErrorKind::Unknown
        );

        let invoice = serde_json::json!({
            "lines": {"data": [{
                "parent": {"subscription_item_details": {
                    "subscription_item": "si-sponsored",
                    "proration": false
                }},
                "pricing": {"price_details": {"price": "price-sponsored"}},
                "quantity": 3,
                "amount": 897,
                "period": {"start": 100, "end": 200}
            }]}
        });
        assert_eq!(
            sponsored_invoice_items(&invoice),
            vec![("si-sponsored".into(), "price-sponsored".into(), 3)]
        );
        assert_eq!(
            sponsored_invoice_line_terms(&invoice),
            vec![(
                "si-sponsored".into(),
                "price-sponsored".into(),
                100,
                200,
                897,
                false
            )]
        );
    }

    #[test]
    fn sponsored_invoice_operation_matches_its_own_line_term_not_invoice_max_end() {
        let lines = vec![
            (
                "si_month".into(),
                "price_month".into(),
                100,
                130,
                299,
                false,
            ),
            (
                "si_year".into(),
                "price_year".into(),
                100,
                465,
                2_999,
                false,
            ),
        ];
        assert_eq!(
            sponsored_invoice_line_item_for_operation(&lines, "price_month", 100, None, true)
                .as_deref(),
            Some("si_month")
        );
        assert_eq!(
            sponsored_invoice_line_item_for_operation(&lines, "price_month", 130, None, true),
            None
        );
        let next_lines = vec![(
            "si_month_next".into(),
            "price_month".into(),
            130,
            160,
            299,
            false,
        )];
        assert_eq!(
            sponsored_invoice_line_item_for_operation(&next_lines, "price_month", 130, None, true),
            Some("si_month_next".into())
        );
        let prorated_lines = vec![(
            "si_year_prorated".into(),
            "price_year".into(),
            100,
            465,
            1_835,
            true,
        )];
        assert_eq!(
            sponsored_invoice_line_item_for_operation(
                &prorated_lines,
                "price_year",
                130,
                None,
                true
            ),
            Some("si_year_prorated".into())
        );
        assert_eq!(
            sponsored_invoice_line_item_for_operation(
                &lines,
                "price_month",
                100,
                Some("si_year"),
                true
            ),
            None
        );
        let credit_line = vec![(
            "si_month_credit".into(),
            "price_month".into(),
            100,
            130,
            -299,
            true,
        )];
        assert_eq!(
            sponsored_invoice_line_item_for_operation(&credit_line, "price_month", 100, None, true),
            None
        );
        assert_eq!(
            sponsored_invoice_line_item_for_operation(
                &credit_line,
                "price_month",
                100,
                None,
                false
            ),
            Some("si_month_credit".into())
        );
    }

    #[test]
    fn sponsored_history_reads_only_invoices_with_a_contiguous_item_price_line() {
        let line = |item: &str, price: &str, start, end| {
            crate::cloud_provider_stripe_http::StripeInvoiceLineResource {
                id: "il_test".into(),
                quantity: Some(1),
                amount: Some(299),
                subscription_id: Some("sub_test".into()),
                subscription_item_id: Some(item.into()),
                legacy_subscription_id: None,
                legacy_subscription_item_id: None,
                price_id: Some(price.into()),
                allocation_reference: None,
                parent_type: Some("subscription_item_details".into()),
                pricing_type: Some("price_details".into()),
                livemode: Some(false),
                period_start: Some(start),
                period_end: Some(end),
                invoice_id: Some("in_test".into()),
                proration: Some(false),
            }
        };
        let failed = [line("si_month", "price_month", 200, 300)];
        assert!(sponsored_invoice_lines_can_be_predecessors(
            &[line("si_month", "price_month", 100, 200)],
            &failed,
        ));
        assert!(!sponsored_invoice_lines_can_be_predecessors(
            &[line("si_year", "price_year", 100, 200)],
            &failed,
        ));
        assert!(!sponsored_invoice_lines_can_be_predecessors(
            &[line("si_month", "price_month", 100, 199)],
            &failed,
        ));
    }

    #[test]
    fn sponsored_failure_only_rejects_permanent_evidence_errors() {
        use crate::cloud_provider_stripe_http::StripeReadError;

        assert!(StripeReadError::MalformedResponse("invoice.status").is_evidence_rejection());
        assert!(StripeReadError::Observation(
            crate::cloud_provider_stripe::StripeContractError::ContextMismatch
        )
        .is_evidence_rejection());
        assert!(StripeReadError::Settlement(
            crate::cloud_provider_stripe::StripeContractError::UnsupportedSettlement(
                "ambiguous payment"
            )
        )
        .is_evidence_rejection());

        let operational = [
            StripeReadError::Authentication { status: 401 },
            StripeReadError::Permission { status: 403 },
            StripeReadError::ContextMismatch,
            StripeReadError::SessionClientMismatch,
            StripeReadError::Transport,
            StripeReadError::Timeout,
            StripeReadError::RateLimited {
                retry_after: Some(std::time::Duration::from_secs(1)),
            },
            StripeReadError::Retryable { status: 503 },
            StripeReadError::RequestBoundExceeded,
            StripeReadError::PageBoundExceeded,
            StripeReadError::RecordBoundExceeded,
            StripeReadError::SessionBytesExceeded,
        ];
        assert!(operational
            .iter()
            .all(|error| !error.is_evidence_rejection()));
    }

    #[test]
    fn operator_refund_evidence_must_match_payment_and_approved_amount() {
        let request = billing_refunds::BillingRefundRequest {
            request_id: "refund:request".into(),
            requester_user_id: "user".into(),
            beneficiary_id: "user".into(),
            organization_id: None,
            payer_kind: PayerKind::Personal,
            payment_reference: "pi_expected".into(),
            subscription_id: "sub_expected".into(),
            amount_pence: Some(299),
            reason: "billing_error".into(),
            policy_version: "policy".into(),
            idempotency_key: "key".into(),
            request_hash: "hash".into(),
            full_refund_requested: false,
            state: billing_refunds::CorrectionState::Unknown,
            preserve_paid_term: true,
            early_termination_confirmed_at_epoch: None,
            effective_at_epoch: None,
            provider_refund_id: None,
            provider_refund_started_at_epoch: Some(1_900_000_000),
            result_code: Some("refund_create_horizon_expired".into()),
        };
        let receipt = RefundReceipt {
            refund_id: "re_verified".into(),
            status: "succeeded".into(),
            payment_reference: "pi_expected".into(),
            charge_id: Some("ch_expected".into()),
            amount_pence: 299,
            currency: "gbp".into(),
        };
        assert!(refund_receipt_matches_approved_correction(
            &request, &receipt, None
        ));

        let mut wrong_payment = receipt.clone();
        wrong_payment.payment_reference = "pi_other".into();
        assert!(!refund_receipt_matches_approved_correction(
            &request,
            &wrong_payment,
            None
        ));
        let mut wrong_amount = receipt.clone();
        wrong_amount.amount_pence = 300;
        assert!(!refund_receipt_matches_approved_correction(
            &request,
            &wrong_amount,
            None
        ));
        let mut wrong_currency = receipt;
        wrong_currency.currency = "usd".into();
        assert!(!refund_receipt_matches_approved_correction(
            &request,
            &wrong_currency,
            None
        ));

        let mut full_refund = request;
        full_refund.full_refund_requested = true;
        full_refund.amount_pence = None;
        let full_receipt = RefundReceipt {
            refund_id: "re_full".into(),
            status: "succeeded".into(),
            payment_reference: "pi_expected".into(),
            charge_id: Some("ch_expected".into()),
            amount_pence: 299,
            currency: "gbp".into(),
        };
        assert!(refund_receipt_matches_approved_correction(
            &full_refund,
            &full_receipt,
            Some(&(299, "gbp".into()))
        ));
        assert!(!refund_receipt_matches_approved_correction(
            &full_refund,
            &full_receipt,
            None
        ));
        assert!(!refund_receipt_matches_approved_correction(
            &full_refund,
            &full_receipt,
            Some(&(300, "gbp".into()))
        ));
        let mut incomplete_full_receipt = full_receipt.clone();
        incomplete_full_receipt.amount_pence = 100;
        assert!(!refund_receipt_matches_approved_correction(
            &full_refund,
            &incomplete_full_receipt,
            Some(&(299, "gbp".into()))
        ));
        assert!(refund_receipt_matches_charge(&full_receipt, "ch_expected"));
        assert!(!refund_receipt_matches_charge(&full_receipt, "ch_other"));
    }
}
