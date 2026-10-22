//! Database coverage for named sponsor seat intents.
//!
//! These tests are opt-in like the other PostgreSQL integration targets. They use unique fixture
//! identities and remove the operation rows explicitly because their foreign keys preserve history.

use sqlx::postgres::PgConnectOptions;
use sqlx::PgPool;
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::Notify;
use uuid::Uuid;

static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

use async_trait::async_trait;
use sotto_server::billing::{
    recover_sponsored_billing_operation_by_id, recover_sponsored_billing_operation_once,
    BillingState, ProviderError, ProviderErrorKind, ProviderResult, SponsoredCheckoutSession,
    SponsoredSubscriptionItemRequest, SubscriptionObservation, SubscriptionProvider,
};
use sotto_server::billing_catalogue::{BillingOffer, BillingPriceIds};
use sotto_server::db;
use sotto_server::sponsored_billing::{
    begin_operation, cancel_claimed_expired_checkout, cancel_claimed_rejected_operation,
    cancel_failed_operation, claim_recovery, complete_paid_checkout, next_recovery_candidate,
    persist_provider_request, record_checkout, record_founding_refund_required,
    record_provider_update, record_recovery_failure, SponsoredBillingError,
    SponsoredProviderEvidence, SponsoredProviderRequest, SponsoredSeatAction, SponsoredSeatRequest,
};

struct LostResponseProvider {
    customer_id: String,
    session_id: String,
    calls: AtomicUsize,
    remote_creations: AtomicUsize,
    accepted_sessions: Mutex<std::collections::HashMap<String, SponsoredCheckoutSession>>,
    idempotency_keys: Mutex<Vec<String>>,
    first_call_started: Notify,
    release_first_call: Notify,
}

struct AppliedScheduleProvider {
    subscription_id: String,
    customer_id: String,
    schedule_id: String,
    period_start: i64,
    period_end: i64,
    applied_quantity: i64,
    verification_calls: AtomicUsize,
    update_calls: AtomicUsize,
}

impl LostResponseProvider {
    fn new(customer_id: String, session_id: String) -> Self {
        Self {
            customer_id,
            session_id,
            calls: AtomicUsize::new(0),
            remote_creations: AtomicUsize::new(0),
            accepted_sessions: Mutex::new(std::collections::HashMap::new()),
            idempotency_keys: Mutex::new(Vec::new()),
            first_call_started: Notify::new(),
            release_first_call: Notify::new(),
        }
    }
}

#[async_trait]
impl SubscriptionProvider for LostResponseProvider {
    async fn create_checkout(
        &self,
        _org_id: &str,
        _customer: Option<&str>,
        _success_url: &str,
        _cancel_url: &str,
    ) -> ProviderResult<String> {
        Err(unsupported_provider_call())
    }

    async fn create_portal(
        &self,
        _customer: &str,
        _return_url: &str,
        _configuration_id: Option<&str>,
    ) -> ProviderResult<String> {
        Err(unsupported_provider_call())
    }

    async fn get_subscription(
        &self,
        _subscription_id: &str,
    ) -> ProviderResult<SubscriptionObservation> {
        Err(unsupported_provider_call())
    }

    async fn cancel_subscription(
        &self,
        _subscription_id: &str,
        _idempotency_key: &str,
        _org_id: &str,
    ) -> ProviderResult<SubscriptionObservation> {
        Err(unsupported_provider_call())
    }

    async fn create_sponsored_checkout(
        &self,
        _organization_id: &str,
        _customer: Option<&str>,
        _items: &[SponsoredSubscriptionItemRequest],
        _operation_id: &str,
        idempotency_key: &str,
        _expires_at_epoch: i64,
        _success_url: &str,
        _cancel_url: &str,
    ) -> ProviderResult<SponsoredCheckoutSession> {
        let accepted = SponsoredCheckoutSession {
            session_id: self.session_id.clone(),
            checkout_url: format!("https://checkout.stripe.test/{}", self.session_id),
            customer_id: Some(self.customer_id.clone()),
            subscription_id: None,
        };
        let previous = self
            .accepted_sessions
            .lock()
            .expect("accepted provider session lock")
            .get(idempotency_key)
            .cloned();
        if let Some(session) = previous {
            self.idempotency_keys
                .lock()
                .expect("idempotency key lock")
                .push(idempotency_key.to_owned());
            return Ok(session.clone());
        }
        self.idempotency_keys
            .lock()
            .expect("idempotency key lock")
            .push(idempotency_key.to_owned());
        match self.calls.fetch_add(1, Ordering::SeqCst) {
            0 => {
                self.accepted_sessions
                    .lock()
                    .expect("accepted provider session lock")
                    .insert(idempotency_key.to_owned(), accepted);
                self.remote_creations.fetch_add(1, Ordering::SeqCst);
                self.first_call_started.notify_one();
                self.release_first_call.notified().await;
                Err(ProviderError {
                    status: None,
                    code: Some("transport_error".into()),
                    kind: ProviderErrorKind::Retryable,
                })
            }
            _ => {
                // The simulated provider has accepted the first request. Returning its stored
                // result for the same key proves the retry does not create a second Checkout.
                self.accepted_sessions
                    .lock()
                    .expect("accepted provider session lock")
                    .insert(idempotency_key.to_owned(), accepted.clone());
                self.remote_creations.fetch_add(1, Ordering::SeqCst);
                Ok(accepted)
            }
        }
    }
}

#[async_trait]
impl SubscriptionProvider for AppliedScheduleProvider {
    async fn create_checkout(
        &self,
        _org_id: &str,
        _customer: Option<&str>,
        _success_url: &str,
        _cancel_url: &str,
    ) -> ProviderResult<String> {
        Err(unsupported_provider_call())
    }

    async fn create_portal(
        &self,
        _customer: &str,
        _return_url: &str,
        _configuration_id: Option<&str>,
    ) -> ProviderResult<String> {
        Err(unsupported_provider_call())
    }

    async fn get_subscription(
        &self,
        _subscription_id: &str,
    ) -> ProviderResult<SubscriptionObservation> {
        Err(unsupported_provider_call())
    }

    async fn cancel_subscription(
        &self,
        _subscription_id: &str,
        _idempotency_key: &str,
        _org_id: &str,
    ) -> ProviderResult<SubscriptionObservation> {
        Err(unsupported_provider_call())
    }

    async fn get_sponsored_subscription(
        &self,
        subscription_id: &str,
    ) -> ProviderResult<sotto_server::billing::SponsoredSubscriptionSnapshot> {
        assert_eq!(subscription_id, self.subscription_id);
        Ok(self.snapshot())
    }

    async fn verify_sponsored_subscription_update(
        &self,
        subscription_id: &str,
        expected_period_start: i64,
        expected_period_end: i64,
        phases: &[sotto_server::billing::SponsoredSubscriptionPhase],
    ) -> ProviderResult<Option<sotto_server::billing::SponsoredSubscriptionSnapshot>> {
        self.verification_calls.fetch_add(1, Ordering::SeqCst);
        let phase_at_end = phases
            .iter()
            .find(|phase| {
                phase.start_at_epoch <= self.period_end
                    && phase.end_at_epoch.is_none_or(|end| self.period_end < end)
            })
            .or_else(|| {
                phases
                    .iter()
                    .find(|phase| phase.start_at_epoch == self.period_end)
            });
        let normalized_phases = phase_at_end.map(|phase| {
            let mut normalized = vec![sotto_server::billing::SponsoredSubscriptionPhase {
                start_at_epoch: self.period_end,
                end_at_epoch: phase.end_at_epoch,
                items: phase.items.clone(),
            }];
            normalized.extend(
                phases
                    .iter()
                    .filter(|later| later.start_at_epoch > self.period_end)
                    .cloned(),
            );
            if normalized
                .last()
                .is_some_and(|phase| phase.items.is_empty())
            {
                normalized.pop();
            }
            normalized
        });
        let expected_phases = [sotto_server::billing::SponsoredSubscriptionPhase {
            start_at_epoch: self.period_end,
            end_at_epoch: None,
            items: vec![SponsoredSubscriptionItemRequest {
                provider_price_id: "price_recovery_test".into(),
                quantity: self.applied_quantity,
            }],
        }];
        if subscription_id != self.subscription_id
            || expected_period_start != self.period_start
            || expected_period_end != self.period_end
            || normalized_phases.as_deref() != Some(expected_phases.as_slice())
        {
            return Ok(None);
        }
        Ok(Some(self.snapshot()))
    }

    async fn update_sponsored_subscription(
        &self,
        _organization_id: &str,
        _subscription_id: &str,
        _phases: &[sotto_server::billing::SponsoredSubscriptionPhase],
        _idempotency_key: &str,
    ) -> ProviderResult<sotto_server::billing::SponsoredSubscriptionSnapshot> {
        self.update_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.snapshot())
    }
}

impl AppliedScheduleProvider {
    fn snapshot(&self) -> sotto_server::billing::SponsoredSubscriptionSnapshot {
        sotto_server::billing::SponsoredSubscriptionSnapshot {
            customer_id: Some(self.customer_id.clone()),
            default_payment_method_id: None,
            subscription_id: self.subscription_id.clone(),
            status: sotto_server::billing::SubscriptionStatus::Active,
            cancel_at_period_end: false,
            cancel_at: None,
            current_period_start: Some(self.period_start),
            current_period_end: Some(self.period_end),
            schedule_id: Some(self.schedule_id.clone()),
            items: vec![],
        }
    }
}

fn unsupported_provider_call() -> ProviderError {
    ProviderError {
        status: None,
        code: Some("unexpected_provider_call".into()),
        kind: ProviderErrorKind::Unknown,
    }
}

struct RecoveryFixture {
    organization_id: String,
    owner_id: String,
    beneficiary_id: String,
    operation_id: String,
    customer_id: String,
    session_id: String,
    subscription_id: String,
    schedule_id: String,
    period_start: i64,
    period_end: i64,
}

async fn create_recovery_fixture(
    pool: &PgPool,
    suffix: &str,
    schedule_update: bool,
) -> RecoveryFixture {
    let organization_id = format!("sponsored-recovery-org-{suffix}");
    let owner_id = format!("sponsored-recovery-owner-{suffix}");
    let beneficiary_id = format!("sponsored-recovery-beneficiary-{suffix}");
    let customer_id = format!("cus_recovery_{suffix}");
    let session_id = format!("cs_recovery_{suffix}");
    let subscription_id = format!("sub_recovery_{suffix}");
    let schedule_id = format!("sched_recovery_{suffix}");
    sqlx::query(
        "INSERT INTO users (id, oauth_provider, oauth_subject) VALUES \
         ($1, 'sponsored-recovery-test', $1), ($2, 'sponsored-recovery-test', $2)",
    )
    .bind(&owner_id)
    .bind(&beneficiary_id)
    .execute(pool)
    .await
    .expect("insert recovery users");
    sqlx::query(
        "INSERT INTO organizations (id, enc_name, created_by) \
         VALUES ($1, decode('6f7267', 'hex'), $2)",
    )
    .bind(&organization_id)
    .bind(&owner_id)
    .execute(pool)
    .await
    .expect("insert recovery organisation");
    sqlx::query(
        "INSERT INTO organization_memberships (org_id, user_id, role) \
         VALUES ($1, $2, 'owner')",
    )
    .bind(&organization_id)
    .bind(&owner_id)
    .execute(pool)
    .await
    .expect("insert recovery owner membership");

    let now = sotto_server::sponsored_billing::current_epoch();
    let request = SponsoredSeatRequest {
        action: SponsoredSeatAction::Add,
        beneficiary_id: beneficiary_id.clone(),
        replacement_beneficiary_id: None,
        offer: BillingOffer::StandardMonthly,
        quote_version: 1,
        quote_expires_at_epoch: now + 600,
        effective_from: now + 600,
        effective_until: None,
        idempotency_key: format!("recovery-{suffix}"),
    };
    let mut tx = pool
        .begin()
        .await
        .expect("begin sponsored recovery operation");
    let operation = begin_operation(&mut tx, &organization_id, &owner_id, &request, now)
        .await
        .expect("create sponsored recovery operation");
    let period_start = now - 3_600;
    let period_end = now + 86_400;
    let provider_request = if schedule_update {
        sqlx::query(
            "UPDATE billing_sponsored_operations SET provider_subscription_id = $2 WHERE operation_id = $1",
        )
        .bind(&operation.operation_id)
        .bind(&subscription_id)
        .execute(&mut *tx)
        .await
        .expect("bind schedule mutation to existing subscription");
        SponsoredProviderRequest::ScheduleUpdate {
            organization_id: organization_id.clone(),
            subscription_id: subscription_id.clone(),
            expected_period_start: period_start,
            expected_period_end: period_end,
            phases: vec![sotto_server::billing::SponsoredSubscriptionPhase {
                start_at_epoch: period_end,
                end_at_epoch: None,
                items: vec![SponsoredSubscriptionItemRequest {
                    provider_price_id: "price_recovery_test".into(),
                    quantity: 1,
                }],
            }],
        }
    } else {
        SponsoredProviderRequest::Checkout {
            organization_id: organization_id.clone(),
            customer_id: Some(customer_id.clone()),
            items: vec![SponsoredSubscriptionItemRequest {
                provider_price_id: "price_recovery_test".into(),
                quantity: 1,
            }],
            expires_at_epoch: now + 3_600,
            success_url: "https://example.test/success".into(),
            cancel_url: "https://example.test/cancel".into(),
        }
    };
    persist_provider_request(&mut tx, &operation.operation_id, &provider_request)
        .await
        .expect("persist immutable provider request");
    tx.commit()
        .await
        .expect("commit sponsored recovery operation");

    RecoveryFixture {
        organization_id,
        owner_id,
        beneficiary_id,
        operation_id: operation.operation_id,
        customer_id,
        session_id,
        subscription_id,
        schedule_id,
        period_start,
        period_end,
    }
}

async fn clean_recovery_fixture(pool: &PgPool, fixture: &RecoveryFixture) {
    sqlx::query("DELETE FROM billing_sponsored_subscription_items WHERE organization_id = $1")
        .bind(&fixture.organization_id)
        .execute(pool)
        .await
        .expect("delete recovery subscription items");
    sqlx::query("DELETE FROM billing_sponsored_subscriptions WHERE organization_id = $1")
        .bind(&fixture.organization_id)
        .execute(pool)
        .await
        .expect("delete recovery subscriptions");
    sqlx::query("DELETE FROM billing_sponsored_seats WHERE organization_id = $1")
        .bind(&fixture.organization_id)
        .execute(pool)
        .await
        .expect("delete recovery seats");
    sqlx::query("DELETE FROM billing_sponsored_operations WHERE organization_id = $1")
        .bind(&fixture.organization_id)
        .execute(pool)
        .await
        .expect("delete recovery operations");
    sqlx::query("DELETE FROM organization_memberships WHERE org_id = $1")
        .bind(&fixture.organization_id)
        .execute(pool)
        .await
        .expect("delete recovery membership");
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(&fixture.organization_id)
        .execute(pool)
        .await
        .expect("delete recovery organisation");
    sqlx::query("DELETE FROM users WHERE id IN ($1, $2)")
        .bind(&fixture.owner_id)
        .bind(&fixture.beneficiary_id)
        .execute(pool)
        .await
        .expect("delete recovery users");
}

async fn pool() -> Option<PgPool> {
    if std::env::var("SOTTO_RUN_DB_TESTS").as_deref() != Ok("1") {
        eprintln!("skipping sponsored billing tests: SOTTO_RUN_DB_TESTS=1 not set");
        return None;
    }
    let url = match std::env::var("DATABASE_URL") {
        Ok(url) => url,
        Err(_) => {
            eprintln!("skipping sponsored billing tests: DATABASE_URL not set");
            return None;
        }
    };
    let options = PgConnectOptions::from_str(&url).expect("parse DATABASE_URL");
    assert!(
        matches!(options.get_host(), "localhost" | "127.0.0.1" | "::1"),
        "refusing sponsored billing tests against non-local host: {}",
        options.get_host()
    );
    let pool = db::connect(&url).await.expect("connect");
    db::migrate(&pool).await.expect("migrate");
    Some(pool)
}

#[tokio::test]
async fn named_seat_operation_is_idempotent_and_activates_after_paid_result() {
    let _guard = TEST_LOCK.lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    let suffix = Uuid::new_v4().to_string();
    let org_id = format!("sponsored-org-{suffix}");
    let owner = format!("sponsored-owner-{suffix}");
    let beneficiary = format!("sponsored-beneficiary-{suffix}");
    sqlx::query(
        "INSERT INTO users (id, oauth_provider, oauth_subject) VALUES \
         ($1, 'sponsored-test', $1), ($2, 'sponsored-test', $2)",
    )
    .bind(&owner)
    .bind(&beneficiary)
    .execute(&pool)
    .await
    .expect("insert users");
    sqlx::query(
        "INSERT INTO organizations (id, enc_name, created_by) \
         VALUES ($1, decode('6f7267', 'hex'), $2)",
    )
    .bind(&org_id)
    .bind(&owner)
    .execute(&pool)
    .await
    .expect("insert organisation");
    sqlx::query(
        "INSERT INTO organization_memberships (org_id, user_id, role) VALUES ($1, $2, 'owner')",
    )
    .bind(&org_id)
    .bind(&owner)
    .execute(&pool)
    .await
    .expect("insert owner membership");

    let request = SponsoredSeatRequest {
        action: SponsoredSeatAction::Add,
        beneficiary_id: beneficiary.clone(),
        replacement_beneficiary_id: None,
        offer: BillingOffer::StandardMonthly,
        quote_version: 1,
        quote_expires_at_epoch: 4_000_000_000,
        effective_from: 1_700_000_001,
        effective_until: None,
        idempotency_key: "seat-add-1".into(),
    };
    let mut tx = pool.begin().await.expect("begin operation transaction");
    let first = begin_operation(&mut tx, &org_id, &owner, &request, 1_700_000_001)
        .await
        .expect("create operation");
    tx.commit().await.expect("commit operation");

    let mut tx = pool.begin().await.expect("begin replay transaction");
    let replay = begin_operation(&mut tx, &org_id, &owner, &request, 1_700_000_001)
        .await
        .expect("replay operation");
    tx.commit().await.expect("commit replay");
    assert_eq!(first.operation_id, replay.operation_id);

    let mut tx = pool.begin().await.expect("begin late replay transaction");
    let late_replay = begin_operation(&mut tx, &org_id, &owner, &request, 5_000_000_000)
        .await
        .expect("late replay keeps the original operation recoverable");
    tx.commit().await.expect("commit late replay");
    assert_eq!(first.operation_id, late_replay.operation_id);

    let mut tx = pool.begin().await.expect("begin checkout transaction");
    record_checkout(
        &mut tx,
        &first.operation_id,
        "https://stripe.test/checkout",
        "cs_sponsored_test",
        Some("cus_sponsored_test"),
        Some("sub_sponsored_test"),
    )
    .await
    .expect("record checkout");
    tx.commit().await.expect("commit checkout");

    let mut tx = pool.begin().await.expect("begin settlement transaction");
    let evidence = SponsoredProviderEvidence {
        customer_id: Some("cus_sponsored_test".into()),
        subscription_id: "sub_sponsored_test".into(),
        schedule_id: None,
        checkout_session_id: Some("cs_sponsored_test".into()),
        payment_reference: "pi_sponsored_test".into(),
        provider_item_id: Some("si_sponsored_test".into()),
    };
    sqlx::query(
        "UPDATE billing_sponsored_operations SET state = 'manual_review', \
         result_code = 'provider_idempotency_window_elapsed', \
         recovery_manual_review_required = TRUE, \
         recovery_last_error_code = 'provider_idempotency_window_elapsed' \
         WHERE operation_id = $1",
    )
    .bind(&first.operation_id)
    .execute(&mut *tx)
    .await
    .expect("park an unresolved provider result for operator review");
    let settled = complete_paid_checkout(&mut tx, &first.operation_id, &evidence)
        .await
        .expect("verified payment settles even while recovery is parked");
    tx.commit().await.expect("commit settlement");
    assert_eq!(settled.state, "active");
    assert!(!settled.recovery_manual_review_required);
    let state: String =
        sqlx::query_scalar("SELECT state FROM billing_sponsored_seats WHERE operation_id = $1")
            .bind(&first.operation_id)
            .fetch_one(&pool)
            .await
            .expect("load seat state");
    assert_eq!(state, "active");
    let provider_item: Option<String> = sqlx::query_scalar(
        "SELECT provider_item_id FROM billing_sponsored_seats WHERE operation_id = $1",
    )
    .bind(&first.operation_id)
    .fetch_one(&pool)
    .await
    .expect("load provider item");
    assert_eq!(provider_item.as_deref(), Some("si_sponsored_test"));
    let stored_provider_ids: (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT provider_customer_id, provider_subscription_id \
         FROM billing_sponsored_subscriptions WHERE organization_id = $1",
    )
    .bind(&org_id)
    .fetch_one(&pool)
    .await
    .expect("load sponsored subscription");
    assert_eq!(stored_provider_ids.0.as_deref(), Some("cus_sponsored_test"));
    assert_eq!(stored_provider_ids.1.as_deref(), Some("sub_sponsored_test"));

    let removal_request = SponsoredSeatRequest {
        action: SponsoredSeatAction::Remove,
        beneficiary_id: beneficiary.clone(),
        replacement_beneficiary_id: None,
        offer: BillingOffer::StandardMonthly,
        quote_version: 1,
        quote_expires_at_epoch: 4_000_000_000,
        effective_from: 1_800_000_000,
        effective_until: Some(1_800_000_000 + 1),
        idempotency_key: "seat-remove-1".into(),
    };
    let mut tx = pool.begin().await.expect("begin removal transaction");
    let removal = begin_operation(&mut tx, &org_id, &owner, &removal_request, 1_700_000_001)
        .await
        .expect("create removal operation");
    tx.commit().await.expect("commit removal operation");
    let mut tx = pool.begin().await.expect("begin provider update");
    record_provider_update(
        &mut tx,
        &removal.operation_id,
        Some("cus_sponsored_test"),
        "sub_sponsored_test",
        Some("sch_sponsored_test"),
        Some(1_700_000_100),
        None,
    )
    .await
    .expect("record provider update");
    tx.commit().await.expect("commit provider update");
    let removal_state: (String, Option<i64>) = sqlx::query_as(
        "SELECT state, effective_until FROM billing_sponsored_seats WHERE operation_id = $1",
    )
    .bind(&first.operation_id)
    .fetch_one(&pool)
    .await
    .expect("load scheduled removal");
    assert_eq!(removal_state.0, "scheduled_removal");
    assert_eq!(removal_state.1, Some(1_800_000_001));
    let schedule_id: Option<String> = sqlx::query_scalar(
        "SELECT provider_schedule_id FROM billing_sponsored_subscriptions WHERE organization_id = $1",
    )
    .bind(&org_id)
    .fetch_one(&pool)
    .await
    .expect("load stored schedule");
    assert_eq!(schedule_id.as_deref(), Some("sch_sponsored_test"));

    let mut tx = pool.begin().await.expect("begin conflicting settlement");
    let mut conflicting_evidence = evidence.clone();
    conflicting_evidence.payment_reference = "pi_other".into();
    let conflict = complete_paid_checkout(&mut tx, &first.operation_id, &conflicting_evidence)
        .await
        .expect_err("a different provider result must conflict");
    assert!(matches!(conflict, SponsoredBillingError::ResultConflict));
    tx.rollback()
        .await
        .expect("rollback conflicting settlement");

    sqlx::query("UPDATE organizations SET lifecycle_state = 'deleting' WHERE id = $1")
        .bind(&org_id)
        .execute(&pool)
        .await
        .expect("mark organisation deleting");
    let mut tx = pool.begin().await.expect("begin deleting settlement");
    let deletion_error = complete_paid_checkout(&mut tx, &first.operation_id, &evidence)
        .await
        .expect_err("a deleting organisation cannot settle another webhook");
    assert!(matches!(
        deletion_error,
        SponsoredBillingError::OrganisationNotActive
    ));
    tx.rollback().await.expect("rollback deleting settlement");
    sqlx::query("UPDATE organizations SET lifecycle_state = 'active' WHERE id = $1")
        .bind(&org_id)
        .execute(&pool)
        .await
        .expect("restore organisation lifecycle");

    sqlx::query("DELETE FROM billing_sponsored_seats WHERE organization_id = $1")
        .bind(&org_id)
        .execute(&pool)
        .await
        .expect("delete seats");
    sqlx::query("DELETE FROM billing_sponsored_operations WHERE organization_id = $1")
        .bind(&org_id)
        .execute(&pool)
        .await
        .expect("delete operations");
    sqlx::query("DELETE FROM billing_sponsored_subscription_items WHERE organization_id = $1")
        .bind(&org_id)
        .execute(&pool)
        .await
        .expect("delete subscription items");
    sqlx::query("DELETE FROM billing_sponsored_subscriptions WHERE organization_id = $1")
        .bind(&org_id)
        .execute(&pool)
        .await
        .expect("delete sponsored subscription");
    sqlx::query("DELETE FROM organization_memberships WHERE org_id = $1")
        .bind(&org_id)
        .execute(&pool)
        .await
        .expect("delete membership");
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(&org_id)
        .execute(&pool)
        .await
        .expect("delete organisation");
    sqlx::query("DELETE FROM users WHERE id IN ($1, $2)")
        .bind(&owner)
        .bind(&beneficiary)
        .execute(&pool)
        .await
        .expect("delete users");
}

#[tokio::test]
async fn sponsored_recovery_replays_immutable_request_under_a_fenced_lease() {
    let _guard = TEST_LOCK.lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    let suffix = Uuid::new_v4().to_string();
    let org_id = format!("sponsored-recovery-org-{suffix}");
    let owner = format!("sponsored-recovery-owner-{suffix}");
    let beneficiary = format!("sponsored-recovery-beneficiary-{suffix}");
    sqlx::query(
        "INSERT INTO users (id, oauth_provider, oauth_subject) VALUES \
         ($1, 'sponsored-recovery-test', $1), ($2, 'sponsored-recovery-test', $2)",
    )
    .bind(&owner)
    .bind(&beneficiary)
    .execute(&pool)
    .await
    .expect("insert users");
    sqlx::query(
        "INSERT INTO organizations (id, enc_name, created_by) \
         VALUES ($1, decode('6f7267', 'hex'), $2)",
    )
    .bind(&org_id)
    .bind(&owner)
    .execute(&pool)
    .await
    .expect("insert organisation");
    sqlx::query(
        "INSERT INTO organization_memberships (org_id, user_id, role) \
         VALUES ($1, $2, 'owner')",
    )
    .bind(&org_id)
    .bind(&owner)
    .execute(&pool)
    .await
    .expect("insert owner membership");

    let request = SponsoredSeatRequest {
        action: SponsoredSeatAction::Add,
        beneficiary_id: beneficiary.clone(),
        replacement_beneficiary_id: None,
        offer: BillingOffer::StandardMonthly,
        quote_version: 1,
        quote_expires_at_epoch: 4_000_000_000,
        effective_from: 1_700_000_001,
        effective_until: None,
        idempotency_key: "recovery-add-1".into(),
    };
    let mut tx = pool.begin().await.expect("begin sponsored operation");
    let operation = begin_operation(&mut tx, &org_id, &owner, &request, 1_700_000_001)
        .await
        .expect("create operation");
    tx.commit().await.expect("commit operation");
    assert_eq!(
        next_recovery_candidate(&pool)
            .await
            .expect("read recovery candidates"),
        None,
        "an operation without a persisted provider request is not claimable"
    );

    let provider_request = SponsoredProviderRequest::Checkout {
        organization_id: org_id.clone(),
        customer_id: Some("cus_saved".into()),
        items: vec![sotto_server::billing::SponsoredSubscriptionItemRequest {
            provider_price_id: "price_saved".into(),
            quantity: 1,
        }],
        expires_at_epoch: 1_700_003_600,
        success_url: "https://example.test/success".into(),
        cancel_url: "https://example.test/cancel".into(),
    };
    let mut tx = pool.begin().await.expect("begin request save");
    persist_provider_request(&mut tx, &operation.operation_id, &provider_request)
        .await
        .expect("persist exact request before provider I/O");
    tx.commit().await.expect("commit request");
    assert_eq!(
        next_recovery_candidate(&pool)
            .await
            .expect("read prepared recovery candidate")
            .as_deref(),
        Some(operation.operation_id.as_str())
    );

    let claimed = claim_recovery(&pool, &operation.operation_id, "claim:first")
        .await
        .expect("claim operation")
        .expect("operation is due");
    assert_eq!(claimed.recovery_attempt_count, 0);
    assert!(claimed.provider_first_sent_epoch.is_some());
    assert_eq!(claimed.provider_request_version, Some(1));
    assert!(
        claim_recovery(&pool, &operation.operation_id, "claim:second")
            .await
            .expect("second claim check")
            .is_none()
    );

    // An expired owner cannot record a provider response after another worker takes the lease.
    sqlx::query(
        "UPDATE billing_sponsored_operations SET recovery_lease_until = now() - interval '1 second', \
         recovery_next_attempt_at = now() WHERE operation_id = $1",
    )
    .bind(&operation.operation_id)
    .execute(&pool)
    .await
    .expect("expire first lease");
    let reclaimed = claim_recovery(&pool, &operation.operation_id, "claim:second")
        .await
        .expect("reclaim operation")
        .expect("expired lease is reclaimable");
    assert_eq!(reclaimed.recovery_attempt_count, 0);
    record_recovery_failure(
        &pool,
        &operation.operation_id,
        "claim:second",
        "provider_retryable",
    )
    .await
    .expect("record one provider failure");
    sqlx::query(
        "UPDATE billing_sponsored_operations SET recovery_next_attempt_at = now() \
         WHERE operation_id = $1",
    )
    .bind(&operation.operation_id)
    .execute(&pool)
    .await
    .expect("make retry due");
    let retry = claim_recovery(&pool, &operation.operation_id, "claim:third")
        .await
        .expect("claim retry")
        .expect("retry is due");
    assert_eq!(retry.recovery_attempt_count, 1);

    let mut tx = pool.begin().await.expect("begin stale result");
    let stale = sotto_server::sponsored_billing::record_claimed_checkout(
        &mut tx,
        &operation.operation_id,
        "claim:first",
        "https://checkout.stripe.test/cs_saved",
        "cs_saved",
        Some("cus_saved"),
        Some("sub_saved"),
    )
    .await
    .expect_err("stale provider owner cannot finish the local operation");
    assert!(matches!(stale, SponsoredBillingError::ResultConflict));
    tx.rollback().await.expect("rollback stale result");

    let mut tx = pool.begin().await.expect("begin current result");
    let saved = sotto_server::sponsored_billing::record_claimed_checkout(
        &mut tx,
        &operation.operation_id,
        "claim:third",
        "https://checkout.stripe.test/cs_saved",
        "cs_saved",
        Some("cus_saved"),
        Some("sub_saved"),
    )
    .await
    .expect("current lease records the provider result");
    tx.commit().await.expect("commit current result");
    assert_eq!(saved.state, "checkout_created");
    assert_eq!(
        saved.provider_checkout_session_id.as_deref(),
        Some("cs_saved")
    );
    assert_eq!(saved.recovery_lease_token, None);

    sqlx::query(
        "UPDATE billing_sponsored_operations SET state = 'unknown', recovery_lease_token = $2, \
         recovery_lease_until = now() + interval '5 minutes' WHERE operation_id = $1",
    )
    .bind(&operation.operation_id)
    .bind("claim:expiry")
    .execute(&pool)
    .await
    .expect("model an unresolved checkout with a known session");
    let mut tx = pool
        .begin()
        .await
        .expect("begin expired checkout cancellation");
    sotto_server::founding_allocator::lock_capacity(&mut tx)
        .await
        .expect("lock founding capacity before releasing reservation");
    assert!(!cancel_claimed_rejected_operation(
        &mut tx,
        &operation.operation_id,
        "claim:stale",
        "provider_rejected"
    )
    .await
    .expect("a stale provider owner is a harmless no-op"));
    assert!(
        cancel_claimed_expired_checkout(&mut tx, &operation.operation_id, "claim:expiry")
            .await
            .expect("cancel a provider-confirmed expired checkout")
    );
    tx.commit()
        .await
        .expect("commit expired checkout cancellation");
    let failed_state: String = sqlx::query_scalar(
        "SELECT state FROM billing_sponsored_operations WHERE operation_id = $1",
    )
    .bind(&operation.operation_id)
    .fetch_one(&pool)
    .await
    .expect("read cancelled checkout operation");
    assert_eq!(failed_state, "failed");

    sqlx::query("DELETE FROM billing_sponsored_seats WHERE organization_id = $1")
        .bind(&org_id)
        .execute(&pool)
        .await
        .expect("delete seats");
    sqlx::query("DELETE FROM billing_sponsored_operations WHERE organization_id = $1")
        .bind(&org_id)
        .execute(&pool)
        .await
        .expect("delete operations");
    sqlx::query("DELETE FROM organization_memberships WHERE org_id = $1")
        .bind(&org_id)
        .execute(&pool)
        .await
        .expect("delete membership");
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(&org_id)
        .execute(&pool)
        .await
        .expect("delete organisation");
    sqlx::query("DELETE FROM users WHERE id IN ($1, $2)")
        .bind(&owner)
        .bind(&beneficiary)
        .execute(&pool)
        .await
        .expect("delete users");
}

#[tokio::test]
async fn sponsored_recovery_replays_accepted_checkout_after_response_loss_and_serialises_caller_with_worker(
) {
    let _guard = TEST_LOCK.lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    let fixture = create_recovery_fixture(&pool, &Uuid::new_v4().to_string(), false).await;
    let provider = Arc::new(LostResponseProvider::new(
        fixture.customer_id.clone(),
        fixture.session_id.clone(),
    ));
    let billing = BillingState::with_provider(
        provider.clone(),
        "whsec_test".into(),
        "https://app.example.test".into(),
    );

    let first_pool = pool.clone();
    let first_billing = billing.clone();
    let first_worker = tokio::spawn(async move {
        recover_sponsored_billing_operation_once(&first_pool, &first_billing).await
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        provider.first_call_started.notified(),
    )
    .await
    .expect("first provider call started while holding the recovery lease");

    assert!(
        recover_sponsored_billing_operation_by_id(&pool, &billing, &fixture.operation_id)
            .await
            .is_ok(),
        "the original caller may retry while the worker owns the lease"
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    provider.release_first_call.notify_one();
    assert!(first_worker
        .await
        .expect("join first worker")
        .expect("first worker records the retryable response loss"));

    let failed_attempts: i32 = sqlx::query_scalar(
        "SELECT recovery_attempt_count FROM billing_sponsored_operations WHERE operation_id = $1",
    )
    .bind(&fixture.operation_id)
    .fetch_one(&pool)
    .await
    .expect("read retry count after lost response");
    assert_eq!(failed_attempts, 1);
    sqlx::query(
        "UPDATE billing_sponsored_operations SET recovery_next_attempt_at = now() \
         WHERE operation_id = $1",
    )
    .bind(&fixture.operation_id)
    .execute(&pool)
    .await
    .expect("make idempotent replay due");

    assert!(recover_sponsored_billing_operation_once(&pool, &billing)
        .await
        .expect("replay original provider request"));
    let operation = sotto_server::sponsored_billing::load_operation_for_actor(
        &pool,
        &fixture.operation_id,
        &fixture.owner_id,
    )
    .await
    .expect("load operation after response-loss recovery")
    .expect("actor can read its operation");
    assert_eq!(operation.state, "checkout_created");
    assert_eq!(
        operation.provider_checkout_session_id.as_deref(),
        Some(fixture.session_id.as_str())
    );
    let keys = provider
        .idempotency_keys
        .lock()
        .expect("read captured provider idempotency keys")
        .clone();
    assert_eq!(keys.len(), 2);
    assert_eq!(keys[0], keys[1], "recovery must replay the stored key");
    assert_eq!(
        provider.remote_creations.load(Ordering::SeqCst),
        1,
        "the provider accepted the first request before its response was lost, so the retry must not create a second Checkout"
    );

    let (first_claim, second_claim) = tokio::join!(
        claim_recovery(&pool, &fixture.operation_id, "concurrent:one"),
        claim_recovery(&pool, &fixture.operation_id, "concurrent:two")
    );
    let first_claim = first_claim.expect("first concurrent claim");
    let second_claim = second_claim.expect("second concurrent claim");
    assert_ne!(
        first_claim.is_some(),
        second_claim.is_some(),
        "exactly one concurrent worker may own a recovery lease"
    );

    clean_recovery_fixture(&pool, &fixture).await;
}

#[tokio::test]
async fn paid_checkout_settlement_wins_against_an_in_flight_recovery_attempt() {
    let _guard = TEST_LOCK.lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    let fixture = create_recovery_fixture(&pool, &Uuid::new_v4().to_string(), false).await;
    let provider = Arc::new(LostResponseProvider::new(
        fixture.customer_id.clone(),
        fixture.session_id.clone(),
    ));
    let billing = BillingState::with_provider(
        provider.clone(),
        "whsec_test".into(),
        "https://app.example.test".into(),
    );

    let worker_pool = pool.clone();
    let worker_billing = billing.clone();
    let worker = tokio::spawn(async move {
        recover_sponsored_billing_operation_once(&worker_pool, &worker_billing).await
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        provider.first_call_started.notified(),
    )
    .await
    .expect("provider call is in flight under a recovery lease");

    let evidence = SponsoredProviderEvidence {
        customer_id: Some(fixture.customer_id.clone()),
        subscription_id: fixture.subscription_id.clone(),
        schedule_id: None,
        checkout_session_id: Some(fixture.session_id.clone()),
        payment_reference: "pi_paid_webhook".into(),
        provider_item_id: None,
    };
    let mut tx = pool.begin().await.expect("begin paid webhook settlement");
    complete_paid_checkout(&mut tx, &fixture.operation_id, &evidence)
        .await
        .expect("paid checkout evidence settles despite the recovery lease");
    tx.commit().await.expect("commit paid webhook settlement");

    provider.release_first_call.notify_one();
    let worker_result = worker.await.expect("join recovery worker");
    assert!(worker_result.is_ok());
    let operation = sotto_server::sponsored_billing::load_operation_for_actor(
        &pool,
        &fixture.operation_id,
        &fixture.owner_id,
    )
    .await
    .expect("load operation after webhook settlement")
    .expect("actor can read its operation");
    assert_eq!(operation.state, "active");
    assert_eq!(
        operation.provider_payment_reference.as_deref(),
        Some("pi_paid_webhook")
    );
    assert_eq!(
        operation.provider_checkout_session_id.as_deref(),
        Some(fixture.session_id.as_str())
    );
    assert_eq!(operation.recovery_lease_token, None);
    assert_eq!(operation.recovery_attempt_count, 0);

    clean_recovery_fixture(&pool, &fixture).await;
}

#[tokio::test]
async fn schedule_recovery_reads_back_the_exact_saved_update_before_replaying() {
    let _guard = TEST_LOCK.lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    let fixture = create_recovery_fixture(&pool, &Uuid::new_v4().to_string(), true).await;
    let provider = Arc::new(AppliedScheduleProvider {
        subscription_id: fixture.subscription_id.clone(),
        customer_id: fixture.customer_id.clone(),
        schedule_id: fixture.schedule_id.clone(),
        period_start: fixture.period_start,
        period_end: fixture.period_end,
        applied_quantity: 1,
        verification_calls: AtomicUsize::new(0),
        update_calls: AtomicUsize::new(0),
    });
    let billing = BillingState::with_provider(
        provider.clone(),
        "whsec_test".into(),
        "https://app.example.test".into(),
    );

    assert!(recover_sponsored_billing_operation_once(&pool, &billing)
        .await
        .expect("recover already-applied schedule mutation"));
    assert_eq!(provider.verification_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        provider.update_calls.load(Ordering::SeqCst),
        0,
        "read-back evidence avoids replaying an already-applied schedule update"
    );
    let operation = sotto_server::sponsored_billing::load_operation_for_actor(
        &pool,
        &fixture.operation_id,
        &fixture.owner_id,
    )
    .await
    .expect("load recovered schedule operation")
    .expect("actor can read its operation");
    assert_eq!(operation.state, "provider_pending");
    assert_eq!(
        operation.provider_subscription_id.as_deref(),
        Some(fixture.subscription_id.as_str())
    );

    clean_recovery_fixture(&pool, &fixture).await;
}

#[tokio::test]
async fn older_sponsored_schedule_recovery_settles_when_newer_schedule_includes_its_seat() {
    let _guard = TEST_LOCK.lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    let fixture = create_recovery_fixture(&pool, &Uuid::new_v4().to_string(), true).await;
    let newer_beneficiary = format!("sponsored-recovery-new-beneficiary-{}", Uuid::new_v4());
    sqlx::query(
        "INSERT INTO users (id, oauth_provider, oauth_subject) \
         VALUES ($1, 'sponsored-recovery-test', $1)",
    )
    .bind(&newer_beneficiary)
    .execute(&pool)
    .await
    .expect("insert newer operation beneficiary");

    let now = sotto_server::sponsored_billing::current_epoch();
    let newer_request = SponsoredSeatRequest {
        action: SponsoredSeatAction::Add,
        beneficiary_id: newer_beneficiary.clone(),
        replacement_beneficiary_id: None,
        offer: BillingOffer::StandardMonthly,
        quote_version: 1,
        quote_expires_at_epoch: now + 600,
        effective_from: now + 600,
        effective_until: None,
        idempotency_key: format!("newer-recovery-{}", Uuid::new_v4()),
    };
    let mut tx = pool.begin().await.expect("begin newer schedule operation");
    let newer_operation = begin_operation(
        &mut tx,
        &fixture.organization_id,
        &fixture.owner_id,
        &newer_request,
        now,
    )
    .await
    .expect("create newer operation for the same sponsored subscription");
    sqlx::query(
        "UPDATE billing_sponsored_seats SET effective_from = $2 \
         WHERE operation_id = $1",
    )
    .bind(&fixture.operation_id)
    .bind(fixture.period_end)
    .execute(&mut *tx)
    .await
    .expect("align the older seat with the shared renewal boundary");
    sqlx::query(
        "UPDATE billing_sponsored_seats SET effective_from = $2 \
         WHERE operation_id = $1",
    )
    .bind(&newer_operation.operation_id)
    .bind(fixture.period_end)
    .execute(&mut *tx)
    .await
    .expect("align the newer seat with the shared renewal boundary");
    sqlx::query(
        "UPDATE billing_sponsored_operations SET provider_subscription_id = $2 \
         WHERE operation_id = $1",
    )
    .bind(&newer_operation.operation_id)
    .bind(&fixture.subscription_id)
    .execute(&mut *tx)
    .await
    .expect("bind newer operation to the same provider subscription");
    persist_provider_request(
        &mut tx,
        &newer_operation.operation_id,
        &SponsoredProviderRequest::ScheduleUpdate {
            organization_id: fixture.organization_id.clone(),
            subscription_id: fixture.subscription_id.clone(),
            expected_period_start: fixture.period_start,
            expected_period_end: fixture.period_end,
            phases: vec![sotto_server::billing::SponsoredSubscriptionPhase {
                start_at_epoch: fixture.period_end,
                end_at_epoch: None,
                items: vec![SponsoredSubscriptionItemRequest {
                    provider_price_id: "price_recovery_test".into(),
                    quantity: 2,
                }],
            }],
        },
    )
    .await
    .expect("persist newer provider schedule request");
    tx.commit().await.expect("commit newer schedule operation");

    let provider = Arc::new(AppliedScheduleProvider {
        subscription_id: fixture.subscription_id.clone(),
        customer_id: fixture.customer_id.clone(),
        schedule_id: fixture.schedule_id.clone(),
        period_start: fixture.period_start,
        period_end: fixture.period_end,
        applied_quantity: 2,
        verification_calls: AtomicUsize::new(0),
        update_calls: AtomicUsize::new(0),
    });
    let billing = BillingState::with_provider(
        provider.clone(),
        "whsec_test".into(),
        "https://app.example.test".into(),
    )
    .with_price_catalogue(BillingPriceIds {
        standard_monthly: "price_recovery_test".into(),
        standard_annual: "price_recovery_annual".into(),
        founding_monthly: "price_recovery_founding_monthly".into(),
        founding_annual: "price_recovery_founding_annual".into(),
    });

    recover_sponsored_billing_operation_by_id(&pool, &billing, &fixture.operation_id)
        .await
        .expect("older operation settles from the current aggregate schedule");
    assert_eq!(provider.verification_calls.load(Ordering::SeqCst), 2);
    assert_eq!(provider.update_calls.load(Ordering::SeqCst), 0);
    let state: String = sqlx::query_scalar(
        "SELECT state FROM billing_sponsored_operations WHERE operation_id = $1",
    )
    .bind(&fixture.operation_id)
    .fetch_one(&pool)
    .await
    .expect("read recovered older operation");
    assert_eq!(state, "provider_pending");

    let evidence = SponsoredProviderEvidence {
        customer_id: Some(fixture.customer_id.clone()),
        subscription_id: fixture.subscription_id.clone(),
        schedule_id: Some(fixture.schedule_id.clone()),
        checkout_session_id: None,
        payment_reference: format!("pi_recovered_{}", Uuid::new_v4()),
        provider_item_id: Some("si_recovered_seat".into()),
    };
    let mut tx = pool.begin().await.expect("begin paid invoice settlement");
    complete_paid_checkout(&mut tx, &fixture.operation_id, &evidence)
        .await
        .expect("paid evidence activates the recovered seat");
    tx.commit().await.expect("commit recovered seat settlement");

    let (operation_state, seat_state): (String, String) = sqlx::query_as(
        "SELECT o.state, s.state FROM billing_sponsored_operations o \
         JOIN billing_sponsored_seats s USING (operation_id) WHERE o.operation_id = $1",
    )
    .bind(&fixture.operation_id)
    .fetch_one(&pool)
    .await
    .expect("read final recovered operation and seat state");
    assert_eq!(operation_state, "active");
    assert_eq!(seat_state, "active");

    clean_recovery_fixture(&pool, &fixture).await;
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(&newer_beneficiary)
        .execute(&pool)
        .await
        .expect("delete newer operation beneficiary after fixture cleanup");
}

#[tokio::test]
async fn unknown_removal_recovery_replays_when_stripe_still_has_both_seats() {
    let _guard = TEST_LOCK.lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    let fixture = create_recovery_fixture(&pool, &Uuid::new_v4().to_string(), true).await;
    let first_payment = SponsoredProviderEvidence {
        customer_id: Some(fixture.customer_id.clone()),
        subscription_id: fixture.subscription_id.clone(),
        schedule_id: Some(fixture.schedule_id.clone()),
        checkout_session_id: None,
        payment_reference: format!("pi_first_{}", Uuid::new_v4()),
        provider_item_id: Some("si_recovery_test".into()),
    };
    let mut tx = pool.begin().await.expect("begin first seat settlement");
    complete_paid_checkout(&mut tx, &fixture.operation_id, &first_payment)
        .await
        .expect("activate the first seat");
    tx.commit().await.expect("commit first seat settlement");

    let second_beneficiary = format!("sponsored-recovery-second-{}", Uuid::new_v4());
    sqlx::query(
        "INSERT INTO users (id, oauth_provider, oauth_subject) \
         VALUES ($1, 'sponsored-recovery-test', $1)",
    )
    .bind(&second_beneficiary)
    .execute(&pool)
    .await
    .expect("insert second beneficiary");
    let now = sotto_server::sponsored_billing::current_epoch();
    let add_request = SponsoredSeatRequest {
        action: SponsoredSeatAction::Add,
        beneficiary_id: second_beneficiary.clone(),
        replacement_beneficiary_id: None,
        offer: BillingOffer::StandardMonthly,
        quote_version: 1,
        quote_expires_at_epoch: now + 600,
        effective_from: now + 300,
        effective_until: None,
        idempotency_key: format!("second-seat-{}", Uuid::new_v4()),
    };
    let mut tx = pool.begin().await.expect("begin second seat operation");
    let second_operation = begin_operation(
        &mut tx,
        &fixture.organization_id,
        &fixture.owner_id,
        &add_request,
        now,
    )
    .await
    .expect("create second seat operation");
    tx.commit().await.expect("commit second seat operation");
    let second_payment = SponsoredProviderEvidence {
        customer_id: Some(fixture.customer_id.clone()),
        subscription_id: fixture.subscription_id.clone(),
        schedule_id: Some(fixture.schedule_id.clone()),
        checkout_session_id: None,
        payment_reference: format!("pi_second_{}", Uuid::new_v4()),
        provider_item_id: Some("si_recovery_test".into()),
    };
    let mut tx = pool.begin().await.expect("begin second seat settlement");
    complete_paid_checkout(&mut tx, &second_operation.operation_id, &second_payment)
        .await
        .expect("activate the second seat");
    tx.commit().await.expect("commit second seat settlement");

    let removal_request = SponsoredSeatRequest {
        action: SponsoredSeatAction::Remove,
        beneficiary_id: fixture.beneficiary_id.clone(),
        replacement_beneficiary_id: None,
        offer: BillingOffer::StandardMonthly,
        quote_version: 1,
        quote_expires_at_epoch: now + 600,
        effective_from: now + 300,
        effective_until: Some(fixture.period_end),
        idempotency_key: format!("remove-first-seat-{}", Uuid::new_v4()),
    };
    let mut tx = pool.begin().await.expect("begin removal operation");
    let removal = begin_operation(
        &mut tx,
        &fixture.organization_id,
        &fixture.owner_id,
        &removal_request,
        now,
    )
    .await
    .expect("create removal operation");
    persist_provider_request(
        &mut tx,
        &removal.operation_id,
        &SponsoredProviderRequest::ScheduleUpdate {
            organization_id: fixture.organization_id.clone(),
            subscription_id: fixture.subscription_id.clone(),
            expected_period_start: fixture.period_start,
            expected_period_end: fixture.period_end,
            phases: vec![sotto_server::billing::SponsoredSubscriptionPhase {
                start_at_epoch: fixture.period_end,
                end_at_epoch: None,
                items: vec![SponsoredSubscriptionItemRequest {
                    provider_price_id: "price_recovery_test".into(),
                    quantity: 1,
                }],
            }],
        },
    )
    .await
    .expect("save the removal request that timed out");
    sqlx::query(
        "UPDATE billing_sponsored_operations SET state = 'unknown', \
         provider_subscription_id = $2 WHERE operation_id = $1",
    )
    .bind(&removal.operation_id)
    .bind(&fixture.subscription_id)
    .execute(&mut *tx)
    .await
    .expect("model a timed-out provider send");
    tx.commit().await.expect("commit timed-out removal");

    let provider = Arc::new(AppliedScheduleProvider {
        subscription_id: fixture.subscription_id.clone(),
        customer_id: fixture.customer_id.clone(),
        schedule_id: fixture.schedule_id.clone(),
        period_start: fixture.period_start,
        period_end: fixture.period_end,
        applied_quantity: 2,
        verification_calls: AtomicUsize::new(0),
        update_calls: AtomicUsize::new(0),
    });
    let billing = BillingState::with_provider(
        provider.clone(),
        "whsec_test".into(),
        "https://app.example.test".into(),
    )
    .with_price_catalogue(BillingPriceIds {
        standard_monthly: "price_recovery_test".into(),
        standard_annual: "price_recovery_annual".into(),
        founding_monthly: "price_recovery_founding_monthly".into(),
        founding_annual: "price_recovery_founding_annual".into(),
    });

    recover_sponsored_billing_operation_by_id(&pool, &billing, &removal.operation_id)
        .await
        .expect("replay the removal absent from Stripe");
    assert_eq!(provider.verification_calls.load(Ordering::SeqCst), 1);
    assert_eq!(provider.update_calls.load(Ordering::SeqCst), 1);
    let (operation_state, seat_state, effective_until): (String, String, Option<i64>) =
        sqlx::query_as(
            "SELECT operation.state, seat.state, seat.effective_until \
             FROM billing_sponsored_operations AS operation \
             JOIN billing_sponsored_seats AS seat \
               ON seat.organization_id = operation.organization_id \
              AND seat.beneficiary_id = operation.beneficiary_id \
             WHERE operation.operation_id = $1",
        )
        .bind(&removal.operation_id)
        .fetch_one(&pool)
        .await
        .expect("read recovered removal state");
    assert_eq!(operation_state, "provider_pending");
    assert_eq!(seat_state, "scheduled_removal");
    assert_eq!(effective_until, Some(fixture.period_end));

    clean_recovery_fixture(&pool, &fixture).await;
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(&second_beneficiary)
        .execute(&pool)
        .await
        .expect("delete second beneficiary after fixture cleanup");
}

#[tokio::test]
async fn signed_expiry_and_refund_close_sponsored_manual_review_operations() {
    let _guard = TEST_LOCK.lock().await;
    let Some(pool) = pool().await else {
        return;
    };

    let expiry_fixture = create_recovery_fixture(&pool, &Uuid::new_v4().to_string(), false).await;
    sqlx::query(
        "UPDATE billing_sponsored_operations SET state = 'manual_review', \
         result_code = 'request_facts_missing', recovery_manual_review_required = TRUE \
         WHERE operation_id = $1",
    )
    .bind(&expiry_fixture.operation_id)
    .execute(&pool)
    .await
    .expect("park checkout for manual review");
    let mut tx = pool.begin().await.expect("begin signed expiry transaction");
    let expired =
        cancel_failed_operation(&mut tx, &expiry_fixture.operation_id, "checkout_expired")
            .await
            .expect("signed expiry closes the parked checkout");
    tx.commit().await.expect("commit signed expiry");
    assert_eq!(expired.state, "failed");
    assert!(!expired.recovery_manual_review_required);
    let expiry_seat_state: String =
        sqlx::query_scalar("SELECT state FROM billing_sponsored_seats WHERE operation_id = $1")
            .bind(&expiry_fixture.operation_id)
            .fetch_one(&pool)
            .await
            .expect("load expired seat state");
    assert_eq!(expiry_seat_state, "canceled");

    let refund_fixture = create_recovery_fixture(&pool, &Uuid::new_v4().to_string(), false).await;
    sqlx::query(
        "UPDATE billing_sponsored_operations SET state = 'manual_review', \
         result_code = 'request_facts_missing', recovery_manual_review_required = TRUE \
         WHERE operation_id = $1",
    )
    .bind(&refund_fixture.operation_id)
    .execute(&pool)
    .await
    .expect("park founding checkout for manual review");
    let evidence = SponsoredProviderEvidence {
        customer_id: Some(refund_fixture.customer_id.clone()),
        subscription_id: refund_fixture.subscription_id.clone(),
        schedule_id: None,
        checkout_session_id: Some(refund_fixture.session_id.clone()),
        payment_reference: format!("pi_refund_{}", Uuid::new_v4()),
        provider_item_id: None,
    };
    let mut tx = pool.begin().await.expect("begin signed refund transaction");
    let refund = record_founding_refund_required(&mut tx, &refund_fixture.operation_id, &evidence)
        .await
        .expect("signed paid evidence records the refund for a parked checkout");
    tx.commit().await.expect("commit signed refund result");
    assert_eq!(refund.state, "failed");
    assert_eq!(
        refund.result_code.as_deref(),
        Some("founding_refund_required")
    );
    assert_eq!(
        refund.provider_payment_reference.as_deref(),
        Some(evidence.payment_reference.as_str())
    );
    assert!(!refund.recovery_manual_review_required);
    let refund_seat_state: String =
        sqlx::query_scalar("SELECT state FROM billing_sponsored_seats WHERE operation_id = $1")
            .bind(&refund_fixture.operation_id)
            .fetch_one(&pool)
            .await
            .expect("load refund-required seat state");
    assert_eq!(refund_seat_state, "canceled");

    clean_recovery_fixture(&pool, &expiry_fixture).await;
    clean_recovery_fixture(&pool, &refund_fixture).await;
}
