//! OAuth login + session integration tests.
//!
//! DB-gated like `tests/db.rs`: each test skips when `DATABASE_URL` is unset, and otherwise runs
//! against the CI Postgres service (or a local `docker compose up`). A mock [`OAuthProvider`]
//! stands in for GitHub, so no network is involved. Tests use fixed marker ids and clean up
//! before and after themselves so reruns are idempotent.

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use sqlx::PgPool;
use std::sync::Arc;
use tower::ServiceExt;

use sotto_server::auth::{self, session, Identity, OAuthProvider};
use sotto_server::config::OAuthConfig;
use sotto_server::db;
use sotto_server::error::Result;
use sotto_server::state::AppState;

/// Provider stub that returns a fixed identity regardless of the code.
struct MockOAuth {
    identity: Identity,
}

#[async_trait]
impl OAuthProvider for MockOAuth {
    async fn exchange_code(&self, _code: &str) -> Result<Identity> {
        Ok(self.identity.clone())
    }
}

async fn pool_or_skip() -> Option<PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = db::connect(&url).await.expect("connect");
    db::migrate(&pool).await.expect("migrate");
    Some(pool)
}

fn app(pool: PgPool, identity: Identity) -> Router {
    let state = AppState {
        pool,
        oauth: Some(Arc::new(MockOAuth { identity }) as Arc<dyn OAuthProvider>),
        oauth_config: Some(OAuthConfig {
            github_client_id: "test-client-id".into(),
            github_client_secret: "test-secret".into(),
            public_base_url: "http://localhost:8080".into(),
        }),
    };
    Router::new().merge(auth::router()).with_state(state)
}

fn identity(subject: &str, email: Option<&str>) -> Identity {
    Identity {
        provider: "github".into(),
        subject: subject.into(),
        email: email.map(Into::into),
    }
}

fn get(uri: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .body(Body::empty())
        .expect("request")
}

async fn body_text(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body");
    String::from_utf8(bytes.to_vec()).expect("utf8")
}

#[tokio::test]
async fn login_redirects_to_github_and_records_state() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let cli_state = "test-login-cli-state";
    sqlx::query("DELETE FROM oauth_logins WHERE cli_state = $1")
        .bind(cli_state)
        .execute(&pool)
        .await
        .expect("pre-clean");

    let app = app(pool.clone(), identity("1", None));
    let resp = app
        .oneshot(get(&format!(
            "/auth/github/login?redirect_uri=http://127.0.0.1:51999/cb&state={cli_state}"
        )))
        .await
        .expect("oneshot");

    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let location = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .expect("location header");
    assert!(location.starts_with("https://github.com/login/oauth/authorize"));
    assert!(location.contains("client_id=test-client-id"));
    assert!(location.contains("state="));

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM oauth_logins WHERE cli_state = $1")
        .bind(cli_state)
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(count, 1);

    sqlx::query("DELETE FROM oauth_logins WHERE cli_state = $1")
        .bind(cli_state)
        .execute(&pool)
        .await
        .expect("clean");
}

#[tokio::test]
async fn login_rejects_non_loopback_redirect() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let app = app(pool, identity("1", None));
    let resp = app
        .oneshot(get(
            "/auth/github/login?redirect_uri=https://evil.example.com/cb&state=x",
        ))
        .await
        .expect("oneshot");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn callback_upserts_user_and_mints_session() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let subject = "test-mock-subject";
    let state = "test-cb-state";
    // Idempotent start.
    sqlx::query("DELETE FROM users WHERE oauth_provider = 'github' AND oauth_subject = $1")
        .bind(subject)
        .execute(&pool)
        .await
        .expect("pre-clean user");
    sqlx::query("DELETE FROM oauth_logins WHERE state = $1")
        .bind(state)
        .execute(&pool)
        .await
        .expect("pre-clean login");
    sqlx::query(
        "INSERT INTO oauth_logins (state, cli_redirect_uri, cli_state) VALUES ($1, $2, $3)",
    )
    .bind(state)
    .bind("http://127.0.0.1:52001/cb")
    .bind("cb-cli")
    .execute(&pool)
    .await
    .expect("seed login");

    let app = app(pool.clone(), identity(subject, Some("u@example.com")));
    let resp = app
        .oneshot(get(&format!(
            "/auth/github/callback?code=any-code&state={state}"
        )))
        .await
        .expect("oneshot");

    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let location = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .expect("location header")
        .to_string();
    assert!(location.starts_with("http://127.0.0.1:52001/cb?"));
    assert!(location.contains("session=st_"));
    assert!(location.contains("state=cb-cli"));

    let user_id: String = sqlx::query_scalar(
        "SELECT id FROM users WHERE oauth_provider = 'github' AND oauth_subject = $1",
    )
    .bind(subject)
    .fetch_one(&pool)
    .await
    .expect("user exists");
    let sessions: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions WHERE user_id = $1")
        .bind(&user_id)
        .fetch_one(&pool)
        .await
        .expect("session count");
    assert_eq!(sessions, 1);

    // The login state is single-use: it was consumed by the callback.
    let leftover: i64 = sqlx::query_scalar("SELECT count(*) FROM oauth_logins WHERE state = $1")
        .bind(state)
        .fetch_one(&pool)
        .await
        .expect("leftover count");
    assert_eq!(leftover, 0);

    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(&user_id)
        .execute(&pool)
        .await
        .expect("clean");
}

#[tokio::test]
async fn me_requires_a_valid_session() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let user_id = "test-me-user";
    let subject = "test-me-subject";
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("pre-clean");
    sqlx::query(
        "INSERT INTO users (id, oauth_provider, oauth_subject, email) VALUES ($1, 'github', $2, NULL)",
    )
    .bind(user_id)
    .bind(subject)
    .execute(&pool)
    .await
    .expect("insert user");

    let token = session::issue(&pool, user_id).await.expect("issue");

    // Valid bearer token → 200 with the user id.
    let authed = Request::builder()
        .uri("/auth/me")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .expect("req");
    let resp = app(pool.clone(), identity("x", None))
        .oneshot(authed)
        .await
        .expect("oneshot");
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(body_text(resp)
        .await
        .contains(&format!("\"user_id\":\"{user_id}\"")));

    // Missing token → 401.
    let resp = app(pool.clone(), identity("x", None))
        .oneshot(get("/auth/me"))
        .await
        .expect("oneshot");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Bogus token → 401.
    let bogus = Request::builder()
        .uri("/auth/me")
        .header("authorization", "Bearer st_deadbeef")
        .body(Body::empty())
        .expect("req");
    let resp = app(pool.clone(), identity("x", None))
        .oneshot(bogus)
        .await
        .expect("oneshot");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("clean");
}
