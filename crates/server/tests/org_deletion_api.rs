//! Organisation-deletion HTTP contract tests.
//!
//! The handlers are driven through their test-only router so this suite proves the wire contract
//! without enabling the destructive routes in the production application.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use sqlx::postgres::PgConnectOptions;
use sqlx::PgPool;
use std::str::FromStr;
use tower::ServiceExt;
use uuid::Uuid;

use sotto_server::auth::session;
use sotto_server::db;
use sotto_server::state::AppState;

async fn pool_or_skip() -> Option<PgPool> {
    let Ok(database_url) = std::env::var("DATABASE_URL") else {
        return None;
    };
    if std::env::var("SOTTO_RUN_DB_TESTS").as_deref() != Ok("1") {
        return None;
    }
    let options = PgConnectOptions::from_str(&database_url).expect("parse DATABASE_URL");
    assert!(
        matches!(options.get_host(), "localhost" | "127.0.0.1" | "::1"),
        "refusing destructive deletion API tests against non-local host: {}",
        options.get_host()
    );
    let pool = db::connect(&database_url).await.expect("connect");
    db::migrate(&pool).await.expect("migrate");
    Some(pool)
}

fn state(pool: PgPool) -> AppState {
    AppState {
        telemetry_ingest: false,
        pool,
        oauth: None,
        oauth_config: None,
        billing: None,
    }
}

fn deletion_app(pool: PgPool) -> Router {
    Router::new()
        .merge(sotto_server::org_deletion_api::router())
        .with_state(state(pool))
}

async fn seed_owner(pool: &PgPool) -> (String, String, String) {
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("deletion-api-owner-{suffix}");
    let org_id = format!("deletion-api-org-{suffix}");
    sqlx::query("INSERT INTO users (id, oauth_provider, oauth_subject) VALUES ($1, 'test', $1)")
        .bind(&user_id)
        .execute(pool)
        .await
        .expect("insert owner");
    sqlx::query("INSERT INTO organizations (id, enc_name, created_by) VALUES ($1, $2, $3)")
        .bind(&org_id)
        .bind(b"opaque".as_slice())
        .bind(&user_id)
        .execute(pool)
        .await
        .expect("insert organisation");
    sqlx::query(
        "INSERT INTO organization_memberships (org_id, user_id, role) VALUES ($1, $2, 'owner')",
    )
    .bind(&org_id)
    .bind(&user_id)
    .execute(pool)
    .await
    .expect("insert owner membership");
    let token = session::issue(pool, &user_id).await.expect("issue session");
    (org_id, user_id, token)
}

async fn seed_user(pool: &PgPool, label: &str) -> (String, String) {
    let user_id = format!("deletion-api-{label}-{}", Uuid::new_v4().simple());
    sqlx::query("INSERT INTO users (id, oauth_provider, oauth_subject) VALUES ($1, 'test', $1)")
        .bind(&user_id)
        .execute(pool)
        .await
        .expect("insert user");
    let token = session::issue(pool, &user_id).await.expect("issue session");
    (user_id, token)
}

async fn cleanup(pool: &PgPool, org_id: &str, user_ids: &[&str]) {
    sqlx::query("DELETE FROM organization_deletions WHERE org_id = $1")
        .bind(org_id)
        .execute(pool)
        .await
        .expect("delete operation history");
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org_id)
        .execute(pool)
        .await
        .expect("delete organisation fixture");
    for user_id in user_ids {
        sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(user_id)
            .execute(pool)
            .await
            .expect("delete user fixture");
    }
}

async fn send(
    app: Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, String) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let request = match body {
        Some(body) => builder
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("request"),
        None => builder.body(Body::empty()).expect("request"),
    };
    let response = app.oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("response body");
    (status, String::from_utf8(bytes.to_vec()).expect("utf8"))
}

#[tokio::test]
async fn owner_can_request_deletion_with_the_documented_status_shape() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let (org_id, owner_id, token) = seed_owner(&pool).await;

    let (status, body) = send(
        deletion_app(pool.clone()),
        "POST",
        &format!("/orgs/{org_id}/deletion"),
        Some(&token),
        Some(json!({
            "confirm_org_id": org_id,
            "acknowledge_subscription_cancellation": true
        })),
    )
    .await;

    assert_eq!(status, StatusCode::ACCEPTED);
    let response: Value = serde_json::from_str(&body).expect("deletion status JSON");
    assert_eq!(response["state"], "requested");
    assert!(response["requested_at"].as_str().unwrap().ends_with('Z'));
    assert!(response["recoverable_until"]
        .as_str()
        .unwrap()
        .ends_with('Z'));
    assert_eq!(response["managed_backup_expiry_by"], Value::Null);
    assert_eq!(response["next_retry_at"], Value::Null);
    assert_eq!(response["error"], Value::Null);

    cleanup(&pool, &org_id, &[&owner_id]).await;
}

#[tokio::test]
async fn owner_can_read_the_current_deletion_status() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let (org_id, owner_id, token) = seed_owner(&pool).await;
    let uri = format!("/orgs/{org_id}/deletion");
    let (_, requested_body) = send(
        deletion_app(pool.clone()),
        "POST",
        &uri,
        Some(&token),
        Some(json!({
            "confirm_org_id": org_id,
            "acknowledge_subscription_cancellation": true
        })),
    )
    .await;

    let (status, status_body) =
        send(deletion_app(pool.clone()), "GET", &uri, Some(&token), None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(status_body, requested_body);

    cleanup(&pool, &org_id, &[&owner_id]).await;
}

#[tokio::test]
async fn repeated_deletion_request_returns_the_same_operation() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let (org_id, owner_id, token) = seed_owner(&pool).await;
    let uri = format!("/orgs/{org_id}/deletion");
    let body = json!({
        "confirm_org_id": org_id,
        "acknowledge_subscription_cancellation": true
    });

    let (first_status, first_body) = send(
        deletion_app(pool.clone()),
        "POST",
        &uri,
        Some(&token),
        Some(body.clone()),
    )
    .await;
    let (repeated_status, repeated_body) = send(
        deletion_app(pool.clone()),
        "POST",
        &uri,
        Some(&token),
        Some(body),
    )
    .await;

    assert_eq!(first_status, StatusCode::ACCEPTED);
    assert_eq!(repeated_status, StatusCode::ACCEPTED);
    assert_eq!(first_body, repeated_body);

    cleanup(&pool, &org_id, &[&owner_id]).await;
}

#[tokio::test]
async fn owner_can_cancel_deletion_idempotently() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let (org_id, owner_id, token) = seed_owner(&pool).await;
    let deletion_uri = format!("/orgs/{org_id}/deletion");
    send(
        deletion_app(pool.clone()),
        "POST",
        &deletion_uri,
        Some(&token),
        Some(json!({
            "confirm_org_id": org_id,
            "acknowledge_subscription_cancellation": true
        })),
    )
    .await;
    let cancel_uri = format!("{deletion_uri}/cancel");

    let (first_status, first_body) = send(
        deletion_app(pool.clone()),
        "POST",
        &cancel_uri,
        Some(&token),
        None,
    )
    .await;
    let (repeated_status, repeated_body) = send(
        deletion_app(pool.clone()),
        "POST",
        &cancel_uri,
        Some(&token),
        None,
    )
    .await;

    assert_eq!(first_status, StatusCode::ACCEPTED);
    assert_eq!(repeated_status, StatusCode::ACCEPTED);
    assert_eq!(first_body, repeated_body);
    let response: Value = serde_json::from_str(&first_body).expect("cancellation status JSON");
    assert_eq!(response["state"], "recovering");
    assert!(response["next_retry_at"].as_str().unwrap().ends_with('Z'));

    cleanup(&pool, &org_id, &[&owner_id]).await;
}

#[tokio::test]
async fn deletion_request_requires_both_explicit_confirmations() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let (org_id, owner_id, token) = seed_owner(&pool).await;
    let uri = format!("/orgs/{org_id}/deletion");
    let cases = [
        (
            json!({"acknowledge_subscription_cancellation": true}),
            "deletion confirmation is required",
        ),
        (
            json!({"confirm_org_id": org_id}),
            "subscription cancellation acknowledgement is required",
        ),
        (
            json!({
                "confirm_org_id": org_id,
                "acknowledge_subscription_cancellation": false
            }),
            "subscription cancellation acknowledgement is required",
        ),
        (
            json!({
                "confirm_org_id": "another-organisation",
                "acknowledge_subscription_cancellation": true
            }),
            "deletion confirmation does not match the organisation",
        ),
    ];

    for (body, expected) in cases {
        let (status, response) = send(
            deletion_app(pool.clone()),
            "POST",
            &uri,
            Some(&token),
            Some(body),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(response, expected);
    }
    let (status, _) = send(deletion_app(pool.clone()), "GET", &uri, Some(&token), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    cleanup(&pool, &org_id, &[&owner_id]).await;
}

#[tokio::test]
async fn deletion_routes_preserve_the_owner_access_policy() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let (org_id, owner_id, owner_token) = seed_owner(&pool).await;
    let (member_id, member_token) = seed_user(&pool, "member").await;
    let (outsider_id, outsider_token) = seed_user(&pool, "outsider").await;
    sqlx::query(
        "INSERT INTO organization_memberships (org_id, user_id, role) VALUES ($1, $2, 'member')",
    )
    .bind(&org_id)
    .bind(&member_id)
    .execute(&pool)
    .await
    .expect("insert member");
    let uri = format!("/orgs/{org_id}/deletion");
    send(
        deletion_app(pool.clone()),
        "POST",
        &uri,
        Some(&owner_token),
        Some(json!({
            "confirm_org_id": org_id,
            "acknowledge_subscription_cancellation": true
        })),
    )
    .await;

    let (unauthenticated, _) = send(deletion_app(pool.clone()), "GET", &uri, None, None).await;
    let (member, _) = send(
        deletion_app(pool.clone()),
        "GET",
        &uri,
        Some(&member_token),
        None,
    )
    .await;
    let (outsider, _) = send(
        deletion_app(pool.clone()),
        "GET",
        &uri,
        Some(&outsider_token),
        None,
    )
    .await;
    let (missing, _) = send(
        deletion_app(pool.clone()),
        "GET",
        "/orgs/missing-organisation/deletion",
        Some(&owner_token),
        None,
    )
    .await;
    let repeated_request = json!({
        "confirm_org_id": org_id,
        "acknowledge_subscription_cancellation": true
    });
    let (member_request, _) = send(
        deletion_app(pool.clone()),
        "POST",
        &uri,
        Some(&member_token),
        Some(repeated_request.clone()),
    )
    .await;
    let (outsider_request, _) = send(
        deletion_app(pool.clone()),
        "POST",
        &uri,
        Some(&outsider_token),
        Some(repeated_request),
    )
    .await;
    let cancel_uri = format!("{uri}/cancel");
    let (member_cancel, _) = send(
        deletion_app(pool.clone()),
        "POST",
        &cancel_uri,
        Some(&member_token),
        None,
    )
    .await;
    let (outsider_cancel, _) = send(
        deletion_app(pool.clone()),
        "POST",
        &cancel_uri,
        Some(&outsider_token),
        None,
    )
    .await;

    assert_eq!(unauthenticated, StatusCode::UNAUTHORIZED);
    assert_eq!(member, StatusCode::FORBIDDEN);
    assert_eq!(outsider, StatusCode::NOT_FOUND);
    assert_eq!(missing, StatusCode::NOT_FOUND);
    assert_eq!(member_request, StatusCode::FORBIDDEN);
    assert_eq!(outsider_request, StatusCode::NOT_FOUND);
    assert_eq!(member_cancel, StatusCode::FORBIDDEN);
    assert_eq!(outsider_cancel, StatusCode::NOT_FOUND);

    cleanup(&pool, &org_id, &[&owner_id, &member_id, &outsider_id]).await;
}

#[tokio::test]
async fn production_router_does_not_expose_deletion() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let (org_id, owner_id, token) = seed_owner(&pool).await;
    let (status, _) = send(
        sotto_server::app(state(pool.clone())),
        "POST",
        &format!("/orgs/{org_id}/deletion"),
        Some(&token),
        Some(json!({
            "confirm_org_id": org_id,
            "acknowledge_subscription_cancellation": true
        })),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);

    cleanup(&pool, &org_id, &[&owner_id]).await;
}

#[tokio::test]
async fn deletion_status_exposes_only_sanitised_failure_codes() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let (org_id, owner_id, token) = seed_owner(&pool).await;
    sqlx::query("UPDATE organizations SET stripe_subscription_id = $2 WHERE id = $1")
        .bind(&org_id)
        .bind("sub-deletion-api-unconfigured")
        .execute(&pool)
        .await
        .expect("link subscription fixture");
    let uri = format!("/orgs/{org_id}/deletion");
    send(
        deletion_app(pool.clone()),
        "POST",
        &uri,
        Some(&token),
        Some(json!({
            "confirm_org_id": org_id,
            "acknowledge_subscription_cancellation": true
        })),
    )
    .await;
    // Arrange the worker-owned failure directly so this HTTP test cannot claim another test's
    // globally due operation while the suite runs in parallel.
    sqlx::query(
        "UPDATE organization_deletions SET state = 'failed', \
         resume_state = 'cancelling_billing', last_error_code = 'billing_unavailable' \
         WHERE org_id = $1",
    )
    .bind(&org_id)
    .execute(&pool)
    .await
    .expect("arrange failed deletion");

    let cases = [
        ("billing_unavailable", "billing_unavailable"),
        ("billing_status_unknown", "billing_unknown"),
        ("purge_precondition_failed", "purge_failed"),
        (
            "provider_detail_that_must_not_escape",
            "billing_unavailable",
        ),
    ];
    for (stored_error, public_error) in cases {
        sqlx::query("UPDATE organization_deletions SET last_error_code = $2 WHERE org_id = $1")
            .bind(&org_id)
            .bind(stored_error)
            .execute(&pool)
            .await
            .expect("set failure code");
        let (status, body) =
            send(deletion_app(pool.clone()), "GET", &uri, Some(&token), None).await;

        assert_eq!(status, StatusCode::OK);
        let response: Value = serde_json::from_str(&body).expect("failed status JSON");
        assert_eq!(response["state"], "failed");
        assert_eq!(response["error"], public_error);
        assert!(!body.contains("provider_detail_that_must_not_escape"));
        assert!(!body.contains("sub-deletion-api-unconfigured"));
    }

    cleanup(&pool, &org_id, &[&owner_id]).await;
}

#[tokio::test]
async fn requester_can_read_a_completed_deletion_without_reusing_the_id() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let (org_id, owner_id, owner_token) = seed_owner(&pool).await;
    let (outsider_id, outsider_token) = seed_user(&pool, "terminal-outsider").await;
    let uri = format!("/orgs/{org_id}/deletion");
    send(
        deletion_app(pool.clone()),
        "POST",
        &uri,
        Some(&owner_token),
        Some(json!({
            "confirm_org_id": org_id,
            "acknowledge_subscription_cancellation": true
        })),
    )
    .await;
    let mut tx = pool.begin().await.expect("begin tombstone fixture");
    sqlx::query(
        "UPDATE organization_deletions SET state = 'completed', completed_at = now() \
         WHERE org_id = $1",
    )
    .bind(&org_id)
    .execute(&mut *tx)
    .await
    .expect("complete operation fixture");
    sqlx::query("DELETE FROM organization_memberships WHERE org_id = $1")
        .bind(&org_id)
        .execute(&mut *tx)
        .await
        .expect("purge memberships fixture");
    sqlx::query(
        "UPDATE organizations SET lifecycle_state = 'deleted', deleted_at = now(), \
         enc_name = NULL, created_by = NULL, tier = 'free', trial_ends_at = NULL, \
         stripe_customer_id = NULL, stripe_subscription_id = NULL WHERE id = $1",
    )
    .bind(&org_id)
    .execute(&mut *tx)
    .await
    .expect("create tombstone fixture");
    tx.commit().await.expect("commit tombstone fixture");

    let (requester_status, requester_body) = send(
        deletion_app(pool.clone()),
        "GET",
        &uri,
        Some(&owner_token),
        None,
    )
    .await;
    let (outsider_status, _) = send(
        deletion_app(pool.clone()),
        "GET",
        &uri,
        Some(&outsider_token),
        None,
    )
    .await;
    let (repeated_status, _) = send(
        deletion_app(pool.clone()),
        "POST",
        &uri,
        Some(&owner_token),
        Some(json!({
            "confirm_org_id": org_id,
            "acknowledge_subscription_cancellation": true
        })),
    )
    .await;

    assert_eq!(requester_status, StatusCode::OK);
    let response: Value = serde_json::from_str(&requester_body).expect("completed status JSON");
    assert_eq!(response["state"], "completed");
    assert_eq!(outsider_status, StatusCode::NOT_FOUND);
    assert_eq!(repeated_status, StatusCode::CONFLICT);

    cleanup(&pool, &org_id, &[&owner_id, &outsider_id]).await;
}

#[tokio::test]
async fn cancellation_is_rejected_after_purge_starts() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let (org_id, owner_id, token) = seed_owner(&pool).await;
    let deletion_uri = format!("/orgs/{org_id}/deletion");
    send(
        deletion_app(pool.clone()),
        "POST",
        &deletion_uri,
        Some(&token),
        Some(json!({
            "confirm_org_id": org_id,
            "acknowledge_subscription_cancellation": true
        })),
    )
    .await;
    // Purging is worker-owned, so arrange the phase without claiming the suite's global queue.
    sqlx::query("UPDATE organization_deletions SET state = 'purging' WHERE org_id = $1")
        .bind(&org_id)
        .execute(&pool)
        .await
        .expect("arrange purging deletion");

    let (status, body) = send(
        deletion_app(pool.clone()),
        "POST",
        &format!("{deletion_uri}/cancel"),
        Some(&token),
        None,
    )
    .await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        body,
        "organisation deletion cannot be cancelled after purge has started"
    );

    cleanup(&pool, &org_id, &[&owner_id]).await;
}

#[tokio::test]
async fn completed_recovery_keeps_cancellation_idempotent() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let (org_id, owner_id, token) = seed_owner(&pool).await;
    let deletion_uri = format!("/orgs/{org_id}/deletion");
    send(
        deletion_app(pool.clone()),
        "POST",
        &deletion_uri,
        Some(&token),
        Some(json!({
            "confirm_org_id": org_id,
            "acknowledge_subscription_cancellation": true
        })),
    )
    .await;
    let cancel_uri = format!("{deletion_uri}/cancel");
    send(
        deletion_app(pool.clone()),
        "POST",
        &cancel_uri,
        Some(&token),
        None,
    )
    .await;
    let mut tx = pool.begin().await.expect("begin recovery fixture");
    // Recovery completion is worker-owned; arranging it directly keeps this contract test away
    // from the shared due-work queue used by the lifecycle suite.
    sqlx::query(
        "UPDATE organization_deletions SET state = 'cancelled', cancelled_at = now(), \
         next_attempt_at = NULL WHERE org_id = $1",
    )
    .bind(&org_id)
    .execute(&mut *tx)
    .await
    .expect("complete recovery fixture");
    sqlx::query("UPDATE organizations SET lifecycle_state = 'active' WHERE id = $1")
        .bind(&org_id)
        .execute(&mut *tx)
        .await
        .expect("restore organisation fixture");
    tx.commit().await.expect("commit recovery fixture");

    let (status, body) = send(
        deletion_app(pool.clone()),
        "POST",
        &cancel_uri,
        Some(&token),
        None,
    )
    .await;

    assert_eq!(status, StatusCode::ACCEPTED);
    let response: Value = serde_json::from_str(&body).expect("cancelled status JSON");
    assert_eq!(response["state"], "cancelled");

    cleanup(&pool, &org_id, &[&owner_id]).await;
}
