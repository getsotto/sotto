//! Postgres connection pool and schema migrations.

use sqlx::postgres::{PgPool, PgPoolOptions};

use crate::error::{Error, Result};

/// Connect to Postgres and build a connection pool.
pub async fn connect(database_url: &str) -> Result<PgPool> {
    PgPoolOptions::new()
        .max_connections(8)
        .connect(database_url)
        .await
        .map_err(Into::into)
}

/// Apply all pending schema migrations (embedded from `migrations/`).
pub async fn migrate(pool: &PgPool) -> Result<()> {
    sqlx::migrate!("./migrations")
        .run(pool)
        .await
        .map_err(|e| Error::Migrate(e.to_string()))
}
