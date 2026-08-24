//! Organisation-deletion metrics endpoint tests.
//!
//! Authentication tests use a lazy, unreachable pool so they prove the exporter rejects callers
//! before touching Postgres. The successful scrape is DB-gated because it exercises the snapshot
//! query against the migrated schema.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use sqlx::PgPool;
use tower::ServiceExt;

use sotto_server::config::DEFAULT_ORGANISATION_DELETION_RETENTION_DAYS;
use sotto_server::db;
use sotto_server::state::AppState;

fn app(pool: PgPool, token: Option<&str>) -> Router {
    let state = AppState {
        pool,
        oauth: None,
        oauth_config: None,
        billing: None,
        telemetry_ingest: false,
        organisation_deletion_retention_days: DEFAULT_ORGANISATION_DELETION_RETENTION_DAYS,
        organisation_deletion_metrics_token: token.map(str::to_owned),
    };
    Router::new()
        .merge(sotto_server::org_deletion_metrics::router())
        .with_state(state)
}

async fn get_metrics(app: &Router, token: Option<&str>) -> axum::response::Response {
    let mut request = Request::builder()
        .method("GET")
        .uri("/ops/organisation-deletion/metrics");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    app.clone()
        .oneshot(request.body(Body::empty()).expect("request"))
        .await
        .expect("response")
}

async fn pool_or_skip() -> Option<PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = db::connect(&url).await.expect("connect");
    db::migrate(&pool).await.expect("migrate");
    Some(pool)
}

#[tokio::test]
async fn metrics_endpoint_is_dark_without_a_token() {
    let pool = PgPool::connect_lazy("postgres://127.0.0.1:1/sotto").expect("lazy pool");
    let response = get_metrics(&app(pool, None), None).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn metrics_endpoint_rejects_missing_and_wrong_tokens() {
    let pool = PgPool::connect_lazy("postgres://127.0.0.1:1/sotto").expect("lazy pool");
    let app = app(pool, Some("metrics-secret"));

    assert_eq!(
        get_metrics(&app, None).await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        get_metrics(&app, Some("wrong-secret")).await.status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn metrics_endpoint_returns_prometheus_text_for_the_configured_token() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let response = get_metrics(&app(pool, Some("metrics-secret")), Some("metrics-secret")).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/plain; version=0.0.4"
    );
    assert_eq!(response.headers().get("cache-control").unwrap(), "no-store");
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let body = String::from_utf8(body.to_vec()).expect("utf8");
    assert!(body.contains("# HELP sotto_organisation_deletion_operations"));
    assert!(body.contains("sotto_organisation_deletion_purge_duration_count 0"));
}
