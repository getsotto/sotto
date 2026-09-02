//! Community snapshot tests: cache, stale-on-failure, and the 502 empty-cache path.
//!
//! No database - the route does not touch Postgres. A scripted source stands in for GitHub.

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use tower::ServiceExt;

use sotto_server::community::{Contributor, Handle, Snapshot};

fn sample() -> Snapshot {
    Snapshot {
        stars: 12,
        forks: 3,
        repo_url: "https://github.com/getsotto/sotto".into(),
        contributor_count: 1,
        contributors: vec![Contributor {
            login: "alice".into(),
            html_url: "https://github.com/alice".into(),
            contributions: 8,
        }],
    }
}

fn app(handle: Handle) -> Router {
    sotto_server::community::router_standalone(handle)
}

async fn get_community(app: &Router) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/community")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, value)
}

#[tokio::test]
async fn returns_the_pinned_payload() {
    let app = app(Handle::sequence(vec![Ok(sample())]));
    let (status, body) = get_community(&app).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["stars"], 12);
    assert_eq!(body["forks"], 3);
    assert_eq!(body["repo_url"], "https://github.com/getsotto/sotto");
    assert_eq!(body["contributor_count"], 1);
    assert_eq!(body["contributors"][0]["login"], "alice");
    assert_eq!(body["contributors"][0]["contributions"], 8);
    assert!(body["contributors"][0].get("avatar_url").is_none());
}

#[tokio::test]
async fn caches_a_successful_fetch() {
    let app = app(Handle::sequence(vec![
        Ok(sample()),
        Err("should not be called".into()),
    ]));
    let (first, a) = get_community(&app).await;
    let (second, b) = get_community(&app).await;
    assert_eq!(first, StatusCode::OK);
    assert_eq!(second, StatusCode::OK);
    assert_eq!(a, b);
}

#[tokio::test]
async fn serves_stale_when_github_fails_after_a_hit() {
    let app = app(Handle::sequence_with_ttl(
        vec![Ok(sample()), Err("github down".into())],
        Duration::from_millis(1),
    ));
    let (first, body) = get_community(&app).await;
    assert_eq!(first, StatusCode::OK);
    tokio::time::sleep(Duration::from_millis(5)).await;
    let (second, stale) = get_community(&app).await;
    assert_eq!(second, StatusCode::OK);
    assert_eq!(body, stale);
    assert_eq!(stale["stars"], 12);
}

#[tokio::test]
async fn disabled_source_is_service_unavailable() {
    let (status, _) = get_community(&app(Handle::disabled())).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn empty_cache_and_failed_fetch_is_bad_gateway() {
    let app = app(Handle::sequence(vec![Err("github down".into())]));
    let (status, _) = get_community(&app).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn coalesces_concurrent_misses_into_one_fetch() {
    let app = app(Handle::sequence_with_ttl_and_backoff(
        vec![Ok(sample()), Err("second fetch must not happen".into())],
        Duration::from_secs(60 * 60),
        Duration::from_secs(60 * 60),
        Duration::from_millis(50),
    ));
    let (a, b) = tokio::join!(get_community(&app), get_community(&app));
    assert_eq!(a.0, StatusCode::OK);
    assert_eq!(b.0, StatusCode::OK);
    assert_eq!(a.1, b.1);
    assert_eq!(a.1["stars"], 12);
}

#[tokio::test]
async fn failed_fetch_is_not_retried_until_backoff_elapses() {
    let app = app(Handle::sequence_with_ttl_and_backoff(
        vec![Err("github down".into()), Ok(sample())],
        Duration::from_secs(60 * 60),
        Duration::from_millis(40),
        Duration::ZERO,
    ));
    let (first, _) = get_community(&app).await;
    assert_eq!(first, StatusCode::BAD_GATEWAY);
    let (second, _) = get_community(&app).await;
    assert_eq!(second, StatusCode::BAD_GATEWAY);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (third, body) = get_community(&app).await;
    assert_eq!(third, StatusCode::OK);
    assert_eq!(body["stars"], 12);
}
