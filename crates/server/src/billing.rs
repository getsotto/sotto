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
#[cfg(feature = "e2e-mock-billing")]
use axum::extract::Query;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
#[cfg(feature = "e2e-mock-billing")]
use axum::response::Html;
#[cfg(feature = "e2e-mock-billing")]
use axum::routing::get;
use axum::routing::post;
use axum::{Json, Router};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use sqlx::{Postgres, Transaction};

#[cfg(feature = "e2e-mock-billing")]
use url::Url;

use crate::auth::AuthUser;
use crate::config::BillingConfig;
use crate::error::{Error, Result};
use crate::state::AppState;
use crate::{audit, org};

/// Reject webhook timestamps further than this from now (replay protection).
const SIGNATURE_TOLERANCE_SECS: i64 = 300;
/// Every Stripe request and webhook fixture uses the same version so response shapes cannot drift
/// underneath billing or deletion decisions.
pub const STRIPE_API_VERSION: &str = "2026-07-29.dahlia";

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

    #[cfg(feature = "e2e-mock-billing")]
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

    fn entitlement_tier(&self) -> &'static str {
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

impl SubscriptionObservation {
    pub fn purge_gate(&self) -> PurgeGate {
        match self {
            Self::Current(snapshot) => snapshot.status.purge_gate(),
            Self::Missing => PurgeGate::Missing,
        }
    }
}

/// The small interface between billing handlers and an external payment provider.
#[async_trait]
pub trait SubscriptionProvider: Send + Sync {
    async fn create_checkout(
        &self,
        org_id: &str,
        customer: Option<&str>,
        success_url: &str,
        cancel_url: &str,
    ) -> ProviderResult<String>;

    async fn create_portal(&self, customer: &str, return_url: &str) -> ProviderResult<String>;

    async fn get_subscription(
        &self,
        subscription_id: &str,
    ) -> ProviderResult<SubscriptionObservation>;

    async fn cancel_subscription(
        &self,
        subscription_id: &str,
        idempotency_key: &str,
        org_id: &str,
    ) -> ProviderResult<SubscriptionObservation>;
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
}

impl BillingState {
    pub fn from_config(config: BillingConfig) -> Self {
        let provider = StripeBilling {
            api_key: config.api_key.clone(),
            price_id: config.price_id,
        };
        Self {
            provider: Arc::new(provider),
            webhook_secret: config.webhook_secret,
            return_url: config.return_url,
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
        }
    }

    #[cfg(feature = "e2e-mock-billing")]
    pub fn with_e2e_provider(config: BillingConfig, provider_origin: String) -> Self {
        Self {
            provider: Arc::new(E2eBilling { provider_origin }),
            webhook_secret: config.webhook_secret,
            return_url: config.return_url,
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
    let status = object["status"]
        .as_str()
        .ok_or_else(ProviderError::malformed_response)?;
    Ok(SubscriptionSnapshot {
        id: object["id"].as_str().unwrap_or(requested_id).to_string(),
        status: SubscriptionStatus::parse(status),
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
    if event.api_version.as_deref() != Some(STRIPE_API_VERSION) {
        let mut tx = state.pool.begin().await?;
        let inserted = record_webhook_receipt(&mut tx, &event, None).await?;
        mark_webhook_event_processed(&mut tx, &event.id).await?;
        tx.commit().await?;
        if inserted {
            eprintln!(
                "warning: ignored Stripe webhook {} with API version {}",
                event.id,
                event.api_version.as_deref().unwrap_or("missing")
            );
        }
        return Ok(());
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
    if disposition == EventDisposition::Reconcile {
        tx.commit().await?;
        let subscription_id = subscription_id.ok_or_else(|| {
            Error::Config("stripe reconciliation event has no subscription id".into())
        })?;
        let observation = billing
            .provider
            .get_subscription(&subscription_id)
            .await
            .map_err(ProviderError::into_error)?;

        let mut tx = state.pool.begin().await?;
        lock_subscription(&mut tx, &subscription_id).await?;
        let processed: bool = sqlx::query_scalar(
            "SELECT processed_at IS NOT NULL FROM stripe_webhook_events WHERE event_id = $1",
        )
        .bind(&event.id)
        .fetch_one(&mut *tx)
        .await?;
        if processed {
            tx.commit().await?;
            return Ok(());
        }
        let watermark: Option<(i64, String)> = sqlx::query_as(
            "SELECT stripe_created, event_id FROM stripe_subscription_watermarks \
             WHERE subscription_id = $1",
        )
        .bind(&subscription_id)
        .fetch_optional(&mut *tx)
        .await?;
        if watermark.is_some_and(|(created, event_id)| {
            created > event.created || (created == event.created && event_id > event.id)
        }) {
            mark_webhook_event_processed(&mut tx, &event.id).await?;
        } else {
            reconcile_subscription(
                &mut tx,
                observation,
                &subscription_id,
                event_org_hint(&event),
            )
            .await?;
            update_subscription_watermark(&mut tx, &event, &subscription_id).await?;
            mark_webhook_event_processed(&mut tx, &event.id).await?;
        }
        tx.commit().await?;
        return Ok(());
    }

    match disposition {
        EventDisposition::Apply => match event.kind.as_str() {
            "checkout.session.completed" => checkout_completed(&mut tx, object).await?,
            "customer.subscription.updated" => subscription_updated(&mut tx, object).await?,
            "customer.subscription.deleted" => subscription_deleted(&mut tx, object).await?,
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
        "checkout.session.completed" => event.data.object["subscription"]
            .as_str()
            .map(str::to_string),
        "customer.subscription.updated" | "customer.subscription.deleted" => {
            event.data.object["id"].as_str().map(str::to_string)
        }
        _ => None,
    }
}

fn event_org_hint(event: &Event) -> Option<&str> {
    match event.kind.as_str() {
        "checkout.session.completed" => event.data.object["client_reference_id"].as_str(),
        "customer.subscription.updated" | "customer.subscription.deleted" => {
            event.data.object["metadata"]["org_id"].as_str()
        }
        _ => None,
    }
}

/// A paid checkout: record the Stripe ids and grant the Team tier. Idempotent - a redelivered
/// event changes no rows and writes no duplicate audit entry.
async fn checkout_completed(
    tx: &mut Transaction<'_, Postgres>,
    object: &serde_json::Value,
) -> Result<()> {
    // Sessions this server creates always carry the org id; anything else isn't ours to act on.
    let Some(org_id) = object["client_reference_id"].as_str() else {
        return Ok(());
    };
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

/// A subscription lifecycle change: the status decides the tier. Handles late/failed payments
/// (`unpaid` → free) and recoveries (`active` again → team).
async fn subscription_updated(
    tx: &mut Transaction<'_, Postgres>,
    object: &serde_json::Value,
) -> Result<()> {
    let Some(org_id) = org_for_subscription(tx, object).await? else {
        return Ok(());
    };
    let Some(status) = object["status"].as_str() else {
        eprintln!(
            "warning: ignored Stripe subscription event without a status for organisation {org_id}"
        );
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

/// The subscription ended for good: back to the free tier (existing data stays readable - the
/// entitlement gates are creation-time only).
async fn subscription_deleted(
    tx: &mut Transaction<'_, Postgres>,
    object: &serde_json::Value,
) -> Result<()> {
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
) -> Result<()> {
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

/// Verify a `Stripe-Signature` header: `t=<unix>,v1=<hex hmac>[,v1=…]`, where the MAC is
/// HMAC-SHA256 over `"{t}.{payload}"`. Any valid `v1` within the timestamp tolerance passes
/// (Stripe sends multiples during secret rotation); comparison is constant-time via the `hmac`
/// crate's `verify_slice`.
fn verify_signature(secret: &str, header: &str, payload: &str, now: i64) -> bool {
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
    let Some(t) = timestamp else { return false };
    if (now - t).abs() > SIGNATURE_TOLERANCE_SECS || candidates.is_empty() {
        return false;
    }
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(t.to_string().as_bytes());
    mac.update(b".");
    mac.update(payload.as_bytes());
    candidates
        .into_iter()
        .any(|candidate| mac.clone().verify_slice(&candidate).is_ok())
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
    fn cancellation_form_is_explicit_and_traceable() {
        let form = cancellation_form("org-123");
        assert!(form.contains(&("invoice_now".into(), "false".into())));
        assert!(form.contains(&("prorate".into(), "false".into())));
        assert!(form.iter().any(|(key, value)| {
            key == "cancellation_details[comment]" && value.contains("org-123")
        }));
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
}
