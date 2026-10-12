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
    sotto_server::app(AppState {
        pool,
        deployment_mode,
        oauth: None,
        oauth_config: None,
        billing: None,
        telemetry_ingest: false,
        organisation_deletion_enabled: false,
        organisation_deletion_retention_days: DEFAULT_ORGANISATION_DELETION_RETENTION_DAYS,
        organisation_deletion_metrics_token: None,
        organisation_deletion_operator_token: None,
        cloud_action_enforcement_enabled: enforcement_enabled,
        machine_eligibility_enforcement_enabled: false,
    })
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
