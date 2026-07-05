//! Share-link integration tests.
//!
//! DB-gated like the others: skips when `DATABASE_URL` is unset. Creation/revocation are
//! session-gated; fetching is public. Each test uses a fixed marker user and cleans up.

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
        .merge(sotto_server::share::router())
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

/// POST /shares with a bearer token; returns (status, body).
async fn create(pool: &PgPool, token: &str, body: String) -> (StatusCode, String) {
    let req = Request::builder()
        .method("POST")
        .uri("/shares")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(body))
        .expect("req");
    let resp = app(pool.clone()).oneshot(req).await.expect("oneshot");
    let status = resp.status();
    (status, body_text(resp).await)
}

/// GET /shares/:token (public); returns (status, body).
async fn fetch(pool: &PgPool, share_token: &str) -> (StatusCode, String) {
    let req = Request::builder()
        .method("GET")
        .uri(format!("/shares/{share_token}"))
        .body(Body::empty())
        .expect("req");
    let resp = app(pool.clone()).oneshot(req).await.expect("oneshot");
    let status = resp.status();
    (status, body_text(resp).await)
}

/// Extract the `"token"` value from a CreatedShare JSON body (no serde_json dep in tests).
fn token_of(body: &str) -> String {
    let key = "\"token\":\"";
    let start = body.find(key).expect("token field") + key.len();
    let end = body[start..].find('"').expect("token end") + start;
    body[start..end].to_string()
}

fn create_body(blob: &[u8], max_views: i32, ttl: Option<i64>) -> String {
    let ttl = match ttl {
        Some(t) => t.to_string(),
        None => "null".to_string(),
    };
    format!(
        r#"{{"enc_blob":"{}","max_views":{max_views},"ttl_seconds":{ttl}}}"#,
        b64(blob)
    )
}

#[tokio::test]
async fn create_fetch_then_burn() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let session = fresh_session(&pool, "share-burn-u", "share-burn-s").await;

    let (status, body) = create(&pool, &session, create_body(b"ciphertext", 1, Some(3600))).await;
    assert_eq!(status, StatusCode::CREATED);
    let token = token_of(&body);

    // First fetch returns the ciphertext…
    let (status, body) = fetch(&pool, &token).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(&b64(b"ciphertext")));

    // …and the one-time link is now burned.
    assert_eq!(fetch(&pool, &token).await.0, StatusCode::NOT_FOUND);

    sqlx::query("DELETE FROM users WHERE id = 'share-burn-u'")
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn max_views_is_enforced() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let session = fresh_session(&pool, "share-views-u", "share-views-s").await;
    let (_, body) = create(&pool, &session, create_body(b"v", 2, None)).await;
    let token = token_of(&body);

    assert_eq!(fetch(&pool, &token).await.0, StatusCode::OK);
    assert_eq!(fetch(&pool, &token).await.0, StatusCode::OK);
    assert_eq!(fetch(&pool, &token).await.0, StatusCode::NOT_FOUND);

    sqlx::query("DELETE FROM users WHERE id = 'share-views-u'")
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn expired_link_is_not_found() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let session = fresh_session(&pool, "share-exp-u", "share-exp-s").await;
    let (_, body) = create(&pool, &session, create_body(b"v", 5, Some(3600))).await;
    let token = token_of(&body);

    // Force expiry.
    sqlx::query("UPDATE share_links SET expires_at = now() - interval '1 hour' WHERE token = $1")
        .bind(&token)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(fetch(&pool, &token).await.0, StatusCode::NOT_FOUND);

    sqlx::query("DELETE FROM users WHERE id = 'share-exp-u'")
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn revoke_is_owner_only() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let owner = fresh_session(&pool, "share-rev-owner", "share-rev-owner-s").await;
    let intruder = fresh_session(&pool, "share-rev-other", "share-rev-other-s").await;
    let (_, body) = create(&pool, &owner, create_body(b"v", 5, None)).await;
    let token = token_of(&body);

    let del = |session: &str, tok: &str| {
        let req = Request::builder()
            .method("DELETE")
            .uri(format!("/shares/{tok}"))
            .header("authorization", format!("Bearer {session}"))
            .body(Body::empty())
            .expect("req");
        app(pool.clone()).oneshot(req)
    };

    // The intruder can't revoke someone else's link.
    assert_eq!(
        del(&intruder, &token).await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
    // The owner can, and the link then 404s on fetch.
    assert_eq!(
        del(&owner, &token).await.unwrap().status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(fetch(&pool, &token).await.0, StatusCode::NOT_FOUND);

    sqlx::query("DELETE FROM users WHERE id IN ('share-rev-owner', 'share-rev-other')")
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn passphrase_salt_round_trips_and_create_requires_auth() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let session = fresh_session(&pool, "share-salt-u", "share-salt-s").await;

    let body = format!(
        r#"{{"enc_blob":"{}","max_views":1,"passphrase_salt":"{}"}}"#,
        b64(b"ct"),
        b64(b"salt-bytes")
    );
    let (_, created) = create(&pool, &session, body).await;
    let token = token_of(&created);
    let (status, fetched) = fetch(&pool, &token).await;
    assert_eq!(status, StatusCode::OK);
    assert!(fetched.contains(&b64(b"salt-bytes")));

    // Creation without a session is rejected.
    let req = Request::builder()
        .method("POST")
        .uri("/shares")
        .header("content-type", "application/json")
        .body(Body::from(create_body(b"v", 1, None)))
        .expect("req");
    assert_eq!(
        app(pool.clone()).oneshot(req).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );

    sqlx::query("DELETE FROM users WHERE id = 'share-salt-u'")
        .execute(&pool)
        .await
        .unwrap();
}
