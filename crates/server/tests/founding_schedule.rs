//! Database-backed coverage for durable founding-price transition work.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

use async_trait::async_trait;
use sqlx::{PgPool, Row};

use sotto_server::{
    billing::{
        ProviderError, ProviderErrorKind, ProviderResult, SponsoredSubscriptionItem,
        SponsoredSubscriptionPhase, SponsoredSubscriptionSnapshot, SubscriptionObservation,
        SubscriptionProvider, SubscriptionSnapshot, SubscriptionStatus,
    },
    billing_catalogue::BillingPriceIds,
    db,
    founding_allocator::{FoundingDate, FoundingOffer, FoundingPayerKind},
    founding_price_notices, founding_schedule, founding_schedule_worker,
};

mod support {
    pub mod migrations;
}

use support::migrations::{migrator_before, DisposableDatabase};

struct CancellationWorkerProvider {
    pool: PgPool,
    cancellations: Arc<AtomicUsize>,
    updates: Arc<AtomicUsize>,
    cancellation_keys: Arc<Mutex<Vec<String>>>,
    request_during_update: bool,
    provider_cancel_at_period_end: bool,
}

impl CancellationWorkerProvider {
    fn unsupported() -> ProviderError {
        ProviderError {
            status: None,
            code: Some("test_unsupported".into()),
            kind: ProviderErrorKind::Unknown,
        }
    }

    fn snapshot(
        subscription_id: &str,
        cancel_at_period_end: bool,
    ) -> SponsoredSubscriptionSnapshot {
        SponsoredSubscriptionSnapshot {
            customer_id: Some("cus_founding_schedule".into()),
            default_payment_method_id: None,
            subscription_id: subscription_id.into(),
            status: SubscriptionStatus::Active,
            automatic_tax_enabled: false,
            cancel_at_period_end,
            cancel_at: None,
            current_period_start: Some(1_700_000_000),
            current_period_end: Some(1_702_592_000),
            schedule_id: Some("sched_founding_schedule".into()),
            items: vec![SponsoredSubscriptionItem {
                provider_item_id: "si_founding_schedule".into(),
                provider_price_id: "price_founding_monthly".into(),
                quantity: 1,
            }],
        }
    }
}

#[async_trait]
impl SubscriptionProvider for CancellationWorkerProvider {
    async fn create_checkout(
        &self,
        _org_id: &str,
        _customer: Option<&str>,
        _success_url: &str,
        _cancel_url: &str,
    ) -> ProviderResult<String> {
        Err(Self::unsupported())
    }

    async fn create_portal(
        &self,
        _customer: &str,
        _return_url: &str,
        _configuration_id: Option<&str>,
    ) -> ProviderResult<String> {
        Err(Self::unsupported())
    }

    async fn get_subscription(
        &self,
        subscription_id: &str,
    ) -> ProviderResult<SubscriptionObservation> {
        Ok(SubscriptionObservation::Current(SubscriptionSnapshot {
            id: subscription_id.into(),
            status: SubscriptionStatus::Active,
        }))
    }

    async fn cancel_subscription(
        &self,
        subscription_id: &str,
        idempotency_key: &str,
        _user_id: &str,
    ) -> ProviderResult<SubscriptionObservation> {
        self.cancellations.fetch_add(1, Ordering::Relaxed);
        self.cancellation_keys
            .lock()
            .expect("cancellation key lock")
            .push(idempotency_key.to_owned());
        self.get_subscription(subscription_id).await
    }

    async fn get_sponsored_subscription(
        &self,
        subscription_id: &str,
    ) -> ProviderResult<SponsoredSubscriptionSnapshot> {
        Ok(Self::snapshot(
            subscription_id,
            self.provider_cancel_at_period_end,
        ))
    }

    async fn update_personal_subscription(
        &self,
        subscription_id: &str,
        _phases: &[SponsoredSubscriptionPhase],
        _idempotency_key: &str,
    ) -> ProviderResult<SponsoredSubscriptionSnapshot> {
        self.updates.fetch_add(1, Ordering::Relaxed);
        if self.request_during_update {
            let mut tx = self.pool.begin().await.map_err(|_| Self::unsupported())?;
            founding_schedule::request_personal_cancellation(
                &mut tx,
                "founding-schedule-beneficiary",
                subscription_id,
            )
            .await
            .map_err(|_| Self::unsupported())?;
            tx.commit().await.map_err(|_| Self::unsupported())?;
            self.cancel_personal_subscription(
                subscription_id,
                "route-request-key",
                "founding-schedule-beneficiary",
            )
            .await?;
            founding_schedule::record_personal_cancellation(
                &self.pool,
                "founding-schedule-beneficiary",
                subscription_id,
            )
            .await
            .map_err(|_| Self::unsupported())?;
        }
        Ok(Self::snapshot(
            subscription_id,
            self.provider_cancel_at_period_end,
        ))
    }
}

#[tokio::test]
async fn founding_schedule_retargets_and_cancellation_wins_provider_races() {
    let Some(database) = DisposableDatabase::create().await else {
        eprintln!("skipping founding schedule test: set SOTTO_RUN_DB_TESTS=1");
        return;
    };
    let pool = &database.pool;
    migrator_before(55)
        .run(pool)
        .await
        .expect("migrate to the founding schedule boundary");

    let user = "founding-schedule-beneficiary";
    sqlx::query(
        "INSERT INTO users (id, oauth_provider, oauth_subject) \
         VALUES ($1, 'founding-schedule-test', $1)",
    )
    .bind(user)
    .execute(pool)
    .await
    .expect("insert beneficiary");
    sqlx::query(
        "INSERT INTO billing_operations \
         (operation_id, idempotency_key, request_hash, actor_user_id, payer_id, beneficiary_id, \
          offer, quote_version, quote_expires_at_epoch, provider_idempotency_key) \
         VALUES ('founding-schedule-operation', 'founding-schedule-operation', 'hash', \
          $1, $1, $1, 'founding_monthly', 1, 1800000000, 'founding-schedule-operation')",
    )
    .bind(user)
    .execute(pool)
    .await
    .expect("insert source operation");
    sqlx::query(
        "INSERT INTO billing_personal_accounts \
         (user_id, operation_id, offer, stripe_subscription_id, state, pending_expires_at_epoch) \
         VALUES ($1, 'founding-schedule-operation', 'founding_monthly', \
          'sub_founding_schedule_old', 'active', 1900000000)",
    )
    .bind(user)
    .execute(pool)
    .await
    .expect("insert active personal subscription");
    sqlx::query(
        "INSERT INTO billing_founding_awards \
         (award_id, beneficiary_id, payer_kind, payer_id, offer, cohort_ordinal, \
          original_start_date, original_end_date) \
         VALUES ('founding-schedule-award', $1, 'personal', $1, 'founding_monthly', \
          1, '2026-01-01', '2027-01-01')",
    )
    .bind(user)
    .execute(pool)
    .await
    .expect("insert legacy award without a verified time anchor");

    db::migrate(pool)
        .await
        .expect("apply founding schedule migration");
    let legacy_notice_eligible: bool = sqlx::query_scalar(
        "SELECT price_notice_eligible FROM billing_founding_schedule_jobs WHERE award_id='founding-schedule-award'",
    )
    .fetch_one(pool)
    .await
    .expect("read migrated legacy notice eligibility");
    assert!(
        !legacy_notice_eligible,
        "migration must not backfill notices for pre-existing awards"
    );
    assert!(sqlx::query(
        "UPDATE billing_founding_awards SET original_start_epoch = NULL, original_end_epoch = 1 \
         WHERE award_id = 'founding-schedule-award'",
    )
    .execute(pool)
    .await
    .is_err());
    assert!(sqlx::query(
        "UPDATE billing_founding_awards SET original_start_epoch = 1, original_end_epoch = NULL \
         WHERE award_id = 'founding-schedule-award'",
    )
    .execute(pool)
    .await
    .is_err());
    let migrated = sqlx::query(
        "SELECT job.state, job.last_error_code, job.provider_subscription_id, \
                award.original_end_epoch \
         FROM billing_founding_schedule_jobs AS job \
         JOIN billing_founding_awards AS award ON award.award_id = job.award_id \
         WHERE job.award_id = 'founding-schedule-award'",
    )
    .fetch_one(pool)
    .await
    .expect("legacy subscription is represented by an unresolved job");
    assert_eq!(
        migrated.try_get::<String, _>("state").unwrap(),
        "unresolved"
    );
    assert_eq!(
        migrated.try_get::<String, _>("last_error_code").unwrap(),
        "local_evidence_invalid"
    );
    assert_eq!(
        migrated
            .try_get::<String, _>("provider_subscription_id")
            .unwrap(),
        "sub_founding_schedule_old"
    );
    assert_eq!(
        migrated
            .try_get::<Option<i64>, _>("original_end_epoch")
            .unwrap(),
        None,
        "migration must not guess a legacy award's verified Stripe boundary"
    );

    let award = sotto_server::founding_allocator::FoundingAward {
        award_id: "founding-schedule-award".into(),
        beneficiary_id: user.into(),
        payer_kind: FoundingPayerKind::Personal,
        payer_id: user.into(),
        offer: FoundingOffer::Monthly,
        cohort_ordinal: 1,
        original_start: FoundingDate::parse("2026-01-01").unwrap(),
        original_end: FoundingDate::parse("2027-01-01").unwrap(),
        original_start_epoch: None,
        original_end_epoch: None,
    };
    let mut tx = pool.begin().await.expect("begin queue retarget");
    founding_schedule::enqueue(&mut tx, &award, "sub_founding_schedule_new")
        .await
        .expect("retarget the job to the verified destination subscription");
    tx.commit().await.expect("commit retarget");

    let stale = founding_schedule::claim_due(pool)
        .await
        .expect("claim first retargeted work")
        .expect("first retargeted work is due");
    assert_eq!(stale.subscription_id, "sub_founding_schedule_new");
    assert_eq!(
        stale.idempotency_key,
        "founding-transition:founding-schedule-award:2"
    );
    let mut tx = pool.begin().await.expect("begin second retarget");
    founding_schedule::enqueue(&mut tx, &award, "sub_founding_schedule_final")
        .await
        .expect("a newer destination supersedes the leased work");
    tx.commit().await.expect("commit second retarget");
    assert!(
        !founding_schedule::mark_scheduled(pool, &stale, "sched_stale")
            .await
            .expect("stale completion is checked conditionally")
    );

    let current = founding_schedule::claim_due(pool)
        .await
        .expect("claim current work")
        .expect("new version remains due");
    assert_eq!(current.subscription_id, "sub_founding_schedule_final");
    assert_eq!(
        current.idempotency_key,
        "founding-transition:founding-schedule-award:3"
    );
    assert!(
        founding_schedule::mark_scheduled(pool, &current, "sched_current")
            .await
            .expect("record current verified schedule")
    );
    let mut tx = pool.begin().await.expect("begin idempotent enqueue");
    founding_schedule::enqueue(&mut tx, &award, "sub_founding_schedule_final")
        .await
        .expect("same target is a no-op");
    tx.commit().await.expect("commit idempotent enqueue");
    let state: String = sqlx::query_scalar(
        "SELECT state FROM billing_founding_schedule_jobs WHERE award_id = 'founding-schedule-award'",
    )
    .fetch_one(pool)
    .await
    .expect("load final schedule job state");
    assert_eq!(state, "scheduled");

    sqlx::query(
        "UPDATE billing_personal_accounts SET stripe_subscription_id = \
           'sub_founding_schedule_final' WHERE user_id = $1",
    )
    .bind(user)
    .execute(pool)
    .await
    .expect("move account to the verified destination subscription");
    let mut tx = pool.begin().await.expect("begin cancellation request");
    founding_schedule::request_personal_cancellation(&mut tx, user, "sub_founding_schedule_final")
        .await
        .expect("persist cancellation intent and requeue transition work");
    tx.commit().await.expect("commit cancellation request");
    let pending: (String, Option<String>, Option<String>, bool) = sqlx::query_as(
        "SELECT job.state, job.provider_schedule_id, job.lease_token, \
                account.cancellation_requested_at IS NOT NULL \
         FROM billing_founding_schedule_jobs AS job \
         JOIN billing_personal_accounts AS account ON account.user_id = job.beneficiary_id \
         WHERE job.award_id = 'founding-schedule-award'",
    )
    .fetch_one(pool)
    .await
    .expect("read pending cancellation");
    assert_eq!(pending, ("pending".into(), None, None, true));

    let cancellations = Arc::new(AtomicUsize::new(0));
    let updates = Arc::new(AtomicUsize::new(0));
    let cancellation_keys = Arc::new(Mutex::new(Vec::new()));
    let catalogue = BillingPriceIds {
        standard_monthly: "price_standard_monthly".into(),
        standard_annual: "price_standard_annual".into(),
        founding_monthly: "price_founding_monthly".into(),
        founding_annual: "price_founding_annual".into(),
    };
    let provider = CancellationWorkerProvider {
        pool: pool.clone(),
        cancellations: cancellations.clone(),
        updates: updates.clone(),
        cancellation_keys: cancellation_keys.clone(),
        request_during_update: false,
        provider_cancel_at_period_end: false,
    };
    assert!(
        founding_schedule_worker::run_once(pool, &provider, &catalogue)
            .await
            .expect("worker reconciles pending cancellation")
    );
    assert_eq!(cancellations.load(Ordering::Relaxed), 1);
    assert_eq!(updates.load(Ordering::Relaxed), 0);
    let completed: (String, bool) = sqlx::query_as(
        "SELECT job.state, account.cancel_at_period_end \
         FROM billing_founding_schedule_jobs AS job \
         JOIN billing_personal_accounts AS account ON account.user_id = job.beneficiary_id \
         WHERE job.award_id = 'founding-schedule-award'",
    )
    .fetch_one(pool)
    .await
    .expect("read completed cancellation");
    assert_eq!(completed, ("not_required".into(), true));

    let mut tx = pool.begin().await.expect("begin reactivation");
    founding_schedule::resume_personal_transition(&mut tx, user, "sub_founding_schedule_final")
        .await
        .expect("reactivation requeues founding transition");
    tx.commit().await.expect("commit reactivation");
    let resumed: (String, Option<String>, i64, String) = sqlx::query_as(
        "SELECT state, provider_schedule_id, version, provider_idempotency_key \
         FROM billing_founding_schedule_jobs WHERE award_id = 'founding-schedule-award'",
    )
    .fetch_one(pool)
    .await
    .expect("read requeued founding transition");
    assert_eq!(
        resumed,
        (
            "pending".into(),
            None,
            4,
            "founding-transition:founding-schedule-award:4".into(),
        )
    );

    sqlx::query(
        "UPDATE billing_personal_accounts SET cancel_at_period_end = FALSE, \
           cancellation_requested_at = NULL WHERE user_id = $1",
    )
    .bind(user)
    .execute(pool)
    .await
    .expect("simulate stale reactivation webhook state");
    sqlx::query(
        "UPDATE billing_founding_awards SET original_start_epoch = 1600000000, \
           original_end_epoch = 1750000000 WHERE award_id = 'founding-schedule-award'",
    )
    .execute(pool)
    .await
    .expect("pin verified founding boundary for worker reconciliation");
    let provider = CancellationWorkerProvider {
        pool: pool.clone(),
        cancellations: cancellations.clone(),
        updates: updates.clone(),
        cancellation_keys: cancellation_keys.clone(),
        request_during_update: false,
        provider_cancel_at_period_end: true,
    };
    assert!(
        founding_schedule_worker::run_once(pool, &provider, &catalogue)
            .await
            .expect("worker trusts current provider cancellation state")
    );
    assert_eq!(cancellations.load(Ordering::Relaxed), 1);
    assert_eq!(updates.load(Ordering::Relaxed), 0);
    let reconciled: (String, bool) = sqlx::query_as(
        "SELECT job.state, account.cancel_at_period_end \
         FROM billing_founding_schedule_jobs AS job \
         JOIN billing_personal_accounts AS account ON account.user_id = job.beneficiary_id \
         WHERE job.award_id = 'founding-schedule-award'",
    )
    .fetch_one(pool)
    .await
    .expect("read provider cancellation reconciliation");
    assert_eq!(reconciled, ("not_required".into(), true));

    sqlx::query(
        "UPDATE billing_personal_accounts SET cancel_at_period_end = FALSE, \
           cancellation_requested_at = NULL WHERE user_id = $1",
    )
    .bind(user)
    .execute(pool)
    .await
    .expect("reset account for in-flight cancellation race");
    sqlx::query(
        "UPDATE billing_founding_schedule_jobs SET state = 'pending', \
           provider_schedule_id = NULL, last_error_code = NULL, lease_token = NULL, \
           lease_until = NULL, next_attempt_at = now() \
         WHERE award_id = 'founding-schedule-award'",
    )
    .execute(pool)
    .await
    .expect("queue transition before cancellation race");
    let provider = CancellationWorkerProvider {
        pool: pool.clone(),
        cancellations: cancellations.clone(),
        updates: updates.clone(),
        cancellation_keys: cancellation_keys.clone(),
        request_during_update: true,
        provider_cancel_at_period_end: false,
    };
    assert!(
        founding_schedule_worker::run_once(pool, &provider, &catalogue)
            .await
            .expect("worker reconciles cancellation racing a schedule write")
    );
    assert_eq!(updates.load(Ordering::Relaxed), 1);
    assert_eq!(cancellations.load(Ordering::Relaxed), 3);
    {
        let cancellation_keys = cancellation_keys.lock().expect("cancellation key lock");
        assert_eq!(cancellation_keys.len(), 3);
        assert_ne!(
            cancellation_keys[1], cancellation_keys[2],
            "the worker's repair must not replay the route's earlier Stripe response"
        );
    }
    let completed: (String, bool) = sqlx::query_as(
        "SELECT job.state, account.cancel_at_period_end \
         FROM billing_founding_schedule_jobs AS job \
         JOIN billing_personal_accounts AS account ON account.user_id = job.beneficiary_id \
         WHERE job.award_id = 'founding-schedule-award'",
    )
    .fetch_one(pool)
    .await
    .expect("read raced cancellation completion");
    assert_eq!(completed, ("not_required".into(), true));

    // A legacy award can still be transferred, but the transfer must not opt a row with no
    // verified boundary into repeated notice reconciliation.
    sqlx::query(
        "UPDATE billing_founding_awards SET payer_kind='sponsor', payer_id='legacy-sponsor' \
         WHERE award_id='founding-schedule-award'",
    )
    .execute(pool)
    .await
    .expect("simulate transfer of a legacy award without a verified boundary");
    let mut transferred_legacy_award = award.clone();
    transferred_legacy_award.payer_kind = FoundingPayerKind::Sponsor;
    transferred_legacy_award.payer_id = "legacy-sponsor".into();
    let mut tx = pool.begin().await.expect("begin legacy transfer enqueue");
    founding_schedule::enqueue(&mut tx, &transferred_legacy_award, "sub_legacy_sponsor")
        .await
        .expect("retarget the legacy award schedule");
    tx.commit().await.expect("commit legacy transfer enqueue");
    let eligible: bool = sqlx::query_scalar(
        "SELECT price_notice_eligible FROM billing_founding_schedule_jobs \
         WHERE award_id='founding-schedule-award'",
    )
    .fetch_one(pool)
    .await
    .expect("read legacy transfer notice eligibility");
    assert!(!eligible);
    assert!(
        !founding_price_notices::run_once(pool, 1_900_000_000)
            .await
            .expect("legacy award is excluded from notice reconciliation"),
        "an award without a verified boundary must not be selected on every worker poll"
    );

    database.cleanup().await;
}
