//! Billing integration tests: webhook signature enforcement, tier assignment, idempotency, and
//! the configuration/role gates. DB-gated like the other server tests; provider calls use the
//! in-memory adapter and no test calls the real Stripe API.

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use sqlx::PgPool;
use std::sync::Arc;
use tower::ServiceExt;

use sotto_server::auth::session;
use sotto_server::billing::{
    BillingState, ProviderResult, SubscriptionObservation, SubscriptionProvider,
    SubscriptionSnapshot, SubscriptionStatus, STRIPE_API_VERSION,
};
use sotto_server::billing_catalogue::BillingOffer;
use sotto_server::billing_operations::{self, BeginOperation};
use sotto_server::config::{BillingConfig, DEFAULT_ORGANISATION_DELETION_RETENTION_DAYS};
use sotto_server::db;
use sotto_server::founding_allocator::FoundingDate;
use sotto_server::personal_billing;
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
        deployment_mode: sotto_server::config::DeploymentMode::SelfHosted,
        telemetry_ingest: false,
        cloud_action_enforcement_enabled: false,
        machine_eligibility_enforcement_enabled: false,
        pool,
        oauth: None,
        oauth_config: None,
        billing: configured.then(|| {
            BillingState::from_config(BillingConfig {
                api_key: "rk_test_never_called".into(),
                webhook_secret: WEBHOOK_SECRET.into(),
                price_id: "price_test".into(),
                price_catalogue: None,
                cloud_sales_enabled: true,
                return_url: "https://app.sotto.test".into(),
            })
        }),
        organisation_deletion_enabled: false,
        organisation_deletion_retention_days: DEFAULT_ORGANISATION_DELETION_RETENTION_DAYS,
        organisation_deletion_metrics_token: None,
        organisation_deletion_operator_token: None,
    };
    Router::new()
        .merge(sotto_server::billing::router())
        .with_state(state)
}

struct TestProvider {
    observation: SubscriptionObservation,
    personal_period_end: Option<i64>,
}

#[async_trait]
impl SubscriptionProvider for TestProvider {
    async fn create_checkout(
        &self,
        _org_id: &str,
        _customer: Option<&str>,
        _success_url: &str,
        _cancel_url: &str,
    ) -> ProviderResult<String> {
        Ok("https://stripe.test/checkout".into())
    }

    async fn create_portal(&self, _customer: &str, _return_url: &str) -> ProviderResult<String> {
        Ok("https://stripe.test/portal".into())
    }

    async fn get_subscription(
        &self,
        _subscription_id: &str,
    ) -> ProviderResult<SubscriptionObservation> {
        Ok(self.observation.clone())
    }

    async fn personal_subscription_period_end(
        &self,
        _subscription_id: &str,
    ) -> ProviderResult<Option<i64>> {
        Ok(self.personal_period_end)
    }

    async fn cancel_subscription(
        &self,
        _subscription_id: &str,
        _operation_id: &str,
        _organisation_id: &str,
    ) -> ProviderResult<SubscriptionObservation> {
        Ok(self.observation.clone())
    }
}

fn app_with_provider(pool: PgPool, provider: Arc<dyn SubscriptionProvider>) -> Router {
    let state = AppState {
        deployment_mode: sotto_server::config::DeploymentMode::SelfHosted,
        telemetry_ingest: false,
        cloud_action_enforcement_enabled: false,
        machine_eligibility_enforcement_enabled: false,
        pool,
        oauth: None,
        oauth_config: None,
        billing: Some(BillingState::with_provider_and_cloud_sales(
            provider,
            WEBHOOK_SECRET.into(),
            "https://app.sotto.test".into(),
            true,
        )),
        organisation_deletion_enabled: false,
        organisation_deletion_retention_days: DEFAULT_ORGANISATION_DELETION_RETENTION_DAYS,
        organisation_deletion_metrics_token: None,
        organisation_deletion_operator_token: None,
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

async fn seed_personal_account(pool: &PgPool, user_id: &str, idempotency_key: &str) -> String {
    let mut tx = pool.begin().await.expect("begin personal account seed");
    let operation = match billing_operations::begin_personal_operation(
        &mut tx,
        user_id,
        idempotency_key,
        BillingOffer::StandardMonthly,
        1,
        2_000_000_000,
        "https://app.sotto.test",
        "https://app.sotto.test",
    )
    .await
    .expect("seed personal operation")
    {
        BeginOperation::Created(operation) | BeginOperation::AlreadyExists(operation) => operation,
    };
    personal_billing::begin_account(
        &mut tx,
        user_id,
        &operation.operation_id,
        BillingOffer::StandardMonthly,
        2_000_000_000,
        1_700_000_000,
    )
    .await
    .expect("seed personal account");
    tx.commit().await.expect("commit personal account seed");
    operation.operation_id
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
async fn billing_writes_are_frozen_during_organisation_deletion() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let org_id = "billing-org-deleting";
    let user_id = "billing-user-deleting";
    let token = seed_user(&pool, user_id).await;
    seed_org(&pool, org_id, "team", user_id, "owner").await;
    sqlx::query("UPDATE organizations SET lifecycle_state = 'deleting' WHERE id = $1")
        .bind(org_id)
        .execute(&pool)
        .await
        .expect("mark org deleting");
    let app = app(pool, true);

    assert_eq!(
        post_authed(&app, &format!("/orgs/{org_id}/billing/checkout"), &token,).await,
        StatusCode::CONFLICT
    );
    assert_eq!(
        post_authed(&app, &format!("/orgs/{org_id}/billing/portal"), &token,).await,
        StatusCode::CONFLICT
    );
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

#[test]
fn the_versions_this_deployment_actually_receives_stay_accepted() {
    // Pinned literally rather than derived, because a test that iterates the constant only ever
    // proves the constant agrees with itself: delete the live endpoint's version from the list
    // and every other test here still passes while production silently stops applying payments.
    //
    // `2026-06-24.dahlia` is what the live webhook endpoint renders, fixed when the endpoint was
    // created and not editable afterwards. `2026-08-26.dahlia` is the account default, which is
    // what a recreated endpoint would inherit. Changing either is a deliberate act; changing this
    // list without one is the bug.
    for required in ["2026-06-24.dahlia", "2026-08-26.dahlia"] {
        assert!(
            sotto_server::billing::ACCEPTED_WEBHOOK_API_VERSIONS.contains(&required),
            "{required} is a version this deployment receives; removing it stops billing"
        );
    }
}

#[tokio::test]
async fn a_webhook_at_any_accepted_version_is_acted_on() {
    // Every version in the list has to work, not just the one the fixtures happen to use.
    // Stripe pins a webhook endpoint's version when the endpoint is created and will not let it
    // be edited, so an older deployment's endpoint keeps sending an older version for ever.
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    for (n, version) in sotto_server::billing::ACCEPTED_WEBHOOK_API_VERSIONS
        .iter()
        .enumerate()
    {
        let org = format!("billing-org-ver-{n}");
        let user = format!("billing-user-ver-{n}");
        seed_user(&pool, &user).await;
        seed_org(&pool, &org, "free", &user, "owner").await;
        let app = app(pool.clone(), true);

        let payload = serde_json::json!({
            "id": format!("evt_version_{n}"),
            "created": 100,
            "api_version": version,
            "type": "checkout.session.completed",
            "data": { "object": {
                "client_reference_id": org,
                "customer": format!("cus_ver_{n}"),
                "subscription": format!("sub_ver_{n}"),
            }}
        })
        .to_string();
        let signature = stripe_signature(&payload);

        assert_eq!(
            post_webhook(&app, &payload, Some(&signature)).await,
            StatusCode::OK,
            "version {version} should be accepted"
        );
        let (tier, _, _) = org_billing_state(&pool, &org).await;
        assert_eq!(
            tier, "team",
            "version {version} should have applied the tier"
        );
    }
}

#[tokio::test]
async fn a_webhook_at_an_unknown_version_fails_so_stripe_retries() {
    // The shape of the bug this replaced: answering 200 told Stripe the event was handled, so it
    // was never resent, and the tier silently never moved. Failing is what buys the retries that
    // let a version be added to the list and the backlog delivered.
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    seed_user(&pool, "billing-user-badver").await;
    seed_org(
        &pool,
        "billing-org-badver",
        "free",
        "billing-user-badver",
        "owner",
    )
    .await;
    let app = app(pool.clone(), true);

    let payload = serde_json::json!({
        "id": "evt_unknown_version",
        "created": 100,
        "api_version": "1999-01-01.jurassic",
        "type": "checkout.session.completed",
        "data": { "object": {
            "client_reference_id": "billing-org-badver",
            "customer": "cus_badver",
            "subscription": "sub_badver",
        }}
    })
    .to_string();
    let signature = stripe_signature(&payload);

    assert_eq!(
        post_webhook(&app, &payload, Some(&signature)).await,
        StatusCode::INTERNAL_SERVER_ERROR,
        "an unreadable version must not be reported as handled"
    );
    let (tier, _, _) = org_billing_state(&pool, "billing-org-badver").await;
    assert_eq!(
        tier, "free",
        "and must not have acted on the payload either"
    );
}

#[tokio::test]
async fn a_refused_webhook_is_still_processed_when_its_version_is_added() {
    // The other half of buying retries: refusing must not mark the event processed, or the
    // redelivery would be skipped as a duplicate and the retries would be worthless.
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    seed_user(&pool, "billing-user-retry").await;
    seed_org(
        &pool,
        "billing-org-retry",
        "free",
        "billing-user-retry",
        "owner",
    )
    .await;
    let app = app(pool.clone(), true);

    let refused = serde_json::json!({
        "id": "evt_retry_same_id",
        "created": 100,
        "api_version": "1999-01-01.jurassic",
        "type": "checkout.session.completed",
        "data": { "object": {
            "client_reference_id": "billing-org-retry",
            "customer": "cus_retry",
            "subscription": "sub_retry",
        }}
    })
    .to_string();
    assert_eq!(
        post_webhook(&app, &refused, Some(&stripe_signature(&refused))).await,
        StatusCode::INTERNAL_SERVER_ERROR
    );

    // Stripe redelivers the same event id once the version is one this server knows.
    let redelivered = serde_json::json!({
        "id": "evt_retry_same_id",
        "created": 100,
        "api_version": STRIPE_API_VERSION,
        "type": "checkout.session.completed",
        "data": { "object": {
            "client_reference_id": "billing-org-retry",
            "customer": "cus_retry",
            "subscription": "sub_retry",
        }}
    })
    .to_string();
    assert_eq!(
        post_webhook(&app, &redelivered, Some(&stripe_signature(&redelivered))).await,
        StatusCode::OK
    );
    let (tier, _, _) = org_billing_state(&pool, "billing-org-retry").await;
    assert_eq!(
        tier, "team",
        "the redelivery must not be skipped as already processed"
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
        "id": "evt_checkout_1",
        "created": 100,
        "api_version": STRIPE_API_VERSION,
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
async fn personal_paid_checkout_webhook_records_provider_term() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let user_id = "billing-user-personal-paid";
    seed_user(&pool, user_id).await;
    let operation_id = seed_personal_account(&pool, user_id, "personal-paid-webhook").await;
    let provider = Arc::new(TestProvider {
        observation: SubscriptionObservation::Missing,
        personal_period_end: Some(1_900_000_000),
    });
    let app = app_with_provider(pool.clone(), provider);
    let payload = serde_json::json!({
        "id": "evt_personal_paid_webhook",
        "created": 1_800_000_000,
        "api_version": STRIPE_API_VERSION,
        "type": "checkout.session.completed",
        "data": { "object": {
            "client_reference_id": format!("personal:{operation_id}"),
            "payment_status": "paid",
            "customer": "cus_personal_paid",
            "subscription": "sub_personal_paid",
            "payment_intent": "pi_personal_paid"
        }}
    })
    .to_string();

    assert_eq!(
        post_webhook(&app, &payload, Some(&stripe_signature(&payload))).await,
        StatusCode::OK
    );
    let account: (String, String, i64, String) = sqlx::query_as(
        "SELECT state, stripe_subscription_id, paid_through_epoch, payment_reference \
         FROM billing_personal_accounts WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .expect("personal account after paid webhook");
    assert_eq!(account.0, "active");
    assert_eq!(account.1, "sub_personal_paid");
    // Paid-through is stored as the UTC billing date anchor, so the provider's timestamp is
    // normalised to midnight rather than retaining the time-of-day component.
    assert_eq!(
        account.2,
        FoundingDate::from_unix_seconds(1_900_000_000)
            .unwrap()
            .to_unix_seconds()
    );
    assert_eq!(account.3, "pi_personal_paid");

    let subscription_update = serde_json::json!({
        "id": "evt_personal_paid_period",
        "created": 1_800_000_001,
        "api_version": STRIPE_API_VERSION,
        "type": "customer.subscription.updated",
        "data": { "object": {
            "id": "sub_personal_paid",
            "status": "active",
            "cancel_at_period_end": false,
            "current_period_end": 1_900_000_001,
            "items": {"data": [{"current_period_end": 2_000_000_000}]},
            "metadata": {"personal_user_id": user_id}
        }}
    })
    .to_string();
    assert_eq!(
        post_webhook(
            &app,
            &subscription_update,
            Some(&stripe_signature(&subscription_update))
        )
        .await,
        StatusCode::OK
    );
    let rollover_epoch: i64 = sqlx::query_scalar(
        "SELECT paid_through_epoch FROM billing_personal_accounts WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .expect("personal account after subscription rollover");
    assert_eq!(
        rollover_epoch,
        FoundingDate::from_unix_seconds(1_900_000_000)
            .unwrap()
            .to_unix_seconds()
    );

    let renewal_invoice = serde_json::json!({
        "id": "evt_personal_paid_invoice",
        "created": 1_800_000_002,
        "api_version": STRIPE_API_VERSION,
        "type": "invoice.paid",
        "data": { "object": {
            "id": "in_personal_paid",
            "payment_intent": "pi_billing_webhook_renewal",
            "period_end": 1_950_000_000,
            "lines": {"data": [{
                "parent": {
                    "type": "subscription_item_details",
                    "subscription_item_details": {
                        "subscription": "sub_personal_paid"
                    }
                },
                "period": {"end": 2_100_000_000}
            }]},
            "parent": {
                "type": "subscription_details",
                "subscription_details": {"subscription": "sub_personal_paid"}
            }
        }}
    })
    .to_string();
    assert_eq!(
        post_webhook(
            &app,
            &renewal_invoice,
            Some(&stripe_signature(&renewal_invoice))
        )
        .await,
        StatusCode::OK
    );
    let renewed_epoch: i64 = sqlx::query_scalar(
        "SELECT paid_through_epoch FROM billing_personal_accounts WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .expect("renewed personal account");
    assert_eq!(renewed_epoch, 2_100_000_000,);
}

#[tokio::test]
async fn personal_incomplete_expired_webhook_cancels_pending_account() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let user_id = "billing-user-personal-expired";
    seed_user(&pool, user_id).await;
    let operation_id = seed_personal_account(&pool, user_id, "personal-expired-webhook").await;
    let app = app_with_provider(
        pool.clone(),
        Arc::new(TestProvider {
            observation: SubscriptionObservation::Missing,
            personal_period_end: None,
        }),
    );
    let payload = serde_json::json!({
        "id": "evt_personal_incomplete_expired",
        "created": 1_800_000_001,
        "api_version": STRIPE_API_VERSION,
        "type": "customer.subscription.updated",
        "data": { "object": {
            "id": "sub_personal_expired",
            "status": "incomplete_expired",
            "cancel_at_period_end": false,
            "metadata": {
                "personal_user_id": user_id,
                "operation_id": operation_id
            }
        }}
    })
    .to_string();

    assert_eq!(
        post_webhook(&app, &payload, Some(&stripe_signature(&payload))).await,
        StatusCode::OK
    );
    let account: (String, String) = sqlx::query_as(
        "SELECT state, stripe_subscription_id FROM billing_personal_accounts WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .expect("personal account after incomplete expiry");
    assert_eq!(account.0, "canceled");
    assert_eq!(account.1, "sub_personal_expired");
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

    let event = |id: &str, created: i64, status: &str| {
        serde_json::json!({
            "id": id,
            "created": created,
            "api_version": STRIPE_API_VERSION,
            "type": "customer.subscription.updated",
            "data": { "object": {
                "id": "sub_test_status",
                "status": status,
                "metadata": { "org_id": "billing-org-status" },
            }}
        })
        .to_string()
    };

    let active = event("evt_status_active", 100, "active");
    assert_eq!(
        post_webhook(&app, &active, Some(&stripe_signature(&active))).await,
        StatusCode::OK
    );
    assert_eq!(
        org_billing_state(&pool, "billing-org-status").await.0,
        "team"
    );

    // Dunning keeps the lights on…
    let past_due = event("evt_status_past_due", 101, "past_due");
    assert_eq!(
        post_webhook(&app, &past_due, Some(&stripe_signature(&past_due))).await,
        StatusCode::OK
    );
    assert_eq!(
        org_billing_state(&pool, "billing-org-status").await.0,
        "team"
    );

    // …but a lost subscription does not.
    let unpaid = event("evt_status_unpaid", 102, "unpaid");
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
async fn older_subscription_events_cannot_overwrite_newer_tier_state() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    seed_user(&pool, "billing-user-order").await;
    seed_org(
        &pool,
        "billing-org-order",
        "free",
        "billing-user-order",
        "owner",
    )
    .await;
    let app = app(pool.clone(), true);
    let event = |id: &str, created: i64, status: &str| {
        serde_json::json!({
            "id": id,
            "created": created,
            "api_version": STRIPE_API_VERSION,
            "type": "customer.subscription.updated",
            "data": { "object": {
                "id": "sub_test_order",
                "status": status,
                "metadata": { "org_id": "billing-org-order" },
            }}
        })
        .to_string()
    };

    let newer = event("evt_order_new", 200, "active");
    assert_eq!(
        post_webhook(&app, &newer, Some(&stripe_signature(&newer))).await,
        StatusCode::OK
    );
    let older = event("evt_order_old", 100, "unpaid");
    assert_eq!(
        post_webhook(&app, &older, Some(&stripe_signature(&older))).await,
        StatusCode::OK
    );
    assert_eq!(
        org_billing_state(&pool, "billing-org-order").await.0,
        "team"
    );
}

#[tokio::test]
async fn equal_timestamp_events_reconcile_with_the_provider() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    seed_user(&pool, "billing-user-equal").await;
    seed_org(
        &pool,
        "billing-org-equal",
        "free",
        "billing-user-equal",
        "owner",
    )
    .await;
    let provider = Arc::new(TestProvider {
        observation: SubscriptionObservation::Current(SubscriptionSnapshot {
            id: "sub_test_equal".into(),
            status: SubscriptionStatus::Canceled,
        }),
        personal_period_end: None,
    });
    let app = app_with_provider(pool.clone(), provider);
    let event = |id: &str, status: &str| {
        serde_json::json!({
            "id": id,
            "created": 100,
            "api_version": STRIPE_API_VERSION,
            "type": "customer.subscription.updated",
            "data": { "object": {
                "id": "sub_test_equal",
                "status": status,
                "metadata": { "org_id": "billing-org-equal" },
            }}
        })
        .to_string()
    };

    let first = event("evt_equal_first", "active");
    assert_eq!(
        post_webhook(&app, &first, Some(&stripe_signature(&first))).await,
        StatusCode::OK
    );
    assert_eq!(
        org_billing_state(&pool, "billing-org-equal").await.0,
        "team"
    );
    let second = event("evt_equal_second", "active");
    assert_eq!(
        post_webhook(&app, &second, Some(&stripe_signature(&second))).await,
        StatusCode::OK
    );
    assert_eq!(
        org_billing_state(&pool, "billing-org-equal").await.0,
        "free"
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
        "id": "evt_deleted_1",
        "created": 100,
        "api_version": STRIPE_API_VERSION,
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
async fn sponsored_subscription_deleted_cancels_named_seats_without_downgrading_org() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let org_id = "billing-org-sponsored-deleted";
    let owner_id = "billing-user-sponsored-owner";
    let beneficiary_id = "billing-user-sponsored-beneficiary";
    let operation_id = "sponsored:billing-deleted-operation";
    for table in [
        "billing_sponsored_seats",
        "billing_sponsored_operations",
        "billing_sponsored_subscription_items",
        "billing_sponsored_subscriptions",
    ] {
        sqlx::query(&format!("DELETE FROM {table} WHERE organization_id = $1"))
            .bind(org_id)
            .execute(&pool)
            .await
            .expect("clean sponsored lifecycle fixture");
    }
    seed_user(&pool, owner_id).await;
    seed_user(&pool, beneficiary_id).await;
    seed_org(&pool, org_id, "team", owner_id, "owner").await;
    sqlx::query(
        "INSERT INTO billing_sponsored_subscriptions \
         (organization_id, provider_customer_id, provider_subscription_id, status) \
         VALUES ($1, 'cus_sponsored_deleted', 'sub_sponsored_deleted', 'active')",
    )
    .bind(org_id)
    .execute(&pool)
    .await
    .expect("seed sponsored subscription");
    sqlx::query(
        "INSERT INTO billing_sponsored_operations \
         (operation_id, organization_id, actor_user_id, idempotency_key, request_hash, action, \
          beneficiary_id, offer, quote_version, quote_expires_at_epoch, effective_from, \
          provider_idempotency_key, state, result_code) \
         VALUES ($1, $2, $3, 'deleted-idempotency', 'deleted-hash', 'add', $4, \
                 'standard_monthly', 1, 2000000000, 1700000000, 'deleted-provider-key', 'active', 'ok')",
    )
    .bind(operation_id)
    .bind(org_id)
    .bind(owner_id)
    .bind(beneficiary_id)
    .execute(&pool)
    .await
    .expect("seed sponsored operation");
    sqlx::query(
        "INSERT INTO billing_sponsored_seats \
         (seat_id, organization_id, beneficiary_id, offer, effective_from, state, operation_id) \
         VALUES ('seat:billing-deleted', $1, $2, 'standard_monthly', 1700000000, 'active', $3)",
    )
    .bind(org_id)
    .bind(beneficiary_id)
    .bind(operation_id)
    .execute(&pool)
    .await
    .expect("seed sponsored seat");

    let app = app_with_provider(
        pool.clone(),
        Arc::new(TestProvider {
            observation: SubscriptionObservation::Missing,
            personal_period_end: None,
        }),
    );
    let payload = serde_json::json!({
        "id": "evt_sponsored_deleted",
        "created": 1_800_000_000,
        "api_version": STRIPE_API_VERSION,
        "type": "customer.subscription.deleted",
        "data": { "object": {
            "id": "sub_sponsored_deleted",
            "customer": "cus_sponsored_deleted",
            "metadata": {"organization_id": org_id}
        }}
    })
    .to_string();
    assert_eq!(
        post_webhook(&app, &payload, Some(&stripe_signature(&payload))).await,
        StatusCode::OK
    );
    let stored: (String, Option<String>) = sqlx::query_as(
        "SELECT status, provider_subscription_id FROM billing_sponsored_subscriptions \
         WHERE organization_id = $1",
    )
    .bind(org_id)
    .fetch_one(&pool)
    .await
    .expect("sponsored lifecycle state");
    assert_eq!(stored, ("canceled".into(), None));
    let seat_state: String =
        sqlx::query_scalar("SELECT state FROM billing_sponsored_seats WHERE organization_id = $1")
            .bind(org_id)
            .fetch_one(&pool)
            .await
            .expect("canceled sponsored seat");
    assert_eq!(seat_state, "canceled");
    assert_eq!(org_billing_state(&pool, org_id).await.0, "team");
}

#[tokio::test]
async fn unmatched_sponsored_lifecycle_events_never_change_legacy_billing() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let free_org = "billing-org-sponsored-early-free";
    let team_org = "billing-org-sponsored-early-team";
    let free_owner = "billing-user-sponsored-early-free";
    let team_owner = "billing-user-sponsored-early-team";
    seed_user(&pool, free_owner).await;
    seed_user(&pool, team_owner).await;
    seed_org(&pool, free_org, "free", free_owner, "owner").await;
    seed_org(&pool, team_org, "team", team_owner, "owner").await;
    sqlx::query(
        "UPDATE organizations SET stripe_subscription_id = 'sub_legacy_early' WHERE id = $1",
    )
    .bind(team_org)
    .execute(&pool)
    .await
    .expect("seed legacy subscription link");
    let app = app_with_provider(
        pool.clone(),
        Arc::new(TestProvider {
            observation: SubscriptionObservation::Missing,
            personal_period_end: None,
        }),
    );
    let updated = serde_json::json!({
        "id": "evt_sponsored_early_updated",
        "created": 1_800_000_000,
        "api_version": STRIPE_API_VERSION,
        "type": "customer.subscription.updated",
        "data": { "object": {
            "id": "sub_sponsored_early",
            "status": "active",
            "cancel_at_period_end": false,
            "metadata": {"organization_id": free_org}
        }}
    })
    .to_string();
    assert_eq!(
        post_webhook(&app, &updated, Some(&stripe_signature(&updated))).await,
        StatusCode::OK
    );
    assert_eq!(org_billing_state(&pool, free_org).await.0, "free");

    let deleted = serde_json::json!({
        "id": "evt_sponsored_early_deleted",
        "created": 1_800_000_001,
        "api_version": STRIPE_API_VERSION,
        "type": "customer.subscription.deleted",
        "data": { "object": {
            "id": "sub_sponsored_early_deleted",
            "metadata": {"organization_id": team_org}
        }}
    })
    .to_string();
    assert_eq!(
        post_webhook(&app, &deleted, Some(&stripe_signature(&deleted))).await,
        StatusCode::OK
    );
    let team_state = org_billing_state(&pool, team_org).await;
    assert_eq!(team_state.0, "team");
    assert_eq!(team_state.2.as_deref(), Some("sub_legacy_early"));
}

#[tokio::test]
async fn webhooks_cannot_change_deleting_or_deleted_organisations() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    // These lifecycle receipts and watermarks must be cleared before the test so a rerun cannot be
    // acknowledged by deduplication without executing the lifecycle guards again.
    sqlx::query(
        "DELETE FROM stripe_subscription_watermarks \
         WHERE subscription_id IN ('sub_lifecycle_new', 'sub_test_lifecycle', \
                                   'sub_late', 'sub_late_updated', 'sub_late_deleted')",
    )
    .execute(&pool)
    .await
    .expect("clean lifecycle webhook watermarks");
    sqlx::query(
        "DELETE FROM stripe_webhook_events \
         WHERE event_id LIKE 'evt\\_lifecycle\\_%' ESCAPE '\\'",
    )
    .execute(&pool)
    .await
    .expect("clean lifecycle webhook receipts");
    seed_user(&pool, "billing-user-lifecycle").await;
    seed_org(
        &pool,
        "billing-org-lifecycle",
        "free",
        "billing-user-lifecycle",
        "owner",
    )
    .await;
    sqlx::query(
        "UPDATE organizations SET lifecycle_state = 'deleting', \
         stripe_subscription_id = 'sub_test_lifecycle' WHERE id = $1",
    )
    .bind("billing-org-lifecycle")
    .execute(&pool)
    .await
    .expect("mark org deleting");
    let app = app(pool.clone(), true);

    let checkout = serde_json::json!({
        "id": "evt_lifecycle_checkout",
        "created": 100,
        "api_version": STRIPE_API_VERSION,
        "type": "checkout.session.completed",
        "data": { "object": {
            "client_reference_id": "billing-org-lifecycle",
            "customer": "cus_lifecycle_new",
            "subscription": "sub_lifecycle_new"
        }}
    })
    .to_string();
    let updated = serde_json::json!({
        "id": "evt_lifecycle_updated",
        "created": 101,
        "api_version": STRIPE_API_VERSION,
        "type": "customer.subscription.updated",
        "data": { "object": {
            "id": "sub_test_lifecycle",
            "status": "active",
            "metadata": { "org_id": "billing-org-lifecycle" }
        }}
    })
    .to_string();
    let deleted = serde_json::json!({
        "id": "evt_lifecycle_deleted",
        "created": 102,
        "api_version": STRIPE_API_VERSION,
        "type": "customer.subscription.deleted",
        "data": { "object": { "id": "sub_test_lifecycle" } }
    })
    .to_string();
    for payload in [&checkout, &updated, &deleted] {
        assert_eq!(
            post_webhook(&app, payload, Some(&stripe_signature(payload))).await,
            StatusCode::OK
        );
        assert_eq!(
            org_billing_state(&pool, "billing-org-lifecycle").await,
            ("free".into(), None, Some("sub_test_lifecycle".into()))
        );
    }

    sqlx::query(
        "UPDATE organizations SET lifecycle_state = 'deleted', deleted_at = now(), \
         enc_name = NULL, tier = 'free', stripe_customer_id = NULL, \
         stripe_subscription_id = NULL WHERE id = $1",
    )
    .bind("billing-org-lifecycle")
    .execute(&pool)
    .await
    .expect("mark org deleted");
    let late = serde_json::json!({
        "id": "evt_lifecycle_late",
        "created": 103,
        "api_version": STRIPE_API_VERSION,
        "type": "checkout.session.completed",
        "data": { "object": {
            "client_reference_id": "billing-org-lifecycle",
            "customer": "cus_late",
            "subscription": "sub_late"
        }}
    })
    .to_string();
    let late_updated = serde_json::json!({
        "id": "evt_lifecycle_late_updated",
        "created": 104,
        "api_version": STRIPE_API_VERSION,
        "type": "customer.subscription.updated",
        "data": { "object": {
            "id": "sub_late_updated",
            "status": "active",
            "metadata": { "org_id": "billing-org-lifecycle" }
        }}
    })
    .to_string();
    let late_deleted = serde_json::json!({
        "id": "evt_lifecycle_late_deleted",
        "created": 105,
        "api_version": STRIPE_API_VERSION,
        "type": "customer.subscription.deleted",
        "data": { "object": {
            "id": "sub_late_deleted",
            "metadata": { "org_id": "billing-org-lifecycle" }
        }}
    })
    .to_string();
    for payload in [&late, &late_updated, &late_deleted] {
        assert_eq!(
            post_webhook(&app, payload, Some(&stripe_signature(payload))).await,
            StatusCode::OK
        );
        assert_eq!(
            org_billing_state(&pool, "billing-org-lifecycle").await,
            ("free".into(), None, None)
        );
    }
}

#[tokio::test]
async fn webhook_api_version_mismatch_is_recorded_and_refused() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    seed_user(&pool, "billing-user-version").await;
    seed_org(
        &pool,
        "billing-org-version",
        "free",
        "billing-user-version",
        "owner",
    )
    .await;
    let app = app(pool.clone(), true);
    let payload = serde_json::json!({
        "id": "evt_version_mismatch",
        "created": 100,
        "api_version": "2025-01-01",
        "type": "checkout.session.completed",
        "data": { "object": {
            "client_reference_id": "billing-org-version",
            "customer": "cus_version",
            "subscription": "sub_version"
        }}
    })
    .to_string();
    // Refused rather than accepted. This assertion was the opposite until a version mismatch
    // dropped twelve days of live webhooks in silence: reporting success to Stripe means the
    // event is never retried, so a mismatch that could have been fixed in an hour was instead
    // unrecoverable the moment it arrived.
    assert_eq!(
        post_webhook(&app, &payload, Some(&stripe_signature(&payload))).await,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(
        org_billing_state(&pool, "billing-org-version").await,
        ("free".into(), None, None)
    );
    // Recorded, so the receipt shows what arrived, but deliberately not marked processed: the
    // redelivery has to be allowed to do the work once the version is understood.
    let processed: bool = sqlx::query_scalar(
        "SELECT processed_at IS NOT NULL FROM stripe_webhook_events \
         WHERE event_id = 'evt_version_mismatch'",
    )
    .fetch_one(&pool)
    .await
    .expect("read mismatched webhook receipt");
    assert!(
        !processed,
        "a refused event must stay eligible for redelivery"
    );
}

#[tokio::test]
async fn webhook_missing_api_version_is_recorded_and_refused() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    seed_user(&pool, "billing-user-missing-version").await;
    seed_org(
        &pool,
        "billing-org-missing-version",
        "free",
        "billing-user-missing-version",
        "owner",
    )
    .await;
    let app = app(pool.clone(), true);
    let payload = serde_json::json!({
        "id": "evt_missing_version",
        "created": 100,
        "type": "checkout.session.completed",
        "data": { "object": {
            "client_reference_id": "billing-org-missing-version",
            "customer": "cus_missing_version",
            "subscription": "sub_missing_version"
        }}
    })
    .to_string();
    assert_eq!(
        post_webhook(&app, &payload, Some(&stripe_signature(&payload))).await,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(
        org_billing_state(&pool, "billing-org-missing-version").await,
        ("free".into(), None, None)
    );
    let receipt: (String, bool) = sqlx::query_as(
        "SELECT api_version, processed_at IS NOT NULL FROM stripe_webhook_events \
         WHERE event_id = 'evt_missing_version'",
    )
    .fetch_one(&pool)
    .await
    .expect("read missing-version receipt");
    assert_eq!(
        receipt,
        ("missing".into(), false),
        "recorded as missing, and left unprocessed so a redelivery can still be acted on"
    );
}

#[tokio::test]
async fn unknown_subscription_status_is_acknowledged_without_downgrade() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    seed_user(&pool, "billing-user-unknown-status").await;
    seed_org(
        &pool,
        "billing-org-unknown-status",
        "team",
        "billing-user-unknown-status",
        "owner",
    )
    .await;
    let app = app(pool.clone(), true);
    let payload = serde_json::json!({
        "id": "evt_unknown_status",
        "created": 100,
        "api_version": STRIPE_API_VERSION,
        "type": "customer.subscription.updated",
        "data": { "object": {
            "id": "sub_unknown_status",
            "status": "future_status",
            "metadata": { "org_id": "billing-org-unknown-status" }
        }}
    })
    .to_string();
    assert_eq!(
        post_webhook(&app, &payload, Some(&stripe_signature(&payload))).await,
        StatusCode::OK
    );
    assert_eq!(
        org_billing_state(&pool, "billing-org-unknown-status")
            .await
            .0,
        "team"
    );
}

#[tokio::test]
async fn webhook_prunes_expired_receipts() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    sqlx::query("DELETE FROM stripe_webhook_events WHERE event_id IN ('evt_old_receipt', 'evt_prune_trigger')")
        .execute(&pool)
        .await
        .expect("clean old receipt fixtures");
    sqlx::query(
        "INSERT INTO stripe_webhook_events \
         (event_id, event_type, api_version, stripe_created, received_at, processed_at) \
         VALUES ('evt_old_receipt', 'invoice.created', $1, 1, now() - interval '31 days', \
                 now() - interval '31 days')",
    )
    .bind(STRIPE_API_VERSION)
    .execute(&pool)
    .await
    .expect("insert expired receipt");
    let app = app(pool.clone(), true);
    let payload = serde_json::json!({
        "id": "evt_prune_trigger",
        "created": 100,
        "api_version": STRIPE_API_VERSION,
        "type": "invoice.created",
        "data": { "object": {} }
    })
    .to_string();
    assert_eq!(
        post_webhook(&app, &payload, Some(&stripe_signature(&payload))).await,
        StatusCode::OK
    );
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM stripe_webhook_events WHERE event_id = 'evt_old_receipt')",
    )
    .fetch_one(&pool)
    .await
    .expect("check expired receipt");
    assert!(!exists);
}

#[tokio::test]
async fn unhandled_events_are_acknowledged() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let app = app(pool, true);
    let payload = serde_json::json!({
        "id": "evt_invoice_1",
        "created": 100,
        "api_version": STRIPE_API_VERSION,
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
