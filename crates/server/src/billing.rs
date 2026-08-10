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
use sqlx::{PgPool, Postgres, Transaction};

#[cfg(feature = "e2e-mock-billing")]
use url::Url;

use crate::auth::AuthUser;
use crate::config::BillingConfig;
use crate::error::{Error, Result};
use crate::state::AppState;
use crate::{audit, org};

/// Reject webhook timestamps further than this from now (replay protection).
const SIGNATURE_TOLERANCE_SECS: i64 = 300;
/// Subscription statuses that keep the Team tier. `past_due` stays paid while Stripe retries the
/// card (dunning) - losing entitlements over a bounced payment is the wrong first touch.
const ACTIVE_STATUSES: [&str; 3] = ["active", "trialing", "past_due"];

/// The small interface between billing handlers and an external payment provider.
#[async_trait]
pub trait BillingProvider: Send + Sync {
    async fn create_checkout(
        &self,
        org_id: &str,
        customer: Option<&str>,
        success_url: &str,
        cancel_url: &str,
    ) -> Result<String>;

    async fn create_portal(&self, customer: &str, return_url: &str) -> Result<String>;
}

/// Billing resources shared by handlers. The provider is swappable for the browser E2E build,
/// while webhook verification keeps its own secret regardless of which checkout adapter runs.
#[derive(Clone)]
pub struct BillingState {
    provider: Arc<dyn BillingProvider>,
    webhook_secret: String,
    return_url: String,
}

impl BillingState {
    pub fn from_config(config: BillingConfig) -> Self {
        let provider = StripeBilling {
            secret_key: config.secret_key.clone(),
            price_id: config.price_id,
        };
        Self {
            provider: Arc::new(provider),
            webhook_secret: config.webhook_secret,
            return_url: config.return_url,
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
    secret_key: String,
    price_id: String,
}

#[async_trait]
impl BillingProvider for StripeBilling {
    async fn create_checkout(
        &self,
        org_id: &str,
        customer: Option<&str>,
        success_url: &str,
        cancel_url: &str,
    ) -> Result<String> {
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

        let session = stripe_post(&self.secret_key, "checkout/sessions", &form).await?;
        session["url"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| Error::Upstream("stripe checkout session had no url".into()))
    }

    async fn create_portal(&self, customer: &str, return_url: &str) -> Result<String> {
        let form = vec![
            ("customer".to_string(), customer.to_string()),
            ("return_url".to_string(), return_url.to_string()),
        ];
        let session = stripe_post(&self.secret_key, "billing_portal/sessions", &form).await?;
        session["url"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| Error::Upstream("stripe portal session had no url".into()))
    }
}

#[cfg(feature = "e2e-mock-billing")]
struct E2eBilling {
    provider_origin: String,
}

#[cfg(feature = "e2e-mock-billing")]
#[async_trait]
impl BillingProvider for E2eBilling {
    async fn create_checkout(
        &self,
        _org_id: &str,
        _customer: Option<&str>,
        success_url: &str,
        cancel_url: &str,
    ) -> Result<String> {
        self.page_url(
            "checkout",
            &[("success_url", success_url), ("cancel_url", cancel_url)],
        )
    }

    async fn create_portal(&self, _customer: &str, return_url: &str) -> Result<String> {
        self.page_url("portal", &[("return_url", return_url)])
    }
}

#[cfg(feature = "e2e-mock-billing")]
impl E2eBilling {
    fn page_url(&self, page: &str, params: &[(&str, &str)]) -> Result<String> {
        let base = format!(
            "{}/e2e/billing/{page}",
            self.provider_origin.trim_end_matches('/')
        );
        let mut url = Url::parse(&base).map_err(|e| Error::Config(e.to_string()))?;
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
        .await?;
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
        .await?;
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

/// One form-encoded call to the Stripe API.
async fn stripe_post(
    secret_key: &str,
    path: &str,
    form: &[(String, String)],
) -> Result<serde_json::Value> {
    let response = stripe_client()
        .post(format!("https://api.stripe.com/v1/{path}"))
        .bearer_auth(secret_key)
        .form(form)
        .send()
        .await
        .map_err(|e| Error::Upstream(format!("stripe: {e}")))?;
    let status = response.status();
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|e| Error::Upstream(format!("stripe: {e}")))?;
    if !status.is_success() {
        let message = body["error"]["message"]
            .as_str()
            .unwrap_or("request failed");
        return Err(Error::Upstream(format!("stripe {path}: {message}")));
    }
    Ok(body)
}

// --- webhook -------------------------------------------------------------------------------------

/// The slice of a Stripe event we act on; everything else in the payload is ignored.
#[derive(Deserialize)]
struct Event {
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
    let object = &event.data.object;
    match event.kind.as_str() {
        "checkout.session.completed" => checkout_completed(&state.pool, object).await,
        "customer.subscription.updated" => subscription_updated(&state.pool, object).await,
        "customer.subscription.deleted" => subscription_deleted(&state.pool, object).await,
        _ => Ok(()),
    }
}

/// A paid checkout: record the Stripe ids and grant the Team tier. Idempotent - a redelivered
/// event changes no rows and writes no duplicate audit entry.
async fn checkout_completed(pool: &PgPool, object: &serde_json::Value) -> Result<()> {
    // Sessions this server creates always carry the org id; anything else isn't ours to act on.
    let Some(org_id) = object["client_reference_id"].as_str() else {
        return Ok(());
    };
    let customer = object["customer"].as_str();
    let subscription = object["subscription"].as_str();

    let mut tx = pool.begin().await?;
    let changed = sqlx::query(
        "UPDATE organizations \
         SET tier = 'team', stripe_customer_id = $2, stripe_subscription_id = $3 \
         WHERE id = $1 AND (tier <> 'team' \
            OR stripe_customer_id IS DISTINCT FROM $2 \
            OR stripe_subscription_id IS DISTINCT FROM $3)",
    )
    .bind(org_id)
    .bind(customer)
    .bind(subscription)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if changed > 0 {
        audit::record_tx(
            &mut tx,
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
    tx.commit().await?;
    Ok(())
}

/// A subscription lifecycle change: the status decides the tier. Handles late/failed payments
/// (`unpaid` → free) and recoveries (`active` again → team).
async fn subscription_updated(pool: &PgPool, object: &serde_json::Value) -> Result<()> {
    let Some(org_id) = org_for_subscription(pool, object).await? else {
        return Ok(());
    };
    let status = object["status"].as_str().unwrap_or_default();
    let tier = if ACTIVE_STATUSES.contains(&status) {
        "team"
    } else {
        "free"
    };

    let mut tx = pool.begin().await?;
    let changed = sqlx::query("UPDATE organizations SET tier = $2 WHERE id = $1 AND tier <> $2")
        .bind(&org_id)
        .bind(tier)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if changed > 0 {
        audit::record_tx(
            &mut tx,
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
    tx.commit().await?;
    Ok(())
}

/// The subscription ended for good: back to the free tier (existing data stays readable - the
/// entitlement gates are creation-time only).
async fn subscription_deleted(pool: &PgPool, object: &serde_json::Value) -> Result<()> {
    let Some(org_id) = org_for_subscription(pool, object).await? else {
        return Ok(());
    };
    let mut tx = pool.begin().await?;
    let changed = sqlx::query(
        "UPDATE organizations SET tier = 'free', stripe_subscription_id = NULL \
         WHERE id = $1 AND (tier <> 'free' OR stripe_subscription_id IS NOT NULL)",
    )
    .bind(&org_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if changed > 0 {
        audit::record_tx(
            &mut tx,
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
    tx.commit().await?;
    Ok(())
}

/// Name the org for a subscription event: the metadata stamped at checkout, else the stored
/// subscription id (covers subscriptions relinked by Stripe support), else not ours.
async fn org_for_subscription(pool: &PgPool, object: &serde_json::Value) -> Result<Option<String>> {
    if let Some(org_id) = object["metadata"]["org_id"].as_str() {
        return Ok(Some(org_id.to_string()));
    }
    let Some(subscription_id) = object["id"].as_str() else {
        return Ok(None);
    };
    Ok(
        sqlx::query_scalar("SELECT id FROM organizations WHERE stripe_subscription_id = $1")
            .bind(subscription_id)
            .fetch_optional(pool)
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
}
