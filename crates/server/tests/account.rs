//! Account crypto-material sync integration tests.
//!
//! DB-gated like the others: skips when `DATABASE_URL` is unset. Each test uses a fixed marker
//! user id and cleans up, so reruns are idempotent.

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

/// Account endpoints don't use OAuth, so the provider/config can be absent.
fn app(pool: PgPool) -> Router {
    let state = AppState {
        telemetry_ingest: false,
        pool,
        oauth: None,
        oauth_config: None,
        billing: None,
    };
    Router::new()
        .merge(sotto_server::account::router())
        .with_state(state)
}

/// Create a fresh, uninitialized user and return a valid session token for it.
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

async fn cleanup(pool: &PgPool, user_id: &str) {
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("clean");
}

fn bundle_json(pk: &[u8], epk: &[u8], kdf: &[u8], rec: &[u8]) -> String {
    format!(
        r#"{{"public_key":"{}","enc_private_keys":"{}","kdf_params":"{}","recovery_blob":"{}"}}"#,
        STANDARD.encode(pk),
        STANDARD.encode(epk),
        STANDARD.encode(kdf),
        STANDARD.encode(rec),
    )
}

fn put_req(token: Option<&str>, body: String) -> Request<Body> {
    let mut builder = Request::builder()
        .method("PUT")
        .uri("/account")
        .header("content-type", "application/json");
    if let Some(t) = token {
        builder = builder.header("authorization", format!("Bearer {t}"));
    }
    builder.body(Body::from(body)).expect("request")
}

fn get_req(token: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().method("GET").uri("/account");
    if let Some(t) = token {
        builder = builder.header("authorization", format!("Bearer {t}"));
    }
    builder.body(Body::empty()).expect("request")
}

async fn body_text(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body");
    String::from_utf8(bytes.to_vec()).expect("utf8")
}

#[tokio::test]
async fn put_then_get_round_trips() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let user_id = "test-acct-rt-user";
    let token = fresh_session(&pool, user_id, "test-acct-rt-subject").await;

    let pk = [7u8; 32];
    let epk = b"sealed-private-keys".to_vec();
    let kdf = b"kdf-params-including-salt".to_vec();
    let rec = b"emergency-kit-recovery-blob".to_vec();

    let resp = app(pool.clone())
        .oneshot(put_req(Some(&token), bundle_json(&pk, &epk, &kdf, &rec)))
        .await
        .expect("put");
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = app(pool.clone())
        .oneshot(get_req(Some(&token)))
        .await
        .expect("get");
    assert_eq!(resp.status(), StatusCode::OK);
    // Assert the exact payload, not just that each blob appears somewhere: this catches swapped,
    // duplicated, missing, or extra fields. The response is compact serde_json in struct-field
    // order, which is byte-identical to `bundle_json`.
    assert_eq!(body_text(resp).await, bundle_json(&pk, &epk, &kdf, &rec));

    cleanup(&pool, user_id).await;
}

#[tokio::test]
async fn second_put_conflicts() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let user_id = "test-acct-conflict-user";
    let token = fresh_session(&pool, user_id, "test-acct-conflict-subject").await;
    let body = bundle_json(&[1u8; 32], b"a", b"b", b"c");

    let resp = app(pool.clone())
        .oneshot(put_req(Some(&token), body.clone()))
        .await
        .expect("put1");
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = app(pool.clone())
        .oneshot(put_req(Some(&token), body))
        .await
        .expect("put2");
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    cleanup(&pool, user_id).await;
}

#[tokio::test]
async fn get_before_init_is_not_found() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let user_id = "test-acct-uninit-user";
    let token = fresh_session(&pool, user_id, "test-acct-uninit-subject").await;

    let resp = app(pool.clone())
        .oneshot(get_req(Some(&token)))
        .await
        .expect("get");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    cleanup(&pool, user_id).await;
}

#[tokio::test]
async fn invalid_public_key_length_is_rejected() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let user_id = "test-acct-badpk-user";
    let token = fresh_session(&pool, user_id, "test-acct-badpk-subject").await;

    // 31 bytes is not a valid X25519 public key.
    let body = bundle_json(&[9u8; 31], b"a", b"b", b"c");
    let resp = app(pool.clone())
        .oneshot(put_req(Some(&token), body))
        .await
        .expect("put");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    cleanup(&pool, user_id).await;
}

#[tokio::test]
async fn auth_is_required() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let resp = app(pool.clone()).oneshot(get_req(None)).await.expect("get");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let resp = app(pool.clone())
        .oneshot(put_req(None, bundle_json(&[1u8; 32], b"a", b"b", b"c")))
        .await
        .expect("put");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
