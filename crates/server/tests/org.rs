//! Organisation / membership / role integration tests.
//!
//! DB-gated like the other server tests. Each test uses fixed, test-scoped ids so parallel runs
//! don't collide, and pre-cleans by deleting its orgs (cascading memberships) and re-minting its
//! acting users, so reruns are idempotent.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;

use sotto_server::auth::session;
use sotto_server::config::DEFAULT_ORGANISATION_DELETION_RETENTION_DAYS;
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
        organisation_deletion_enabled: false,
        organisation_deletion_retention_days: DEFAULT_ORGANISATION_DELETION_RETENTION_DAYS,
        organisation_deletion_metrics_token: None,
        organisation_deletion_operator_token: None,
    };
    Router::new()
        .merge(sotto_server::org::router())
        .merge(sotto_server::machine::router())
        .merge(sotto_server::sync::router())
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
    // A non-member gets 404 (the org's existence is not leaked) for member reads and writes.
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

    // Removal returns a receipt (here empty: the member held no grants and created no tokens).
    let (status, body) = delete(&pool, &owner, &member_uri).await;
    assert_eq!(status, StatusCode::OK);
    let receipt: Value = serde_json::from_str(&body).expect("receipt json");
    assert_eq!(receipt["grants_deleted"], 0);
    assert_eq!(receipt["revoked_tokens"].as_array().unwrap().len(), 0);
    let (_, body) = get(&pool, Some(&owner), &members_uri).await;
    assert!(
        !body.contains("org-upd-member"),
        "removed member should be gone"
    );
}

#[tokio::test]
async fn org_deletion_is_not_exposed() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let org = "org-del-o";
    reset_orgs(&pool, &[org]).await;
    let owner = fresh_session(&pool, "org-del-owner", "org-del-owner-s").await;
    assert_eq!(
        post(&pool, &owner, "/orgs", org_body(org)).await.0,
        StatusCode::CREATED
    );

    sqlx::query(
        "UPDATE organizations SET stripe_customer_id = 'cus_org_del', \
         stripe_subscription_id = 'sub_org_del' WHERE id = $1",
    )
    .bind(org)
    .execute(&pool)
    .await
    .expect("set billing linkage");

    assert_eq!(
        delete(&pool, &owner, &format!("/orgs/{org}")).await.0,
        StatusCode::NOT_FOUND
    );

    let (customer_id, subscription_id): (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT stripe_customer_id, stripe_subscription_id FROM organizations WHERE id = $1",
    )
    .bind(org)
    .fetch_one(&pool)
    .await
    .expect("organisation remains");
    assert_eq!(customer_id.as_deref(), Some("cus_org_del"));
    assert_eq!(subscription_id.as_deref(), Some("sub_org_del"));

    let (_, body) = get(&pool, Some(&owner), "/orgs").await;
    assert!(body.contains(org));
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

// --- S-06: removal revokes grants and tokens ---------------------------------------------

fn org_project_body(id: &str, org_id: &str) -> String {
    format!(
        r#"{{"id":"{id}","enc_name":"{}","org_id":"{org_id}"}}"#,
        STANDARD.encode(b"project")
    )
}

fn personal_project_body(id: &str) -> String {
    format!(
        r#"{{"id":"{id}","enc_name":"{}"}}"#,
        STANDARD.encode(b"project")
    )
}

fn env_body(id: &str) -> String {
    format!(
        r#"{{"id":"{id}","enc_name":"{}","enc_vault_key":"{}"}}"#,
        STANDARD.encode(b"env"),
        STANDARD.encode(b"owner-grant"),
    )
}

fn grant_body(user_id: &str) -> String {
    format!(
        r#"{{"user_id":"{user_id}","enc_vault_key":"{}"}}"#,
        STANDARD.encode(b"sealed-grant"),
    )
}

fn token_body(name: &str) -> String {
    format!(
        r#"{{"name":"{name}","public_key":"{}","enc_vault_key":"{}"}}"#,
        STANDARD.encode([0xAB; 32]),
        STANDARD.encode(b"machine-grant"),
    )
}

/// Owner + org + org project + env (the owner holds a grant as creator); returns the session.
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
    owner
}

#[tokio::test]
async fn removal_revokes_grants_and_tokens_with_receipt() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let (o, p, e) = ("rm-rev-o", "rm-rev-p", "rm-rev-e");
    let owner = seed_org_env(&pool, o, p, e, "rm-rev-owner").await;
    let target = fresh_session(&pool, "rm-rev-target", "rm-rev-target-s").await;
    // The target is an admin (so they can create tokens) with a grant on the env.
    post(
        &pool,
        &owner,
        &format!("/orgs/{o}/members"),
        member_body("rm-rev-target", "admin"),
    )
    .await;
    assert_eq!(
        post(
            &pool,
            &owner,
            &format!("/environments/{e}/grants"),
            grant_body("rm-rev-target")
        )
        .await
        .0,
        StatusCode::OK
    );
    let (status, body) = post(
        &pool,
        &target,
        &format!("/environments/{e}/tokens"),
        token_body("target-ci"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create token: {body}");
    let created: Value = serde_json::from_str(&body).expect("token json");
    let token_id = created["token_id"].as_str().expect("token_id").to_string();
    let api_token = created["token"].as_str().expect("token").to_string();
    assert_eq!(
        get(&pool, Some(&api_token), "/machine/grant").await.0,
        StatusCode::OK,
        "token works before removal"
    );

    // Removal revokes everything and returns a receipt naming what died.
    let (status, body) = delete(&pool, &owner, &format!("/orgs/{o}/members/rm-rev-target")).await;
    assert_eq!(status, StatusCode::OK, "removal: {body}");
    let receipt: Value = serde_json::from_str(&body).expect("receipt json");
    assert_eq!(receipt["grants_deleted"], 1);
    let revoked = receipt["revoked_tokens"].as_array().expect("revoked array");
    assert_eq!(revoked.len(), 1);
    assert_eq!(revoked[0]["token_id"].as_str(), Some(token_id.as_str()));
    assert_eq!(revoked[0]["name"].as_str(), Some("target-ci"));
    assert_eq!(revoked[0]["env_id"].as_str(), Some(e));

    // The grant row is gone, the token 401s, and it left the active listing.
    let grants: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM environment_grants WHERE env_id = $1 AND user_id = $2",
    )
    .bind(e)
    .bind("rm-rev-target")
    .fetch_one(&pool)
    .await
    .expect("count grants");
    assert_eq!(grants, 0, "the removed member's grant row is gone");
    assert_eq!(
        get(&pool, Some(&api_token), "/machine/grant").await.0,
        StatusCode::UNAUTHORIZED,
        "the removed member's token 401s"
    );
    let (status, body) = get(&pool, Some(&owner), &format!("/environments/{e}/tokens")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains(&token_id), "revoked token left the listing");

    // The trail shows what was revoked: one `token.revoked` plus counts on `member.removed`.
    let events: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT action, target, detail FROM audit_events WHERE org_id = $1 AND action IN \
         ('member.removed', 'token.revoked') ORDER BY id",
    )
    .bind(o)
    .fetch_all(&pool)
    .await
    .expect("audit events");
    assert!(
        events
            .iter()
            .any(|(action, target, _)| action == "token.revoked"
                && target.as_deref() == Some(token_id.as_str())),
        "a token.revoked event names the token: {events:?}"
    );
    let removed = events
        .iter()
        .find(|(action, _, _)| action == "member.removed");
    assert!(removed.is_some(), "member.removed is audited");
    assert!(
        removed
            .unwrap()
            .2
            .as_deref()
            .unwrap_or("")
            .contains("revoked 1 token(s)"),
        "member.removed carries the counts: {events:?}"
    );
}

#[tokio::test]
async fn removal_fails_when_the_caller_cannot_rekey() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let (o, p, e) = ("rm-rekey-o", "rm-rekey-p", "rm-rekey-e");
    let owner = seed_org_env(&pool, o, p, e, "rm-rekey-owner").await;
    // An admin who holds no grant to the env, and a member who does.
    let admin = fresh_session(&pool, "rm-rekey-admin", "rm-rekey-admin-s").await;
    ensure_user(&pool, "rm-rekey-member", "rm-rekey-member-s").await;
    post(
        &pool,
        &owner,
        &format!("/orgs/{o}/members"),
        member_body("rm-rekey-admin", "admin"),
    )
    .await;
    post(
        &pool,
        &owner,
        &format!("/orgs/{o}/members"),
        member_body("rm-rekey-member", "member"),
    )
    .await;
    assert_eq!(
        post(
            &pool,
            &owner,
            &format!("/environments/{e}/grants"),
            grant_body("rm-rekey-member")
        )
        .await
        .0,
        StatusCode::OK
    );

    // The admin cannot re-key the member's env, so removal fails naming it - and nothing is
    // half-revoked: the membership and the grant both survive.
    let (status, body) = delete(&pool, &admin, &format!("/orgs/{o}/members/rm-rekey-member")).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "removal without re-key: {body}"
    );
    assert!(body.contains(e), "the 409 names the env: {body}");
    // Rotating would not unblock a retry (it preserves every holder), so the recovery the 409
    // prescribes must be sharing or handing the removal over.
    assert!(body.contains("share"), "the 409 points at sharing: {body}");
    assert!(
        !body.contains("rotate"),
        "the 409 must not prescribe a futile rotation: {body}"
    );
    let (_, body) = get(&pool, Some(&owner), &format!("/orgs/{o}/members")).await;
    assert!(body.contains("rm-rekey-member"), "membership retained");
    let grants: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM environment_grants WHERE env_id = $1 AND user_id = $2",
    )
    .bind(e)
    .bind("rm-rekey-member")
    .fetch_one(&pool)
    .await
    .expect("count grants");
    assert_eq!(grants, 1, "grant retained");
}

#[tokio::test]
async fn readded_member_starts_grantless() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let (o, p, e) = ("rm-readd-o", "rm-readd-p", "rm-readd-e");
    let owner = seed_org_env(&pool, o, p, e, "rm-readd-owner").await;
    let target = fresh_session(&pool, "rm-readd-target", "rm-readd-target-s").await;
    post(
        &pool,
        &owner,
        &format!("/orgs/{o}/members"),
        member_body("rm-readd-target", "member"),
    )
    .await;
    assert_eq!(
        post(
            &pool,
            &owner,
            &format!("/environments/{e}/grants"),
            grant_body("rm-readd-target")
        )
        .await
        .0,
        StatusCode::OK
    );

    assert_eq!(
        delete(&pool, &owner, &format!("/orgs/{o}/members/rm-readd-target"))
            .await
            .0,
        StatusCode::OK
    );
    // Re-adding restores membership but no decryption capability: no grant row, and the grant
    // endpoint 404s until an admin explicitly re-shares.
    assert_eq!(
        post(
            &pool,
            &owner,
            &format!("/orgs/{o}/members"),
            member_body("rm-readd-target", "member"),
        )
        .await
        .0,
        StatusCode::CREATED
    );
    let grants: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM environment_grants WHERE env_id = $1 AND user_id = $2",
    )
    .bind(e)
    .bind("rm-readd-target")
    .fetch_one(&pool)
    .await
    .expect("count grants");
    assert_eq!(grants, 0, "re-added member has no grant");
    assert_eq!(
        get(&pool, Some(&target), &format!("/environments/{e}/grant"))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn an_env_only_the_target_holds_does_not_block_removal() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let (o, p, e) = ("rm-sole-o", "rm-sole-p", "rm-sole-e");
    let owner = seed_org_env(&pool, o, p, e, "rm-sole-owner").await;
    let target = fresh_session(&pool, "rm-sole-target", "rm-sole-target-s").await;
    post(
        &pool,
        &owner,
        &format!("/orgs/{o}/members"),
        member_body("rm-sole-target", "admin"),
    )
    .await;
    // The target creates their own project and env in the org, so they alone hold its grant.
    let (tp, te) = ("rm-sole-tp", "rm-sole-te");
    post(&pool, &target, "/projects", org_project_body(tp, o)).await;
    post(
        &pool,
        &target,
        &format!("/projects/{tp}/environments"),
        env_body(te),
    )
    .await;
    let holders: Vec<String> =
        sqlx::query_scalar("SELECT user_id FROM environment_grants WHERE env_id = $1")
            .bind(te)
            .fetch_all(&pool)
            .await
            .expect("holders");
    assert_eq!(holders, vec!["rm-sole-target".to_string()], "precondition");

    // The owner cannot open it, and nobody else ever could, so there is nothing to re-key and
    // nobody to protect: the removal goes through instead of 409ing forever, and the grant still
    // dies with the membership.
    let (status, body) = delete(&pool, &owner, &format!("/orgs/{o}/members/rm-sole-target")).await;
    assert_eq!(status, StatusCode::OK, "removal: {body}");
    let receipt: Value = serde_json::from_str(&body).expect("receipt json");
    assert_eq!(receipt["grants_deleted"], 1);
    let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM environment_grants WHERE env_id = $1")
        .bind(te)
        .fetch_one(&pool)
        .await
        .expect("count grants");
    assert_eq!(left, 0, "no grant to the env survives the removal");
}

/// A removal from before the revocation fix deleted the membership but left the grant row. That
/// is the stale state both rejoin tests below start from.
async fn seed_stale_grant(pool: &PgPool, o: &str, e: &str, target_id: &str) {
    sqlx::query("DELETE FROM organization_memberships WHERE org_id = $1 AND user_id = $2")
        .bind(o)
        .bind(target_id)
        .execute(pool)
        .await
        .expect("simulate a pre-fix removal");
    let grants: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM environment_grants WHERE env_id = $1 AND user_id = $2",
    )
    .bind(e)
    .bind(target_id)
    .fetch_one(pool)
    .await
    .expect("count grants");
    assert_eq!(grants, 1, "precondition: the stale grant row survived");
}

#[tokio::test]
async fn rejoining_by_add_wipes_stale_grants() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let (o, p, e) = ("rm-stale-o", "rm-stale-p", "rm-stale-e");
    let owner = seed_org_env(&pool, o, p, e, "rm-stale-owner").await;
    let target = fresh_session(&pool, "rm-stale-target", "rm-stale-target-s").await;
    post(
        &pool,
        &owner,
        &format!("/orgs/{o}/members"),
        member_body("rm-stale-target", "member"),
    )
    .await;
    assert_eq!(
        post(
            &pool,
            &owner,
            &format!("/environments/{e}/grants"),
            grant_body("rm-stale-target")
        )
        .await
        .0,
        StatusCode::OK
    );
    seed_stale_grant(&pool, o, e, "rm-stale-target").await;

    // Re-adding wipes the stale row instead of silently restoring the old vault key.
    assert_eq!(
        post(
            &pool,
            &owner,
            &format!("/orgs/{o}/members"),
            member_body("rm-stale-target", "member"),
        )
        .await
        .0,
        StatusCode::CREATED
    );
    let grants: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM environment_grants WHERE env_id = $1 AND user_id = $2",
    )
    .bind(e)
    .bind("rm-stale-target")
    .fetch_one(&pool)
    .await
    .expect("count grants");
    assert_eq!(grants, 0, "re-adding wipes the stale grant");
    assert_eq!(
        get(&pool, Some(&target), &format!("/environments/{e}/grant"))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn rejoining_by_invite_wipes_stale_grants() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let (o, p, e) = ("rm-stinv-o", "rm-stinv-p", "rm-stinv-e");
    let owner = seed_org_env(&pool, o, p, e, "rm-stinv-owner").await;
    let target = fresh_session(&pool, "rm-stinv-target", "rm-stinv-target-s").await;
    sqlx::query("UPDATE users SET email = $2 WHERE id = $1")
        .bind("rm-stinv-target")
        .bind("rm-stinv-target@example.test")
        .execute(&pool)
        .await
        .expect("set email");
    post(
        &pool,
        &owner,
        &format!("/orgs/{o}/members"),
        member_body("rm-stinv-target", "member"),
    )
    .await;
    assert_eq!(
        post(
            &pool,
            &owner,
            &format!("/environments/{e}/grants"),
            grant_body("rm-stinv-target")
        )
        .await
        .0,
        StatusCode::OK
    );
    seed_stale_grant(&pool, o, e, "rm-stinv-target").await;

    // The invite path back in wipes the stale row too.
    let (status, _) = post(
        &pool,
        &owner,
        &format!("/orgs/{o}/invites"),
        r#"{"email":"rm-stinv-target@example.test"}"#.to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let grants: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM environment_grants WHERE env_id = $1 AND user_id = $2",
    )
    .bind(e)
    .bind("rm-stinv-target")
    .fetch_one(&pool)
    .await
    .expect("count grants");
    assert_eq!(grants, 0, "inviting back wipes the stale grant");
    assert_eq!(
        get(&pool, Some(&target), &format!("/environments/{e}/grant"))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn removal_ignores_a_departed_users_grant_when_checking_for_peers() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let (o, p, e) = ("rm-peer-o", "rm-peer-p", "rm-peer-e");
    let owner = seed_org_env(&pool, o, p, e, "rm-peer-owner").await;
    // An admin who holds no grant to the env, a member who does, and a departed user whose
    // pre-fix removal left a stale grant row behind.
    let admin = fresh_session(&pool, "rm-peer-admin", "rm-peer-admin-s").await;
    ensure_user(&pool, "rm-peer-member", "rm-peer-member-s").await;
    ensure_user(&pool, "rm-peer-ghost", "rm-peer-ghost-s").await;
    for (user, role) in [
        ("rm-peer-admin", "admin"),
        ("rm-peer-member", "member"),
        ("rm-peer-ghost", "member"),
    ] {
        post(
            &pool,
            &owner,
            &format!("/orgs/{o}/members"),
            member_body(user, role),
        )
        .await;
    }
    for user in ["rm-peer-member", "rm-peer-ghost"] {
        assert_eq!(
            post(
                &pool,
                &owner,
                &format!("/environments/{e}/grants"),
                grant_body(user)
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    sqlx::query("DELETE FROM organization_memberships WHERE org_id = $1 AND user_id = $2")
        .bind(o)
        .bind("rm-peer-ghost")
        .execute(&pool)
        .await
        .expect("simulate a pre-fix removal");
    // The owner drops their own grant, so the target is the only remaining holder.
    sqlx::query("DELETE FROM environment_grants WHERE env_id = $1 AND user_id = $2")
        .bind(e)
        .bind("rm-peer-owner")
        .execute(&pool)
        .await
        .expect("drop the owners grant");

    // The ghost's stale row must not count as a peer: nobody remaining can re-key the env, so
    // the removal goes through instead of 409ing forever.
    let (status, body) = delete(&pool, &admin, &format!("/orgs/{o}/members/rm-peer-member")).await;
    assert_eq!(status, StatusCode::OK, "removal: {body}");
    let grants: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM environment_grants WHERE env_id = $1 AND user_id = $2",
    )
    .bind(e)
    .bind("rm-peer-member")
    .fetch_one(&pool)
    .await
    .expect("count grants");
    assert_eq!(grants, 0, "the removed member's grant is gone");
}

#[tokio::test]
async fn removal_leaves_personal_tokens_untouched() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let (o, p, e) = ("rm-pers-o", "rm-pers-p", "rm-pers-e");
    let owner = seed_org_env(&pool, o, p, e, "rm-pers-owner").await;
    let target = fresh_session(&pool, "rm-pers-target", "rm-pers-target-s").await;
    post(
        &pool,
        &owner,
        &format!("/orgs/{o}/members"),
        member_body("rm-pers-target", "admin"),
    )
    .await;
    assert_eq!(
        post(
            &pool,
            &owner,
            &format!("/environments/{e}/grants"),
            grant_body("rm-pers-target")
        )
        .await
        .0,
        StatusCode::OK
    );
    // One token on the org env, one on the target's own personal env.
    let (status, body) = post(
        &pool,
        &target,
        &format!("/environments/{e}/tokens"),
        token_body("org-ci"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create org token: {body}");
    let org_token = serde_json::from_str::<Value>(&body).expect("token json")["token"]
        .as_str()
        .expect("token")
        .to_string();
    post(
        &pool,
        &target,
        "/projects",
        personal_project_body("rm-pers-pp"),
    )
    .await;
    post(
        &pool,
        &target,
        "/projects/rm-pers-pp/environments",
        env_body("rm-pers-pe"),
    )
    .await;
    let (status, body) = post(
        &pool,
        &target,
        "/environments/rm-pers-pe/tokens",
        token_body("personal-ci"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create personal token: {body}");
    let personal_token = serde_json::from_str::<Value>(&body).expect("token json")["token"]
        .as_str()
        .expect("token")
        .to_string();

    assert_eq!(
        delete(&pool, &owner, &format!("/orgs/{o}/members/rm-pers-target"))
            .await
            .0,
        StatusCode::OK
    );
    // The org token dies with the membership; the personal one is not the org's to revoke.
    assert_eq!(
        get(&pool, Some(&org_token), "/machine/grant").await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        get(&pool, Some(&personal_token), "/machine/grant").await.0,
        StatusCode::OK,
        "a personal token survives the org removal"
    );
}
