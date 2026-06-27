//! Database round-trip test.
//!
//! Runs only when `SOTTO_RUN_DB_TESTS=1` and `DATABASE_URL` points at a local Postgres instance
//! (the CI `server` job's Postgres service, or a local `docker compose up`); otherwise it skips,
//! so `cargo test --workspace` stays DB-free.

use sotto_server::db;
use sqlx::postgres::PgConnectOptions;
use std::str::FromStr;

fn should_run_db_tests(database_url: &str) -> bool {
    if std::env::var("SOTTO_RUN_DB_TESTS").as_deref() != Ok("1") {
        eprintln!("skipping: SOTTO_RUN_DB_TESTS=1 not set");
        return false;
    }

    let options = PgConnectOptions::from_str(database_url).expect("parse DATABASE_URL");
    let host = options.get_host();
    assert!(
        matches!(host, "localhost" | "127.0.0.1" | "::1"),
        "refusing to run destructive DB tests against non-local host: {host}"
    );
    true
}

#[tokio::test]
async fn migrations_apply_and_user_round_trips() {
    let Ok(database_url) = std::env::var("DATABASE_URL") else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    if !should_run_db_tests(&database_url) {
        return;
    }

    let pool = db::connect(&database_url).await.expect("connect");
    db::migrate(&pool).await.expect("migrate");

    let id = "test-user-roundtrip";
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .expect("cleanup");
    sqlx::query(
        "INSERT INTO users (id, oauth_provider, oauth_subject, email) VALUES ($1, $2, $3, $4)",
    )
    .bind(id)
    .bind("github")
    .bind("12345")
    .bind("user@example.com")
    .execute(&pool)
    .await
    .expect("insert");

    let (provider, subject): (String, String) =
        sqlx::query_as("SELECT oauth_provider, oauth_subject FROM users WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .expect("select");
    assert_eq!(provider, "github");
    assert_eq!(subject, "12345");

    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .expect("cleanup");
}
