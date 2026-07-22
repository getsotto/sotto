//! Telemetry ingest tests: the ships-dark gate, payload validation, and the upsert that turns
//! daily pings into a countable census row. DB-gated like the other server tests.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

use sotto_server::db;
use sotto_server::state::AppState;

async fn pool_or_skip() -> Option<PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = db::connect(&url).await.expect("connect");
    db::migrate(&pool).await.expect("migrate");
    Some(pool)
}

fn app(pool: PgPool, ingest: bool) -> Router {
    let state = AppState {
        pool,
        oauth: None,
        oauth_config: None,
        billing: None,
        telemetry_ingest: ingest,
    };
    Router::new()
        .merge(sotto_server::telemetry::router())
        .with_state(state)
}

async fn post_raw(app: &Router, body: String) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/telemetry/v1/ping")
                .header("content-type", "application/json")
                .body(Body::from(body))
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

async fn post_ping(app: &Router, body: &serde_json::Value) -> (StatusCode, serde_json::Value) {
    post_raw(app, body.to_string()).await
}

fn ping_body(instance_id: &str, version: &str) -> serde_json::Value {
    serde_json::json!({
        "instance_id": instance_id,
        "version": version,
        "os": "linux",
        "arch": "x86_64",
    })
}

#[tokio::test]
async fn ingest_ships_dark_without_the_flag() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let app = app(pool, false);
    let (status, _) = post_ping(&app, &ping_body(&Uuid::new_v4().to_string(), "0.2.0")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    // The gate must answer before body parsing: even a malformed post gets the 503, never a
    // JSON-extractor 400 (the contract in crates/server/src/telemetry.rs's module docs).
    let (status, _) = post_raw(&app, "not json at all".into()).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn ping_upserts_one_census_row_and_names_the_latest_version() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let app = app(pool.clone(), true);
    let id = Uuid::new_v4().to_string();

    let (status, body) = post_ping(&app, &ping_body(&id, "0.1.0")).await;
    assert_eq!(status, StatusCode::OK);
    // The ingest host's own version is the fleet's update reference.
    assert_eq!(body["latest_version"], env!("CARGO_PKG_VERSION"));

    // A later ping from the same instance - sent with an UPPERCASED id to prove ids are
    // normalised, not duplicated - updates the row in place.
    let (status, _) = post_ping(&app, &ping_body(&id.to_uppercase(), "0.2.0")).await;
    assert_eq!(status, StatusCode::OK);

    let rows: Vec<(String, bool)> = sqlx::query_as(
        "SELECT version, first_seen <= last_seen FROM telemetry_pings WHERE instance_id = $1",
    )
    .bind(&id)
    .fetch_all(&pool)
    .await
    .expect("select");
    assert_eq!(rows.len(), 1, "one instance must stay one row");
    assert_eq!(rows[0].0, "0.2.0");
    assert!(rows[0].1);

    sqlx::query("DELETE FROM telemetry_pings WHERE instance_id = $1")
        .bind(&id)
        .execute(&pool)
        .await
        .expect("cleanup");
}

#[tokio::test]
async fn ingest_rejects_malformed_payloads() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let app = app(pool, true);

    // A non-UUID instance id is the vector for stuffing arbitrary data into the table.
    let (status, _) = post_ping(&app, &ping_body("not-a-uuid", "0.2.0")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = post_ping(
        &app,
        &ping_body(&Uuid::new_v4().to_string(), &"x".repeat(65)),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let mut body = ping_body(&Uuid::new_v4().to_string(), "0.2.0");
    body["os"] = serde_json::json!("");
    let (status, _) = post_ping(&app, &body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // On an ingest-enabled instance, unparseable JSON is a plain 400 from the handler.
    let (status, _) = post_raw(&app, "not json at all".into()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
