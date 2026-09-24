//! Disposable databases for populated migration-upgrade tests.
//!
//! An upgrade test migrates a throwaway database to just before the migration under test,
//! populates the old schema, then runs the rest. Doing that on the shared test database would
//! rewind it under every other integration test, so each test gets its own and drops it after.

use std::borrow::Cow;
use std::str::FromStr;

use sqlx::migrate::Migrator;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::PgPool;
use uuid::Uuid;

static ALL_MIGRATIONS: Migrator = sqlx::migrate!("./migrations");

pub struct DisposableDatabase {
    admin: PgPool,
    name: String,
    pub pool: PgPool,
}

impl DisposableDatabase {
    /// A fresh, empty database, or `None` when DB tests are not enabled.
    pub async fn create() -> Option<Self> {
        if std::env::var("SOTTO_RUN_DB_TESTS").as_deref() != Ok("1") {
            return None;
        }
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL is required");
        let base = PgConnectOptions::from_str(&url).expect("parse DATABASE_URL");
        assert!(
            matches!(base.get_host(), "localhost" | "127.0.0.1" | "::1"),
            "refusing migration test against non-local host: {}",
            base.get_host()
        );
        let admin = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(base.clone().database("postgres"))
            .await
            .expect("connect to postgres maintenance database");
        let name = format!("sotto_upgrade_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE DATABASE \"{name}\""))
            .execute(&admin)
            .await
            .expect("create disposable migration database");
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect_with(base.database(&name))
            .await
            .expect("connect to disposable migration database");
        Some(Self { admin, name, pool })
    }

    pub async fn cleanup(self) {
        self.pool.close().await;
        sqlx::query(&format!("DROP DATABASE \"{}\" WITH (FORCE)", self.name))
            .execute(&self.admin)
            .await
            .expect("drop disposable migration database");
        self.admin.close().await;
    }
}

/// Every migration strictly below `version`, so a test can stop just short of the one it covers.
pub fn migrator_before(version: i64) -> Migrator {
    Migrator {
        migrations: Cow::Owned(
            ALL_MIGRATIONS
                .iter()
                .filter(|migration| migration.version < version)
                .cloned()
                .collect(),
        ),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    }
}
