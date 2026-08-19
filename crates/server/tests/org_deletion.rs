//! Database-backed organisation-deletion lifecycle tests.
//!
//! These tests use the same local-Postgres guard as the other destructive server tests. The
//! provider port is exercised with a deterministic in-memory adapter so the worker tests cover
//! billing outcomes without making network calls.

use async_trait::async_trait;
use sqlx::postgres::PgConnectOptions;
use sqlx::PgPool;
use std::collections::VecDeque;
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::{Mutex as AsyncMutex, Notify};
use uuid::Uuid;

use sotto_server::billing::{
    ProviderError, ProviderErrorKind, ProviderResult, SubscriptionObservation,
    SubscriptionProvider, SubscriptionSnapshot, SubscriptionStatus,
};
use sotto_server::db;
use sotto_server::error::Error;
use sotto_server::org_deletion::{
    advance, cancel, claim_due, record_operator_observation, request, status, DeletionState,
    OperatorObservation,
};

static DB_TEST_LOCK: OnceLock<Arc<AsyncMutex<()>>> = OnceLock::new();

// Keep one provider fake for ordinary outcomes and the in-flight cancellation race. The optional
// gate holds the provider call open until recovery commits, forcing the worker's stale
// compare-and-set result to race the newer recovery state.
#[derive(Clone)]
struct CancellationGate {
    started: Arc<Notify>,
    release: Arc<Notify>,
}

struct TestProvider {
    cancellation: Mutex<VecDeque<ProviderResult<SubscriptionObservation>>>,
    status: Mutex<VecDeque<ProviderResult<SubscriptionObservation>>>,
    cancellation_calls: AtomicUsize,
    status_calls: AtomicUsize,
    cancellation_gate: Option<CancellationGate>,
}

impl TestProvider {
    fn new(
        cancellation: impl IntoIterator<Item = ProviderResult<SubscriptionObservation>>,
        status: impl IntoIterator<Item = ProviderResult<SubscriptionObservation>>,
    ) -> Self {
        Self {
            cancellation: Mutex::new(cancellation.into_iter().collect()),
            status: Mutex::new(status.into_iter().collect()),
            cancellation_calls: AtomicUsize::new(0),
            status_calls: AtomicUsize::new(0),
            cancellation_gate: None,
        }
    }

    fn with_blocking_cancellation(mut self) -> (Self, Arc<Notify>, Arc<Notify>) {
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        self.cancellation_gate = Some(CancellationGate {
            started: started.clone(),
            release: release.clone(),
        });
        (self, started, release)
    }

    fn unsupported(operation: &str) -> ProviderError {
        ProviderError {
            status: None,
            code: Some(format!("test_unsupported_{operation}")),
            kind: ProviderErrorKind::Unknown,
        }
    }

    fn cancellation_calls(&self) -> usize {
        self.cancellation_calls.load(Ordering::Relaxed)
    }

    fn status_calls(&self) -> usize {
        self.status_calls.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl SubscriptionProvider for TestProvider {
    async fn create_checkout(
        &self,
        _org_id: &str,
        _customer: Option<&str>,
        _success_url: &str,
        _cancel_url: &str,
    ) -> ProviderResult<String> {
        Err(Self::unsupported("checkout"))
    }

    async fn create_portal(&self, _customer: &str, _return_url: &str) -> ProviderResult<String> {
        Err(Self::unsupported("portal"))
    }

    async fn get_subscription(
        &self,
        _subscription_id: &str,
    ) -> ProviderResult<SubscriptionObservation> {
        self.status_calls.fetch_add(1, Ordering::Relaxed);
        self.status
            .lock()
            .expect("status queue lock")
            .pop_front()
            .unwrap_or_else(|| Err(Self::unsupported("status")))
    }

    async fn cancel_subscription(
        &self,
        _subscription_id: &str,
        _idempotency_key: &str,
        _org_id: &str,
    ) -> ProviderResult<SubscriptionObservation> {
        self.cancellation_calls.fetch_add(1, Ordering::Relaxed);
        if let Some(gate) = &self.cancellation_gate {
            gate.started.notify_one();
            gate.release.notified().await;
        }
        self.cancellation
            .lock()
            .expect("cancellation queue lock")
            .pop_front()
            .unwrap_or_else(|| Err(Self::unsupported("cancellation")))
    }
}

fn cancelled(subscription_id: &str) -> SubscriptionObservation {
    SubscriptionObservation::Current(SubscriptionSnapshot {
        id: subscription_id.into(),
        status: SubscriptionStatus::Canceled,
    })
}

fn current(subscription_id: &str, status: SubscriptionStatus) -> SubscriptionObservation {
    SubscriptionObservation::Current(SubscriptionSnapshot {
        id: subscription_id.into(),
        status,
    })
}

async fn pool_or_skip() -> Option<PgPool> {
    let Ok(database_url) = std::env::var("DATABASE_URL") else {
        return None;
    };
    if std::env::var("SOTTO_RUN_DB_TESTS").as_deref() != Ok("1") {
        return None;
    }
    let options = PgConnectOptions::from_str(&database_url).expect("parse DATABASE_URL");
    assert!(
        matches!(options.get_host(), "localhost" | "127.0.0.1" | "::1"),
        "refusing destructive deletion tests against non-local host: {}",
        options.get_host()
    );
    let pool = db::connect(&database_url).await.expect("connect");
    db::migrate(&pool).await.expect("migrate");
    Some(pool)
}

async fn db_test_lock() -> tokio::sync::OwnedMutexGuard<()> {
    DB_TEST_LOCK
        .get_or_init(|| Arc::new(AsyncMutex::new(())))
        .clone()
        .lock_owned()
        .await
}

async fn seed_owner(pool: &PgPool) -> (String, String) {
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = format!("deletion-owner-{suffix}");
    let org_id = format!("deletion-org-{suffix}");
    sqlx::query("INSERT INTO users (id, oauth_provider, oauth_subject) VALUES ($1, 'test', $1)")
        .bind(&user_id)
        .execute(pool)
        .await
        .expect("insert owner");
    sqlx::query("INSERT INTO organizations (id, enc_name, created_by) VALUES ($1, $2, $3)")
        .bind(&org_id)
        .bind(b"opaque".as_slice())
        .bind(&user_id)
        .execute(pool)
        .await
        .expect("insert organisation");
    sqlx::query(
        "INSERT INTO organization_memberships (org_id, user_id, role) VALUES ($1, $2, 'owner')",
    )
    .bind(&org_id)
    .bind(&user_id)
    .execute(pool)
    .await
    .expect("insert owner membership");
    (org_id, user_id)
}

async fn link_subscription(pool: &PgPool, org_id: &str) -> String {
    let subscription_id = format!("sub-deletion-{}", Uuid::new_v4().simple());
    sqlx::query("UPDATE organizations SET stripe_subscription_id = $2 WHERE id = $1")
        .bind(org_id)
        .bind(&subscription_id)
        .execute(pool)
        .await
        .expect("link subscription fixture");
    subscription_id
}

async fn cleanup(pool: &PgPool, org_id: &str, user_id: &str) {
    sqlx::query("DELETE FROM organization_deletions WHERE org_id = $1")
        .bind(org_id)
        .execute(pool)
        .await
        .expect("delete operation history");
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org_id)
        .execute(pool)
        .await
        .expect("delete organisation fixture");
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("delete owner fixture");
}

async fn assert_audit_actions(pool: &PgPool, org_id: &str, expected: &[&str]) {
    let actions: Vec<String> =
        sqlx::query_scalar("SELECT action FROM audit_events WHERE org_id = $1 ORDER BY id")
            .bind(org_id)
            .fetch_all(pool)
            .await
            .expect("read deletion audit events");
    for action in expected {
        assert!(
            actions.iter().any(|observed| observed == action),
            "missing audit action {action}: {actions:?}"
        );
    }
}

#[tokio::test]
async fn owner_requests_are_idempotent_and_cancellable() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let _test_lock = db_test_lock().await;
    let (org_id, owner_id) = seed_owner(&pool).await;
    assert!(matches!(
        request(&pool, &org_id, &owner_id, "other-org").await,
        Err(Error::BadRequest(_))
    ));
    let first = request(&pool, &org_id, &owner_id, &org_id)
        .await
        .expect("request deletion");
    let first_times: (String, String) = sqlx::query_as(
        "SELECT requested_at::text, purge_after::text FROM organization_deletions WHERE id = $1::uuid",
    )
    .bind(&first.id)
    .fetch_one(&pool)
    .await
    .expect("read initial retention window");
    let repeated = request(&pool, &org_id, &owner_id, &org_id)
        .await
        .expect("repeat deletion");
    assert_eq!(first, repeated);
    assert_eq!(status(&pool, &org_id, &owner_id).await.unwrap(), first);
    let repeated_times: (String, String) = sqlx::query_as(
        "SELECT requested_at::text, purge_after::text FROM organization_deletions WHERE id = $1::uuid",
    )
    .bind(&first.id)
    .fetch_one(&pool)
    .await
    .expect("read repeated retention window");
    assert_eq!(repeated_times, first_times);

    sqlx::query(
        "UPDATE organization_deletions \
         SET state = 'failed', resume_state = 'cancelling_billing', attempt_count = 7, \
             last_error_code = 'billing_unavailable', next_attempt_at = now() \
         WHERE id = $1::uuid",
    )
    .bind(&first.id)
    .execute(&pool)
    .await
    .expect("exhaust retry attempts");
    // Failed operations resume through an owner action, not the worker queue.
    assert!(claim_due(&pool, "deletion-test-worker")
        .await
        .expect("check failed operation queue")
        .is_none());
    let retried = request(&pool, &org_id, &owner_id, &org_id)
        .await
        .expect("retry failed deletion");
    assert_eq!(retried.state, DeletionState::CancellingBilling);
    let attempts: i32 =
        sqlx::query_scalar("SELECT attempt_count FROM organization_deletions WHERE id = $1::uuid")
            .bind(&first.id)
            .fetch_one(&pool)
            .await
            .expect("read reset attempts");
    assert_eq!(attempts, 0);

    let cancelled_view = cancel(&pool, &org_id, &owner_id)
        .await
        .expect("request recovery");
    assert_eq!(cancelled_view.state, DeletionState::Recovering);
    let lease = claim_due(&pool, "deletion-test-worker")
        .await
        .expect("claim recovery")
        .expect("recovery is due");
    assert_eq!(lease.state, DeletionState::Recovering);
    let recovered = advance(&pool, &lease, None)
        .await
        .expect("reconcile recovery")
        .expect("recovery transition");
    assert_eq!(recovered.state, DeletionState::Cancelled);
    let lifecycle: String =
        sqlx::query_scalar("SELECT lifecycle_state FROM organizations WHERE id = $1")
            .bind(&org_id)
            .fetch_one(&pool)
            .await
            .expect("read lifecycle");
    assert_eq!(lifecycle, "active");
    let tier: String = sqlx::query_scalar("SELECT tier FROM organizations WHERE id = $1")
        .bind(&org_id)
        .fetch_one(&pool)
        .await
        .expect("read recovered tier");
    assert_eq!(tier, "free");
    assert_audit_actions(
        &pool,
        &org_id,
        &[
            "org.deletion.requested",
            "org.deletion.cancel_requested",
            "org.deletion.recovery_completed",
        ],
    )
    .await;
    cleanup(&pool, &org_id, &owner_id).await;
}

#[tokio::test]
async fn worker_reconciles_free_deletion_and_purges_tombstone() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let _test_lock = db_test_lock().await;
    let (org_id, owner_id) = seed_owner(&pool).await;
    let requested = request(&pool, &org_id, &owner_id, &org_id)
        .await
        .expect("request deletion");
    let requested_lease = claim_due(&pool, "deletion-purge-worker")
        .await
        .expect("claim requested")
        .expect("requested work is due");
    assert_eq!(requested_lease.state, DeletionState::Requested);
    assert_eq!(requested_lease.attempt_count, 0);
    let billing = advance(&pool, &requested_lease, None)
        .await
        .expect("start billing cancellation")
        .expect("billing transition");
    assert_eq!(billing.state, DeletionState::CancellingBilling);

    let cancelling_lease = claim_due(&pool, "deletion-purge-worker")
        .await
        .expect("claim billing")
        .expect("billing work is due");
    assert_eq!(cancelling_lease.attempt_count, 1);
    let retention = advance(&pool, &cancelling_lease, None)
        .await
        .expect("reconcile missing subscription")
        .expect("retention transition");
    assert_eq!(retention.state, DeletionState::Retention);

    sqlx::query(
        "UPDATE organization_deletions SET requested_at = now() - interval '31 days', \
         purge_after = now() - interval '1 day', billing_checked_at = now() - interval '30 days' \
         WHERE id = $1::uuid",
    )
    .bind(&requested.id)
    .execute(&pool)
    .await
    .expect("age retention operation");
    let retention_lease = claim_due(&pool, "deletion-purge-worker")
        .await
        .expect("claim retention")
        .expect("retention is due");
    let purging = advance(&pool, &retention_lease, None)
        .await
        .expect("start purge")
        .expect("purge transition");
    assert_eq!(purging.state, DeletionState::Purging);
    assert!(matches!(
        cancel(&pool, &org_id, &owner_id).await,
        Err(Error::Conflict(_))
    ));

    let purging_lease = claim_due(&pool, "deletion-purge-worker")
        .await
        .expect("claim purge")
        .expect("purge is due");
    let completed = advance(&pool, &purging_lease, None)
        .await
        .expect("purge organisation")
        .expect("completed transition");
    assert_eq!(completed.state, DeletionState::Completed);
    let tombstone: (String, bool, bool) = sqlx::query_as(
        "SELECT lifecycle_state, enc_name IS NULL, stripe_subscription_id IS NULL \
         FROM organizations WHERE id = $1",
    )
    .bind(&org_id)
    .fetch_one(&pool)
    .await
    .expect("read tombstone");
    assert_eq!(tombstone, ("deleted".into(), true, true));
    assert_eq!(
        status(&pool, &org_id, &owner_id).await.unwrap().state,
        DeletionState::Completed
    );
    assert_audit_actions(
        &pool,
        &org_id,
        &[
            "org.deletion.requested",
            "org.deletion.purge_started",
            "org.deletion.completed",
        ],
    )
    .await;
    cleanup(&pool, &org_id, &owner_id).await;
}

#[tokio::test]
async fn worker_cancels_a_paid_subscription_before_purge() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let _test_lock = db_test_lock().await;
    let (org_id, owner_id) = seed_owner(&pool).await;
    let subscription_id = link_subscription(&pool, &org_id).await;
    let provider = TestProvider::new(
        [
            Ok(cancelled(&subscription_id)),
            Ok(cancelled(&subscription_id)),
        ],
        [
            Ok(current(&subscription_id, SubscriptionStatus::Active)),
            Ok(cancelled(&subscription_id)),
        ],
    );
    let requested = request(&pool, &org_id, &owner_id, &org_id)
        .await
        .expect("request deletion");
    let requested_lease = claim_due(&pool, "paid-deletion-worker")
        .await
        .expect("claim requested")
        .expect("requested work is due");
    advance(&pool, &requested_lease, None)
        .await
        .expect("start billing cancellation")
        .expect("billing transition");
    let billing_lease = claim_due(&pool, "paid-deletion-worker")
        .await
        .expect("claim billing")
        .expect("billing work is due");
    let retention = advance(&pool, &billing_lease, Some(&provider))
        .await
        .expect("cancel subscription")
        .expect("retention transition");
    assert_eq!(retention.state, DeletionState::Retention);
    assert_eq!(provider.cancellation_calls(), 1);
    let attempts: i32 =
        sqlx::query_scalar("SELECT attempt_count FROM organization_deletions WHERE id = $1::uuid")
            .bind(&requested.id)
            .fetch_one(&pool)
            .await
            .expect("read reset attempts");
    assert_eq!(attempts, 0);

    // Move the retention deadline into the past so the worker path can run without waiting thirty
    // days in a database-backed test.
    sqlx::query("UPDATE organization_deletions SET purge_after = now() WHERE id = $1::uuid")
        .bind(&requested.id)
        .execute(&pool)
        .await
        .expect("make retention due");
    let retention_lease = claim_due(&pool, "paid-deletion-worker")
        .await
        .expect("claim retention")
        .expect("retention is due");
    let cancelling = advance(&pool, &retention_lease, Some(&provider))
        .await
        .expect("reconcile blocking subscription")
        .expect("cancellation transition");
    assert_eq!(cancelling.state, DeletionState::CancellingBilling);
    let cancellation_lease = claim_due(&pool, "paid-deletion-worker")
        .await
        .expect("claim retention cancellation")
        .expect("retention cancellation is due");
    let retention = advance(&pool, &cancellation_lease, Some(&provider))
        .await
        .expect("cancel blocking subscription")
        .expect("retention transition");
    assert_eq!(retention.state, DeletionState::Retention);
    assert_eq!(provider.cancellation_calls(), 2);

    // Reconcile the cancellation before entering the purge state.
    sqlx::query("UPDATE organization_deletions SET purge_after = now() WHERE id = $1::uuid")
        .bind(&requested.id)
        .execute(&pool)
        .await
        .expect("make reconciled retention due");
    let reconciled_retention_lease = claim_due(&pool, "paid-deletion-worker")
        .await
        .expect("claim reconciled retention")
        .expect("reconciled retention is due");
    let purging = advance(&pool, &reconciled_retention_lease, Some(&provider))
        .await
        .expect("reconcile terminal subscription")
        .expect("purge transition");
    assert_eq!(purging.state, DeletionState::Purging);
    assert_eq!(provider.status_calls(), 2);

    let purging_lease = claim_due(&pool, "paid-deletion-worker")
        .await
        .expect("claim purge")
        .expect("purge is due");
    // Keep the stale observation after the request so the migration's timestamp ordering remains
    // valid while the purge freshness check still rejects it. Sixteen minutes is deliberately just
    // beyond the documented fifteen-minute freshness bound.
    sqlx::query(
        "UPDATE organization_deletions SET requested_at = now() - interval '1 hour', \
         billing_checked_at = now() - interval '16 minutes' WHERE id = $1::uuid",
    )
    .bind(&requested.id)
    .execute(&pool)
    .await
    .expect("age billing observation");
    let failed = advance(&pool, &purging_lease, None)
        .await
        .expect("reject stale billing observation")
        .expect("failed purge transition");
    assert_eq!(failed.state, DeletionState::Failed);
    assert_audit_actions(
        &pool,
        &org_id,
        &[
            "org.deletion.billing_cancelled",
            "org.deletion.purge_started",
            "org.deletion.failed",
        ],
    )
    .await;
    cleanup(&pool, &org_id, &owner_id).await;
}

#[tokio::test]
async fn provider_missing_satisfies_the_retention_purge_gate() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let _test_lock = db_test_lock().await;
    let (org_id, owner_id) = seed_owner(&pool).await;
    let subscription_id = link_subscription(&pool, &org_id).await;
    let provider = TestProvider::new(
        [Ok(cancelled(&subscription_id))],
        [Ok(SubscriptionObservation::Missing)],
    );
    let requested = request(&pool, &org_id, &owner_id, &org_id)
        .await
        .expect("request deletion");
    let requested_lease = claim_due(&pool, "missing-subscription-worker")
        .await
        .expect("claim requested")
        .expect("requested work is due");
    advance(&pool, &requested_lease, None)
        .await
        .expect("start billing cancellation")
        .expect("billing transition");
    let billing_lease = claim_due(&pool, "missing-subscription-worker")
        .await
        .expect("claim billing")
        .expect("billing work is due");
    let retention = advance(&pool, &billing_lease, Some(&provider))
        .await
        .expect("cancel subscription")
        .expect("retention transition");
    assert_eq!(retention.state, DeletionState::Retention);
    assert_eq!(provider.cancellation_calls(), 1);

    // Move the retention deadline into the past so the worker path can run without waiting thirty
    // days in a database-backed test.
    sqlx::query("UPDATE organization_deletions SET purge_after = now() WHERE id = $1::uuid")
        .bind(&requested.id)
        .execute(&pool)
        .await
        .expect("make retention due");
    let retention_lease = claim_due(&pool, "missing-subscription-worker")
        .await
        .expect("claim retention")
        .expect("retention is due");
    let purging = advance(&pool, &retention_lease, Some(&provider))
        .await
        .expect("reconcile missing subscription")
        .expect("purge transition");
    assert_eq!(purging.state, DeletionState::Purging);
    assert_eq!(provider.status_calls(), 1);
    let billing_result: (String, String) = sqlx::query_as(
        "SELECT last_billing_state, billing_observation_source \
         FROM organization_deletions WHERE id = $1::uuid",
    )
    .bind(&requested.id)
    .fetch_one(&pool)
    .await
    .expect("read missing billing result");
    assert_eq!(billing_result, ("missing".into(), "provider".into()));

    let purging_lease = claim_due(&pool, "missing-subscription-worker")
        .await
        .expect("claim purge")
        .expect("purge is due");
    let completed = advance(&pool, &purging_lease, None)
        .await
        .expect("purge missing subscription")
        .expect("completed transition");
    assert_eq!(completed.state, DeletionState::Completed);
    cleanup(&pool, &org_id, &owner_id).await;
}

#[tokio::test]
async fn unknown_provider_status_blocks_purge_and_schedules_retry() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let _test_lock = db_test_lock().await;
    let (org_id, owner_id) = seed_owner(&pool).await;
    let subscription_id = link_subscription(&pool, &org_id).await;
    let provider = TestProvider::new(
        [Ok(cancelled(&subscription_id))],
        [Ok(current(
            &subscription_id,
            SubscriptionStatus::Unknown("future_status".into()),
        ))],
    );
    let requested = request(&pool, &org_id, &owner_id, &org_id)
        .await
        .expect("request deletion");
    let requested_lease = claim_due(&pool, "unknown-status-worker")
        .await
        .expect("claim requested")
        .expect("requested work is due");
    advance(&pool, &requested_lease, None)
        .await
        .expect("start billing cancellation")
        .expect("billing transition");
    let billing_lease = claim_due(&pool, "unknown-status-worker")
        .await
        .expect("claim billing")
        .expect("billing work is due");
    advance(&pool, &billing_lease, Some(&provider))
        .await
        .expect("cancel subscription")
        .expect("retention transition");

    // Move the retention deadline into the past so the worker path can run without waiting thirty
    // days in a database-backed test.
    sqlx::query("UPDATE organization_deletions SET purge_after = now() WHERE id = $1::uuid")
        .bind(&requested.id)
        .execute(&pool)
        .await
        .expect("make retention due");
    let retention_lease = claim_due(&pool, "unknown-status-worker")
        .await
        .expect("claim retention")
        .expect("retention is due");
    let retry = advance(&pool, &retention_lease, Some(&provider))
        .await
        .expect("reject unknown status")
        .expect("retry transition");
    assert_eq!(retry.state, DeletionState::Retention);
    let retry_state: (String, bool, Option<String>) = sqlx::query_as(
        "SELECT state, next_attempt_at IS NOT NULL, last_error_code \
         FROM organization_deletions WHERE id = $1::uuid",
    )
    .bind(&requested.id)
    .fetch_one(&pool)
    .await
    .expect("read unknown status retry");
    assert_eq!(
        retry_state,
        (
            "retention".into(),
            true,
            Some("billing_status_unknown".into())
        )
    );
    cleanup(&pool, &org_id, &owner_id).await;
}

#[tokio::test]
async fn retryable_provider_failure_schedules_the_next_attempt() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let _test_lock = db_test_lock().await;
    let (org_id, owner_id) = seed_owner(&pool).await;
    let _subscription_id = link_subscription(&pool, &org_id).await;
    let provider = TestProvider::new(
        [Err(ProviderError {
            status: Some(429),
            code: Some("rate_limit_error".into()),
            kind: ProviderErrorKind::Retryable,
        })],
        [],
    );
    let requested = request(&pool, &org_id, &owner_id, &org_id)
        .await
        .expect("request deletion");
    let requested_lease = claim_due(&pool, "retryable-provider-worker")
        .await
        .expect("claim requested")
        .expect("requested work is due");
    advance(&pool, &requested_lease, None)
        .await
        .expect("start billing cancellation")
        .expect("billing transition");
    let billing_lease = claim_due(&pool, "retryable-provider-worker")
        .await
        .expect("claim billing")
        .expect("billing work is due");
    let retry = advance(&pool, &billing_lease, Some(&provider))
        .await
        .expect("handle retryable provider failure")
        .expect("retry transition");
    assert_eq!(retry.state, DeletionState::CancellingBilling);
    let retry_state: (i32, bool, Option<String>) = sqlx::query_as(
        "SELECT attempt_count, next_attempt_at > now() + interval '30 seconds', last_error_code \
         FROM organization_deletions WHERE id = $1::uuid",
    )
    .bind(&requested.id)
    .fetch_one(&pool)
    .await
    .expect("read retry state");
    // The timestamp must be newly scheduled after the failure, not merely left over from the
    // requested-to-cancelling transition.
    assert_eq!(retry_state, (1, true, Some("rate_limit_error".into())));
    cleanup(&pool, &org_id, &owner_id).await;
}

#[tokio::test]
async fn authentication_failure_fails_without_a_retry_schedule() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let _test_lock = db_test_lock().await;
    let (org_id, owner_id) = seed_owner(&pool).await;
    let _subscription_id = link_subscription(&pool, &org_id).await;
    let provider = TestProvider::new(
        [Err(ProviderError {
            status: Some(401),
            code: Some("authentication_error".into()),
            kind: ProviderErrorKind::Authentication,
        })],
        [],
    );
    let requested = request(&pool, &org_id, &owner_id, &org_id)
        .await
        .expect("request deletion");
    let requested_lease = claim_due(&pool, "authentication-provider-worker")
        .await
        .expect("claim requested")
        .expect("requested work is due");
    advance(&pool, &requested_lease, None)
        .await
        .expect("start billing cancellation")
        .expect("billing transition");
    let billing_lease = claim_due(&pool, "authentication-provider-worker")
        .await
        .expect("claim billing")
        .expect("billing work is due");
    let failed = advance(&pool, &billing_lease, Some(&provider))
        .await
        .expect("handle authentication failure")
        .expect("failed transition");
    assert_eq!(failed.state, DeletionState::Failed);
    let failure_state: (String, Option<String>, bool) = sqlx::query_as(
        "SELECT state, resume_state, next_attempt_at IS NOT NULL \
         FROM organization_deletions WHERE id = $1::uuid",
    )
    .bind(&requested.id)
    .fetch_one(&pool)
    .await
    .expect("read authentication failure");
    assert_eq!(
        failure_state,
        ("failed".into(), Some("cancelling_billing".into()), false)
    );
    let error_code: String = sqlx::query_scalar(
        "SELECT last_error_code FROM organization_deletions WHERE id = $1::uuid",
    )
    .bind(&requested.id)
    .fetch_one(&pool)
    .await
    .expect("read authentication error code");
    assert_eq!(error_code, "billing_unavailable");
    assert_audit_actions(&pool, &org_id, &["org.deletion.failed"]).await;
    cleanup(&pool, &org_id, &owner_id).await;
}

#[tokio::test]
async fn owner_can_recover_from_requested_phase() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let _test_lock = db_test_lock().await;

    let (org_id, owner_id) = seed_owner(&pool).await;
    request(&pool, &org_id, &owner_id, &org_id)
        .await
        .expect("request deletion");
    let recovering = cancel(&pool, &org_id, &owner_id)
        .await
        .expect("cancel requested deletion");
    assert_eq!(recovering.state, DeletionState::Recovering);
    let recovery_lease = claim_due(&pool, "requested-recovery-worker")
        .await
        .expect("claim requested recovery")
        .expect("requested recovery is due");
    let recovered = advance(&pool, &recovery_lease, None)
        .await
        .expect("complete requested recovery")
        .expect("requested recovery transition");
    assert_eq!(recovered.state, DeletionState::Cancelled);
    cleanup(&pool, &org_id, &owner_id).await;
}

#[tokio::test]
async fn owner_can_recover_from_cancelling_billing_phase() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let _test_lock = db_test_lock().await;

    let (org_id, owner_id) = seed_owner(&pool).await;
    request(&pool, &org_id, &owner_id, &org_id)
        .await
        .expect("request deletion");
    let requested_lease = claim_due(&pool, "billing-recovery-worker")
        .await
        .expect("claim requested billing transition")
        .expect("requested billing transition is due");
    let billing_state = advance(&pool, &requested_lease, None)
        .await
        .expect("enter billing cancellation")
        .expect("billing cancellation transition");
    assert_eq!(billing_state.state, DeletionState::CancellingBilling);
    let recovering = cancel(&pool, &org_id, &owner_id)
        .await
        .expect("cancel billing deletion");
    assert_eq!(recovering.state, DeletionState::Recovering);
    let recovery_lease = claim_due(&pool, "billing-recovery-worker")
        .await
        .expect("claim billing recovery")
        .expect("billing recovery is due");
    let recovered = advance(&pool, &recovery_lease, None)
        .await
        .expect("complete billing recovery")
        .expect("billing recovery transition");
    assert_eq!(recovered.state, DeletionState::Cancelled);
    cleanup(&pool, &org_id, &owner_id).await;
}

#[tokio::test]
async fn owner_can_recover_from_retention_phase() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let _test_lock = db_test_lock().await;

    let (org_id, owner_id) = seed_owner(&pool).await;
    request(&pool, &org_id, &owner_id, &org_id)
        .await
        .expect("request deletion");
    let requested_lease = claim_due(&pool, "retention-recovery-worker")
        .await
        .expect("claim requested retention transition")
        .expect("requested retention transition is due");
    advance(&pool, &requested_lease, None)
        .await
        .expect("enter retention billing")
        .expect("retention billing transition");
    let billing_lease = claim_due(&pool, "retention-recovery-worker")
        .await
        .expect("claim retention billing")
        .expect("retention billing is due");
    let retention = advance(&pool, &billing_lease, None)
        .await
        .expect("enter retention")
        .expect("retention transition");
    assert_eq!(retention.state, DeletionState::Retention);
    let recovering = cancel(&pool, &org_id, &owner_id)
        .await
        .expect("cancel retention deletion");
    assert_eq!(recovering.state, DeletionState::Recovering);
    let recovery_lease = claim_due(&pool, "retention-recovery-worker")
        .await
        .expect("claim retention recovery")
        .expect("retention recovery is due");
    let recovered = advance(&pool, &recovery_lease, None)
        .await
        .expect("complete retention recovery")
        .expect("retention recovery transition");
    assert_eq!(recovered.state, DeletionState::Cancelled);
    cleanup(&pool, &org_id, &owner_id).await;
}

#[tokio::test]
async fn owner_recovery_wins_an_in_flight_provider_cancellation() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let _test_lock = db_test_lock().await;
    let (org_id, owner_id) = seed_owner(&pool).await;
    let subscription_id = link_subscription(&pool, &org_id).await;
    sqlx::query("UPDATE organizations SET tier = 'team' WHERE id = $1")
        .bind(&org_id)
        .execute(&pool)
        .await
        .expect("seed team tier");
    let _requested = request(&pool, &org_id, &owner_id, &org_id)
        .await
        .expect("request deletion");
    let requested_lease = claim_due(&pool, "racing-recovery-worker")
        .await
        .expect("claim requested")
        .expect("requested work is due");
    advance(&pool, &requested_lease, None)
        .await
        .expect("start billing cancellation")
        .expect("billing transition");
    let billing_lease = claim_due(&pool, "racing-recovery-worker")
        .await
        .expect("claim billing")
        .expect("billing work is due");
    let (provider, started, release) = TestProvider::new(
        [Ok(cancelled(&subscription_id))],
        [Ok(cancelled(&subscription_id))],
    )
    .with_blocking_cancellation();
    let provider = Arc::new(provider);
    let advance_task = tokio::spawn({
        let pool = pool.clone();
        let lease = billing_lease.clone();
        let provider = provider.clone();
        async move { advance(&pool, &lease, Some(provider.as_ref())).await }
    });
    tokio::time::timeout(Duration::from_secs(5), started.notified())
        .await
        .expect("provider cancellation started");

    let recovering = cancel(&pool, &org_id, &owner_id)
        .await
        .expect("request recovery while cancellation is in flight");
    assert_eq!(recovering.state, DeletionState::Recovering);
    release.notify_one();
    let stale_worker_result = advance_task
        .await
        .expect("join cancellation worker")
        .expect("complete cancellation worker");
    // None means the worker compare-and-set lost to the newer recovery state.
    assert!(stale_worker_result.is_none());

    let recovery_lease = claim_due(&pool, "racing-recovery-worker")
        .await
        .expect("claim recovery")
        .expect("recovery is due");
    let recovered = advance(&pool, &recovery_lease, Some(provider.as_ref()))
        .await
        .expect("complete recovery")
        .expect("recovery transition");
    assert_eq!(recovered.state, DeletionState::Cancelled);
    assert_eq!(status(&pool, &org_id, &owner_id).await.unwrap(), recovered);
    let tier: String = sqlx::query_scalar("SELECT tier FROM organizations WHERE id = $1")
        .bind(&org_id)
        .fetch_one(&pool)
        .await
        .expect("read recovered tier");
    assert_eq!(tier, "free");
    cleanup(&pool, &org_id, &owner_id).await;
}

#[tokio::test]
async fn recovery_restores_team_from_a_provider_observation() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let _test_lock = db_test_lock().await;
    let (org_id, owner_id) = seed_owner(&pool).await;
    let subscription_id = link_subscription(&pool, &org_id).await;
    let provider = TestProvider::new(
        [],
        [Ok(current(&subscription_id, SubscriptionStatus::Active))],
    );
    let _requested = request(&pool, &org_id, &owner_id, &org_id)
        .await
        .expect("request deletion");
    let requested_lease = claim_due(&pool, "team-recovery-worker")
        .await
        .expect("claim requested")
        .expect("requested work is due");
    advance(&pool, &requested_lease, None)
        .await
        .expect("start billing cancellation")
        .expect("billing transition");
    let _billing_lease = claim_due(&pool, "team-recovery-worker")
        .await
        .expect("claim billing")
        .expect("billing work is due");
    let recovering = cancel(&pool, &org_id, &owner_id)
        .await
        .expect("request recovery");
    assert_eq!(recovering.state, DeletionState::Recovering);
    let recovery_lease = claim_due(&pool, "team-recovery-worker")
        .await
        .expect("claim recovery")
        .expect("recovery is due");
    assert_eq!(recovery_lease.state, DeletionState::Recovering);
    let recovered = advance(&pool, &recovery_lease, Some(&provider))
        .await
        .expect("reconcile active subscription")
        .expect("recovery transition");
    assert_eq!(recovered.state, DeletionState::Cancelled);
    let tier: String = sqlx::query_scalar("SELECT tier FROM organizations WHERE id = $1")
        .bind(&org_id)
        .fetch_one(&pool)
        .await
        .expect("read recovered tier");
    assert_eq!(tier, "team");
    cleanup(&pool, &org_id, &owner_id).await;
}

#[tokio::test]
async fn recovery_restores_free_from_a_cancelled_provider_observation() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let _test_lock = db_test_lock().await;
    let (org_id, owner_id) = seed_owner(&pool).await;
    let subscription_id = link_subscription(&pool, &org_id).await;
    let provider = TestProvider::new([], [Ok(cancelled(&subscription_id))]);
    request(&pool, &org_id, &owner_id, &org_id)
        .await
        .expect("request deletion");
    let requested_lease = claim_due(&pool, "free-recovery-worker")
        .await
        .expect("claim requested")
        .expect("requested work is due");
    advance(&pool, &requested_lease, None)
        .await
        .expect("start billing cancellation")
        .expect("billing transition");
    let _billing_lease = claim_due(&pool, "free-recovery-worker")
        .await
        .expect("claim billing")
        .expect("billing work is due");
    cancel(&pool, &org_id, &owner_id)
        .await
        .expect("request recovery");
    let recovery_lease = claim_due(&pool, "free-recovery-worker")
        .await
        .expect("claim recovery")
        .expect("recovery is due");
    let recovered = advance(&pool, &recovery_lease, Some(&provider))
        .await
        .expect("reconcile missing subscription")
        .expect("recovery transition");
    assert_eq!(recovered.state, DeletionState::Cancelled);
    let tier: String = sqlx::query_scalar("SELECT tier FROM organizations WHERE id = $1")
        .bind(&org_id)
        .fetch_one(&pool)
        .await
        .expect("read recovered tier");
    assert_eq!(tier, "free");
    cleanup(&pool, &org_id, &owner_id).await;
}

#[tokio::test]
async fn fresh_operator_observation_unblocks_an_unconfigured_provider() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let _test_lock = db_test_lock().await;
    let (org_id, owner_id) = seed_owner(&pool).await;
    let subscription_id = link_subscription(&pool, &org_id).await;
    let requested = request(&pool, &org_id, &owner_id, &org_id)
        .await
        .expect("request deletion");
    let requested_lease = claim_due(&pool, "operator-observation-worker")
        .await
        .expect("claim requested")
        .expect("requested work is due");
    advance(&pool, &requested_lease, None)
        .await
        .expect("start billing cancellation")
        .expect("billing transition");
    let billing_lease = claim_due(&pool, "operator-observation-worker")
        .await
        .expect("claim billing")
        .expect("billing work is due");
    let failed = advance(&pool, &billing_lease, None)
        .await
        .expect("record unavailable provider")
        .expect("failed transition");
    assert_eq!(failed.state, DeletionState::Failed);

    // PostgreSQL resolves the special "now" timestamp literal when it casts the bound value,
    // keeping these observations fresh without adding a time-formatting dependency to the test.
    // Empty fields intentionally exercise the required-field validation one at a time.
    for (operator, observed_status, observed_at, reason, evidence) in [
        ("", "canceled", "now", "reason", "evidence"),
        ("operator-1", "", "now", "reason", "evidence"),
        ("operator-1", "canceled", "", "reason", "evidence"),
        ("operator-1", "canceled", "now", "", "evidence"),
        ("operator-1", "canceled", "now", "reason", ""),
    ] {
        let result = record_operator_observation(
            &pool,
            &org_id,
            operator,
            OperatorObservation {
                subscription_id: &subscription_id,
                observed_status,
                observed_at,
                reason,
                evidence,
            },
        )
        .await;
        assert!(matches!(result, Err(Error::BadRequest(_))));
    }

    let mismatched = record_operator_observation(
        &pool,
        &org_id,
        "operator-1",
        OperatorObservation {
            subscription_id: "sub-other",
            observed_status: "canceled",
            observed_at: "now",
            reason: "provider credentials are being rotated",
            evidence: "stripe-dashboard-request-1",
        },
    )
    .await;
    assert!(matches!(mismatched, Err(Error::Conflict(_))));
    let non_terminal = record_operator_observation(
        &pool,
        &org_id,
        "operator-1",
        OperatorObservation {
            subscription_id: &subscription_id,
            observed_status: "active",
            observed_at: "now",
            reason: "provider credentials are being rotated",
            evidence: "stripe-dashboard-request-1",
        },
    )
    .await;
    assert!(matches!(non_terminal, Err(Error::BadRequest(_))));

    let observed = record_operator_observation(
        &pool,
        &org_id,
        "operator-1",
        OperatorObservation {
            subscription_id: &subscription_id,
            observed_status: "canceled",
            observed_at: "now",
            reason: "provider credentials are being rotated",
            evidence: "stripe-dashboard-request-1",
        },
    )
    .await
    .expect("record operator observation");
    assert_eq!(observed.state, DeletionState::Retention);
    // Age the operator observation beyond the fifteen-minute freshness bound while making the
    // retention work due, proving that an old manual result cannot unlock the purge.
    sqlx::query(
        "UPDATE organization_deletions SET requested_at = now() - interval '1 hour', \
         purge_after = now(), \
         billing_checked_at = now() - interval '16 minutes' WHERE id = $1::uuid",
    )
    .bind(&requested.id)
    .execute(&pool)
    .await
    .expect("age operator observation");
    let stale_retention_lease = claim_due(&pool, "operator-observation-worker")
        .await
        .expect("claim stale retention")
        .expect("stale retention is due");
    let stale = advance(&pool, &stale_retention_lease, None)
        .await
        .expect("reject stale operator observation")
        .expect("failed stale retention transition");
    assert_eq!(stale.state, DeletionState::Failed);
    let observed = record_operator_observation(
        &pool,
        &org_id,
        "operator-1",
        OperatorObservation {
            subscription_id: &subscription_id,
            observed_status: "canceled",
            observed_at: "now",
            reason: "provider credentials are being rotated",
            evidence: "stripe-dashboard-request-1",
        },
    )
    .await
    .expect("refresh operator observation");
    assert_eq!(observed.state, DeletionState::Retention);
    let retention_lease = claim_due(&pool, "operator-observation-worker")
        .await
        .expect("claim fresh retention")
        .expect("fresh retention is due");
    let purging = advance(&pool, &retention_lease, None)
        .await
        .expect("use fresh operator observation")
        .expect("purge transition");
    assert_eq!(purging.state, DeletionState::Purging);
    let purging_lease = claim_due(&pool, "operator-observation-worker")
        .await
        .expect("claim purge")
        .expect("purge is due");
    let completed = advance(&pool, &purging_lease, None)
        .await
        .expect("purge observed organisation")
        .expect("completed transition");
    assert_eq!(completed.state, DeletionState::Completed);
    assert_audit_actions(&pool, &org_id, &["org.deletion.billing_observed"]).await;
    cleanup(&pool, &org_id, &owner_id).await;
}
