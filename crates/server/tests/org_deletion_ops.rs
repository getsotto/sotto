//! Protected operator-observation endpoint tests.
//!
//! Authentication cases use an unreachable pool so invalid callers are rejected before any
//! database operation. The successful path remains DB-gated with the lifecycle integration suite.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::Value;
use sqlx::{PgPool, Postgres, Transaction};
use tower::ServiceExt;
use uuid::Uuid;

use sotto_server::config::DEFAULT_ORGANISATION_DELETION_RETENTION_DAYS;
use sotto_server::db;
use sotto_server::org_deletion::request_with_retention;
use sotto_server::state::AppState;

fn app(pool: PgPool, token: Option<&str>) -> Router {
    let state = AppState {
        pool,
        oauth: None,
        oauth_config: None,
        billing: None,
        telemetry_ingest: false,
        organisation_deletion_enabled: false,
        organisation_deletion_retention_days: DEFAULT_ORGANISATION_DELETION_RETENTION_DAYS,
        organisation_deletion_metrics_token: None,
        organisation_deletion_operator_token: token.map(str::to_owned),
    };
    Router::new()
        .merge(sotto_server::org_deletion_ops::router())
        .with_state(state)
}

async fn post_observation(
    app: &Router,
    token: Option<&str>,
    org_id: &str,
    body: &str,
) -> (StatusCode, String) {
    let mut request = Request::builder()
        .method("POST")
        .uri(format!(
            "/ops/organisation-deletion/{org_id}/billing-observation"
        ))
        .header("content-type", "application/json");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from(body.to_owned())).expect("request"))
        .await
        .expect("response");
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("response body");
    (status, String::from_utf8(body.to_vec()).expect("utf8"))
}

#[tokio::test]
async fn operator_endpoint_is_dark_without_a_token() {
    let pool = PgPool::connect_lazy("postgres://127.0.0.1:1/sotto").expect("lazy pool");
    assert_eq!(
        post_observation(
            &app(pool, None),
            None,
            "example",
            r#"{"operator":"ops","subscription_id":"sub","observed_status":"canceled","observed_at":"now","reason":"rotation","evidence":"ticket-1"}"#,
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[tokio::test]
async fn operator_endpoint_rejects_missing_and_wrong_tokens() {
    let pool = PgPool::connect_lazy("postgres://127.0.0.1:1/sotto").expect("lazy pool");
    let app = app(pool, Some("operator-secret"));
    let body = r#"{"operator":"ops","subscription_id":"sub","observed_status":"canceled","observed_at":"now","reason":"rotation","evidence":"ticket-1"}"#;

    assert_eq!(
        post_observation(&app, None, "example", body).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        post_observation(&app, Some("wrong-secret"), "example", body)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn operator_endpoint_rejects_malformed_json_after_authentication() {
    let pool = PgPool::connect_lazy("postgres://127.0.0.1:1/sotto").expect("lazy pool");
    assert_eq!(
        post_observation(
            &app(pool, Some("operator-secret")),
            Some("operator-secret"),
            "example",
            "not-json"
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
}

async fn pool_or_skip() -> Option<PgPool> {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return None;
    };
    if std::env::var("SOTTO_RUN_DB_TESTS").as_deref() != Ok("1") {
        return None;
    }
    let pool = db::connect(&url).await.expect("connect");
    db::migrate(&pool).await.expect("migrate");
    Some(pool)
}

async fn prepare_test(pool: &PgPool) -> Transaction<'static, Postgres> {
    let mut tx = pool.begin().await.expect("begin test lock");
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('sotto organisation deletion tests'))")
        .execute(&mut *tx)
        .await
        .expect("lock deletion tests");
    // The lifecycle suites share one queue, so clean only this test's prefixes while the advisory
    // lock prevents another deletion test from claiming an abandoned fixture at the same time.
    sqlx::query("DELETE FROM organization_deletions WHERE org_id LIKE 'deletion-ops-org-%'")
        .execute(pool)
        .await
        .expect("delete abandoned operations");
    sqlx::query("DELETE FROM organizations WHERE id LIKE 'deletion-ops-org-%'")
        .execute(pool)
        .await
        .expect("delete abandoned organisations");
    sqlx::query("DELETE FROM users WHERE id LIKE 'deletion-ops-owner-%'")
        .execute(pool)
        .await
        .expect("delete abandoned owners");
    tx
}

#[tokio::test]
async fn operator_endpoint_records_audited_observation() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let _test_lock = prepare_test(&pool).await;
    let suffix = Uuid::new_v4().simple().to_string();
    let org_id = format!("deletion-ops-org-{suffix}");
    let owner_id = format!("deletion-ops-owner-{suffix}");
    sqlx::query("INSERT INTO users (id, oauth_provider, oauth_subject) VALUES ($1, 'test', $1)")
        .bind(&owner_id)
        .execute(&pool)
        .await
        .expect("insert owner");
    sqlx::query(
        "INSERT INTO organizations (id, enc_name, created_by, stripe_subscription_id) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(&org_id)
    .bind(b"opaque".as_slice())
    .bind(&owner_id)
    .bind("sub-operator")
    .execute(&pool)
    .await
    .expect("insert organisation");
    sqlx::query(
        "INSERT INTO organization_memberships (org_id, user_id, role) VALUES ($1, $2, 'owner')",
    )
    .bind(&org_id)
    .bind(&owner_id)
    .execute(&pool)
    .await
    .expect("insert owner membership");
    request_with_retention(
        &pool,
        &org_id,
        &owner_id,
        &org_id,
        DEFAULT_ORGANISATION_DELETION_RETENTION_DAYS,
    )
    .await
    .expect("request deletion");

    let observed_at: String = sqlx::query_scalar(
        "SELECT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"')",
    )
    .fetch_one(&pool)
    .await
    .expect("format observation timestamp");
    let body = format!(
        r#"{{"operator":"on-call","subscription_id":"sub-operator","observed_status":"canceled","observed_at":"{observed_at}","reason":"provider unavailable","evidence":"ticket-123","managed_backup_expiry_by":"2099-01-01T00:00:00Z"}}"#
    );
    let (status, body) = post_observation(
        &app(pool.clone(), Some("operator-secret")),
        Some("operator-secret"),
        &org_id,
        &body,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let response: Value = serde_json::from_str(&body).expect("status JSON");
    assert_eq!(response["state"], "retention");
    assert_eq!(response["error"], Value::Null);
    assert!(response["managed_backup_expiry_by"].as_str().is_some());
    let action: String = sqlx::query_scalar(
        "SELECT action FROM audit_events WHERE org_id = $1 ORDER BY id DESC LIMIT 1",
    )
    .bind(&org_id)
    .fetch_one(&pool)
    .await
    .expect("read observation audit");
    assert_eq!(action, "org.deletion.billing_observed");

    sqlx::query("DELETE FROM organization_deletions WHERE org_id = $1")
        .bind(&org_id)
        .execute(&pool)
        .await
        .expect("delete operation");
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(&org_id)
        .execute(&pool)
        .await
        .expect("delete organisation");
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(&owner_id)
        .execute(&pool)
        .await
        .expect("delete owner");
}
