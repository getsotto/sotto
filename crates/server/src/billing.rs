//! Stripe billing: subscription checkout, the customer portal, and the webhook that assigns tiers.
//!
//! Deliberately thin: entitlements ([`crate::entitlements`]) already gate everything on
//! `organizations.tier`, so this module's only real job is flipping that column in response to
//! **signature-verified** Stripe webhooks. Checkout and the portal are Stripe-hosted pages — the
//! server hands the browser a redirect URL and never touches card data.
//!
//! Ships dark: without the `STRIPE_*` environment variables every endpoint returns 503 (the OAuth
//! pattern). Zero-knowledge is unaffected — Stripe learns an org *id* and whatever the payer types
//! into Stripe's own pages; org names, membership, and vault data never leave the server.

use std::sync::OnceLock;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::routing::post;
use axum::{Json, Router};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use sqlx::PgPool;

use crate::auth::AuthUser;
use crate::config::BillingConfig;
use crate::error::{Error, Result};
use crate::state::AppState;
use crate::{audit, org};

/// Reject webhook timestamps further than this from now (replay protection).
const SIGNATURE_TOLERANCE_SECS: i64 = 300;
/// Subscription statuses that keep the Team tier. `past_due` stays paid while Stripe retries the
/// card (dunning) — losing entitlements over a bounced payment is the wrong first touch.
const ACTIVE_STATUSES: [&str; 3] = ["active", "trialing", "past_due"];

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/orgs/{org_id}/billing/checkout", post(create_checkout))
        .route("/orgs/{org_id}/billing/portal", post(create_portal))
        .route("/billing/webhook", post(webhook))
}

fn billing_config(state: &AppState) -> Result<&BillingConfig> {
    state
        .billing
        .as_ref()
        .ok_or_else(|| Error::NotConfigured("billing is not configured".into()))
}

/// Billing is admin+: the same bar as membership management, and a non-member sees a 404.
async fn require_billing_admin(pool: &PgPool, org_id: &str, user_id: &str) -> Result<()> {
    match org::role_of(pool, org_id, user_id).await? {
        Some(role) if role.can_manage_members() => Ok(()),
        Some(_) => Err(Error::Forbidden(
            "managing billing requires the admin or owner role".into(),
        )),
        None => Err(Error::NotFound("organization not found".into())),
    }
}

/// A Stripe-hosted page for the browser to navigate to.
#[derive(Serialize)]
struct RedirectView {
    url: String,
}

/// `POST /orgs/{org_id}/billing/checkout` — start a Team subscription (admin+). Returns the URL of
/// a Stripe Checkout page; the tier flips when the `checkout.session.completed` webhook arrives.
async fn create_checkout(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<String>,
) -> Result<Json<RedirectView>> {
    let billing = billing_config(&state)?;
    require_billing_admin(&state.pool, &org_id, &user.user_id).await?;

    // Reuse the org's Stripe customer if one exists, so a cancel/resubscribe doesn't fork billing
    // history; otherwise Checkout creates one and the webhook records it.
    let customer: Option<String> =
        sqlx::query_scalar("SELECT stripe_customer_id FROM organizations WHERE id = $1")
            .bind(&org_id)
            .fetch_optional(&state.pool)
            .await?
            .flatten();

    let (success_url, cancel_url) = checkout_return_urls(&billing.return_url);
    let mut form = vec![
        ("mode".to_string(), "subscription".to_string()),
        ("line_items[0][price]".to_string(), billing.price_id.clone()),
        ("line_items[0][quantity]".to_string(), "1".to_string()),
        ("client_reference_id".to_string(), org_id.clone()),
        // Mirrored onto the subscription so its lifecycle webhooks name the org even if they
        // arrive before (or without) the checkout-completed event.
        (
            "subscription_data[metadata][org_id]".to_string(),
            org_id.clone(),
        ),
        ("success_url".to_string(), success_url),
        ("cancel_url".to_string(), cancel_url),
    ];
    if let Some(customer) = customer {
        form.push(("customer".to_string(), customer));
    }

    let session = stripe_post(&billing.secret_key, "checkout/sessions", &form).await?;
    let url = session["url"]
        .as_str()
        .ok_or_else(|| Error::Upstream("stripe checkout session had no url".into()))?;
    Ok(Json(RedirectView {
        url: url.to_string(),
    }))
}

/// `POST /orgs/{org_id}/billing/portal` — manage/cancel the subscription (admin+) via Stripe's
/// hosted customer portal.
async fn create_portal(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<String>,
) -> Result<Json<RedirectView>> {
    let billing = billing_config(&state)?;
    require_billing_admin(&state.pool, &org_id, &user.user_id).await?;

    let customer: Option<String> =
        sqlx::query_scalar("SELECT stripe_customer_id FROM organizations WHERE id = $1")
            .bind(&org_id)
            .fetch_optional(&state.pool)
            .await?
            .flatten();
    let customer = customer.ok_or_else(|| {
        Error::BadRequest("this organization has no billing account yet — subscribe first".into())
    })?;

    let form = vec![
        ("customer".to_string(), customer),
        ("return_url".to_string(), app_url(&billing.return_url)),
    ];
    let session = stripe_post(&billing.secret_key, "billing_portal/sessions", &form).await?;
    let url = session["url"]
        .as_str()
        .ok_or_else(|| Error::Upstream("stripe portal session had no url".into()))?;
    Ok(Json(RedirectView {
        url: url.to_string(),
    }))
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
/// (`auth::oauth`): a stalled Stripe — slow DNS/TLS, a hung connection — must not tie up the request
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

/// `POST /billing/webhook` — Stripe's event delivery. Signature-verified against the endpoint's
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

/// A paid checkout: record the Stripe ids and grant the Team tier. Idempotent — a redelivered
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

/// The subscription ended for good: back to the free tier (existing data stays readable — the
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
