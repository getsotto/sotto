//! Account reset (M5 PR6a): the recovery path for a user who lost their Emergency Kit.
//!
//! `POST /account/reset` replaces the account's crypto material and deletes the user's now-dead
//! environment grants in one transaction. DB-gated like the other server tests.

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
        billing: None,
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
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    sqlx::query("DELETE FROM organizations WHERE id = 'rec-o'")
        .execute(&pool)
        .await
        .unwrap();
    let owner = fresh_session(&pool, "rec-owner", "rec-owner-s").await;
    let member = fresh_session(&pool, "rec-member", "rec-member-s").await;

    // Both users initialize accounts; the owner stands up an org env and grants the member.
    for (token, tag) in [(&owner, "own"), (&member, "old")] {
        let (status, body) = send(&pool, "PUT", "/account", token, Some(bundle_body(tag))).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    // Assert each setup step succeeds, so a later auth/validation/schema change surfaces here with a
    // clear status + body rather than as a confusing failure in the reset assertions below.
    let expect_ok = |label: &str, (status, body): (StatusCode, String)| {
        assert!(status.is_success(), "{label} failed: {status} — {body}");
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
    // Membership itself survives — the admin re-grants rather than re-invites.
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
