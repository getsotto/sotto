//! Billing integration tests: webhook signature enforcement, tier assignment, idempotency, and
//! the configuration/role gates. DB-gated like the other server tests; no test ever calls the
//! real Stripe API (the only handler paths exercised return before any outbound request).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use sqlx::PgPool;
use tower::ServiceExt;

use sotto_server::auth::session;
use sotto_server::config::BillingConfig;
use sotto_server::db;
use sotto_server::state::AppState;

const WEBHOOK_SECRET: &str = "whsec_test_secret";

async fn pool_or_skip() -> Option<PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = db::connect(&url).await.expect("connect");
    db::migrate(&pool).await.expect("migrate");
    Some(pool)
}

fn app(pool: PgPool, configured: bool) -> Router {
    let state = AppState {
        telemetry_ingest: false,
        pool,
        oauth: None,
        oauth_config: None,
        billing: configured.then(|| BillingConfig {
            secret_key: "sk_test_never_called".into(),
            webhook_secret: WEBHOOK_SECRET.into(),
            price_id: "price_test".into(),
            return_url: "https://app.sotto.test".into(),
        }),
    };
    Router::new()
        .merge(sotto_server::billing::router())
        .with_state(state)
}

async fn seed_user(pool: &PgPool, user_id: &str) -> String {
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("pre-clean user");
    sqlx::query("INSERT INTO users (id, oauth_provider, oauth_subject) VALUES ($1, 'github', $1)")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("insert user");
    session::issue(pool, user_id).await.expect("issue session")
}

/// An org with the given tier and one membership. `enc_name` is opaque bytes - the server never
/// reads it, so a fixed placeholder is fine.
async fn seed_org(pool: &PgPool, org_id: &str, tier: &str, user_id: &str, role: &str) {
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org_id)
        .execute(pool)
        .await
        .expect("pre-clean org");
    sqlx::query("INSERT INTO organizations (id, enc_name, tier) VALUES ($1, $2, $3)")
        .bind(org_id)
        .bind(b"opaque".as_slice())
        .bind(tier)
        .execute(pool)
        .await
        .expect("insert org");
    sqlx::query("INSERT INTO organization_memberships (org_id, user_id, role) VALUES ($1, $2, $3)")
        .bind(org_id)
        .bind(user_id)
        .bind(role)
        .execute(pool)
        .await
        .expect("insert membership");
}

fn stripe_signature(payload: &str) -> String {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut mac = Hmac::<Sha256>::new_from_slice(WEBHOOK_SECRET.as_bytes()).unwrap();
    mac.update(format!("{t}.{payload}").as_bytes());
    let hex: String = mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("t={t},v1={hex}")
}

async fn post_webhook(app: &Router, payload: &str, signature: Option<&str>) -> StatusCode {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/billing/webhook")
        .header("content-type", "application/json");
    if let Some(sig) = signature {
        builder = builder.header("Stripe-Signature", sig);
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::from(payload.to_string())).unwrap())
        .await
        .expect("request");
    response.status()
}

async fn post_authed(app: &Router, uri: &str, token: &str) -> StatusCode {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("request");
    response.status()
}

async fn org_billing_state(
    pool: &PgPool,
    org_id: &str,
) -> (String, Option<String>, Option<String>) {
    sqlx::query_as(
        "SELECT tier, stripe_customer_id, stripe_subscription_id FROM organizations WHERE id = $1",
    )
    .bind(org_id)
    .fetch_one(pool)
    .await
    .expect("org row")
}

async fn audit_count(pool: &PgPool, org_id: &str, action: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE org_id = $1 AND action = $2")
        .bind(org_id)
        .bind(action)
        .fetch_one(pool)
        .await
        .expect("audit count")
}

#[tokio::test]
async fn billing_endpoints_are_503_when_unconfigured() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let token = seed_user(&pool, "billing-user-unconf").await;
    seed_org(
        &pool,
        "billing-org-unconf",
        "free",
        "billing-user-unconf",
        "owner",
    )
    .await;
    let app = app(pool, false);

    let status = post_authed(&app, "/orgs/billing-org-unconf/billing/checkout", &token).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let status = post_webhook(&app, "{}", Some("t=1,v1=00")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn webhook_rejects_missing_and_invalid_signatures() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let app = app(pool, true);

    assert_eq!(
        post_webhook(&app, "{}", None).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        post_webhook(&app, "{}", Some("t=1000,v1=deadbeef")).await,
        StatusCode::UNAUTHORIZED
    );
    // A valid signature over DIFFERENT content must not authenticate this body.
    let other = stripe_signature("{\"other\":true}");
    assert_eq!(
        post_webhook(&app, "{}", Some(&other)).await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn checkout_completed_grants_team_and_audits_once() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    seed_user(&pool, "billing-user-co").await;
    seed_org(&pool, "billing-org-co", "free", "billing-user-co", "owner").await;
    let app = app(pool.clone(), true);

    let payload = serde_json::json!({
        "type": "checkout.session.completed",
        "data": { "object": {
            "client_reference_id": "billing-org-co",
            "customer": "cus_test_1",
            "subscription": "sub_test_1",
        }}
    })
    .to_string();
    let signature = stripe_signature(&payload);

    assert_eq!(
        post_webhook(&app, &payload, Some(&signature)).await,
        StatusCode::OK
    );
    let (tier, customer, subscription) = org_billing_state(&pool, "billing-org-co").await;
    assert_eq!(tier, "team");
    assert_eq!(customer.as_deref(), Some("cus_test_1"));
    assert_eq!(subscription.as_deref(), Some("sub_test_1"));
    assert_eq!(
        audit_count(&pool, "billing-org-co", "billing.subscribed").await,
        1
    );

    // Stripe redelivers webhooks; a duplicate must change nothing and not double-audit.
    assert_eq!(
        post_webhook(&app, &payload, Some(&signature)).await,
        StatusCode::OK
    );
    assert_eq!(
        audit_count(&pool, "billing-org-co", "billing.subscribed").await,
        1
    );
}

#[tokio::test]
async fn subscription_status_governs_tier() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    seed_user(&pool, "billing-user-status").await;
    seed_org(
        &pool,
        "billing-org-status",
        "free",
        "billing-user-status",
        "owner",
    )
    .await;
    let app = app(pool.clone(), true);

    let event = |status: &str| {
        serde_json::json!({
            "type": "customer.subscription.updated",
            "data": { "object": {
                "id": "sub_test_status",
                "status": status,
                "metadata": { "org_id": "billing-org-status" },
            }}
        })
        .to_string()
    };

    let active = event("active");
    assert_eq!(
        post_webhook(&app, &active, Some(&stripe_signature(&active))).await,
        StatusCode::OK
    );
    assert_eq!(
        org_billing_state(&pool, "billing-org-status").await.0,
        "team"
    );

    // Dunning keeps the lights on…
    let past_due = event("past_due");
    assert_eq!(
        post_webhook(&app, &past_due, Some(&stripe_signature(&past_due))).await,
        StatusCode::OK
    );
    assert_eq!(
        org_billing_state(&pool, "billing-org-status").await.0,
        "team"
    );

    // …but a lost subscription does not.
    let unpaid = event("unpaid");
    assert_eq!(
        post_webhook(&app, &unpaid, Some(&stripe_signature(&unpaid))).await,
        StatusCode::OK
    );
    assert_eq!(
        org_billing_state(&pool, "billing-org-status").await.0,
        "free"
    );

    // team → team and free → free transitions were no-ops audit-wise: only the 2 real changes.
    assert_eq!(
        audit_count(&pool, "billing-org-status", "billing.updated").await,
        2
    );
}

#[tokio::test]
async fn subscription_deleted_downgrades_via_stored_id() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    seed_user(&pool, "billing-user-del").await;
    seed_org(
        &pool,
        "billing-org-del",
        "team",
        "billing-user-del",
        "owner",
    )
    .await;
    sqlx::query("UPDATE organizations SET stripe_subscription_id = 'sub_test_del' WHERE id = $1")
        .bind("billing-org-del")
        .execute(&pool)
        .await
        .expect("link subscription");
    let app = app(pool.clone(), true);

    // No metadata on this event - the org must be found via the stored subscription id.
    let payload = serde_json::json!({
        "type": "customer.subscription.deleted",
        "data": { "object": { "id": "sub_test_del" } }
    })
    .to_string();
    assert_eq!(
        post_webhook(&app, &payload, Some(&stripe_signature(&payload))).await,
        StatusCode::OK
    );
    let (tier, _, subscription) = org_billing_state(&pool, "billing-org-del").await;
    assert_eq!(tier, "free");
    assert_eq!(subscription, None);
    assert_eq!(
        audit_count(&pool, "billing-org-del", "billing.cancelled").await,
        1
    );
}

#[tokio::test]
async fn unhandled_events_are_acknowledged() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let app = app(pool, true);
    let payload = serde_json::json!({
        "type": "invoice.created",
        "data": { "object": {} }
    })
    .to_string();
    assert_eq!(
        post_webhook(&app, &payload, Some(&stripe_signature(&payload))).await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn checkout_requires_the_admin_role() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let owner_token = seed_user(&pool, "billing-user-owner").await;
    let member_token = seed_user(&pool, "billing-user-member").await;
    let outsider_token = seed_user(&pool, "billing-user-outsider").await;
    seed_org(
        &pool,
        "billing-org-roles",
        "free",
        "billing-user-owner",
        "owner",
    )
    .await;
    sqlx::query(
        "INSERT INTO organization_memberships (org_id, user_id, role) \
         VALUES ('billing-org-roles', 'billing-user-member', 'member')",
    )
    .execute(&pool)
    .await
    .expect("add member");
    let app = app(pool, true);

    // Plain members can't touch billing; non-members can't see the org exists. (The owner path
    // isn't exercised end-to-end here - it would call the real Stripe API.)
    let status = post_authed(
        &app,
        "/orgs/billing-org-roles/billing/checkout",
        &member_token,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let status = post_authed(
        &app,
        "/orgs/billing-org-roles/billing/checkout",
        &outsider_token,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // And the portal, before any Stripe call, requires a billing account to exist.
    let status = post_authed(&app, "/orgs/billing-org-roles/billing/portal", &owner_token).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
