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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
#[cfg(feature = "e2e-mock-billing")]
use axum::response::Html;
use axum::routing::{get, post};
use axum::{Json, Router};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use sqlx::{Postgres, Row, Transaction};

#[cfg(feature = "e2e-mock-billing")]
use url::Url;

use crate::auth::AuthUser;
use crate::billing_catalogue::{BillingOffer, BillingPriceIds};
use crate::billing_operations::{self, BeginOperation, BillingOperationState};
use crate::billing_refunds::{self, CorrectionReason, PayerKind};
use crate::config::BillingConfig;
use crate::error::{Error, Result};
use crate::founding_allocator::{self, FoundingOffer};
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

    fn into_error(self) -> Error {
        let detail = self.code.unwrap_or_else(|| "unknown_error".into());
        Error::Upstream(format!("stripe provider error: {detail}"))
    }
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SponsoredSubscriptionItem {
    pub provider_item_id: String,
    pub provider_price_id: String,
    pub quantity: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SponsoredSubscriptionItemRequest {
    pub provider_price_id: String,
    pub quantity: i64,
}

/// A bounded or open-ended future phase for the organisation's sponsored subscription.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SponsoredSubscriptionPhase {
    pub start_at_epoch: i64,
    pub end_at_epoch: Option<i64>,
    pub items: Vec<SponsoredSubscriptionItemRequest>,
}

/// Provider state needed to reconcile a sponsored subscription mutation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SponsoredSubscriptionSnapshot {
    pub customer_id: Option<String>,
    pub subscription_id: String,
    pub status: SubscriptionStatus,
    pub current_period_start: Option<i64>,
    pub current_period_end: Option<i64>,
    pub schedule_id: Option<String>,
    pub items: Vec<SponsoredSubscriptionItem>,
}

/// The durable identifiers returned while creating the initial hosted subscription.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SponsoredCheckoutSession {
    pub session_id: String,
    pub checkout_url: String,
    pub customer_id: Option<String>,
    pub subscription_id: Option<String>,
}

/// Provider evidence for a refund request. A refund response is not an access decision; the
/// durable correction state decides whether the paid term remains or ends early.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefundReceipt {
    pub refund_id: String,
    pub status: String,
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
        idempotency_key: &str,
        expires_at_epoch: i64,
        success_url: &str,
        cancel_url: &str,
    ) -> ProviderResult<String> {
        let _ = (price_id, operation_id, idempotency_key, expires_at_epoch);
        self.create_checkout(user_id, customer, success_url, cancel_url)
            .await
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

    async fn create_portal(&self, customer: &str, return_url: &str) -> ProviderResult<String>;

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

    async fn get_subscription(
        &self,
        subscription_id: &str,
    ) -> ProviderResult<SubscriptionObservation>;

    /// Read the provider's current period end for a newly paid personal subscription.
    async fn personal_subscription_period_end(
        &self,
        _subscription_id: &str,
    ) -> ProviderResult<Option<i64>> {
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
}

impl BillingState {
    /// Share the provider with the deletion worker without exposing billing credentials.
    pub fn provider(&self) -> Arc<dyn SubscriptionProvider> {
        Arc::clone(&self.provider)
    }

    pub fn price_catalogue(&self) -> Option<&BillingPriceIds> {
        self.price_catalogue.as_ref()
    }

    pub fn from_config(config: BillingConfig) -> Self {
        let provider = StripeBilling {
            api_key: config.api_key.clone(),
            price_id: config.price_id,
        };
        Self {
            provider: Arc::new(provider),
            webhook_secret: config.webhook_secret,
            return_url: config.return_url,
            price_catalogue: config.price_catalogue,
        }
    }

    pub fn with_provider(
        provider: Arc<dyn SubscriptionProvider>,
        webhook_secret: String,
        return_url: String,
    ) -> Self {
        Self {
            provider,
            webhook_secret,
            return_url,
            price_catalogue: None,
        }
    }

    #[cfg(feature = "e2e-mock-billing")]
    pub fn with_e2e_provider(config: BillingConfig, provider_origin: String) -> Self {
        Self {
            provider: Arc::new(E2eBilling { provider_origin }),
            webhook_secret: config.webhook_secret,
            return_url: config.return_url,
            price_catalogue: config.price_catalogue,
        }
    }
}

struct StripeBilling {
    api_key: String,
    price_id: String,
}

#[async_trait]
impl SubscriptionProvider for StripeBilling {
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
        idempotency_key: &str,
        expires_at_epoch: i64,
        success_url: &str,
        cancel_url: &str,
    ) -> ProviderResult<String> {
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
        session["url"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(ProviderError::malformed_response)
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
            idempotency_key,
        )
        .await?;
        self.get_sponsored_subscription(subscription_id).await
    }

    async fn get_sponsored_subscription(
        &self,
        subscription_id: &str,
    ) -> ProviderResult<SponsoredSubscriptionSnapshot> {
        let value = stripe_get(&self.api_key, &format!("subscriptions/{subscription_id}")).await?;
        sponsored_subscription_snapshot(&value, subscription_id)
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

    async fn create_portal(&self, customer: &str, return_url: &str) -> ProviderResult<String> {
        let form = vec![
            ("customer".to_string(), customer.to_string()),
            ("return_url".to_string(), return_url.to_string()),
        ];
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
        Ok(RefundReceipt {
            refund_id: refund_id.to_string(),
            status: status.to_string(),
        })
    }

    async fn get_refund(&self, provider_refund_id: &str) -> ProviderResult<RefundReceipt> {
        if !provider_refund_id.starts_with("re_") {
            return Err(ProviderError::malformed_response());
        }
        let value = stripe_get(&self.api_key, &format!("refunds/{provider_refund_id}")).await?;
        let refund_id = value["id"]
            .as_str()
            .filter(|id| *id == provider_refund_id)
            .ok_or_else(ProviderError::malformed_response)?;
        let status = value["status"]
            .as_str()
            .filter(|status| !status.is_empty())
            .ok_or_else(ProviderError::malformed_response)?;
        Ok(RefundReceipt {
            refund_id: refund_id.to_string(),
            status: status.to_string(),
        })
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

    async fn personal_subscription_period_end(
        &self,
        subscription_id: &str,
    ) -> ProviderResult<Option<i64>> {
        let object = stripe_get(&self.api_key, &format!("subscriptions/{subscription_id}")).await?;
        if object["id"].as_str() != Some(subscription_id) {
            return Err(ProviderError::malformed_response());
        }
        Ok(subscription_period_end(&object))
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
        let form = vec![("cancel_at_period_end".to_string(), "true".to_string())];
        let response = stripe_post_with_idempotency(
            &self.api_key,
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

    async fn create_portal(&self, _customer: &str, return_url: &str) -> ProviderResult<String> {
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

#[derive(Debug, Deserialize)]
struct SponsoredQuoteRequest {
    action: String,
    offer: String,
    beneficiary_ids: Vec<String>,
}

#[derive(Debug, Serialize)]
struct SponsoredQuoteView {
    action: String,
    seat_count: i64,
    amount_pence: i64,
    currency: &'static str,
    interval: &'static str,
    quote_version: i64,
    quote_expires_at_epoch: i64,
}

#[derive(Debug, Serialize)]
struct SponsoredSeatView {
    seat_id: String,
    beneficiary_id: String,
    offer: String,
    effective_from: i64,
    effective_until: Option<i64>,
    state: String,
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
    if request.beneficiary_ids.is_empty()
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
            "beneficiary_ids must contain unique existing accounts".into(),
        ));
    }
    let offer = billing_offer(&request.offer)?;
    let billing = sponsored_billing_config(&state)?;
    let catalogue = billing
        .price_catalogue()
        .ok_or_else(|| Error::NotConfigured("hosted sponsored billing is not configured".into()))?;
    validate_personal_catalogue(billing, catalogue).await?;
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
    let quote = sponsored_billing::quote(
        action,
        offer,
        request.beneficiary_ids.len(),
        sponsored_billing::current_epoch(),
    )
    .map_err(Error::from)?;
    Ok(Json(SponsoredQuoteView {
        action: quote.action.as_str().into(),
        seat_count: quote.seat_count,
        amount_pence: quote.amount_pence,
        currency: quote.currency,
        interval: quote.interval,
        quote_version: quote.quote_version,
        quote_expires_at_epoch: quote.quote_expires_at_epoch,
    }))
}

async fn sponsored_checkout(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<String>,
    Json(input): Json<SponsoredCheckoutRequest>,
) -> Result<Json<SponsoredOperationView>> {
    let billing = sponsored_billing_config(&state)?;
    let catalogue = billing
        .price_catalogue()
        .ok_or_else(|| Error::NotConfigured("hosted sponsored billing is not configured".into()))?;
    let action = SponsoredSeatAction::parse(&input.action).map_err(Error::from)?;
    let offer = billing_offer(&input.offer)?;
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
    let now = sponsored_billing::current_epoch();
    let mut tx = state.pool.begin().await?;
    let operation =
        sponsored_billing::begin_operation(&mut tx, &org_id, &user.user_id, &request, now)
            .await
            .map_err(Error::from)?;
    if operation.state != "pending" {
        tx.rollback().await?;
        return Ok(Json(SponsoredOperationView {
            operation_id: operation.operation_id,
            action: operation.action.as_str().into(),
            state: operation.state,
            provider_checkout_url: operation.provider_checkout_url,
        }));
    }
    let existing_subscription: Option<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT provider_customer_id, provider_subscription_id \
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
    tx.commit().await?;

    let (success_url, cancel_url) = checkout_return_urls(&billing.return_url);
    let result = if let Some((customer_id, Some(subscription_id))) = existing_subscription {
        let phases = sponsored_provider_phases(
            &state.pool,
            &org_id,
            catalogue,
            sponsored_billing::current_epoch(),
        )
        .await?;
        let snapshot = billing
            .provider
            .update_sponsored_subscription(
                &org_id,
                &subscription_id,
                &phases,
                &operation.provider_idempotency_key,
            )
            .await
            .map_err(ProviderError::into_error)?;
        let mut result_tx = state.pool.begin().await?;
        let operation = sponsored_billing::record_provider_update(
            &mut result_tx,
            &operation.operation_id,
            customer_id.as_deref().or(snapshot.customer_id.as_deref()),
            &subscription_id,
            snapshot.schedule_id.as_deref(),
            snapshot.current_period_end,
            None,
        )
        .await
        .map_err(Error::from)?;
        result_tx.commit().await?;
        SponsoredOperationView {
            operation_id: operation.operation_id,
            action: operation.action.as_str().into(),
            state: operation.state,
            provider_checkout_url: operation.provider_checkout_url,
        }
    } else if existing_subscription.is_none() && action == SponsoredSeatAction::Add {
        let items = sponsored_provider_items(&state.pool, &org_id, catalogue, &operation).await?;
        let checkout = billing
            .provider
            .create_sponsored_checkout(
                &org_id,
                existing_customer.as_deref(),
                &items,
                &operation.operation_id,
                &operation.provider_idempotency_key,
                personal_checkout_expiry(
                    operation.quote_expires_at_epoch,
                    sponsored_billing::current_epoch(),
                )?,
                &success_url,
                &cancel_url,
            )
            .await
            .map_err(ProviderError::into_error)?;
        let mut result_tx = state.pool.begin().await?;
        let operation = sponsored_billing::record_checkout(
            &mut result_tx,
            &operation.operation_id,
            &checkout.checkout_url,
            &checkout.session_id,
            checkout.customer_id.as_deref(),
            checkout.subscription_id.as_deref(),
        )
        .await
        .map_err(Error::from)?;
        result_tx.commit().await?;
        SponsoredOperationView {
            operation_id: operation.operation_id,
            action: operation.action.as_str().into(),
            state: operation.state,
            provider_checkout_url: operation.provider_checkout_url,
        }
    } else {
        return Err(Error::Conflict(
            "sponsored subscription is missing for this change".into(),
        ));
    };
    Ok(Json(result))
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
async fn sponsored_provider_phases(
    pool: &sqlx::PgPool,
    organization_id: &str,
    catalogue: &BillingPriceIds,
    now_epoch: i64,
) -> Result<Vec<SponsoredSubscriptionPhase>> {
    let mut rows: Vec<(String, String, i64, Option<i64>)> = sqlx::query_as(
        "SELECT beneficiary_id, offer, effective_from, effective_until \
         FROM billing_sponsored_seats \
         WHERE organization_id = $1 AND state IN ('active', 'pending', 'scheduled_removal') \
         ORDER BY effective_from, seat_id",
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await?;

    // Removal and replacement rows are not marked scheduled_removal until the provider accepts
    // their schedule. Apply every still-live operation in memory so a second mutation cannot
    // rebuild Stripe's schedule without an earlier pending change.
    let pending_changes: Vec<(String, String, Option<i64>)> = sqlx::query_as(
        "SELECT beneficiary_id, action, effective_until \
         FROM billing_sponsored_operations \
         WHERE organization_id = $1 AND state IN ('pending', 'checkout_created', 'provider_pending') \
           AND action IN ('remove', 'replace') ORDER BY created_at",
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await?;
    for (beneficiary_id, _, effective_until) in pending_changes {
        for row in &mut rows {
            if row.0 == beneficiary_id {
                row.3 = effective_until;
            }
        }
    }

    let mut boundaries = std::collections::BTreeSet::from([now_epoch]);
    for (_, _, effective_from, effective_until) in &rows {
        if *effective_from > now_epoch {
            boundaries.insert(*effective_from);
        }
        if let Some(effective_until) = effective_until.filter(|until| *until > now_epoch) {
            boundaries.insert(effective_until);
        }
    }
    let boundaries: Vec<i64> = boundaries.into_iter().collect();
    let mut phases = Vec::with_capacity(boundaries.len());
    for (index, start_at_epoch) in boundaries.iter().copied().enumerate() {
        let end_at_epoch = boundaries.get(index + 1).copied();
        let mut quantities = std::collections::BTreeMap::<String, i64>::new();
        for (_, offer, effective_from, effective_until) in &rows {
            if *effective_from <= start_at_epoch
                && effective_until
                    .map(|effective_until| start_at_epoch < effective_until)
                    .unwrap_or(true)
            {
                let quantity = quantities.entry(offer.clone()).or_default();
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

fn billing_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_secs() as i64
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
    let billing = billing_config(&state)?;
    let catalogue = billing
        .price_catalogue()
        .ok_or_else(|| Error::NotConfigured("hosted personal billing is not configured".into()))?;
    let offer = billing_offer(&query.offer)?;
    validate_personal_catalogue(billing, catalogue).await?;
    let now = billing_epoch();
    let expires = now
        .checked_add(founding_allocator::RESERVATION_SECONDS)
        .ok_or_else(|| Error::Internal("billing clock overflow".into()))?;
    let mut tx = state.pool.begin().await?;
    if let Some(account) = personal_billing::load_account(&mut tx, &user.user_id)
        .await
        .map_err(personal_billing_error)?
    {
        let reusable = matches!(
            account.state,
            personal_billing::PersonalBillingState::Canceled
        ) || (matches!(
            account.state,
            personal_billing::PersonalBillingState::Pending
        ) && account.pending_expires_at_epoch <= now);
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
                FoundingOffer::Monthly => "through the founding monthly term",
                FoundingOffer::Annual => "through the founding annual term",
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
async fn personal_checkout(
    State(state): State<AppState>,
    user: AuthUser,
    Json(request): Json<PersonalCheckoutRequest>,
) -> Result<Json<PersonalCheckoutView>> {
    let billing = billing_config(&state)?;
    let catalogue = billing
        .price_catalogue()
        .ok_or_else(|| Error::NotConfigured("hosted personal billing is not configured".into()))?;
    let offer = billing_offer(&request.offer)?;
    validate_personal_catalogue(billing, catalogue).await?;
    let price_id = catalogue.id_for(offer).to_string();
    billing_operations::validate_return_url(&billing.return_url, &request.return_url)
        .map_err(Error::from)?;
    let mut tx = state.pool.begin().await?;
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
            if let Some(founding_offer) = FoundingOffer::from_billing_offer(offer) {
                let reservation = founding_allocator::reserve(
                    &mut tx,
                    &format!("founding:{}", operation.operation_id),
                    &operation.operation_id,
                    &user.user_id,
                    &user.user_id,
                    founding_offer,
                    request.quote_version,
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
            operation
        }
    };
    let checkout_expires_at_epoch = match checkout_expires_at_epoch {
        Some(expiry) => expiry,
        None => personal_checkout_expiry(operation.quote_expires_at_epoch, billing_epoch())?,
    };
    let (success_url, cancel_url) = checkout_return_urls(&billing.return_url);
    let checkout_url = billing
        .provider
        .create_personal_checkout(
            &user.user_id,
            None,
            &price_id,
            &operation.operation_id,
            &operation.provider_idempotency_key,
            checkout_expires_at_epoch,
            &success_url,
            &cancel_url,
        )
        .await
        .map_err(ProviderError::into_error)?;
    let mut result_tx = state.pool.begin().await?;
    personal_billing::record_checkout_url(&mut result_tx, &operation.operation_id, &checkout_url)
        .await
        .map_err(personal_billing_error)?;
    let result = billing_operations::record_provider_result(
        &mut result_tx,
        &operation.operation_id,
        BillingOperationState::Succeeded,
        None,
        Some("checkout_created"),
    )
    .await
    .map_err(Error::from)?;
    result_tx.commit().await?;
    Ok(Json(PersonalCheckoutView {
        operation_id: result.operation_id,
        state: result.state.as_str().into(),
        checkout_url: Some(checkout_url),
    }))
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
    let current = if current.state == billing_refunds::CorrectionState::Approved {
        let next = billing_refunds::begin_provider_refund(&mut tx, &request_id)
            .await
            .map_err(Error::from)?;
        tx.commit().await?;
        next
    } else {
        tx.rollback().await?;
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
    if current.state != billing_refunds::CorrectionState::ProviderPending {
        return Ok(Json(personal_refund_view(current)));
    }
    let provider_key = format!("sotto-refund:{request_id}");
    let receipt = if let Some(provider_refund_id) = current.provider_refund_id.as_deref() {
        billing
            .provider
            .get_refund(provider_refund_id)
            .await
            .map_err(ProviderError::into_error)?
    } else {
        billing
            .provider
            .create_refund(
                &current.payment_reference,
                current.amount_pence,
                &provider_key,
            )
            .await
            .map_err(ProviderError::into_error)?
    };
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
        status => billing_refunds::record_provider_refund(
            &mut tx,
            &request_id,
            &receipt.refund_id,
            false,
            Some(status),
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
    tx.rollback().await?;
    let portal_url = billing
        .provider
        .create_portal(&customer, &app_url(&billing.return_url))
        .await
        .map_err(ProviderError::into_error)?;
    Ok(Json(PersonalLifecycleView {
        state: account.state.as_str().into(),
        stripe_subscription_id: account.stripe_subscription_id,
        paid_through_date: account.paid_through_date,
        cancel_at_period_end: account.cancel_at_period_end,
        portal_url: Some(portal_url),
    }))
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
    let account = personal_billing::load_account(&mut tx, &user.user_id)
        .await
        .map_err(personal_billing_error)?
        .ok_or_else(|| Error::NotFound("personal billing account not found".into()))?;
    if account.cancel_at_period_end {
        tx.rollback().await?;
        return Ok(Json(PersonalLifecycleView {
            state: account.state.as_str().into(),
            stripe_subscription_id: account.stripe_subscription_id,
            paid_through_date: account.paid_through_date,
            cancel_at_period_end: true,
            portal_url: None,
        }));
    }
    let subscription_id = account.stripe_subscription_id.clone().ok_or_else(|| {
        Error::Conflict("personal billing has not received a paid subscription yet".into())
    })?;
    tx.rollback().await?;
    billing
        .provider
        .cancel_personal_subscription(&subscription_id, &request.idempotency_key, &user.user_id)
        .await
        .map_err(ProviderError::into_error)?;
    let mut update_tx = state.pool.begin().await?;
    sqlx::query(
        "UPDATE billing_personal_accounts SET cancel_at_period_end = TRUE, \
         cancellation_requested_at = now(), updated_at = now() WHERE user_id = $1",
    )
    .bind(&user.user_id)
    .execute(&mut *update_tx)
    .await?;
    update_tx.commit().await?;
    Ok(Json(PersonalLifecycleView {
        state: account.state.as_str().into(),
        stripe_subscription_id: Some(subscription_id),
        paid_through_date: account.paid_through_date,
        cancel_at_period_end: true,
        portal_url: None,
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
    let billing = billing_config(&state)?;
    let mut tx = state.pool.begin().await?;
    // Keep the organisation lock through provider session creation so deletion cannot transition
    // between the lifecycle check and this billing side effect. The bounded provider timeout
    // briefly pins a pool connection and serialises this organisation's writes; that is the
    // deliberate trade-off for closing the race.
    require_billing_admin(&mut tx, &org_id, &user.user_id).await?;

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
        .create_portal(&customer, &app_url(&billing.return_url))
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
    let response = stripe_client()
        .post(format!("https://api.stripe.com/v1/{path}"))
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
    let response = stripe_client()
        .post(format!("https://api.stripe.com/v1/{path}"))
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
    let response = stripe_client()
        .get(format!("https://api.stripe.com/v1/{path}"))
        .bearer_auth(api_key)
        .headers(stripe_headers(None)?)
        .send()
        .await
        .map_err(|_| ProviderError::transport())?;
    stripe_response(response).await
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
        subscription_id: base.id,
        status: base.status,
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

/// `POST /billing/webhook` - Stripe's event delivery. Signature-verified against the endpoint's
/// signing secret; unhandled event types are acknowledged and ignored (so the endpoint can be
/// subscribed broadly in the dashboard without breaking).
async fn webhook(State(state): State<AppState>, headers: HeaderMap, body: String) -> Result<()> {
    let billing = billing_config(&state)?;
    let signature = headers
        .get("Stripe-Signature")
        .and_then(|v| v.to_str().ok())
        .ok_or(Error::Unauthorized)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
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

    let mut tx = state.pool.begin().await?;
    prune_webhook_events(&mut tx).await?;
    let disposition = record_webhook_event(&mut tx, &event).await?;
    if disposition == EventDisposition::Ignore {
        tx.commit().await?;
        return Ok(());
    }

    let object = &event.data.object;
    let subscription_id = event_subscription_id(&event);
    let personal_period_end = if is_personal_checkout_event(&event) {
        if let Some(subscription_id) = subscription_id.as_deref() {
            billing
                .provider
                .personal_subscription_period_end(subscription_id)
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
        if is_sponsored_checkout_expired_event(&event) {
            sponsored_checkout_expired(&mut tx, object).await?;
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
                personal_period_end,
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

    match disposition {
        EventDisposition::Apply => match event.kind.as_str() {
            "checkout.session.completed" | "checkout.session.async_payment_succeeded" => {
                checkout_completed(
                    &mut tx,
                    object,
                    event.created,
                    event.kind == "checkout.session.async_payment_succeeded",
                    personal_period_end,
                    sponsored_snapshot.as_ref(),
                )
                .await?
            }
            "checkout.session.expired" => sponsored_checkout_expired(&mut tx, object).await?,
            "customer.subscription.updated" => subscription_updated(&mut tx, object).await?,
            "customer.subscription.deleted" => subscription_deleted(&mut tx, object).await?,
            "invoice.paid" => invoice_paid(&mut tx, object, billing.price_catalogue()).await?,
            _ => {}
        },
        EventDisposition::Reconcile | EventDisposition::Ignore => {
            return Err(Error::Config("invalid webhook disposition".into()));
        }
    }
    if let Some(subscription_id) = subscription_id {
        update_subscription_watermark(&mut tx, &event, &subscription_id).await?;
    }
    mark_webhook_event_processed(&mut tx, &event.id).await?;
    tx.commit().await?;
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EventDisposition {
    /// The event is newer than the stored watermark and can apply its payload normally.
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
         (event_id, event_type, api_version, stripe_created, subscription_id) \
         VALUES ($1, $2, $3, $4, $5) ON CONFLICT (event_id) DO NOTHING",
    )
    .bind(&event.id)
    .bind(&event.kind)
    .bind(event.api_version.as_deref().unwrap_or("missing"))
    .bind(event.created)
    .bind(subscription_id)
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

fn event_subscription_id(event: &Event) -> Option<String> {
    match event.kind.as_str() {
        "checkout.session.completed" | "checkout.session.async_payment_succeeded" => {
            event.data.object["subscription"]
                .as_str()
                .map(str::to_string)
        }
        "invoice.paid" => invoice_subscription_id(&event.data.object).map(str::to_string),
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

async fn sponsored_checkout_expired(
    tx: &mut Transaction<'_, Postgres>,
    object: &serde_json::Value,
) -> Result<()> {
    if !sponsored_billing_enabled() {
        return Ok(());
    }
    let Some(reference) = object["client_reference_id"].as_str() else {
        return Ok(());
    };
    let Some(operation_id) = reference.strip_prefix("sponsored:") else {
        return Ok(());
    };
    sponsored_billing::cancel_failed_operation(tx, operation_id, "checkout_expired")
        .await
        .map_err(Error::from)?;
    Ok(())
}

/// A paid checkout: record the Stripe ids and grant the Team tier. Idempotent - a redelivered
/// event changes no rows and writes no duplicate audit entry.
async fn checkout_completed(
    tx: &mut Transaction<'_, Postgres>,
    object: &serde_json::Value,
    event_created: i64,
    async_payment_succeeded: bool,
    personal_period_end: Option<i64>,
    sponsored_snapshot: Option<&SponsoredSubscriptionSnapshot>,
) -> Result<()> {
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
            personal_period_end,
        )
        .await;
    }
    if let Some(operation_id) = org_id.strip_prefix("sponsored:") {
        if !sponsored_billing_enabled() {
            return Ok(());
        }
        if !async_payment_succeeded && object["payment_status"].as_str() != Some("paid") {
            return Ok(());
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
        sponsored_billing::complete_paid_checkout(tx, operation_id, &evidence)
            .await
            .map_err(Error::from)?;
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

async fn personal_checkout_completed(
    tx: &mut Transaction<'_, Postgres>,
    object: &serde_json::Value,
    event_created: i64,
    operation_id: &str,
    async_payment_succeeded: bool,
    personal_period_end: Option<i64>,
) -> Result<()> {
    if !async_payment_succeeded && object["payment_status"].as_str() != Some("paid") {
        // A completed Checkout session is not itself authority. The account remains pending until
        // Stripe says the session is paid, so SCA failures and asynchronous payment methods never
        // become a free hosted term.
        return Ok(());
    }
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
    let paid_on = founding_allocator::FoundingDate::from_unix_seconds(event_created)
        .map_err(|_| Error::Config("personal checkout timestamp is invalid".into()))?;
    let interval_offer = match offer {
        BillingOffer::StandardMonthly | BillingOffer::FoundingMonthly => FoundingOffer::Monthly,
        BillingOffer::StandardAnnual | BillingOffer::FoundingAnnual => FoundingOffer::Annual,
    };
    let paid_through = if let Some(period_end) = personal_period_end {
        founding_allocator::FoundingDate::from_unix_seconds(period_end)
            .map_err(|_| Error::Config("personal subscription period end is invalid".into()))?
    } else {
        paid_on.add_term(interval_offer)
    };
    if FoundingOffer::from_billing_offer(offer).is_some() {
        let reservation_id = format!("founding:{operation_id}");
        match founding_allocator::confirm_payment(
            tx,
            &reservation_id,
            payment_reference,
            paid_on,
            event_created,
        )
        .await
        .map_err(|error| Error::Internal(error.to_string()))?
        {
            founding_allocator::ConfirmationOutcome::Awarded(_)
            | founding_allocator::ConfirmationOutcome::AlreadyAwarded(_) => {}
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
        let cancel_at_period_end = object["cancel_at_period_end"].as_bool().ok_or_else(|| {
            Error::Config("personal subscription event has no cancellation state".into())
        })?;
        let state = match SubscriptionStatus::parse(status) {
            SubscriptionStatus::Active | SubscriptionStatus::Trialing => "active",
            SubscriptionStatus::PastDue | SubscriptionStatus::Paused => "past_due",
            SubscriptionStatus::Unpaid => "unpaid",
            SubscriptionStatus::Canceled | SubscriptionStatus::IncompleteExpired => "canceled",
            SubscriptionStatus::Incomplete => "pending",
            SubscriptionStatus::Unknown(_) => return Ok(()),
        };
        sqlx::query(
            "UPDATE billing_personal_accounts SET state = CASE \
                 WHEN state = 'pending' AND $2 <> 'canceled' THEN state ELSE $2 END, \
                 cancel_at_period_end = $3, stripe_subscription_id = COALESCE(stripe_subscription_id, $4), \
                 updated_at = now() WHERE user_id = $1",
        )
        .bind(&user_id)
        .bind(state)
        .bind(cancel_at_period_end)
        .bind(subscription_id)
        .execute(&mut **tx)
        .await?;
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

fn invoice_subscription_id(object: &serde_json::Value) -> Option<&str> {
    object["parent"]["subscription_details"]["subscription"].as_str()
}

async fn invoice_paid(
    tx: &mut Transaction<'_, Postgres>,
    object: &serde_json::Value,
    catalogue: Option<&BillingPriceIds>,
) -> Result<()> {
    let Some(subscription_id) = invoice_subscription_id(object) else {
        return Ok(());
    };
    if sponsored_billing_enabled()
        && sponsored_invoice_paid(tx, object, subscription_id, catalogue).await?
    {
        return Ok(());
    }
    let personal: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM billing_personal_accounts \
         WHERE stripe_subscription_id = $1)",
    )
    .bind(subscription_id)
    .fetch_one(&mut **tx)
    .await?;
    if !personal {
        return Ok(());
    }
    let period_end = invoice_period_end(object)
        .ok_or_else(|| Error::Config("paid personal invoice has no period end".into()))?;
    let paid_through = founding_allocator::FoundingDate::from_unix_seconds(period_end)
        .map_err(|_| Error::Config("paid personal invoice has invalid period end".into()))?;
    let payment_reference = object["payment_intent"]
        .as_str()
        .or_else(|| object["id"].as_str())
        .ok_or_else(|| Error::Config("paid personal invoice has no payment reference".into()))?;
    personal_billing::record_invoice_paid(
        tx,
        subscription_id,
        payment_reference,
        period_end,
        &paid_through.to_string(),
    )
    .await
    .map_err(personal_billing_error)?;
    Ok(())
}

async fn sponsored_invoice_paid(
    tx: &mut Transaction<'_, Postgres>,
    object: &serde_json::Value,
    subscription_id: &str,
    catalogue: Option<&BillingPriceIds>,
) -> Result<bool> {
    let Some(catalogue) = catalogue else {
        return Err(Error::NotConfigured(
            "sponsored billing price catalogue is not configured".into(),
        ));
    };
    let organization_id: Option<String> = sqlx::query_scalar(
        "SELECT organization_id FROM billing_sponsored_subscriptions \
         WHERE provider_subscription_id = $1 FOR UPDATE",
    )
    .bind(subscription_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(organization_id) = organization_id else {
        return Ok(false);
    };
    let payment_reference = object["payment_intent"]
        .as_str()
        .or_else(|| object["id"].as_str())
        .ok_or_else(|| Error::Config("paid sponsored invoice has no payment reference".into()))?;
    let period_end = invoice_period_end(object)
        .ok_or_else(|| Error::Config("paid sponsored invoice has no period end".into()))?;
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
    let operations: Vec<(String, String)> = sqlx::query_as(
        "SELECT operation_id, offer FROM billing_sponsored_operations \
         WHERE organization_id = $1 AND state IN ('checkout_created', 'provider_pending') \
           AND effective_from < $2 \
         ORDER BY created_at FOR UPDATE",
    )
    .bind(&organization_id)
    .bind(period_end)
    .fetch_all(&mut **tx)
    .await?;
    for (operation_id, offer) in operations {
        let offer = billing_offer(&offer)?;
        let provider_item_id = line_items
            .iter()
            .find(|(_, price_id, _)| price_id == catalogue.id_for(offer))
            .map(|(item_id, _, _)| item_id.clone());
        if provider_item_id.is_none() {
            continue;
        }
        let evidence = sponsored_billing::SponsoredProviderEvidence {
            customer_id: customer_id.clone(),
            subscription_id: subscription_id.to_string(),
            schedule_id: None,
            checkout_session_id: None,
            payment_reference: payment_reference.to_string(),
            provider_item_id,
        };
        sponsored_billing::complete_paid_checkout(tx, &operation_id, &evidence)
            .await
            .map_err(Error::from)?;
    }
    Ok(true)
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
        sqlx::query(
            "UPDATE billing_personal_accounts SET state = 'canceled', \
             stripe_subscription_id = COALESCE(stripe_subscription_id, $2), updated_at = now() \
             WHERE user_id = $1",
        )
        .bind(user_id)
        .bind(subscription_id)
        .execute(&mut **tx)
        .await?;
        return Ok(());
    }
    if let Some(organization_id) = sponsored_subscription_organization(tx, object).await? {
        let subscription_id = object["id"].as_str().ok_or_else(|| {
            Error::Config("sponsored subscription event has no subscription id".into())
        })?;
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
            .create_portal("cus-test", "http://127.0.0.1:5199/app")
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
    fn sponsored_snapshot_and_invoice_items_use_pinned_nested_fields() {
        let subscription = serde_json::json!({
            "id": "sub-sponsored",
            "customer": "cus-sponsored",
            "status": "active",
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

        let invoice = serde_json::json!({
            "lines": {"data": [{
                "parent": {"subscription_item_details": {
                    "subscription_item": "si-sponsored"
                }},
                "pricing": {"price_details": {"price": "price-sponsored"}},
                "quantity": 3
            }]}
        });
        assert_eq!(
            sponsored_invoice_items(&invoice),
            vec![("si-sponsored".into(), "price-sponsored".into(), 3)]
        );
    }
}
