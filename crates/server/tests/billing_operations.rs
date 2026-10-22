//! Database-backed checks for the durable billing operation identity.
//!
//! These tests are opt-in because they create and remove rows in a disposable Postgres database.

use async_trait::async_trait;
use sqlx::{postgres::PgConnectOptions, PgPool, Row};
use std::str::FromStr;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, OnceLock,
};
use tokio::sync::{Mutex as AsyncMutex, Notify};
use tokio::time::{timeout, Duration};

use sotto_server::billing::{
    self, BillingState, PersonalCheckoutSession, PersonalCheckoutStatus, ProviderResult,
    SubscriptionObservation, SubscriptionProvider,
};
use sotto_server::billing_catalogue::BillingOffer;
use sotto_server::billing_operations::{
    begin_organization_operation, begin_personal_operation, claim_reconciliation,
    load_reconciliation_candidates, reconcile_operation, record_personal_checkout_request,
    record_provider_result, release_reconciliation_failure, BeginOperation, BillingOperation,
    BillingOperationError, BillingOperationProvider, BillingOperationState, BillingRecoveryError,
    ProviderResolution,
};
use sotto_server::config::BillingPriceIds;
use sotto_server::db;

fn test_lock() -> &'static AsyncMutex<()> {
    static LOCK: OnceLock<AsyncMutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| AsyncMutex::new(()))
}

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
    sqlx::query("DELETE FROM billing_personal_accounts WHERE user_id = $1")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("clean personal billing account fixture");
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

#[derive(Clone)]
struct RecoveryCheckoutProvider {
    calls: Arc<AtomicUsize>,
    received: Arc<std::sync::Mutex<Vec<CapturedCheckoutRequest>>>,
    status_requests: Arc<std::sync::Mutex<Vec<(String, String)>>>,
    lose_first_response: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CapturedCheckoutRequest {
    price_id: String,
    idempotency_key: String,
    success_url: String,
    cancel_url: String,
    expires_at_epoch: i64,
}

#[async_trait]
impl SubscriptionProvider for RecoveryCheckoutProvider {
    async fn create_checkout(
        &self,
        _org_id: &str,
        _customer: Option<&str>,
        _success_url: &str,
        _cancel_url: &str,
    ) -> ProviderResult<String> {
        Ok("https://checkout.example.test/mock".into())
    }

    async fn create_personal_checkout(
        &self,
        _user_id: &str,
        _customer: Option<&str>,
        price_id: &str,
        _operation_id: &str,
        _allocation_reference: &str,
        idempotency_key: &str,
        expires_at_epoch: i64,
        success_url: &str,
        cancel_url: &str,
    ) -> ProviderResult<PersonalCheckoutSession> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        self.received.lock().unwrap().push(CapturedCheckoutRequest {
            price_id: price_id.into(),
            idempotency_key: idempotency_key.into(),
            success_url: success_url.into(),
            cancel_url: cancel_url.into(),
            expires_at_epoch,
        });
        if self.lose_first_response && call == 0 {
            // Stripe accepted and retained this session for the idempotency key, but the
            // response was lost before Sotto could persist the session id.
            return Err(sotto_server::billing::ProviderError {
                status: None,
                code: None,
                kind: sotto_server::billing::ProviderErrorKind::Retryable,
            });
        }
        Ok(PersonalCheckoutSession {
            session_id: Some("cs_recovered_personal".into()),
            checkout_url: "https://checkout.example.test/cs_recovered_personal".into(),
        })
    }

    async fn get_personal_checkout_status(
        &self,
        session_id: &str,
        operation_id: &str,
    ) -> ProviderResult<PersonalCheckoutStatus> {
        self.status_requests
            .lock()
            .unwrap()
            .push((session_id.to_string(), operation_id.to_string()));
        Ok(PersonalCheckoutStatus::ExpiredUnpaid)
    }

    async fn create_portal(
        &self,
        _customer: &str,
        _return_url: &str,
        _configuration_id: Option<&str>,
    ) -> ProviderResult<String> {
        Ok("https://billing.example.test/portal".into())
    }

    async fn get_subscription(
        &self,
        _subscription_id: &str,
    ) -> ProviderResult<SubscriptionObservation> {
        Ok(SubscriptionObservation::Missing)
    }

    async fn cancel_subscription(
        &self,
        _subscription_id: &str,
        _idempotency_key: &str,
        _org_id: &str,
    ) -> ProviderResult<SubscriptionObservation> {
        Ok(SubscriptionObservation::Missing)
    }
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
    let _guard = test_lock().lock().await;
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

#[tokio::test]
async fn interrupted_personal_checkout_reuses_its_operation_and_records_the_session_id() {
    let _guard = test_lock().lock().await;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    const USER_ID: &str = "billing-operation-test-checkout-recovery";
    cleanup(&pool, USER_ID).await;
    seed_user(&pool, USER_ID).await;

    let mut tx = pool.begin().await.expect("begin operation transaction");
    let operation = match begin_personal_operation(
        &mut tx,
        USER_ID,
        "checkout-recovery-key",
        BillingOffer::StandardMonthly,
        1,
        4_000_000_000,
        "https://app.sotto.test/billing",
        "https://app.sotto.test/billing",
    )
    .await
    .expect("persist personal checkout operation")
    {
        BeginOperation::Created(operation) => operation,
        BeginOperation::AlreadyExists(_) => panic!("fixture operation unexpectedly existed"),
    };
    sotto_server::personal_billing::begin_account(
        &mut tx,
        USER_ID,
        &operation.operation_id,
        BillingOffer::StandardMonthly,
        4_000_000_100,
        4_000_000_000,
    )
    .await
    .expect("persist the original personal account binding");
    record_personal_checkout_request(
        &mut tx,
        &operation.operation_id,
        "price_original_monthly",
        "https://app.sotto.test/app?billing=success",
        "https://app.sotto.test/app?billing=cancelled",
    )
    .await
    .expect("persist original provider request parameters");
    tx.commit().await.expect("commit original request");
    sqlx::query(
        "UPDATE billing_operations SET reconciliation_lease_until = now() - interval '1 second' \
         WHERE operation_id = $1",
    )
    .bind(&operation.operation_id)
    .execute(&pool)
    .await
    .expect("expire caller lease for restart recovery");

    let provider = RecoveryCheckoutProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        received: Arc::new(std::sync::Mutex::new(Vec::new())),
        status_requests: Arc::new(std::sync::Mutex::new(Vec::new())),
        lose_first_response: true,
    };
    let billing_state = BillingState::with_provider(
        Arc::new(provider.clone()),
        "whsec_test".into(),
        "https://app.sotto.test".into(),
    )
    .with_price_catalogue(BillingPriceIds {
        standard_monthly: "price_standard_monthly".into(),
        standard_annual: "price_standard_annual".into(),
        founding_monthly: "price_founding_monthly".into(),
        founding_annual: "price_founding_annual".into(),
    });

    assert!(
        billing::recover_personal_billing_operation_once(&pool, &billing_state)
            .await
            .expect("retry an ambiguous provider result")
    );
    sqlx::query(
        "UPDATE billing_operations SET reconciliation_next_attempt_at = now() - interval '1 second' \
         WHERE operation_id = $1",
    )
    .bind(&operation.operation_id)
    .execute(&pool)
    .await
    .expect("make the bounded retry due");
    assert!(
        billing::recover_personal_billing_operation_once(&pool, &billing_state)
            .await
            .expect("recover the provider-accepted session after response loss")
    );
    let recovered = sqlx::query(
        "SELECT state, provider_checkout_session_id, provider_checkout_url, result_code \
         FROM billing_operations WHERE operation_id = $1",
    )
    .bind(&operation.operation_id)
    .fetch_one(&pool)
    .await
    .expect("read recovered checkout");
    use sqlx::Row;
    assert_eq!(
        recovered.try_get::<String, _>("state").unwrap(),
        "succeeded"
    );
    assert_eq!(
        recovered
            .try_get::<Option<String>, _>("provider_checkout_session_id")
            .unwrap()
            .as_deref(),
        Some("cs_recovered_personal")
    );
    assert_eq!(
        recovered
            .try_get::<String, _>("result_code")
            .unwrap()
            .as_str(),
        "checkout_created"
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    let original_request = CapturedCheckoutRequest {
        price_id: "price_original_monthly".into(),
        idempotency_key: operation.provider_idempotency_key.clone(),
        success_url: "https://app.sotto.test/app?billing=success".into(),
        cancel_url: "https://app.sotto.test/app?billing=cancelled".into(),
        expires_at_epoch: 4_000_000_100,
    };
    assert_eq!(
        provider.received.lock().unwrap().as_slice(),
        &[original_request.clone(), original_request]
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT state FROM billing_personal_accounts WHERE operation_id = $1",
        )
        .bind(&operation.operation_id)
        .fetch_one(&pool)
        .await
        .expect("read personal account"),
        "pending",
        "creating a Checkout session must not record payment"
    );
    assert!(
        !billing::recover_personal_billing_operation_once(&pool, &billing_state)
            .await
            .expect("settled recovery queue is idle")
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);

    cleanup(&pool, USER_ID).await;
}

#[tokio::test]
async fn refund_required_checkout_is_parked_without_another_provider_request() {
    let _guard = test_lock().lock().await;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    const USER_ID: &str = "billing-operation-test-refund-required-recovery";
    cleanup(&pool, USER_ID).await;
    seed_user(&pool, USER_ID).await;
    let mut tx = pool.begin().await.expect("begin operation transaction");
    let operation = match begin_personal_operation(
        &mut tx,
        USER_ID,
        "refund-required-recovery-key",
        BillingOffer::StandardMonthly,
        1,
        4_000_000_000,
        "https://app.sotto.test/billing",
        "https://app.sotto.test/billing",
    )
    .await
    .expect("create operation")
    {
        BeginOperation::Created(operation) => operation,
        BeginOperation::AlreadyExists(_) => panic!("fixture operation unexpectedly existed"),
    };
    sotto_server::personal_billing::begin_account(
        &mut tx,
        USER_ID,
        &operation.operation_id,
        BillingOffer::StandardMonthly,
        4_000_000_100,
        4_000_000_000,
    )
    .await
    .expect("persist personal account");
    record_personal_checkout_request(
        &mut tx,
        &operation.operation_id,
        "price_original_monthly",
        "https://app.sotto.test/app?billing=success",
        "https://app.sotto.test/app?billing=cancelled",
    )
    .await
    .expect("persist provider request");
    sqlx::query(
        "UPDATE billing_personal_accounts SET state='refund_required', \
         stripe_customer_id='cus_refund_required', stripe_subscription_id='sub_refund_required', \
         payment_reference='pi_refund_required' WHERE operation_id=$1",
    )
    .bind(&operation.operation_id)
    .execute(&mut *tx)
    .await
    .expect("record paid checkout awaiting refund");
    tx.commit().await.expect("commit operation");
    sqlx::query(
        "UPDATE billing_operations SET reconciliation_lease_until = now() - interval '1 second' \
         WHERE operation_id=$1",
    )
    .bind(&operation.operation_id)
    .execute(&pool)
    .await
    .expect("expire creator lease");

    let provider = RecoveryCheckoutProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        received: Arc::new(std::sync::Mutex::new(Vec::new())),
        status_requests: Arc::new(std::sync::Mutex::new(Vec::new())),
        lose_first_response: false,
    };
    let billing_state = BillingState::with_provider(
        Arc::new(provider.clone()),
        "whsec_test".into(),
        "https://app.sotto.test".into(),
    )
    .with_price_catalogue(BillingPriceIds {
        standard_monthly: "price_standard_monthly".into(),
        standard_annual: "price_standard_annual".into(),
        founding_monthly: "price_founding_monthly".into(),
        founding_annual: "price_founding_annual".into(),
    });

    assert!(
        billing::recover_personal_billing_operation_once(&pool, &billing_state)
            .await
            .expect("park already-paid refund case")
    );
    let operation_state: (String, Option<String>) =
        sqlx::query_as("SELECT state, result_code FROM billing_operations WHERE operation_id=$1")
            .bind(&operation.operation_id)
            .fetch_one(&pool)
            .await
            .expect("read parked operation");
    assert_eq!(operation_state.0, "manual_review");
    assert_eq!(
        operation_state.1.as_deref(),
        Some("paid_checkout_requires_refund")
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    assert_eq!(provider.received.lock().unwrap().len(), 0);

    cleanup(&pool, USER_ID).await;
}

#[tokio::test]
async fn locally_expired_personal_checkout_is_reconciled_from_legacy_checkout_url() {
    let _guard = test_lock().lock().await;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    const USER_ID: &str = "billing-operation-test-expired-session-recovery";
    cleanup(&pool, USER_ID).await;
    seed_user(&pool, USER_ID).await;
    let mut tx = pool.begin().await.expect("begin operation transaction");
    let operation = match begin_personal_operation(
        &mut tx,
        USER_ID,
        "expired-session-recovery-key",
        BillingOffer::StandardMonthly,
        1,
        4_000_000_000,
        "https://app.sotto.test/billing",
        "https://app.sotto.test/billing",
    )
    .await
    .expect("create operation")
    {
        BeginOperation::Created(operation) => operation,
        BeginOperation::AlreadyExists(_) => panic!("fixture operation unexpectedly existed"),
    };
    sotto_server::personal_billing::begin_account(
        &mut tx,
        USER_ID,
        &operation.operation_id,
        BillingOffer::StandardMonthly,
        4_000_000_100,
        4_000_000_000,
    )
    .await
    .expect("persist personal account");
    record_provider_result(
        &mut tx,
        &operation.operation_id,
        BillingOperationState::Succeeded,
        None,
        Some("checkout_created"),
    )
    .await
    .expect("persist pre-migration checkout result");
    // Older releases stored only the Checkout URL, not an explicit session id or request facts.
    sqlx::query("UPDATE billing_operations SET provider_checkout_url = $2 WHERE operation_id = $1")
        .bind(&operation.operation_id)
        .bind("https://checkout.stripe.com/c/pay/cs_legacy_expired")
        .execute(&mut *tx)
        .await
        .expect("persist legacy checkout URL containing the session id");
    sqlx::query(
        "UPDATE billing_personal_accounts SET pending_expires_at_epoch = \
         extract(epoch FROM now())::bigint - 60 WHERE operation_id = $1",
    )
    .bind(&operation.operation_id)
    .execute(&mut *tx)
    .await
    .expect("expire account locally");
    tx.commit().await.expect("commit operation");

    let provider = RecoveryCheckoutProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        received: Arc::new(std::sync::Mutex::new(Vec::new())),
        status_requests: Arc::new(std::sync::Mutex::new(Vec::new())),
        lose_first_response: false,
    };
    let billing_state = BillingState::with_provider(
        Arc::new(provider.clone()),
        "whsec_test".into(),
        "https://app.sotto.test".into(),
    );

    assert!(
        billing::recover_personal_billing_operation_once(&pool, &billing_state)
            .await
            .expect("reconcile provider expiry for old account")
    );
    let account_state: String =
        sqlx::query_scalar("SELECT state FROM billing_personal_accounts WHERE operation_id = $1")
            .bind(&operation.operation_id)
            .fetch_one(&pool)
            .await
            .expect("read released account");
    assert_eq!(account_state, "canceled");
    let operation_result: (String, String, Option<String>) = sqlx::query_as(
        "SELECT state, result_code, provider_checkout_session_id \
         FROM billing_operations WHERE operation_id = $1",
    )
    .bind(&operation.operation_id)
    .fetch_one(&pool)
    .await
    .expect("read expired operation");
    assert_eq!(operation_result.0, "failed");
    assert_eq!(operation_result.1, "checkout_expired");
    assert_eq!(operation_result.2.as_deref(), Some("cs_legacy_expired"));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        provider.status_requests.lock().unwrap().as_slice(),
        &[("cs_legacy_expired".into(), operation.operation_id.clone())]
    );
    cleanup(&pool, USER_ID).await;
}

#[tokio::test]
async fn verified_paid_invoice_resolves_recovery_lease_and_manual_review() {
    let _guard = test_lock().lock().await;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    for (user_id, key, operation_state, lease_token) in [
        (
            "billing-operation-test-paid-recovery-lease",
            "paid-recovery-lease-key",
            "pending",
            Some("billing-recovery:paid-race"),
        ),
        (
            "billing-operation-test-paid-manual-review",
            "paid-manual-review-key",
            "manual_review",
            None,
        ),
    ] {
        cleanup(&pool, user_id).await;
        seed_user(&pool, user_id).await;
        let mut tx = pool.begin().await.expect("begin operation transaction");
        let operation = match begin_personal_operation(
            &mut tx,
            user_id,
            key,
            BillingOffer::StandardMonthly,
            1,
            4_000_000_000,
            "https://app.sotto.test/billing",
            "https://app.sotto.test/billing",
        )
        .await
        .expect("create operation")
        {
            BeginOperation::Created(operation) => operation,
            BeginOperation::AlreadyExists(_) => panic!("fixture operation unexpectedly existed"),
        };
        sotto_server::personal_billing::begin_account(
            &mut tx,
            user_id,
            &operation.operation_id,
            BillingOffer::StandardMonthly,
            4_000_000_100,
            4_000_000_000,
        )
        .await
        .expect("persist personal account");
        let result_code = (operation_state == "manual_review").then_some("manual_review_required");
        sqlx::query(
            "UPDATE billing_operations SET state=$2, result_code=$3, \
             reconciliation_manual_review_required=$4, reconciliation_lease_token=$5, \
             reconciliation_lease_until=CASE WHEN $5 IS NULL THEN NULL ELSE now()+interval '5 minutes' END \
             WHERE operation_id=$1",
        )
        .bind(&operation.operation_id)
        .bind(operation_state)
        .bind(result_code)
        .bind(operation_state == "manual_review")
        .bind(lease_token)
        .execute(&mut *tx)
        .await
        .expect("seed payment race state");
        tx.commit().await.expect("commit operation");

        let mut paid_tx = pool
            .begin()
            .await
            .expect("begin verified payment transaction");
        let settled = sotto_server::billing_operations::record_verified_paid_invoice(
            &mut paid_tx,
            &operation.operation_id,
        )
        .await
        .expect("verified invoice settles independently of recovery state");
        assert_eq!(settled.state, BillingOperationState::Succeeded);
        assert_eq!(
            settled.result_code.as_deref(),
            Some("source_restoration_paid")
        );
        paid_tx.commit().await.expect("commit verified payment");
        let recovery_state: (bool, Option<String>) = sqlx::query_as(
            "SELECT reconciliation_manual_review_required, reconciliation_lease_token \
             FROM billing_operations WHERE operation_id=$1",
        )
        .bind(&operation.operation_id)
        .fetch_one(&pool)
        .await
        .expect("read resolved recovery state");
        assert_eq!(recovery_state, (false, None));
        cleanup(&pool, user_id).await;
    }
}

#[tokio::test]
async fn customer_retry_parks_checkout_after_provider_idempotency_window() {
    let _guard = test_lock().lock().await;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    const USER_ID: &str = "billing-operation-test-checkout-window";
    cleanup(&pool, USER_ID).await;
    seed_user(&pool, USER_ID).await;
    let mut tx = pool.begin().await.expect("begin operation transaction");
    let operation = match begin_personal_operation(
        &mut tx,
        USER_ID,
        "checkout-window-key",
        BillingOffer::StandardMonthly,
        1,
        4_000_000_000,
        "https://app.sotto.test/billing",
        "https://app.sotto.test/billing",
    )
    .await
    .expect("create operation")
    {
        BeginOperation::Created(operation) => operation,
        BeginOperation::AlreadyExists(_) => panic!("fixture operation unexpectedly existed"),
    };
    sotto_server::personal_billing::begin_account(
        &mut tx,
        USER_ID,
        &operation.operation_id,
        BillingOffer::StandardMonthly,
        4_000_000_100,
        4_000_000_000,
    )
    .await
    .expect("persist personal account");
    tx.commit().await.expect("commit operation");
    sqlx::query(
        "UPDATE billing_operations SET created_at = now() - interval '24 hours' \
         WHERE operation_id = $1",
    )
    .bind(&operation.operation_id)
    .execute(&pool)
    .await
    .expect("age operation past the safe retry window");

    assert!(
        sotto_server::billing_operations::park_personal_checkout_outside_idempotency_window(
            &pool,
            &operation.operation_id
        )
        .await
        .expect("park stale customer retry")
    );
    let state: String =
        sqlx::query_scalar("SELECT state FROM billing_operations WHERE operation_id = $1")
            .bind(&operation.operation_id)
            .fetch_one(&pool)
            .await
            .expect("read review state");
    assert_eq!(state, "manual_review");

    cleanup(&pool, USER_ID).await;
}

#[tokio::test]
async fn signed_expiry_fences_a_late_recovery_result_before_account_reuse() {
    let _guard = test_lock().lock().await;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    const USER_ID: &str = "billing-operation-test-expiry-race";
    cleanup(&pool, USER_ID).await;
    seed_user(&pool, USER_ID).await;
    let mut tx = pool.begin().await.expect("begin checkout transaction");
    let operation = match begin_personal_operation(
        &mut tx,
        USER_ID,
        "checkout-expiry-race-key",
        BillingOffer::StandardMonthly,
        1,
        4_000_000_000,
        "https://app.sotto.test/billing",
        "https://app.sotto.test/billing",
    )
    .await
    .expect("create checkout operation")
    {
        BeginOperation::Created(operation) => operation,
        BeginOperation::AlreadyExists(_) => panic!("fixture operation unexpectedly existed"),
    };
    sotto_server::personal_billing::begin_account(
        &mut tx,
        USER_ID,
        &operation.operation_id,
        BillingOffer::StandardMonthly,
        4_000_000_100,
        4_000_000_000,
    )
    .await
    .expect("persist personal account");
    record_personal_checkout_request(
        &mut tx,
        &operation.operation_id,
        "price_original_monthly",
        "https://app.sotto.test/app?billing=success",
        "https://app.sotto.test/app?billing=cancelled",
    )
    .await
    .expect("persist exact provider request");
    tx.commit().await.expect("commit checkout operation");
    sqlx::query(
        "UPDATE billing_operations SET reconciliation_lease_until = now() - interval '1 second' \
         WHERE operation_id = $1",
    )
    .bind(&operation.operation_id)
    .execute(&pool)
    .await
    .expect("expire creator lease");
    let worker_token = "billing-recovery:expiry-race-worker";
    claim_reconciliation(&pool, &operation.operation_id, worker_token)
        .await
        .expect("claim the pending operation");

    let mut expiry_tx = pool.begin().await.expect("begin signed expiry");
    let expired = sotto_server::billing_operations::record_checkout_expired(
        &mut expiry_tx,
        &operation.operation_id,
        "cs_expiry_race",
    )
    .await
    .expect("apply signed provider expiry");
    sqlx::query(
        "UPDATE billing_personal_accounts SET state = 'canceled' \
         WHERE operation_id = $1 AND state = 'pending'",
    )
    .bind(&operation.operation_id)
    .execute(&mut *expiry_tx)
    .await
    .expect("release the expired personal account");
    expiry_tx.commit().await.expect("commit signed expiry");
    assert_eq!(expired.state, BillingOperationState::Failed);

    let mut late_result_tx = pool.begin().await.expect("begin late result");
    let late_result = sotto_server::billing_operations::record_claimed_checkout_result(
        &mut late_result_tx,
        &operation.operation_id,
        worker_token,
        "cs_expiry_race",
        "https://checkout.example.test/cs_expiry_race",
    )
    .await;
    assert!(matches!(late_result, Err(BillingRecoveryError::InProgress)));
    late_result_tx
        .rollback()
        .await
        .expect("discard stale result");

    let mut reuse_tx = pool.begin().await.expect("begin new checkout");
    let replacement = match begin_personal_operation(
        &mut reuse_tx,
        USER_ID,
        "checkout-after-expiry-key",
        BillingOffer::StandardMonthly,
        1,
        4_000_000_100,
        "https://app.sotto.test/billing",
        "https://app.sotto.test/billing",
    )
    .await
    .expect("create replacement operation")
    {
        BeginOperation::Created(operation) => operation,
        BeginOperation::AlreadyExists(_) => panic!("replacement operation unexpectedly existed"),
    };
    sotto_server::personal_billing::begin_account(
        &mut reuse_tx,
        USER_ID,
        &replacement.operation_id,
        BillingOffer::StandardMonthly,
        4_000_000_200,
        4_000_000_100,
    )
    .await
    .expect("reuse the account after signed expiry");
    reuse_tx
        .commit()
        .await
        .expect("commit replacement checkout");

    let account_operation: String =
        sqlx::query_scalar("SELECT operation_id FROM billing_personal_accounts WHERE user_id = $1")
            .bind(USER_ID)
            .fetch_one(&pool)
            .await
            .expect("read replacement account binding");
    assert_eq!(account_operation, replacement.operation_id);
    cleanup(&pool, USER_ID).await;
}

#[tokio::test]
async fn recovery_failure_budget_counts_failures_and_parks_at_twenty() {
    let _guard = test_lock().lock().await;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    const USER_ID: &str = "billing-operation-test-failure-budget";
    cleanup(&pool, USER_ID).await;
    seed_user(&pool, USER_ID).await;
    let mut tx = pool.begin().await.expect("begin operation transaction");
    let operation_id = match begin_personal_operation(
        &mut tx,
        USER_ID,
        "failure-budget-key",
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
    let mut tx = pool.begin().await.expect("begin unknown-state transaction");
    record_provider_result(
        &mut tx,
        &operation_id,
        BillingOperationState::Unknown,
        None,
        Some("provider_timeout"),
    )
    .await
    .expect("record unknown outcome");
    tx.commit().await.expect("commit unknown outcome");

    let first_claim_token = "recovery-failure-test:first";
    claim_reconciliation(&pool, &operation_id, first_claim_token)
        .await
        .expect("claim for one recovery attempt");
    let attempts: i32 = sqlx::query_scalar(
        "SELECT reconciliation_attempt_count FROM billing_operations WHERE operation_id = $1",
    )
    .bind(&operation_id)
    .fetch_one(&pool)
    .await
    .expect("read attempts after claim");
    assert_eq!(attempts, 0, "a lease claim is not a provider failure");
    assert!(!release_reconciliation_failure(
        &pool,
        &operation_id,
        first_claim_token,
        "provider_outcome_uncertain",
    )
    .await
    .expect("record first failure"));

    for attempt in 1..20 {
        sqlx::query(
            "UPDATE billing_operations SET reconciliation_next_attempt_at = now() \
             WHERE operation_id = $1",
        )
        .bind(&operation_id)
        .execute(&pool)
        .await
        .expect("make test retry due");
        let token = format!("recovery-failure-test:{attempt}");
        claim_reconciliation(&pool, &operation_id, &token)
            .await
            .expect("claim due retry");
        let parked = release_reconciliation_failure(
            &pool,
            &operation_id,
            &token,
            "provider_outcome_uncertain",
        )
        .await
        .expect("record retry failure");
        assert_eq!(parked, attempt == 19);
    }
    let final_row = sqlx::query(
        "SELECT state, reconciliation_attempt_count, reconciliation_manual_review_required \
         FROM billing_operations WHERE operation_id = $1",
    )
    .bind(&operation_id)
    .fetch_one(&pool)
    .await
    .expect("read parked operation");
    assert_eq!(
        final_row.try_get::<String, _>("state").unwrap(),
        "manual_review"
    );
    assert_eq!(
        final_row
            .try_get::<i32, _>("reconciliation_attempt_count")
            .unwrap(),
        20
    );
    assert!(final_row
        .try_get::<bool, _>("reconciliation_manual_review_required")
        .unwrap());
    assert!(!load_reconciliation_candidates(&pool, 10)
        .await
        .expect("manual review is excluded from automatic work")
        .iter()
        .any(|operation| operation.operation_id == operation_id));

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
    let _guard = test_lock().lock().await;
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
    let _guard = test_lock().lock().await;
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
    let _guard = test_lock().lock().await;
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
