//! Database round-trip test.
//!
//! Runs only when `DATABASE_URL` is set (the CI `server` job's Postgres service, or a local
//! `docker compose up`); otherwise it skips, so `cargo test --workspace` stays DB-free.

use sotto_server::db;

#[tokio::test]
async fn migrations_apply_and_user_round_trips() {
    let Ok(database_url) = std::env::var("DATABASE_URL") else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };

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
