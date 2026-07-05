//! Machine-token endpoints (M5 PR5a): per-environment CI / service access.
//!
//! A machine token authenticates a tiny read-only surface (`/machine/grant`, `/machine/secrets`)
//! scoped to one environment. Management is admin+/owner; revocation kills access immediately; and
//! rotation must re-seal every active token's grant or it is rejected. DB-gated like the other
//! server tests; ids are test-scoped so parallel runs don't collide.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde_json::Value;
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
        billing: None,
    };
    Router::new()
        .merge(sotto_server::org::router())
        .merge(sotto_server::machine::router())
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

async fn delete(pool: &PgPool, token: &str, uri: &str) -> (StatusCode, String) {
    request(pool, "DELETE", uri, token, None).await
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
        b64(b"owner-grant"),
    )
}

fn set_body(base: i64, secret_id: &str) -> String {
    format!(
        r#"{{"base_revision":{base},"changes":[{{"id":"{secret_id}","op":"set","version":1,"enc_name":"{}","enc_value":"{}","enc_data_key":"{}"}}]}}"#,
        b64(b"name"),
        b64(b"value"),
        b64(b"old-dk"),
    )
}

fn token_body(name: &str, machine_grant: &[u8]) -> String {
    format!(
        r#"{{"name":"{name}","public_key":"{}","enc_vault_key":"{}"}}"#,
        b64(&[0xAB; 32]),
        b64(machine_grant),
    )
}

/// Create a machine token on `env` as `admin_token`; returns `(token_id, raw_api_token)`.
async fn create_token(
    pool: &PgPool,
    admin_token: &str,
    env: &str,
    machine_grant: &[u8],
) -> (String, String) {
    let (status, body) = post(
        pool,
        admin_token,
        &format!("/environments/{env}/tokens"),
        token_body("ci", machine_grant),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create token: {body}");
    let v: Value = serde_json::from_str(&body).expect("json");
    (
        v["token_id"].as_str().expect("token_id").to_string(),
        v["token"].as_str().expect("token").to_string(),
    )
}

/// Owner + org + org project + env with one secret; returns the owner's session.
async fn seed_org_env(pool: &PgPool, o: &str, p: &str, e: &str, owner_id: &str) -> String {
    reset_orgs(pool, &[o]).await;
    let owner = fresh_session(pool, owner_id, &format!("{owner_id}-s")).await;
    post(pool, &owner, "/orgs", org_body(o)).await;
    post(pool, &owner, "/projects", org_project_body(p, o)).await;
    post(
        pool,
        &owner,
        &format!("/projects/{p}/environments"),
        env_body(e),
    )
    .await;
    post(
        pool,
        &owner,
        &format!("/environments/{e}/secrets"),
        set_body(0, &format!("{e}-s1")),
    )
    .await;
    owner
}

#[tokio::test]
async fn machine_reads_its_grant_and_secrets() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let (o, p, e) = ("mt-read-o", "mt-read-p", "mt-read-e");
    let owner = seed_org_env(&pool, o, p, e, "mt-read-owner").await;
    let (token_id, api_token) = create_token(&pool, &owner, e, b"machine-grant").await;

    // The machine fetches its grant and the env snapshot with only its token.
    let (status, body) = get(&pool, &api_token, "/machine/grant").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(e), "grant response names the env");
    assert!(body.contains(&b64(b"machine-grant")));

    let (status, body) = get(&pool, &api_token, "/machine/secrets").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(&format!("{e}-s1")));
    assert!(body.contains(&b64(b"value")));

    // The token shows in the active listing (with its public key), visible to the admin.
    let (status, body) = get(&pool, &owner, &format!("/environments/{e}/tokens")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(&token_id));
    assert!(body.contains(&b64(&[0xAB; 32])));
}

#[tokio::test]
async fn token_management_requires_admin() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let (o, p, e) = ("mt-adm-o", "mt-adm-p", "mt-adm-e");
    let owner = seed_org_env(&pool, o, p, e, "mt-adm-owner").await;
    let member = fresh_session(&pool, "mt-adm-member", "mt-adm-member-s").await;
    post(
        &pool,
        &owner,
        &format!("/orgs/{o}/members"),
        member_body("mt-adm-member", "member"),
    )
    .await;

    let tokens_uri = format!("/environments/{e}/tokens");
    assert_eq!(
        post(&pool, &member, &tokens_uri, token_body("ci", b"g"))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        get(&pool, &member, &tokens_uri).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        delete(&pool, &member, &format!("{tokens_uri}/whatever"))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn revoked_token_loses_access_immediately() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let (o, p, e) = ("mt-rev-o", "mt-rev-p", "mt-rev-e");
    let owner = seed_org_env(&pool, o, p, e, "mt-rev-owner").await;
    let (token_id, api_token) = create_token(&pool, &owner, e, b"g").await;
    assert_eq!(
        get(&pool, &api_token, "/machine/grant").await.0,
        StatusCode::OK
    );

    let revoke_uri = format!("/environments/{e}/tokens/{token_id}");
    assert_eq!(
        delete(&pool, &owner, &revoke_uri).await.0,
        StatusCode::NO_CONTENT
    );
    // Dead immediately, gone from the active listing, and double-revoke is 404.
    assert_eq!(
        get(&pool, &api_token, "/machine/grant").await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        get(&pool, &api_token, "/machine/secrets").await.0,
        StatusCode::UNAUTHORIZED
    );
    let (_, body) = get(&pool, &owner, &format!("/environments/{e}/tokens")).await;
    assert!(!body.contains(&token_id));
    assert_eq!(
        delete(&pool, &owner, &revoke_uri).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn token_namespaces_do_not_cross() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let (o, p, e) = ("mt-ns-o", "mt-ns-p", "mt-ns-e");
    let owner = seed_org_env(&pool, o, p, e, "mt-ns-owner").await;
    let (_token_id, api_token) = create_token(&pool, &owner, e, b"g").await;

    // A machine token is not a session: the general API rejects it.
    assert_eq!(
        get(&pool, &api_token, "/projects").await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        get(&pool, &api_token, &format!("/environments/{e}/secrets"))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    // And a user session is not a machine token.
    assert_eq!(
        get(&pool, &owner, "/machine/grant").await.0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn rotation_must_regrant_active_tokens() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let (o, p, e) = ("mt-rot-o", "mt-rot-p", "mt-rot-e");
    let owner = seed_org_env(&pool, o, p, e, "mt-rot-owner").await;
    let (token_id, api_token) = create_token(&pool, &owner, e, b"machine-old").await;
    let rotate_uri = format!("/environments/{e}/rotate");
    let s1 = format!("{e}-s1");

    // Omitting the machine grant is rejected, and nothing changed.
    let without = format!(
        r#"{{"base_revision":1,"grants":[{{"user_id":"mt-rot-owner","enc_vault_key":"{}"}}],"data_keys":[{{"secret_id":"{s1}","enc_data_key":"{}"}}],"history_keys":[{{"secret_id":"{s1}","version":1,"enc_data_key":"{}"}}]}}"#,
        b64(b"owner-new"),
        b64(b"new-dk"),
        b64(b"new-dk-hist"),
    );
    assert_eq!(
        post(&pool, &owner, &rotate_uri, without).await.0,
        StatusCode::BAD_REQUEST
    );
    let (_, body) = get(&pool, &api_token, "/machine/grant").await;
    assert!(
        body.contains(&b64(b"machine-old")),
        "grant untouched after rejected rotation"
    );

    // Including it succeeds, and the machine immediately sees its re-sealed grant.
    let with = format!(
        r#"{{"base_revision":1,"grants":[{{"user_id":"mt-rot-owner","enc_vault_key":"{}"}}],"data_keys":[{{"secret_id":"{s1}","enc_data_key":"{}"}}],"history_keys":[{{"secret_id":"{s1}","version":1,"enc_data_key":"{}"}}],"machine_grants":[{{"token_id":"{token_id}","enc_vault_key":"{}"}}]}}"#,
        b64(b"owner-new"),
        b64(b"new-dk"),
        b64(b"new-dk-hist"),
        b64(b"machine-new"),
    );
    assert_eq!(
        post(&pool, &owner, &rotate_uri, with).await.0,
        StatusCode::OK
    );
    let (_, body) = get(&pool, &api_token, "/machine/grant").await;
    assert!(body.contains(&b64(b"machine-new")));

    // After revoking the token, a rotation with no machine grants is accepted again.
    delete(
        &pool,
        &owner,
        &format!("/environments/{e}/tokens/{token_id}"),
    )
    .await;
    let after_revoke = format!(
        r#"{{"base_revision":2,"grants":[{{"user_id":"mt-rot-owner","enc_vault_key":"{}"}}],"data_keys":[{{"secret_id":"{s1}","enc_data_key":"{}"}}],"history_keys":[{{"secret_id":"{s1}","version":1,"enc_data_key":"{}"}}]}}"#,
        b64(b"owner-newer"),
        b64(b"newer-dk"),
        b64(b"newer-dk-hist"),
    );
    assert_eq!(
        post(&pool, &owner, &rotate_uri, after_revoke).await.0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn rotation_rejects_duplicate_machine_tokens() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let (o, p, e) = ("mt-dup-o", "mt-dup-p", "mt-dup-e");
    let owner = seed_org_env(&pool, o, p, e, "mt-dup-owner").await;
    let (token_id, _api_token) = create_token(&pool, &owner, e, b"machine-old").await;
    let s1 = format!("{e}-s1");

    // Listing the one active token twice would dedup to a valid coverage set but drive an ambiguous
    // double-UPDATE; it must be rejected as malformed input, like a duplicate user grant.
    let dup = format!(
        r#"{{"base_revision":1,"grants":[{{"user_id":"mt-dup-owner","enc_vault_key":"{}"}}],"data_keys":[{{"secret_id":"{s1}","enc_data_key":"{}"}}],"history_keys":[{{"secret_id":"{s1}","version":1,"enc_data_key":"{}"}}],"machine_grants":[{{"token_id":"{token_id}","enc_vault_key":"{}"}},{{"token_id":"{token_id}","enc_vault_key":"{}"}}]}}"#,
        b64(b"owner-new"),
        b64(b"new-dk"),
        b64(b"new-dk-hist"),
        b64(b"machine-a"),
        b64(b"machine-b"),
    );
    assert_eq!(
        post(&pool, &owner, &format!("/environments/{e}/rotate"), dup)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    // The rejected rotation changed nothing: the machine still holds its original grant.
    let (_, body) = get(&pool, &owner, &format!("/environments/{e}/tokens")).await;
    assert!(
        body.contains(&token_id),
        "token still active after rejection"
    );
}

#[tokio::test]
async fn personal_project_tokens_work() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let (p, e) = ("mt-pers-p", "mt-pers-e");
    let owner = fresh_session(&pool, "mt-pers-owner", "mt-pers-owner-s").await;
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
        set_body(0, "mt-pers-s1"),
    )
    .await;

    // Solo-dev CI: a personal env takes machine tokens too.
    let (_token_id, api_token) = create_token(&pool, &owner, e, b"g").await;
    let (status, body) = get(&pool, &api_token, "/machine/secrets").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("mt-pers-s1"));
}
