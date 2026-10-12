//! Database-backed checks for explicit retention scope and dry-run safety.

use sqlx::postgres::PgConnectOptions;
use sqlx::{PgPool, Row};
use std::str::FromStr;
use std::sync::OnceLock;
use tokio::sync::Mutex;

use sotto_server::db;
use sotto_server::retention::{
    self, EnqueueOutcome, RetentionIntent, RetentionMode, RetentionState, ScopeItem, ScopeKind,
};

fn retention_test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

async fn pool_or_skip() -> Option<PgPool> {
    if std::env::var("SOTTO_RUN_DB_TESTS").as_deref() != Ok("1") {
        eprintln!("skipping retention tests: set SOTTO_RUN_DB_TESTS=1");
        return None;
    }
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL for opted-in database tests");
    let options = PgConnectOptions::from_str(&url).expect("parse DATABASE_URL");
    assert!(
        matches!(options.get_host(), "localhost" | "127.0.0.1" | "::1"),
        "refusing retention tests against non-local host: {}",
        options.get_host()
    );
    let pool = db::connect(&url).await.expect("connect to test database");
    db::migrate(&pool).await.expect("apply migrations");
    Some(pool)
}

async fn cleanup(pool: &PgPool, job_id: &str, user_id: &str, project_id: &str) {
    sqlx::query("DELETE FROM cloud_retention_tombstone_journal WHERE job_id=$1")
        .bind(job_id)
        .execute(pool)
        .await
        .expect("clean retention tombstone journal");
    sqlx::query("DELETE FROM cloud_retention_receipts WHERE job_id=$1")
        .bind(job_id)
        .execute(pool)
        .await
        .expect("clean retention receipts");
    sqlx::query("DELETE FROM cloud_retention_jobs WHERE job_id=$1")
        .bind(job_id)
        .execute(pool)
        .await
        .expect("clean retention job");
    sqlx::query("DELETE FROM projects WHERE id=$1")
        .bind(project_id)
        .execute(pool)
        .await
        .expect("clean retention project");
    sqlx::query("DELETE FROM users WHERE id=$1")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("clean retention user");
}

async fn seed_user_and_project(pool: &PgPool, user_id: &str, project_id: &str) {
    sqlx::query(
        "INSERT INTO users (id, oauth_provider, oauth_subject) VALUES ($1, 'retention-test', $1)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(user_id)
    .execute(pool)
    .await
    .expect("seed retention user");
    sqlx::query("INSERT INTO projects (id, owner_id, enc_name) VALUES ($1,$2,decode('01','hex'))")
        .bind(project_id)
        .bind(user_id)
        .execute(pool)
        .await
        .expect("seed retention project");
}

async fn project_created_epoch(pool: &PgPool, project_id: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT (extract(epoch from created_at) * 1000000)::bigint FROM projects WHERE id=$1",
    )
    .bind(project_id)
    .fetch_one(pool)
    .await
    .expect("read retention project creation identity")
}

fn intent(job_id: &str, user_id: &str) -> RetentionIntent {
    RetentionIntent {
        job_id: job_id.into(),
        scope_kind: ScopeKind::Personal,
        scope_key: user_id.into(),
        subject_user_id: Some(user_id.into()),
        organisation_id: None,
        eligibility_episode: "export-expiry:1".into(),
        deadline_epoch: 1,
        notice_event_key: format!("notice:export-expiry:{user_id}"),
        notice_evidence_epoch: Some(1),
        expected_coverage_revision: None,
    }
}

#[tokio::test]
async fn dry_run_is_idempotent_and_purge_deletes_only_the_explicit_personal_scope() {
    let _guard = retention_test_lock().lock().await;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    const USER_ID: &str = "retention-test-user";
    const PROJECT_ID: &str = "retention-test-project";
    const JOB_ID: &str = "retention:test-job";
    cleanup(&pool, JOB_ID, USER_ID, PROJECT_ID).await;
    seed_user_and_project(&pool, USER_ID, PROJECT_ID).await;
    let expected_created_at_epoch = project_created_epoch(&pool, PROJECT_ID).await;

    assert_eq!(
        retention::enqueue(&pool, &intent(JOB_ID, USER_ID))
            .await
            .expect("enqueue retention job"),
        EnqueueOutcome::Enqueued
    );
    assert_eq!(
        retention::enqueue(&pool, &intent(JOB_ID, USER_ID))
            .await
            .expect("replay retention job"),
        EnqueueOutcome::AlreadyQueued
    );
    retention::add_scope_item(
        &pool,
        &ScopeItem {
            job_id: JOB_ID.into(),
            resource_kind: "project".into(),
            resource_id: PROJECT_ID.into(),
            ownership_kind: "personal".into(),
            expected_created_at_epoch,
            expected_revision: None,
        },
    )
    .await
    .expect("add personal scope item");
    retention::mark_ready(&pool, JOB_ID)
        .await
        .expect("mark retention job ready");
    assert!(matches!(
        retention::add_scope_item(
            &pool,
            &ScopeItem {
                job_id: JOB_ID.into(),
                resource_kind: "project".into(),
                resource_id: "late-scope-item".into(),
                ownership_kind: "personal".into(),
                expected_created_at_epoch,
                expected_revision: None,
            },
        )
        .await,
        Err(retention::RetentionError::Held(
            "retention scope is immutable"
        ))
    ));

    let dry_run = retention::run_once(&pool, "retention-test-dry-run", RetentionMode::DryRun)
        .await
        .expect("run retention dry run")
        .expect("dry run should claim a job");
    assert_eq!(dry_run.state, RetentionState::DryRun);
    assert_eq!(dry_run.processed_items, 1);
    let repeated_dry_run =
        retention::run_once(&pool, "retention-test-dry-run-again", RetentionMode::DryRun)
            .await
            .expect("repeat retention dry run")
            .expect("repeat dry run should claim the same job");
    assert_eq!(repeated_dry_run.state, RetentionState::DryRun);
    assert_eq!(repeated_dry_run.processed_items, 1);
    let project_exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM projects WHERE id=$1)")
            .bind(PROJECT_ID)
            .fetch_one(&pool)
            .await
            .expect("check dry-run project");
    assert!(project_exists);

    retention::enable_purge(&pool, JOB_ID)
        .await
        .expect("enable explicit purge");
    assert!(
        retention::run_once(&pool, "retention-test-wrong-mode", RetentionMode::DryRun)
            .await
            .expect("wrong-mode dry run must be ignored")
            .is_none()
    );
    let purge = retention::run_once(&pool, "retention-test-purger", RetentionMode::Purge)
        .await
        .expect("run bounded purge")
        .expect("purge should claim a job");
    assert_eq!(purge.state, RetentionState::Completed);
    let row = sqlx::query(
        "SELECT state, action FROM cloud_retention_scope_items i \
         JOIN cloud_retention_receipts r USING (job_id, resource_kind, resource_id) \
         WHERE i.job_id=$1 AND i.resource_id=$2",
    )
    .bind(JOB_ID)
    .bind(PROJECT_ID)
    .fetch_one(&pool)
    .await
    .expect("load purge receipt");
    assert_eq!(row.get::<String, _>("state"), "deleted");
    assert_eq!(row.get::<String, _>("action"), "deleted");
    let journal = sqlx::query(
        "SELECT resource_kind, resource_id, ownership_kind, expected_owner_id, expected_created_at \
         FROM cloud_retention_tombstone_journal WHERE job_id=$1 AND resource_id=$2",
    )
    .bind(JOB_ID)
    .bind(PROJECT_ID)
    .fetch_one(&pool)
    .await
    .expect("load retention tombstone journal");
    assert_eq!(journal.get::<String, _>("resource_kind"), "project");
    assert_eq!(journal.get::<String, _>("resource_id"), PROJECT_ID);
    assert_eq!(journal.get::<String, _>("ownership_kind"), "personal");
    assert_eq!(journal.get::<String, _>("expected_owner_id"), USER_ID);
    assert!(journal.get::<i64, _>("expected_created_at") > 0);
    cleanup(&pool, JOB_ID, USER_ID, PROJECT_ID).await;
}

#[tokio::test]
async fn shared_scope_holds_and_cancellation_stops_due_work() {
    let _guard = retention_test_lock().lock().await;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    const USER_ID: &str = "retention-test-shared-user";
    const PROJECT_ID: &str = "retention-test-shared-project";
    const SHARED_JOB: &str = "retention:shared-job";
    const CANCEL_JOB: &str = "retention:cancelled-job";
    cleanup(&pool, SHARED_JOB, USER_ID, PROJECT_ID).await;
    cleanup(&pool, CANCEL_JOB, USER_ID, PROJECT_ID).await;
    seed_user_and_project(&pool, USER_ID, PROJECT_ID).await;
    let expected_created_at_epoch = project_created_epoch(&pool, PROJECT_ID).await;

    let mut shared = intent(SHARED_JOB, USER_ID);
    shared.notice_event_key = "notice:shared-expiry".into();
    retention::enqueue(&pool, &shared)
        .await
        .expect("enqueue shared retention job");
    retention::add_scope_item(
        &pool,
        &ScopeItem {
            job_id: SHARED_JOB.into(),
            resource_kind: "project".into(),
            resource_id: PROJECT_ID.into(),
            ownership_kind: "shared".into(),
            expected_created_at_epoch,
            expected_revision: None,
        },
    )
    .await
    .expect("add shared scope item");
    retention::mark_ready(&pool, SHARED_JOB)
        .await
        .expect("mark shared job ready");
    retention::run_once(
        &pool,
        "retention-test-shared-dry-run",
        RetentionMode::DryRun,
    )
    .await
    .expect("run shared dry run")
    .expect("shared dry run should claim a job");
    retention::enable_purge(&pool, SHARED_JOB)
        .await
        .expect("enable shared purge attempt");
    let held = retention::run_once(&pool, "retention-test-shared", RetentionMode::Purge)
        .await
        .expect("run shared purge")
        .expect("shared job should be claimed");
    assert_eq!(held.state, RetentionState::Held);
    let project_exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM projects WHERE id=$1)")
            .bind(PROJECT_ID)
            .fetch_one(&pool)
            .await
            .expect("check shared project");
    assert!(project_exists);

    let mut cancelled = intent(CANCEL_JOB, USER_ID);
    cancelled.eligibility_episode = "export-expiry:2".into();
    cancelled.notice_event_key = "notice:cancelled-expiry".into();
    retention::enqueue(&pool, &cancelled)
        .await
        .expect("enqueue cancellation job");
    retention::mark_ready(&pool, CANCEL_JOB)
        .await
        .expect("mark cancellation job ready");
    sqlx::query(
        "UPDATE cloud_retention_jobs SET state='leased', lease_owner='retention-test-cancel', \
         lease_expires_at=now()+interval '5 minutes' WHERE job_id=$1",
    )
    .bind(CANCEL_JOB)
    .execute(&pool)
    .await
    .expect("lease cancellation job");
    assert!(retention::cancel(&pool, CANCEL_JOB, "repaid")
        .await
        .unwrap());
    assert!(
        retention::run_once(&pool, "retention-test-cancel", RetentionMode::Purge)
            .await
            .expect("cancelled job must not run")
            .is_none()
    );
    cleanup(&pool, SHARED_JOB, USER_ID, PROJECT_ID).await;
    cleanup(&pool, CANCEL_JOB, USER_ID, PROJECT_ID).await;
}

#[tokio::test]
async fn recreated_project_is_held_instead_of_being_purged() {
    let _guard = retention_test_lock().lock().await;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    const USER_ID: &str = "retention-test-recreated-user";
    const PROJECT_ID: &str = "retention-test-recreated-project";
    const JOB_ID: &str = "retention:recreated-job";
    cleanup(&pool, JOB_ID, USER_ID, PROJECT_ID).await;
    seed_user_and_project(&pool, USER_ID, PROJECT_ID).await;
    let expected_created_at_epoch = project_created_epoch(&pool, PROJECT_ID).await;

    retention::enqueue(&pool, &intent(JOB_ID, USER_ID))
        .await
        .expect("enqueue recreated retention job");
    retention::add_scope_item(
        &pool,
        &ScopeItem {
            job_id: JOB_ID.into(),
            resource_kind: "project".into(),
            resource_id: PROJECT_ID.into(),
            ownership_kind: "personal".into(),
            expected_created_at_epoch,
            expected_revision: None,
        },
    )
    .await
    .expect("add recreated project scope item");
    retention::mark_ready(&pool, JOB_ID)
        .await
        .expect("mark recreated job ready");
    retention::run_once(
        &pool,
        "retention-test-recreated-dry-run",
        RetentionMode::DryRun,
    )
    .await
    .expect("run recreated dry run")
    .expect("recreated dry run should claim a job");
    retention::enable_purge(&pool, JOB_ID)
        .await
        .expect("enable recreated project purge");

    sqlx::query("DELETE FROM projects WHERE id=$1")
        .bind(PROJECT_ID)
        .execute(&pool)
        .await
        .expect("remove original project");
    sqlx::query("INSERT INTO projects (id, owner_id, enc_name) VALUES ($1,$2,decode('02','hex'))")
        .bind(PROJECT_ID)
        .bind(USER_ID)
        .execute(&pool)
        .await
        .expect("recreate project with the same id");

    let result = retention::run_once(&pool, "retention-test-recreated", RetentionMode::Purge)
        .await
        .expect("run recreated project purge")
        .expect("recreated job should be claimed");
    assert_eq!(result.state, RetentionState::Held);
    let row = sqlx::query(
        "SELECT state, hold_code FROM cloud_retention_scope_items WHERE job_id=$1 AND resource_id=$2",
    )
    .bind(JOB_ID)
    .bind(PROJECT_ID)
    .fetch_one(&pool)
    .await
    .expect("load recreated project hold");
    assert_eq!(row.get::<String, _>("state"), "held");
    assert_eq!(
        row.get::<String, _>("hold_code"),
        "resource_identity_changed"
    );
    let project_exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM projects WHERE id=$1)")
            .bind(PROJECT_ID)
            .fetch_one(&pool)
            .await
            .expect("check recreated project");
    assert!(project_exists);
    cleanup(&pool, JOB_ID, USER_ID, PROJECT_ID).await;
}
