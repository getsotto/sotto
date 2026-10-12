//! Account reset (M5 PR6a): the recovery path for a user who lost their Emergency Kit.
//!
//! `POST /account/reset` replaces the account's crypto material and deletes the user's now-dead
//! environment grants in one transaction. DB-gated like the other server tests.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use sqlx::postgres::PgConnectOptions;
use sqlx::{PgPool, Row};
use std::str::FromStr;
use tower::ServiceExt;
use uuid::Uuid;

use sotto_server::auth::session;
use sotto_server::cloud_coverage::ConfirmedPaidInterval;
use sotto_server::cloud_coverage_reconciliation::{
    begin_collection, finish_collection, register_source, SourceBinding, SourceObservation,
};
use sotto_server::cloud_coverage_store::PublicationOutcome;
use sotto_server::config::DEFAULT_ORGANISATION_DELETION_RETENTION_DAYS;
use sotto_server::db;
use sotto_server::state::AppState;

async fn pool_or_skip() -> Option<PgPool> {
    if std::env::var("SOTTO_RUN_DB_TESTS").as_deref() != Ok("1") {
        return None;
    }
    let url =
        std::env::var("DATABASE_URL").expect("DATABASE_URL is required when SOTTO_RUN_DB_TESTS=1");
    let options = PgConnectOptions::from_str(&url).expect("parse DATABASE_URL");
    assert!(
        matches!(options.get_host(), "localhost" | "127.0.0.1" | "::1"),
        "refusing destructive recovery tests against non-local host: {}",
        options.get_host()
    );
    let pool = db::connect(&url).await.expect("connect");
    db::migrate(&pool).await.expect("migrate");
    Some(pool)
}

async fn coverage_snapshot(pool: &PgPool, beneficiary_id: &str) -> Vec<Vec<serde_json::Value>> {
    let queries = [
        "SELECT to_jsonb(t) AS value FROM (SELECT * FROM cloud_coverage_coordinators WHERE beneficiary_id = $1) t",
        "SELECT to_jsonb(t) AS value FROM (SELECT * FROM cloud_coverage_sources WHERE beneficiary_id = $1 ORDER BY source_id) t",
        "SELECT to_jsonb(t) AS value FROM (SELECT * FROM cloud_coverage_collection_attempts WHERE beneficiary_id = $1 ORDER BY attempt_id) t",
        "SELECT to_jsonb(t) AS value FROM (SELECT * FROM cloud_coverage_revisions WHERE beneficiary_id = $1 ORDER BY revision) t",
        "SELECT to_jsonb(t) AS value FROM (SELECT * FROM cloud_coverage_revision_facts WHERE beneficiary_id = $1 ORDER BY revision, coverage_id) t",
        "SELECT to_jsonb(t) AS value FROM (SELECT * FROM cloud_coverage_heads WHERE beneficiary_id = $1) t",
    ];
    let mut snapshots = Vec::with_capacity(queries.len());
    for query in queries {
        snapshots.push(
            sqlx::query(query)
                .bind(beneficiary_id)
                .fetch_all(pool)
                .await
                .expect("read coverage snapshot")
                .into_iter()
                .map(|row| row.try_get("value").expect("decode coverage snapshot"))
                .collect(),
        );
    }
    snapshots
}

async fn cleanup_coverage(pool: &PgPool, beneficiary_id: &str) {
    sqlx::query("DELETE FROM cloud_coverage_heads WHERE beneficiary_id = $1")
        .bind(beneficiary_id)
        .execute(pool)
        .await
        .expect("delete coverage head");
    sqlx::query("DELETE FROM cloud_coverage_revision_facts WHERE beneficiary_id = $1")
        .bind(beneficiary_id)
        .execute(pool)
        .await
        .expect("delete coverage facts");
    sqlx::query(
        "UPDATE cloud_coverage_coordinators SET current_attempt_id = NULL WHERE beneficiary_id = $1",
    )
    .bind(beneficiary_id)
    .execute(pool)
    .await
    .expect("clear coverage current attempt");
    for table in [
        "cloud_coverage_collection_attempts",
        "cloud_coverage_sources",
        "cloud_coverage_revisions",
        "cloud_coverage_coordinators",
    ] {
        sqlx::query(&format!("DELETE FROM {table} WHERE beneficiary_id = $1"))
            .bind(beneficiary_id)
            .execute(pool)
            .await
            .expect("delete coverage fixture");
    }
}

fn app(pool: PgPool) -> Router {
    let state = AppState {
        deployment_mode: sotto_server::config::DeploymentMode::SelfHosted,
        telemetry_ingest: false,
        cloud_action_enforcement_enabled: false,
        machine_eligibility_enforcement_enabled: false,
        pool,
        oauth: None,
        oauth_config: None,
        billing: None,
        organisation_deletion_enabled: false,
        organisation_deletion_retention_days: DEFAULT_ORGANISATION_DELETION_RETENTION_DAYS,
        organisation_deletion_metrics_token: None,
        organisation_deletion_operator_token: None,
    };
    Router::new()
        .merge(sotto_server::account::router())
        .merge(sotto_server::audit::router())
        .merge(sotto_server::org::router())
        .merge(sotto_server::sync::router())
        .with_state(state)
}

async fn fresh_session(pool: &PgPool, user_id: &str, subject: &str) -> String {
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("pre-clean");
    sqlx::query("INSERT INTO users (id, oauth_provider, oauth_subject) VALUES ($1, 'github', $2)")
        .bind(user_id)
        .bind(subject)
        .execute(pool)
        .await
        .expect("insert user");
    session::issue(pool, user_id).await.expect("issue")
}

async fn body_text(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body");
    String::from_utf8(bytes.to_vec()).expect("utf8")
}

async fn send(
    pool: &PgPool,
    method: &str,
    uri: &str,
    token: &str,
    body: Option<String>,
) -> (StatusCode, String) {
    let builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"));
    let req = match body {
        Some(b) => builder
            .header("content-type", "application/json")
            .body(Body::from(b))
            .expect("req"),
        None => builder.body(Body::empty()).expect("req"),
    };
    let resp = app(pool.clone()).oneshot(req).await.expect("oneshot");
    let status = resp.status();
    (status, body_text(resp).await)
}

fn b64(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

fn bundle_body(tag: &str) -> String {
    format!(
        r#"{{"public_key":"{}","enc_private_keys":"{}","kdf_params":"{}","recovery_blob":"{}"}}"#,
        b64(&[0xCD; 32]),
        b64(format!("{tag}-priv").as_bytes()),
        b64(format!("{tag}-kdf").as_bytes()),
        b64(format!("{tag}-rec").as_bytes()),
    )
}

#[tokio::test]
async fn reset_replaces_material_and_deletes_grants() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: SOTTO_RUN_DB_TESTS=1 and DATABASE_URL required");
        return;
    };
    sqlx::query("DELETE FROM organizations WHERE id = 'rec-o'")
        .execute(&pool)
        .await
        .unwrap();
    let owner = fresh_session(&pool, "rec-owner", "rec-owner-s").await;
    let member = fresh_session(&pool, "rec-member", "rec-member-s").await;

    // Both users initialise accounts; the owner stands up an org env and grants the member.
    for (token, tag) in [(&owner, "own"), (&member, "old")] {
        let (status, body) = send(&pool, "PUT", "/account", token, Some(bundle_body(tag))).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    // Assert each setup step succeeds, so a later auth/validation/schema change surfaces here with a
    // clear status + body rather than as a confusing failure in the reset assertions below.
    let expect_ok = |label: &str, (status, body): (StatusCode, String)| {
        assert!(status.is_success(), "{label} failed: {status} - {body}");
    };
    expect_ok(
        "create org",
        send(
            &pool,
            "POST",
            "/orgs",
            &owner,
            Some(format!(
                r#"{{"id":"rec-o","enc_name":"{}","enc_org_key":"{}"}}"#,
                b64(b"org"),
                b64(b"owner-org-key"),
            )),
        )
        .await,
    );
    expect_ok(
        "add member",
        send(
            &pool,
            "POST",
            "/orgs/rec-o/members",
            &owner,
            Some(r#"{"user_id":"rec-member","role":"member"}"#.into()),
        )
        .await,
    );
    // Grant the member an org-key copy too, so the reset has one to clear.
    expect_ok(
        "grant org key",
        send(
            &pool,
            "POST",
            "/orgs/rec-o/members/rec-member/org-key",
            &owner,
            Some(format!(r#"{{"enc_org_key":"{}"}}"#, b64(b"member-org-key"))),
        )
        .await,
    );
    expect_ok(
        "create project",
        send(
            &pool,
            "POST",
            "/projects",
            &owner,
            Some(format!(
                r#"{{"id":"rec-p","enc_name":"{}","org_id":"rec-o"}}"#,
                b64(b"p")
            )),
        )
        .await,
    );
    expect_ok(
        "create environment",
        send(
            &pool,
            "POST",
            "/projects/rec-p/environments",
            &owner,
            Some(format!(
                r#"{{"id":"rec-e","enc_name":"{}","enc_vault_key":"{}"}}"#,
                b64(b"e"),
                b64(b"vk")
            )),
        )
        .await,
    );
    expect_ok(
        "create grant",
        send(
            &pool,
            "POST",
            "/environments/rec-e/grants",
            &owner,
            Some(format!(
                r#"{{"user_id":"rec-member","enc_vault_key":"{}"}}"#,
                b64(b"member-grant")
            )),
        )
        .await,
    );
    assert_eq!(
        send(&pool, "GET", "/environments/rec-e/grant", &member, None)
            .await
            .0,
        StatusCode::OK
    );

    // Reset: the material is replaced and the member's grant is gone.
    let (status, body) = send(
        &pool,
        "POST",
        "/account/reset",
        &member,
        Some(bundle_body("new")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = send(&pool, "GET", "/account", &member, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(&b64(b"new-priv")), "material was replaced");
    assert!(!body.contains(&b64(b"old-priv")));
    assert_eq!(
        send(&pool, "GET", "/environments/rec-e/grant", &member, None)
            .await
            .0,
        StatusCode::NOT_FOUND,
        "dead grants are deleted by the reset"
    );
    // Membership itself survives - the admin re-grants rather than re-invites.
    assert_eq!(
        send(&pool, "GET", "/orgs/rec-o/members", &member, None)
            .await
            .0,
        StatusCode::OK
    );
    // The org-key copy was sealed to the dead keypair: cleared by the reset (names fall back to
    // ids until an admin re-grants it).
    let (status, body) = send(&pool, "GET", "/orgs", &member, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !body.contains(&b64(b"member-org-key")),
        "reset must clear the member's org-key copy"
    );
    // The reset surfaced in the org's audit log, so admins know to re-grant.
    let (status, body) = send(&pool, "GET", "/orgs/rec-o/audit", &owner, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("account.reset"),
        "reset must be audited: {body}"
    );
}

#[tokio::test]
async fn reset_requires_an_initialized_account() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let user = fresh_session(&pool, "rec-uninit", "rec-uninit-s").await;
    assert_eq!(
        send(
            &pool,
            "POST",
            "/account/reset",
            &user,
            Some(bundle_body("x"))
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn reset_preserves_cloud_coverage_evidence_and_ticket_lifecycle() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: SOTTO_RUN_DB_TESTS=1 and DATABASE_URL required");
        return;
    };
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("rec-coverage-{suffix}");
    let token = fresh_session(&pool, &user_id, &format!("{user_id}-subject")).await;
    let (status, body) = send(&pool, "PUT", "/account", &token, Some(bundle_body("old"))).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let org_id = format!("rec-coverage-org-{suffix}");
    let project_id = format!("rec-coverage-project-{suffix}");
    let environment_id = format!("rec-coverage-environment-{suffix}");
    let (status, body) = send(
        &pool,
        "POST",
        "/orgs",
        &token,
        Some(format!(
            r#"{{"id":"{org_id}","enc_name":"{}","enc_org_key":"{}"}}"#,
            b64(b"coverage-org"),
            b64(b"coverage-org-key")
        )),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = send(
        &pool,
        "POST",
        "/projects",
        &token,
        Some(format!(
            r#"{{"id":"{project_id}","enc_name":"{}","org_id":"{org_id}"}}"#,
            b64(b"coverage-project")
        )),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = send(
        &pool,
        "POST",
        &format!("/projects/{project_id}/environments"),
        &token,
        Some(format!(
            r#"{{"id":"{environment_id}","enc_name":"{}","enc_vault_key":"{}"}}"#,
            b64(b"coverage-environment"),
            b64(b"coverage-vault-key")
        )),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = send(
        &pool,
        "POST",
        &format!("/environments/{environment_id}/grants"),
        &token,
        Some(format!(
            r#"{{"user_id":"{user_id}","enc_vault_key":"{}"}}"#,
            b64(b"coverage-grant")
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let source = SourceBinding {
        beneficiary_id: user_id.clone(),
        source_id: format!("{user_id}:source"),
        provider_namespace: "recovery-test".into(),
        external_allocation_reference: format!("allocation-{suffix}"),
        ownership_evidence_reference: format!("ownership-{suffix}"),
    };
    let mut tx = pool.begin().await.expect("begin coverage registration");
    register_source(&mut tx, "recovery-registration", &source)
        .await
        .expect("register coverage source");
    tx.commit().await.expect("commit coverage registration");

    let mut tx = pool.begin().await.expect("begin completed coverage");
    let completed_ticket = begin_collection(&mut tx, &user_id, "recovery-completed")
        .await
        .expect("begin completed coverage");
    tx.commit().await.expect("commit completed begin");
    let completed_observation = SourceObservation::Complete {
        source_id: source.source_id.clone(),
        evidence_reference: "source-evidence".into(),
        paid_intervals: vec![ConfirmedPaidInterval {
            coverage_id: "recovery-coverage".into(),
            source_id: source.source_id.clone(),
            starts_at: 10,
            paid_until: 20,
            failed_renewal_id: None,
        }],
    };
    let mut tx = pool.begin().await.expect("begin completed finish");
    finish_collection(
        &mut tx,
        &completed_ticket,
        "aggregate-evidence",
        std::slice::from_ref(&completed_observation),
    )
    .await
    .expect("finish completed coverage");
    tx.commit().await.expect("commit completed coverage");

    let mut tx = pool.begin().await.expect("begin pending coverage");
    let pending_ticket = begin_collection(&mut tx, &user_id, "recovery-pending")
        .await
        .expect("begin pending coverage");
    tx.commit().await.expect("commit pending begin");
    let before_reset = coverage_snapshot(&pool, &user_id).await;

    let (status, body) = send(
        &pool,
        "POST",
        "/account/reset",
        &token,
        Some(bundle_body("new")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(coverage_snapshot(&pool, &user_id).await, before_reset);
    let (status, body) = send(&pool, "GET", "/account", &token, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains(&b64(b"new-priv")),
        "reset must replace key material"
    );
    assert!(!body.contains(&b64(b"old-priv")));
    assert_eq!(
        send(
            &pool,
            "GET",
            &format!("/environments/{environment_id}/grant"),
            &token,
            None,
        )
        .await
        .0,
        StatusCode::NOT_FOUND,
        "reset must remove the grant before coverage replay"
    );

    let mut tx = pool
        .begin()
        .await
        .expect("begin completed replay after reset");
    let replay = finish_collection(
        &mut tx,
        &completed_ticket,
        "aggregate-evidence",
        std::slice::from_ref(&completed_observation),
    )
    .await
    .expect("replay completed coverage after reset");
    tx.commit()
        .await
        .expect("commit completed replay after reset");
    assert_eq!(replay.outcome, PublicationOutcome::AlreadyApplied);
    assert_eq!(replay.revision, 2);
    assert_eq!(
        send(
            &pool,
            "GET",
            &format!("/environments/{environment_id}/grant"),
            &token,
            None,
        )
        .await
        .0,
        StatusCode::NOT_FOUND,
        "coverage replay must not restore a deleted grant"
    );

    let mut tx = pool
        .begin()
        .await
        .expect("begin pending finish after reset");
    finish_collection(
        &mut tx,
        &pending_ticket,
        "aggregate-evidence-after-reset",
        &[SourceObservation::Unavailable {
            source_id: source.source_id.clone(),
            evidence_reference: "source-evidence-after-reset".into(),
            reason: sotto_server::cloud_coverage_store::UnavailableReason::NeedsReconciliation,
        }],
    )
    .await
    .expect("finish pending coverage after reset");
    tx.commit()
        .await
        .expect("commit pending finish after reset");
    assert_eq!(
        send(
            &pool,
            "GET",
            &format!("/environments/{environment_id}/grant"),
            &token,
            None,
        )
        .await
        .0,
        StatusCode::NOT_FOUND,
        "coverage completion must not restore a deleted grant"
    );

    cleanup_coverage(&pool, &user_id).await;
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(&org_id)
        .execute(&pool)
        .await
        .expect("delete recovery coverage organisation");
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(&user_id)
        .execute(&pool)
        .await
        .expect("delete recovery coverage user");
}
