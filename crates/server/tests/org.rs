//! Organization / membership / role integration tests.
//!
//! DB-gated like the other server tests. Each test uses fixed, test-scoped ids so parallel runs
//! don't collide, and pre-cleans by deleting its orgs (cascading memberships) and re-minting its
//! acting users, so reruns are idempotent.

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
        .with_state(state)
}

/// Delete the named orgs (cascading their memberships), so a rerun starts clean.
async fn reset_orgs(pool: &PgPool, orgs: &[&str]) {
    for org in orgs {
        sqlx::query("DELETE FROM organizations WHERE id = $1")
            .bind(org)
            .execute(pool)
            .await
            .expect("reset org");
    }
}

/// Re-mint an acting user (fresh, with a session token).
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

/// Ensure a plain user row exists (a membership target that never acts, so needs no session).
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

async fn get(pool: &PgPool, token: Option<&str>, uri: &str) -> (StatusCode, String) {
    request(pool, "GET", uri, token, None).await
}

async fn delete(pool: &PgPool, token: &str, uri: &str) -> (StatusCode, String) {
    request(pool, "DELETE", uri, Some(token), None).await
}

fn org_body(id: &str) -> String {
    format!(
        r#"{{"id":"{id}","enc_name":"{}","enc_org_key":"{}"}}"#,
        STANDARD.encode(b"org"),
        STANDARD.encode(b"sealed-org-key"),
    )
}

fn member_body(user_id: &str, role: &str) -> String {
    format!(r#"{{"user_id":"{user_id}","role":"{role}"}}"#)
}

fn role_body(role: &str) -> String {
    format!(r#"{{"role":"{role}"}}"#)
}

#[tokio::test]
async fn create_and_list_orgs() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let org = "org-create-o";
    reset_orgs(&pool, &[org]).await;
    let owner = fresh_session(&pool, "org-create-u", "org-create-s").await;

    assert_eq!(
        post(&pool, &owner, "/orgs", org_body(org)).await.0,
        StatusCode::CREATED
    );
    // Idempotent re-create of one's own org.
    assert_eq!(
        post(&pool, &owner, "/orgs", org_body(org)).await.0,
        StatusCode::OK
    );

    let (status, body) = get(&pool, Some(&owner), "/orgs").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(org));
    assert!(body.contains("\"role\":\"owner\""));
    assert!(body.contains(&STANDARD.encode(b"org")));
}

#[tokio::test]
async fn create_conflicts_for_another_user() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let org = "org-conflict-o";
    reset_orgs(&pool, &[org]).await;
    let owner = fresh_session(&pool, "org-conflict-a", "org-conflict-a-s").await;
    let other = fresh_session(&pool, "org-conflict-b", "org-conflict-b-s").await;

    assert_eq!(
        post(&pool, &owner, "/orgs", org_body(org)).await.0,
        StatusCode::CREATED
    );
    // A different user cannot claim an id already in use.
    assert_eq!(
        post(&pool, &other, "/orgs", org_body(org)).await.0,
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn non_member_cannot_see_or_manage() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let org = "org-nonmem-o";
    reset_orgs(&pool, &[org]).await;
    let owner = fresh_session(&pool, "org-nonmem-a", "org-nonmem-a-s").await;
    let intruder = fresh_session(&pool, "org-nonmem-b", "org-nonmem-b-s").await;
    post(&pool, &owner, "/orgs", org_body(org)).await;

    let members_uri = format!("/orgs/{org}/members");
    // A non-member gets 404 (the org's existence is not leaked), for both read and write.
    assert_eq!(
        get(&pool, Some(&intruder), &members_uri).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        post(
            &pool,
            &intruder,
            &members_uri,
            member_body("org-nonmem-a", "member")
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        delete(&pool, &intruder, &format!("/orgs/{org}")).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn admins_manage_members_but_plain_members_cannot() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let org = "org-manage-o";
    reset_orgs(&pool, &[org]).await;
    let owner = fresh_session(&pool, "org-manage-owner", "org-manage-owner-s").await;
    let admin = fresh_session(&pool, "org-manage-admin", "org-manage-admin-s").await;
    let plain = fresh_session(&pool, "org-manage-member", "org-manage-member-s").await;
    ensure_user(&pool, "org-manage-target", "org-manage-target-s").await;
    post(&pool, &owner, "/orgs", org_body(org)).await;
    let members_uri = format!("/orgs/{org}/members");

    assert_eq!(
        post(
            &pool,
            &owner,
            &members_uri,
            member_body("org-manage-admin", "admin")
        )
        .await
        .0,
        StatusCode::CREATED
    );
    assert_eq!(
        post(
            &pool,
            &owner,
            &members_uri,
            member_body("org-manage-member", "member")
        )
        .await
        .0,
        StatusCode::CREATED
    );

    // A plain member may not add members; an admin may.
    assert_eq!(
        post(
            &pool,
            &plain,
            &members_uri,
            member_body("org-manage-target", "member")
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        post(
            &pool,
            &admin,
            &members_uri,
            member_body("org-manage-target", "member")
        )
        .await
        .0,
        StatusCode::CREATED
    );

    // Re-adding an existing member is a conflict (use update to change role).
    assert_eq!(
        post(
            &pool,
            &admin,
            &members_uri,
            member_body("org-manage-target", "admin")
        )
        .await
        .0,
        StatusCode::CONFLICT
    );

    let (status, body) = get(&pool, Some(&owner), &members_uri).await;
    assert_eq!(status, StatusCode::OK);
    for who in [
        "org-manage-owner",
        "org-manage-admin",
        "org-manage-member",
        "org-manage-target",
    ] {
        assert!(body.contains(who), "members list should contain {who}");
    }
}

#[tokio::test]
async fn only_owner_can_grant_owner_role() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let org = "org-grant-o";
    reset_orgs(&pool, &[org]).await;
    let owner = fresh_session(&pool, "org-grant-owner", "org-grant-owner-s").await;
    let admin = fresh_session(&pool, "org-grant-admin", "org-grant-admin-s").await;
    ensure_user(&pool, "org-grant-target", "org-grant-target-s").await;
    post(&pool, &owner, "/orgs", org_body(org)).await;
    let members_uri = format!("/orgs/{org}/members");
    post(
        &pool,
        &owner,
        &members_uri,
        member_body("org-grant-admin", "admin"),
    )
    .await;

    // An admin cannot mint another owner…
    assert_eq!(
        post(
            &pool,
            &admin,
            &members_uri,
            member_body("org-grant-target", "owner")
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    // …but the owner can.
    assert_eq!(
        post(
            &pool,
            &owner,
            &members_uri,
            member_body("org-grant-target", "owner")
        )
        .await
        .0,
        StatusCode::CREATED
    );
}

#[tokio::test]
async fn last_owner_cannot_be_demoted_or_removed() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let org = "org-lastowner-o";
    reset_orgs(&pool, &[org]).await;
    let owner = fresh_session(&pool, "org-lastowner-a", "org-lastowner-a-s").await;
    ensure_user(&pool, "org-lastowner-b", "org-lastowner-b-s").await;
    post(&pool, &owner, "/orgs", org_body(org)).await;
    let self_uri = format!("/orgs/{org}/members/org-lastowner-a");

    // The sole owner may not demote or remove themselves.
    assert_eq!(
        post(&pool, &owner, &self_uri, role_body("admin")).await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        delete(&pool, &owner, &self_uri).await.0,
        StatusCode::CONFLICT
    );

    // With a second owner in place, the first can be demoted.
    post(
        &pool,
        &owner,
        &format!("/orgs/{org}/members"),
        member_body("org-lastowner-b", "owner"),
    )
    .await;
    assert_eq!(
        post(&pool, &owner, &self_uri, role_body("admin")).await.0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn admin_cannot_touch_an_owner() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let org = "org-touch-o";
    reset_orgs(&pool, &[org]).await;
    let owner = fresh_session(&pool, "org-touch-owner", "org-touch-owner-s").await;
    let admin = fresh_session(&pool, "org-touch-admin", "org-touch-admin-s").await;
    post(&pool, &owner, "/orgs", org_body(org)).await;
    post(
        &pool,
        &owner,
        &format!("/orgs/{org}/members"),
        member_body("org-touch-admin", "admin"),
    )
    .await;
    let owner_uri = format!("/orgs/{org}/members/org-touch-owner");

    // An admin can manage members but not an owner.
    assert_eq!(
        post(&pool, &admin, &owner_uri, role_body("member")).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        delete(&pool, &admin, &owner_uri).await.0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn update_then_remove_member() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let org = "org-upd-o";
    reset_orgs(&pool, &[org]).await;
    let owner = fresh_session(&pool, "org-upd-owner", "org-upd-owner-s").await;
    ensure_user(&pool, "org-upd-member", "org-upd-member-s").await;
    post(&pool, &owner, "/orgs", org_body(org)).await;
    let members_uri = format!("/orgs/{org}/members");
    post(
        &pool,
        &owner,
        &members_uri,
        member_body("org-upd-member", "member"),
    )
    .await;
    let member_uri = format!("/orgs/{org}/members/org-upd-member");

    // Promote to admin, then remove.
    assert_eq!(
        post(&pool, &owner, &member_uri, role_body("admin")).await.0,
        StatusCode::OK
    );
    let (_, body) = get(&pool, Some(&owner), &members_uri).await;
    assert!(body.contains("org-upd-member") && body.contains("\"role\":\"admin\""));

    assert_eq!(
        delete(&pool, &owner, &member_uri).await.0,
        StatusCode::NO_CONTENT
    );
    let (_, body) = get(&pool, Some(&owner), &members_uri).await;
    assert!(
        !body.contains("org-upd-member"),
        "removed member should be gone"
    );
}

#[tokio::test]
async fn only_owner_can_delete_org() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let org = "org-del-o";
    reset_orgs(&pool, &[org]).await;
    let owner = fresh_session(&pool, "org-del-owner", "org-del-owner-s").await;
    let admin = fresh_session(&pool, "org-del-admin", "org-del-admin-s").await;
    post(&pool, &owner, "/orgs", org_body(org)).await;
    post(
        &pool,
        &owner,
        &format!("/orgs/{org}/members"),
        member_body("org-del-admin", "admin"),
    )
    .await;

    assert_eq!(
        delete(&pool, &admin, &format!("/orgs/{org}")).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        delete(&pool, &owner, &format!("/orgs/{org}")).await.0,
        StatusCode::NO_CONTENT
    );
    // The org (and its memberships) are gone.
    let (_, body) = get(&pool, Some(&owner), "/orgs").await;
    assert!(!body.contains(org));
}

#[tokio::test]
async fn add_nonexistent_user_is_404() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let org = "org-ghost-o";
    reset_orgs(&pool, &[org]).await;
    let owner = fresh_session(&pool, "org-ghost-owner", "org-ghost-owner-s").await;
    post(&pool, &owner, "/orgs", org_body(org)).await;

    assert_eq!(
        post(
            &pool,
            &owner,
            &format!("/orgs/{org}/members"),
            member_body("org-ghost-nobody", "member")
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn org_key_is_stored_listed_and_grantable() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let org = "org-key-o";
    reset_orgs(&pool, &[org]).await;
    let owner = fresh_session(&pool, "org-key-owner", "org-key-owner-s").await;
    let member = fresh_session(&pool, "org-key-member", "org-key-member-s").await;
    post(&pool, &owner, "/orgs", org_body(org)).await;
    post(
        &pool,
        &owner,
        &format!("/orgs/{org}/members"),
        member_body("org-key-member", "member"),
    )
    .await;

    // The creator's sealed copy (from org creation) shows in their listing; the member has none.
    let (_, body) = get(&pool, Some(&owner), "/orgs").await;
    assert!(body.contains(&STANDARD.encode(b"sealed-org-key")));
    let (_, body) = get(&pool, Some(&member), "/orgs").await;
    assert!(body.contains("\"enc_org_key\":null"));

    // A plain member cannot grant the org key; an owner can, and the member then sees their copy.
    let grant_uri = format!("/orgs/{org}/members/org-key-member/org-key");
    let grant_body = format!(
        r#"{{"enc_org_key":"{}"}}"#,
        STANDARD.encode(b"member-org-key")
    );
    assert_eq!(
        post(&pool, &member, &grant_uri, grant_body.clone()).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        post(&pool, &owner, &grant_uri, grant_body).await.0,
        StatusCode::OK
    );
    let (_, body) = get(&pool, Some(&member), "/orgs").await;
    assert!(body.contains(&STANDARD.encode(b"member-org-key")));

    // Granting to a non-member is 404.
    assert_eq!(
        post(
            &pool,
            &owner,
            &format!("/orgs/{org}/members/org-key-nobody/org-key"),
            format!(r#"{{"enc_org_key":"{}"}}"#, STANDARD.encode(b"x")),
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn auth_is_required() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    assert_eq!(get(&pool, None, "/orgs").await.0, StatusCode::UNAUTHORIZED);
}
