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
        expected_scope_generation: None,
        eligibility_loss_epoch: None,
    }
}

#[tokio::test]
async fn dry_run_is_idempotent_and_generic_worker_cannot_purge() {
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
    assert_eq!(dry_run.state, RetentionState::Held);
    assert_eq!(dry_run.processed_items, 0);
    assert!(
        retention::run_once(&pool, "retention-test-dry-run-again", RetentionMode::DryRun)
            .await
            .expect("repeat retention dry run")
            .is_none(),
        "a held dry run is not claimed again until an operator retries it"
    );
    let project_exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM projects WHERE id=$1)")
            .bind(PROJECT_ID)
            .fetch_one(&pool)
            .await
            .expect("check dry-run project");
    assert!(project_exists);

    assert!(
        retention::run_once(&pool, "retention-test-wrong-mode", RetentionMode::DryRun)
            .await
            .expect("wrong-mode dry run must be ignored")
            .is_none()
    );
    assert!(matches!(
        retention::run_once(&pool, "retention-test-purger", RetentionMode::Purge).await,
        Err(retention::RetentionError::Held(
            "purge execution requires the configured approved-retention worker"
        ))
    ));
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM projects WHERE id=$1)")
            .bind(PROJECT_ID)
            .fetch_one(&pool)
            .await
            .expect("check generic worker did not delete project")
    );
    let receipt_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM cloud_retention_receipts WHERE job_id=$1")
            .bind(JOB_ID)
            .fetch_one(&pool)
            .await
            .expect("count purge receipts");
    assert_eq!(receipt_count, 0);
    cleanup(&pool, JOB_ID, USER_ID, PROJECT_ID).await;
}

#[tokio::test]
async fn changed_revision_reopens_an_unfinished_personal_episode_for_recheck() {
    let _guard = retention_test_lock().lock().await;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let suffix = uuid::Uuid::new_v4();
    let user_id = format!("retention-revision-user-{suffix}");
    let job_id = format!("retention:revision-{suffix}");
    let project_id = format!("revision-project-{suffix}");
    cleanup(&pool, &job_id, &user_id, &project_id).await;
    seed_user_and_project(&pool, &user_id, &project_id).await;

    let mut original = intent(&job_id, &user_id);
    original.expected_coverage_revision = Some(17);
    assert_eq!(
        retention::enqueue(&pool, &original)
            .await
            .expect("enqueue initial revision"),
        EnqueueOutcome::Enqueued
    );
    sqlx::query(
        r#"UPDATE cloud_retention_jobs SET state='held', hold_code='coverage_revision_changed',
             dry_run_completed_at=now() WHERE job_id=$1"#,
    )
    .bind(&job_id)
    .execute(&pool)
    .await
    .expect("simulate hold on the old coverage revision");
    sqlx::query(
        r#"INSERT INTO cloud_retention_scope_items
           (job_id,resource_kind,resource_id,ownership_kind,expected_created_at,expected_revision,state)
           VALUES ($1,'project',$2,'personal',1,NULL,'dry_run')"#,
    )
    .bind(&job_id)
    .bind(&project_id)
    .execute(&pool)
    .await
    .expect("seed an item that must be rechecked");

    let mut refreshed = original;
    refreshed.job_id = format!("{job_id}:new-attempt");
    refreshed.expected_coverage_revision = Some(18);
    assert_eq!(
        retention::enqueue(&pool, &refreshed)
            .await
            .expect("same episode with refreshed coverage is rechecked"),
        EnqueueOutcome::AlreadyQueued
    );
    let job = sqlx::query(
        "SELECT expected_coverage_revision,state,hold_code,dry_run_completed_at IS NULL AS dry_run_reset
         FROM cloud_retention_jobs WHERE job_id=$1",
    )
    .bind(&job_id)
    .fetch_one(&pool)
    .await
    .expect("read refreshed retention job");
    assert_eq!(
        job.get::<Option<i64>, _>("expected_coverage_revision"),
        Some(18)
    );
    assert_eq!(job.get::<String, _>("state"), "ready");
    assert_eq!(job.get::<Option<String>, _>("hold_code"), None);
    assert!(job.get::<bool, _>("dry_run_reset"));
    let item_state: String = sqlx::query_scalar(
        "SELECT state FROM cloud_retention_scope_items WHERE job_id=$1 AND resource_id=$2",
    )
    .bind(&job_id)
    .bind(&project_id)
    .fetch_one(&pool)
    .await
    .expect("read recheck item state");
    assert_eq!(item_state, "planned");
    cleanup(&pool, &job_id, &user_id, &project_id).await;
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
    let shared_dry_run = retention::run_once(
        &pool,
        "retention-test-shared-dry-run",
        RetentionMode::DryRun,
    )
    .await
    .expect("run shared dry run")
    .expect("shared dry run should claim a job");
    assert_eq!(shared_dry_run.state, RetentionState::Held);
    assert!(matches!(
        retention::run_once(&pool, "retention-test-shared-purger", RetentionMode::Purge).await,
        Err(retention::RetentionError::Held(
            "purge execution requires the configured approved-retention worker"
        ))
    ));
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
        retention::run_once(&pool, "retention-test-cancel", RetentionMode::DryRun)
            .await
            .expect("cancelled job must not run")
            .is_none()
    );
    cleanup(&pool, SHARED_JOB, USER_ID, PROJECT_ID).await;
    cleanup(&pool, CANCEL_JOB, USER_ID, PROJECT_ID).await;
}

#[tokio::test]
async fn generic_worker_cannot_purge_even_a_recreated_project() {
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

    assert!(matches!(
        retention::run_once(&pool, "retention-test-recreated", RetentionMode::Purge).await,
        Err(retention::RetentionError::Held(
            "purge execution requires the configured approved-retention worker"
        ))
    ));
    let project_exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM projects WHERE id=$1)")
            .bind(PROJECT_ID)
            .fetch_one(&pool)
            .await
            .expect("check recreated project");
    assert!(project_exists);
    cleanup(&pool, JOB_ID, USER_ID, PROJECT_ID).await;
}
