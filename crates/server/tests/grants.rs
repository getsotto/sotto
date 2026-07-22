//! Environment vault-key grants + invite-by-email (M5 PR3b).
//!
//! A member with org access still cannot decrypt an environment until they are granted its vault
//! key; sharing is admin+/owner and org-scoped. Invites resolve an existing user by email and add
//! them as a member. DB-gated; ids/emails are test-scoped for parallel runs.

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
        telemetry_ingest: false,
        pool,
        oauth: None,
        oauth_config: None,
        billing: None,
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

/// Insert a plain user (an invite/grant target that never acts), optionally with an email.
async fn ensure_user(pool: &PgPool, user_id: &str, subject: &str, email: Option<&str>) {
    sqlx::query(
        "INSERT INTO users (id, oauth_provider, oauth_subject, email) VALUES ($1, 'github', $2, $3) \
         ON CONFLICT (id) DO UPDATE SET email = EXCLUDED.email",
    )
    .bind(user_id)
    .bind(subject)
    .bind(email)
    .execute(pool)
    .await
    .expect("ensure user");
}

/// Set a user's public key to 32 bytes of `byte` (a stand-in for a real X25519 key).
async fn set_public_key(pool: &PgPool, user_id: &str, byte: u8) {
    sqlx::query("UPDATE users SET public_key = $2 WHERE id = $1")
        .bind(user_id)
        .bind(vec![byte; 32])
        .execute(pool)
        .await
        .expect("set public_key");
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

fn env_body(id: &str, vault_key: &[u8]) -> String {
    format!(
        r#"{{"id":"{id}","enc_name":"{}","enc_vault_key":"{}"}}"#,
        b64(b"env"),
        b64(vault_key),
    )
}

fn grant_body(user_id: &str, enc_vault_key: &[u8]) -> String {
    format!(
        r#"{{"user_id":"{user_id}","enc_vault_key":"{}"}}"#,
        b64(enc_vault_key)
    )
}

#[tokio::test]
async fn creator_has_a_grant_others_do_not_until_shared() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let (o, p, e) = ("gr-share-o", "gr-share-p", "gr-share-e");
    reset_orgs(&pool, &[o]).await;
    let owner = fresh_session(&pool, "gr-share-owner", "gr-share-owner-s").await;
    let member = fresh_session(&pool, "gr-share-member", "gr-share-member-s").await;
    post(&pool, &owner, "/orgs", org_body(o)).await;
    post(
        &pool,
        &owner,
        &format!("/orgs/{o}/members"),
        member_body("gr-share-member", "member"),
    )
    .await;
    post(&pool, &owner, "/projects", org_project_body(p, o)).await;
    post(
        &pool,
        &owner,
        &format!("/projects/{p}/environments"),
        env_body(e, b"owner-grant"),
    )
    .await;

    let grant_uri = format!("/environments/{e}/grant");
    // The creator's grant was recorded at env creation.
    let (status, body) = get(&pool, &owner, &grant_uri).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(&b64(b"owner-grant")));

    // The member has access (PR3a) but no grant yet - cannot decrypt.
    assert_eq!(
        get(&pool, &member, &grant_uri).await.0,
        StatusCode::NOT_FOUND
    );

    // Owner shares the env with the member; now they have a grant.
    assert_eq!(
        post(
            &pool,
            &owner,
            &format!("/environments/{e}/grants"),
            grant_body("gr-share-member", b"member-grant")
        )
        .await
        .0,
        StatusCode::OK
    );
    let (status, body) = get(&pool, &member, &grant_uri).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(&b64(b"member-grant")));
}

#[tokio::test]
async fn only_admins_can_share() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let (o, p, e) = ("gr-adm-o", "gr-adm-p", "gr-adm-e");
    reset_orgs(&pool, &[o]).await;
    let owner = fresh_session(&pool, "gr-adm-owner", "gr-adm-owner-s").await;
    let admin = fresh_session(&pool, "gr-adm-admin", "gr-adm-admin-s").await;
    let member = fresh_session(&pool, "gr-adm-member", "gr-adm-member-s").await;
    post(&pool, &owner, "/orgs", org_body(o)).await;
    post(
        &pool,
        &owner,
        &format!("/orgs/{o}/members"),
        member_body("gr-adm-admin", "admin"),
    )
    .await;
    post(
        &pool,
        &owner,
        &format!("/orgs/{o}/members"),
        member_body("gr-adm-member", "member"),
    )
    .await;
    post(&pool, &owner, "/projects", org_project_body(p, o)).await;
    post(
        &pool,
        &owner,
        &format!("/projects/{p}/environments"),
        env_body(e, b"k"),
    )
    .await;
    let grants_uri = format!("/environments/{e}/grants");

    // A plain member cannot share; an admin can.
    assert_eq!(
        post(
            &pool,
            &member,
            &grants_uri,
            grant_body("gr-adm-member", b"g")
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        post(
            &pool,
            &admin,
            &grants_uri,
            grant_body("gr-adm-member", b"g")
        )
        .await
        .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn cannot_grant_to_a_non_member() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let (o, p, e) = ("gr-nonmem-o", "gr-nonmem-p", "gr-nonmem-e");
    reset_orgs(&pool, &[o]).await;
    let owner = fresh_session(&pool, "gr-nonmem-owner", "gr-nonmem-owner-s").await;
    post(&pool, &owner, "/orgs", org_body(o)).await;
    post(&pool, &owner, "/projects", org_project_body(p, o)).await;
    post(
        &pool,
        &owner,
        &format!("/projects/{p}/environments"),
        env_body(e, b"k"),
    )
    .await;

    // The target is not a member of the org: bad request, not a silent dead grant.
    assert_eq!(
        post(
            &pool,
            &owner,
            &format!("/environments/{e}/grants"),
            grant_body("gr-nonmem-stranger", b"g")
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn cannot_share_a_personal_environment() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let (p, e) = ("gr-pers-p", "gr-pers-e");
    let owner = fresh_session(&pool, "gr-pers-owner", "gr-pers-owner-s").await;
    post(&pool, &owner, "/projects", project_body(p)).await;
    post(
        &pool,
        &owner,
        &format!("/projects/{p}/environments"),
        env_body(e, b"k"),
    )
    .await;

    // A personal environment has no org to share within.
    assert_eq!(
        post(
            &pool,
            &owner,
            &format!("/environments/{e}/grants"),
            grant_body("gr-pers-owner", b"g")
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn members_listing_exposes_public_keys() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let o = "gr-keys-o";
    reset_orgs(&pool, &[o]).await;
    let owner = fresh_session(&pool, "gr-keys-owner", "gr-keys-owner-s").await;
    ensure_user(&pool, "gr-keys-member", "gr-keys-member-s", None).await;
    set_public_key(&pool, "gr-keys-owner", 0xAA).await;
    set_public_key(&pool, "gr-keys-member", 0xBB).await;
    post(&pool, &owner, "/orgs", org_body(o)).await;
    post(
        &pool,
        &owner,
        &format!("/orgs/{o}/members"),
        member_body("gr-keys-member", "member"),
    )
    .await;

    let (status, body) = get(&pool, &owner, &format!("/orgs/{o}/members")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains(&b64(&[0xAAu8; 32])),
        "owner public key should be listed"
    );
    assert!(
        body.contains(&b64(&[0xBBu8; 32])),
        "member public key should be listed"
    );
}

#[tokio::test]
async fn invite_by_email_adds_member() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let o = "gr-inv-o";
    reset_orgs(&pool, &[o]).await;
    let owner = fresh_session(&pool, "gr-inv-owner", "gr-inv-owner-s").await;
    ensure_user(
        &pool,
        "gr-inv-target",
        "gr-inv-target-s",
        Some("gr-inv-target@example.test"),
    )
    .await;
    set_public_key(&pool, "gr-inv-target", 0xCC).await;
    post(&pool, &owner, "/orgs", org_body(o)).await;
    let invites_uri = format!("/orgs/{o}/invites");

    // Invite the existing user by email: added as a member, pubkey returned for immediate granting.
    let (status, body) = post(
        &pool,
        &owner,
        &invites_uri,
        r#"{"email":"gr-inv-target@example.test"}"#.into(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("gr-inv-target"));
    assert!(body.contains(&b64(&[0xCCu8; 32])));

    let (_, members) = get(&pool, &owner, &format!("/orgs/{o}/members")).await;
    assert!(members.contains("gr-inv-target"));

    // Re-inviting the same user conflicts; an unknown email is 404.
    assert_eq!(
        post(
            &pool,
            &owner,
            &invites_uri,
            r#"{"email":"gr-inv-target@example.test"}"#.into()
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        post(
            &pool,
            &owner,
            &invites_uri,
            r#"{"email":"nobody@example.test"}"#.into()
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn invite_requires_admin() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let o = "gr-invadm-o";
    reset_orgs(&pool, &[o]).await;
    let owner = fresh_session(&pool, "gr-invadm-owner", "gr-invadm-owner-s").await;
    let member = fresh_session(&pool, "gr-invadm-member", "gr-invadm-member-s").await;
    post(&pool, &owner, "/orgs", org_body(o)).await;
    post(
        &pool,
        &owner,
        &format!("/orgs/{o}/members"),
        member_body("gr-invadm-member", "member"),
    )
    .await;

    assert_eq!(
        post(
            &pool,
            &member,
            &format!("/orgs/{o}/invites"),
            r#"{"email":"whoever@example.test"}"#.into()
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}
