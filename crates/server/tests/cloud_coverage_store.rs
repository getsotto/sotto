//! Loader snapshot-consistency acceptance for cloud coverage projections.
//!
//! `load` reads head, revision metadata and facts keyed off one head value in a single
//! `REPEATABLE READ` transaction. These tests deterministically force a writer commit or
//! rollback between those internal reads and prove every result is one committed snapshot:
//!
//! - `loader_pause_acknowledges_between_metadata_and_facts_reads`: the observation point
//!   itself; the loader blocks at the facts read behind the writer and observes old-then-new
//!   revisions.
//! - `complete_load_returns_one_committed_snapshot_across_writer_commit`: the paused load
//!   returns the exact old snapshot (revision, metadata, fact count, all fact fields,
//!   canonical order) and the next load the exact replacement; no facts cross revisions.
//! - `unavailable_load_never_mixes_with_committed_replacement`: a loader paused at the
//!   metadata read observes the old typed unavailable outcome, never replacement facts,
//!   and the next load returns the exact complete replacement.
//! - `rolled_back_publication_preserves_old_snapshot_and_retry_applies_once`: rollback
//!   preserves the exact old snapshot while an unrelated beneficiary publishes and loads;
//!   retry applies once with no orphan facts and no empty head.
//! - `corrupt_projection_fails_closed_while_unrelated_beneficiary_progresses`: small
//!   controls proving rejected head/status corruption, an exact fact-count reason, no
//!   repair write and unrelated progress.
//!
//! Coordination uses no production hook: the writer holds `ACCESS EXCLUSIVE ... NOWAIT` on
//! one table, the loader runs on a dedicated pool with a unique application name, and the
//! test acknowledges the pause through bounded `pg_stat_activity`/`pg_blocking_pids`
//! readiness before committing or rolling back the writer. Overlapping observation writers
//! serialize on an in-process mutex so publication writes never queue behind another test's
//! held table lock.
//!
//! Sensitivity (isolated checkout, restored before validation): removing `REPEATABLE READ`
//! or replacing the transaction with separate autocommit queries still passes, because
//! publication is atomic and revision rows are immutable and key-chained; a loader that
//! refreshes the head after the facts read fails with a new-head/old-facts mix, which is
//! the regression these tests guard.
//!
//! Timestamp and ordering boundary acceptance:
//!
//! - `max_ending_interval_round_trip_defers_export_overflow_until_evaluation`: the
//!   `[i64::MAX - 1, i64::MAX)` interval stores without eager export arithmetic and reports
//!   typed export overflow only when evaluated at its end; the stored rows stay intact.
//! - `recovery_overflow_through_publisher_writes_nothing_durable`: recovery overflow is
//!   rejected before any write; the error transaction is deliberately committed and the
//!   snapshot is unchanged while an unrelated beneficiary progresses.
//! - `canonical_ordering_is_bytewise_across_database_collations`: permuted inputs with
//!   identical duplicates load the same exact bytewise projection on the normal database
//!   and on a disposable explicitly linguistic-collation database; each operation replays
//!   `AlreadyApplied` with an unchanged head, a changed fact stays `OperationConflict`,
//!   and a default-order control proves the linguistic collation is really active.

use std::{
    str::FromStr,
    sync::{Arc, OnceLock},
};

use sotto_server::cloud_coverage::{
    evaluate, ConfirmedPaidInterval, CoverageDecision, CoverageState, InvalidCoverage,
    PersonCoverage,
};
use sotto_server::cloud_coverage_store::{
    load, publish, CoverageProjection, LoadedCoverage, PublicationOutcome, PublicationReceipt,
    StoreError, UnavailableReason,
};
use sotto_server::db;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgPool, Postgres, Transaction};
use tokio::sync::{oneshot, Barrier, Mutex, Notify};
use tokio::time::{sleep, timeout, Duration, Instant};
use uuid::Uuid;

mod support;

use support::coverage_concurrency::{
    receive_owned, receive_pid, run_with_context, run_with_teardown,
    run_with_teardown_with_budgets, transaction_pid, wait_for_specific_block, RaceTaskOwner,
    RACE_TIMEOUT,
};

const DAY: i64 = 24 * 60 * 60;

struct Fixture {
    pool: PgPool,
    beneficiary_id: String,
}

impl Fixture {
    async fn create() -> Option<Self> {
        if std::env::var("SOTTO_RUN_DB_TESTS").as_deref() != Ok("1") {
            eprintln!("skipping cloud coverage store test: set SOTTO_RUN_DB_TESTS=1");
            return None;
        }
        let database_url = std::env::var("DATABASE_URL")
            .expect("DATABASE_URL is required when SOTTO_RUN_DB_TESTS=1");
        let options = PgConnectOptions::from_str(&database_url).expect("parse DATABASE_URL");
        assert!(
            matches!(options.get_host(), "localhost" | "127.0.0.1" | "::1"),
            "refusing coverage store tests against non-local host: {}",
            options.get_host()
        );
        let pool = db::connect(&database_url).await.expect("connect");
        db::migrate(&pool).await.expect("migrate");
        let beneficiary_id = format!("coverage-store-test-{}", Uuid::new_v4());
        sqlx::query(
            "INSERT INTO users (id, oauth_provider, oauth_subject) VALUES ($1, 'coverage-test', $2)",
        )
        .bind(&beneficiary_id)
        .bind(&beneficiary_id)
        .execute(&pool)
        .await
        .expect("insert coverage test user");
        Some(Self {
            pool,
            beneficiary_id,
        })
    }

    async fn create_owned(owner: &mut RaceTaskOwner) -> Result<Option<Self>, String> {
        if std::env::var("SOTTO_RUN_DB_TESTS").as_deref() != Ok("1") {
            eprintln!("skipping cloud coverage store test: set SOTTO_RUN_DB_TESTS=1");
            return Ok(None);
        }
        let database_url = std::env::var("DATABASE_URL")
            .map_err(|_| "DATABASE_URL is required when SOTTO_RUN_DB_TESTS=1".to_string())?;
        let options = PgConnectOptions::from_str(&database_url)
            .map_err(|error| format!("parse DATABASE_URL: {error}"))?;
        if !matches!(options.get_host(), "localhost" | "127.0.0.1" | "::1") {
            return Err(format!(
                "refusing coverage store tests against non-local host: {}",
                options.get_host()
            ));
        }
        let pool = db::connect(&database_url)
            .await
            .map_err(|error| format!("connect: {error}"))?;
        db::migrate(&pool)
            .await
            .map_err(|error| format!("migrate: {error}"))?;
        let beneficiary_id = format!("coverage-store-test-{}", Uuid::new_v4());
        let cleanup_pool = pool.clone();
        let cleanup_beneficiary = beneficiary_id.clone();
        owner.register_cleanup(move || async move {
            cleanup_result_for(&cleanup_pool, &cleanup_beneficiary).await
        });
        let insert_result = sqlx::query(
            "INSERT INTO users (id, oauth_provider, oauth_subject) VALUES ($1, 'coverage-test', $2)",
        )
        .bind(&beneficiary_id)
        .bind(&beneficiary_id)
        .execute(&pool)
        .await;
        if let Err(error) = insert_result {
            let error = format!("insert coverage test user: {error}");
            return match owner.cleanup_registered().await {
                Ok(()) => Err(error),
                Err(cleanup) => Err(format!("{error}; cleanup: {cleanup}")),
            };
        }
        Ok(Some(Self {
            pool,
            beneficiary_id,
        }))
    }
}

async fn cleanup(fixture: &Fixture) {
    cleanup_result(fixture)
        .await
        .expect("delete coverage test fixture");
}

async fn cleanup_result(fixture: &Fixture) -> Result<(), String> {
    cleanup_result_for(&fixture.pool, &fixture.beneficiary_id).await
}

async fn cleanup_result_for(pool: &PgPool, beneficiary_id: &str) -> Result<(), String> {
    sqlx::query("DELETE FROM cloud_coverage_heads WHERE beneficiary_id = $1")
        .bind(beneficiary_id)
        .execute(pool)
        .await
        .map_err(|error| format!("delete coverage head: {error}"))?;
    sqlx::query("DELETE FROM cloud_coverage_revision_facts WHERE beneficiary_id = $1")
        .bind(beneficiary_id)
        .execute(pool)
        .await
        .map_err(|error| format!("delete coverage facts: {error}"))?;
    sqlx::query("DELETE FROM cloud_coverage_revisions WHERE beneficiary_id = $1")
        .bind(beneficiary_id)
        .execute(pool)
        .await
        .map_err(|error| format!("delete coverage revisions: {error}"))?;
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(beneficiary_id)
        .execute(pool)
        .await
        .map_err(|error| format!("delete coverage test user: {error}"))?;
    Ok(())
}

fn paid(id: &str, source: &str, starts_at: i64, paid_until: i64) -> ConfirmedPaidInterval {
    ConfirmedPaidInterval {
        coverage_id: id.into(),
        source_id: source.into(),
        starts_at,
        paid_until,
        failed_renewal_id: None,
    }
}

fn recovery(
    id: &str,
    source: &str,
    starts_at: i64,
    paid_until: i64,
    renewal: &str,
) -> ConfirmedPaidInterval {
    ConfirmedPaidInterval {
        failed_renewal_id: Some(renewal.into()),
        ..paid(id, source, starts_at, paid_until)
    }
}

/// A dedicated single-connection pool that identifies the loader backend.
///
/// `load` checks out its own pooled connection, so the observation tests give that connection
/// a unique application name and acknowledge the exact pause point through `pg_stat_activity`
/// instead of adding a hook to the production reader.
async fn loader_pool(application_name: &str) -> PgPool {
    debug_assert!(
        application_name.len() <= 63,
        "PostgreSQL truncates application_name past 63 bytes, breaking backend lookup"
    );
    let database_url =
        std::env::var("DATABASE_URL").expect("DATABASE_URL is required for loader pool");
    let options = PgConnectOptions::from_str(&database_url)
        .expect("parse DATABASE_URL for loader pool")
        .application_name(application_name);
    PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .expect("connect loader pool")
}

static OBSERVATION_SERIALIZER: OnceLock<Mutex<()>> = OnceLock::new();

/// Serialize observation writers across pause tests.
///
/// The `NOWAIT` lock never queues, but the publication writes before it would queue behind
/// another test's held observation lock. Hold this guard across the probe, lock, pause and
/// commit so overlapping observation writers serialize in-process instead of in PostgreSQL.
/// Bounded like every other readiness wait; unrelated progress tasks must not acquire it.
async fn acquire_observation() -> tokio::sync::MutexGuard<'static, ()> {
    let serializer = OBSERVATION_SERIALIZER.get_or_init(|| Mutex::new(()));
    timeout(RACE_TIMEOUT, serializer.lock())
        .await
        .unwrap_or_else(|_| panic!("timed out acquiring observation serialization"))
}

/// Begin a writer transaction holding the loader observation lock.
///
/// `ACCESS EXCLUSIVE` is the only table lock that blocks the loader's plain `SELECT`s. The
/// writer takes it after its publication writes, so the loader completes every earlier read
/// and then waits at this table until the writer commits or rolls back. A blocking lock
/// request would deadlock against overlapping writers upgrading from their own publication
/// locks, so take the lock with `NOWAIT` and retry on a fresh transaction while the table
/// is contended. Failed `NOWAIT` attempts never queue, so no other test can wait behind
/// this writer and no lock cycle can form. Callers hold the observation serializer across
/// the pause, so retries here only cover millisecond ordinary-test contention. Callers
/// prove uncommitted invisibility in a separate probe transaction first, so the pause
/// choreography after the lock never retries.
async fn begin_locked_writer(
    pool: &PgPool,
    beneficiary_id: &str,
    expected_revision: Option<i64>,
    operation_id: &str,
    evidence_reference: &str,
    projection: &CoverageProjection,
    table: &'static str,
) -> (Transaction<'static, Postgres>, PublicationReceipt) {
    assert!(
        matches!(
            table,
            "cloud_coverage_revisions" | "cloud_coverage_revision_facts"
        ),
        "refusing loader pause on unexpected table"
    );
    let deadline = Instant::now() + RACE_TIMEOUT;
    loop {
        if Instant::now() >= deadline {
            panic!("timed out acquiring loader observation lock on {table}");
        }
        let mut tx = pool.begin().await.expect("begin locked writer");
        let receipt = match publish(
            &mut tx,
            beneficiary_id,
            expected_revision,
            operation_id,
            evidence_reference,
            projection,
        )
        .await
        {
            Ok(receipt) => receipt,
            Err(error) => {
                tx.rollback()
                    .await
                    .expect("roll back failed locked publication");
                panic!("publish locked revision: {error}");
            }
        };
        let lock = sqlx::query(&format!(
            "LOCK TABLE {table} IN ACCESS EXCLUSIVE MODE NOWAIT"
        ))
        .execute(&mut *tx)
        .await;
        match lock {
            Ok(_) => return (tx, receipt),
            Err(error) if is_lock_unavailable(&error) => {
                tx.rollback().await.expect("roll back contended writer");
                sleep(Duration::from_millis(25)).await;
            }
            Err(error) => {
                tx.rollback().await.expect("roll back failed lock");
                panic!("lock loader observation table: {error}");
            }
        }
    }
}

fn is_lock_unavailable(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Database(db) if db.code().as_deref() == Some("55P03"))
}

/// Wait until the backend running under `application_name` blocks behind `holder_pid`.
///
/// The loader observation tests identify the loader backend by its unique application name
/// because `load` checks out its own pooled connection instead of reporting a pid. Bounded
/// readiness only: callers must commit or roll back the holder afterwards so the observed
/// backend is released on every path.
async fn wait_for_blocked_backend(pool: &PgPool, application_name: &str, holder_pid: i32) -> i32 {
    let deadline = Instant::now() + RACE_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            panic!("timed out waiting for backend '{application_name}' to block on {holder_pid}");
        }
        let waiter = timeout(
            remaining,
            sqlx::query_scalar::<_, i32>(
                "SELECT pid FROM pg_stat_activity \
                 WHERE datname = current_database() AND application_name = $1 \
                 AND $2 = ANY(pg_blocking_pids(pid)) \
                 ORDER BY pid LIMIT 1",
            )
            .bind(application_name)
            .bind(holder_pid)
            .fetch_optional(pool),
        )
        .await
        .unwrap_or_else(|_| panic!("timed out inspecting loader blocking"))
        .unwrap_or_else(|error| panic!("failed to inspect loader blocking: {error}"));
        if let Some(waiter) = waiter {
            return waiter;
        }
        sleep(Duration::from_millis(25)).await;
    }
}

#[derive(Clone, Copy)]
enum CollationProvider {
    Libc,
    Icu,
}

struct LinguisticCollation {
    provider: CollationProvider,
    locale: &'static str,
}

/// Select an installed linguistic collation for the ordering database.
///
/// Probes a small preference list so the test works wherever at least one linguistic
/// collation exists. When none is installed the opted-in run fails clearly instead of
/// skipping the assertion.
async fn select_linguistic_collation(pool: &PgPool) -> LinguisticCollation {
    const CANDIDATES: &[(CollationProvider, &str, &str)] = &[
        (CollationProvider::Libc, "en_US.UTF-8", "en_US.UTF-8"),
        (CollationProvider::Libc, "en_US.utf8", "en_US.utf8"),
        (CollationProvider::Icu, "en-US", "en-US"),
        (CollationProvider::Icu, "und", "und-x-icu"),
    ];
    for (provider, locale, catalog) in CANDIDATES {
        let present: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_collation WHERE collname = $1)")
                .bind(catalog)
                .fetch_one(pool)
                .await
                .expect("probe linguistic collation");
        if present {
            return LinguisticCollation {
                provider: *provider,
                locale,
            };
        }
    }
    panic!(
        "no linguistic collation installed (probed en_US.UTF-8, en_US.utf8, en-US, und-x-icu); \
         the opted-in collation case cannot run without one"
    );
}

/// Create an empty database with the requested linguistic collation.
///
/// Uses an explicit template and locale without touching cluster defaults or the shared
/// test database. Callers open the database through `open_linguistic_database`, which
/// drops it again when any setup step fails.
async fn create_linguistic_database(
    admin_pool: &PgPool,
    collation: &LinguisticCollation,
) -> String {
    let name = format!("sotto_collation_{}", Uuid::new_v4().simple());
    let create = match collation.provider {
        CollationProvider::Libc => format!(
            "CREATE DATABASE \"{name}\" TEMPLATE template0 LOCALE_PROVIDER libc LOCALE '{}'",
            collation.locale
        ),
        CollationProvider::Icu => format!(
            "CREATE DATABASE \"{name}\" TEMPLATE template0 LOCALE_PROVIDER icu ICU_LOCALE '{}'",
            collation.locale
        ),
    };
    sqlx::query(&create)
        .execute(admin_pool)
        .await
        .expect("create linguistic database");
    name
}

/// Connect, migrate and verify the linguistic database, dropping it on any failure.
async fn open_linguistic_database(
    admin_pool: &PgPool,
    name: &str,
    collation: &LinguisticCollation,
) -> PgPool {
    match try_open_linguistic_database(name, collation).await {
        Ok(pool) => pool,
        Err(error) => {
            let drop = sqlx::query(&format!("DROP DATABASE \"{name}\" WITH (FORCE)"))
                .execute(admin_pool)
                .await;
            panic!("open linguistic database: {error}; cleanup: {drop:?}");
        }
    }
}

async fn try_open_linguistic_database(
    name: &str,
    collation: &LinguisticCollation,
) -> Result<PgPool, String> {
    let database_url = std::env::var("DATABASE_URL")
        .map_err(|_| "DATABASE_URL is required for collation database".to_string())?;
    let options = PgConnectOptions::from_str(&database_url)
        .map_err(|error| format!("parse DATABASE_URL for collation database: {error}"))?
        .database(name);
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await
        .map_err(|error| format!("connect linguistic database: {error}"))?;
    db::migrate(&pool)
        .await
        .map_err(|error| format!("migrate linguistic database: {error}"))?;
    let active: (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT datcollate, daticulocale FROM pg_database WHERE datname = current_database()",
    )
    .fetch_one(&pool)
    .await
    .map_err(|error| format!("read linguistic database locale: {error}"))?;
    match collation.provider {
        CollationProvider::Libc => {
            if active.0.as_deref() != Some(collation.locale) {
                return Err(format!(
                    "linguistic database locale is not active, observed: {active:?}"
                ));
            }
        }
        CollationProvider::Icu => {
            if active.1.as_deref() != Some(collation.locale) {
                return Err(format!(
                    "linguistic database locale is not active, observed: {active:?}"
                ));
            }
        }
    }
    Ok(pool)
}

async fn drop_linguistic_database(
    admin_pool: &PgPool,
    name: &str,
    pool: PgPool,
) -> Result<(), String> {
    pool.close().await;
    sqlx::query(&format!("DROP DATABASE \"{name}\" WITH (FORCE)"))
        .execute(admin_pool)
        .await
        .map(|_| ())
        .map_err(|error| format!("drop linguistic database: {error}"))
}

/// The durable projection state used to prove a failed load repairs nothing.
async fn projection_snapshot(
    pool: &PgPool,
    beneficiary_id: &str,
) -> (
    Option<Option<i64>>,
    Vec<(i64, String, Option<String>, i64)>,
    Vec<(i64, String, String, i64, i64, Option<String>)>,
) {
    let head: Option<Option<i64>> = sqlx::query_scalar(
        "SELECT current_revision FROM cloud_coverage_heads WHERE beneficiary_id = $1",
    )
    .bind(beneficiary_id)
    .fetch_optional(pool)
    .await
    .expect("snapshot corrupt head");
    let revisions: Vec<(i64, String, Option<String>, i64)> = sqlx::query_as(
        "SELECT revision, status, unavailable_reason, fact_count \
         FROM cloud_coverage_revisions WHERE beneficiary_id = $1 ORDER BY revision",
    )
    .bind(beneficiary_id)
    .fetch_all(pool)
    .await
    .expect("snapshot corrupt revisions");
    let facts: Vec<(i64, String, String, i64, i64, Option<String>)> = sqlx::query_as(
        "SELECT revision, coverage_id, source_id, starts_at, paid_until, failed_renewal_id \
         FROM cloud_coverage_revision_facts WHERE beneficiary_id = $1 \
         ORDER BY revision, coverage_id",
    )
    .bind(beneficiary_id)
    .fetch_all(pool)
    .await
    .expect("snapshot corrupt facts");
    (head, revisions, facts)
}

/// Publish permuted inputs on one database and prove bytewise canonical results.
///
/// Asserts the exact coverage-ID order, source-ID order and loaded projection (never a set
/// comparison), `AlreadyApplied` replay of each operation with an unchanged head, and
/// `OperationConflict` for a changed fact. Returns the database-default fact order so the
/// caller can prove a linguistic collation is really active.
async fn canonical_order_fixture(pool: &PgPool, beneficiary_id: &str) -> Vec<String> {
    sqlx::query(
        "INSERT INTO users (id, oauth_provider, oauth_subject) VALUES ($1, 'coverage-test', $2) \
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(beneficiary_id)
    .bind(beneficiary_id)
    .execute(pool)
    .await
    .expect("insert collation test user");
    let fixture = Fixture {
        pool: pool.clone(),
        beneficiary_id: beneficiary_id.to_owned(),
    };

    let fact_b = paid("ord-B", "src-m", 0, 30 * DAY);
    let fact_d = paid("ord-D", "src-q", 30 * DAY, 60 * DAY);
    let fact_a = paid("ord-a", "src-Z", 60 * DAY, 90 * DAY);
    let fact_c = paid("ord-c", "src-A", 90 * DAY, 120 * DAY);
    let canonical = vec![
        fact_b.clone(),
        fact_d.clone(),
        fact_a.clone(),
        fact_c.clone(),
    ];

    let first = committed_publish(
        &fixture,
        None,
        "collation-op-1",
        "collation-evidence-1",
        &CoverageProjection::Complete {
            paid_intervals: vec![
                fact_c.clone(),
                fact_a.clone(),
                fact_b.clone(),
                fact_a.clone(),
                fact_d.clone(),
            ],
        },
    )
    .await
    .expect("publish first ordering revision");
    assert_eq!(first.outcome, PublicationOutcome::Applied);
    assert_eq!(first.revision, 1);

    let loaded = load(pool, beneficiary_id)
        .await
        .expect("load first ordering revision");
    assert_eq!(
        loaded,
        LoadedCoverage {
            revision: 1,
            coverage: PersonCoverage {
                beneficiary_id: beneficiary_id.to_owned(),
                paid_intervals: canonical.clone(),
            },
        }
    );
    let coverage_ids: Vec<String> = sqlx::query_scalar(
        "SELECT coverage_id FROM cloud_coverage_revision_facts \
         WHERE beneficiary_id = $1 AND revision = 1 \
         ORDER BY coverage_id COLLATE \"C\"",
    )
    .bind(beneficiary_id)
    .fetch_all(pool)
    .await
    .expect("read canonical coverage order");
    assert_eq!(coverage_ids, vec!["ord-B", "ord-D", "ord-a", "ord-c"]);
    let source_ids: Vec<String> = sqlx::query_scalar(
        "SELECT source_id FROM cloud_coverage_revision_facts \
         WHERE beneficiary_id = $1 AND revision = 1 \
         ORDER BY coverage_id COLLATE \"C\"",
    )
    .bind(beneficiary_id)
    .fetch_all(pool)
    .await
    .expect("read canonical source order");
    assert_eq!(source_ids, vec!["src-m", "src-q", "src-Z", "src-A"]);

    let second = committed_publish(
        &fixture,
        Some(1),
        "collation-op-2",
        "collation-evidence-2",
        &CoverageProjection::Complete {
            paid_intervals: vec![
                fact_d.clone(),
                fact_d.clone(),
                fact_b.clone(),
                fact_c.clone(),
                fact_a.clone(),
            ],
        },
    )
    .await
    .expect("publish second ordering revision");
    assert_eq!(second.outcome, PublicationOutcome::Applied);
    assert_eq!(second.revision, 2);
    let reloaded = load(pool, beneficiary_id)
        .await
        .expect("load second ordering revision");
    assert_eq!(reloaded.revision, 2);
    assert_eq!(reloaded.coverage.paid_intervals, canonical);

    let replay_first = committed_publish(
        &fixture,
        None,
        "collation-op-1",
        "collation-evidence-1",
        &CoverageProjection::Complete {
            paid_intervals: vec![
                fact_a.clone(),
                fact_c.clone(),
                fact_d.clone(),
                fact_b.clone(),
                fact_b.clone(),
            ],
        },
    )
    .await
    .expect("replay first ordering operation");
    assert_eq!(replay_first.outcome, PublicationOutcome::AlreadyApplied);
    assert_eq!(replay_first.revision, 1);
    let replay_second = committed_publish(
        &fixture,
        Some(1),
        "collation-op-2",
        "collation-evidence-2",
        &CoverageProjection::Complete {
            paid_intervals: vec![
                fact_b,
                fact_a.clone(),
                fact_d.clone(),
                fact_c.clone(),
                fact_c.clone(),
            ],
        },
    )
    .await
    .expect("replay second ordering operation");
    assert_eq!(replay_second.outcome, PublicationOutcome::AlreadyApplied);
    assert_eq!(replay_second.revision, 2);
    let head: i64 = sqlx::query_scalar(
        "SELECT current_revision FROM cloud_coverage_heads WHERE beneficiary_id = $1",
    )
    .bind(beneficiary_id)
    .fetch_one(pool)
    .await
    .expect("read ordering head");
    assert_eq!(head, 2);

    let mut changed_a = fact_a;
    changed_a.paid_until += 1;
    assert!(matches!(
        committed_publish(
            &fixture,
            None,
            "collation-op-1",
            "collation-evidence-1",
            &CoverageProjection::Complete {
                paid_intervals: vec![changed_a, fact_c, fact_d],
            },
        )
        .await,
        Err(StoreError::OperationConflict)
    ));

    sqlx::query_scalar(
        "SELECT coverage_id FROM cloud_coverage_revision_facts \
         WHERE beneficiary_id = $1 AND revision = 2 ORDER BY coverage_id",
    )
    .bind(beneficiary_id)
    .fetch_all(pool)
    .await
    .expect("read database-default fact order")
}

async fn committed_publish(
    fixture: &Fixture,
    expected_revision: Option<i64>,
    operation_id: &str,
    evidence_reference: &str,
    projection: &CoverageProjection,
) -> Result<sotto_server::cloud_coverage_store::PublicationReceipt, StoreError> {
    let mut tx = fixture.pool.begin().await.expect("begin publication");
    let receipt = publish(
        &mut tx,
        &fixture.beneficiary_id,
        expected_revision,
        operation_id,
        evidence_reference,
        projection,
    )
    .await?;
    tx.commit().await.expect("commit publication");
    Ok(receipt)
}

#[tokio::test]
async fn missing_and_complete_empty_projection_are_distinct() {
    let Some(fixture) = Fixture::create().await else {
        return;
    };
    assert!(matches!(
        load(&fixture.pool, &fixture.beneficiary_id).await,
        Err(StoreError::ProjectionMissing)
    ));

    let receipt = committed_publish(
        &fixture,
        None,
        "empty-op",
        "empty-evidence",
        &CoverageProjection::Complete {
            paid_intervals: vec![],
        },
    )
    .await
    .expect("publish empty projection");
    assert_eq!(receipt.revision, 1);
    let loaded = load(&fixture.pool, &fixture.beneficiary_id)
        .await
        .expect("load empty projection");
    assert_eq!(loaded.coverage.paid_intervals, Vec::new());
    assert_eq!(
        evaluate(&loaded.coverage, 5 * DAY).unwrap().state,
        CoverageState::Free
    );
    cleanup(&fixture).await;
}

#[tokio::test]
async fn complete_facts_round_trip_and_exact_replay_is_idempotent() {
    let Some(fixture) = Fixture::create().await else {
        return;
    };
    let facts = vec![
        paid("B", "sponsor", 0, 30 * DAY),
        paid("a", "sponsor", 40 * DAY, 70 * DAY),
    ];
    let first = committed_publish(
        &fixture,
        None,
        "operation-1",
        "evidence-1",
        &CoverageProjection::Complete {
            paid_intervals: facts.clone(),
        },
    )
    .await
    .expect("publish complete projection");
    assert_eq!(first.outcome, PublicationOutcome::Applied);

    let mut reordered = facts;
    reordered.reverse();
    reordered.push(reordered[0].clone());
    let replay = committed_publish(
        &fixture,
        None,
        "operation-1",
        "evidence-1",
        &CoverageProjection::Complete {
            paid_intervals: reordered,
        },
    )
    .await
    .expect("replay complete projection");
    assert_eq!(replay.revision, first.revision);
    assert_eq!(replay.outcome, PublicationOutcome::AlreadyApplied);

    let loaded = load(&fixture.pool, &fixture.beneficiary_id)
        .await
        .expect("load complete projection");
    assert_eq!(loaded.revision, 1);
    assert_eq!(loaded.coverage.paid_intervals.len(), 2);
    assert_eq!(loaded.coverage.paid_intervals[0].coverage_id, "B");
    assert_eq!(loaded.coverage.paid_intervals[1].coverage_id, "a");
    cleanup(&fixture).await;
}

#[tokio::test]
async fn operation_conflict_and_stale_replay_cannot_rewind_head() {
    let Some(fixture) = Fixture::create().await else {
        return;
    };
    let first_projection = CoverageProjection::Complete {
        paid_intervals: vec![paid("first", "personal", 0, 30 * DAY)],
    };
    committed_publish(
        &fixture,
        None,
        "operation-1",
        "evidence-1",
        &first_projection,
    )
    .await
    .expect("publish first projection");
    committed_publish(
        &fixture,
        Some(1),
        "operation-2",
        "evidence-2",
        &CoverageProjection::Complete {
            paid_intervals: vec![paid("second", "personal", 30 * DAY, 60 * DAY)],
        },
    )
    .await
    .expect("publish correction");
    assert!(matches!(
        committed_publish(
            &fixture,
            Some(1),
            "competing-correction",
            "evidence-competing",
            &CoverageProjection::Complete {
                paid_intervals: vec![paid("competing", "personal", 30 * DAY, 90 * DAY)],
            },
        )
        .await,
        Err(StoreError::RevisionConflict {
            expected: Some(1),
            actual: Some(2),
        })
    ));

    let replay = committed_publish(
        &fixture,
        None,
        "operation-1",
        "evidence-1",
        &first_projection,
    )
    .await
    .expect("replay old operation");
    assert_eq!(replay.outcome, PublicationOutcome::AlreadyApplied);
    assert_eq!(replay.revision, 1);
    assert!(matches!(
        committed_publish(
            &fixture,
            None,
            "operation-1",
            "changed-evidence",
            &first_projection,
        )
        .await,
        Err(StoreError::OperationConflict)
    ));
    assert!(matches!(
        committed_publish(
            &fixture,
            None,
            "operation-1",
            "evidence-1",
            &CoverageProjection::Complete {
                paid_intervals: vec![paid("different-facts", "personal", 0, 30 * DAY)],
            },
        )
        .await,
        Err(StoreError::OperationConflict)
    ));
    assert!(matches!(
        committed_publish(
            &fixture,
            None,
            "stale-operation",
            "stale-evidence",
            &first_projection,
        )
        .await,
        Err(StoreError::RevisionConflict {
            expected: None,
            actual: Some(2),
        })
    ));
    assert_eq!(
        load(&fixture.pool, &fixture.beneficiary_id)
            .await
            .unwrap()
            .revision,
        2
    );
    cleanup(&fixture).await;
}

#[tokio::test]
async fn competing_corrections_serialize_on_the_head_and_reject_the_loser() {
    let Some(fixture) = Fixture::create().await else {
        return;
    };
    committed_publish(
        &fixture,
        None,
        "correction-base",
        "correction-base-evidence",
        &CoverageProjection::Complete {
            paid_intervals: vec![paid("correction-base-fact", "personal", 0, 30 * DAY)],
        },
    )
    .await
    .expect("publish correction base");

    let winning_projection = CoverageProjection::Complete {
        paid_intervals: vec![paid(
            "correction-winner-fact",
            "personal",
            30 * DAY,
            60 * DAY,
        )],
    };
    let losing_projection = CoverageProjection::Complete {
        paid_intervals: vec![paid(
            "correction-loser-fact",
            "personal",
            60 * DAY,
            90 * DAY,
        )],
    };
    let release = Arc::new(Notify::new());
    let mut owner = RaceTaskOwner::new();
    let cleanup_pool = fixture.pool.clone();
    let cleanup_beneficiary = fixture.beneficiary_id.clone();
    owner.register_cleanup(move || async move {
        cleanup_result_for(&cleanup_pool, &cleanup_beneficiary).await
    });
    let result = run_with_context(
        &mut owner,
        |owner| {
            Box::pin(async move {
                let (holder_ready, holder_ready_rx) = oneshot::channel();
                let holder_pool = fixture.pool.clone();
                let holder_beneficiary = fixture.beneficiary_id.clone();
                let holder_projection = winning_projection.clone();
                let holder_release = release.clone();
                let mut holder = Some(owner.spawn(async move {
                    let mut tx = holder_pool.begin().await.expect("begin held correction");
                    let pid = transaction_pid(&mut tx).await;
                    let result = publish(
                        &mut tx,
                        &holder_beneficiary,
                        Some(1),
                        "correction-winner",
                        "correction-winner-evidence",
                        &holder_projection,
                    )
                    .await;
                    holder_ready.send(pid).expect("signal held correction");
                    holder_release.notified().await;
                    match result {
                        Ok(receipt) => {
                            tx.commit().await.expect("commit held correction");
                            Ok(receipt)
                        }
                        Err(error) => {
                            tx.rollback().await.expect("rollback held correction");
                            Err(error)
                        }
                    }
                }));
                let holder_pid =
                    receive_pid(holder_ready_rx, "receive correction holder pid").await;

                let (waiter_ready, waiter_ready_rx) = oneshot::channel();
                let waiter_pool = fixture.pool.clone();
                let waiter_beneficiary = fixture.beneficiary_id.clone();
                let mut waiter = Some(owner.spawn(async move {
                    let mut tx = waiter_pool.begin().await.expect("begin waiting correction");
                    let pid = transaction_pid(&mut tx).await;
                    waiter_ready.send(pid).expect("signal waiting correction");
                    let result = publish(
                        &mut tx,
                        &waiter_beneficiary,
                        Some(1),
                        "correction-loser",
                        "correction-loser-evidence",
                        &losing_projection,
                    )
                    .await;
                    tx.rollback().await.expect("rollback waiting correction");
                    result
                }));
                let waiter_pid =
                    receive_pid(waiter_ready_rx, "receive correction waiter pid").await;
                wait_for_specific_block(&fixture.pool, waiter_pid, holder_pid).await;
                release.notify_one();

                let winner = receive_owned(&mut holder, "held correction")
                    .await
                    .expect("held correction task completed")
                    .expect("winning correction applied");
                let loser = receive_owned(&mut waiter, "waiting correction")
                    .await
                    .expect("waiting correction task completed");
                assert_eq!(winner.outcome, PublicationOutcome::Applied);
                assert_eq!(winner.revision, 2);
                assert!(matches!(
                    loser,
                    Err(StoreError::RevisionConflict {
                        expected: Some(1),
                        actual: Some(2),
                    })
                ));
                let loaded = load(&fixture.pool, &fixture.beneficiary_id)
                    .await
                    .expect("load winning correction");
                assert_eq!(loaded.revision, 2);
                assert_eq!(loaded.coverage.paid_intervals.len(), 1);
                assert_eq!(
                    loaded.coverage.paid_intervals[0].coverage_id,
                    "correction-winner-fact"
                );
                let historical: (String, i64) = sqlx::query_as(
                    "SELECT operation_id, fact_count FROM cloud_coverage_revisions \
                     WHERE beneficiary_id = $1 AND revision = 1",
                )
                .bind(&fixture.beneficiary_id)
                .fetch_one(&fixture.pool)
                .await
                .expect("read preserved correction history");
                assert_eq!(historical, ("correction-base".into(), 1));
                let historical_fact: String = sqlx::query_scalar(
                    "SELECT coverage_id FROM cloud_coverage_revision_facts \
                     WHERE beneficiary_id = $1 AND revision = 1",
                )
                .bind(&fixture.beneficiary_id)
                .fetch_one(&fixture.pool)
                .await
                .expect("read preserved correction fact");
                assert_eq!(historical_fact, "correction-base-fact");
                let loser_count: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM cloud_coverage_revisions \
                     WHERE beneficiary_id = $1 AND operation_id = 'correction-loser'",
                )
                .bind(&fixture.beneficiary_id)
                .fetch_one(&fixture.pool)
                .await
                .expect("count losing correction");
                assert_eq!(loser_count, 0);
                Ok(())
            })
        },
        || async { Ok::<(), String>(()) },
    )
    .await;
    result.expect("supervised correction race");
}

#[tokio::test]
async fn aborted_owned_publication_task_rolls_back_before_fixture_cleanup() {
    let Some(fixture) = Fixture::create().await else {
        return;
    };
    let Some(unrelated) = Fixture::create().await else {
        return;
    };
    committed_publish(
        &unrelated,
        None,
        "unrelated-publication",
        "unrelated-evidence",
        &CoverageProjection::Complete {
            paid_intervals: vec![],
        },
    )
    .await
    .expect("publish unrelated fixture");

    let mut owner = RaceTaskOwner::new();
    let cleanup_pool = fixture.pool.clone();
    let cleanup_beneficiary = fixture.beneficiary_id.clone();
    let unrelated_pool = unrelated.pool.clone();
    let unrelated_beneficiary = unrelated.beneficiary_id.clone();
    owner.register_cleanup(move || async move {
        cleanup_result_for(&cleanup_pool, &cleanup_beneficiary).await?;
        cleanup_result_for(&unrelated_pool, &unrelated_beneficiary).await
    });
    let result = run_with_context(
        &mut owner,
        |owner| {
            Box::pin(async move {
                let (ready, ready_rx) = oneshot::channel();
                let (release, release_rx) = oneshot::channel();
                let (finished, finished_rx) = oneshot::channel();
                let pool = fixture.pool.clone();
                let beneficiary_id = fixture.beneficiary_id.clone();
                let _task = owner.spawn(async move {
                    let mut tx = pool.begin().await.expect("begin owned publication");
                    publish(
                        &mut tx,
                        &beneficiary_id,
                        None,
                        "aborted-publication",
                        "aborted-evidence",
                        &CoverageProjection::Complete {
                            paid_intervals: vec![],
                        },
                    )
                    .await
                    .expect("publish owned publication");
                    ready.send(()).expect("signal owned publication");
                    release_rx.await.expect("release owned publication");
                    finished
                        .send(())
                        .expect("signal owned publication completion");
                });
                tokio::time::timeout(RACE_TIMEOUT, ready_rx)
                    .await
                    .expect("owned publication became ready")
                    .expect("owned publication task exited before readiness");
                release.send(()).expect("release owned publication");
                finished_rx.await.expect("owned publication task completed");
                let receipt = committed_publish(
                    &fixture,
                    None,
                    "aborted-publication",
                    "aborted-evidence",
                    &CoverageProjection::Complete {
                        paid_intervals: vec![],
                    },
                )
                .await
                .expect("replacement publication after task cleanup");
                assert_eq!(receipt.revision, 1);
                assert_eq!(
                    load(&fixture.pool, &fixture.beneficiary_id)
                        .await
                        .expect("load replacement publication")
                        .revision,
                    receipt.revision
                );
                let unrelated_loaded = load(&unrelated.pool, &unrelated.beneficiary_id)
                    .await
                    .expect("load unrelated fixture after cleanup");
                assert_eq!(unrelated_loaded.revision, 1);
                Ok(())
            })
        },
        || async { Ok::<(), String>(()) },
    )
    .await;
    result.expect("supervised publication cleanup");
}

#[tokio::test]
async fn scenario_panic_cleans_owned_publication_fixture() {
    let Some(unrelated) = Fixture::create().await else {
        return;
    };
    committed_publish(
        &unrelated,
        None,
        "unrelated-publication",
        "unrelated-evidence",
        &CoverageProjection::Complete {
            paid_intervals: vec![],
        },
    )
    .await
    .expect("publish unrelated fixture");
    let mut owner = RaceTaskOwner::new();
    let Some(fixture) = Fixture::create_owned(&mut owner)
        .await
        .expect("create owned fixture")
    else {
        cleanup(&unrelated).await;
        return;
    };

    let (ready, ready_rx) = oneshot::channel();
    let pool = fixture.pool.clone();
    let beneficiary_id = fixture.beneficiary_id.clone();
    let _task = owner.spawn(async move {
        let mut tx = pool.begin().await.expect("begin panic fixture publication");
        publish(
            &mut tx,
            &beneficiary_id,
            None,
            "panic-publication",
            "panic-evidence",
            &CoverageProjection::Complete {
                paid_intervals: vec![],
            },
        )
        .await
        .expect("publish panic fixture");
        ready.send(()).expect("signal panic fixture readiness");
        std::future::pending::<()>().await;
    });

    let result = run_with_teardown(
        &mut owner,
        async {
            tokio::time::timeout(RACE_TIMEOUT, ready_rx)
                .await
                .map_err(|_| "timed out waiting for panic fixture".to_string())
                .and_then(|result| {
                    result.map_err(|_| "panic fixture task exited before readiness".to_string())
                })?;
            panic!("intentional scenario failure");
            #[allow(unreachable_code)]
            Ok::<(), String>(())
        },
        || async { Ok::<(), String>(()) },
    )
    .await;
    assert_eq!(
        result,
        Err("scenario: scenario panicked: intentional scenario failure".into())
    );

    let remaining_user: Option<String> = sqlx::query_scalar("SELECT id FROM users WHERE id = $1")
        .bind(&fixture.beneficiary_id)
        .fetch_optional(&fixture.pool)
        .await
        .expect("check panic fixture cleanup");
    assert!(remaining_user.is_none());
    let unrelated_loaded = load(&unrelated.pool, &unrelated.beneficiary_id)
        .await
        .expect("load unrelated fixture after panic cleanup");
    assert_eq!(unrelated_loaded.revision, 1);

    cleanup(&unrelated).await;
}

#[tokio::test]
async fn setup_failure_after_insert_cleans_the_registered_fixture() {
    let mut owner = RaceTaskOwner::new();
    let Some(fixture) = Fixture::create_owned(&mut owner)
        .await
        .expect("create owned fixture")
    else {
        return;
    };
    let result = run_with_teardown(
        &mut owner,
        async { Err::<(), _>("setup failed after user insert".into()) },
        || async { Ok::<(), String>(()) },
    )
    .await;
    assert_eq!(
        result,
        Err("scenario: setup failed after user insert".into())
    );
    let remaining_user: Option<String> = sqlx::query_scalar("SELECT id FROM users WHERE id = $1")
        .bind(&fixture.beneficiary_id)
        .fetch_optional(&fixture.pool)
        .await
        .expect("check setup failure cleanup");
    assert!(remaining_user.is_none());
}

#[tokio::test]
async fn readiness_timeout_cancels_a_parked_publication_before_cleanup() {
    let mut owner = RaceTaskOwner::new();
    let Some(fixture) = Fixture::create_owned(&mut owner)
        .await
        .expect("create owned fixture")
    else {
        return;
    };
    let (ready, ready_rx) = oneshot::channel();
    let pool = fixture.pool.clone();
    let beneficiary_id = fixture.beneficiary_id.clone();
    let _task = owner.spawn(async move {
        let mut tx = pool.begin().await.expect("begin parked publication");
        publish(
            &mut tx,
            &beneficiary_id,
            None,
            "readiness-timeout-publication",
            "readiness-timeout-evidence",
            &CoverageProjection::Complete {
                paid_intervals: vec![],
            },
        )
        .await
        .expect("publish parked publication");
        ready.send(()).expect("signal parked publication");
        std::future::pending::<()>().await;
    });

    let result = run_with_teardown_with_budgets(
        &mut owner,
        async {
            tokio::time::timeout(RACE_TIMEOUT, ready_rx)
                .await
                .map_err(|_| "parked publication did not become ready".to_string())
                .and_then(|result| {
                    result.map_err(|_| "parked publication exited early".to_string())
                })?;
            tokio::time::timeout(Duration::from_millis(10), std::future::pending::<()>())
                .await
                .map_err(|_| "readiness timed out".to_string())?;
            Ok(())
        },
        || async { Ok::<(), String>(()) },
        RACE_TIMEOUT,
        RACE_TIMEOUT,
    )
    .await;
    assert_eq!(result, Err("scenario: readiness timed out".into()));
    let remaining_user: Option<String> = sqlx::query_scalar("SELECT id FROM users WHERE id = $1")
        .bind(&fixture.beneficiary_id)
        .fetch_optional(&fixture.pool)
        .await
        .expect("check readiness timeout cleanup");
    assert!(remaining_user.is_none());
}

#[tokio::test]
async fn invalid_publication_does_not_create_a_head() {
    let Some(fixture) = Fixture::create().await else {
        return;
    };
    let mut tx = fixture
        .pool
        .begin()
        .await
        .expect("begin invalid publication");
    assert!(matches!(
        publish(
            &mut tx,
            &fixture.beneficiary_id,
            None,
            "invalid",
            "evidence-invalid",
            &CoverageProjection::Complete {
                paid_intervals: vec![paid("invalid", "personal", 10, 10)],
            },
        )
        .await,
        Err(StoreError::InvalidCoverage(
            sotto_server::cloud_coverage::InvalidCoverage::InvalidInterval
        ))
    ));
    tx.commit()
        .await
        .expect("commit invalid publication transaction");
    assert!(matches!(
        load(&fixture.pool, &fixture.beneficiary_id).await,
        Err(StoreError::ProjectionMissing)
    ));
    cleanup(&fixture).await;
}

#[tokio::test]
async fn first_revision_conflict_does_not_leave_an_empty_head() {
    let Some(fixture) = Fixture::create().await else {
        return;
    };
    let mut tx = fixture
        .pool
        .begin()
        .await
        .expect("begin first revision conflict");
    assert!(matches!(
        publish(
            &mut tx,
            &fixture.beneficiary_id,
            Some(1),
            "wrong-first-revision",
            "evidence-wrong-first-revision",
            &CoverageProjection::Complete {
                paid_intervals: vec![],
            },
        )
        .await,
        Err(StoreError::RevisionConflict {
            expected: Some(1),
            actual: None,
        })
    ));
    tx.commit()
        .await
        .expect("commit unrelated work after conflict");
    assert!(matches!(
        load(&fixture.pool, &fixture.beneficiary_id).await,
        Err(StoreError::ProjectionMissing)
    ));
    cleanup(&fixture).await;
}

#[tokio::test]
async fn simultaneous_first_publications_have_one_winner() {
    let Some(fixture) = Fixture::create().await else {
        return;
    };
    let barrier = Arc::new(Barrier::new(2));
    let first_pool = fixture.pool.clone();
    let second_pool = fixture.pool.clone();
    let beneficiary = fixture.beneficiary_id.clone();
    let first_barrier = barrier.clone();
    let second_barrier = barrier;
    let publish_one = async move {
        let mut tx = first_pool.begin().await.expect("begin first race");
        first_barrier.wait().await;
        let result = publish(
            &mut tx,
            &beneficiary,
            None,
            "race-one",
            "race-evidence-one",
            &CoverageProjection::Complete {
                paid_intervals: vec![paid("one", "personal", 0, 30 * DAY)],
            },
        )
        .await;
        match result {
            Ok(receipt) => {
                tx.commit().await.expect("commit first race");
                Ok(receipt)
            }
            Err(error) => {
                tx.rollback().await.expect("rollback first race");
                Err(error)
            }
        }
    };
    let beneficiary = fixture.beneficiary_id.clone();
    let publish_two = async move {
        let mut tx = second_pool.begin().await.expect("begin second race");
        second_barrier.wait().await;
        let result = publish(
            &mut tx,
            &beneficiary,
            None,
            "race-two",
            "race-evidence-two",
            &CoverageProjection::Complete {
                paid_intervals: vec![paid("two", "personal", 0, 30 * DAY)],
            },
        )
        .await;
        match result {
            Ok(receipt) => {
                tx.commit().await.expect("commit second race");
                Ok(receipt)
            }
            Err(error) => {
                tx.rollback().await.expect("rollback second race");
                Err(error)
            }
        }
    };
    let (first, second) = tokio::join!(publish_one, publish_two);
    let results = [first, second];
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Ok(receipt) if receipt.outcome == PublicationOutcome::Applied))
            .count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(StoreError::RevisionConflict { .. })))
            .count(),
        1
    );
    cleanup(&fixture).await;
}

#[tokio::test]
async fn simultaneous_first_conflicts_retry_after_empty_head_cleanup() {
    let Some(fixture) = Fixture::create().await else {
        return;
    };
    let barrier = Arc::new(Barrier::new(2));
    let first_pool = fixture.pool.clone();
    let second_pool = fixture.pool.clone();
    let first_beneficiary = fixture.beneficiary_id.clone();
    let second_beneficiary = fixture.beneficiary_id.clone();
    let first_barrier = barrier.clone();
    let second_barrier = barrier;
    let publish_one = async move {
        let mut tx = first_pool.begin().await.expect("begin first conflict race");
        first_barrier.wait().await;
        let result = publish(
            &mut tx,
            &first_beneficiary,
            Some(1),
            "first-conflict-one",
            "first-conflict-evidence-one",
            &CoverageProjection::Complete {
                paid_intervals: vec![],
            },
        )
        .await;
        tx.commit().await.expect("commit first conflict cleanup");
        result
    };
    let publish_two = async move {
        let mut tx = second_pool
            .begin()
            .await
            .expect("begin second conflict race");
        second_barrier.wait().await;
        let result = publish(
            &mut tx,
            &second_beneficiary,
            Some(1),
            "first-conflict-two",
            "first-conflict-evidence-two",
            &CoverageProjection::Complete {
                paid_intervals: vec![],
            },
        )
        .await;
        tx.commit().await.expect("commit second conflict cleanup");
        result
    };
    let (first, second) = tokio::join!(publish_one, publish_two);
    assert!(matches!(
        first,
        Err(StoreError::RevisionConflict {
            expected: Some(1),
            actual: None,
        })
    ));
    assert!(matches!(
        second,
        Err(StoreError::RevisionConflict {
            expected: Some(1),
            actual: None,
        })
    ));
    assert!(matches!(
        load(&fixture.pool, &fixture.beneficiary_id).await,
        Err(StoreError::ProjectionMissing)
    ));
    cleanup(&fixture).await;
}

#[tokio::test]
async fn simultaneous_replays_of_one_operation_have_one_application() {
    let Some(fixture) = Fixture::create().await else {
        return;
    };
    let barrier = Arc::new(Barrier::new(2));
    let first_pool = fixture.pool.clone();
    let second_pool = fixture.pool.clone();
    let first_beneficiary = fixture.beneficiary_id.clone();
    let second_beneficiary = fixture.beneficiary_id.clone();
    let first_barrier = barrier.clone();
    let second_barrier = barrier;
    let publish_one = async move {
        let mut tx = first_pool.begin().await.expect("begin first replay race");
        first_barrier.wait().await;
        let result = publish(
            &mut tx,
            &first_beneficiary,
            None,
            "same-operation",
            "same-evidence",
            &CoverageProjection::Complete {
                paid_intervals: vec![paid("same", "personal", 0, 30 * DAY)],
            },
        )
        .await;
        match result {
            Ok(receipt) => {
                tx.commit().await.expect("commit first replay race");
                Ok(receipt)
            }
            Err(error) => {
                tx.rollback().await.expect("rollback first replay race");
                Err(error)
            }
        }
    };
    let publish_two = async move {
        let mut tx = second_pool.begin().await.expect("begin second replay race");
        second_barrier.wait().await;
        let result = publish(
            &mut tx,
            &second_beneficiary,
            None,
            "same-operation",
            "same-evidence",
            &CoverageProjection::Complete {
                paid_intervals: vec![paid("same", "personal", 0, 30 * DAY)],
            },
        )
        .await;
        match result {
            Ok(receipt) => {
                tx.commit().await.expect("commit second replay race");
                Ok(receipt)
            }
            Err(error) => {
                tx.rollback().await.expect("rollback second replay race");
                Err(error)
            }
        }
    };
    let (first, second) = tokio::join!(publish_one, publish_two);
    let results = [first, second];
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Ok(receipt) if receipt.outcome == PublicationOutcome::Applied))
            .count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Ok(receipt) if receipt.outcome == PublicationOutcome::AlreadyApplied))
            .count(),
        1
    );
    cleanup(&fixture).await;
}

#[tokio::test]
async fn unavailable_projection_blocks_load_until_complete_revision() {
    let Some(fixture) = Fixture::create().await else {
        return;
    };
    committed_publish(
        &fixture,
        None,
        "unavailable-op",
        "reconcile-1",
        &CoverageProjection::Unavailable {
            reason: UnavailableReason::NeedsReconciliation,
        },
    )
    .await
    .expect("publish unavailable projection");
    assert!(matches!(
        load(&fixture.pool, &fixture.beneficiary_id).await,
        Err(StoreError::ProjectionUnavailable(
            UnavailableReason::NeedsReconciliation
        ))
    ));

    committed_publish(
        &fixture,
        Some(1),
        "complete-op",
        "reconcile-2",
        &CoverageProjection::Complete {
            paid_intervals: vec![paid("paid", "personal", 0, 30 * DAY)],
        },
    )
    .await
    .expect("publish recovered projection");
    assert_eq!(
        load(&fixture.pool, &fixture.beneficiary_id)
            .await
            .unwrap()
            .revision,
        2
    );
    cleanup(&fixture).await;
}

#[tokio::test]
async fn rollback_leaves_missing_projection_and_retry_can_apply() {
    let Some(fixture) = Fixture::create().await else {
        return;
    };
    let mut tx = fixture.pool.begin().await.expect("begin rollback");
    publish(
        &mut tx,
        &fixture.beneficiary_id,
        None,
        "rolled-back",
        "evidence",
        &CoverageProjection::Complete {
            paid_intervals: vec![paid("paid", "personal", 0, 30 * DAY)],
        },
    )
    .await
    .expect("publish before rollback");
    tx.rollback().await.expect("rollback publication");
    assert!(matches!(
        load(&fixture.pool, &fixture.beneficiary_id).await,
        Err(StoreError::ProjectionMissing)
    ));
    committed_publish(
        &fixture,
        None,
        "rolled-back",
        "evidence",
        &CoverageProjection::Complete {
            paid_intervals: vec![paid("paid", "personal", 0, 30 * DAY)],
        },
    )
    .await
    .expect("retry after rollback");
    cleanup(&fixture).await;
}

#[tokio::test]
async fn renewal_correction_replaces_recovery_without_rewriting_old_revision() {
    let Some(fixture) = Fixture::create().await else {
        return;
    };
    let old = CoverageProjection::Complete {
        paid_intervals: vec![recovery("paid", "personal", 0, 30 * DAY, "renewal-1")],
    };
    committed_publish(&fixture, None, "old", "evidence-old", &old)
        .await
        .expect("publish old recovery");
    let renewed = CoverageProjection::Complete {
        paid_intervals: vec![
            paid("paid", "personal", 0, 30 * DAY),
            paid("renewed", "personal", 30 * DAY, 60 * DAY),
        ],
    };
    let mut rolled_back = fixture
        .pool
        .begin()
        .await
        .expect("begin rolled-back correction");
    publish(
        &mut rolled_back,
        &fixture.beneficiary_id,
        Some(1),
        "rolled-back-correction",
        "evidence-rolled-back-correction",
        &renewed,
    )
    .await
    .expect("publish rolled-back correction");
    rolled_back.rollback().await.expect("rollback correction");
    assert_eq!(
        load(&fixture.pool, &fixture.beneficiary_id)
            .await
            .expect("read revision after rollback")
            .revision,
        1
    );
    let mut pending = fixture.pool.begin().await.expect("begin confirmed renewal");
    publish(
        &mut pending,
        &fixture.beneficiary_id,
        Some(1),
        "renewed",
        "evidence-renewed",
        &renewed,
    )
    .await
    .expect("publish confirmed renewal");
    assert_eq!(
        load(&fixture.pool, &fixture.beneficiary_id)
            .await
            .expect("read old committed revision")
            .revision,
        1
    );
    pending.commit().await.expect("commit confirmed renewal");
    let loaded = load(&fixture.pool, &fixture.beneficiary_id)
        .await
        .expect("load renewed coverage");
    let decision = evaluate(&loaded.coverage, 31 * DAY).unwrap();
    assert_eq!(decision.state, CoverageState::Paid);
    assert_eq!(decision.active_until, Some(60 * DAY));
    assert_eq!(
        evaluate(&loaded.coverage, 60 * DAY).unwrap().export_until,
        Some(90 * DAY)
    );

    let replay = committed_publish(&fixture, None, "old", "evidence-old", &old)
        .await
        .expect("replay old recovery");
    assert_eq!(replay.outcome, PublicationOutcome::AlreadyApplied);
    assert_eq!(
        load(&fixture.pool, &fixture.beneficiary_id)
            .await
            .unwrap()
            .revision,
        2
    );
    cleanup(&fixture).await;
}

#[tokio::test]
async fn future_max_timestamp_can_be_stored_without_eager_export_overflow() {
    let Some(fixture) = Fixture::create().await else {
        return;
    };
    committed_publish(
        &fixture,
        None,
        "future-max",
        "evidence-max",
        &CoverageProjection::Complete {
            paid_intervals: vec![paid("future", "personal", i64::MAX - 1, i64::MAX)],
        },
    )
    .await
    .expect("publish future max interval");
    let loaded = load(&fixture.pool, &fixture.beneficiary_id)
        .await
        .expect("load future max interval");
    assert_eq!(
        evaluate(&loaded.coverage, 5).unwrap().state,
        CoverageState::Free
    );
    cleanup(&fixture).await;
}

#[tokio::test]
async fn stored_fact_count_mismatch_fails_closed() {
    let Some(fixture) = Fixture::create().await else {
        return;
    };
    committed_publish(
        &fixture,
        None,
        "corrupt-count",
        "evidence-corrupt",
        &CoverageProjection::Complete {
            paid_intervals: vec![paid("paid", "personal", 0, 30 * DAY)],
        },
    )
    .await
    .expect("publish projection to corrupt");
    sqlx::query(
        "UPDATE cloud_coverage_revisions SET fact_count = 0 \
         WHERE beneficiary_id = $1 AND revision = 1",
    )
    .bind(&fixture.beneficiary_id)
    .execute(&fixture.pool)
    .await
    .expect("corrupt stored fact count");
    assert!(matches!(
        load(&fixture.pool, &fixture.beneficiary_id).await,
        Err(StoreError::CorruptProjection(_))
    ));
    cleanup(&fixture).await;
}

#[tokio::test]
async fn unavailable_projection_with_stored_facts_fails_closed() {
    let Some(fixture) = Fixture::create().await else {
        return;
    };
    committed_publish(
        &fixture,
        None,
        "corrupt-unavailable",
        "evidence-corrupt-unavailable",
        &CoverageProjection::Unavailable {
            reason: UnavailableReason::NeedsReconciliation,
        },
    )
    .await
    .expect("publish unavailable projection to corrupt");
    sqlx::query(
        "INSERT INTO cloud_coverage_revision_facts \
         (beneficiary_id, revision, coverage_id, source_id, starts_at, paid_until) \
         VALUES ($1, 1, 'unexpected', 'source', 0, 10)",
    )
    .bind(&fixture.beneficiary_id)
    .execute(&fixture.pool)
    .await
    .expect("insert unexpected unavailable fact");
    assert!(matches!(
        load(&fixture.pool, &fixture.beneficiary_id).await,
        Err(StoreError::CorruptProjection(_))
    ));
    cleanup(&fixture).await;
}

#[tokio::test]
async fn loader_pause_acknowledges_between_metadata_and_facts_reads() {
    let Some(fixture) = Fixture::create().await else {
        return;
    };
    let base = committed_publish(
        &fixture,
        None,
        "pause-base",
        "pause-base-evidence",
        &CoverageProjection::Complete {
            paid_intervals: vec![
                paid("pause-old-a", "personal", 0, 30 * DAY),
                paid("pause-old-b", "personal", 30 * DAY, 60 * DAY),
            ],
        },
    )
    .await
    .expect("publish pause base");
    assert_eq!(base.revision, 1);

    let mut owner = RaceTaskOwner::new();
    let cleanup_pool = fixture.pool.clone();
    let cleanup_beneficiary = fixture.beneficiary_id.clone();
    owner.register_cleanup(move || async move {
        cleanup_result_for(&cleanup_pool, &cleanup_beneficiary).await
    });
    let result = run_with_context(
        &mut owner,
        |owner| {
            Box::pin(async move {
                let _observation = acquire_observation().await;
                let next = CoverageProjection::Complete {
                    paid_intervals: vec![
                        paid("pause-new-a", "personal", 0, 30 * DAY),
                        paid("pause-new-b", "personal", 30 * DAY, 60 * DAY),
                    ],
                };
                let mut probe = fixture
                    .pool
                    .begin()
                    .await
                    .expect("begin invisibility probe");
                publish(
                    &mut probe,
                    &fixture.beneficiary_id,
                    Some(1),
                    "pause-next",
                    "pause-next-evidence",
                    &next,
                )
                .await
                .expect("publish probe revision");
                let before = load(&fixture.pool, &fixture.beneficiary_id)
                    .await
                    .expect("load before lock");
                assert_eq!(before.revision, 1);
                probe
                    .rollback()
                    .await
                    .expect("roll back invisibility probe");

                let (mut writer, held) = begin_locked_writer(
                    &fixture.pool,
                    &fixture.beneficiary_id,
                    Some(1),
                    "pause-next",
                    "pause-next-evidence",
                    &next,
                    "cloud_coverage_revision_facts",
                )
                .await;
                assert_eq!(held.revision, 2);
                let writer_pid = transaction_pid(&mut writer).await;

                let loader_app = format!("coverage-loader-pause-{}", Uuid::new_v4().simple());
                let loader_pool = loader_pool(&loader_app).await;
                let load_pool = loader_pool.clone();
                let beneficiary_id = fixture.beneficiary_id.clone();
                let mut loader =
                    Some(owner.spawn(async move { load(&load_pool, &beneficiary_id).await }));
                let loader_pid =
                    wait_for_blocked_backend(&fixture.pool, &loader_app, writer_pid).await;
                assert_ne!(loader_pid, writer_pid);
                let loader_query: String =
                    sqlx::query_scalar("SELECT query FROM pg_stat_activity WHERE pid = $1")
                        .bind(loader_pid)
                        .fetch_one(&fixture.pool)
                        .await
                        .expect("read paused loader query");
                assert!(
                    loader_query.contains("FROM cloud_coverage_revision_facts"),
                    "paused loader waits at the facts read, observed: {loader_query}"
                );

                writer.commit().await.expect("commit held publication");
                let paused = receive_owned(&mut loader, "paused loader")
                    .await
                    .expect("paused loader task completed")
                    .expect("paused load succeeded");
                assert_eq!(paused.revision, 1);
                loader_pool.close().await;
                let after = load(&fixture.pool, &fixture.beneficiary_id)
                    .await
                    .expect("load after commit");
                assert_eq!(after.revision, 2);
                Ok(())
            })
        },
        || async { Ok::<(), String>(()) },
    )
    .await;
    result.expect("supervised loader pause");
}

#[tokio::test]
async fn complete_load_returns_one_committed_snapshot_across_writer_commit() {
    let Some(fixture) = Fixture::create().await else {
        return;
    };
    let old_intervals = vec![
        paid("snap-old-A", "personal", 0, 30 * DAY),
        recovery("snap-old-b", "sponsor", 30 * DAY, 60 * DAY, "renewal-7"),
        paid("snap-old-c", "personal", 60 * DAY, 90 * DAY),
    ];
    let mut published_old = old_intervals.clone();
    published_old.reverse();
    let base = committed_publish(
        &fixture,
        None,
        "snapshot-base",
        "snapshot-base-evidence",
        &CoverageProjection::Complete {
            paid_intervals: published_old,
        },
    )
    .await
    .expect("publish snapshot base");
    assert_eq!(base.revision, 1);
    let expected_old = LoadedCoverage {
        revision: 1,
        coverage: PersonCoverage {
            beneficiary_id: fixture.beneficiary_id.clone(),
            paid_intervals: old_intervals,
        },
    };

    let mut owner = RaceTaskOwner::new();
    let cleanup_pool = fixture.pool.clone();
    let cleanup_beneficiary = fixture.beneficiary_id.clone();
    owner.register_cleanup(move || async move {
        cleanup_result_for(&cleanup_pool, &cleanup_beneficiary).await
    });
    let result = run_with_context(
        &mut owner,
        |owner| {
            Box::pin(async move {
                let _observation = acquire_observation().await;
                let next = CoverageProjection::Complete {
                    paid_intervals: vec![
                        paid("snap-new-a", "sponsor", 90 * DAY, 120 * DAY),
                        paid("snap-new-B", "personal", 120 * DAY, 150 * DAY),
                    ],
                };
                let mut probe = fixture
                    .pool
                    .begin()
                    .await
                    .expect("begin invisibility probe");
                publish(
                    &mut probe,
                    &fixture.beneficiary_id,
                    Some(1),
                    "snapshot-next",
                    "snapshot-next-evidence",
                    &next,
                )
                .await
                .expect("publish probe revision");
                let before = load(&fixture.pool, &fixture.beneficiary_id)
                    .await
                    .expect("load before lock");
                assert_eq!(before, expected_old);
                probe
                    .rollback()
                    .await
                    .expect("roll back invisibility probe");

                let (mut writer, held) = begin_locked_writer(
                    &fixture.pool,
                    &fixture.beneficiary_id,
                    Some(1),
                    "snapshot-next",
                    "snapshot-next-evidence",
                    &next,
                    "cloud_coverage_revision_facts",
                )
                .await;
                assert_eq!(held.revision, 2);
                let writer_pid = transaction_pid(&mut writer).await;

                let loader_app = format!("coverage-loader-snapshot-{}", Uuid::new_v4().simple());
                let loader_pool = loader_pool(&loader_app).await;
                let load_pool = loader_pool.clone();
                let beneficiary_id = fixture.beneficiary_id.clone();
                let mut loader =
                    Some(owner.spawn(async move { load(&load_pool, &beneficiary_id).await }));
                let loader_pid =
                    wait_for_blocked_backend(&fixture.pool, &loader_app, writer_pid).await;
                assert_ne!(loader_pid, writer_pid);

                writer.commit().await.expect("commit held publication");
                let paused = receive_owned(&mut loader, "paused loader")
                    .await
                    .expect("paused loader task completed")
                    .expect("paused load succeeded");
                assert_eq!(paused, expected_old);
                loader_pool.close().await;
                assert!(
                    paused
                        .coverage
                        .paid_intervals
                        .iter()
                        .all(|interval| interval.coverage_id.starts_with("snap-old-")),
                    "paused load carries no replacement facts"
                );

                let after = load(&fixture.pool, &fixture.beneficiary_id)
                    .await
                    .expect("load after commit");
                let expected_new = LoadedCoverage {
                    revision: 2,
                    coverage: PersonCoverage {
                        beneficiary_id: fixture.beneficiary_id.clone(),
                        paid_intervals: vec![
                            paid("snap-new-B", "personal", 120 * DAY, 150 * DAY),
                            paid("snap-new-a", "sponsor", 90 * DAY, 120 * DAY),
                        ],
                    },
                };
                assert_eq!(after, expected_new);
                assert!(
                    after
                        .coverage
                        .paid_intervals
                        .iter()
                        .all(|interval| interval.coverage_id.starts_with("snap-new-")),
                    "replacement load carries no old facts"
                );

                let head: i64 = sqlx::query_scalar(
                    "SELECT current_revision FROM cloud_coverage_heads WHERE beneficiary_id = $1",
                )
                .bind(&fixture.beneficiary_id)
                .fetch_one(&fixture.pool)
                .await
                .expect("read snapshot head");
                assert_eq!(head, 2);
                let metadata: (String, String, String, i64) = sqlx::query_as(
                    "SELECT operation_id, evidence_reference, status, fact_count \
                     FROM cloud_coverage_revisions WHERE beneficiary_id = $1 AND revision = 2",
                )
                .bind(&fixture.beneficiary_id)
                .fetch_one(&fixture.pool)
                .await
                .expect("read snapshot metadata");
                assert_eq!(
                    metadata,
                    (
                        "snapshot-next".into(),
                        "snapshot-next-evidence".into(),
                        "complete".into(),
                        2
                    )
                );
                let stored_ids: Vec<String> = sqlx::query_scalar(
                    "SELECT coverage_id FROM cloud_coverage_revision_facts \
                     WHERE beneficiary_id = $1 AND revision = 2 \
                     ORDER BY coverage_id COLLATE \"C\"",
                )
                .bind(&fixture.beneficiary_id)
                .fetch_all(&fixture.pool)
                .await
                .expect("read snapshot facts");
                assert_eq!(
                    stored_ids,
                    vec!["snap-new-B".to_string(), "snap-new-a".to_string()]
                );
                Ok(())
            })
        },
        || async { Ok::<(), String>(()) },
    )
    .await;
    result.expect("supervised snapshot interleaving");
}

#[tokio::test]
async fn unavailable_load_never_mixes_with_committed_replacement() {
    let Some(fixture) = Fixture::create().await else {
        return;
    };
    committed_publish(
        &fixture,
        None,
        "unavailable-base",
        "unavailable-base-evidence",
        &CoverageProjection::Unavailable {
            reason: UnavailableReason::NeedsReconciliation,
        },
    )
    .await
    .expect("publish unavailable base");

    let mut owner = RaceTaskOwner::new();
    let cleanup_pool = fixture.pool.clone();
    let cleanup_beneficiary = fixture.beneficiary_id.clone();
    owner.register_cleanup(move || async move {
        cleanup_result_for(&cleanup_pool, &cleanup_beneficiary).await
    });
    let result = run_with_context(
        &mut owner,
        |owner| {
            Box::pin(async move {
                let _observation = acquire_observation().await;
                let next = CoverageProjection::Complete {
                    paid_intervals: vec![
                        paid("replace-a", "personal", 0, 30 * DAY),
                        paid("replace-b", "sponsor", 30 * DAY, 60 * DAY),
                    ],
                };
                let mut probe = fixture
                    .pool
                    .begin()
                    .await
                    .expect("begin invisibility probe");
                publish(
                    &mut probe,
                    &fixture.beneficiary_id,
                    Some(1),
                    "unavailable-replacement",
                    "unavailable-replacement-evidence",
                    &next,
                )
                .await
                .expect("publish probe replacement");
                // The probe load early-returns without committing, leaving its rollback
                // queued on its pooled connection; close the check pool so the lingering
                // table lock is released before the writer takes its observation lock.
                let check_pool = loader_pool(&format!(
                    "coverage-loader-unavail-check-{}",
                    Uuid::new_v4().simple()
                ))
                .await;
                assert!(matches!(
                    load(&check_pool, &fixture.beneficiary_id).await,
                    Err(StoreError::ProjectionUnavailable(
                        UnavailableReason::NeedsReconciliation
                    ))
                ));
                check_pool.close().await;
                probe
                    .rollback()
                    .await
                    .expect("roll back invisibility probe");

                let (mut writer, held) = begin_locked_writer(
                    &fixture.pool,
                    &fixture.beneficiary_id,
                    Some(1),
                    "unavailable-replacement",
                    "unavailable-replacement-evidence",
                    &next,
                    "cloud_coverage_revisions",
                )
                .await;
                assert_eq!(held.revision, 2);
                let writer_pid = transaction_pid(&mut writer).await;

                let loader_app = format!("coverage-loader-unavailable-{}", Uuid::new_v4().simple());
                let loader_pool = loader_pool(&loader_app).await;
                let load_pool = loader_pool.clone();
                let beneficiary_id = fixture.beneficiary_id.clone();
                let mut loader =
                    Some(owner.spawn(async move { load(&load_pool, &beneficiary_id).await }));
                let loader_pid =
                    wait_for_blocked_backend(&fixture.pool, &loader_app, writer_pid).await;
                assert_ne!(loader_pid, writer_pid);
                let loader_query: String =
                    sqlx::query_scalar("SELECT query FROM pg_stat_activity WHERE pid = $1")
                        .bind(loader_pid)
                        .fetch_one(&fixture.pool)
                        .await
                        .expect("read paused loader query");
                assert!(
                    loader_query.contains("FROM cloud_coverage_revisions"),
                    "paused loader waits at the metadata read, observed: {loader_query}"
                );

                writer.commit().await.expect("commit held replacement");
                let paused = receive_owned(&mut loader, "paused loader")
                    .await
                    .expect("paused loader task completed");
                loader_pool.close().await;
                assert!(matches!(
                    paused,
                    Err(StoreError::ProjectionUnavailable(
                        UnavailableReason::NeedsReconciliation
                    ))
                ));

                let after = load(&fixture.pool, &fixture.beneficiary_id)
                    .await
                    .expect("load after commit");
                assert_eq!(
                    after,
                    LoadedCoverage {
                        revision: 2,
                        coverage: PersonCoverage {
                            beneficiary_id: fixture.beneficiary_id.clone(),
                            paid_intervals: vec![
                                paid("replace-a", "personal", 0, 30 * DAY),
                                paid("replace-b", "sponsor", 30 * DAY, 60 * DAY),
                            ],
                        },
                    }
                );
                Ok(())
            })
        },
        || async { Ok::<(), String>(()) },
    )
    .await;
    result.expect("supervised unavailable interleaving");
}

#[tokio::test]
async fn rolled_back_publication_preserves_old_snapshot_and_retry_applies_once() {
    let Some(fixture) = Fixture::create().await else {
        return;
    };
    let Some(unrelated) = Fixture::create().await else {
        return;
    };
    let old_intervals = vec![
        paid("rollback-old-a", "personal", 0, 30 * DAY),
        paid("rollback-old-b", "sponsor", 30 * DAY, 60 * DAY),
    ];
    committed_publish(
        &fixture,
        None,
        "rollback-base",
        "rollback-base-evidence",
        &CoverageProjection::Complete {
            paid_intervals: old_intervals.clone(),
        },
    )
    .await
    .expect("publish rollback base");
    committed_publish(
        &unrelated,
        None,
        "rollback-unrelated-base",
        "rollback-unrelated-base-evidence",
        &CoverageProjection::Complete {
            paid_intervals: vec![paid("rollback-unrelated-a", "personal", 0, 30 * DAY)],
        },
    )
    .await
    .expect("publish unrelated base");

    let mut owner = RaceTaskOwner::new();
    for (pool, beneficiary_id) in [
        (fixture.pool.clone(), fixture.beneficiary_id.clone()),
        (unrelated.pool.clone(), unrelated.beneficiary_id.clone()),
    ] {
        owner.register_cleanup(
            move || async move { cleanup_result_for(&pool, &beneficiary_id).await },
        );
    }
    let result = run_with_context(
        &mut owner,
        |owner| {
            Box::pin(async move {
                let _observation = acquire_observation().await;
                let next = CoverageProjection::Complete {
                    paid_intervals: vec![paid("rollback-new-a", "personal", 0, 30 * DAY)],
                };
                let mut probe = fixture
                    .pool
                    .begin()
                    .await
                    .expect("begin invisibility probe");
                publish(
                    &mut probe,
                    &fixture.beneficiary_id,
                    Some(1),
                    "rollback-next",
                    "rollback-next-evidence",
                    &next,
                )
                .await
                .expect("publish probe revision");
                let before = load(&fixture.pool, &fixture.beneficiary_id)
                    .await
                    .expect("load before lock");
                assert_eq!(before.revision, 1);
                assert_eq!(before.coverage.paid_intervals, old_intervals);
                probe
                    .rollback()
                    .await
                    .expect("roll back invisibility probe");

                let (mut writer, held) = begin_locked_writer(
                    &fixture.pool,
                    &fixture.beneficiary_id,
                    Some(1),
                    "rollback-next",
                    "rollback-next-evidence",
                    &next,
                    "cloud_coverage_revision_facts",
                )
                .await;
                assert_eq!(held.revision, 2);
                let writer_pid = transaction_pid(&mut writer).await;

                let loader_app = format!("coverage-loader-rollback-{}", Uuid::new_v4().simple());
                let loader_pool = loader_pool(&loader_app).await;
                let load_pool = loader_pool.clone();
                let beneficiary_id = fixture.beneficiary_id.clone();
                let mut loader =
                    Some(owner.spawn(async move { load(&load_pool, &beneficiary_id).await }));
                let loader_pid =
                    wait_for_blocked_backend(&fixture.pool, &loader_app, writer_pid).await;
                assert_ne!(loader_pid, writer_pid);

                let unrelated_pool = unrelated.pool.clone();
                let unrelated_beneficiary = unrelated.beneficiary_id.clone();
                let mut progress = Some(owner.spawn(async move {
                    let mut tx = unrelated_pool
                        .begin()
                        .await
                        .expect("begin unrelated publication");
                    let receipt = publish(
                        &mut tx,
                        &unrelated_beneficiary,
                        Some(1),
                        "rollback-unrelated-next",
                        "rollback-unrelated-next-evidence",
                        &CoverageProjection::Complete {
                            paid_intervals: vec![paid(
                                "rollback-unrelated-b",
                                "sponsor",
                                30 * DAY,
                                60 * DAY,
                            )],
                        },
                    )
                    .await
                    .expect("publish unrelated revision");
                    tx.commit().await.expect("commit unrelated publication");
                    let loaded = load(&unrelated_pool, &unrelated_beneficiary)
                        .await
                        .expect("load unrelated revision");
                    (receipt, loaded)
                }));

                writer.rollback().await.expect("roll back held publication");
                let paused = receive_owned(&mut loader, "paused loader")
                    .await
                    .expect("paused loader task completed")
                    .expect("paused load succeeded");
                assert_eq!(paused.revision, 1);
                assert_eq!(paused.coverage.paid_intervals, old_intervals);
                loader_pool.close().await;

                let (receipt, unrelated_loaded) =
                    receive_owned(&mut progress, "unrelated progress")
                        .await
                        .expect("unrelated progress task completed");
                assert_eq!(receipt.outcome, PublicationOutcome::Applied);
                assert_eq!(receipt.revision, 2);
                assert_eq!(unrelated_loaded.revision, 2);

                let aborted: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM cloud_coverage_revisions \
                     WHERE beneficiary_id = $1 AND operation_id = 'rollback-next'",
                )
                .bind(&fixture.beneficiary_id)
                .fetch_one(&fixture.pool)
                .await
                .expect("count aborted revision");
                assert_eq!(aborted, 0);

                let retry = committed_publish(
                    &fixture,
                    Some(1),
                    "rollback-next",
                    "rollback-next-evidence",
                    &CoverageProjection::Complete {
                        paid_intervals: vec![paid("rollback-new-a", "personal", 0, 30 * DAY)],
                    },
                )
                .await
                .expect("retry after rollback");
                assert_eq!(retry.outcome, PublicationOutcome::Applied);
                assert_eq!(retry.revision, 2);
                let head: Option<i64> = sqlx::query_scalar(
                    "SELECT current_revision FROM cloud_coverage_heads WHERE beneficiary_id = $1",
                )
                .bind(&fixture.beneficiary_id)
                .fetch_one(&fixture.pool)
                .await
                .expect("read retry head");
                assert_eq!(head, Some(2));
                let orphans: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM cloud_coverage_revision_facts AS facts \
                     WHERE beneficiary_id = $1 AND NOT EXISTS (
                         SELECT 1 FROM cloud_coverage_revisions AS revisions \
                         WHERE revisions.beneficiary_id = facts.beneficiary_id \
                         AND revisions.revision = facts.revision
                     )",
                )
                .bind(&fixture.beneficiary_id)
                .fetch_one(&fixture.pool)
                .await
                .expect("count orphan facts");
                assert_eq!(orphans, 0);
                Ok(())
            })
        },
        || async { Ok::<(), String>(()) },
    )
    .await;
    result.expect("supervised rollback interleaving");
}

#[tokio::test]
async fn corrupt_projection_fails_closed_while_unrelated_beneficiary_progresses() {
    let Some(missing) = Fixture::create().await else {
        return;
    };
    let Some(miscounted) = Fixture::create().await else {
        return;
    };
    let Some(status) = Fixture::create().await else {
        return;
    };
    let Some(unrelated) = Fixture::create().await else {
        return;
    };
    for fixture in [&missing, &miscounted, &status] {
        committed_publish(
            fixture,
            None,
            "corrupt-base",
            "corrupt-base-evidence",
            &CoverageProjection::Complete {
                paid_intervals: vec![paid("corrupt-fact", "personal", 0, 30 * DAY)],
            },
        )
        .await
        .expect("publish corrupt base");
    }
    committed_publish(
        &unrelated,
        None,
        "corrupt-unrelated-base",
        "corrupt-unrelated-base-evidence",
        &CoverageProjection::Complete {
            paid_intervals: vec![paid("corrupt-unrelated-a", "personal", 0, 30 * DAY)],
        },
    )
    .await
    .expect("publish unrelated base");
    sqlx::query(
        "UPDATE cloud_coverage_revisions SET fact_count = 999 \
         WHERE beneficiary_id = $1 AND revision = 1",
    )
    .bind(&miscounted.beneficiary_id)
    .execute(&miscounted.pool)
    .await
    .expect("corrupt stored fact count");

    let mut owner = RaceTaskOwner::new();
    for (pool, beneficiary_id) in [
        (missing.pool.clone(), missing.beneficiary_id.clone()),
        (miscounted.pool.clone(), miscounted.beneficiary_id.clone()),
        (status.pool.clone(), status.beneficiary_id.clone()),
        (unrelated.pool.clone(), unrelated.beneficiary_id.clone()),
    ] {
        owner.register_cleanup(
            move || async move { cleanup_result_for(&pool, &beneficiary_id).await },
        );
    }
    let result = run_with_context(
        &mut owner,
        |owner| {
            Box::pin(async move {
                let unrelated_pool = unrelated.pool.clone();
                let unrelated_beneficiary = unrelated.beneficiary_id.clone();
                let mut progress = Some(owner.spawn(async move {
                    let mut tx = unrelated_pool
                        .begin()
                        .await
                        .expect("begin unrelated publication");
                    let receipt = publish(
                        &mut tx,
                        &unrelated_beneficiary,
                        Some(1),
                        "corrupt-unrelated-next",
                        "corrupt-unrelated-next-evidence",
                        &CoverageProjection::Complete {
                            paid_intervals: vec![
                                paid("corrupt-unrelated-b", "sponsor", 30 * DAY, 60 * DAY),
                            ],
                        },
                    )
                    .await
                    .expect("publish unrelated revision");
                    tx.commit().await.expect("commit unrelated publication");
                    let loaded = load(&unrelated_pool, &unrelated_beneficiary)
                        .await
                        .expect("load unrelated revision");
                    (receipt, loaded)
                }));

                // A head pointing at a missing revision cannot exist durably: the
                // head-to-revision foreign key rejects the corrupting write, so the
                // loader's missing-revision arm stays unreachable and the projection
                // keeps loading.
                let missing_before =
                    projection_snapshot(&missing.pool, &missing.beneficiary_id).await;
                let missing_head = sqlx::query(
                    "UPDATE cloud_coverage_heads SET current_revision = 999 \
                     WHERE beneficiary_id = $1",
                )
                .bind(&missing.beneficiary_id)
                .execute(&missing.pool)
                .await;
                assert!(
                    matches!(&missing_head, Err(sqlx::Error::Database(error)) if error.code().as_deref() == Some("23503")),
                    "head at missing revision violates the head foreign key, observed: {missing_head:?}"
                );
                assert_eq!(
                    projection_snapshot(&missing.pool, &missing.beneficiary_id).await,
                    missing_before
                );
                let missing_loaded = load(&missing.pool, &missing.beneficiary_id)
                    .await
                    .expect("load after rejected head corruption");
                assert_eq!(missing_loaded.revision, 1);

                let miscounted_before =
                    projection_snapshot(&miscounted.pool, &miscounted.beneficiary_id).await;
                let miscounted_result = load(&miscounted.pool, &miscounted.beneficiary_id).await;
                assert!(matches!(
                    miscounted_result,
                    Err(StoreError::CorruptProjection(message))
                        if message == "revision 1 declares 999 facts but stores 1"
                ));
                assert_eq!(
                    projection_snapshot(&miscounted.pool, &miscounted.beneficiary_id).await,
                    miscounted_before
                );

                let status_before =
                    projection_snapshot(&status.pool, &status.beneficiary_id).await;
                let invalid_status = sqlx::query(
                    "UPDATE cloud_coverage_revisions SET status = 'bogus' \
                     WHERE beneficiary_id = $1 AND revision = 1",
                )
                .bind(&status.beneficiary_id)
                .execute(&status.pool)
                .await;
                assert!(
                    matches!(&invalid_status, Err(sqlx::Error::Database(error)) if error.code().as_deref() == Some("23514")),
                    "invalid status violates the status check, observed: {invalid_status:?}"
                );
                assert_eq!(
                    projection_snapshot(&status.pool, &status.beneficiary_id).await,
                    status_before
                );

                let (receipt, unrelated_loaded) = receive_owned(&mut progress, "unrelated progress")
                    .await
                    .expect("unrelated progress task completed");
                assert_eq!(receipt.outcome, PublicationOutcome::Applied);
                assert_eq!(receipt.revision, 2);
                assert_eq!(unrelated_loaded.revision, 2);
                assert_eq!(
                    unrelated_loaded.coverage.paid_intervals,
                    vec![paid("corrupt-unrelated-b", "sponsor", 30 * DAY, 60 * DAY)]
                );
                Ok(())
            })
        },
        || async { Ok::<(), String>(()) },
    )
    .await;
    result.expect("supervised corruption controls");
}

#[tokio::test]
async fn max_ending_interval_round_trip_defers_export_overflow_until_evaluation() {
    let Some(fixture) = Fixture::create().await else {
        return;
    };
    let receipt = committed_publish(
        &fixture,
        None,
        "max-round-trip",
        "max-round-trip-evidence",
        &CoverageProjection::Complete {
            paid_intervals: vec![paid("max-fact", "personal", i64::MAX - 1, i64::MAX)],
        },
    )
    .await
    .expect("publish max-ending interval");
    assert_eq!(receipt.revision, 1);

    let loaded = load(&fixture.pool, &fixture.beneficiary_id)
        .await
        .expect("load max-ending interval");
    assert_eq!(loaded.revision, 1);
    assert_eq!(
        evaluate(&loaded.coverage, i64::MAX - 1).unwrap(),
        CoverageDecision {
            state: CoverageState::Paid,
            active_until: Some(i64::MAX),
            recovery_until: None,
            export_until: None,
        }
    );
    assert_eq!(
        evaluate(&loaded.coverage, i64::MAX).unwrap_err(),
        InvalidCoverage::ExportDeadlineOverflow
    );

    let stored: (i64, i64) = sqlx::query_as(
        "SELECT starts_at, paid_until FROM cloud_coverage_revision_facts \
         WHERE beneficiary_id = $1 AND revision = 1",
    )
    .bind(&fixture.beneficiary_id)
    .fetch_one(&fixture.pool)
    .await
    .expect("read stored max fact");
    assert_eq!(stored, (i64::MAX - 1, i64::MAX));
    let reloaded = load(&fixture.pool, &fixture.beneficiary_id)
        .await
        .expect("reload max-ending interval after overflow evaluation");
    assert_eq!(reloaded, loaded);
    cleanup(&fixture).await;
}

#[tokio::test]
async fn recovery_overflow_through_publisher_writes_nothing_durable() {
    let Some(fixture) = Fixture::create().await else {
        return;
    };
    let Some(unrelated) = Fixture::create().await else {
        return;
    };
    committed_publish(
        &unrelated,
        None,
        "overflow-unrelated-base",
        "overflow-unrelated-base-evidence",
        &CoverageProjection::Complete {
            paid_intervals: vec![paid("overflow-unrelated-a", "personal", 0, 30 * DAY)],
        },
    )
    .await
    .expect("publish unrelated base");

    let mut owner = RaceTaskOwner::new();
    for (pool, beneficiary_id) in [
        (fixture.pool.clone(), fixture.beneficiary_id.clone()),
        (unrelated.pool.clone(), unrelated.beneficiary_id.clone()),
    ] {
        owner.register_cleanup(
            move || async move { cleanup_result_for(&pool, &beneficiary_id).await },
        );
    }
    let result = run_with_context(
        &mut owner,
        |owner| {
            Box::pin(async move {
                let unrelated_pool = unrelated.pool.clone();
                let unrelated_beneficiary = unrelated.beneficiary_id.clone();
                let mut progress = Some(owner.spawn(async move {
                    let mut tx = unrelated_pool
                        .begin()
                        .await
                        .expect("begin unrelated publication");
                    let receipt = publish(
                        &mut tx,
                        &unrelated_beneficiary,
                        Some(1),
                        "overflow-unrelated-next",
                        "overflow-unrelated-next-evidence",
                        &CoverageProjection::Complete {
                            paid_intervals: vec![paid(
                                "overflow-unrelated-b",
                                "sponsor",
                                30 * DAY,
                                60 * DAY,
                            )],
                        },
                    )
                    .await
                    .expect("publish unrelated revision");
                    tx.commit().await.expect("commit unrelated publication");
                    let loaded = load(&unrelated_pool, &unrelated_beneficiary)
                        .await
                        .expect("load unrelated revision");
                    (receipt, loaded)
                }));

                let before = projection_snapshot(&fixture.pool, &fixture.beneficiary_id).await;
                let mut tx = fixture.pool.begin().await.expect("begin overflow attempt");
                let attempt = publish(
                    &mut tx,
                    &fixture.beneficiary_id,
                    None,
                    "overflow-attempt",
                    "overflow-attempt-evidence",
                    &CoverageProjection::Complete {
                        paid_intervals: vec![recovery(
                            "overflow-fact",
                            "personal",
                            0,
                            i64::MAX,
                            "renewal-overflow",
                        )],
                    },
                )
                .await;
                assert!(matches!(
                    attempt,
                    Err(StoreError::InvalidCoverage(
                        InvalidCoverage::RecoveryDeadlineOverflow
                    ))
                ));
                tx.commit()
                    .await
                    .expect("commit after typed overflow error");
                assert_eq!(
                    projection_snapshot(&fixture.pool, &fixture.beneficiary_id).await,
                    before
                );
                let operations: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM cloud_coverage_revisions \
                     WHERE beneficiary_id = $1 AND operation_id = 'overflow-attempt'",
                )
                .bind(&fixture.beneficiary_id)
                .fetch_one(&fixture.pool)
                .await
                .expect("count overflow operation rows");
                assert_eq!(operations, 0);

                let (receipt, unrelated_loaded) =
                    receive_owned(&mut progress, "unrelated progress")
                        .await
                        .expect("unrelated progress task completed");
                assert_eq!(receipt.outcome, PublicationOutcome::Applied);
                assert_eq!(receipt.revision, 2);
                assert_eq!(unrelated_loaded.revision, 2);
                Ok(())
            })
        },
        || async { Ok::<(), String>(()) },
    )
    .await;
    result.expect("supervised overflow no-write");
}

#[tokio::test]
async fn canonical_ordering_is_bytewise_across_database_collations() {
    let Some(fixture) = Fixture::create().await else {
        return;
    };
    let mut owner = RaceTaskOwner::new();
    let cleanup_pool = fixture.pool.clone();
    let cleanup_beneficiary = fixture.beneficiary_id.clone();
    owner.register_cleanup(move || async move {
        cleanup_result_for(&cleanup_pool, &cleanup_beneficiary).await
    });
    let result = run_with_context(
        &mut owner,
        |owner| {
            Box::pin(async move {
                canonical_order_fixture(&fixture.pool, &fixture.beneficiary_id).await;

                let collation = select_linguistic_collation(&fixture.pool).await;
                let database_name = create_linguistic_database(&fixture.pool, &collation).await;
                let linguistic_pool =
                    open_linguistic_database(&fixture.pool, &database_name, &collation).await;
                let drop_pool = fixture.pool.clone();
                let drop_name = database_name.clone();
                let drop_pool_handle = linguistic_pool.clone();
                owner.register_cleanup(move || async move {
                    drop_linguistic_database(&drop_pool, &drop_name, drop_pool_handle).await
                });

                let beneficiary_id = format!("coverage-collation-test-{}", Uuid::new_v4().simple());
                let default_order =
                    canonical_order_fixture(&linguistic_pool, &beneficiary_id).await;
                let bytewise = vec![
                    "ord-B".to_string(),
                    "ord-D".to_string(),
                    "ord-a".to_string(),
                    "ord-c".to_string(),
                ];
                assert_ne!(
                    default_order, bytewise,
                    "linguistic collation orders facts non-bytewise, observed: {default_order:?}"
                );
                Ok(())
            })
        },
        || async { Ok::<(), String>(()) },
    )
    .await;
    result.expect("supervised collation ordering");
}
