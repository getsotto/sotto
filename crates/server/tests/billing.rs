//! Billing integration tests: webhook signature enforcement, tier assignment, idempotency, and
//! the configuration/role gates. DB-gated like the other server tests; provider calls use the
//! in-memory adapter and no test calls the real Stripe API.

use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgPool, Row};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use tower::ServiceExt;
use uuid::Uuid;

use sotto_server::auth::session;
use sotto_server::billing::{
    BillingState, ProviderError, ProviderResult, SubscriptionObservation, SubscriptionProvider,
    SubscriptionSnapshot, SubscriptionStatus, STRIPE_API_VERSION,
};
use sotto_server::billing_catalogue::{BillingOffer, BillingPriceIds};
use sotto_server::billing_operations::{self, BeginOperation};
use sotto_server::cloud_provider::{
    register_verified_allocation, AllocationState, PayerKind as ProviderPayerKind, ProviderContext,
    ProviderEnvironment, VerifiedAllocation,
};
use sotto_server::cloud_provider_stripe::StripeCoverageConfig;
use sotto_server::cloud_provider_stripe_http::{StripeReadClient, StripeReadLimits};
use sotto_server::config::{BillingConfig, DEFAULT_ORGANISATION_DELETION_RETENTION_DAYS};
use sotto_server::db;
use sotto_server::founding_allocator::{
    reserve_with_payer_kind_until, FoundingDate, FoundingOffer, FoundingPayerKind,
    ReservationOutcome,
};
use sotto_server::personal_billing;
use sotto_server::state::AppState;
use tokio::net::TcpListener;
use url::Url;

const WEBHOOK_SECRET: &str = "whsec_test_secret";

async fn pool_or_skip() -> Option<PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = db::connect(&url).await.expect("connect");
    db::migrate(&pool).await.expect("migrate");
    Some(pool)
}

async fn one_connection_pool_or_skip() -> Option<PgPool> {
    if std::env::var("SOTTO_RUN_DB_TESTS").as_deref() != Ok("1") {
        return None;
    }
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL is required");
    let options = PgConnectOptions::from_str(&url).expect("parse DATABASE_URL");
    assert!(
        matches!(options.get_host(), "localhost" | "127.0.0.1" | "::1"),
        "refusing one-connection coverage test against non-local host: {}",
        options.get_host()
    );
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .expect("connect one-connection test pool");
    db::migrate(&pool)
        .await
        .expect("migrate one-connection pool");
    Some(pool)
}

fn app(pool: PgPool, configured: bool) -> Router {
    app_with_billing(
        pool,
        configured.then(|| {
            BillingState::from_config(BillingConfig {
                api_key: "rk_test_never_called".into(),
                webhook_secret: WEBHOOK_SECRET.into(),
                price_id: "price_test".into(),
                price_catalogue: None,
                cloud_portal_configuration_id: Some("bpc_test".into()),
                cloud_sales_enabled: true,
                return_url: "https://app.sotto.test".into(),
            })
        }),
    )
}

fn app_with_billing(pool: PgPool, billing: Option<BillingState>) -> Router {
    app_with_billing_mode(
        pool,
        billing,
        sotto_server::config::DeploymentMode::SelfHosted,
    )
}

fn app_with_billing_mode(
    pool: PgPool,
    billing: Option<BillingState>,
    deployment_mode: sotto_server::config::DeploymentMode,
) -> Router {
    let state = AppState {
        deployment_mode,
        telemetry_ingest: false,
        contact_verification: None,
        cloud_notice_dispatch_enabled: false,
        resend_webhook_secret: None,
        cloud_action_enforcement_enabled: false,
        cloud_coverage_authority_enforcement_enabled: false,
        cloud_coverage_authority_shadow_enabled: false,
        machine_eligibility_enforcement_enabled: false,
        billing_transfers_enabled: false,
        billing_transfer_execution_enabled: false,
        billing_transfer_setup_enabled: false,
        cloud_retention_dry_run_enabled: false,
        cloud_shared_retention_dry_run_enabled: false,
        cloud_retention_operator_token: None,
        cloud_retention_purge_enabled: false,
        cloud_retention_destructive_token: None,
        cloud_retention_evidence_hmac_key: None,
        cloud_retention_build_sha: None,
        pool,
        oauth: None,
        oauth_config: None,
        billing,
        organisation_deletion_enabled: false,
        organisation_deletion_retention_days: DEFAULT_ORGANISATION_DELETION_RETENTION_DAYS,
        organisation_deletion_metrics_token: None,
        cloud_operations_metrics_token: None,
        cloud_coverage_repair_token: None,
        provider_refresh_ingest_enabled: false,
        provider_refresh_worker_enabled: false,
        provider_refresh_reconciliation_enabled: false,
        provider_authority_revalidation_enabled: false,
        cloud_sales_enabled: false,
        founding_schedule_worker_enabled: false,
        founding_price_notices_enabled: false,
        billing_recovery_worker_enabled: false,
        sponsored_billing_recovery_worker_enabled: false,
        organisation_deletion_operator_token: None,
        billing_transfer_operator_token: None,
    };
    Router::new()
        .merge(sotto_server::billing::router())
        .merge(sotto_server::person_eligibility::router())
        .with_state(state)
}

async fn get_eligibility(app: &Router, token: &str) -> (StatusCode, serde_json::Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/account/eligibility")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("eligibility request");
    let status = response.status();
    let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

#[derive(Clone)]
struct StripeMockResponses(Arc<Mutex<HashMap<String, serde_json::Value>>>);

async fn stripe_mock_handler(
    State(responses): State<StripeMockResponses>,
    request: Request<Body>,
) -> Response<Body> {
    let request_uri = request.uri();
    let exact_route = request_uri
        .query()
        .map(|query| format!("{}?{query}", request_uri.path()));
    let routes = responses.0.lock().unwrap();
    let mut value = exact_route
        .as_deref()
        .and_then(|route| routes.get(route))
        .or_else(|| routes.get(request_uri.path()))
        .cloned()
        .unwrap_or_else(|| serde_json::json!({"error":"not found"}));
    let status = value
        .get("_http_status")
        .and_then(serde_json::Value::as_u64)
        .and_then(|status| u16::try_from(status).ok())
        .and_then(|status| StatusCode::from_u16(status).ok())
        .unwrap_or(if value.get("error").is_some() {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::OK
        });
    if let Some(object) = value.as_object_mut() {
        object.remove("_http_status");
    }
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&value).unwrap()))
        .unwrap()
}

async fn stripe_mock_server(
    responses: HashMap<String, serde_json::Value>,
) -> (Url, tokio::task::JoinHandle<()>) {
    let (origin, task, _) = stripe_mock_server_with_state(responses).await;
    (origin, task)
}

async fn stripe_mock_server_with_state(
    responses: HashMap<String, serde_json::Value>,
) -> (Url, tokio::task::JoinHandle<()>, StripeMockResponses) {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let state = StripeMockResponses(Arc::new(Mutex::new(responses)));
    let router = Router::new()
        .fallback(any(stripe_mock_handler))
        .with_state(state.clone());
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (
        Url::parse(&format!("http://{address}/")).unwrap(),
        task,
        state,
    )
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

    async fn create_portal(
        &self,
        _customer: &str,
        _return_url: &str,
        _configuration_id: Option<&str>,
    ) -> ProviderResult<String> {
        Ok("https://stripe.test/portal".into())
    }

    async fn get_subscription(
        &self,
        _subscription_id: &str,
    ) -> ProviderResult<SubscriptionObservation> {
        Ok(self.observation.clone())
    }

    async fn personal_subscription_period(
        &self,
        _subscription_id: &str,
    ) -> ProviderResult<Option<sotto_server::billing::PersonalSubscriptionPeriod>> {
        Ok(self.personal_period_end.map(|end_epoch| {
            sotto_server::billing::PersonalSubscriptionPeriod {
                start_epoch: end_epoch - 30 * 24 * 60 * 60,
                end_epoch,
            }
        }))
    }

    async fn validate_personal_price(
        &self,
        _price_id: &str,
        _offer: BillingOffer,
    ) -> ProviderResult<()> {
        Ok(())
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

struct CancellationCountingProvider {
    calls: Arc<AtomicUsize>,
    pool: PgPool,
    user_id: String,
    intent_visible: Arc<AtomicBool>,
}

#[async_trait]
impl SubscriptionProvider for CancellationCountingProvider {
    async fn create_checkout(
        &self,
        _org_id: &str,
        _customer: Option<&str>,
        _success_url: &str,
        _cancel_url: &str,
    ) -> ProviderResult<String> {
        Ok("https://stripe.test/checkout".into())
    }

    async fn create_portal(
        &self,
        _customer: &str,
        _return_url: &str,
        _configuration_id: Option<&str>,
    ) -> ProviderResult<String> {
        Ok("https://stripe.test/portal".into())
    }

    async fn get_subscription(
        &self,
        _subscription_id: &str,
    ) -> ProviderResult<SubscriptionObservation> {
        Ok(SubscriptionObservation::Missing)
    }

    async fn cancel_subscription(
        &self,
        _subscription_id: &str,
        _idempotency_key: &str,
        _org_id: &str,
    ) -> ProviderResult<SubscriptionObservation> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let intent: bool = sqlx::query_scalar(
            "SELECT cancellation_requested_at IS NOT NULL FROM billing_personal_accounts \
             WHERE user_id = $1",
        )
        .bind(&self.user_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|_| ProviderError {
            status: None,
            code: Some("test_database_error".into()),
            kind: sotto_server::billing::ProviderErrorKind::Unknown,
        })?;
        self.intent_visible.store(intent, Ordering::Relaxed);
        Ok(SubscriptionObservation::Missing)
    }
}

fn app_with_provider(pool: PgPool, provider: Arc<dyn SubscriptionProvider>) -> Router {
    let state = AppState {
        deployment_mode: sotto_server::config::DeploymentMode::SelfHosted,
        telemetry_ingest: false,
        contact_verification: None,
        cloud_notice_dispatch_enabled: false,
        resend_webhook_secret: None,
        cloud_action_enforcement_enabled: false,
        cloud_coverage_authority_enforcement_enabled: false,
        cloud_coverage_authority_shadow_enabled: false,
        machine_eligibility_enforcement_enabled: false,
        billing_transfers_enabled: false,
        billing_transfer_execution_enabled: false,
        billing_transfer_setup_enabled: false,
        cloud_retention_dry_run_enabled: false,
        cloud_shared_retention_dry_run_enabled: false,
        cloud_retention_operator_token: None,
        cloud_retention_purge_enabled: false,
        cloud_retention_destructive_token: None,
        cloud_retention_evidence_hmac_key: None,
        cloud_retention_build_sha: None,
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
        cloud_operations_metrics_token: None,
        cloud_coverage_repair_token: None,
        provider_refresh_ingest_enabled: false,
        provider_refresh_worker_enabled: false,
        provider_refresh_reconciliation_enabled: false,
        provider_authority_revalidation_enabled: false,
        cloud_sales_enabled: false,
        founding_schedule_worker_enabled: false,
        founding_price_notices_enabled: false,
        billing_recovery_worker_enabled: false,
        sponsored_billing_recovery_worker_enabled: false,
        organisation_deletion_operator_token: None,
        billing_transfer_operator_token: None,
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

async fn post_authed_json(
    app: &Router,
    uri: &str,
    token: &str,
    body: serde_json::Value,
) -> StatusCode {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
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
async fn personal_cancellation_persists_intent_before_acknowledging_provider_acceptance() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let fixture = Uuid::new_v4().simple().to_string();
    let user_id = format!("billing-user-cancel-{fixture}");
    let token = seed_user(&pool, &user_id).await;
    let operation_id =
        seed_personal_account(&pool, &user_id, &format!("personal-cancel-{fixture}")).await;
    sqlx::query(
        "UPDATE billing_personal_accounts SET state = 'active', stripe_customer_id = $2, \
           stripe_subscription_id = $3 WHERE user_id = $1",
    )
    .bind(&user_id)
    .bind(format!("cus_cancel_{fixture}"))
    .bind(format!("sub_cancel_{fixture}"))
    .execute(&pool)
    .await
    .expect("activate personal subscription fixture");
    let calls = Arc::new(AtomicUsize::new(0));
    let intent_visible = Arc::new(AtomicBool::new(false));
    let app = app_with_provider(
        pool.clone(),
        Arc::new(CancellationCountingProvider {
            calls: calls.clone(),
            pool: pool.clone(),
            user_id: user_id.clone(),
            intent_visible: intent_visible.clone(),
        }),
    );

    assert_eq!(
        post_authed_json(
            &app,
            "/billing/personal/cancel",
            &token,
            serde_json::json!({"idempotency_key": format!("cancel-{fixture}")}),
        )
        .await,
        StatusCode::OK
    );
    let cancellation: (bool, bool) = sqlx::query_as(
        "SELECT cancel_at_period_end, cancellation_requested_at IS NOT NULL \
         FROM billing_personal_accounts WHERE user_id = $1",
    )
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("read accepted cancellation");
    assert_eq!(cancellation, (true, true));
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert!(
        intent_visible.load(Ordering::Relaxed),
        "cancellation intent must be committed before the provider call"
    );

    sqlx::query("DELETE FROM billing_personal_accounts WHERE user_id = $1")
        .bind(&user_id)
        .execute(&pool)
        .await
        .expect("clean personal account");
    sqlx::query("DELETE FROM billing_operations WHERE operation_id = $1")
        .bind(operation_id)
        .execute(&pool)
        .await
        .expect("clean billing operation");
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("clean user");
}

#[tokio::test]
async fn already_flagged_personal_cancellation_still_reconciles_provider_state() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let fixture = Uuid::new_v4().simple().to_string();
    let user_id = format!("billing-user-cancel-retry-{fixture}");
    let token = seed_user(&pool, &user_id).await;
    let operation_id =
        seed_personal_account(&pool, &user_id, &format!("personal-cancel-retry-{fixture}")).await;
    sqlx::query(
        "UPDATE billing_personal_accounts SET state = 'active', cancel_at_period_end = TRUE, \
           stripe_customer_id = $2, stripe_subscription_id = $3 WHERE user_id = $1",
    )
    .bind(&user_id)
    .bind(format!("cus_cancel_retry_{fixture}"))
    .bind(format!("sub_cancel_retry_{fixture}"))
    .execute(&pool)
    .await
    .expect("seed cancellation already accepted by webhook");
    let calls = Arc::new(AtomicUsize::new(0));
    let intent_visible = Arc::new(AtomicBool::new(false));
    let app = app_with_provider(
        pool.clone(),
        Arc::new(CancellationCountingProvider {
            calls: calls.clone(),
            pool: pool.clone(),
            user_id: user_id.clone(),
            intent_visible: intent_visible.clone(),
        }),
    );

    assert_eq!(
        post_authed_json(
            &app,
            "/billing/personal/cancel",
            &token,
            serde_json::json!({"idempotency_key": format!("cancel-retry-{fixture}")}),
        )
        .await,
        StatusCode::OK
    );
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert!(intent_visible.load(Ordering::Relaxed));
    let intent_recorded: bool = sqlx::query_scalar(
        "SELECT cancellation_requested_at IS NOT NULL FROM billing_personal_accounts WHERE user_id = $1",
    )
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("read cancellation intent");
    assert!(intent_recorded);

    sqlx::query("DELETE FROM billing_personal_accounts WHERE user_id = $1")
        .bind(&user_id)
        .execute(&pool)
        .await
        .expect("clean personal account");
    sqlx::query("DELETE FROM billing_operations WHERE operation_id = $1")
        .bind(operation_id)
        .execute(&pool)
        .await
        .expect("clean billing operation");
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("clean user");
}

#[tokio::test]
async fn personal_subscription_events_track_cancel_at_and_reactivation() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let fixture = Uuid::new_v4().simple().to_string();
    let user_id = format!("billing-user-cancel-at-{fixture}");
    seed_user(&pool, &user_id).await;
    let operation_id =
        seed_personal_account(&pool, &user_id, &format!("personal-cancel-at-{fixture}")).await;
    let subscription_id = format!("sub_cancel_at_{fixture}");
    sqlx::query(
        "UPDATE billing_personal_accounts SET state = 'active', stripe_subscription_id = $2, \
           cancellation_requested_at = now() WHERE user_id = $1",
    )
    .bind(&user_id)
    .bind(&subscription_id)
    .execute(&pool)
    .await
    .expect("seed schedule-managed cancellation intent");
    let award_id = format!("founding-cancel-at-{fixture}");
    let cohort_ordinal = i64::from_str_radix(&fixture[..2], 16).unwrap() % 100 + 1;
    sqlx::query(
        "INSERT INTO billing_founding_awards \
         (award_id, beneficiary_id, payer_kind, payer_id, offer, cohort_ordinal, \
          original_start_date, original_end_date, original_start_epoch, original_end_epoch) \
         VALUES ($1, $2, 'personal', $2, 'founding_monthly', $3, \
                 '2026-01-01', '2027-01-01', 1767225600, 1798761600)",
    )
    .bind(&award_id)
    .bind(&user_id)
    .bind(cohort_ordinal)
    .execute(&pool)
    .await
    .expect("seed founding award");
    sqlx::query(
        "INSERT INTO billing_founding_schedule_jobs \
         (job_id, award_id, beneficiary_id, payer_kind, payer_id, provider_subscription_id, \
          provider_idempotency_key, state) \
         VALUES ($1, $2, $3, 'personal', $3, $4, $5, 'not_required')",
    )
    .bind(format!("founding-schedule:{award_id}"))
    .bind(&award_id)
    .bind(&user_id)
    .bind(&subscription_id)
    .bind(format!("founding-transition:{award_id}:1"))
    .execute(&pool)
    .await
    .expect("seed ended founding schedule job");
    let app = app(pool.clone(), true);
    let event = |event_id: &str, created: i64, cancel_at: Option<i64>, schedule: Option<&str>| {
        serde_json::json!({
            "id": event_id,
            "created": created,
            "api_version": STRIPE_API_VERSION,
            "type": "customer.subscription.updated",
            "data": { "object": {
                "id": subscription_id,
                "status": "active",
                "cancel_at_period_end": false,
                "cancel_at": cancel_at,
                "schedule": schedule,
                "metadata": {"personal_user_id": user_id}
            }}
        })
        .to_string()
    };

    let cancelling = event(
        &format!("evt_personal_cancel_at_{fixture}"),
        1_900_000_000,
        Some(1_902_592_000),
        Some("sched_personal"),
    );
    assert_eq!(
        post_webhook(&app, &cancelling, Some(&stripe_signature(&cancelling))).await,
        StatusCode::OK
    );
    let cancellation: (bool, bool) = sqlx::query_as(
        "SELECT cancel_at_period_end, cancellation_requested_at IS NOT NULL \
         FROM billing_personal_accounts WHERE user_id = $1",
    )
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("read schedule cancellation state");
    assert_eq!(cancellation, (true, true));

    let reactivated = event(
        &format!("evt_personal_reactivated_{fixture}"),
        1_900_000_001,
        None,
        None,
    );
    assert_eq!(
        post_webhook(&app, &reactivated, Some(&stripe_signature(&reactivated))).await,
        StatusCode::OK
    );
    let cancellation: (bool, bool) = sqlx::query_as(
        "SELECT cancel_at_period_end, cancellation_requested_at IS NOT NULL \
         FROM billing_personal_accounts WHERE user_id = $1",
    )
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("read reactivated subscription state");
    assert_eq!(cancellation, (false, false));
    let resumed: (String, i64, String) = sqlx::query_as(
        "SELECT state, version, provider_idempotency_key \
         FROM billing_founding_schedule_jobs WHERE award_id = $1",
    )
    .bind(&award_id)
    .fetch_one(&pool)
    .await
    .expect("read founding transition after reactivation");
    assert_eq!(
        resumed,
        (
            "pending".into(),
            2,
            format!("founding-transition:{award_id}:2"),
        )
    );

    sqlx::query("DELETE FROM billing_founding_schedule_jobs WHERE award_id = $1")
        .bind(&award_id)
        .execute(&pool)
        .await
        .expect("clean founding schedule job");
    sqlx::query("DELETE FROM billing_founding_awards WHERE award_id = $1")
        .bind(&award_id)
        .execute(&pool)
        .await
        .expect("clean founding award");
    sqlx::query("DELETE FROM billing_personal_accounts WHERE user_id = $1")
        .bind(&user_id)
        .execute(&pool)
        .await
        .expect("clean personal account");
    sqlx::query("DELETE FROM billing_operations WHERE operation_id = $1")
        .bind(operation_id)
        .execute(&pool)
        .await
        .expect("clean billing operation");
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("clean user");
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
async fn personal_renewal_notices_follow_signed_failure_and_matching_paid_invoice() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let fixture = Uuid::new_v4().simple().to_string();
    let user_id = format!("billing-renewal-notice-user-{fixture}");
    let operation_key = format!("renewal-notice-{fixture}");
    let subscription_id = format!("sub_notice_{fixture}");
    let customer_id = format!("cus_notice_{fixture}");
    let invoice_id = format!("in_notice_{fixture}");
    let first_failure_event = format!("evt_notice_failure_{fixture}");
    let retry_failure_event = format!("evt_notice_retry_{fixture}");
    let mismatch_failure_event = format!("evt_notice_mismatch_{fixture}");
    let unbound_failure_event = format!("evt_notice_unbound_{fixture}");
    let unrelated_paid_event = format!("evt_notice_unrelated_paid_{fixture}");
    let recovered_paid_event = format!("evt_notice_recovered_paid_{fixture}");
    let paid_before_failure_event = format!("evt_notice_paid_first_{fixture}");
    let late_failure_event = format!("evt_notice_late_failure_{fixture}");
    seed_user(&pool, &user_id).await;
    let operation_id = seed_personal_account(&pool, &user_id, &operation_key).await;
    // Stripe period boundaries preserve their exact time of day. Keep this boundary off
    // midnight so the notice path cannot accidentally round it back to a calendar date.
    let paid_through = 1_900_000_000;
    sqlx::query(
        "UPDATE billing_personal_accounts SET state='active', stripe_customer_id=$2, \
           stripe_subscription_id=$3, paid_through_epoch=$4, paid_through_date=$5 \
         WHERE user_id=$1",
    )
    .bind(&user_id)
    .bind(&customer_id)
    .bind(&subscription_id)
    .bind(paid_through)
    .bind(
        FoundingDate::from_unix_seconds(paid_through)
            .unwrap()
            .to_string(),
    )
    .execute(&pool)
    .await
    .expect("activate personal billing fixture");
    let app = app_with_provider(
        pool.clone(),
        Arc::new(TestProvider {
            observation: SubscriptionObservation::Missing,
            personal_period_end: None,
        }),
    );

    let make_failure = |event_id: &str,
                        created: i64,
                        failed_invoice_id: &str,
                        period_start: i64| {
        serde_json::json!({
            "id": event_id,
            "created": created,
            "api_version": STRIPE_API_VERSION,
            "type": "invoice.payment_failed",
            "livemode": false,
            "data": { "object": {
                "object": "invoice", "id": failed_invoice_id, "customer": customer_id,
                "parent": {"type":"subscription_details", "subscription_details":{"subscription":subscription_id}},
                "status":"open", "billing_reason":"subscription_cycle", "collection_method":"charge_automatically",
                "amount_due":299, "amount_remaining":299, "amount_paid":0,
                "lines":{"data":[{"parent":{"type":"subscription_item_details", "subscription_item_details":{"subscription":subscription_id}},
                    "period":{"start":period_start, "end":period_start + 2_592_000}}]}
            }}
        }).to_string()
    };
    let make_paid = |event_id: &str, created: i64, paid_invoice_id: &str, period_start: i64| {
        serde_json::json!({
            "id": event_id,
            "created": created,
            "api_version": STRIPE_API_VERSION,
            "type": "invoice.paid",
            "livemode": false,
            "data": { "object": {
                "object":"invoice", "id":paid_invoice_id, "customer":customer_id,
                "payment_intent":format!("pi_{fixture}_{event_id}"), "status":"paid",
                "billing_reason":"subscription_cycle", "period_end":period_start + 2_592_000,
                "lines":{"data":[{"parent":{"type":"subscription_item_details", "subscription_item_details":{"subscription":subscription_id}},
                    "period":{"start":period_start, "end":period_start + 2_592_000}}]},
                "parent":{"type":"subscription_details", "subscription_details":{"subscription":subscription_id}}
            }}
        }).to_string()
    };
    let send = |payload: String| {
        let app = app.clone();
        async move {
            let signature = stripe_signature(&payload);
            post_webhook(&app, &payload, Some(&signature)).await
        }
    };

    // Older Stripe invoice snapshots expose the subscription at the top level. Keep this
    // fallback covered through the signed webhook path, including the notice and its later
    // resolution by the matching paid invoice.
    let mut legacy_failure: serde_json::Value = serde_json::from_str(&make_failure(
        &first_failure_event,
        100,
        &invoice_id,
        paid_through,
    ))
    .expect("build the legacy renewal failure event");
    let legacy_invoice = legacy_failure["data"]["object"]
        .as_object_mut()
        .expect("invoice object");
    legacy_invoice.remove("parent");
    legacy_invoice.insert("subscription".into(), serde_json::json!(subscription_id));
    let failed_payload = legacy_failure.to_string();
    let invalid_signature = stripe_signature("different body");
    assert_eq!(
        post_webhook(&app, &failed_payload, Some(&invalid_signature)).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(send(failed_payload).await, StatusCode::OK);
    let notice_after_legacy_failure: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM cloud_notice_outbox \
         WHERE recipient_user_id=$1 AND kind='failed_renewal'",
    )
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("count notices after the legacy invoice shape");
    assert_eq!(
        notice_after_legacy_failure, 1,
        "the legacy top-level subscription must bind the failed invoice to its account"
    );
    // Stripe can emit more than one failure event for the same invoice. The customer sees one
    // notice with the original deadline, even though the delivery ids differ.
    let retry_payload = make_failure(&retry_failure_event, 101, &invoice_id, paid_through);
    assert_eq!(send(retry_payload).await, StatusCode::OK);
    let mut mismatched_customer: serde_json::Value = serde_json::from_str(&make_failure(
        &mismatch_failure_event,
        102,
        &format!("in_mismatch_{fixture}"),
        paid_through,
    ))
    .unwrap();
    mismatched_customer["data"]["object"]["customer"] =
        serde_json::Value::String(format!("cus_unbound_{fixture}"));
    assert_eq!(send(mismatched_customer.to_string()).await, StatusCode::OK);
    let unbound_payload = make_failure(
        &unbound_failure_event,
        103,
        &format!("in_unbound_{fixture}"),
        paid_through,
    )
    .replace(&subscription_id, &format!("sub_unbound_{fixture}"))
    .replace(&customer_id, &format!("cus_unbound_{fixture}"));
    assert_eq!(send(unbound_payload).await, StatusCode::OK);
    let failed_notice = sqlx::query(
        "SELECT event_key, payload, extract(epoch FROM due_at)::bigint AS due_at \
         FROM cloud_notice_outbox WHERE recipient_user_id=$1 AND kind='failed_renewal'",
    )
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("load failed renewal notice");
    let event_key: String = failed_notice.try_get("event_key").unwrap();
    let failure_content: serde_json::Value = failed_notice.try_get("payload").unwrap();
    assert!(event_key.starts_with("personal-renewal-v1:"));
    assert!(!event_key.contains(&subscription_id));
    assert!(!event_key.contains(&invoice_id));
    assert_eq!(
        failure_content["deadline_epoch"].as_i64(),
        Some(paid_through + sotto_server::cloud_coverage::RENEWAL_RECOVERY_SECONDS)
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM cloud_notice_outbox WHERE recipient_user_id=$1 AND kind='failed_renewal'",
        )
        .bind(&user_id)
        .fetch_one(&pool)
        .await
        .unwrap(),
        1
    );

    // A different paid invoice does not resolve this failed invoice.
    let next_period = paid_through + 2_592_000;
    let unrelated_payload = make_paid(
        &unrelated_paid_event,
        104,
        &format!("in_unrelated_{fixture}"),
        next_period,
    );
    assert_eq!(send(unrelated_payload).await, StatusCode::OK);
    let resolved_before_matching_payment: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM cloud_notice_outbox WHERE recipient_user_id=$1 AND kind='recovery_resolved'",
    )
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(resolved_before_matching_payment, 0);

    // The matching invoice can be paid later. Its resolution is committed with the billing
    // update, while the failed notice's key, deadline and content stay unchanged.
    let recovered_payload = make_paid(&recovered_paid_event, 105, &invoice_id, paid_through);
    assert_eq!(send(recovered_payload).await, StatusCode::OK);
    let resolved_notice: serde_json::Value = sqlx::query_scalar(
        "SELECT payload FROM cloud_notice_outbox WHERE recipient_user_id=$1 AND kind='recovery_resolved'",
    )
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("load recovery resolved notice");
    assert_eq!(resolved_notice["title"], "Your Cloud renewal is up to date");
    let visible_notices = sotto_server::notifications::list_for_user(&pool, &user_id)
        .await
        .expect("list renewal notices after payment");
    assert_eq!(visible_notices.len(), 1);
    assert_eq!(visible_notices[0].kind, "recovery_resolved");
    let failed_after_recovery: (String, serde_json::Value) = sqlx::query_as(
        "SELECT event_key, payload FROM cloud_notice_outbox \
         WHERE recipient_user_id=$1 AND kind='failed_renewal'",
    )
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(failed_after_recovery.0, event_key);
    assert_eq!(failed_after_recovery.1, failure_content);

    // If payment is processed before the failure event, the paid-through row makes the late
    // failure stale and prevents a notice that contradicts the final billing state.
    let current_paid_through = paid_through + 2_592_000 * 2;
    let paid_first_invoice = format!("in_paid_first_{fixture}");
    let paid_first = make_paid(
        &paid_before_failure_event,
        106,
        &paid_first_invoice,
        current_paid_through,
    );
    assert_eq!(send(paid_first).await, StatusCode::OK);
    let late_failure = make_failure(
        &late_failure_event,
        107,
        &paid_first_invoice,
        current_paid_through,
    );
    assert_eq!(send(late_failure).await, StatusCode::OK);
    let failure_notice_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM cloud_notice_outbox WHERE recipient_user_id=$1 AND kind='failed_renewal'",
    )
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(failure_notice_count, 1);

    // A late payment can arrive after the subscription itself was cancelled. It should retire
    // the stale failure warning without presenting the cancelled subscription as active again.
    sqlx::query("UPDATE billing_personal_accounts SET state='canceled' WHERE user_id=$1")
        .bind(&user_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE cloud_notice_outbox SET status='pending', cancelled_at=NULL \
         WHERE recipient_user_id=$1 AND kind='failed_renewal'",
    )
    .bind(&user_id)
    .execute(&pool)
    .await
    .unwrap();
    let late_paid_after_cancel = make_paid(
        &format!("evt_paid_after_cancel_{fixture}"),
        108,
        &invoice_id,
        paid_through,
    );
    assert_eq!(send(late_paid_after_cancel).await, StatusCode::OK);
    let (account_state, failed_notice_status): (String, String) = sqlx::query_as(
        "SELECT (SELECT state FROM billing_personal_accounts WHERE user_id=$1), \
                (SELECT status FROM cloud_notice_outbox WHERE recipient_user_id=$1 \
                   AND kind='failed_renewal')",
    )
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(account_state, "canceled");
    assert_eq!(failed_notice_status, "cancelled");
    let visible_after_cancelled_payment =
        sotto_server::notifications::list_for_user(&pool, &user_id)
            .await
            .expect("list notices after late cancelled payment");
    assert!(visible_after_cancelled_payment
        .iter()
        .all(|notice| notice.kind != "failed_renewal"));

    // Notices contain only fixed copy and dates, never the payment, invoice, customer or
    // subscription references used to derive their opaque identity.
    let payload_text: String = sqlx::query_scalar(
        "SELECT string_agg(payload::text, ' ') FROM cloud_notice_outbox WHERE recipient_user_id=$1",
    )
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    for provider_reference in [&subscription_id, &customer_id, &invoice_id] {
        assert!(!payload_text.contains(provider_reference));
    }
    assert!(failed_notice.try_get::<i64, _>("due_at").unwrap() > 0);

    sqlx::query("DELETE FROM cloud_notice_outbox WHERE recipient_user_id=$1")
        .bind(&user_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM billing_personal_accounts WHERE user_id=$1")
        .bind(&user_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM billing_operations WHERE operation_id=$1")
        .bind(&operation_id)
        .execute(&pool)
        .await
        .unwrap();
    let webhook_event_ids = vec![
        first_failure_event,
        retry_failure_event,
        mismatch_failure_event,
        unbound_failure_event,
        unrelated_paid_event,
        recovered_paid_event,
        paid_before_failure_event,
        late_failure_event,
        format!("evt_paid_after_cancel_{fixture}"),
    ];
    sqlx::query(
        "DELETE FROM stripe_subscription_watermarks \
         WHERE subscription_id=$1 OR event_id = ANY($2)",
    )
    .bind(&subscription_id)
    .bind(&webhook_event_ids)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("DELETE FROM stripe_webhook_events WHERE event_id = ANY($1)")
        .bind(&webhook_event_ids)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM users WHERE id=$1")
        .bind(&user_id)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn personal_renewal_notice_enqueue_failure_rolls_back_its_webhook_receipt() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let fixture = Uuid::new_v4().simple().to_string();
    let user_id = format!("billing-renewal-rollback-user-{fixture}");
    seed_user(&pool, &user_id).await;
    let operation_id =
        seed_personal_account(&pool, &user_id, &format!("renewal-rollback-{fixture}")).await;
    let subscription_id = format!("sub_notice_rollback_{fixture}");
    let customer_id = format!("cus_notice_rollback_{fixture}");
    let event_id = format!("evt_notice_rollback_{fixture}");
    let invoice_id = format!("in_notice_rollback_{fixture}");
    let paid_through = FoundingDate::from_unix_seconds(1_900_000_000)
        .expect("valid paid-through date")
        .to_unix_seconds();
    sqlx::query(
        "UPDATE billing_personal_accounts SET state='active', stripe_customer_id=$2, \
           stripe_subscription_id=$3, paid_through_epoch=$4, paid_through_date=$5 \
         WHERE user_id=$1",
    )
    .bind(&user_id)
    .bind(&customer_id)
    .bind(&subscription_id)
    .bind(paid_through)
    .bind(
        FoundingDate::from_unix_seconds(paid_through)
            .unwrap()
            .to_string(),
    )
    .execute(&pool)
    .await
    .expect("activate personal billing fixture");
    let function_name = format!("fail_notice_insert_{fixture}");
    let trigger_name = format!("fail_notice_insert_trigger_{fixture}");
    sqlx::query(&format!(
        "CREATE FUNCTION {function_name}() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN IF NEW.recipient_user_id = '{user_id}' THEN \
           RAISE EXCEPTION 'intentional notice enqueue failure'; \
         END IF; RETURN NEW; END $$"
    ))
    .execute(&pool)
    .await
    .expect("install recipient-scoped notice fault");
    sqlx::query(&format!(
        "CREATE TRIGGER {trigger_name} BEFORE INSERT ON cloud_notice_outbox \
         FOR EACH ROW EXECUTE FUNCTION {function_name}()"
    ))
    .execute(&pool)
    .await
    .expect("install notice fault trigger");

    let payload = serde_json::json!({
        "id": event_id,
        "created": 100,
        "api_version": STRIPE_API_VERSION,
        "type": "invoice.payment_failed",
        "livemode": false,
        "data": { "object": {
            "object":"invoice", "id":invoice_id, "customer":customer_id,
            "parent":{"type":"subscription_details", "subscription_details":{"subscription":subscription_id}},
            "status":"open", "billing_reason":"subscription_cycle", "collection_method":"charge_automatically",
            "amount_remaining":299,
            "lines":{"data":[{"parent":{"type":"subscription_item_details", "subscription_item_details":{"subscription":subscription_id}},
                "period":{"start":paid_through, "end":paid_through + 2_592_000}}]}
        }}
    })
    .to_string();
    let app = app_with_provider(
        pool.clone(),
        Arc::new(TestProvider {
            observation: SubscriptionObservation::Missing,
            personal_period_end: None,
        }),
    );
    assert_eq!(
        post_webhook(&app, &payload, Some(&stripe_signature(&payload))).await,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    sqlx::query(&format!(
        "DROP TRIGGER {trigger_name} ON cloud_notice_outbox"
    ))
    .execute(&pool)
    .await
    .expect("remove notice fault trigger");
    sqlx::query(&format!("DROP FUNCTION {function_name}()"))
        .execute(&pool)
        .await
        .expect("remove notice fault");

    let counts: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM stripe_webhook_events WHERE event_id=$1), \
                (SELECT count(*) FROM cloud_notice_outbox WHERE recipient_user_id=$2)",
    )
    .bind(&event_id)
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("query rolled-back rows");
    assert_eq!(counts, (0, 0));

    sqlx::query("DELETE FROM billing_personal_accounts WHERE user_id=$1")
        .bind(&user_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM billing_operations WHERE operation_id=$1")
        .bind(&operation_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM users WHERE id=$1")
        .bind(&user_id)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn personal_renewal_notice_is_committed_before_coverage_history_retry() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let fixture = Uuid::new_v4().simple().to_string();
    let user_id = format!("billing-renewal-read-failure-user-{fixture}");
    let operation_id = format!("personal-renewal-read-failure-{fixture}");
    let subscription_id = format!("sub_read_failure_{fixture}");
    let customer_id = format!("cus_read_failure_{fixture}");
    let account_id = format!("acct_read_failure_{fixture}");
    let allocation_reference = format!("alloc_read_failure_{fixture}");
    let allocation_id = format!("allocation_read_failure_{fixture}");
    let payer_id = format!("payer_read_failure_{fixture}");
    let source_id = format!("source_read_failure_{fixture}");
    let event_id = format!("evt_read_failure_{fixture}");
    let invoice_id = format!("in_read_failure_{fixture}");
    seed_user(&pool, &user_id).await;
    let seeded_operation = seed_personal_account(&pool, &user_id, &operation_id).await;
    let paid_through = FoundingDate::from_unix_seconds(1_900_000_000)
        .expect("valid paid-through date")
        .to_unix_seconds();
    sqlx::query(
        "UPDATE billing_personal_accounts SET state='active', stripe_customer_id=$2, \
           stripe_subscription_id=$3, allocation_reference=$4, paid_through_epoch=$5, \
           paid_through_date=$6 WHERE user_id=$1",
    )
    .bind(&user_id)
    .bind(&customer_id)
    .bind(&subscription_id)
    .bind(&allocation_reference)
    .bind(paid_through)
    .bind(
        FoundingDate::from_unix_seconds(paid_through)
            .unwrap()
            .to_string(),
    )
    .execute(&pool)
    .await
    .expect("bind active personal account");

    let coverage = StripeCoverageConfig::new(
        &account_id,
        ProviderEnvironment::Test,
        "price_read_failure_month",
        "price_read_failure_year",
    )
    .unwrap();
    let context = ProviderContext::new("stripe", &account_id, ProviderEnvironment::Test).unwrap();
    let allocation = VerifiedAllocation::new(
        &allocation_id,
        &payer_id,
        &customer_id,
        ProviderPayerKind::Personal,
        &user_id,
        &subscription_id,
        "si_read_failure",
        &allocation_reference,
        &source_id,
        0,
        None,
        AllocationState::Active,
        format!("stripe:renewal-notice-owner:{fixture}"),
    )
    .unwrap();
    let mut register = pool.begin().await.expect("begin allocation registration");
    register_verified_allocation(&mut register, &context, &allocation)
        .await
        .expect("register personal allocation");
    register
        .commit()
        .await
        .expect("commit allocation registration");

    // The feature's coverage read fails before it can link predecessor evidence. The signed
    // billing fact still has enough local binding to create its customer notice and pending
    // receipt, so the customer is not left uninformed while Stripe retries the coverage work.
    let (origin, stripe_task) = stripe_mock_server(HashMap::from([(
        "/v1/account".into(),
        serde_json::json!({"_http_status":500,"error":{"message":"temporary read failure"}}),
    )]))
    .await;
    let stripe = StripeReadClient::for_test(
        "sk_test_read_failure",
        &coverage,
        origin,
        StripeReadLimits::default(),
    )
    .unwrap();
    let billing = BillingState::with_provider_and_cloud_sales(
        Arc::new(TestProvider {
            observation: SubscriptionObservation::Missing,
            personal_period_end: None,
        }),
        WEBHOOK_SECRET.into(),
        "https://app.sotto.test".into(),
        false,
    )
    .with_personal_coverage(coverage, stripe, true);
    let app = app_with_billing_mode(
        pool.clone(),
        Some(billing),
        sotto_server::config::DeploymentMode::Cloud,
    );
    let payload = serde_json::json!({
        "id":event_id, "created":100, "api_version":STRIPE_API_VERSION,
        "type":"invoice.payment_failed", "livemode":false,
        "data":{"object":{
            "object":"invoice", "id":invoice_id, "customer":customer_id,
            "parent":{"type":"subscription_details", "subscription_details":{"subscription":subscription_id}},
            "status":"open", "billing_reason":"subscription_cycle", "collection_method":"charge_automatically",
            "amount_due":299, "amount_remaining":299, "amount_paid":0,
            "metadata":{"sotto_allocation_reference":allocation_reference},
            "lines":{"data":[{"id":"il_failed_read", "parent":{"type":"subscription_item_details", "subscription_item_details":{"subscription":subscription_id, "subscription_item":"si_read_failure"}},
                "period":{"start":paid_through, "end":paid_through + 2_592_000}}]}
        }}
    })
    .to_string();
    assert_eq!(
        post_webhook(&app, &payload, Some(&stripe_signature(&payload))).await,
        StatusCode::INTERNAL_SERVER_ERROR,
        "Stripe must retry the still-unresolved coverage evidence"
    );
    let (notice_count, processed): (i64, bool) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM cloud_notice_outbox WHERE recipient_user_id=$1 AND kind='failed_renewal'), \
                (SELECT processed_at IS NOT NULL FROM stripe_webhook_events WHERE event_id=$2)",
    )
    .bind(&user_id)
    .bind(&event_id)
    .fetch_one(&pool)
    .await
    .expect("read notice and pending receipt");
    assert_eq!(notice_count, 1);
    assert!(!processed);

    sqlx::query("DELETE FROM cloud_notice_outbox WHERE recipient_user_id=$1")
        .bind(&user_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM stripe_webhook_events WHERE event_id=$1")
        .bind(&event_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM cloud_provider_allocations WHERE allocation_id=$1")
        .bind(&allocation_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM cloud_coverage_sources WHERE source_id=$1")
        .bind(&source_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM cloud_provider_payers WHERE payer_id=$1")
        .bind(&payer_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM cloud_coverage_coordinators WHERE beneficiary_id=$1")
        .bind(&user_id)
        .execute(&pool)
        .await
        .unwrap();
    for table in [
        "cloud_coverage_heads",
        "cloud_coverage_revision_facts",
        "cloud_coverage_revisions",
    ] {
        sqlx::query(&format!("DELETE FROM {table} WHERE beneficiary_id=$1"))
            .bind(&user_id)
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query("DELETE FROM billing_personal_accounts WHERE user_id=$1")
        .bind(&user_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM billing_operations WHERE operation_id=$1")
        .bind(&seeded_operation)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM users WHERE id=$1")
        .bind(&user_id)
        .execute(&pool)
        .await
        .unwrap();
    stripe_task.abort();
}

#[tokio::test]
async fn personal_invoice_paid_is_not_blocked_by_an_open_transfer_invitation() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let fixture = Uuid::new_v4().simple().to_string();
    let user_id = format!("billing-transfer-webhook-{fixture}-user");
    let counterparty_id = format!("billing-transfer-webhook-{fixture}-counterparty");
    let organization_id = format!("billing-transfer-webhook-{fixture}-org");
    let transfer_id = format!("transfer:webhook-{fixture}");
    seed_user(&pool, &user_id).await;
    seed_user(&pool, &counterparty_id).await;
    let operation_id =
        seed_personal_account(&pool, &user_id, &format!("transfer-webhook-{fixture}")).await;

    sqlx::query("INSERT INTO organizations (id, enc_name, created_by) VALUES ($1, decode('6f7267', 'hex'), $2)")
        .bind(&organization_id)
        .bind(&counterparty_id)
        .execute(&pool)
        .await
        .expect("insert transfer destination organisation");

    let provider = Arc::new(TestProvider {
        observation: SubscriptionObservation::Missing,
        personal_period_end: Some(1_900_000_000),
    });
    let app = app_with_provider(pool.clone(), provider);
    let checkout_event_id = format!("evt_transfer_webhook_checkout_{fixture}");
    let checkout = serde_json::json!({
        "id": checkout_event_id,
        "created": 1_800_000_000,
        "api_version": STRIPE_API_VERSION,
        "type": "checkout.session.completed",
        "data": { "object": {
            "client_reference_id": format!("personal:{operation_id}"),
            "payment_status": "paid",
            "customer": format!("cus_transfer_webhook_{fixture}"),
            "subscription": format!("sub_transfer_webhook_{fixture}"),
            "payment_intent": format!("pi_transfer_webhook_{fixture}")
        }}
    })
    .to_string();
    assert_eq!(
        post_webhook(&app, &checkout, Some(&stripe_signature(&checkout))).await,
        StatusCode::OK
    );

    sqlx::query(
        "INSERT INTO billing_transfer_intents \
         (transfer_id, actor_user_id, counterparty_user_id, beneficiary_id, source_kind, \
          destination_kind, destination_organization_id, offer, quote_version, \
          quote_expires_at_epoch, consent_expires_at_epoch, effective_from, idempotency_key, \
          request_hash, provider_idempotency_key, source_provider_subscription_id, state) \
         VALUES ($1,$2,$3,$2,'personal','sponsor',$4,'standard_monthly',1, \
                 1900000000,2000000000,2000000000,$1,$1,$1,$5,'awaiting_consent')",
    )
    .bind(&transfer_id)
    .bind(&user_id)
    .bind(&counterparty_id)
    .bind(&organization_id)
    .bind(format!("sub_transfer_webhook_{fixture}"))
    .execute(&pool)
    .await
    .expect("insert open transfer invitation");

    let invoice_event_id = format!("evt_transfer_webhook_invoice_{fixture}");
    let invoice = serde_json::json!({
        "id": invoice_event_id,
        "created": 1_800_000_001,
        "api_version": STRIPE_API_VERSION,
        "type": "invoice.paid",
        "data": { "object": {
            "id": format!("in_transfer_webhook_{fixture}"),
            "payment_intent": format!("pi_transfer_webhook_renewal_{fixture}"),
            "period_end": 1_950_000_000,
            "lines": { "data": [{
                "parent": {
                    "type": "subscription_item_details",
                    "subscription_item_details": {
                        "subscription": format!("sub_transfer_webhook_{fixture}")
                    }
                },
                "period": { "end": 2_000_000_000 }
            }]},
            "parent": {
                "type": "subscription_details",
                "subscription_details": {
                    "subscription": format!("sub_transfer_webhook_{fixture}")
                }
            }
        }}
    })
    .to_string();
    assert_eq!(
        post_webhook(&app, &invoice, Some(&stripe_signature(&invoice))).await,
        StatusCode::OK,
        "an invitation must not make the paid invoice retry or roll back"
    );
    let (paid_through, processed): (i64, bool) = sqlx::query_as(
        "SELECT account.paid_through_epoch, event.processed_at IS NOT NULL \
         FROM billing_personal_accounts AS account \
         JOIN stripe_webhook_events AS event ON event.event_id = $2 \
         WHERE account.user_id = $1",
    )
    .bind(&user_id)
    .bind(&invoice_event_id)
    .fetch_one(&pool)
    .await
    .expect("load paid invoice and webhook receipt");
    assert_eq!(paid_through, 2_000_000_000);
    assert!(processed);

    let event_ids = vec![checkout_event_id, invoice_event_id];
    sqlx::query("DELETE FROM stripe_subscription_watermarks WHERE event_id = ANY($1)")
        .bind(&event_ids)
        .execute(&pool)
        .await
        .expect("clean webhook watermarks");
    sqlx::query("DELETE FROM stripe_webhook_events WHERE event_id = ANY($1)")
        .bind(&event_ids)
        .execute(&pool)
        .await
        .expect("clean webhook receipts");
    sqlx::query("DELETE FROM billing_transfer_intents WHERE transfer_id = $1")
        .bind(&transfer_id)
        .execute(&pool)
        .await
        .expect("clean transfer invitation");
    sqlx::query("DELETE FROM billing_personal_accounts WHERE user_id = $1")
        .bind(&user_id)
        .execute(&pool)
        .await
        .expect("clean personal account");
    sqlx::query("DELETE FROM billing_personal_events WHERE user_id = $1")
        .bind(&user_id)
        .execute(&pool)
        .await
        .expect("clean personal billing events");
    sqlx::query("DELETE FROM billing_operations WHERE actor_user_id = $1")
        .bind(&user_id)
        .execute(&pool)
        .await
        .expect("clean billing operation");
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(&organization_id)
        .execute(&pool)
        .await
        .expect("clean destination organisation");
    sqlx::query("DELETE FROM sessions WHERE user_id = ANY($1)")
        .bind(vec![user_id.clone(), counterparty_id.clone()])
        .execute(&pool)
        .await
        .expect("clean test sessions");
    sqlx::query("DELETE FROM users WHERE id = ANY($1)")
        .bind(vec![user_id, counterparty_id])
        .execute(&pool)
        .await
        .expect("clean test users");
}

#[tokio::test]
async fn personal_invoice_first_delivery_waits_for_checkout_binding_then_queues_coverage() {
    let Some(pool) = one_connection_pool_or_skip().await else {
        return;
    };
    let fixture = Uuid::new_v4().simple().to_string();
    let user_id = format!("billing-coverage-user-{fixture}");
    let token = seed_user(&pool, &user_id).await;
    let operation_id =
        seed_personal_account(&pool, &user_id, &format!("personal-coverage-{fixture}")).await;
    let allocation_reference: String = sqlx::query_scalar(
        "SELECT allocation_reference FROM billing_personal_accounts WHERE operation_id = $1",
    )
    .bind(&operation_id)
    .fetch_one(&pool)
    .await
    .expect("personal allocation reference");

    let catalogue = BillingPriceIds {
        standard_monthly: "price_coverage_month".into(),
        standard_annual: "price_coverage_year".into(),
        founding_monthly: "price_coverage_found_month".into(),
        founding_annual: "price_coverage_found_year".into(),
    };
    let coverage = StripeCoverageConfig::new(
        "acct_coverage_test",
        ProviderEnvironment::Test,
        catalogue.standard_monthly.clone(),
        catalogue.standard_annual.clone(),
    )
    .unwrap()
    .with_founding_prices(
        catalogue.founding_monthly.clone(),
        catalogue.founding_annual.clone(),
    )
    .unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let period_start = now.saturating_sub(3_600);
    let period_end = now + 2_592_000;
    let customer_id = format!("cus_coverage_person_{fixture}");
    let subscription_id = format!("sub_coverage_person_{fixture}");
    let initial_item_id = format!("si_coverage_person_{fixture}");
    let transition_item_id = format!("si_coverage_transition_{fixture}");
    let invoice_id = format!("in_coverage_{fixture}");
    let invoice = serde_json::json!({
        "id": invoice_id,
        "customer": customer_id,
        "parent": {"type":"subscription_details", "subscription_details": {
            "subscription":subscription_id,
            "metadata": {
                "sotto_allocation_reference": allocation_reference,
                "personal_user_id": user_id,
                "operation_id": operation_id
            }
        }},
        "status":"paid", "currency":"gbp", "amount_paid":299, "amount_due":299,
        "amount_remaining":0, "amount_overpaid":0,
        "amount_paid_off_stripe":0, "starting_balance":0,
        "livemode":false, "metadata":{"sotto_allocation_reference":allocation_reference},
        "lines":{"data":[{
            "id":"il_coverage_person", "quantity":1, "livemode":false,
            "parent":{"type":"subscription_item_details", "subscription_item_details": {
                "subscription":subscription_id, "subscription_item":initial_item_id
            }},
            "pricing":{"type":"price_details", "price_details":{"price":"price_coverage_month"}},
            "metadata":{"sotto_allocation_reference":allocation_reference},
            "period":{"start":period_start,"end":period_end}
        }]}
    });
    let line = invoice["lines"]["data"][0].clone();
    let payment = serde_json::json!({
        "id":format!("inpay_coverage_person_{fixture}"), "object":"invoice_payment", "invoice":invoice_id,
        "status":"paid", "amount_paid":299, "amount_requested":299, "currency":"gbp",
        "livemode":false,
        "payment":{"type":"payment_intent", "payment_intent":format!("pi_coverage_person_{fixture}")}
    });
    let next_invoice_id = format!("in_coverage_transition_{fixture}");
    let next_period_end = period_end + 2_592_000;
    let mut next_invoice = invoice.clone();
    next_invoice["id"] = serde_json::json!(next_invoice_id);
    next_invoice["amount_paid"] = serde_json::json!(299);
    next_invoice["amount_due"] = serde_json::json!(299);
    next_invoice["lines"]["data"][0]["id"] = serde_json::json!("il_coverage_transition");
    next_invoice["lines"]["data"][0]["parent"]["subscription_item_details"]["subscription_item"] =
        serde_json::json!(transition_item_id);
    next_invoice["lines"]["data"][0]["pricing"]["price_details"]["price"] =
        serde_json::json!("price_coverage_month");
    next_invoice["lines"]["data"][0]["period"]["start"] = serde_json::json!(period_end);
    next_invoice["lines"]["data"][0]["period"]["end"] = serde_json::json!(next_period_end);
    let next_payment = serde_json::json!({
        "id":format!("inpay_coverage_transition_{fixture}"), "object":"invoice_payment", "invoice":next_invoice_id,
        "status":"paid", "amount_paid":299, "amount_requested":299, "currency":"gbp",
        "livemode":false,
        "payment":{"type":"payment_intent", "payment_intent":format!("pi_coverage_transition_{fixture}")}
    });
    let listed_invoice = invoice.clone();
    let mut malformed_invoice = invoice.clone();
    malformed_invoice
        .as_object_mut()
        .expect("invoice object")
        .remove("id");
    let (origin, stripe_task, stripe_mock) = stripe_mock_server_with_state(HashMap::from([
        (
            "/v1/account".into(),
            serde_json::json!({"id":"acct_coverage_test","livemode":false}),
        ),
        (
            format!("/v1/subscriptions/{subscription_id}"),
            serde_json::json!({"id":subscription_id,"customer":customer_id,"status":"active","livemode":false}),
        ),
        (
            "/v1/invoices".into(),
            serde_json::json!({"object":"list","data":[listed_invoice],"has_more":false}),
        ),
        (format!("/v1/invoices/{invoice_id}"), invoice.clone()),
        (
            format!("/v1/invoices/in_coverage_auth_{fixture}"),
            serde_json::json!({"_http_status":401,"error":{"message":"invalid api key"}}),
        ),
        (
            format!("/v1/invoices/in_coverage_malformed_{fixture}"),
            malformed_invoice,
        ),
        (
            format!("/v1/invoices/{invoice_id}/lines"),
            serde_json::json!({"object":"list","data":[line],"has_more":false}),
        ),
        (
            "/v1/invoice_payments".into(),
            serde_json::json!({"object":"list","data":[payment],"has_more":false}),
        ),
        (
            format!("/v1/invoice_payments?invoice={next_invoice_id}&limit=100"),
            serde_json::json!({"object":"list","data":[next_payment],"has_more":false}),
        ),
        (format!("/v1/invoices/{next_invoice_id}"), next_invoice.clone()),
        (
            format!("/v1/invoices/{next_invoice_id}/lines"),
            serde_json::json!({"object":"list","data":next_invoice["lines"]["data"],"has_more":false}),
        ),
        (
            "/v1/credit_notes".into(),
            serde_json::json!({"object":"list","data":[],"has_more":false}),
        ),
        (
            "/v1/refunds".into(),
            serde_json::json!({"object":"list","data":[],"has_more":false}),
        ),
        (
            "/v1/disputes".into(),
            serde_json::json!({"object":"list","data":[],"has_more":false}),
        ),
    ]))
    .await;
    let stripe = StripeReadClient::for_test(
        "sk_test_coverage",
        &coverage,
        origin,
        StripeReadLimits::default(),
    )
    .unwrap();
    let provider = Arc::new(TestProvider {
        observation: SubscriptionObservation::Missing,
        personal_period_end: Some(period_end),
    });
    let billing = BillingState::with_provider_and_cloud_sales(
        provider,
        WEBHOOK_SECRET.into(),
        "https://app.sotto.test".into(),
        false,
    )
    .with_price_catalogue(catalogue)
    .with_personal_coverage(coverage.clone(), stripe.clone(), true);
    let app = app_with_billing_mode(
        pool.clone(),
        Some(billing),
        sotto_server::config::DeploymentMode::Cloud,
    );

    let mut invoice_event_object = invoice.clone();
    invoice_event_object
        .as_object_mut()
        .expect("invoice event object")
        .remove("amount_paid_off_stripe");
    let invoice_event = serde_json::json!({
        "id":format!("evt_coverage_invoice_{fixture}"), "created":now + 5,
        "api_version":STRIPE_API_VERSION, "type":"invoice.paid", "livemode":false,
        "data":{"object":invoice_event_object}
    })
    .to_string();

    // Stripe can deliver invoice.paid before the Checkout completion has linked the local row.
    // The failure occurs before any webhook/provider receipt or financial write is committed.
    assert_eq!(
        post_webhook(
            &app,
            &invoice_event,
            Some(&stripe_signature(&invoice_event))
        )
        .await,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    let receipt_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM stripe_webhook_events WHERE event_id = $1")
            .bind(format!("evt_coverage_invoice_{fixture}"))
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(receipt_count, 0);

    let checkout_event = serde_json::json!({
        "id":format!("evt_coverage_checkout_{fixture}"), "created":now,
        "api_version":STRIPE_API_VERSION, "type":"checkout.session.completed", "livemode":false,
        "data":{"object":{
            "client_reference_id":format!("personal:{operation_id}"), "payment_status":"paid",
            "customer":customer_id, "subscription":subscription_id,
            "payment_intent":format!("pi_coverage_person_{fixture}")
        }}
    })
    .to_string();
    assert_eq!(
        post_webhook(
            &app,
            &checkout_event,
            Some(&stripe_signature(&checkout_event))
        )
        .await,
        StatusCode::OK
    );

    // A Stripe credential failure is operational, not permanent invoice evidence. Keep the
    // webhook unprocessed so Stripe retries after the key or permissions are repaired.
    let auth_invoice_id = format!("in_coverage_auth_{fixture}");
    let mut auth_invoice = invoice.clone();
    auth_invoice["id"] = serde_json::json!(auth_invoice_id);
    let auth_event_id = format!("evt_coverage_auth_failure_{fixture}");
    let auth_event = serde_json::json!({
        "id":auth_event_id, "created":now + 1,
        "api_version":STRIPE_API_VERSION, "type":"invoice.paid", "livemode":false,
        "data":{"object":auth_invoice}
    })
    .to_string();
    assert_eq!(
        post_webhook(&app, &auth_event, Some(&stripe_signature(&auth_event))).await,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    let auth_receipt_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM stripe_webhook_events WHERE event_id = $1")
            .bind(&auth_event_id)
            .fetch_one(&pool)
            .await
            .expect("authentication failure must not be receipted");
    assert_eq!(auth_receipt_count, 0);

    // A Stripe invoice that cannot be decoded is a permanent evidence gap, unlike a
    // credential failure. Acknowledge the billing event but leave Cloud coverage untouched.
    let malformed_invoice_id = format!("in_coverage_malformed_{fixture}");
    let mut malformed_event_invoice = invoice.clone();
    malformed_event_invoice["id"] = serde_json::json!(malformed_invoice_id);
    let malformed_event_id = format!("evt_coverage_malformed_invoice_{fixture}");
    let malformed_event = serde_json::json!({
        "id":malformed_event_id, "created":now + 2,
        "api_version":STRIPE_API_VERSION, "type":"invoice.paid", "livemode":false,
        "data":{"object":malformed_event_invoice}
    })
    .to_string();
    assert_eq!(
        post_webhook(
            &app,
            &malformed_event,
            Some(&stripe_signature(&malformed_event))
        )
        .await,
        StatusCode::OK
    );
    let malformed_receipt: (i64, i64) = sqlx::query_as(
        "SELECT count(*), count(processed_at) FROM stripe_webhook_events WHERE event_id = $1",
    )
    .bind(&malformed_event_id)
    .fetch_one(&pool)
    .await
    .expect("malformed invoice receipt");
    assert_eq!(malformed_receipt, (1, 1));
    let malformed_coverage_rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM cloud_provider_event_receipts WHERE event_id = $1",
    )
    .bind(&malformed_event_id)
    .fetch_one(&pool)
    .await
    .expect("no coverage receipt for malformed Stripe evidence");
    assert_eq!(malformed_coverage_rows, 0);

    let (eligibility_status, eligibility) = get_eligibility(&app, &token).await;
    assert_eq!(eligibility_status, StatusCode::OK);
    assert_eq!(eligibility["state"], "unavailable");
    let missing_binding_event_id = format!("evt_coverage_missing_binding_{fixture}");
    let mut invoice_without_binding = invoice.clone();
    invoice_without_binding
        .as_object_mut()
        .expect("invoice object")
        .remove("metadata");
    invoice_without_binding["parent"]["subscription_details"]
        .as_object_mut()
        .expect("subscription details")
        .remove("metadata");
    invoice_without_binding["lines"]["data"][0]
        .as_object_mut()
        .expect("invoice line")
        .remove("metadata");
    let missing_binding_event = serde_json::json!({
        "id":missing_binding_event_id, "created":now + 3,
        "api_version":STRIPE_API_VERSION, "type":"invoice.paid", "livemode":false,
        "data":{"object":invoice_without_binding}
    })
    .to_string();
    assert_eq!(
        post_webhook(
            &app,
            &missing_binding_event,
            Some(&stripe_signature(&missing_binding_event))
        )
        .await,
        StatusCode::OK
    );
    let missing_binding_receipt: (i64, i64) = sqlx::query_as(
        "SELECT count(*), count(processed_at) FROM stripe_webhook_events WHERE event_id = $1",
    )
    .bind(missing_binding_event_id)
    .fetch_one(&pool)
    .await
    .expect("missing-binding webhook receipt count");
    assert_eq!(missing_binding_receipt.0, 1);
    assert_eq!(missing_binding_receipt.1, 1);
    let paid_through_after_missing_binding: i64 = sqlx::query_scalar(
        "SELECT paid_through_epoch FROM billing_personal_accounts WHERE user_id = $1",
    )
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("billing update despite missing coverage binding");
    assert_eq!(paid_through_after_missing_binding, period_end);

    // A fully credited invoice cannot prove Cloud coverage, but its signed paid event still
    // advances billing and is acknowledged so Stripe does not retry a permanent evidence gap.
    let credited_event_id = format!("evt_coverage_credited_invoice_{fixture}");
    let mut credited_invoice = invoice.clone();
    credited_invoice["amount_paid"] = serde_json::json!(0);
    credited_invoice["amount_due"] = serde_json::json!(0);
    let credited_event = serde_json::json!({
        "id":credited_event_id, "created":now + 4,
        "api_version":STRIPE_API_VERSION, "type":"invoice.paid", "livemode":false,
        "data":{"object":credited_invoice}
    })
    .to_string();
    assert_eq!(
        post_webhook(
            &app,
            &credited_event,
            Some(&stripe_signature(&credited_event))
        )
        .await,
        StatusCode::OK
    );
    let credited_receipt: (i64, i64) = sqlx::query_as(
        "SELECT count(*), count(processed_at) FROM stripe_webhook_events WHERE event_id = $1",
    )
    .bind(&credited_event_id)
    .fetch_one(&pool)
    .await
    .expect("credited invoice receipt");
    assert_eq!(credited_receipt.0, 1);
    assert_eq!(credited_receipt.1, 1);
    let credited_coverage_rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM cloud_provider_event_receipts WHERE event_id = $1",
    )
    .bind(&credited_event_id)
    .fetch_one(&pool)
    .await
    .expect("no coverage receipt for unsupported settlement");
    assert_eq!(credited_coverage_rows, 0);
    let credited_diagnostics: (i64, String, String) = sqlx::query_as(
        "SELECT count(*)::bigint, min(coverage_kind), min(reason_code) \
         FROM cloud_coverage_rejection_incidents WHERE event_id = $1 \
           AND provider_account_id = 'acct_coverage_test' AND provider_environment = 'test'",
    )
    .bind(&credited_event_id)
    .fetch_one(&pool)
    .await
    .expect("persisted paid-but-uncovered diagnostic");
    assert_eq!(
        credited_diagnostics,
        (1, "personal".into(), "invoice_evidence_unsupported".into())
    );
    let paid_through_after_credit: i64 = sqlx::query_scalar(
        "SELECT paid_through_epoch FROM billing_personal_accounts WHERE user_id = $1",
    )
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("billing update despite unsupported coverage settlement");
    assert_eq!(paid_through_after_credit, period_end);

    assert_eq!(
        post_webhook(
            &app,
            &invoice_event,
            Some(&stripe_signature(&invoice_event))
        )
        .await,
        StatusCode::OK
    );
    let (queued_status, queued_view) = get_eligibility(&app, &token).await;
    assert_eq!(queued_status, StatusCode::OK);
    assert_eq!(queued_view["state"], "unavailable");

    let provider_rows: (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM cloud_provider_event_receipts WHERE event_id = $1), \
                (SELECT count(*) FROM cloud_provider_allocations WHERE external_allocation_reference = $2), \
                (SELECT count(*) FROM cloud_provider_refresh_jobs WHERE event_id = $1)",
    )
    .bind(format!("evt_coverage_invoice_{fixture}"))
    .bind(&allocation_reference)
    .fetch_one(&pool)
    .await
    .expect("provider coverage intake rows");
    assert_eq!(provider_rows, (1, 1, 1));
    let stored_payment: String = sqlx::query_scalar(
        "SELECT payment_reference FROM billing_personal_accounts WHERE user_id = $1",
    )
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("paid invoice settlement");
    assert_eq!(stored_payment, format!("pi_coverage_person_{fixture}"));

    let mut executor =
        sotto_server::cloud_provider_stripe_personal_refresh::PersonalStripeRefreshExecutor::new(
            pool.clone(),
            stripe.clone(),
            coverage.clone(),
        );
    let worker_outcome = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        sotto_server::cloud_provider_refresh_worker::run_once(
            &pool,
            &format!("coverage-worker-{fixture}"),
            &mut executor,
        ),
    )
    .await
    .expect("personal Stripe reads must not hold the only database connection")
    .expect("personal coverage worker run");
    assert_eq!(
        worker_outcome,
        sotto_server::cloud_provider_refresh_worker::RefreshWorkerOutcome::Completed
    );
    let (projected_status, projected_view) = get_eligibility(&app, &token).await;
    assert_eq!(projected_status, StatusCode::OK);
    assert_eq!(projected_view["state"], "paid");
    assert_eq!(
        sotto_server::cloud_action_policy::decide(
            sotto_server::cloud_action_policy::ActionClass::SecretRead,
            sotto_server::person_eligibility::EligibilityState::Paid,
        ),
        sotto_server::cloud_action_policy::ShadowDecision::Allowed
    );

    // A scheduled price transition may replace Stripe's subscription item while preserving the
    // paid subscription and Sotto allocation. A contiguous, detailed paid invoice is the proof
    // that lets the allocation advance to the replacement item.
    stripe_mock.0.lock().unwrap().insert(
        "/v1/invoices".into(),
        serde_json::json!({"object":"list","data":[next_invoice.clone(), listed_invoice],"has_more":false}),
    );
    let mut next_event_invoice = next_invoice.clone();
    next_event_invoice
        .as_object_mut()
        .expect("invoice object")
        .remove("amount_paid_off_stripe");
    let next_event_id = format!("evt_coverage_transition_{fixture}");
    let next_event = serde_json::json!({
        "id":next_event_id, "created":now + 7,
        "api_version":STRIPE_API_VERSION, "type":"invoice.paid", "livemode":false,
        "data":{"object":next_event_invoice}
    })
    .to_string();
    assert_eq!(
        post_webhook(&app, &next_event, Some(&stripe_signature(&next_event))).await,
        StatusCode::OK
    );
    let current_item: String = sqlx::query_scalar(
        "SELECT provider_item_id FROM cloud_provider_allocations WHERE external_allocation_reference = $1",
    )
    .bind(&allocation_reference)
    .fetch_one(&pool)
    .await
    .expect("advanced personal provider item binding");
    assert_eq!(current_item, transition_item_id);
    let next_paid_through: i64 = sqlx::query_scalar(
        "SELECT paid_through_epoch FROM billing_personal_accounts WHERE user_id = $1",
    )
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("second paid term advances billing");
    assert_eq!(next_paid_through, next_period_end);
    let mut transition_executor =
        sotto_server::cloud_provider_stripe_personal_refresh::PersonalStripeRefreshExecutor::new(
            pool.clone(),
            stripe,
            coverage,
        );
    assert_eq!(
        sotto_server::cloud_provider_refresh_worker::run_once(
            &pool,
            &format!("coverage-transition-worker-{fixture}"),
            &mut transition_executor,
        )
        .await
        .expect("personal transition refresh"),
        sotto_server::cloud_provider_refresh_worker::RefreshWorkerOutcome::Completed
    );
    let (transition_status, transition_view) = get_eligibility(&app, &token).await;
    assert_eq!(transition_status, StatusCode::OK);
    assert_eq!(transition_view["state"], "paid");

    // A processed redelivery must be acknowledged from its durable receipt without depending
    // on the current Stripe binding or making another provider read.
    let replaced_reference = format!("alloc_replaced_{fixture}");
    sqlx::query(
        "UPDATE billing_personal_accounts SET allocation_reference = $2 WHERE user_id = $1",
    )
    .bind(&user_id)
    .bind(replaced_reference)
    .execute(&pool)
    .await
    .expect("replace personal binding after accepted invoice");
    assert_eq!(
        post_webhook(
            &app,
            &invoice_event,
            Some(&stripe_signature(&invoice_event))
        )
        .await,
        StatusCode::OK
    );
    sqlx::query(
        "UPDATE billing_personal_accounts SET allocation_reference = $2, \
         stripe_coverage_binding_required = FALSE WHERE user_id = $1",
    )
    .bind(&user_id)
    .bind(&allocation_reference)
    .execute(&pool)
    .await
    .expect("mark migrated personal account as legacy-bound");

    // Accounts created before coverage binding was introduced cannot have the new reference in
    // Stripe metadata. They still receive their billing update, without creating coverage work.
    let mut legacy_invoice = invoice.clone();
    legacy_invoice
        .as_object_mut()
        .expect("invoice object")
        .remove("metadata");
    legacy_invoice["parent"]["subscription_details"]["metadata"] = serde_json::json!({
        "personal_user_id": user_id,
        "operation_id": operation_id
    });
    legacy_invoice["lines"]["data"][0]
        .as_object_mut()
        .expect("invoice line")
        .remove("metadata");
    let legacy_event_id = format!("evt_coverage_legacy_invoice_{fixture}");
    let legacy_invoice_event = serde_json::json!({
        "id":legacy_event_id, "created":now + 6,
        "api_version":STRIPE_API_VERSION, "type":"invoice.paid", "livemode":false,
        "data":{"object":legacy_invoice}
    })
    .to_string();
    assert_eq!(
        post_webhook(
            &app,
            &legacy_invoice_event,
            Some(&stripe_signature(&legacy_invoice_event))
        )
        .await,
        StatusCode::OK
    );
    let legacy_provider_work: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM cloud_provider_event_receipts WHERE event_id = $1), \
                (SELECT count(*) FROM cloud_provider_refresh_jobs WHERE event_id = $1)",
    )
    .bind(&legacy_event_id)
    .fetch_one(&pool)
    .await
    .expect("legacy invoice must not enqueue provider work");
    assert_eq!(legacy_provider_work, (0, 0));
    let legacy_receipt_processed: bool = sqlx::query_scalar(
        "SELECT processed_at IS NOT NULL FROM stripe_webhook_events WHERE event_id = $1",
    )
    .bind(&legacy_event_id)
    .fetch_one(&pool)
    .await
    .expect("legacy invoice webhook receipt");
    assert!(legacy_receipt_processed);

    // Remove the queue-owned and provider rows first; the beneficiary/source FKs deliberately
    // prevent ordinary user deletion while evidence exists.
    sqlx::query("DELETE FROM cloud_provider_invalidation_associations WHERE event_id = ANY($1)")
        .bind(vec![
            format!("evt_coverage_invoice_{fixture}"),
            format!("evt_coverage_transition_{fixture}"),
        ])
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM cloud_provider_event_receipts WHERE event_id = ANY($1)")
        .bind(vec![
            format!("evt_coverage_invoice_{fixture}"),
            format!("evt_coverage_transition_{fixture}"),
        ])
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM cloud_provider_allocations WHERE external_allocation_reference = $1")
        .bind(&allocation_reference)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM cloud_coverage_sources WHERE beneficiary_id = $1")
        .bind(&user_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE cloud_coverage_coordinators SET current_attempt_id = NULL WHERE beneficiary_id = $1")
        .bind(&user_id)
        .execute(&pool)
        .await
        .unwrap();
    for table in [
        "cloud_coverage_collection_attempts",
        "cloud_coverage_coordinators",
    ] {
        sqlx::query(&format!("DELETE FROM {table} WHERE beneficiary_id = $1"))
            .bind(&user_id)
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query("DELETE FROM cloud_provider_payers WHERE payer_kind = 'personal' AND provider_customer_id = $1")
        .bind(&customer_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM stripe_subscription_watermarks WHERE event_id = ANY($1)")
        .bind(vec![
            format!("evt_coverage_invoice_{fixture}"),
            format!("evt_coverage_transition_{fixture}"),
            format!("evt_coverage_checkout_{fixture}"),
            format!("evt_coverage_legacy_invoice_{fixture}"),
        ])
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM stripe_webhook_events WHERE event_id = ANY($1)")
        .bind(vec![
            format!("evt_coverage_invoice_{fixture}"),
            format!("evt_coverage_transition_{fixture}"),
            format!("evt_coverage_checkout_{fixture}"),
            format!("evt_coverage_legacy_invoice_{fixture}"),
        ])
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "DELETE FROM billing_personal_events WHERE operation_id IN \
         (SELECT operation_id FROM billing_operations WHERE actor_user_id = $1)",
    )
    .bind(&user_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("DELETE FROM billing_personal_accounts WHERE user_id = $1")
        .bind(&user_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM billing_operations WHERE actor_user_id = $1")
        .bind(&user_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM cloud_coverage_heads WHERE beneficiary_id = $1")
        .bind(&user_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM cloud_coverage_revision_facts WHERE beneficiary_id = $1")
        .bind(&user_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM cloud_coverage_revisions WHERE beneficiary_id = $1")
        .bind(&user_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();
    stripe_task.abort();
}

#[tokio::test]
async fn signed_failed_renewal_is_persisted_before_the_repair_scan_window() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let fixture = Uuid::new_v4().simple().to_string();
    let user_id = format!("billing-renewal-user-{fixture}");
    let account_id = format!("acct_renewal_{fixture}");
    let customer_id = format!("cus_renewal_{fixture}");
    let subscription_id = format!("sub_renewal_{fixture}");
    let allocation_reference = format!("alloc_renewal_{fixture}");
    let allocation_id = format!("allocation_renewal_{fixture}");
    let payer_id = format!("payer_renewal_{fixture}");
    let source_id = format!("source_renewal_{fixture}");
    let event_id = format!("evt_renewal_{fixture}");
    let update_event_id = format!("evt_zz_subscription_updated_{fixture}");
    let invoice_id = format!("in_failed_{fixture}");
    let transitioned_item_id = format!("si_renewal_transition_{fixture}");
    seed_user(&pool, &user_id).await;
    let operation_id =
        seed_personal_account(&pool, &user_id, &format!("renewal-failure-{fixture}")).await;
    let paid_through = 172_800;
    sqlx::query(
        "UPDATE billing_personal_accounts SET state='active', stripe_customer_id=$2, \
           stripe_subscription_id=$3, paid_through_epoch=$4, paid_through_date=$5 \
         WHERE user_id=$1",
    )
    .bind(&user_id)
    .bind(&customer_id)
    .bind(&subscription_id)
    .bind(paid_through)
    .bind(
        FoundingDate::from_unix_seconds(paid_through)
            .unwrap()
            .to_string(),
    )
    .execute(&pool)
    .await
    .expect("activate the personal renewal notice fixture");

    let coverage = StripeCoverageConfig::new(
        &account_id,
        ProviderEnvironment::Test,
        "price_renewal_month",
        "price_renewal_year",
    )
    .unwrap();
    let context = ProviderContext::new("stripe", &account_id, ProviderEnvironment::Test).unwrap();
    let allocation = VerifiedAllocation::new(
        &allocation_id,
        &payer_id,
        &customer_id,
        ProviderPayerKind::Personal,
        &user_id,
        &subscription_id,
        "si_renewal",
        &allocation_reference,
        &source_id,
        0,
        None,
        AllocationState::Active,
        format!("stripe:renewal-owner:{fixture}"),
    )
    .unwrap();
    let mut register = pool.begin().await.expect("begin allocation registration");
    register_verified_allocation(&mut register, &context, &allocation)
        .await
        .expect("register the personal allocation");
    register
        .commit()
        .await
        .expect("commit allocation registration");

    let paid_invoice = serde_json::json!({
        "id":format!("in_paid_{fixture}"), "customer":customer_id,
        "parent":{"type":"subscription_details", "subscription_details":{"subscription":subscription_id}},
        "status":"paid", "currency":"gbp", "amount_paid":299, "amount_due":299,
        "amount_remaining":0, "amount_overpaid":0,
        "starting_balance":0, "livemode":false,
        "metadata":{"sotto_allocation_reference":allocation_reference}
    });
    let open_invoice = serde_json::json!({
        "id":invoice_id, "customer":customer_id,
        "parent":{"type":"subscription_details", "subscription_details":{"subscription":subscription_id}},
        "billing_reason":"subscription_cycle", "collection_method":"charge_automatically",
        "status":"open", "currency":"gbp", "amount_paid":0, "amount_due":299,
        "amount_remaining":299, "amount_overpaid":0,
        "starting_balance":0, "livemode":false,
        "metadata":{"sotto_allocation_reference":allocation_reference}
    });
    let paid_line = serde_json::json!({
        "id":format!("il_paid_{fixture}"), "quantity":1, "livemode":false,
        "parent":{"type":"subscription_item_details", "subscription_item_details":{
            "subscription":subscription_id, "subscription_item":"si_renewal", "proration":false
        }},
        "pricing":{"type":"price_details", "price_details":{"price":"price_renewal_month"}},
        "period":{"start":86_400, "end":172_800}
    });
    let failed_line = serde_json::json!({
        "id":format!("il_failed_{fixture}"), "invoice":invoice_id, "quantity":1, "livemode":false,
        "parent":{"type":"subscription_item_details", "subscription_item_details":{
            "subscription":subscription_id, "subscription_item":transitioned_item_id, "proration":false
        }},
        "pricing":{"type":"price_details", "price_details":{"price":"price_renewal_month"}},
        "period":{"start":172_800, "end":259_200}
    });
    let event_payload = serde_json::json!({
        "id":event_id, "created":259_200, "api_version":STRIPE_API_VERSION,
        "type":"invoice.payment_failed", "livemode":false,
        "data":{"object":{
            "object":"invoice", "id":invoice_id, "customer":customer_id,
            "parent":{"type":"subscription_details", "subscription_details":{"subscription":subscription_id}},
            "status":"open", "billing_reason":"subscription_cycle", "collection_method":"charge_automatically",
            "currency":"gbp", "amount_due":299, "amount_remaining":299, "amount_paid":0,
            "amount_overpaid":0, "starting_balance":0, "livemode":false,
            "metadata":{"sotto_allocation_reference":allocation_reference},
            "lines":{"object":"list", "has_more":false, "data":[failed_line]}
        }}
    })
    .to_string();
    let paid_id = paid_invoice["id"].as_str().unwrap().to_owned();
    let listed_paid = paid_invoice.clone();
    let mut paid_detail = paid_invoice.clone();
    paid_detail["amount_paid_off_stripe"] = serde_json::json!(0);
    paid_detail["lines"] =
        serde_json::json!({"object":"list", "data":[paid_line], "has_more":false});
    let mut open_detail = open_invoice.clone();
    open_detail["amount_paid_off_stripe"] = serde_json::json!(0);
    let (origin, stripe_task) = stripe_mock_server(HashMap::from([
        ("/v1/account".into(), serde_json::json!({"id":account_id, "object":"account"})),
        (format!("/v1/subscriptions/{subscription_id}"), serde_json::json!({"id":subscription_id, "customer":customer_id, "status":"active", "livemode":false})),
        ("/v1/invoices".into(), serde_json::json!({"object":"list", "data":[listed_paid, open_invoice], "has_more":false})),
        (format!("/v1/invoices/{paid_id}"), paid_detail),
        (format!("/v1/invoices/{invoice_id}"), open_detail),
        (format!("/v1/invoices/{paid_id}/lines"), serde_json::json!({"object":"list", "data":[paid_line], "has_more":false})),
        ("/v1/invoice_payments".into(), serde_json::json!({"object":"list", "data":[{
            "id":format!("inpay_{fixture}"), "invoice":paid_id, "status":"paid", "amount_paid":299,
            "amount_requested":299, "currency":"gbp", "livemode":false,
            "payment":{"type":"payment_intent", "payment_intent":format!("pi_{fixture}")}
        }], "has_more":false})),
        ("/v1/credit_notes".into(), serde_json::json!({"object":"list", "data":[], "has_more":false})),
        ("/v1/refunds".into(), serde_json::json!({"object":"list", "data":[], "has_more":false})),
        ("/v1/disputes".into(), serde_json::json!({"object":"list", "data":[], "has_more":false})),
    ])).await;
    let stripe = StripeReadClient::for_test(
        "sk_test_renewal",
        &coverage,
        origin,
        StripeReadLimits::default(),
    )
    .unwrap();
    let provider = Arc::new(TestProvider {
        observation: SubscriptionObservation::Missing,
        personal_period_end: None,
    });
    let billing = BillingState::with_provider_and_cloud_sales(
        provider,
        WEBHOOK_SECRET.into(),
        "https://app.sotto.test".into(),
        false,
    )
    .with_personal_coverage(coverage, stripe, true);
    let app = app_with_billing_mode(
        pool.clone(),
        Some(billing),
        sotto_server::config::DeploymentMode::Cloud,
    );

    // Subscription state owns the watermark even when it arrives before the payment failure.
    // Use the same Stripe timestamp and a lexically higher event id to pin the delivery order
    // that previously caused the failure evidence and customer notice to be dropped.
    let subscription_update = serde_json::json!({
        "id": update_event_id,
        "created": 259_200,
        "api_version": STRIPE_API_VERSION,
        "type": "customer.subscription.updated",
        "livemode": false,
        "data": {"object": {
            "object": "subscription",
            "id": subscription_id,
            "status": "past_due",
            "cancel_at_period_end": false,
            "metadata": {"personal_user_id": user_id, "operation_id": operation_id}
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

    assert_eq!(
        post_webhook(
            &app,
            &event_payload,
            Some(&stripe_signature(&event_payload))
        )
        .await,
        StatusCode::OK
    );
    let accepted: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM cloud_provider_stripe_renewal_failures WHERE event_id = $1",
    )
    .bind(&event_id)
    .fetch_one(&pool)
    .await
    .expect("query directly accepted failure evidence");
    assert_eq!(accepted, 1);
    let current_allocation_item: String = sqlx::query_scalar(
        "SELECT provider_item_id FROM cloud_provider_allocations WHERE allocation_id = $1",
    )
    .bind(&allocation_id)
    .fetch_one(&pool)
    .await
    .expect("failure evidence advances the item binding");
    assert_eq!(current_allocation_item, transitioned_item_id);
    let failure_notice_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM cloud_notice_outbox WHERE recipient_user_id=$1 AND kind='failed_renewal'",
    )
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("query the notice committed for the failure event");
    assert_eq!(failure_notice_count, 1);
    let watermark_event_id: String = sqlx::query_scalar(
        "SELECT event_id FROM stripe_subscription_watermarks WHERE subscription_id=$1",
    )
    .bind(&subscription_id)
    .fetch_one(&pool)
    .await
    .expect("query the subscription state watermark");
    assert_eq!(watermark_event_id, update_event_id);
    let processed: bool = sqlx::query_scalar(
        "SELECT processed_at IS NOT NULL FROM stripe_webhook_events WHERE event_id = $1",
    )
    .bind(&event_id)
    .fetch_one(&pool)
    .await
    .expect("query processed failure webhook");
    assert!(processed);
    assert_eq!(
        post_webhook(
            &app,
            &event_payload,
            Some(&stripe_signature(&event_payload))
        )
        .await,
        StatusCode::OK,
        "a duplicate signed failure is acknowledged as a harmless replay"
    );
    let replayed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM cloud_provider_stripe_renewal_failures WHERE event_id = $1",
    )
    .bind(&event_id)
    .fetch_one(&pool)
    .await
    .expect("query replayed failure evidence");
    assert_eq!(replayed, 1);

    // Renewal evidence retains the item that billed its own invoice. A later Stripe phase can
    // replace the live item again without invalidating the older, already verified failure row.
    let later_item_id = format!("si_later_phase_{fixture}");
    sqlx::query(
        "UPDATE cloud_provider_allocations SET provider_item_id = $2 WHERE allocation_id = $1",
    )
    .bind(&allocation_id)
    .bind(&later_item_id)
    .execute(&pool)
    .await
    .expect("simulate a later provider item transition");
    let mut later_allocation = allocation.clone();
    later_allocation.provider_item_id = later_item_id;
    let loaded_failures =
        sotto_server::cloud_provider_stripe_renewal_store::load_personal_renewal_failures(
            &pool,
            &context,
            &later_allocation,
            Default::default(),
        )
        .await
        .expect("historical failure evidence survives item replacement");
    assert_eq!(loaded_failures.len(), 1);
    assert_eq!(loaded_failures[0].provider_item_id(), transitioned_item_id);

    sqlx::query("DELETE FROM cloud_provider_stripe_renewal_failures WHERE event_id = $1")
        .bind(&event_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM cloud_provider_invalidation_associations WHERE event_id = $1")
        .bind(&event_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM cloud_provider_event_receipts WHERE event_id = $1")
        .bind(&event_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM stripe_subscription_watermarks WHERE subscription_id = $1")
        .bind(&subscription_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM stripe_webhook_events WHERE event_id = $1")
        .bind(&event_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM stripe_webhook_events WHERE event_id = $1")
        .bind(&update_event_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM cloud_notice_outbox WHERE recipient_user_id=$1")
        .bind(&user_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM billing_personal_accounts WHERE user_id=$1")
        .bind(&user_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM billing_operations WHERE operation_id=$1")
        .bind(&operation_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM cloud_provider_allocations WHERE allocation_id = $1")
        .bind(&allocation_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM cloud_coverage_sources WHERE source_id = $1")
        .bind(&source_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM cloud_provider_payers WHERE payer_id = $1")
        .bind(&payer_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM cloud_coverage_coordinators WHERE beneficiary_id = $1")
        .bind(&user_id)
        .execute(&pool)
        .await
        .unwrap();
    for table in [
        "cloud_coverage_heads",
        "cloud_coverage_revision_facts",
        "cloud_coverage_revisions",
    ] {
        sqlx::query(&format!("DELETE FROM {table} WHERE beneficiary_id = $1"))
            .bind(&user_id)
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(&user_id)
        .execute(&pool)
        .await
        .unwrap();
    stripe_task.abort();
}

#[tokio::test]
async fn expired_pending_personal_account_cannot_receive_a_new_quote() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let fixture = Uuid::new_v4().simple().to_string();
    let user_id = format!("billing-user-expired-pending-quote-{fixture}");
    let token = seed_user(&pool, &user_id).await;
    let operation_id =
        seed_personal_account(&pool, &user_id, &format!("expired-pending-quote-{fixture}")).await;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    sqlx::query(
        "UPDATE billing_personal_accounts SET pending_expires_at_epoch = $2 WHERE operation_id = $1",
    )
    .bind(&operation_id)
    .bind(now - 1)
    .execute(&pool)
    .await
    .expect("expire pending account locally");

    let billing = BillingState::with_provider_and_cloud_sales(
        Arc::new(TestProvider {
            observation: SubscriptionObservation::Missing,
            personal_period_end: None,
        }),
        WEBHOOK_SECRET.into(),
        "https://app.sotto.test".into(),
        true,
    )
    .with_price_catalogue(BillingPriceIds {
        standard_monthly: "price_standard_monthly".into(),
        standard_annual: "price_standard_annual".into(),
        founding_monthly: "price_founding_monthly".into(),
        founding_annual: "price_founding_annual".into(),
    });
    let app = app_with_billing_mode(
        pool.clone(),
        Some(billing),
        sotto_server::config::DeploymentMode::Cloud,
    );
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/billing/personal/quote?offer=standard_monthly")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("request personal quote");
    assert_eq!(response.status(), StatusCode::CONFLICT);

    sqlx::query("DELETE FROM billing_personal_accounts WHERE user_id = $1")
        .bind(&user_id)
        .execute(&pool)
        .await
        .expect("clean personal account");
    sqlx::query("DELETE FROM billing_operations WHERE operation_id = $1")
        .bind(&operation_id)
        .execute(&pool)
        .await
        .expect("clean operation");
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(&user_id)
        .execute(&pool)
        .await
        .expect("clean user");
}

#[tokio::test]
async fn delayed_personal_checkout_payment_honours_timely_founding_reservation() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let fixture = Uuid::new_v4().simple().to_string();
    let user_id = format!("billing-user-delayed-founding-{fixture}");
    seed_user(&pool, &user_id).await;
    let operation_id =
        seed_personal_account(&pool, &user_id, &format!("delayed-founding-{fixture}")).await;
    let checkout_created_at = 1_800_000_000;
    let quote_expires_at = checkout_created_at + 1_800;
    let checkout_deadline = quote_expires_at + 1_800;
    sqlx::query(
        "UPDATE billing_personal_accounts SET offer = 'founding_monthly', pending_expires_at_epoch = $2 \
         WHERE operation_id = $1",
    )
    .bind(&operation_id)
    .bind(checkout_deadline)
    .execute(&pool)
    .await
    .expect("set founding offer");
    sqlx::query(
        "UPDATE billing_operations SET offer = 'founding_monthly', quote_expires_at_epoch = $2 \
         WHERE operation_id = $1",
    )
    .bind(&operation_id)
    .bind(quote_expires_at)
    .execute(&pool)
    .await
    .expect("set founding quote");
    let mut reservation_tx = pool.begin().await.expect("begin founding reservation");
    assert!(matches!(
        reserve_with_payer_kind_until(
            &mut reservation_tx,
            &format!("founding:{operation_id}"),
            &operation_id,
            &user_id,
            FoundingPayerKind::Personal,
            &user_id,
            FoundingOffer::Monthly,
            1,
            quote_expires_at,
            checkout_deadline,
            checkout_created_at,
        )
        .await
        .expect("reserve founding place"),
        ReservationOutcome::Created(_)
    ));
    reservation_tx.commit().await.expect("commit reservation");

    let app = app_with_provider(
        pool.clone(),
        Arc::new(TestProvider {
            observation: SubscriptionObservation::Missing,
            personal_period_end: Some(1_900_000_000),
        }),
    );
    let checkout_completed_at = checkout_created_at + 1_700;
    let completed = serde_json::json!({
        "id": format!("evt_delayed_founding_checkout_completed_{fixture}"),
        "created": checkout_completed_at,
        "api_version": STRIPE_API_VERSION,
        "type": "checkout.session.completed",
        "data": { "object": {
            "id": "cs_delayed_founding",
            "client_reference_id": format!("personal:{operation_id}"),
            "payment_status": "unpaid",
            "customer": "cus_delayed_founding",
            "subscription": "sub_delayed_founding",
            "payment_intent": "pi_delayed_founding"
        }}
    })
    .to_string();
    assert_eq!(
        post_webhook(&app, &completed, Some(&stripe_signature(&completed))).await,
        StatusCode::OK
    );
    let recorded_completion: Option<i64> = sqlx::query_scalar(
        "SELECT checkout_completed_by_epoch FROM billing_founding_reservations WHERE operation_id = $1",
    )
    .bind(&operation_id)
    .fetch_one(&pool)
    .await
    .expect("record Checkout completion timestamp");
    assert_eq!(recorded_completion, Some(checkout_completed_at));

    let paid_at = checkout_completed_at + 5 * 24 * 60 * 60;
    let succeeded = serde_json::json!({
        "id": format!("evt_delayed_founding_payment_succeeded_{fixture}"),
        "created": paid_at,
        "api_version": STRIPE_API_VERSION,
        "type": "checkout.session.async_payment_succeeded",
        "data": { "object": {
            "id": "cs_delayed_founding",
            "client_reference_id": format!("personal:{operation_id}"),
            "payment_status": "paid",
            "status": "complete",
            "expires_at": checkout_deadline,
            "customer": "cus_delayed_founding",
            "subscription": "sub_delayed_founding",
            "payment_intent": "pi_delayed_founding"
        }}
    })
    .to_string();
    assert_eq!(
        post_webhook(&app, &succeeded, Some(&stripe_signature(&succeeded))).await,
        StatusCode::OK
    );
    let (account_state, reservation_state): (String, String) = sqlx::query_as(
        "SELECT a.state, r.status FROM billing_personal_accounts a \
         JOIN billing_founding_reservations r ON r.operation_id = a.operation_id \
         WHERE a.operation_id = $1",
    )
    .bind(&operation_id)
    .fetch_one(&pool)
    .await
    .expect("read delayed Checkout settlement");
    assert_eq!(account_state, "active");
    assert_eq!(reservation_state, "confirmed");
    let checkout_notices: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM cloud_notice_outbox WHERE recipient_user_id=$1 \
         AND kind='checkout_outcome' AND channel='in_app' AND policy_key='cloud-checkout-outcome-v1'",
    )
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("load checkout outcome notice created by webhook");
    assert_eq!(checkout_notices, 1);

    sqlx::query("DELETE FROM billing_founding_schedule_jobs WHERE beneficiary_id = $1")
        .bind(&user_id)
        .execute(&pool)
        .await
        .expect("clean founding schedule job");
    sqlx::query("DELETE FROM billing_founding_awards WHERE beneficiary_id = $1")
        .bind(&user_id)
        .execute(&pool)
        .await
        .expect("clean founding award");
    sqlx::query("DELETE FROM billing_founding_reservations WHERE operation_id = $1")
        .bind(&operation_id)
        .execute(&pool)
        .await
        .expect("clean founding reservation");
    sqlx::query("DELETE FROM billing_personal_accounts WHERE operation_id = $1")
        .bind(&operation_id)
        .execute(&pool)
        .await
        .expect("clean personal account");
    sqlx::query("DELETE FROM billing_personal_events WHERE operation_id = $1")
        .bind(&operation_id)
        .execute(&pool)
        .await
        .expect("clean personal billing events");
    sqlx::query("DELETE FROM billing_operations WHERE operation_id = $1")
        .bind(&operation_id)
        .execute(&pool)
        .await
        .expect("clean billing operation");
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(&user_id)
        .execute(&pool)
        .await
        .expect("clean delayed founding user");
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
async fn personal_checkout_expiry_notice_is_independent_of_sponsored_billing() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let user_id = "billing-user-personal-checkout-expiry-notice";
    seed_user(&pool, user_id).await;
    let operation_id =
        seed_personal_account(&pool, user_id, "personal-checkout-expiry-notice").await;
    sqlx::query("DELETE FROM stripe_webhook_events WHERE event_id = $1")
        .bind("evt_personal_checkout_expiry_notice")
        .execute(&pool)
        .await
        .expect("pre-clean personal checkout expiry webhook receipt");
    let app = app_with_provider(
        pool.clone(),
        Arc::new(TestProvider {
            observation: SubscriptionObservation::Missing,
            personal_period_end: None,
        }),
    );
    let payload = serde_json::json!({
        "id": "evt_personal_checkout_expiry_notice",
        "created": 1_800_000_001,
        "api_version": STRIPE_API_VERSION,
        "type": "checkout.session.expired",
        "data": { "object": {
            "id": "cs_personal_checkout_expired_notice",
            "client_reference_id": format!("personal:{operation_id}"),
            "status": "expired",
            "payment_status": "unpaid"
        }}
    })
    .to_string();

    assert_eq!(
        post_webhook(&app, &payload, Some(&stripe_signature(&payload))).await,
        StatusCode::OK
    );
    let account_state: String =
        sqlx::query_scalar("SELECT state FROM billing_personal_accounts WHERE operation_id=$1")
            .bind(&operation_id)
            .fetch_one(&pool)
            .await
            .expect("read expired personal account");
    assert_eq!(account_state, "canceled");
    let (operation_state, result_code, session_id): (String, String, Option<String>) =
        sqlx::query_as(
            "SELECT state, result_code, provider_checkout_session_id FROM billing_operations \
             WHERE operation_id=$1",
        )
        .bind(&operation_id)
        .fetch_one(&pool)
        .await
        .expect("read expired personal operation");
    assert_eq!(operation_state, "failed");
    assert_eq!(result_code, "checkout_expired");
    assert_eq!(
        session_id.as_deref(),
        Some("cs_personal_checkout_expired_notice")
    );
    let checkout_notices: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM cloud_notice_outbox WHERE recipient_user_id=$1 \
         AND kind='checkout_outcome' AND channel='in_app' \
         AND policy_key='cloud-checkout-outcome-v1'",
    )
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .expect("load personal checkout expiry notice");
    assert_eq!(checkout_notices, 1);

    sqlx::query("DELETE FROM cloud_notice_outbox WHERE recipient_user_id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("clean personal checkout expiry notice");
    sqlx::query("DELETE FROM billing_personal_accounts WHERE user_id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("clean personal checkout expiry account");
    sqlx::query("DELETE FROM billing_personal_events WHERE operation_id = $1")
        .bind(&operation_id)
        .execute(&pool)
        .await
        .expect("clean personal checkout expiry events");
    sqlx::query("DELETE FROM billing_operations WHERE operation_id = $1")
        .bind(operation_id)
        .execute(&pool)
        .await
        .expect("clean personal checkout expiry operation");
    sqlx::query("DELETE FROM stripe_webhook_events WHERE event_id = $1")
        .bind("evt_personal_checkout_expiry_notice")
        .execute(&pool)
        .await
        .expect("clean personal checkout expiry webhook receipt");
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("clean personal checkout expiry user");
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
