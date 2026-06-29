//! Sync API integration tests: projects, environments, snapshot/ETag, and atomic batch writes.
//!
//! DB-gated like the others. Each test uses fixed marker ids under a fixed user, and pre-cleans by
//! deleting that user (cascading to its projects/environments/secrets), so reruns are idempotent.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use sqlx::PgPool;
use tower::ServiceExt;

use sotto_server::auth::session;
use sotto_server::db;
use sotto_server::state::AppState;

async fn pool_or_skip() -> Option<PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = db::connect(&url).await.expect("connect");
    db::migrate(&pool).await.expect("migrate");
    Some(pool)
}

fn app(pool: PgPool) -> Router {
    let state = AppState {
        pool,
        oauth: None,
        oauth_config: None,
    };
    Router::new()
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

fn b64(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

async fn body_text(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body");
    String::from_utf8(bytes.to_vec()).expect("utf8")
}

/// (status, ETag header, body) for a one-shot request.
async fn send(app: Router, req: Request<Body>) -> (StatusCode, Option<String>, String) {
    let resp = app.oneshot(req).await.expect("oneshot");
    let status = resp.status();
    let etag = resp
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    (status, etag, body_text(resp).await)
}

async fn post(pool: &PgPool, token: &str, uri: &str, body: String) -> (StatusCode, Option<String>, String) {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(body))
        .expect("req");
    send(app(pool.clone()), req).await
}

async fn get(
    pool: &PgPool,
    token: Option<&str>,
    uri: &str,
    if_none_match: Option<&str>,
) -> (StatusCode, Option<String>, String) {
    let mut builder = Request::builder().method("GET").uri(uri);
    if let Some(t) = token {
        builder = builder.header("authorization", format!("Bearer {t}"));
    }
    if let Some(inm) = if_none_match {
        builder = builder.header("if-none-match", inm);
    }
    send(app(pool.clone()), builder.body(Body::empty()).expect("req")).await
}

fn project_body(id: &str) -> String {
    format!(r#"{{"id":"{id}","enc_name":"{}"}}"#, b64(b"project"))
}

fn env_body(id: &str) -> String {
    format!(
        r#"{{"id":"{id}","enc_name":"{}","enc_vault_key":"{}"}}"#,
        b64(b"env"),
        b64(b"vault-key"),
    )
}

fn set_body(base: i64, secret_id: &str, version: i64) -> String {
    format!(
        r#"{{"base_revision":{base},"changes":[{{"id":"{secret_id}","op":"set","version":{version},"enc_name":"{}","enc_value":"{}","enc_data_key":"{}"}}]}}"#,
        b64(b"name"),
        b64(b"value"),
        b64(b"data-key"),
    )
}

fn delete_body(base: i64, secret_id: &str) -> String {
    format!(r#"{{"base_revision":{base},"changes":[{{"id":"{secret_id}","op":"delete"}}]}}"#)
}

#[tokio::test]
async fn project_and_environment_lifecycle() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let token = fresh_session(&pool, "sync-life-u", "sync-life-s").await;
    let (proj, env) = ("sync-life-p", "sync-life-e");

    assert_eq!(post(&pool, &token, "/projects", project_body(proj)).await.0, StatusCode::CREATED);
    // Idempotent re-create of one's own project.
    assert_eq!(post(&pool, &token, "/projects", project_body(proj)).await.0, StatusCode::OK);

    let env_uri = format!("/projects/{proj}/environments");
    assert_eq!(post(&pool, &token, &env_uri, env_body(env)).await.0, StatusCode::CREATED);

    let (status, _, body) = get(&pool, Some(&token), &env_uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(env));
    assert!(body.contains("\"revision\":0"));

    // Fresh environment snapshot: revision 0, no secrets.
    let (status, etag, body) = get(&pool, Some(&token), &format!("/environments/{env}/secrets"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(etag.as_deref(), Some("\"0\""));
    assert!(body.contains("\"revision\":0"));
    assert!(body.contains("\"secrets\":[]"));
}

#[tokio::test]
async fn batch_set_then_snapshot() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let token = fresh_session(&pool, "sync-batch-u", "sync-batch-s").await;
    let (proj, env, secret) = ("sync-batch-p", "sync-batch-e", "sync-batch-s1");
    post(&pool, &token, "/projects", project_body(proj)).await;
    post(&pool, &token, &format!("/projects/{proj}/environments"), env_body(env)).await;

    let secrets_uri = format!("/environments/{env}/secrets");
    let (status, etag, body) = post(&pool, &token, &secrets_uri, set_body(0, secret, 1)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(etag.as_deref(), Some("\"1\""));
    assert!(body.contains("\"revision\":1"));

    let (status, etag, body) = get(&pool, Some(&token), &secrets_uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(etag.as_deref(), Some("\"1\""));
    assert!(body.contains("\"revision\":1"));
    assert!(body.contains(secret));
    assert!(body.contains(&b64(b"value")));
    assert!(body.contains("\"deleted\":false"));

    // History was recorded.
    let versions: i64 = sqlx::query_scalar("SELECT count(*) FROM secret_versions WHERE secret_id = $1")
        .bind(secret)
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(versions, 1);
}

#[tokio::test]
async fn stale_base_revision_is_rejected() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let token = fresh_session(&pool, "sync-stale-u", "sync-stale-s").await;
    let (proj, env) = ("sync-stale-p", "sync-stale-e");
    post(&pool, &token, "/projects", project_body(proj)).await;
    post(&pool, &token, &format!("/projects/{proj}/environments"), env_body(env)).await;
    let secrets_uri = format!("/environments/{env}/secrets");

    assert_eq!(post(&pool, &token, &secrets_uri, set_body(0, "sync-stale-s1", 1)).await.0, StatusCode::OK);
    // Revision is now 1; a write still claiming base_revision 0 is stale.
    assert_eq!(
        post(&pool, &token, &secrets_uri, set_body(0, "sync-stale-s2", 1)).await.0,
        StatusCode::PRECONDITION_FAILED
    );
}

#[tokio::test]
async fn delete_tombstones_without_changing_version() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let token = fresh_session(&pool, "sync-del-u", "sync-del-s").await;
    let (proj, env, secret) = ("sync-del-p", "sync-del-e", "sync-del-s1");
    post(&pool, &token, "/projects", project_body(proj)).await;
    post(&pool, &token, &format!("/projects/{proj}/environments"), env_body(env)).await;
    let secrets_uri = format!("/environments/{env}/secrets");

    post(&pool, &token, &secrets_uri, set_body(0, secret, 1)).await; // rev 1
    let (status, etag, _) = post(&pool, &token, &secrets_uri, delete_body(1, secret)).await; // rev 2
    assert_eq!(status, StatusCode::OK);
    assert_eq!(etag.as_deref(), Some("\"2\""));

    let (_, _, body) = get(&pool, Some(&token), &secrets_uri, None).await;
    assert!(body.contains("\"deleted\":true"));
    assert!(body.contains("\"version\":1")); // version unchanged by delete
}

#[tokio::test]
async fn snapshot_honors_if_none_match() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let token = fresh_session(&pool, "sync-etag-u", "sync-etag-s").await;
    let (proj, env) = ("sync-etag-p", "sync-etag-e");
    post(&pool, &token, "/projects", project_body(proj)).await;
    post(&pool, &token, &format!("/projects/{proj}/environments"), env_body(env)).await;
    let secrets_uri = format!("/environments/{env}/secrets");

    let (_, etag, _) = get(&pool, Some(&token), &secrets_uri, None).await;
    let etag = etag.expect("etag");
    let (status, _, _) = get(&pool, Some(&token), &secrets_uri, Some(&etag)).await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);
}

#[tokio::test]
async fn set_for_another_environments_secret_conflicts() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let token = fresh_session(&pool, "sync-xenv-u", "sync-xenv-s").await;
    let proj = "sync-xenv-p";
    post(&pool, &token, "/projects", project_body(proj)).await;
    post(&pool, &token, &format!("/projects/{proj}/environments"), env_body("sync-xenv-a")).await;
    post(&pool, &token, &format!("/projects/{proj}/environments"), env_body("sync-xenv-b")).await;

    let shared_secret = "sync-xenv-shared";
    assert_eq!(
        post(&pool, &token, "/environments/sync-xenv-a/secrets", set_body(0, shared_secret, 1)).await.0,
        StatusCode::OK
    );
    // The same secret id under env B must not hijack env A's row.
    assert_eq!(
        post(&pool, &token, "/environments/sync-xenv-b/secrets", set_body(0, shared_secret, 1)).await.0,
        StatusCode::CONFLICT
    );
    // Env B's revision was not bumped (transaction rolled back).
    let rev: i64 = sqlx::query_scalar("SELECT revision FROM environments WHERE id = 'sync-xenv-b'")
        .fetch_one(&pool)
        .await
        .expect("rev");
    assert_eq!(rev, 0);
}

#[tokio::test]
async fn ownership_is_enforced() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let owner = fresh_session(&pool, "sync-own-a", "sync-own-a-s").await;
    let intruder = fresh_session(&pool, "sync-own-b", "sync-own-b-s").await;
    let (proj, env) = ("sync-own-p", "sync-own-e");
    post(&pool, &owner, "/projects", project_body(proj)).await;
    post(&pool, &owner, &format!("/projects/{proj}/environments"), env_body(env)).await;

    // The intruder cannot see the owner's environment snapshot.
    let (status, _, _) = get(&pool, Some(&intruder), &format!("/environments/{env}/secrets"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // …nor write to it.
    let (status, _, _) = post(&pool, &intruder, &format!("/environments/{env}/secrets"), set_body(0, "x", 1)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Clean up the second user (the first is cleaned by the next run's fresh_session).
    sqlx::query("DELETE FROM users WHERE id = 'sync-own-a'").execute(&pool).await.unwrap();
}

#[tokio::test]
async fn auth_is_required() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    assert_eq!(get(&pool, None, "/projects", None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(get(&pool, None, "/environments/whatever/secrets", None).await.0, StatusCode::UNAUTHORIZED);
}
