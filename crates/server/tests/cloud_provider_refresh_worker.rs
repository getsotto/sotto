//! Database-gated orchestration coverage for the durable provider refresh worker.

use std::{
    str::FromStr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use async_trait::async_trait;
use sqlx::{postgres::PgConnectOptions, PgPool};
use uuid::Uuid;

use sotto_server::cloud_provider_refresh_inputs::RefreshJobInputs;
use sotto_server::cloud_provider_refresh_worker::{
    run_once, RefreshExecutionError, RefreshJobExecutor, RefreshWorkerError, RefreshWorkerOutcome,
};
use sotto_server::db;

async fn pool_or_skip() -> Option<PgPool> {
    if std::env::var("SOTTO_RUN_DB_TESTS").as_deref() != Ok("1") {
        eprintln!("skipping refresh worker test: set SOTTO_RUN_DB_TESTS=1");
        return None;
    }
    let database_url =
        std::env::var("DATABASE_URL").expect("DATABASE_URL is required when SOTTO_RUN_DB_TESTS=1");
    let options = PgConnectOptions::from_str(&database_url).expect("parse DATABASE_URL");
    assert!(
        matches!(options.get_host(), "localhost" | "127.0.0.1" | "::1"),
        "refusing refresh worker tests against non-local host: {}",
        options.get_host()
    );
    let pool = db::connect(&database_url).await.expect("connect");
    db::migrate(&pool).await.expect("migrate");
    Some(pool)
}

struct ScriptedExecutor {
    outcome: ExecutorOutcome,
    calls: Arc<AtomicUsize>,
    pool: Option<PgPool>,
}

struct Fixture {
    beneficiary_id: String,
    payer_id: String,
    allocation_id: String,
    source_id: String,
    subscription_id: String,
    external_reference: String,
}

enum ExecutorOutcome {
    Complete,
    Fail,
    ExpireThenComplete,
}

#[async_trait]
impl RefreshJobExecutor for ScriptedExecutor {
    async fn execute(&mut self, inputs: &RefreshJobInputs) -> Result<(), RefreshExecutionError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if matches!(self.outcome, ExecutorOutcome::ExpireThenComplete) {
            sqlx::query(
                "UPDATE cloud_provider_refresh_jobs SET lease_expires_at = now() - interval '1 second' \
                 WHERE job_id = $1",
            )
            .bind(&inputs.lease.job_id)
            .execute(self.pool.as_ref().expect("expiry executor pool"))
            .await
            .expect("expire worker lease");
        }
        if matches!(self.outcome, ExecutorOutcome::Fail) {
            return Err(RefreshExecutionError::new("provider_timeout").unwrap());
        }
        Ok(())
    }
}

async fn fixture(pool: &PgPool) -> Fixture {
    let suffix = Uuid::new_v4().to_string();
    let fixture = Fixture {
        beneficiary_id: format!("refresh-worker-beneficiary-{suffix}"),
        payer_id: format!("refresh-worker-payer-{suffix}"),
        allocation_id: format!("refresh-worker-allocation-{suffix}"),
        source_id: format!("refresh-worker-source-{suffix}"),
        subscription_id: format!("refresh-worker-subscription-{suffix}"),
        external_reference: format!("refresh-worker-external-{suffix}"),
    };
    sqlx::query(
        "INSERT INTO users (id, oauth_provider, oauth_subject) VALUES ($1, 'refresh-worker-test', $1)",
    )
    .bind(&fixture.beneficiary_id)
    .execute(pool)
    .await
    .expect("insert worker beneficiary");
    sqlx::query(
        "INSERT INTO cloud_coverage_sources \
         (source_id, beneficiary_id, provider_namespace, external_allocation_reference, \
          ownership_evidence_reference, registration_operation_id, registration_source_set_generation) \
         VALUES ($1, $2, 'stripe', $3, $4, $5, 1)",
    )
    .bind(&fixture.source_id)
    .bind(&fixture.beneficiary_id)
    .bind(&fixture.external_reference)
    .bind("refresh-worker-ownership")
    .bind(format!("refresh-worker-registration-{}", Uuid::new_v4()))
    .execute(pool)
    .await
    .expect("insert worker source");
    sqlx::query(
        "INSERT INTO cloud_provider_payers \
         (payer_id, provider_namespace, provider_account_id, provider_environment, \
          provider_customer_id, payer_kind) \
         VALUES ($1, 'stripe', 'acct_refresh_worker', 'test', $2, 'personal')",
    )
    .bind(&fixture.payer_id)
    .bind(format!("refresh-worker-customer-{}", Uuid::new_v4()))
    .execute(pool)
    .await
    .expect("insert worker payer");
    sqlx::query(
        "INSERT INTO cloud_provider_allocations \
         (allocation_id, payer_id, beneficiary_id, provider_namespace, provider_account_id, \
          provider_environment, provider_subscription_id, provider_item_id, \
          external_allocation_reference, coverage_source_id, effective_from, state, \
          ownership_evidence_reference) \
         VALUES ($1, $2, $3, 'stripe', 'acct_refresh_worker', 'test', $4, $5, $6, $7, 0, \
                 'active', $8)",
    )
    .bind(&fixture.allocation_id)
    .bind(&fixture.payer_id)
    .bind(&fixture.beneficiary_id)
    .bind(&fixture.subscription_id)
    .bind("price_refresh_worker")
    .bind(&fixture.external_reference)
    .bind(&fixture.source_id)
    .bind("refresh-worker-ownership")
    .execute(pool)
    .await
    .expect("insert worker allocation");
    fixture
}

async fn insert_job(pool: &PgPool, suffix: &str, fixture: &Fixture) -> String {
    let event_id = format!("refresh-worker-event-{suffix}-{}", Uuid::new_v4());
    let job_id = format!("provider-refresh-test:{}", Uuid::new_v4());
    sqlx::query(
        "INSERT INTO cloud_provider_event_receipts \
         (provider_namespace, provider_account_id, provider_environment, event_id, event_type, \
          provider_created_at, subscription_id, allocation_reference, normalized_payload_hash, status) \
         VALUES ('stripe', 'acct_refresh_worker', 'test', $1, 'invoice.paid', 1700000000, $3, $4, $2, 'pending')",
    )
    .bind(&event_id)
    .bind("a".repeat(64))
    .bind(&fixture.subscription_id)
    .bind(&fixture.external_reference)
    .execute(pool)
    .await
    .expect("insert worker receipt");
    sqlx::query(
        "INSERT INTO cloud_provider_refresh_jobs \
         (job_id, provider_namespace, provider_account_id, provider_environment, event_id, \
          beneficiary_id, allocation_id, coverage_source_id) \
         VALUES ($1, 'stripe', 'acct_refresh_worker', 'test', $2, $3, $4, $5)",
    )
    .bind(&job_id)
    .bind(&event_id)
    .bind(&fixture.beneficiary_id)
    .bind(&fixture.allocation_id)
    .bind(&fixture.source_id)
    .execute(pool)
    .await
    .expect("insert worker job");
    event_id
}

async fn cleanup(pool: &PgPool, event_id: &str) {
    sqlx::query("DELETE FROM cloud_provider_event_receipts WHERE event_id = $1")
        .bind(event_id)
        .execute(pool)
        .await
        .expect("cleanup worker receipt");
}

async fn cleanup_fixture(pool: &PgPool, fixture: &Fixture) {
    sqlx::query("DELETE FROM cloud_provider_allocations WHERE allocation_id = $1")
        .bind(&fixture.allocation_id)
        .execute(pool)
        .await
        .expect("cleanup worker allocation");
    sqlx::query("DELETE FROM cloud_provider_payers WHERE payer_id = $1")
        .bind(&fixture.payer_id)
        .execute(pool)
        .await
        .expect("cleanup worker payer");
    sqlx::query("DELETE FROM cloud_coverage_sources WHERE source_id = $1")
        .bind(&fixture.source_id)
        .execute(pool)
        .await
        .expect("cleanup worker source");
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(&fixture.beneficiary_id)
        .execute(pool)
        .await
        .expect("cleanup worker beneficiary");
}

#[tokio::test]
async fn run_once_covers_idle_success_retry_poison_and_lease_loss() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let fixture = fixture(&pool).await;

    let idle_calls = Arc::new(AtomicUsize::new(0));
    let mut idle_executor = ScriptedExecutor {
        outcome: ExecutorOutcome::Complete,
        calls: Arc::clone(&idle_calls),
        pool: None,
    };
    assert_eq!(
        run_once(&pool, "refresh-worker-idle", &mut idle_executor)
            .await
            .unwrap(),
        RefreshWorkerOutcome::Idle
    );
    assert_eq!(idle_calls.load(Ordering::SeqCst), 0);

    let success_event = insert_job(&pool, "success", &fixture).await;
    let success_calls = Arc::new(AtomicUsize::new(0));
    let mut success_executor = ScriptedExecutor {
        outcome: ExecutorOutcome::Complete,
        calls: Arc::clone(&success_calls),
        pool: None,
    };
    assert_eq!(
        run_once(&pool, "refresh-worker-success", &mut success_executor)
            .await
            .unwrap(),
        RefreshWorkerOutcome::Completed
    );
    assert_eq!(success_calls.load(Ordering::SeqCst), 1);
    let success_status: String = sqlx::query_scalar(
        "SELECT status FROM cloud_provider_refresh_jobs \
         WHERE provider_namespace = 'stripe' AND event_id = $1",
    )
    .bind(&success_event)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(success_status, "completed");

    let retry_event = insert_job(&pool, "retry", &fixture).await;
    let retry_calls = Arc::new(AtomicUsize::new(0));
    let mut retry_executor = ScriptedExecutor {
        outcome: ExecutorOutcome::Fail,
        calls: Arc::clone(&retry_calls),
        pool: None,
    };
    assert_eq!(
        run_once(&pool, "refresh-worker-retry", &mut retry_executor)
            .await
            .unwrap(),
        RefreshWorkerOutcome::Retried
    );
    assert_eq!(retry_calls.load(Ordering::SeqCst), 1);
    let retry_status: String =
        sqlx::query_scalar("SELECT status FROM cloud_provider_refresh_jobs WHERE event_id = $1")
            .bind(&retry_event)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(retry_status, "pending");

    let poison_event = insert_job(&pool, "poison", &fixture).await;
    sqlx::query("UPDATE cloud_provider_refresh_jobs SET attempt_count = 6 WHERE event_id = $1")
        .bind(&poison_event)
        .execute(&pool)
        .await
        .unwrap();
    let mut poison_executor = ScriptedExecutor {
        outcome: ExecutorOutcome::Fail,
        calls: Arc::new(AtomicUsize::new(0)),
        pool: None,
    };
    assert_eq!(
        run_once(&pool, "refresh-worker-poison", &mut poison_executor)
            .await
            .unwrap(),
        RefreshWorkerOutcome::Poisoned
    );
    let poison_status: String =
        sqlx::query_scalar("SELECT status FROM cloud_provider_refresh_jobs WHERE event_id = $1")
            .bind(&poison_event)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(poison_status, "poisoned");

    let lease_loss_event = insert_job(&pool, "lease-loss", &fixture).await;
    let lease_loss_calls = Arc::new(AtomicUsize::new(0));
    let mut lease_loss_executor = ScriptedExecutor {
        outcome: ExecutorOutcome::ExpireThenComplete,
        calls: Arc::clone(&lease_loss_calls),
        pool: Some(pool.clone()),
    };
    assert!(matches!(
        run_once(&pool, "refresh-worker-lease-loss", &mut lease_loss_executor).await,
        Err(RefreshWorkerError::Queue(
            sotto_server::cloud_provider_refresh_jobs::RefreshJobError::LeaseLost
        ))
    ));
    assert_eq!(lease_loss_calls.load(Ordering::SeqCst), 1);

    for event_id in [success_event, retry_event, poison_event, lease_loss_event] {
        cleanup(&pool, &event_id).await;
    }
    cleanup_fixture(&pool, &fixture).await;
}
