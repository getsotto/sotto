//! The org audit log (M6 PR4): every team state change lands as an append-only event, readable
//! newest-first by admins/owners only. DB-gated like the other server tests.

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
        .merge(sotto_server::audit::router())
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

#[tokio::test]
async fn team_state_changes_are_audited_in_order() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let (o, p, e) = ("aud-o", "aud-p", "aud-e");
    reset_orgs(&pool, &[o]).await;
    let owner = fresh_session(&pool, "aud-owner", "aud-owner-s").await;
    let _member = fresh_session(&pool, "aud-member", "aud-member-s").await;

    // Drive one of everything.
    send(
        &pool,
        "POST",
        "/orgs",
        &owner,
        Some(format!(
            r#"{{"id":"{o}","enc_name":"{}","enc_org_key":"{}"}}"#,
            b64(b"org"),
            b64(b"ok"),
        )),
    )
    .await;
    send(
        &pool,
        "POST",
        &format!("/orgs/{o}/members"),
        &owner,
        Some(r#"{"user_id":"aud-member","role":"member"}"#.into()),
    )
    .await;
    send(
        &pool,
        "POST",
        &format!("/orgs/{o}/members/aud-member/org-key"),
        &owner,
        Some(format!(r#"{{"enc_org_key":"{}"}}"#, b64(b"mk"))),
    )
    .await;
    send(
        &pool,
        "POST",
        "/projects",
        &owner,
        Some(format!(
            r#"{{"id":"{p}","enc_name":"{}","org_id":"{o}"}}"#,
            b64(b"p")
        )),
    )
    .await;
    send(
        &pool,
        "POST",
        &format!("/projects/{p}/environments"),
        &owner,
        Some(format!(
            r#"{{"id":"{e}","enc_name":"{}","enc_vault_key":"{}"}}"#,
            b64(b"e"),
            b64(b"vk"),
        )),
    )
    .await;
    send(
        &pool,
        "POST",
        &format!("/environments/{e}/secrets"),
        &owner,
        Some(format!(
            r#"{{"base_revision":0,"changes":[{{"id":"aud-s1","op":"set","version":1,"enc_name":"{}","enc_value":"{}","enc_data_key":"{}"}}]}}"#,
            b64(b"n"),
            b64(b"v"),
            b64(b"dk"),
        )),
    )
    .await;
    send(
        &pool,
        "POST",
        &format!("/environments/{e}/grants"),
        &owner,
        Some(format!(
            r#"{{"user_id":"aud-member","enc_vault_key":"{}"}}"#,
            b64(b"mg")
        )),
    )
    .await;
    let (_, created) = send(
        &pool,
        "POST",
        &format!("/environments/{e}/tokens"),
        &owner,
        Some(format!(
            r#"{{"name":"ci","public_key":"{}","enc_vault_key":"{}"}}"#,
            b64(&[0xAB; 32]),
            b64(b"tg"),
        )),
    )
    .await;
    let token_id = serde_json::from_str::<serde_json::Value>(&created).expect("json")["token_id"]
        .as_str()
        .expect("token_id")
        .to_string();
    send(
        &pool,
        "DELETE",
        &format!("/environments/{e}/tokens/{token_id}"),
        &owner,
        None,
    )
    .await;
    send(
        &pool,
        "POST",
        &format!("/environments/{e}/rotate"),
        &owner,
        Some(format!(
            r#"{{"base_revision":1,"grants":[{{"user_id":"aud-owner","enc_vault_key":"{}"}},{{"user_id":"aud-member","enc_vault_key":"{}"}}],"data_keys":[{{"secret_id":"aud-s1","enc_data_key":"{}"}}],"history_keys":[{{"secret_id":"aud-s1","version":1,"enc_data_key":"{}"}}]}}"#,
            b64(b"og2"),
            b64(b"mg2"),
            b64(b"dk2"),
            b64(b"hk2"),
        )),
    )
    .await;
    send(
        &pool,
        "POST",
        &format!("/orgs/{o}/members/aud-member"),
        &owner,
        Some(r#"{"role":"admin"}"#.into()),
    )
    .await;
    send(
        &pool,
        "DELETE",
        &format!("/orgs/{o}/members/aud-member"),
        &owner,
        None,
    )
    .await;

    // The log holds every action, newest first.
    let (status, body) = send(&pool, "GET", &format!("/orgs/{o}/audit"), &owner, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let events: Vec<serde_json::Value> = serde_json::from_str(&body).expect("json");
    let actions: Vec<&str> = events
        .iter()
        .map(|e| e["action"].as_str().expect("action"))
        .collect();
    assert_eq!(
        actions,
        vec![
            "member.removed",
            "member.role_changed",
            "env.rotated",
            "token.revoked",
            "token.created",
            "env.shared",
            "secrets.written",
            "member.org_key_granted",
            "member.added",
            "org.created",
        ]
    );
    // Events carry their context.
    let rotated = events
        .iter()
        .find(|e| e["action"] == "env.rotated")
        .unwrap();
    assert_eq!(rotated["actor"], "aud-owner");
    assert_eq!(rotated["env_id"], e);
    assert!(rotated["detail"]
        .as_str()
        .unwrap()
        .contains("2 member grant(s)"));

    // The limit is honored.
    let (_, body) = send(
        &pool,
        "GET",
        &format!("/orgs/{o}/audit?limit=2"),
        &owner,
        None,
    )
    .await;
    let events: Vec<serde_json::Value> = serde_json::from_str(&body).expect("json");
    assert_eq!(events.len(), 2);
}

#[tokio::test]
async fn audit_read_is_admin_only() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let o = "aud-ro-o";
    reset_orgs(&pool, &[o]).await;
    let owner = fresh_session(&pool, "aud-ro-owner", "aud-ro-owner-s").await;
    let member = fresh_session(&pool, "aud-ro-member", "aud-ro-member-s").await;
    let outsider = fresh_session(&pool, "aud-ro-out", "aud-ro-out-s").await;
    send(
        &pool,
        "POST",
        "/orgs",
        &owner,
        Some(format!(
            r#"{{"id":"{o}","enc_name":"{}","enc_org_key":"{}"}}"#,
            b64(b"org"),
            b64(b"ok"),
        )),
    )
    .await;
    send(
        &pool,
        "POST",
        &format!("/orgs/{o}/members"),
        &owner,
        Some(r#"{"user_id":"aud-ro-member","role":"member"}"#.into()),
    )
    .await;

    let uri = format!("/orgs/{o}/audit");
    assert_eq!(
        send(&pool, "GET", &uri, &owner, None).await.0,
        StatusCode::OK
    );
    assert_eq!(
        send(&pool, "GET", &uri, &member, None).await.0,
        StatusCode::FORBIDDEN,
        "plain members cannot read the audit log"
    );
    assert_eq!(
        send(&pool, "GET", &uri, &outsider, None).await.0,
        StatusCode::NOT_FOUND,
        "non-members don't learn the org exists"
    );
}
