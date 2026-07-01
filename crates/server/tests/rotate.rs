//! Environment key-rotation endpoint (M5 PR4a).
//!
//! Rotation replaces an env's grant set (dropping a removed member) and rewraps every secret's data
//! key, in one transaction guarded by optimistic concurrency on the revision. The server treats the
//! crypto as opaque bytes — these tests exercise the DB mechanics (grant swap, data-key update,
//! revision bump, auth, coverage, atomicity); the crypto itself is covered in `sotto-core`.

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
        .merge(sotto_server::org::router())
        .merge(sotto_server::sync::router())
        .with_state(state)
}

async fn reset_orgs(pool: &PgPool, orgs: &[&str]) {
    for org in orgs {
        sqlx::query("DELETE FROM organizations WHERE id = $1")
            .bind(org)
            .execute(pool)
            .await
            .expect("reset org");
    }
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

async fn ensure_user(pool: &PgPool, user_id: &str, subject: &str) {
    sqlx::query(
        "INSERT INTO users (id, oauth_provider, oauth_subject) VALUES ($1, 'github', $2) \
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(user_id)
    .bind(subject)
    .execute(pool)
    .await
    .expect("ensure user");
}

async fn body_text(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body");
    String::from_utf8(bytes.to_vec()).expect("utf8")
}

async fn request(
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

async fn post(pool: &PgPool, token: &str, uri: &str, body: String) -> (StatusCode, String) {
    request(pool, "POST", uri, token, Some(body)).await
}

async fn get(pool: &PgPool, token: &str, uri: &str) -> (StatusCode, String) {
    request(pool, "GET", uri, token, None).await
}

fn b64(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

fn org_body(id: &str) -> String {
    format!(r#"{{"id":"{id}","enc_name":"{}"}}"#, b64(b"org"))
}

fn member_body(user_id: &str, role: &str) -> String {
    format!(r#"{{"user_id":"{user_id}","role":"{role}"}}"#)
}

fn org_project_body(id: &str, org_id: &str) -> String {
    format!(
        r#"{{"id":"{id}","enc_name":"{}","org_id":"{org_id}"}}"#,
        b64(b"project")
    )
}

fn project_body(id: &str) -> String {
    format!(r#"{{"id":"{id}","enc_name":"{}"}}"#, b64(b"project"))
}

fn env_body(id: &str) -> String {
    format!(
        r#"{{"id":"{id}","enc_name":"{}","enc_vault_key":"{}"}}"#,
        b64(b"env"),
        b64(b"old-grant"),
    )
}

fn set_body(base: i64, secret_id: &str, version: i64) -> String {
    format!(
        r#"{{"base_revision":{base},"changes":[{{"id":"{secret_id}","op":"set","version":{version},"enc_name":"{}","enc_value":"{}","enc_data_key":"{}"}}]}}"#,
        b64(b"name"),
        b64(b"value"),
        b64(b"old-dk"),
    )
}

fn grant_body(user_id: &str, key: &[u8]) -> String {
    format!(
        r#"{{"user_id":"{user_id}","enc_vault_key":"{}"}}"#,
        b64(key)
    )
}

fn rotate_body(base: i64, grants: &[(&str, &[u8])], data_keys: &[(&str, &[u8])]) -> String {
    let grants = grants
        .iter()
        .map(|(u, k)| format!(r#"{{"user_id":"{u}","enc_vault_key":"{}"}}"#, b64(k)))
        .collect::<Vec<_>>()
        .join(",");
    let dks = data_keys
        .iter()
        .map(|(s, k)| format!(r#"{{"secret_id":"{s}","enc_data_key":"{}"}}"#, b64(k)))
        .collect::<Vec<_>>()
        .join(",");
    format!(r#"{{"base_revision":{base},"grants":[{grants}],"data_keys":[{dks}]}}"#)
}

/// Owner creates org `o`, an org project `p` with env `e`, adds `member` (a member) and grants them
/// the env; writes secret(s) `secrets`. Returns the env revision after the writes.
async fn seed(
    pool: &PgPool,
    owner: &str,
    o: &str,
    p: &str,
    e: &str,
    member_id: &str,
    secrets: &[&str],
) -> i64 {
    post(pool, owner, "/orgs", org_body(o)).await;
    post(
        pool,
        owner,
        &format!("/orgs/{o}/members"),
        member_body(member_id, "member"),
    )
    .await;
    post(pool, owner, "/projects", org_project_body(p, o)).await;
    post(
        pool,
        owner,
        &format!("/projects/{p}/environments"),
        env_body(e),
    )
    .await;
    // Grant the member the env, so rotation has someone to drop.
    post(
        pool,
        owner,
        &format!("/environments/{e}/grants"),
        grant_body(member_id, b"member-grant"),
    )
    .await;
    let secrets_uri = format!("/environments/{e}/secrets");
    let mut rev = 0;
    for (i, s) in secrets.iter().enumerate() {
        let (status, _) = post(pool, owner, &secrets_uri, set_body(i as i64, s, 1)).await;
        assert_eq!(status, StatusCode::OK, "seed secret write");
        rev = i as i64 + 1;
    }
    rev
}

#[tokio::test]
async fn rotate_swaps_grants_and_rewraps_data_keys() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let (o, p, e, s1) = ("rot-ok-o", "rot-ok-p", "rot-ok-e", "rot-ok-s1");
    reset_orgs(&pool, &[o]).await;
    let owner = fresh_session(&pool, "rot-ok-owner", "rot-ok-owner-s").await;
    let member = fresh_session(&pool, "rot-ok-member", "rot-ok-member-s").await;
    let rev = seed(&pool, &owner, o, p, e, "rot-ok-member", &[s1]).await;

    // Rotate: re-grant only the owner (dropping the member) and rewrap s1's data key.
    let (status, _) = post(
        &pool,
        &owner,
        &format!("/environments/{e}/rotate"),
        rotate_body(rev, &[("rot-ok-owner", b"new-grant")], &[(s1, b"new-dk")]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The owner's grant is the new vault key; the member's grant is gone.
    let (status, body) = get(&pool, &owner, &format!("/environments/{e}/grant")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(&b64(b"new-grant")));
    assert_eq!(
        get(&pool, &member, &format!("/environments/{e}/grant"))
            .await
            .0,
        StatusCode::NOT_FOUND
    );

    // The secret's data key was rewrapped; its value is untouched; the revision advanced.
    let (_, snap) = get(&pool, &owner, &format!("/environments/{e}/secrets")).await;
    assert!(snap.contains(&b64(b"new-dk")));
    assert!(!snap.contains(&b64(b"old-dk")));
    assert!(snap.contains(&b64(b"value")));
    assert!(snap.contains(&format!("\"revision\":{}", rev + 1)));
}

#[tokio::test]
async fn rotate_requires_admin() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let (o, p, e, s1) = ("rot-adm-o", "rot-adm-p", "rot-adm-e", "rot-adm-s1");
    reset_orgs(&pool, &[o]).await;
    let owner = fresh_session(&pool, "rot-adm-owner", "rot-adm-owner-s").await;
    let member = fresh_session(&pool, "rot-adm-member", "rot-adm-member-s").await;
    let rev = seed(&pool, &owner, o, p, e, "rot-adm-member", &[s1]).await;

    // A plain member (even one with a grant) cannot rotate.
    assert_eq!(
        post(
            &pool,
            &member,
            &format!("/environments/{e}/rotate"),
            rotate_body(rev, &[("rot-adm-member", b"k")], &[(s1, b"dk")]),
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn rotate_stale_base_revision_conflicts() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let (o, p, e, s1) = ("rot-stale-o", "rot-stale-p", "rot-stale-e", "rot-stale-s1");
    reset_orgs(&pool, &[o]).await;
    let owner = fresh_session(&pool, "rot-stale-owner", "rot-stale-owner-s").await;
    ensure_user(&pool, "rot-stale-member", "rot-stale-member-s").await;
    let rev = seed(&pool, &owner, o, p, e, "rot-stale-member", &[s1]).await;

    // A rotation at the wrong base revision is rejected.
    assert_eq!(
        post(
            &pool,
            &owner,
            &format!("/environments/{e}/rotate"),
            rotate_body(rev - 1, &[("rot-stale-owner", b"k")], &[(s1, b"dk")]),
        )
        .await
        .0,
        StatusCode::PRECONDITION_FAILED
    );
}

#[tokio::test]
async fn rotate_must_cover_all_secrets_and_is_atomic() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let (o, p, e) = ("rot-cov-o", "rot-cov-p", "rot-cov-e");
    let (s1, s2) = ("rot-cov-s1", "rot-cov-s2");
    reset_orgs(&pool, &[o]).await;
    let owner = fresh_session(&pool, "rot-cov-owner", "rot-cov-owner-s").await;
    let member = fresh_session(&pool, "rot-cov-member", "rot-cov-member-s").await;
    let rev = seed(&pool, &owner, o, p, e, "rot-cov-member", &[s1, s2]).await;

    // Rewrapping only one of the two secrets is rejected…
    assert_eq!(
        post(
            &pool,
            &owner,
            &format!("/environments/{e}/rotate"),
            rotate_body(rev, &[("rot-cov-owner", b"new-grant")], &[(s1, b"new-dk")]),
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );

    // …and nothing was applied: revision, data keys, and the member's grant are all unchanged.
    let (_, snap) = get(&pool, &owner, &format!("/environments/{e}/secrets")).await;
    assert!(snap.contains(&format!("\"revision\":{rev}")));
    assert!(snap.contains(&b64(b"old-dk")));
    assert!(!snap.contains(&b64(b"new-dk")));
    assert_eq!(
        get(&pool, &member, &format!("/environments/{e}/grant"))
            .await
            .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn rotate_requires_the_callers_own_grant() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let (o, p, e, s1) = ("rot-self-o", "rot-self-p", "rot-self-e", "rot-self-s1");
    reset_orgs(&pool, &[o]).await;
    let owner = fresh_session(&pool, "rot-self-owner", "rot-self-owner-s").await;
    ensure_user(&pool, "rot-self-member", "rot-self-member-s").await;
    let rev = seed(&pool, &owner, o, p, e, "rot-self-member", &[s1]).await;

    // Omitting the caller's own grant would lock them out — rejected.
    assert_eq!(
        post(
            &pool,
            &owner,
            &format!("/environments/{e}/rotate"),
            rotate_body(rev, &[("rot-self-member", b"k")], &[(s1, b"dk")]),
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn rotate_personal_env_is_rejected() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let (p, e, s1) = ("rot-pers-p", "rot-pers-e", "rot-pers-s1");
    let owner = fresh_session(&pool, "rot-pers-owner", "rot-pers-owner-s").await;
    post(&pool, &owner, "/projects", project_body(p)).await;
    post(
        &pool,
        &owner,
        &format!("/projects/{p}/environments"),
        env_body(e),
    )
    .await;
    post(
        &pool,
        &owner,
        &format!("/environments/{e}/secrets"),
        set_body(0, s1, 1),
    )
    .await;

    // A personal environment has no org to re-grant within.
    assert_eq!(
        post(
            &pool,
            &owner,
            &format!("/environments/{e}/rotate"),
            rotate_body(1, &[("rot-pers-owner", b"k")], &[(s1, b"dk")]),
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
}
