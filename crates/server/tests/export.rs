//! Cloud exit export integration tests.
//!
//! These tests use the real application router and a disposable local Postgres database. They are
//! gated with `SOTTO_RUN_DB_TESTS=1` like the other database-backed integration tests.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde_json::Value;
use sotto_server::auth::session;
use sotto_server::config::DeploymentMode;
use sotto_server::db;
use sotto_server::state::AppState;
use sqlx::postgres::PgConnectOptions;
use sqlx::PgPool;
use std::str::FromStr;
use tower::ServiceExt;

async fn pool_or_skip() -> Option<PgPool> {
    if std::env::var("SOTTO_RUN_DB_TESTS").as_deref() != Ok("1") {
        eprintln!("skipping export tests: SOTTO_RUN_DB_TESTS=1 not set");
        return None;
    }
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL is required");
    let options = PgConnectOptions::from_str(&url).expect("parse DATABASE_URL");
    assert!(
        matches!(options.get_host(), "localhost" | "127.0.0.1" | "::1"),
        "refusing to run destructive DB tests against non-local host: {}",
        options.get_host()
    );
    let pool = db::connect(&url).await.expect("connect");
    db::migrate(&pool).await.expect("migrate");
    Some(pool)
}

fn app(pool: PgPool) -> axum::Router {
    sotto_server::app(AppState {
        pool,
        deployment_mode: DeploymentMode::SelfHosted,
        oauth: None,
        oauth_config: None,
        billing: None,
        telemetry_ingest: false,
        organisation_deletion_enabled: false,
        organisation_deletion_retention_days:
            sotto_server::config::DEFAULT_ORGANISATION_DELETION_RETENTION_DAYS,
        organisation_deletion_metrics_token: None,
        organisation_deletion_operator_token: None,
        cloud_action_enforcement_enabled: false,
        machine_eligibility_enforcement_enabled: false,
    })
}

fn request(method: &str, uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .expect("request")
}

async fn json(response: axum::response::Response) -> Value {
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("json response")
}

async fn seed_user(pool: &PgPool, user_id: &str) -> String {
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("pre-clean");
    sqlx::query(
        "INSERT INTO users
            (id, oauth_provider, oauth_subject, public_key, enc_private_keys, kdf_params, recovery_blob)
         VALUES ($1, 'github', $2, $3, $4, $5, $6)",
    )
    .bind(user_id)
    .bind(format!("{user_id}-subject"))
    .bind([1u8; 32].as_slice())
    .bind(b"private".as_slice())
    .bind(b"kdf".as_slice())
    .bind(b"recovery".as_slice())
    .execute(pool)
    .await
    .expect("insert user");
    session::issue(pool, user_id).await.expect("issue session")
}

async fn seed(pool: &PgPool, user_id: &str) -> String {
    let project_id = format!("{user_id}-project");
    let environment_id = format!("{user_id}-environment");
    let secret_id = format!("{user_id}-secret");
    let history_id = format!("{user_id}-history");
    let token = seed_user(pool, user_id).await;
    sqlx::query("INSERT INTO projects (id, owner_id, enc_name) VALUES ($1, $2, $3)")
        .bind(&project_id)
        .bind(user_id)
        .bind(b"project-name".as_slice())
        .execute(pool)
        .await
        .expect("insert project");
    sqlx::query(
        "INSERT INTO environments (id, project_id, enc_name, revision)
         VALUES ($1, $2, $3, 7)",
    )
    .bind(&environment_id)
    .bind(&project_id)
    .bind(b"environment-name".as_slice())
    .execute(pool)
    .await
    .expect("insert environment");
    sqlx::query(
        "INSERT INTO environment_grants (env_id, user_id, enc_vault_key, granted_by)
         VALUES ($1, $2, $3, $2)",
    )
    .bind(&environment_id)
    .bind(user_id)
    .bind(b"vault-grant".as_slice())
    .execute(pool)
    .await
    .expect("insert grant");
    sqlx::query(
        "INSERT INTO secrets (id, env_id, enc_name, enc_value, enc_data_key, version)
         VALUES ($1, $2, $3, $4, $5, 2)",
    )
    .bind(&secret_id)
    .bind(&environment_id)
    .bind(b"secret-name".as_slice())
    .bind(b"secret-value".as_slice())
    .bind(b"secret-key".as_slice())
    .execute(pool)
    .await
    .expect("insert secret");
    sqlx::query(
        "INSERT INTO secret_versions
            (id, secret_id, version, enc_name, enc_value, enc_data_key)
         VALUES ($1, $2, 1, $3, $4, $5)",
    )
    .bind(&history_id)
    .bind(&secret_id)
    .bind(b"old-name".as_slice())
    .bind(b"old-value".as_slice())
    .bind(b"old-key".as_slice())
    .execute(pool)
    .await
    .expect("insert history");
    token
}

async fn cleanup(pool: &PgPool, user_id: &str) {
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("clean");
}

#[tokio::test]
async fn export_manifest_and_chunks_preserve_opaque_history() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let user_id = "test-export-manifest-user";
    let token = seed(&pool, user_id).await;

    let response = app(pool.clone())
        .oneshot(request("POST", "/account/export", &token))
        .await
        .expect("start export");
    assert_eq!(response.status(), StatusCode::CREATED);
    let manifest = json(response).await;
    assert_eq!(manifest["version"], 1);
    assert_eq!(manifest["complete"], true);
    assert_eq!(manifest["total_chunks"], 2);
    assert_eq!(manifest["environments"][0]["revision"], 7);
    assert_eq!(manifest["omitted_environment_count"], 0);
    let export_id = manifest["export_id"].as_str().expect("export id");

    let response = app(pool.clone())
        .oneshot(request(
            "GET",
            &format!("/account/export/{export_id}/chunks/0"),
            &token,
        ))
        .await
        .expect("account chunk");
    assert_eq!(response.status(), StatusCode::OK);
    let account = json(response).await;
    assert_eq!(account["account"]["public_key"], STANDARD.encode([1u8; 32]));
    assert!(account["account"].get("plaintext").is_none());

    let response = app(pool.clone())
        .oneshot(request(
            "GET",
            &format!("/account/export/{export_id}/chunks/1"),
            &token,
        ))
        .await
        .expect("environment chunk");
    assert_eq!(response.status(), StatusCode::OK);
    let environment = json(response).await;
    assert_eq!(environment["environment"]["revision"], 7);
    assert_eq!(
        environment["environment"]["secrets"][0]["id"],
        format!("{user_id}-secret")
    );
    assert_eq!(environment["environment"]["history"][0]["version"], 1);
    assert_eq!(
        environment["environment"]["secrets"][0]["enc_value"],
        STANDARD.encode(b"secret-value")
    );

    cleanup(&pool, user_id).await;
}

#[tokio::test]
async fn export_excludes_unshared_organisation_environments_without_blocking_exit() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let owner_id = "test-export-org-owner";
    let member_id = "test-export-org-member";
    let org_id = "test-export-org";
    let project_id = "test-export-org-project";
    let shared_environment_id = "test-export-shared-environment";
    let unshared_environment_id = "test-export-unshared-environment";
    let token = seed_user(&pool, member_id).await;
    seed_user(&pool, owner_id).await;

    sqlx::query("INSERT INTO organizations (id, enc_name, created_by) VALUES ($1, $2, $3)")
        .bind(org_id)
        .bind(b"organisation-name".as_slice())
        .bind(owner_id)
        .execute(&pool)
        .await
        .expect("insert organisation");
    sqlx::query(
        "INSERT INTO organization_memberships (org_id, user_id, role) VALUES ($1, $2, 'owner'), ($1, $3, 'member')",
    )
    .bind(org_id)
    .bind(owner_id)
    .bind(member_id)
    .execute(&pool)
    .await
    .expect("insert memberships");
    sqlx::query("INSERT INTO projects (id, owner_id, org_id, enc_name) VALUES ($1, $2, $3, $4)")
        .bind(project_id)
        .bind(owner_id)
        .bind(org_id)
        .bind(b"project-name".as_slice())
        .execute(&pool)
        .await
        .expect("insert organisation project");
    sqlx::query(
        "INSERT INTO environments (id, project_id, enc_name, revision) VALUES ($1, $3, $2, 1), ($4, $3, $5, 1)",
    )
    .bind(shared_environment_id)
    .bind(b"shared-environment".as_slice())
    .bind(project_id)
    .bind(unshared_environment_id)
    .bind(b"unshared-environment".as_slice())
    .execute(&pool)
    .await
    .expect("insert organisation environments");
    sqlx::query(
        "INSERT INTO environment_grants (env_id, user_id, enc_vault_key, granted_by)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(shared_environment_id)
    .bind(member_id)
    .bind(b"vault-grant".as_slice())
    .bind(owner_id)
    .execute(&pool)
    .await
    .expect("grant shared environment");

    let response = app(pool.clone())
        .oneshot(request("POST", "/account/export", &token))
        .await
        .expect("start organisation export");
    assert_eq!(response.status(), StatusCode::CREATED);
    let manifest = json(response).await;
    assert_eq!(manifest["complete"], true);
    assert_eq!(manifest["omitted_environment_count"], 1);
    assert_eq!(
        manifest["not_shared_environment_ids"],
        serde_json::json!([unshared_environment_id])
    );
    assert_eq!(manifest["environments"].as_array().unwrap().len(), 1);
    assert_eq!(manifest["environments"][0]["id"], shared_environment_id);

    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org_id)
        .execute(&pool)
        .await
        .expect("clean organisation");
    cleanup(&pool, owner_id).await;
    cleanup(&pool, member_id).await;
}

#[tokio::test]
async fn export_rejects_revision_drift_after_manifest_creation() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let user_id = "test-export-drift-user";
    let token = seed(&pool, user_id).await;
    let response = app(pool.clone())
        .oneshot(request("POST", "/account/export", &token))
        .await
        .expect("start export");
    let manifest = json(response).await;
    let export_id = manifest["export_id"].as_str().expect("export id");
    sqlx::query("UPDATE environments SET revision = 8 WHERE id = $1")
        .bind(format!("{user_id}-environment"))
        .execute(&pool)
        .await
        .expect("advance revision");

    let response = app(pool.clone())
        .oneshot(request(
            "GET",
            &format!("/account/export/{export_id}/chunks/1"),
            &token,
        ))
        .await
        .expect("environment chunk");
    assert_eq!(response.status(), StatusCode::CONFLICT);
    cleanup(&pool, user_id).await;
}
