//! Probe tests: that liveness and readiness disagree when the database is gone.
//!
//! The unreachable case needs no database of its own. A lazily connected pool pointed at a closed
//! port fails on first use exactly as a pool pointed at a stopped Postgres would, which is the
//! condition worth asserting: this is the endpoint whose whole reason for existing is to go red
//! when `/health` stays green.

use std::str::FromStr;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::PgPool;
use tower::ServiceExt;

use sotto_server::config::DEFAULT_ORGANISATION_DELETION_RETENTION_DAYS;
use sotto_server::db;
use sotto_server::state::AppState;

/// The house contract for a test that touches a database, per `docs/CLAUDE.md`: an explicit
/// opt-in, and a refusal to run against anything but a local host. `DATABASE_URL` alone is not
/// enough of a signal, because the quickstart tells developers to export it, and this helper runs
/// migrations. A plain `cargo test --workspace` on that shell must not write to whatever it names.
async fn pool_or_skip() -> Option<PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    if std::env::var("SOTTO_RUN_DB_TESTS").as_deref() != Ok("1") {
        eprintln!("skipping: SOTTO_RUN_DB_TESTS=1 not set");
        return None;
    }
    let options = PgConnectOptions::from_str(&url).expect("parse DATABASE_URL");
    assert!(
        matches!(options.get_host(), "localhost" | "127.0.0.1" | "::1"),
        "refusing to run migrations against non-local host: {}",
        options.get_host()
    );
    let pool = db::connect(&url).await.expect("connect");
    db::migrate(&pool).await.expect("migrate");
    Some(pool)
}

/// A pool that parses but cannot reach a Postgres.
///
/// Nothing needs to be true about port 1 for this to hold. Normally nothing is listening there and
/// the connection is refused at once, but a listener would not speak the Postgres protocol either,
/// so the handshake fails and the verdict is the same. The short acquire timeout bounds the only
/// thing that varies, which is how long the failure takes to arrive.
fn unreachable_pool() -> PgPool {
    PgPoolOptions::new()
        .acquire_timeout(Duration::from_secs(2))
        .connect_lazy("postgres://sotto:sotto@127.0.0.1:1/sotto")
        .expect("a well-formed url")
}

fn app(pool: PgPool) -> Router {
    let state = AppState {
        deployment_mode: sotto_server::config::DeploymentMode::SelfHosted,
        pool,
        oauth: None,
        oauth_config: None,
        billing: None,
        telemetry_ingest: false,
        cloud_action_enforcement_enabled: false,
        machine_eligibility_enforcement_enabled: false,
        organisation_deletion_enabled: false,
        organisation_deletion_retention_days: DEFAULT_ORGANISATION_DELETION_RETENTION_DAYS,
        organisation_deletion_metrics_token: None,
        organisation_deletion_operator_token: None,
    };
    sotto_server::app(state)
}

async fn get(app: &Router, path: &str) -> (StatusCode, String) {
    let resp = app
        .clone()
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

#[tokio::test]
async fn liveness_ignores_the_database() {
    // The point of keeping `/health` unchanged: it answers for the process, so a database it never
    // touches cannot take it down.
    let (status, body) = get(&app(unreachable_pool()), "/health").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "ok");
}

#[tokio::test]
async fn readiness_reports_an_unreachable_database() {
    let (status, body) = get(&app(unreachable_pool()), "/health/ready").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        body, "unavailable",
        "the body names no dependency and no error"
    );
}

#[tokio::test]
async fn readiness_passes_against_a_real_database() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let (status, body) = get(&app(pool), "/health/ready").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "ok");
}
