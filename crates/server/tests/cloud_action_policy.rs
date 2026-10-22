//! Cloud human-action middleware wiring.
//!
//! These checks use the real application router and a disposable local Postgres database. They
//! stay gated so ordinary workspace tests remain DB-free, while proving the rollout switch is
//! actually connected to request handling.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use sotto_server::auth::session;
use sotto_server::config::{DeploymentMode, DEFAULT_ORGANISATION_DELETION_RETENTION_DAYS};
use sotto_server::db;
use sotto_server::state::AppState;
use sqlx::postgres::PgConnectOptions;
use sqlx::PgPool;
use std::str::FromStr;
use tower::ServiceExt;

async fn pool_or_skip() -> Option<PgPool> {
    if std::env::var("SOTTO_RUN_DB_TESTS").as_deref() != Ok("1") {
        eprintln!("skipping: SOTTO_RUN_DB_TESTS=1 not set");
        return None;
    }
    let database_url = std::env::var("DATABASE_URL").ok()?;
    let options = PgConnectOptions::from_str(&database_url).expect("parse DATABASE_URL");
    assert!(
        matches!(options.get_host(), "localhost" | "127.0.0.1" | "::1"),
        "refusing to run destructive DB tests against non-local host: {}",
        options.get_host()
    );
    let pool = db::connect(&database_url).await.expect("connect");
    db::migrate(&pool).await.expect("migrate");
    Some(pool)
}

fn app(pool: PgPool, deployment_mode: DeploymentMode, enforcement_enabled: bool) -> axum::Router {
    app_with_authority_enforcement(pool, deployment_mode, enforcement_enabled, false)
}

fn app_with_authority_enforcement(
    pool: PgPool,
    deployment_mode: DeploymentMode,
    enforcement_enabled: bool,
    authority_enforcement_enabled: bool,
) -> axum::Router {
    sotto_server::app(AppState {
        pool,
        deployment_mode,
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
        billing_transfer_operator_token: None,
        cloud_action_enforcement_enabled: enforcement_enabled,
        cloud_coverage_authority_enforcement_enabled: authority_enforcement_enabled,
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

#[tokio::test]
async fn missing_coverage_authority_keeps_new_cloud_accounts_free() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let user_id = format!("new-free-user-{}", uuid::Uuid::new_v4());
    sqlx::query("INSERT INTO users (id, oauth_provider, oauth_subject) VALUES ($1, 'github', $1)")
        .bind(&user_id)
        .execute(&pool)
        .await
        .expect("insert free user without a coverage projection");
    let token = session::issue(&pool, &user_id)
        .await
        .expect("issue session");
    let response = app_with_authority_enforcement(pool.clone(), DeploymentMode::Cloud, false, true)
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/account/eligibility")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .expect("eligibility request"),
        )
        .await
        .expect("eligibility response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("eligibility body");
    let value: serde_json::Value = serde_json::from_slice(&body).expect("eligibility JSON");
    assert_eq!(value["state"], "free");
    sqlx::query("DELETE FROM users WHERE id=$1")
        .bind(&user_id)
        .execute(&pool)
        .await
        .expect("clean up free user");
}

async fn body_text(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    String::from_utf8(bytes.to_vec()).expect("utf8")
}

async fn request(
    pool: PgPool,
    mode: DeploymentMode,
    enforcement: bool,
    token: &str,
) -> (StatusCode, String) {
    let response = app(pool, mode, enforcement)
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/projects")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("oneshot");
    let status = response.status();
    (status, body_text(response).await)
}

#[tokio::test]
async fn cloud_action_enforcement_is_opt_in_and_self_hosted_safe() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let user_id = "test-cloud-action-policy-user";
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("pre-clean");
    sqlx::query("INSERT INTO users (id, oauth_provider, oauth_subject) VALUES ($1, 'github', $2)")
        .bind(user_id)
        .bind("test-cloud-action-policy-subject")
        .execute(&pool)
        .await
        .expect("insert user");
    let token = session::issue(&pool, user_id).await.expect("issue session");

    let (status, body) = request(pool.clone(), DeploymentMode::Cloud, true, &token).await;
    assert_eq!(status, StatusCode::PAYMENT_REQUIRED);
    assert!(body.contains("hosted eligibility is required"));

    let (status, body) = request(pool.clone(), DeploymentMode::Cloud, false, &token).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "[]");

    let (status, body) = request(pool.clone(), DeploymentMode::SelfHosted, true, &token).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "[]");

    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("cleanup");
}
