//! Populated upgrade coverage for the machine-token expiry migration (0029).
//!
//! Deploying expiry must not break CI that works today: every token that exists at upgrade time
//! gets the same notice window counted from the upgrade, however old it is. Database-gated, on a
//! disposable database so the pre-0029 schema can be populated without rewinding the shared one.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use dryoc::generichash::{GenericHash, Key as GhKey};
use sqlx::PgPool;
use tower::ServiceExt;

use sotto_server::config::DEFAULT_ORGANISATION_DELETION_RETENTION_DAYS;
use sotto_server::db;
use sotto_server::state::AppState;

// Only the migration helpers: `support/mod.rs` also carries the race harness, which this target
// does not use, and pulling it in would fail the build on dead code.
mod support {
    pub mod migrations;
}

use support::migrations::{migrator_before, DisposableDatabase};

const EXPIRY_MIGRATION: i64 = 29;

/// The stored form of a bearer token: the same BLAKE2b the server's `session::hash_token` uses,
/// which is crate-private. The seeded token has to authenticate through the real extractor.
fn hash_token(token: &str) -> Vec<u8> {
    GenericHash::hash_with_defaults_to_vec::<_, GhKey>(token.as_bytes(), None).expect("hash")
}

fn app(pool: PgPool) -> Router {
    let state = AppState {
        deployment_mode: sotto_server::config::DeploymentMode::SelfHosted,
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
        .merge(sotto_server::machine::router())
        .with_state(state)
}

/// A personal project with one environment and two machine tokens on the pre-expiry schema: one
/// live and 400 days old (far past any lifetime counted from creation), one revoked.
async fn seed_legacy_tokens(pool: &PgPool, live_token: &str) {
    sqlx::query(
        "INSERT INTO users (id, oauth_provider, oauth_subject) VALUES ('legacy-owner', 'github', 'legacy-owner')",
    )
    .execute(pool)
    .await
    .expect("insert owner");
    sqlx::query(
        "INSERT INTO projects (id, owner_id, enc_name) VALUES ('legacy-p', 'legacy-owner', 'p')",
    )
    .execute(pool)
    .await
    .expect("insert project");
    sqlx::query(
        "INSERT INTO environments (id, project_id, enc_name) VALUES ('legacy-e', 'legacy-p', 'e')",
    )
    .execute(pool)
    .await
    .expect("insert environment");
    for (id, hash, revoked) in [
        ("legacy-live", hash_token(live_token), false),
        ("legacy-revoked", hash_token("smt_revoked"), true),
    ] {
        sqlx::query(
            "INSERT INTO machine_tokens \
             (id, env_id, name, token_hash, public_key, enc_vault_key, created_by, created_at, revoked_at) \
             VALUES ($1, 'legacy-e', 'ci', $2, $3, $4, 'legacy-owner', now() - interval '400 days', \
                     CASE WHEN $5 THEN now() - interval '1 day' END)",
        )
        .bind(id)
        .bind(hash)
        .bind([0xABu8; 32].as_slice())
        .bind(b"sealed-grant".as_slice())
        .bind(revoked)
        .execute(pool)
        .await
        .expect("insert legacy machine token");
    }
}

#[tokio::test]
async fn migration_0029_gives_existing_tokens_ninety_days_from_upgrade() {
    let Some(database) = DisposableDatabase::create().await else {
        return;
    };
    migrator_before(EXPIRY_MIGRATION)
        .run(&database.pool)
        .await
        .expect("apply migrations through 0028");
    let live_token = "smt_legacy-live";
    seed_legacy_tokens(&database.pool, live_token).await;

    db::migrate(&database.pool)
        .await
        .expect("apply the expiry migration");

    // Every row, revoked or not, gets the same end date: 90 days from the upgrade, not from its
    // 400-day-old creation.
    let rows: Vec<(String, bool)> = sqlx::query_as(
        "SELECT id, expires_at BETWEEN now() + interval '89 days' AND now() + interval '90 days' \
         FROM machine_tokens ORDER BY id",
    )
    .fetch_all(&database.pool)
    .await
    .expect("read backfilled expiry");
    assert_eq!(
        rows,
        vec![
            ("legacy-live".to_string(), true),
            ("legacy-revoked".to_string(), true),
        ]
    );

    // No row can be without an end date, and the backfill default is gone: an insert that
    // forgets the expiry must fail rather than quietly inherit it.
    let (nullable, default): (String, Option<String>) = sqlx::query_as(
        "SELECT is_nullable, column_default FROM information_schema.columns \
         WHERE table_name = 'machine_tokens' AND column_name = 'expires_at'",
    )
    .fetch_one(&database.pool)
    .await
    .expect("expires_at column");
    assert_eq!(nullable, "NO");
    assert_eq!(default, None);

    // The regression that matters: CI holding a pre-upgrade token keeps working.
    let resp = app(database.pool.clone())
        .oneshot(
            Request::builder()
                .uri("/machine/grant")
                .header("authorization", format!("Bearer {live_token}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("oneshot");
    assert_eq!(resp.status(), StatusCode::OK);

    database.cleanup().await;
}
