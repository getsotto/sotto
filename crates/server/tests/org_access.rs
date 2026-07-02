//! Org-scoped access to projects, environments, and secrets (M5 PR3a).
//!
//! An org member reaches the org's projects/envs and reads+writes their secret ciphertext; only
//! admins+ (or the personal owner) make structural changes; non-members see 404. Personal projects
//! remain owner-only. DB-gated like the other server tests; ids are test-scoped for parallel runs.

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
    token: Option<&str>,
    body: Option<String>,
) -> (StatusCode, String) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        builder = builder.header("authorization", format!("Bearer {t}"));
    }
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
    request(pool, "POST", uri, Some(token), Some(body)).await
}

async fn get(pool: &PgPool, token: &str, uri: &str) -> (StatusCode, String) {
    request(pool, "GET", uri, Some(token), None).await
}

fn b64(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

fn org_body(id: &str) -> String {
    format!(
        r#"{{"id":"{id}","enc_name":"{}","enc_org_key":"{}"}}"#,
        b64(b"org"),
        b64(b"sealed-org-key"),
    )
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

/// Create org `o` owned by `owner`, add `admin`/`member` at their roles, and stand up an org
/// project `p` with environment `e`. Returns nothing; ids are the caller's to reuse.
#[allow(clippy::too_many_arguments)]
async fn seed_org_project(
    pool: &PgPool,
    owner: &str,
    o: &str,
    p: &str,
    e: &str,
    admin_id: &str,
    member_id: &str,
) {
    post(pool, owner, "/orgs", org_body(o)).await;
    post(
        pool,
        owner,
        &format!("/orgs/{o}/members"),
        member_body(admin_id, "admin"),
    )
    .await;
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
}

#[tokio::test]
async fn org_member_can_read_and_write_org_secrets() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let (o, p, e) = ("acc-rw-o", "acc-rw-p", "acc-rw-e");
    reset_orgs(&pool, &[o]).await;
    let owner = fresh_session(&pool, "acc-rw-owner", "acc-rw-owner-s").await;
    let admin = fresh_session(&pool, "acc-rw-admin", "acc-rw-admin-s").await;
    let member = fresh_session(&pool, "acc-rw-member", "acc-rw-member-s").await;
    seed_org_project(&pool, &owner, o, p, e, "acc-rw-admin", "acc-rw-member").await;

    // Owner writes the first secret (revision 0 -> 1).
    assert_eq!(
        post(
            &pool,
            &owner,
            &format!("/environments/{e}/secrets"),
            set_body(0, "acc-rw-s1", 1)
        )
        .await
        .0,
        StatusCode::OK
    );

    // A plain member sees the project + environment and can read the secret ciphertext.
    let (status, body) = get(&pool, &member, "/projects").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(p), "member should see the org project");
    let (status, body) = get(&pool, &member, &format!("/projects/{p}/environments")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(e));
    let (status, body) = get(&pool, &member, &format!("/environments/{e}/secrets")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("acc-rw-s1"));

    // A member is a collaborator: they may write too (revision 1 -> 2).
    assert_eq!(
        post(
            &pool,
            &member,
            &format!("/environments/{e}/secrets"),
            set_body(1, "acc-rw-s2", 1)
        )
        .await
        .0,
        StatusCode::OK
    );
    // …and so may the admin.
    assert_eq!(
        get(&pool, &admin, &format!("/environments/{e}/secrets"))
            .await
            .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn structural_changes_require_admin() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let (o, p, e) = ("acc-struct-o", "acc-struct-p", "acc-struct-e");
    reset_orgs(&pool, &[o]).await;
    let owner = fresh_session(&pool, "acc-struct-owner", "acc-struct-owner-s").await;
    let admin = fresh_session(&pool, "acc-struct-admin", "acc-struct-admin-s").await;
    let member = fresh_session(&pool, "acc-struct-member", "acc-struct-member-s").await;
    seed_org_project(
        &pool,
        &owner,
        o,
        p,
        e,
        "acc-struct-admin",
        "acc-struct-member",
    )
    .await;

    // A member cannot create a project in the org nor an environment in one.
    assert_eq!(
        post(
            &pool,
            &member,
            "/projects",
            org_project_body("acc-struct-p2", o)
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        post(
            &pool,
            &member,
            &format!("/projects/{p}/environments"),
            env_body("acc-struct-e2")
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    // An admin can do both.
    assert_eq!(
        post(
            &pool,
            &admin,
            "/projects",
            org_project_body("acc-struct-p3", o)
        )
        .await
        .0,
        StatusCode::CREATED
    );
    assert_eq!(
        post(
            &pool,
            &admin,
            &format!("/projects/{p}/environments"),
            env_body("acc-struct-e3")
        )
        .await
        .0,
        StatusCode::CREATED
    );
}

#[tokio::test]
async fn non_member_cannot_reach_org_project() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let (o, p, e) = ("acc-out-o", "acc-out-p", "acc-out-e");
    reset_orgs(&pool, &[o]).await;
    let owner = fresh_session(&pool, "acc-out-owner", "acc-out-owner-s").await;
    let intruder = fresh_session(&pool, "acc-out-intruder", "acc-out-intruder-s").await;
    seed_org_project(&pool, &owner, o, p, e, "acc-out-owner", "acc-out-owner").await;

    // The intruder's project list excludes it, and every path is 404 (existence hidden).
    let (_, body) = get(&pool, &intruder, "/projects").await;
    assert!(!body.contains(p));
    assert_eq!(
        get(&pool, &intruder, &format!("/projects/{p}/environments"))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(&pool, &intruder, &format!("/environments/{e}/secrets"))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        post(
            &pool,
            &intruder,
            &format!("/environments/{e}/secrets"),
            set_body(0, "x", 1)
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    // A non-member cannot create a project in the org either.
    assert_eq!(
        post(
            &pool,
            &intruder,
            "/projects",
            org_project_body("acc-out-p2", o)
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn personal_projects_stay_owner_only() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let (p, e) = ("acc-pers-p", "acc-pers-e");
    let owner = fresh_session(&pool, "acc-pers-owner", "acc-pers-owner-s").await;
    let other = fresh_session(&pool, "acc-pers-other", "acc-pers-other-s").await;

    // A personal project (no org_id) behaves exactly as before: owner-only.
    assert_eq!(
        post(&pool, &owner, "/projects", project_body(p)).await.0,
        StatusCode::CREATED
    );
    assert_eq!(
        post(
            &pool,
            &owner,
            &format!("/projects/{p}/environments"),
            env_body(e)
        )
        .await
        .0,
        StatusCode::CREATED
    );
    assert_eq!(
        post(
            &pool,
            &owner,
            &format!("/environments/{e}/secrets"),
            set_body(0, "acc-pers-s1", 1)
        )
        .await
        .0,
        StatusCode::OK
    );

    // Another user cannot reach it.
    assert_eq!(
        get(&pool, &other, &format!("/projects/{p}/environments"))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(&pool, &other, &format!("/environments/{e}/secrets"))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}
