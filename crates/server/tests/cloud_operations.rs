use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use sqlx::PgPool;
use tower::ServiceExt;

use sotto_server::config::{DeploymentMode, DEFAULT_ORGANISATION_DELETION_RETENTION_DAYS};
use sotto_server::state::AppState;

fn app(metrics_token: Option<&str>, repair_token: Option<&str>) -> Router {
    let pool = PgPool::connect_lazy("postgres://127.0.0.1:1/sotto").expect("lazy pool");
    app_for(pool, metrics_token, repair_token)
}

fn app_for(pool: PgPool, metrics_token: Option<&str>, repair_token: Option<&str>) -> Router {
    Router::new()
        .merge(sotto_server::cloud_operations::router())
        .with_state(AppState {
            pool,
            deployment_mode: DeploymentMode::SelfHosted,
            oauth: None,
            oauth_config: None,
            billing: None,
            contact_verification: None,
            cloud_notice_dispatch_enabled: false,
            resend_webhook_secret: None,
            telemetry_ingest: false,
            organisation_deletion_enabled: false,
            organisation_deletion_retention_days: DEFAULT_ORGANISATION_DELETION_RETENTION_DAYS,
            organisation_deletion_metrics_token: None,
            cloud_operations_metrics_token: metrics_token.map(str::to_owned),
            cloud_coverage_repair_token: repair_token.map(str::to_owned),
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
        })
}

async fn send(
    app: &Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: &str,
) -> axum::response::Response {
    let mut request = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    if !body.is_empty() {
        request = request.header("content-type", "application/json");
    }
    app.clone()
        .oneshot(request.body(Body::from(body.to_owned())).expect("request"))
        .await
        .expect("response")
}

#[tokio::test]
async fn protected_cloud_metrics_are_dark_without_a_read_credential() {
    let response = send(
        &app(None, Some("repair")),
        "GET",
        "/ops/cloud/metrics",
        None,
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn protected_cloud_metrics_reject_missing_and_wrong_credentials_before_database_access() {
    let app = app(Some("read-only"), Some("repair-only"));
    assert_eq!(
        send(&app, "GET", "/ops/cloud/metrics", None, "")
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        send(&app, "GET", "/ops/cloud/metrics", Some("repair-only"), "")
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn read_credential_cannot_invoke_coverage_repair() {
    let app = app(Some("read-only"), Some("repair-only"));
    let response = send(
        &app,
        "POST",
        &format!("/ops/cloud/coverage-rejections/{}/repair", "a".repeat(64)),
        Some("read-only"),
        r#"{"operator_label":"operator"}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn rejection_cursor_is_validated_without_provider_or_database_access() {
    let app = app(Some("read-only"), Some("repair-only"));
    let response = send(
        &app,
        "GET",
        "/ops/cloud/coverage-rejections?cursor=not-a-fingerprint",
        Some("read-only"),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(response.headers().get("cache-control").unwrap(), "no-store");
}

#[tokio::test]
async fn paid_coverage_rejection_is_durable_and_metrics_redact_provider_identity() {
    if std::env::var("SOTTO_RUN_DB_TESTS").as_deref() != Ok("1") {
        eprintln!("skipping Cloud operations DB test: set SOTTO_RUN_DB_TESTS=1");
        return;
    }
    let database_url =
        std::env::var("DATABASE_URL").expect("DATABASE_URL is required when SOTTO_RUN_DB_TESTS=1");
    let pool = sotto_server::db::connect(&database_url)
        .await
        .expect("connect");
    sotto_server::db::migrate(&pool).await.expect("migrate");
    let event_id = format!("evt_cloud_ops_{}", uuid::Uuid::new_v4());
    let account_id = format!("acct_cloud_ops_{}", uuid::Uuid::new_v4());
    sotto_server::cloud_operations::record_coverage_rejection(
        &pool,
        &account_id,
        sotto_server::cloud_provider::ProviderEnvironment::Test,
        &event_id,
        "personal",
        "invoice_evidence_changed",
    )
    .await
    .expect("persist rejection");
    let manual_review_operation = format!("cloud_ops_manual_review_{}", uuid::Uuid::new_v4());
    sqlx::query(
        "INSERT INTO billing_operations (operation_id, idempotency_key, request_hash, \
         actor_user_id, payer_id, beneficiary_id, offer, quote_version, quote_expires_at_epoch, \
         provider_idempotency_key, state, result_code, reconciliation_manual_review_required) \
         VALUES ($1, $1, 'request-hash', 'actor', 'payer', 'beneficiary', 'standard_monthly', \
                 1, 9999999999, $1, 'manual_review', 'provider_outcome_unknown', TRUE)",
    )
    .bind(&manual_review_operation)
    .execute(&pool)
    .await
    .expect("seed parked billing operation");

    let response = send(
        &app_for(pool.clone(), Some("read-only"), None),
        "GET",
        "/ops/cloud/metrics",
        Some("read-only"),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/plain; version=0.0.4"
    );
    assert_eq!(response.headers().get("cache-control").unwrap(), "no-store");
    let body = axum::body::to_bytes(response.into_body(), 32_000)
        .await
        .expect("body");
    let body = String::from_utf8(body.to_vec()).expect("utf8");
    assert!(body.contains("sotto_cloud_coverage_rejections_open "));
    assert!(body.contains(
        "sotto_cloud_lifecycle_operations{component=\"billing_recovery\",state=\"manual_review\"} 1"
    ));
    assert!(!body.contains(&event_id));
    assert!(!body.contains(&account_id));

    let fingerprint: String = sqlx::query_scalar(
        "SELECT fingerprint FROM cloud_coverage_rejection_incidents WHERE event_id = $1",
    )
    .bind(&event_id)
    .fetch_one(&pool)
    .await
    .expect("read rejection fingerprint");
    sqlx::query(
        "UPDATE cloud_coverage_rejection_incidents SET state = 'repairing', \
         repair_lease_token = 'test-lease', repair_lease_until = now() + interval '2 minutes' \
         WHERE fingerprint = $1",
    )
    .bind(&fingerprint)
    .execute(&pool)
    .await
    .expect("claim fixture repair");
    sotto_server::cloud_operations::resolve_coverage_rejections(
        &pool,
        &account_id,
        sotto_server::cloud_provider::ProviderEnvironment::Test,
        &event_id,
    )
    .await
    .expect("automatic resolution must respect a claimed repair");
    let repairing: bool = sqlx::query_scalar(
        "SELECT state = 'repairing' AND repair_lease_token = 'test-lease' \
         FROM cloud_coverage_rejection_incidents WHERE fingerprint = $1",
    )
    .bind(&fingerprint)
    .fetch_one(&pool)
    .await
    .expect("read repair fixture");
    assert!(
        repairing,
        "the lease-owning action must finish its audit row"
    );

    sqlx::query("DELETE FROM cloud_coverage_rejection_incidents WHERE event_id = $1")
        .bind(event_id)
        .execute(&pool)
        .await
        .expect("clean fixture");
    sqlx::query("DELETE FROM billing_operations WHERE operation_id = $1")
        .bind(manual_review_operation)
        .execute(&pool)
        .await
        .expect("clean parked operation fixture");
}
