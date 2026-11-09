//! Protected payer-transfer review routes are absent unless an operator token is configured.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use sqlx::{
    postgres::{PgConnectOptions, PgPoolOptions},
    PgPool,
};
use tower::ServiceExt;
use uuid::Uuid;

use sotto_server::billing_transfers::{
    list_manual_review_transfers, load_manual_review_transfer, mark_failed,
    record_operator_review_decision, TransferError, TransferReviewAction,
};
use sotto_server::config::{DeploymentMode, DEFAULT_ORGANISATION_DELETION_RETENTION_DAYS};
use sotto_server::db;
use sotto_server::state::AppState;

fn app(token: Option<&str>) -> Router {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://sotto:sotto@127.0.0.1:1/sotto")
        .expect("build disconnected pool without contacting a database");
    sotto_server::app(AppState {
        pool,
        deployment_mode: DeploymentMode::Cloud,
        oauth: None,
        oauth_config: None,
        billing: None,
        telemetry_ingest: false,
        contact_verification: None,
        cloud_notice_dispatch_enabled: false,
        resend_webhook_secret: None,
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
        billing_transfer_operator_token: token.map(str::to_owned),
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

async fn pool_or_skip() -> Option<PgPool> {
    if std::env::var("SOTTO_RUN_DB_TESTS").as_deref() != Ok("1") {
        eprintln!("skipping payer transfer operator tests: SOTTO_RUN_DB_TESTS=1 not set");
        return None;
    }
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL for opted-in database tests");
    let options = url.parse::<PgConnectOptions>().expect("parse DATABASE_URL");
    assert!(
        matches!(options.get_host(), "localhost" | "127.0.0.1" | "::1"),
        "refusing payer transfer operator tests against non-local host: {}",
        options.get_host()
    );
    let pool = db::connect(&url).await.expect("connect to test database");
    db::migrate(&pool).await.expect("apply migrations");
    Some(pool)
}

fn app_for_pool(pool: PgPool, token: &str) -> Router {
    sotto_server::app(AppState {
        pool,
        deployment_mode: DeploymentMode::Cloud,
        oauth: None,
        oauth_config: None,
        billing: None,
        telemetry_ingest: false,
        contact_verification: None,
        cloud_notice_dispatch_enabled: false,
        resend_webhook_secret: None,
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
        billing_transfer_operator_token: Some(token.to_owned()),
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

async fn seed_manual_review(pool: &PgPool) -> (String, String, String) {
    let fixture = Uuid::new_v4().to_string();
    let user = format!("transfer-ops-test-{fixture}-user");
    let org = format!("transfer-ops-test-{fixture}-org");
    let transfer = format!("transfer:ops-test-{fixture}");
    sqlx::query("INSERT INTO users (id, oauth_provider, oauth_subject) VALUES ($1, 'transfer-ops-test', $1)")
        .bind(&user)
        .execute(pool)
        .await
        .expect("insert operator review beneficiary");
    sqlx::query("INSERT INTO organizations (id, enc_name, created_by) VALUES ($1, decode('6f7267', 'hex'), $2)")
        .bind(&org)
        .bind(&user)
        .execute(pool)
        .await
        .expect("insert transfer destination organisation");
    sqlx::query(
        "INSERT INTO billing_transfer_intents \
         (transfer_id, actor_user_id, counterparty_user_id, beneficiary_id, source_kind, \
          destination_kind, destination_organization_id, offer, quote_version, \
          quote_expires_at_epoch, consent_expires_at_epoch, effective_from, idempotency_key, request_hash, \
          provider_idempotency_key, source_provider_subscription_id, state, result_code, \
          execution_manual_review_required, execution_review_generation, \
          execution_next_attempt_at, last_execution_error_code) \
         VALUES ($1, $2, $2, $2, 'personal', 'sponsor', $3, 'standard_monthly', 1, \
                 2000000000, 2000000000, 2000001000, $4, 'request-hash', $5, 'sub_source', 'unknown', \
                 'manual_review_required', TRUE, 1, 'infinity'::timestamptz, \
                 'provider_outcome_unknown')",
    )
    .bind(&transfer)
    .bind(&user)
    .bind(&org)
    .bind(format!("operator-test-{fixture}"))
    .bind(format!("provider-test-{fixture}"))
    .execute(pool)
    .await
    .expect("insert parked transfer");
    (fixture, transfer, user)
}

async fn cleanup(pool: &PgPool, fixture: &str) {
    let pattern = format!("transfer:ops-test-{fixture}%");
    sqlx::query("DELETE FROM billing_transfer_review_decisions WHERE transfer_id LIKE $1")
        .bind(&pattern)
        .execute(pool)
        .await
        .expect("delete transfer review decisions");
    sqlx::query("DELETE FROM billing_transfer_intents WHERE transfer_id LIKE $1")
        .bind(&pattern)
        .execute(pool)
        .await
        .expect("delete transfer review fixture");
    sqlx::query("DELETE FROM organizations WHERE id LIKE $1")
        .bind(format!("transfer-ops-test-{fixture}-%"))
        .execute(pool)
        .await
        .expect("delete transfer review organisation");
    sqlx::query("DELETE FROM users WHERE id LIKE $1")
        .bind(format!("transfer-ops-test-{fixture}-%"))
        .execute(pool)
        .await
        .expect("delete transfer review user");
}

#[tokio::test]
async fn manual_review_routes_are_not_mounted_without_the_operator_token() {
    let response = app(None)
        .oneshot(
            Request::builder()
                .uri("/ops/billing/transfers/manual-review")
                .body(Body::empty())
                .expect("build request"),
        )
        .await
        .expect("route response");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn manual_review_routes_reject_missing_and_invalid_operator_tokens_before_database_access() {
    let app = app(Some("configured-test-token"));
    for authorization in [None, Some("Bearer wrong-token")] {
        let mut request = Request::builder().uri("/ops/billing/transfers/manual-review");
        if let Some(authorization) = authorization {
            request = request.header("authorization", authorization);
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::empty()).expect("build request"))
            .await
            .expect("route response");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}

#[tokio::test]
async fn operator_can_review_hold_and_resume_a_parked_transfer() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let (fixture, transfer_id, _user_id) = seed_manual_review(&pool).await;
    let token = "0123456789abcdef0123456789abcdef";
    let app = app_for_pool(pool.clone(), token);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/ops/billing/transfers/manual-review?limit=10")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .expect("build queue request"),
        )
        .await
        .expect("review queue response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers().get("cache-control").unwrap(), "no-store");
    let queue: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read queue response"),
    )
    .expect("decode review queue");
    let queued_transfer = queue["transfers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|transfer| transfer["transfer_id"] == transfer_id)
        .expect("fixture transfer appears in the review queue");
    assert_eq!(queued_transfer["execution_review_generation"], 1);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/ops/billing/transfers/{transfer_id}/manual-review"
                ))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .expect("build detail request"),
        )
        .await
        .expect("review detail response");
    assert_eq!(response.status(), StatusCode::OK);
    let detail: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read detail"),
    )
    .expect("decode detail");
    assert!(detail["decisions"].as_array().unwrap().is_empty());

    let body = serde_json::json!({
        "operator": "billing-operator-test",
        "action": "hold",
        "reason_code": "awaiting_provider_check",
        "review_generation": 1,
        "idempotency_key": "hold-http-test"
    });
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!(
                    "/ops/billing/transfers/{transfer_id}/manual-review/decision"
                ))
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .expect("build hold request"),
        )
        .await
        .expect("hold response");
    assert_eq!(response.status(), StatusCode::OK);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/ops/billing/transfers/{transfer_id}/manual-review"
                ))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .expect("build updated detail request"),
        )
        .await
        .expect("updated review detail response");
    assert_eq!(response.status(), StatusCode::OK);
    let detail: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read updated detail"),
    )
    .expect("decode updated detail");
    assert_eq!(detail["decisions"].as_array().unwrap().len(), 1);
    assert_eq!(detail["decisions"][0]["action"], "hold");

    let body = serde_json::json!({
        "operator": "billing-operator-test",
        "action": "resume",
        "reason_code": "provider_checked",
        "review_generation": 1,
        "idempotency_key": "resume-http-test"
    });
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!(
                    "/ops/billing/transfers/{transfer_id}/manual-review/decision"
                ))
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .expect("build resume request"),
        )
        .await
        .expect("resume response");
    assert_eq!(response.status(), StatusCode::OK);
    let resumed: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read resume response"),
    )
    .expect("decode resume response");
    assert_eq!(
        resumed["transfer"]["execution_manual_review_required"],
        false
    );
    assert_eq!(resumed["decision"]["action"], "resume");

    let response = app
        .oneshot(
            Request::builder()
                .uri("/ops/billing/transfers/manual-review?limit=10")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .expect("build empty queue request"),
        )
        .await
        .expect("empty review queue response");
    assert_eq!(response.status(), StatusCode::OK);
    let queue: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read empty queue response"),
    )
    .expect("decode empty review queue");
    assert!(queue["transfers"]
        .as_array()
        .unwrap()
        .iter()
        .all(|transfer| transfer["transfer_id"] != transfer_id));
    cleanup(&pool, &fixture).await;
}

#[tokio::test]
async fn terminal_transfer_is_removed_from_manual_review_and_cannot_be_resumed() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let (fixture, transfer_id, _) = seed_manual_review(&pool).await;
    let mut tx = pool.begin().await.expect("begin terminal transition");
    let failed = mark_failed(&mut tx, &transfer_id, "destination_subscription_ended")
        .await
        .expect("a terminal provider event can end the parked transfer");
    assert_eq!(failed.state.as_str(), "failed");
    assert!(!failed.execution_manual_review_required);
    tx.commit().await.expect("commit terminal transfer state");

    assert!(load_manual_review_transfer(&pool, &transfer_id)
        .await
        .expect("load manual review details")
        .is_none());
    assert!(list_manual_review_transfers(&pool, None, 50)
        .await
        .expect("list manual review queue")
        .iter()
        .all(|transfer| transfer.transfer_id != transfer_id));
    assert!(matches!(
        record_operator_review_decision(
            &pool,
            &transfer_id,
            "billing-operator-test",
            TransferReviewAction::Resume,
            "provider_checked",
            1,
            "terminal-resume-test",
        )
        .await,
        Err(TransferError::InvalidTransition)
    ));

    cleanup(&pool, &fixture).await;
}
