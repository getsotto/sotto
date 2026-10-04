//! Database-backed checks for the durable billing operation identity.
//!
//! These tests are opt-in because they create and remove rows in a disposable Postgres database.

use async_trait::async_trait;
use sqlx::{postgres::PgConnectOptions, PgPool};
use std::str::FromStr;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::sync::Notify;
use tokio::time::{timeout, Duration};

use sotto_server::billing_catalogue::BillingOffer;
use sotto_server::billing_operations::{
    begin_organization_operation, begin_personal_operation, load_reconciliation_candidates,
    reconcile_operation, record_provider_result, BeginOperation, BillingOperation,
    BillingOperationError, BillingOperationProvider, BillingOperationState, BillingRecoveryError,
    ProviderResolution,
};
use sotto_server::db;

async fn pool_or_skip() -> Option<PgPool> {
    if std::env::var("SOTTO_RUN_DB_TESTS").as_deref() != Ok("1") {
        eprintln!("skipping billing operation tests: set SOTTO_RUN_DB_TESTS=1");
        return None;
    }
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL for opted-in database tests");
    let options = PgConnectOptions::from_str(&url).expect("parse DATABASE_URL");
    assert!(
        matches!(options.get_host(), "localhost" | "127.0.0.1" | "::1"),
        "refusing billing operation tests against non-local host: {}",
        options.get_host()
    );
    let pool = db::connect(&url).await.expect("connect to test database");
    db::migrate(&pool).await.expect("apply migrations");
    Some(pool)
}

async fn cleanup(pool: &PgPool, user_id: &str) {
    sqlx::query("DELETE FROM billing_operations WHERE actor_user_id = $1")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("clean billing operation fixtures");
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("clean billing operation user fixture");
}

async fn seed_user(pool: &PgPool, user_id: &str) {
    sqlx::query(
        "INSERT INTO users (id, oauth_provider, oauth_subject) VALUES ($1, $2, $1) \
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(user_id)
    .bind(format!("billing-operation-test-{user_id}"))
    .execute(pool)
    .await
    .expect("seed billing operation user fixture");
}

async fn seed_organization(pool: &PgPool, organization_id: &str, owner_id: &str, member_id: &str) {
    sqlx::query(
        "INSERT INTO organizations (id, enc_name, created_by) VALUES ($1, $2, $3) \
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(organization_id)
    .bind(vec![0_u8])
    .bind(owner_id)
    .execute(pool)
    .await
    .expect("seed billing operation organisation fixture");
    sqlx::query(
        "INSERT INTO organization_memberships (org_id, user_id, role) VALUES \
         ($1, $2, 'owner'), ($1, $3, 'member') \
         ON CONFLICT (org_id, user_id) DO UPDATE SET role = EXCLUDED.role",
    )
    .bind(organization_id)
    .bind(owner_id)
    .bind(member_id)
    .execute(pool)
    .await
    .expect("seed billing operation memberships");
}

async fn cleanup_organization(pool: &PgPool, organization_id: &str) {
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(organization_id)
        .execute(pool)
        .await
        .expect("clean billing operation organisation fixture");
}

#[tokio::test]
async fn operation_identity_is_idempotent_and_conflicts_on_changed_request() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    const USER_ID: &str = "billing-operation-test-idempotency";
    cleanup(&pool, USER_ID).await;
    seed_user(&pool, USER_ID).await;

    let mut tx = pool.begin().await.expect("begin operation transaction");
    let first_operation_id = match begin_personal_operation(
        &mut tx,
        USER_ID,
        "billing-operation-test-key",
        BillingOffer::StandardMonthly,
        1,
        4_000_000_000,
        "https://app.sotto.test/billing",
        "https://app.sotto.test/billing",
    )
    .await
    .expect("create operation")
    {
        BeginOperation::Created(operation) => {
            assert_eq!(operation.state, BillingOperationState::Pending);
            operation.operation_id
        }
        BeginOperation::AlreadyExists(_) => panic!("fixture operation unexpectedly existed"),
    };
    tx.commit().await.expect("commit operation identity");

    let mut replay_tx = pool.begin().await.expect("begin replay transaction");
    assert!(matches!(
        begin_personal_operation(
            &mut replay_tx,
            USER_ID,
            "billing-operation-test-key",
            BillingOffer::StandardMonthly,
            1,
            4_000_000_000,
            "https://app.sotto.test/billing",
            "https://app.sotto.test/billing",
        )
            .await
            .expect("replay operation"),
        BeginOperation::AlreadyExists(operation) if operation.operation_id == first_operation_id
    ));
    replay_tx.commit().await.expect("commit replay");

    let mut conflict_tx = pool.begin().await.expect("begin conflict transaction");
    assert!(matches!(
        begin_personal_operation(
            &mut conflict_tx,
            USER_ID,
            "billing-operation-test-key",
            BillingOffer::StandardAnnual,
            1,
            4_000_000_000,
            "https://app.sotto.test/billing",
            "https://app.sotto.test/billing",
        )
        .await,
        Err(BillingOperationError::IdempotencyConflict)
    ));
    conflict_tx.rollback().await.expect("rollback conflict");

    cleanup(&pool, USER_ID).await;
}

struct SuccessfulProvider {
    calls: Arc<AtomicUsize>,
    keys: Arc<std::sync::Mutex<Vec<String>>>,
}

#[derive(Clone)]
struct BlockingProvider {
    calls: Arc<AtomicUsize>,
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait]
impl BillingOperationProvider for BlockingProvider {
    async fn resolve(
        &self,
        _operation: &BillingOperation,
    ) -> Result<ProviderResolution, BillingRecoveryError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        self.release.notified().await;
        Ok(ProviderResolution::Succeeded {
            provider_operation_id: "pi_concurrent_recovery".into(),
            result_code: "paid".into(),
        })
    }
}

#[async_trait]
impl BillingOperationProvider for SuccessfulProvider {
    async fn resolve(
        &self,
        operation: &BillingOperation,
    ) -> Result<ProviderResolution, sotto_server::billing_operations::BillingRecoveryError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.keys
            .lock()
            .expect("provider key lock")
            .push(operation.provider_idempotency_key.clone());
        Ok(ProviderResolution::Succeeded {
            provider_operation_id: "pi_recovered".into(),
            result_code: "paid".into(),
        })
    }
}

#[tokio::test]
async fn unknown_provider_result_is_reconciled_after_restart() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    const USER_ID: &str = "billing-operation-test-recovery";
    cleanup(&pool, USER_ID).await;
    seed_user(&pool, USER_ID).await;

    let mut tx = pool.begin().await.expect("begin operation transaction");
    let operation_id = match begin_personal_operation(
        &mut tx,
        USER_ID,
        "billing-operation-test-key",
        BillingOffer::StandardMonthly,
        1,
        4_000_000_000,
        "https://app.sotto.test/billing",
        "https://app.sotto.test/billing",
    )
    .await
    .expect("create operation")
    {
        BeginOperation::Created(operation) => operation.operation_id,
        BeginOperation::AlreadyExists(_) => panic!("fixture operation unexpectedly existed"),
    };
    tx.commit().await.expect("commit operation identity");

    let mut result_tx = pool
        .begin()
        .await
        .expect("begin provider result transaction");
    let stored = record_provider_result(
        &mut result_tx,
        &operation_id,
        BillingOperationState::Unknown,
        None,
        Some("provider_timeout"),
    )
    .await
    .expect("record unknown provider result");
    assert_eq!(stored.state, BillingOperationState::Unknown);
    result_tx.commit().await.expect("commit provider result");

    let mut replay_tx = pool
        .begin()
        .await
        .expect("begin provider replay transaction");
    let replayed = record_provider_result(
        &mut replay_tx,
        &operation_id,
        BillingOperationState::Unknown,
        None,
        Some("provider_timeout"),
    )
    .await
    .expect("replay identical provider result");
    assert_eq!(replayed.state, BillingOperationState::Unknown);
    replay_tx.commit().await.expect("commit provider replay");

    let mut conflicting_tx = pool
        .begin()
        .await
        .expect("begin conflicting provider result transaction");
    assert!(matches!(
        record_provider_result(
            &mut conflicting_tx,
            &operation_id,
            BillingOperationState::Succeeded,
            Some("pi_conflicting"),
            Some("paid"),
        )
        .await,
        Err(BillingOperationError::ResultConflict)
    ));
    conflicting_tx
        .rollback()
        .await
        .expect("rollback conflicting provider result");

    let candidates = load_reconciliation_candidates(&pool, 10)
        .await
        .expect("load reconciliation candidates");
    assert!(candidates
        .iter()
        .any(|candidate| candidate.operation_id == operation_id));

    let provider = SuccessfulProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        keys: Arc::new(std::sync::Mutex::new(Vec::new())),
    };
    let resolved = reconcile_operation(&pool, &provider, &operation_id)
        .await
        .expect("reconcile provider result after restart");
    assert_eq!(resolved.state, BillingOperationState::Succeeded);
    assert_eq!(
        resolved.provider_operation_id.as_deref(),
        Some("pi_recovered")
    );
    assert!(!load_reconciliation_candidates(&pool, 10)
        .await
        .expect("load resolved candidates")
        .iter()
        .any(|candidate| candidate.operation_id == operation_id));
    let replayed = reconcile_operation(&pool, &provider, &operation_id)
        .await
        .expect("reconcile already terminal operation");
    assert_eq!(replayed.state, BillingOperationState::Succeeded);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(provider.keys.lock().expect("provider key lock").len(), 1);

    cleanup(&pool, USER_ID).await;
}

#[tokio::test]
async fn concurrent_recovery_claims_the_provider_call_once() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    const USER_ID: &str = "billing-operation-test-concurrent";
    cleanup(&pool, USER_ID).await;
    seed_user(&pool, USER_ID).await;

    let mut tx = pool.begin().await.expect("begin operation transaction");
    let operation_id = match begin_personal_operation(
        &mut tx,
        USER_ID,
        "billing-operation-test-key",
        BillingOffer::StandardMonthly,
        1,
        4_000_000_000,
        "https://app.sotto.test/billing",
        "https://app.sotto.test/billing",
    )
    .await
    .expect("create operation")
    {
        BeginOperation::Created(operation) => operation.operation_id,
        BeginOperation::AlreadyExists(_) => panic!("fixture operation unexpectedly existed"),
    };
    tx.commit().await.expect("commit operation identity");

    assert!(matches!(
        reconcile_operation(
            &pool,
            &SuccessfulProvider {
                calls: Arc::new(AtomicUsize::new(0)),
                keys: Arc::new(std::sync::Mutex::new(Vec::new())),
            },
            &operation_id
        )
        .await,
        Err(BillingRecoveryError::InProgress)
    ));

    let mut unknown_tx = pool
        .begin()
        .await
        .expect("begin unknown result transaction");
    record_provider_result(
        &mut unknown_tx,
        &operation_id,
        BillingOperationState::Unknown,
        None,
        Some("provider_timeout"),
    )
    .await
    .expect("record unknown provider result");
    unknown_tx.commit().await.expect("commit unknown result");

    let provider = BlockingProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        entered: Arc::new(Notify::new()),
        release: Arc::new(Notify::new()),
    };
    let first_provider = provider.clone();
    let first_pool = pool.clone();
    let first_operation_id = operation_id.clone();
    let first = tokio::spawn(async move {
        reconcile_operation(&first_pool, &first_provider, &first_operation_id).await
    });
    timeout(Duration::from_secs(5), provider.entered.notified())
        .await
        .expect("first recovery entered provider");

    let second_provider = provider.clone();
    let second_pool = pool.clone();
    let second_operation_id = operation_id.clone();
    let second = tokio::spawn(async move {
        reconcile_operation(&second_pool, &second_provider, &second_operation_id).await
    });
    assert!(matches!(
        timeout(Duration::from_secs(5), second)
            .await
            .expect("second recovery completed")
            .expect("second recovery task joined"),
        Err(BillingRecoveryError::InProgress)
    ));
    provider.release.notify_one();
    let resolved = first
        .await
        .expect("first recovery task joined")
        .expect("first recovery succeeded");
    assert_eq!(resolved.state, BillingOperationState::Succeeded);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);

    cleanup(&pool, USER_ID).await;
}

#[tokio::test]
async fn organisation_operations_require_role_and_active_lifecycle() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    const ORGANIZATION_ID: &str = "billing-operation-test-org";
    const OWNER_ID: &str = "billing-operation-test-owner";
    const MEMBER_ID: &str = "billing-operation-test-member";
    cleanup_organization(&pool, ORGANIZATION_ID).await;
    cleanup(&pool, OWNER_ID).await;
    cleanup(&pool, MEMBER_ID).await;
    seed_user(&pool, OWNER_ID).await;
    seed_user(&pool, MEMBER_ID).await;
    seed_organization(&pool, ORGANIZATION_ID, OWNER_ID, MEMBER_ID).await;

    let mut member_tx = pool.begin().await.expect("begin member transaction");
    assert!(matches!(
        begin_organization_operation(
            &mut member_tx,
            MEMBER_ID,
            ORGANIZATION_ID,
            "member-key",
            BillingOffer::StandardMonthly,
            1,
            4_000_000_000,
            "https://app.sotto.test/billing",
            "https://app.sotto.test/billing",
        )
        .await,
        Err(BillingOperationError::Unauthorised)
    ));
    member_tx
        .rollback()
        .await
        .expect("rollback member operation");

    let mut owner_active_tx = pool.begin().await.expect("begin active owner transaction");
    assert!(matches!(
        begin_organization_operation(
            &mut owner_active_tx,
            OWNER_ID,
            ORGANIZATION_ID,
            "owner-active-key",
            BillingOffer::StandardMonthly,
            1,
            4_000_000_000,
            "https://app.sotto.test/billing",
            "https://app.sotto.test/billing",
        )
        .await,
        Ok(BeginOperation::Created(_))
    ));
    owner_active_tx
        .rollback()
        .await
        .expect("rollback active owner operation");

    sqlx::query("UPDATE organizations SET lifecycle_state = 'deleting' WHERE id = $1")
        .bind(ORGANIZATION_ID)
        .execute(&pool)
        .await
        .expect("mark organisation deleting");
    let mut owner_tx = pool.begin().await.expect("begin owner transaction");
    assert!(matches!(
        begin_organization_operation(
            &mut owner_tx,
            OWNER_ID,
            ORGANIZATION_ID,
            "owner-key",
            BillingOffer::StandardMonthly,
            1,
            4_000_000_000,
            "https://app.sotto.test/billing",
            "https://app.sotto.test/billing",
        )
        .await,
        Err(BillingOperationError::OrganisationNotActive)
    ));
    owner_tx.rollback().await.expect("rollback owner operation");

    cleanup_organization(&pool, ORGANIZATION_ID).await;
    cleanup(&pool, OWNER_ID).await;
    cleanup(&pool, MEMBER_ID).await;
}
