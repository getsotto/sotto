//! Database-backed checks for explicit retention scope and dry-run safety.

use sqlx::postgres::PgConnectOptions;
use sqlx::{PgPool, Row};
use std::str::FromStr;

use sotto_server::db;
use sotto_server::retention::{
    self, EnqueueOutcome, RetentionIntent, RetentionMode, RetentionState, ScopeItem, ScopeKind,
};

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
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    const USER_ID: &str = "retention-test-user";
    const PROJECT_ID: &str = "retention-test-project";
    const JOB_ID: &str = "retention:test-job";
    cleanup(&pool, JOB_ID, USER_ID, PROJECT_ID).await;
    seed_user_and_project(&pool, USER_ID, PROJECT_ID).await;

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
            expected_revision: None,
        },
    )
    .await
    .expect("add personal scope item");
    retention::mark_ready(&pool, JOB_ID)
        .await
        .expect("mark retention job ready");

    let dry_run = retention::run_once(&pool, "retention-test-dry-run", RetentionMode::DryRun)
        .await
        .expect("run retention dry run")
        .expect("dry run should claim a job");
    assert_eq!(dry_run.state, RetentionState::DryRun);
    assert_eq!(dry_run.processed_items, 1);
    let project_exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM projects WHERE id=$1)")
            .bind(PROJECT_ID)
            .fetch_one(&pool)
            .await
            .expect("check dry-run project");
    assert!(project_exists);

    retention::mark_ready(&pool, JOB_ID)
        .await
        .expect("re-arm retention job");
    retention::enable_purge(&pool, JOB_ID)
        .await
        .expect("enable explicit purge");
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
    cleanup(&pool, JOB_ID, USER_ID, PROJECT_ID).await;
}
