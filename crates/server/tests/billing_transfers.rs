//! Opt-in Postgres coverage for beneficiary-scoped payer transfer intents.

use std::str::FromStr;

use sqlx::{postgres::PgConnectOptions, PgPool, Row};
use uuid::Uuid;

use sotto_server::billing_catalogue::BillingOffer;
use sotto_server::billing_transfers::{
    accept_transfer, begin_source_adjustment, begin_source_restoration_payment, begin_transfer,
    claim_transfer_for_execution, complete_source_adjustment, complete_source_restoration,
    defer_execution_lease, finish_execution_lease, list_manual_review_transfers,
    load_manual_review_transfer, lock_and_check_no_live_transfer,
    lock_and_check_no_provider_transfer_except, mark_failed, record_destination_paid,
    record_destination_prepared, record_destination_schedule, record_destination_setup_completion,
    record_operator_review_decision, record_source_restoration_payment,
    record_source_restoration_subscription, release_execution_lease,
    repark_mismatched_destination_invoice, request_source_restoration, withdraw_transfer,
    BeginTransfer, TransferError, TransferExecutionFailure, TransferPayer, TransferRequest,
    TransferReviewAction, TransferState,
};
use sotto_server::db;
use sotto_server::founding_allocator::{FoundingDate, FoundingOffer};
use sotto_server::sponsored_billing::{
    begin_operation, complete_paid_checkout, SponsoredBillingError, SponsoredProviderEvidence,
    SponsoredSeatAction, SponsoredSeatRequest,
};

async fn pool_or_skip() -> Option<PgPool> {
    if std::env::var("SOTTO_RUN_DB_TESTS").as_deref() != Ok("1") {
        eprintln!("skipping billing transfer tests: SOTTO_RUN_DB_TESTS=1 not set");
        return None;
    }
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL for opted-in database tests");
    let options = PgConnectOptions::from_str(&url).expect("parse DATABASE_URL");
    assert!(
        matches!(options.get_host(), "localhost" | "127.0.0.1" | "::1"),
        "refusing billing transfer tests against non-local host: {}",
        options.get_host()
    );
    let pool = db::connect(&url).await.expect("connect to test database");
    db::migrate(&pool).await.expect("apply migrations");
    Some(pool)
}

async fn cleanup(pool: &PgPool, fixture: &str) {
    let pattern = format!("transfer-test-{fixture}-%");
    sqlx::query(
        "DELETE FROM billing_transfer_review_decisions \
         WHERE transfer_id IN (SELECT transfer_id FROM billing_transfer_intents \
           WHERE actor_user_id LIKE $1 OR beneficiary_id LIKE $1 \
              OR source_organization_id LIKE $1 OR destination_organization_id LIKE $1)",
    )
    .bind(&pattern)
    .execute(pool)
    .await
    .expect("clean transfer review decisions");
    sqlx::query(
        "DELETE FROM billing_transfer_intents
         WHERE actor_user_id LIKE $1 OR beneficiary_id LIKE $1
            OR source_organization_id LIKE $1 OR destination_organization_id LIKE $1",
    )
    .bind(&pattern)
    .execute(pool)
    .await
    .expect("clean transfer intents");
    sqlx::query("DELETE FROM billing_founding_schedule_jobs WHERE beneficiary_id LIKE $1")
        .bind(&pattern)
        .execute(pool)
        .await
        .expect("clean founding schedule jobs");
    sqlx::query("DELETE FROM billing_founding_awards WHERE beneficiary_id LIKE $1")
        .bind(&pattern)
        .execute(pool)
        .await
        .expect("clean founding awards");
    sqlx::query("DELETE FROM billing_sponsored_seats WHERE seat_id LIKE $1")
        .bind(&pattern)
        .execute(pool)
        .await
        .expect("clean sponsored seats");
    sqlx::query("DELETE FROM billing_sponsored_operations WHERE operation_id LIKE $1")
        .bind(&pattern)
        .execute(pool)
        .await
        .expect("clean sponsored operations");
    sqlx::query("DELETE FROM billing_sponsored_subscriptions WHERE organization_id LIKE $1")
        .bind(&pattern)
        .execute(pool)
        .await
        .expect("clean sponsored subscriptions");
    sqlx::query("DELETE FROM billing_personal_accounts WHERE user_id LIKE $1")
        .bind(&pattern)
        .execute(pool)
        .await
        .expect("clean personal accounts");
    sqlx::query("DELETE FROM billing_operations WHERE operation_id LIKE $1")
        .bind(&pattern)
        .execute(pool)
        .await
        .expect("clean billing operations");
    sqlx::query("DELETE FROM organization_memberships WHERE org_id LIKE $1")
        .bind(&pattern)
        .execute(pool)
        .await
        .expect("clean memberships");
    sqlx::query("DELETE FROM organizations WHERE id LIKE $1")
        .bind(&pattern)
        .execute(pool)
        .await
        .expect("clean organisations");
    sqlx::query("DELETE FROM users WHERE id LIKE $1")
        .bind(&pattern)
        .execute(pool)
        .await
        .expect("clean users");
}

fn personal_to_sponsor_request(
    actor: &str,
    beneficiary: &str,
    org: &str,
    key: &str,
) -> TransferRequest {
    let fixture = actor
        .strip_prefix("transfer-test-")
        .and_then(|value| value.strip_suffix("-user"));
    TransferRequest {
        actor_user_id: actor.into(),
        counterparty_user_id: actor.into(),
        beneficiary_id: beneficiary.into(),
        source_kind: TransferPayer::Personal,
        source_organization_id: None,
        source_provider_subscription_id: fixture
            .map(|fixture| format!("sub_transfer_source_{fixture}"))
            .unwrap_or_else(|| "sub_transfer_source".into()),
        destination_kind: TransferPayer::Sponsor,
        destination_organization_id: Some(org.into()),
        offer: BillingOffer::FoundingMonthly,
        quote_version: 1,
        quote_expires_at_epoch: 1_800_001_000,
        effective_from: 1_802_592_000,
        effective_until: None,
        idempotency_key: key.into(),
    }
}

#[tokio::test]
async fn transfer_is_idempotent_and_moves_founding_payer_after_paid_destination() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let fixture = Uuid::new_v4().to_string();
    let user = format!("transfer-test-{fixture}-user");
    // Deliberately reuse the user text as the organisation text: the typed payer column must
    // distinguish these otherwise-colliding namespaces.
    let org = user.clone();
    let operation = format!("transfer-test-{fixture}-personal-operation");
    let award = format!("transfer-test-{fixture}-award");
    cleanup(&pool, &fixture).await;

    sqlx::query(
        "INSERT INTO users (id, oauth_provider, oauth_subject) VALUES ($1, 'transfer-test', $1)",
    )
    .bind(&user)
    .execute(&pool)
    .await
    .expect("insert beneficiary");
    sqlx::query("INSERT INTO organizations (id, enc_name, created_by) VALUES ($1, decode('6f7267', 'hex'), $2)")
        .bind(&org)
        .bind(&user)
        .execute(&pool)
        .await
        .expect("insert destination organisation");
    sqlx::query(
        "INSERT INTO organization_memberships (org_id, user_id, role) VALUES ($1, $2, 'owner')",
    )
    .bind(&org)
    .bind(&user)
    .execute(&pool)
    .await
    .expect("insert destination owner");
    sqlx::query(
        "INSERT INTO billing_sponsored_subscriptions \
         (organization_id, provider_customer_id, provider_subscription_id, status, \
          current_period_start, current_period_end) \
         VALUES ($1, $2, $3, 'active', 1800000000, 1802592000)",
    )
    .bind(&org)
    .bind(format!("cus_transfer_destination_{fixture}"))
    .bind(format!("sub_transfer_destination_{fixture}"))
    .execute(&pool)
    .await
    .expect("insert destination sponsor subscription at the source boundary");
    sqlx::query(
        "INSERT INTO billing_operations
         (operation_id, idempotency_key, request_hash, actor_user_id, payer_id, beneficiary_id,
          offer, quote_version, quote_expires_at_epoch, provider_idempotency_key)
         VALUES ($1, $1, $1, $2, $2, $2, 'founding_monthly', 1, 1800001000, $1)",
    )
    .bind(&operation)
    .bind(&user)
    .execute(&pool)
    .await
    .expect("insert source operation");
    sqlx::query(
        "INSERT INTO billing_personal_accounts
         (user_id, operation_id, offer, stripe_subscription_id, state, pending_expires_at_epoch,
          paid_through_epoch, paid_through_date)
         VALUES ($1, $2, 'founding_monthly', $3, 'active', 1900000000, $4,
                 to_timestamp($4)::date)",
    )
    .bind(&user)
    .bind(&operation)
    .bind(format!("sub_transfer_source_{fixture}"))
    .bind(1_802_592_000_i64)
    .execute(&pool)
    .await
    .expect("insert active personal source");
    let cohort_ordinal: i32 = sqlx::query_scalar(
        "SELECT candidate FROM generate_series(1, 100) AS series(candidate)
         WHERE NOT EXISTS (SELECT 1 FROM billing_founding_awards WHERE cohort_ordinal = candidate)
         LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .expect("find available founding place");
    sqlx::query(
        "INSERT INTO billing_founding_awards
         (award_id, beneficiary_id, payer_id, offer, cohort_ordinal, original_start_date, original_end_date,
          original_start_epoch, original_end_epoch)
         VALUES ($1, $2, $2, 'founding_monthly', $3, '2027-01-15', '2027-02-14',
                 1800000000, 1802592000)",
    )
    .bind(&award)
    .bind(&user)
    .bind(cohort_ordinal)
    .execute(&pool)
    .await
    .expect("insert founding award");

    let mut request = personal_to_sponsor_request(&user, &user, &org, "transfer-key");
    request.offer = BillingOffer::StandardMonthly;
    let inflight_operation = format!("transfer-test-{fixture}-inflight-operation");
    sqlx::query(
        "INSERT INTO billing_sponsored_operations \
         (operation_id, organization_id, actor_user_id, idempotency_key, request_hash, action, \
          beneficiary_id, offer, quote_version, quote_expires_at_epoch, effective_from, \
          provider_idempotency_key, state) \
         VALUES ($1, $2, $3, $1, $1, 'add', $3, 'standard_monthly', 1, 1800001000, 1802592000, $1, 'pending')",
    )
    .bind(&inflight_operation)
    .bind(&org)
    .bind(&user)
    .execute(&pool)
    .await
    .expect("seed in-flight sponsored operation");
    let mut tx = pool
        .begin()
        .await
        .expect("begin transfer during provider change");
    assert!(matches!(
        begin_transfer(&mut tx, &request, 1_800_000_000).await,
        Err(TransferError::AlreadyInProgress)
    ));
    tx.rollback()
        .await
        .expect("rollback transfer blocked by provider change");
    sqlx::query(
        "UPDATE billing_sponsored_operations SET state = 'failed', result_code = 'fixture_done' \
         WHERE operation_id = $1",
    )
    .bind(&inflight_operation)
    .execute(&pool)
    .await
    .expect("finish in-flight fixture operation");

    sqlx::query(
        "UPDATE billing_sponsored_subscriptions SET current_period_end = 1802592001 \
         WHERE organization_id = $1",
    )
    .bind(&org)
    .execute(&pool)
    .await
    .expect("move destination renewal away from the source boundary");
    let mut tx = pool
        .begin()
        .await
        .expect("begin mismatched destination renewal");
    assert!(matches!(
        begin_transfer(&mut tx, &request, 1_800_000_000).await,
        Err(TransferError::DestinationTermChanged)
    ));
    tx.rollback()
        .await
        .expect("rollback mismatched destination renewal");
    sqlx::query(
        "UPDATE billing_sponsored_subscriptions SET current_period_end = 1802592000 \
         WHERE organization_id = $1",
    )
    .bind(&org)
    .execute(&pool)
    .await
    .expect("restore matching destination renewal");

    let mut tx = pool.begin().await.expect("begin transfer");
    let created = begin_transfer(&mut tx, &request, 1_800_000_000)
        .await
        .expect("create transfer");
    let transfer = match created {
        BeginTransfer::Created(transfer) => transfer,
        BeginTransfer::AlreadyExists(_) => panic!("first transfer unexpectedly existed"),
    };
    tx.commit().await.expect("commit transfer");

    for deferral in 0..25 {
        let waiting_lease = claim_transfer_for_execution(&pool, &transfer.transfer_id)
            .await
            .expect("claim accepted transfer")
            .expect("accepted transfer is due");
        assert_eq!(waiting_lease.transfer.transfer_id, transfer.transfer_id);
        assert_eq!(
            waiting_lease.transfer.execution_attempt_count, 0,
            "a claim after {deferral} invoice deferrals is not a failure"
        );
        if deferral == 0 {
            assert!(claim_transfer_for_execution(&pool, &transfer.transfer_id)
                .await
                .expect("live lease prevents a second claim")
                .is_none());
        }
        assert!(
            defer_execution_lease(&pool, &transfer.transfer_id, &waiting_lease.lease_token, 1,)
                .await
                .expect("defer while waiting for the renewal invoice")
        );
        sqlx::query(
            "UPDATE billing_transfer_intents SET execution_next_attempt_at = now() - interval '1 second' \
             WHERE transfer_id = $1",
        )
        .bind(&transfer.transfer_id)
        .execute(&pool)
        .await
        .expect("make deferred transfer due using the database clock");
    }

    let first_failure = claim_transfer_for_execution(&pool, &transfer.transfer_id)
        .await
        .expect("claim accepted transfer after routine deferrals")
        .expect("accepted transfer is due");
    assert_eq!(first_failure.transfer.execution_attempt_count, 0);
    assert!(release_execution_lease(
        &pool,
        &transfer.transfer_id,
        &first_failure.lease_token,
        TransferExecutionFailure::RetryableProvider,
    )
    .await
    .expect("release retryable lease"));
    let failure_count: i32 = sqlx::query_scalar(
        "SELECT execution_attempt_count FROM billing_transfer_intents WHERE transfer_id = $1",
    )
    .bind(&transfer.transfer_id)
    .fetch_one(&pool)
    .await
    .expect("read consecutive failure count");
    assert_eq!(failure_count, 1, "only the provider failure is counted");
    let needs_review: bool = sqlx::query_scalar(
        "SELECT execution_manual_review_required FROM billing_transfer_intents WHERE transfer_id = $1",
    )
    .bind(&transfer.transfer_id)
    .fetch_one(&pool)
    .await
    .expect("read manual review state");
    assert!(
        !needs_review,
        "routine invoice waits do not exhaust retries"
    );
    assert!(claim_transfer_for_execution(&pool, &transfer.transfer_id)
        .await
        .expect("backoff prevents an early retry")
        .is_none());
    sqlx::query(
        "UPDATE billing_transfer_intents SET execution_next_attempt_at = now() - interval '1 second' \
         WHERE transfer_id = $1",
    )
    .bind(&transfer.transfer_id)
    .execute(&pool)
    .await
    .expect("make retry due using the database clock");
    let retry = claim_transfer_for_execution(&pool, &transfer.transfer_id)
        .await
        .expect("claim after retry delay")
        .expect("transfer becomes due after its delay");
    assert_ne!(first_failure.lease_token, retry.lease_token);
    assert_eq!(retry.transfer.execution_attempt_count, 1);
    assert_eq!(
        retry.transfer.provider_idempotency_key, transfer.provider_idempotency_key,
        "a retry keeps the same provider operation identity"
    );
    sqlx::query(
        "UPDATE billing_transfer_intents SET execution_lease_until = now() - interval '1 second' \
         WHERE transfer_id = $1",
    )
    .bind(&transfer.transfer_id)
    .execute(&pool)
    .await
    .expect("expire the worker lease using the database clock");
    let reclaimed = claim_transfer_for_execution(&pool, &transfer.transfer_id)
        .await
        .expect("expired lease is reclaimable")
        .expect("the transfer can be recovered");
    assert_ne!(retry.lease_token, reclaimed.lease_token);
    assert!(
        !finish_execution_lease(&pool, &transfer.transfer_id, &retry.lease_token)
            .await
            .expect("stale worker cannot clear the new lease")
    );
    assert!(
        finish_execution_lease(&pool, &transfer.transfer_id, &reclaimed.lease_token)
            .await
            .expect("current worker clears its own lease")
    );
    let failure_count: i32 = sqlx::query_scalar(
        "SELECT execution_attempt_count FROM billing_transfer_intents WHERE transfer_id = $1",
    )
    .bind(&transfer.transfer_id)
    .fetch_one(&pool)
    .await
    .expect("read reset failure count");
    assert_eq!(
        failure_count, 0,
        "successful progress resets consecutive failures"
    );

    let rejected_attempt = claim_transfer_for_execution(&pool, &transfer.transfer_id)
        .await
        .expect("claim provider step")
        .expect("transfer remains due after lease completion");
    release_execution_lease(
        &pool,
        &transfer.transfer_id,
        &rejected_attempt.lease_token,
        TransferExecutionFailure::ProviderRejected,
    )
    .await
    .expect("park a definitively rejected provider step for review");
    sqlx::query(
        "UPDATE billing_transfer_intents SET execution_next_attempt_at = now() WHERE transfer_id = $1",
    )
    .bind(&transfer.transfer_id)
    .execute(&pool)
    .await
    .expect("make the parked transfer otherwise due");
    assert!(claim_transfer_for_execution(&pool, &transfer.transfer_id)
        .await
        .expect("manual review blocks automatic polling")
        .is_none());
    let parked = sqlx::query(
        "SELECT execution_review_generation FROM billing_transfer_intents WHERE transfer_id = $1",
    )
    .bind(&transfer.transfer_id)
    .fetch_one(&pool)
    .await
    .expect("read review generation");
    let review_generation: i64 = parked
        .try_get("execution_review_generation")
        .expect("generation is present");
    let queue = list_manual_review_transfers(&pool, None, 50)
        .await
        .expect("read bounded review queue");
    assert!(queue
        .iter()
        .any(|item| item.transfer_id == transfer.transfer_id));
    let details = load_manual_review_transfer(&pool, &transfer.transfer_id)
        .await
        .expect("load manual review details")
        .expect("manual review is visible to the protected store API");
    assert_eq!(details.0.execution_review_generation, review_generation);
    assert!(details.1.is_empty());
    let (held, hold_decision) = record_operator_review_decision(
        &pool,
        &transfer.transfer_id,
        "billing-operator-test",
        TransferReviewAction::Hold,
        "awaiting_provider_check",
        review_generation,
        "review-hold-test-key",
    )
    .await
    .expect("operator can durably keep the transfer on hold");
    assert!(held.execution_manual_review_required);
    let (_, repeated_hold) = record_operator_review_decision(
        &pool,
        &transfer.transfer_id,
        "billing-operator-test",
        TransferReviewAction::Hold,
        "awaiting_provider_check",
        review_generation,
        "review-hold-test-key",
    )
    .await
    .expect("same hold replays idempotently");
    assert_eq!(hold_decision.decision_id, repeated_hold.decision_id);
    assert!(matches!(
        record_operator_review_decision(
            &pool,
            &transfer.transfer_id,
            "billing-operator-test",
            TransferReviewAction::Resume,
            "provider_checked",
            review_generation + 1,
            "stale-review-key",
        )
        .await,
        Err(TransferError::InvalidTransition)
    ));
    let (resumed, decision) = record_operator_review_decision(
        &pool,
        &transfer.transfer_id,
        "billing-operator-test",
        TransferReviewAction::Resume,
        "provider_checked",
        review_generation,
        "review-resume-test-key",
    )
    .await
    .expect("operator explicitly resumes the parked transfer");
    assert!(!resumed.execution_manual_review_required);
    assert_eq!(decision.review_generation, review_generation);
    let manual_retry = claim_transfer_for_execution(&pool, &transfer.transfer_id)
        .await
        .expect("approved transfer is due for ordinary worker polling")
        .expect("manual review is released only after explicit approval");
    release_execution_lease(
        &pool,
        &transfer.transfer_id,
        &manual_retry.lease_token,
        TransferExecutionFailure::ProviderRejected,
    )
    .await
    .expect("a new definitive failure returns the transfer to review");
    let parked_again = load_manual_review_transfer(&pool, &transfer.transfer_id)
        .await
        .expect("read renewed manual review")
        .expect("new failure is parked");
    assert_eq!(
        parked_again.0.execution_review_generation,
        review_generation + 1
    );
    let (old_resume_replay, old_resume) = record_operator_review_decision(
        &pool,
        &transfer.transfer_id,
        "billing-operator-test",
        TransferReviewAction::Resume,
        "provider_checked",
        review_generation,
        "review-resume-test-key",
    )
    .await
    .expect("an idempotent replay returns the original decision");
    assert!(old_resume_replay.execution_manual_review_required);
    assert_eq!(decision.decision_id, old_resume.decision_id);
    let mut tx = pool
        .begin()
        .await
        .expect("begin invoice evidence transaction");
    record_destination_paid(
        &mut tx,
        &transfer.transfer_id,
        "pi_manual_review_test",
        transfer.effective_from,
        transfer.effective_from + 30 * 24 * 60 * 60,
    )
    .await
    .expect("mismatched paid-period evidence parks without aborting the transaction");
    record_destination_paid(
        &mut tx,
        &transfer.transfer_id,
        "pi_manual_review_test",
        transfer.effective_from,
        transfer.effective_from + 30 * 24 * 60 * 60,
    )
    .await
    .expect("replaying the same invoice does not create another review generation");
    tx.commit().await.expect("commit invoice evidence");
    let evidence_changed = load_manual_review_transfer(&pool, &transfer.transfer_id)
        .await
        .expect("read changed evidence")
        .expect("mismatched evidence remains parked");
    let current_review_generation = evidence_changed.0.execution_review_generation;
    assert_eq!(current_review_generation, review_generation + 2);
    assert!(matches!(
        record_operator_review_decision(
            &pool,
            &transfer.transfer_id,
            "billing-operator-test",
            TransferReviewAction::Resume,
            "provider_checked",
            review_generation + 1,
            "stale-after-invoice-key",
        )
        .await,
        Err(TransferError::InvalidTransition)
    ));
    let (resumed_again, _) = record_operator_review_decision(
        &pool,
        &transfer.transfer_id,
        "billing-operator-test",
        TransferReviewAction::Resume,
        "retry_verified",
        current_review_generation,
        "review-resume-second-key",
    )
    .await
    .expect("new evidence needs a new decision");
    assert!(!resumed_again.execution_manual_review_required);
    let manual_retry = claim_transfer_for_execution(&pool, &transfer.transfer_id)
        .await
        .expect("claim the newly approved attempt")
        .expect("a current decision reopens the normal worker path");
    finish_execution_lease(&pool, &transfer.transfer_id, &manual_retry.lease_token)
        .await
        .expect("successful progress clears its lease");
    sqlx::query(
        "UPDATE billing_transfer_intents SET state = 'pending', result_code = NULL, \
         destination_payment_reference = NULL, destination_period_start = NULL, \
         destination_period_end = NULL, \
         execution_manual_review_required = FALSE, execution_review_generation = 0, \
         execution_attempt_count = 0, last_execution_error_code = NULL, \
         execution_next_attempt_at = now() WHERE transfer_id = $1",
    )
    .bind(&transfer.transfer_id)
    .execute(&pool)
    .await
    .expect("restore pending fixture for transition coverage");

    let sponsored_request = SponsoredSeatRequest {
        action: SponsoredSeatAction::Add,
        beneficiary_id: user.clone(),
        replacement_beneficiary_id: None,
        offer: BillingOffer::StandardMonthly,
        quote_version: 1,
        quote_expires_at_epoch: 1_800_001_000,
        effective_from: 1_800_000_001,
        effective_until: None,
        idempotency_key: "seat-change-during-transfer".into(),
    };
    let mut tx = pool.begin().await.expect("begin sponsored mutation fence");
    assert!(matches!(
        begin_operation(&mut tx, &org, &user, &sponsored_request, 1_800_000_001).await,
        Err(SponsoredBillingError::Transfer(
            TransferError::AlreadyInProgress
        ))
    ));
    tx.rollback()
        .await
        .expect("rollback sponsored mutation fence");

    let missing_beneficiary_request = SponsoredSeatRequest {
        beneficiary_id: format!("transfer-test-{fixture}-missing"),
        ..sponsored_request.clone()
    };
    let mut tx = pool.begin().await.expect("begin missing beneficiary check");
    assert!(matches!(
        begin_operation(
            &mut tx,
            &org,
            &user,
            &missing_beneficiary_request,
            1_800_000_001
        )
        .await,
        Err(SponsoredBillingError::Transfer(
            TransferError::BeneficiaryMissing
        ))
    ));
    tx.rollback()
        .await
        .expect("rollback missing beneficiary check");

    let pending_operation = format!("transfer-test-{fixture}-pending-operation");
    let pending_seat = format!("transfer-test-{fixture}-pending-seat");
    sqlx::query(
        "INSERT INTO billing_sponsored_operations \
         (operation_id, organization_id, actor_user_id, idempotency_key, request_hash, action, \
          beneficiary_id, offer, quote_version, quote_expires_at_epoch, effective_from, \
          provider_idempotency_key, state) \
         VALUES ($1, $2, $3, $1, $1, 'add', $3, 'standard_monthly', 1, 1800001000, 1802592000, $1, 'checkout_created')",
    )
    .bind(&pending_operation)
    .bind(&org)
    .bind(&user)
    .execute(&pool)
    .await
    .expect("seed an operation awaiting provider settlement");
    sqlx::query(
        "INSERT INTO billing_sponsored_seats \
         (seat_id, organization_id, beneficiary_id, offer, effective_from, state, operation_id) \
         VALUES ($1, $2, $3, 'standard_monthly', 1802592000, 'pending', $4)",
    )
    .bind(&pending_seat)
    .bind(&org)
    .bind(&user)
    .bind(&pending_operation)
    .execute(&pool)
    .await
    .expect("seed pending seat");
    let evidence = SponsoredProviderEvidence {
        customer_id: Some(format!("cus_transfer_{fixture}")),
        subscription_id: format!("sub_transfer_{fixture}"),
        schedule_id: None,
        checkout_session_id: Some(format!("cs_transfer_{fixture}")),
        payment_reference: format!("pi_transfer_{fixture}"),
        provider_item_id: Some(format!("si_transfer_{fixture}")),
    };
    let mut tx = pool.begin().await.expect("begin late provider settlement");
    assert!(matches!(
        complete_paid_checkout(&mut tx, &pending_operation, &evidence).await,
        Err(SponsoredBillingError::Transfer(
            TransferError::AlreadyInProgress
        ))
    ));
    tx.rollback().await.expect("rollback late settlement");
    let pending_state: String =
        sqlx::query_scalar("SELECT state FROM billing_sponsored_seats WHERE seat_id = $1")
            .bind(&pending_seat)
            .fetch_one(&pool)
            .await
            .expect("load pending seat after blocked settlement");
    assert_eq!(pending_state, "pending");

    let mut tx = pool.begin().await.expect("begin replay");
    let replay = begin_transfer(&mut tx, &request, 1_800_000_001)
        .await
        .expect("replay transfer");
    tx.commit().await.expect("commit replay");
    let replayed = match replay {
        BeginTransfer::AlreadyExists(transfer) => transfer,
        BeginTransfer::Created(_) => panic!("replay created another transfer"),
    };
    assert_eq!(replayed.transfer_id, transfer.transfer_id);

    let mut conflicting = request.clone();
    conflicting.effective_from += 1;
    let mut tx = pool.begin().await.expect("begin conflict");
    assert!(matches!(
        begin_transfer(&mut tx, &conflicting, 1_800_000_000).await,
        Err(TransferError::ResultConflict)
    ));
    tx.rollback().await.expect("rollback conflict");

    let mut tx = pool.begin().await.expect("begin prepare");
    record_destination_prepared(
        &mut tx,
        &transfer.transfer_id,
        "destination-operation",
        Some("sub_destination"),
    )
    .await
    .expect("record destination preparation");
    begin_source_adjustment(&mut tx, &transfer.transfer_id, "source-operation")
        .await
        .expect("begin source adjustment");
    complete_source_adjustment(&mut tx, &transfer.transfer_id, "credit_source")
        .await
        .expect("record verified source schedule while destination invoice is pending");
    let awaiting_invoice: String =
        sqlx::query_scalar("SELECT state FROM billing_transfer_intents WHERE transfer_id = $1")
            .bind(&transfer.transfer_id)
            .fetch_one(&mut *tx)
            .await
            .expect("read pending transfer state");
    assert_eq!(
        awaiting_invoice,
        TransferState::SourceAdjustmentPending.as_str()
    );
    let expected_destination_period_end =
        FoundingDate::anchored_term_end_epoch(transfer.effective_from, FoundingOffer::Monthly, 1)
            .expect("calculate the quoted monthly destination term");
    sqlx::query(
        "UPDATE billing_transfer_intents SET destination_expected_period_end = $2 \
         WHERE transfer_id = $1",
    )
    .bind(&transfer.transfer_id)
    .bind(expected_destination_period_end)
    .execute(&mut *tx)
    .await
    .expect("persist the destination period read back from Stripe");
    let payer_before_invoice: (String, String) = sqlx::query_as(
        "SELECT payer_kind, payer_id FROM billing_founding_awards WHERE award_id = $1",
    )
    .bind(&award)
    .fetch_one(&mut *tx)
    .await
    .expect("founding payer stays unchanged before paid invoice");
    assert_eq!(payer_before_invoice, ("personal".into(), user.clone()));
    record_destination_paid(
        &mut tx,
        &transfer.transfer_id,
        "pi_destination",
        transfer.effective_from,
        expected_destination_period_end,
    )
    .await
    .expect("record destination payment after source scheduling");
    assert!(matches!(
        record_destination_paid(
            &mut tx,
            &transfer.transfer_id,
            "pi_conflicting",
            transfer.effective_from,
            expected_destination_period_end,
        )
        .await,
        Err(TransferError::ResultConflict)
    ));
    assert!(matches!(
        record_destination_paid(
            &mut tx,
            &transfer.transfer_id,
            "pi_destination",
            transfer.effective_from,
            expected_destination_period_end + 1,
        )
        .await,
        Err(TransferError::ResultConflict)
    ));
    complete_source_adjustment(&mut tx, &transfer.transfer_id, "credit_source")
        .await
        .expect("complete local handover after both provider facts are verified");
    tx.commit().await.expect("commit transfer completion");

    let state: String =
        sqlx::query_scalar("SELECT state FROM billing_transfer_intents WHERE transfer_id = $1")
            .bind(&transfer.transfer_id)
            .fetch_one(&pool)
            .await
            .expect("load transfer state");
    assert_eq!(state, TransferState::Completed.as_str());
    let payer: (String, String) = sqlx::query_as(
        "SELECT payer_kind, payer_id FROM billing_founding_awards WHERE award_id = $1",
    )
    .bind(&award)
    .fetch_one(&pool)
    .await
    .expect("load founding payer");
    assert_eq!(payer, ("sponsor".into(), org.clone()));
    let schedule_job: (String, String, String) = sqlx::query_as(
        "SELECT payer_kind, payer_id, provider_subscription_id \
         FROM billing_founding_schedule_jobs WHERE award_id = $1",
    )
    .bind(&award)
    .fetch_one(&pool)
    .await
    .expect("founding transition follows the destination subscription");
    assert_eq!(
        schedule_job,
        ("sponsor".into(), org.clone(), "sub_destination".into())
    );

    let mut tx = pool.begin().await.expect("begin callback replay");
    record_destination_prepared(
        &mut tx,
        &transfer.transfer_id,
        "destination-operation",
        Some("sub_destination"),
    )
    .await
    .expect("matching preparation retry remains idempotent");
    record_destination_paid(
        &mut tx,
        &transfer.transfer_id,
        "pi_destination",
        transfer.effective_from,
        expected_destination_period_end,
    )
    .await
    .expect("matching payment retry remains idempotent");
    assert!(matches!(
        record_destination_paid(
            &mut tx,
            &transfer.transfer_id,
            "pi_other",
            transfer.effective_from,
            expected_destination_period_end,
        )
        .await,
        Err(TransferError::ResultConflict)
    ));
    assert!(matches!(
        record_destination_paid(
            &mut tx,
            &transfer.transfer_id,
            "pi_destination",
            transfer.effective_from,
            expected_destination_period_end + 1,
        )
        .await,
        Err(TransferError::ResultConflict)
    ));
    assert!(matches!(
        mark_failed(&mut tx, &transfer.transfer_id, "late_failure").await,
        Err(TransferError::InvalidTransition)
    ));
    tx.rollback().await.expect("rollback callback replay");

    sqlx::query(
        "UPDATE billing_transfer_intents SET state = 'source_adjustment_pending', \
             result_code = NULL, destination_payment_reference = NULL, \
             destination_period_start = NULL, destination_period_end = NULL, \
             execution_manual_review_required = FALSE, last_execution_error_code = NULL, \
             execution_next_attempt_at = now(), execution_lease_token = 'period-review-lease', \
             execution_lease_until = now() + interval '60 seconds' \
         WHERE transfer_id = $1",
    )
    .bind(&transfer.transfer_id)
    .execute(&pool)
    .await
    .expect("reset the fixture for an unexpected Stripe period");
    let mut tx = pool.begin().await.expect("begin provider period mismatch");
    let parked = record_destination_paid(
        &mut tx,
        &transfer.transfer_id,
        "pi_sponsor_period_mismatch",
        transfer.effective_from,
        expected_destination_period_end + 1,
    )
    .await
    .expect("a valid paid invoice must not fail billing ingest on a transfer mismatch");
    assert!(parked.execution_manual_review_required);
    assert_eq!(
        parked.last_execution_error_code.as_deref(),
        Some("destination_period_end_mismatch")
    );
    assert!(matches!(
        complete_source_adjustment(&mut tx, &transfer.transfer_id, "credit_source").await,
        Err(TransferError::InvalidTransition)
    ));
    assert_eq!(
        parked.destination_period_end,
        Some(expected_destination_period_end + 1)
    );
    assert_eq!(
        parked.destination_period_start,
        Some(transfer.effective_from)
    );
    tx.commit()
        .await
        .expect("commit the invoice period and review flag together");
    assert!(
        !defer_execution_lease(&pool, &transfer.transfer_id, "period-review-lease", 300,)
            .await
            .expect("invoice evidence revokes the in-flight worker lease")
    );
    let remains_parked: bool = sqlx::query_scalar(
        "SELECT execution_manual_review_required AND \
             execution_next_attempt_at = 'infinity'::timestamptz \
         FROM billing_transfer_intents WHERE transfer_id = $1",
    )
    .bind(&transfer.transfer_id)
    .fetch_one(&pool)
    .await
    .expect("read the sticky review fence after deferral");
    assert!(remains_parked);
    assert!(claim_transfer_for_execution(&pool, &transfer.transfer_id)
        .await
        .expect("normal worker never claims a transfer parked for review")
        .is_none());

    assert!(
        !finish_execution_lease(&pool, &transfer.transfer_id, "period-review-lease",)
            .await
            .expect("invoice evidence revokes the in-flight worker lease")
    );
    let remains_parked: bool = sqlx::query_scalar(
        "SELECT execution_manual_review_required AND \
             execution_next_attempt_at = 'infinity'::timestamptz \
         FROM billing_transfer_intents WHERE transfer_id = $1",
    )
    .bind(&transfer.transfer_id)
    .fetch_one(&pool)
    .await
    .expect("read the sticky review fence after finish");
    assert!(remains_parked);
    assert!(claim_transfer_for_execution(&pool, &transfer.transfer_id)
        .await
        .expect("normal worker remains excluded after finish")
        .is_none());

    sqlx::query(
        "UPDATE billing_transfer_intents SET destination_payment_reference = NULL, \
             destination_period_start = NULL, destination_period_end = NULL, \
             execution_manual_review_required = FALSE, \
             last_execution_error_code = NULL, execution_next_attempt_at = now(), \
             execution_lease_token = NULL, execution_lease_until = NULL \
         WHERE transfer_id = $1",
    )
    .bind(&transfer.transfer_id)
    .execute(&pool)
    .await
    .expect("reset the fixture for an unexpected invoice start");
    let mut tx = pool.begin().await.expect("begin transfer start mismatch");
    let parked = record_destination_paid(
        &mut tx,
        &transfer.transfer_id,
        "pi_sponsor_start_mismatch",
        transfer.effective_from + 1,
        expected_destination_period_end,
    )
    .await
    .expect("an invoice start mismatch must not reject the organisation invoice");
    assert!(parked.execution_manual_review_required);
    assert_eq!(
        parked.last_execution_error_code.as_deref(),
        Some("destination_period_start_mismatch")
    );
    assert_eq!(
        parked.destination_period_start,
        Some(transfer.effective_from + 1)
    );
    tx.commit()
        .await
        .expect("commit the start mismatch for operator review");
    assert!(claim_transfer_for_execution(&pool, &transfer.transfer_id)
        .await
        .expect("period-start mismatch also stays parked")
        .is_none());

    let review_generation = parked.execution_review_generation;
    let (resumed, _) = record_operator_review_decision(
        &pool,
        &transfer.transfer_id,
        "billing-operator-test",
        TransferReviewAction::Resume,
        "retry_period_check",
        review_generation,
        "resume-period-mismatch",
    )
    .await
    .expect("resume retries verification without accepting invoice evidence");
    assert!(!resumed.execution_manual_review_required);
    let retry = claim_transfer_for_execution(&pool, &transfer.transfer_id)
        .await
        .expect("claim resumed mismatch for revalidation")
        .expect("resumed mismatch is eligible for the ordinary worker");
    assert_eq!(
        retry.transfer.destination_period_start,
        Some(transfer.effective_from + 1)
    );
    assert!(repark_mismatched_destination_invoice(
        &pool,
        &transfer.transfer_id,
        &retry.lease_token,
    )
    .await
    .expect("repark the still-mismatched invoice"));
    let reparked = load_manual_review_transfer(&pool, &transfer.transfer_id)
        .await
        .expect("load reparked transfer")
        .expect("mismatched payment remains in manual review");
    assert_eq!(
        reparked.0.execution_review_generation,
        review_generation + 1
    );
    assert_eq!(
        reparked.0.destination_payment_reference.as_deref(),
        Some("pi_sponsor_start_mismatch")
    );
    assert_eq!(
        reparked.0.last_execution_error_code.as_deref(),
        Some("destination_period_start_mismatch")
    );
    assert!(claim_transfer_for_execution(&pool, &transfer.transfer_id)
        .await
        .expect("reparked invoice stays out of automatic claims")
        .is_none());

    cleanup(&pool, &fixture).await;
}

#[tokio::test]
async fn concurrent_live_transfer_is_rejected_and_personal_actor_is_required() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let fixture = Uuid::new_v4().to_string();
    let user = format!("transfer-test-{fixture}-user");
    let other = format!("transfer-test-{fixture}-other");
    let org = format!("transfer-test-{fixture}-org");
    let operation = format!("transfer-test-{fixture}-operation");
    cleanup(&pool, &fixture).await;
    sqlx::query(
        "INSERT INTO users (id, oauth_provider, oauth_subject) VALUES
         ($1, 'transfer-test', $1), ($2, 'transfer-test', $2)",
    )
    .bind(&user)
    .bind(&other)
    .execute(&pool)
    .await
    .expect("insert users");
    sqlx::query("INSERT INTO organizations (id, enc_name, created_by) VALUES ($1, decode('6f7267', 'hex'), $2)")
        .bind(&org)
        .bind(&other)
        .execute(&pool)
        .await
        .expect("insert organisation");
    sqlx::query(
        "INSERT INTO organization_memberships (org_id, user_id, role) VALUES
         ($1, $2, 'owner'), ($1, $3, 'member')",
    )
    .bind(&org)
    .bind(&other)
    .bind(&user)
    .execute(&pool)
    .await
    .expect("insert owner");
    sqlx::query(
        "INSERT INTO billing_sponsored_subscriptions \
         (organization_id, provider_customer_id, provider_subscription_id, status, \
          current_period_start, current_period_end) \
         VALUES ($1, $2, $3, 'active', 1800000000, 1802592000)",
    )
    .bind(&org)
    .bind(format!("cus_transfer_destination_{fixture}"))
    .bind(format!("sub_transfer_destination_{fixture}"))
    .execute(&pool)
    .await
    .expect("insert destination sponsor subscription at the source boundary");
    sqlx::query(
        "INSERT INTO billing_operations
         (operation_id, idempotency_key, request_hash, actor_user_id, payer_id, beneficiary_id,
          offer, quote_version, quote_expires_at_epoch, provider_idempotency_key)
         VALUES ($1, $1, $1, $2, $2, $2, 'standard_monthly', 1, 1800001000, $1)",
    )
    .bind(&operation)
    .bind(&user)
    .execute(&pool)
    .await
    .expect("insert source operation");
    sqlx::query(
        "INSERT INTO billing_personal_accounts
         (user_id, operation_id, offer, stripe_subscription_id, state, pending_expires_at_epoch,
          paid_through_epoch, paid_through_date)
         VALUES ($1, $2, 'standard_monthly', $3, 'active', 1900000000, $4,
                 to_timestamp($4)::date)",
    )
    .bind(&user)
    .bind(&operation)
    .bind(format!("sub_transfer_source_{fixture}"))
    .bind(1_802_592_000_i64)
    .execute(&pool)
    .await
    .expect("insert active account");

    let mut request = personal_to_sponsor_request(&user, &user, &org, "live-transfer");
    request.counterparty_user_id = other.clone();
    request.offer = BillingOffer::StandardMonthly;
    let mut stale_boundary = request.clone();
    stale_boundary.idempotency_key = "stale-renewal-boundary".into();
    stale_boundary.effective_from += 1;
    let mut tx = pool.begin().await.expect("begin stale boundary");
    assert!(matches!(
        begin_transfer(&mut tx, &stale_boundary, 1_800_000_000).await,
        Err(TransferError::SourceTermChanged)
    ));
    tx.rollback().await.expect("rollback stale boundary");

    let mut tx = pool.begin().await.expect("begin first transfer");
    let first = begin_transfer(&mut tx, &request, 1_800_000_000)
        .await
        .expect("create first transfer");
    let first_id = match first {
        BeginTransfer::Created(intent) => {
            assert_eq!(intent.state, TransferState::AwaitingConsent);
            intent.transfer_id
        }
        BeginTransfer::AlreadyExists(_) => panic!("first transfer unexpectedly existed"),
    };
    assert!(matches!(
        record_destination_prepared(&mut tx, &first_id, "too-early", Some("sub_destination")).await,
        Err(TransferError::ResultConflict)
    ));
    assert!(matches!(
        accept_transfer(&mut tx, &first_id, &user, 1_800_000_000).await,
        Err(TransferError::Unauthorised)
    ));
    assert!(matches!(
        lock_and_check_no_live_transfer(&mut tx, &[&user], 1_800_000_000).await,
        Err(TransferError::InvitationPending)
    ));
    lock_and_check_no_provider_transfer_except(&mut tx, &[&user], &[], 1_800_000_000)
        .await
        .expect("provider webhooks must continue during consent");
    accept_transfer(&mut tx, &first_id, &other, 1_800_000_000)
        .await
        .expect("counterparty accepts first transfer");
    tx.commit().await.expect("commit first transfer");

    let mut tx = pool.begin().await.expect("begin mutation fence check");
    assert!(matches!(
        lock_and_check_no_live_transfer(&mut tx, &[&user], 1_800_000_000).await,
        Err(TransferError::AlreadyInProgress)
    ));
    assert!(matches!(
        lock_and_check_no_provider_transfer_except(&mut tx, &[&user], &[], 1_800_000_000).await,
        Err(TransferError::AlreadyInProgress)
    ));
    tx.rollback().await.expect("rollback mutation fence check");

    let mut second = request.clone();
    second.idempotency_key = "live-transfer-second".into();
    let mut tx = pool.begin().await.expect("begin second transfer");
    assert!(matches!(
        begin_transfer(&mut tx, &second, 1_800_000_000).await,
        Err(TransferError::AlreadyInProgress)
    ));
    tx.rollback().await.expect("rollback second transfer");

    let mut unauthorised = request;
    unauthorised.actor_user_id = other.clone();
    unauthorised.counterparty_user_id = other;
    unauthorised.idempotency_key = "unauthorised-transfer".into();
    let mut tx = pool.begin().await.expect("begin unauthorised transfer");
    assert!(matches!(
        begin_transfer(&mut tx, &unauthorised, 1_800_000_000).await,
        Err(TransferError::Unauthorised)
    ));
    tx.rollback().await.expect("rollback unauthorised transfer");

    cleanup(&pool, &fixture).await;
}

#[tokio::test]
async fn source_restoration_keeps_beneficiary_fenced_until_provider_confirmation() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let fixture = Uuid::new_v4().to_string();
    let user = format!("transfer-test-{fixture}-user");
    let org = format!("transfer-test-{fixture}-org");
    let transfer_id = format!("transfer:test-{fixture}");
    cleanup(&pool, &fixture).await;

    sqlx::query(
        "INSERT INTO users (id, oauth_provider, oauth_subject) VALUES ($1, 'transfer-test', $1)",
    )
    .bind(&user)
    .execute(&pool)
    .await
    .expect("insert beneficiary");
    sqlx::query(
        "INSERT INTO organizations (id, enc_name, created_by) VALUES ($1, decode('6f7267', 'hex'), $2)",
    )
    .bind(&org)
    .bind(&user)
    .execute(&pool)
    .await
    .expect("insert destination organisation");
    sqlx::query(
        "INSERT INTO billing_transfer_intents \
         (transfer_id, actor_user_id, counterparty_user_id, beneficiary_id, source_kind, \
          destination_kind, destination_organization_id, offer, quote_version, \
          quote_expires_at_epoch, consent_expires_at_epoch, effective_from, idempotency_key, request_hash, \
          provider_idempotency_key, source_provider_subscription_id, \
          destination_provider_subscription_id, destination_operation_id, source_operation_id, \
          source_adjustment_reference, state) \
         VALUES ($1, $2, $2, $2, 'personal', 'sponsor', $3, 'standard_monthly', 1, \
                 2000000000, 2000000000, 2000000100, $1, $1, $1, 'sub_source', 'sub_destination', \
                 'op_destination', 'op_source', 'schedule_source', 'source_adjustment_pending')",
    )
    .bind(&transfer_id)
    .bind(&user)
    .bind(&org)
    .execute(&pool)
    .await
    .expect("insert transfer after source schedule adjustment");

    let mut tx = pool
        .begin()
        .await
        .expect("begin source restoration request");
    let restoration =
        request_source_restoration(&mut tx, &transfer_id, "destination_initial_payment_failed")
            .await
            .expect("begin compensating source restoration");
    assert_eq!(restoration.state, TransferState::SourceRestorationPending);
    assert_eq!(
        request_source_restoration(&mut tx, &transfer_id, "destination_initial_payment_failed")
            .await
            .expect("repeat restoration request is idempotent")
            .state,
        TransferState::SourceRestorationPending
    );
    tx.commit()
        .await
        .expect("persist restoration pending state");

    let mut tx = pool.begin().await.expect("begin fenced billing mutation");
    assert!(matches!(
        lock_and_check_no_live_transfer(&mut tx, &[&user], 1_800_000_001).await,
        Err(TransferError::AlreadyInProgress)
    ));
    tx.rollback().await.expect("rollback fenced mutation");

    let mut tx = pool
        .begin()
        .await
        .expect("begin provider restoration result");
    assert!(matches!(
        complete_source_restoration(&mut tx, &transfer_id, "schedule_source_restored").await,
        Err(TransferError::InvalidTransition)
    ));
    let operation_id = format!("{transfer_id}:source-restoration");
    begin_source_restoration_payment(&mut tx, &transfer_id, &operation_id)
        .await
        .expect("persist stable recovery operation");
    record_source_restoration_subscription(&mut tx, &transfer_id, &operation_id, "sub_restored")
        .await
        .expect("bind replacement subscription");
    record_source_restoration_payment(
        &mut tx,
        &transfer_id,
        &operation_id,
        "sub_restored",
        "pi_restoration_paid",
    )
    .await
    .expect("record paid recovery invoice");
    let failed = complete_source_restoration(&mut tx, &transfer_id, "schedule_source_restored")
        .await
        .expect("record verified source restoration");
    assert_eq!(failed.state, TransferState::Failed);
    assert_eq!(
        failed.result_code.as_deref(),
        Some("destination_initial_payment_failed")
    );
    assert_eq!(
        failed.source_restoration_reference.as_deref(),
        Some("schedule_source_restored")
    );
    assert_eq!(
        failed.source_restoration_payment_reference.as_deref(),
        Some("pi_restoration_paid")
    );
    assert!(matches!(
        complete_source_restoration(&mut tx, &transfer_id, "different_restoration").await,
        Err(TransferError::ResultConflict)
    ));
    assert_eq!(
        complete_source_restoration(&mut tx, &transfer_id, "schedule_source_restored")
            .await
            .expect("repeat restoration confirmation is idempotent")
            .state,
        TransferState::Failed
    );
    tx.commit().await.expect("persist failed transfer");

    let mut tx = pool
        .begin()
        .await
        .expect("begin mutation after restoration");
    lock_and_check_no_live_transfer(&mut tx, &[&user], 1_800_000_001)
        .await
        .expect("verified restoration releases the beneficiary fence");
    tx.rollback().await.expect("finish mutation check");
    cleanup(&pool, &fixture).await;
}

#[tokio::test]
async fn sponsored_admin_and_beneficiary_can_authorise_leaving_a_sponsorship() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let fixture = Uuid::new_v4().to_string();
    let beneficiary = format!("transfer-test-{fixture}-beneficiary");
    let admin = format!("transfer-test-{fixture}-admin");
    let org = format!("transfer-test-{fixture}-source-org");
    let operation = format!("transfer-test-{fixture}-sponsored-operation");
    cleanup(&pool, &fixture).await;

    sqlx::query(
        "INSERT INTO users (id, oauth_provider, oauth_subject) VALUES
         ($1, 'transfer-test', $1), ($2, 'transfer-test', $2)",
    )
    .bind(&beneficiary)
    .bind(&admin)
    .execute(&pool)
    .await
    .expect("insert transfer users");
    sqlx::query(
        "INSERT INTO organizations (id, enc_name, created_by) VALUES ($1, decode('6f7267', 'hex'), $2)",
    )
    .bind(&org)
    .bind(&admin)
    .execute(&pool)
    .await
    .expect("insert source organisation");
    sqlx::query(
        "INSERT INTO organization_memberships (org_id, user_id, role) VALUES
         ($1, $2, 'owner'), ($1, $3, 'member')",
    )
    .bind(&org)
    .bind(&admin)
    .bind(&beneficiary)
    .execute(&pool)
    .await
    .expect("insert source memberships");
    sqlx::query(
        "INSERT INTO billing_sponsored_subscriptions
         (organization_id, provider_customer_id, provider_subscription_id, status,
          current_period_start, current_period_end)
         VALUES ($1, $2, $3, 'active', 1800000000, 1802592000)",
    )
    .bind(&org)
    .bind(format!("cus_transfer_source_{fixture}"))
    .bind(format!("sub_transfer_source_{fixture}"))
    .execute(&pool)
    .await
    .expect("insert source sponsored subscription");
    sqlx::query(
        "INSERT INTO billing_sponsored_operations
         (operation_id, organization_id, actor_user_id, idempotency_key, request_hash, action,
          beneficiary_id, offer, quote_version, quote_expires_at_epoch, effective_from,
          provider_idempotency_key, state, result_code)
         VALUES ($1, $2, $3, $1, $1, 'add', $4, 'standard_monthly', 1, 1800001000,
                 1800000000, $1, 'active', 'active')",
    )
    .bind(&operation)
    .bind(&org)
    .bind(&admin)
    .bind(&beneficiary)
    .execute(&pool)
    .await
    .expect("insert source operation");
    sqlx::query(
        "INSERT INTO billing_sponsored_seats
         (seat_id, organization_id, beneficiary_id, offer, effective_from, state, operation_id)
         VALUES ($1, $2, $3, 'standard_monthly', 1800000000, 'active', $4)",
    )
    .bind(format!("transfer-test-{fixture}-seat"))
    .bind(&org)
    .bind(&beneficiary)
    .bind(&operation)
    .execute(&pool)
    .await
    .expect("insert active source seat");

    sqlx::query(
        "UPDATE billing_sponsored_seats SET effective_until = 1802591999 \
         WHERE operation_id = $1",
    )
    .bind(&operation)
    .execute(&pool)
    .await
    .expect("seed a seat that ends before the paid-through boundary");
    let request = TransferRequest {
        actor_user_id: admin.clone(),
        counterparty_user_id: beneficiary.clone(),
        beneficiary_id: beneficiary.clone(),
        source_kind: TransferPayer::Sponsor,
        source_organization_id: Some(org),
        source_provider_subscription_id: format!("sub_transfer_source_{fixture}"),
        destination_kind: TransferPayer::Personal,
        destination_organization_id: None,
        offer: BillingOffer::StandardMonthly,
        quote_version: 1,
        quote_expires_at_epoch: 1_800_001_000,
        effective_from: 1_802_592_000,
        effective_until: None,
        idempotency_key: "leave-sponsor".into(),
    };
    let mut tx = pool.begin().await.expect("begin transfer from short seat");
    assert!(matches!(
        begin_transfer(&mut tx, &request, 1_800_000_000).await,
        Err(TransferError::SourceInactive)
    ));
    tx.rollback()
        .await
        .expect("rollback transfer from short seat");
    sqlx::query(
        "UPDATE billing_sponsored_seats SET effective_until = 1802592000 \
         WHERE operation_id = $1",
    )
    .bind(&operation)
    .execute(&pool)
    .await
    .expect("extend source seat to the paid-through boundary");

    let mut tx = pool.begin().await.expect("begin sponsored transfer");
    let outcome = begin_transfer(&mut tx, &request, 1_800_000_000)
        .await
        .expect("authorised sponsored-to-personal transfer");
    let transfer_id = match outcome {
        BeginTransfer::Created(intent) => {
            assert_eq!(intent.state, TransferState::AwaitingConsent);
            let transfer_id = intent.transfer_id;
            accept_transfer(&mut tx, &transfer_id, &beneficiary, 1_800_000_000)
                .await
                .expect("beneficiary accepts sponsored transfer");
            tx.commit()
                .await
                .expect("commit transfer while Setup Checkout is incomplete");
            assert!(claim_transfer_for_execution(&pool, &transfer_id)
                .await
                .expect("worker leaves setup waiting for the beneficiary")
                .is_none());
            let attempts: i32 = sqlx::query_scalar(
                "SELECT execution_attempt_count FROM billing_transfer_intents WHERE transfer_id = $1",
            )
            .bind(&transfer_id)
            .fetch_one(&pool)
            .await
            .expect("read setup waiting attempts");
            assert_eq!(attempts, 0, "waiting for setup must not consume retries");
            let mut tx = pool.begin().await.expect("resume setup completion");
            sqlx::query(
                "UPDATE billing_transfer_intents SET destination_customer_id = $2, \
                     destination_setup_session_id = $3 WHERE transfer_id = $1",
            )
            .bind(&transfer_id)
            .bind("cus_transfer_destination")
            .bind("cs_transfer_setup")
            .execute(&mut *tx)
            .await
            .expect("bind beneficiary setup session");
            record_destination_setup_completion(
                &mut tx,
                &transfer_id,
                "cus_transfer_destination",
                "cs_transfer_setup",
                "seti_transfer_setup",
            )
            .await
            .expect("record verified beneficiary setup");
            assert!(matches!(
                record_destination_setup_completion(
                    &mut tx,
                    &transfer_id,
                    "cus_transfer_destination",
                    "cs_transfer_setup",
                    "seti_other",
                )
                .await,
                Err(TransferError::ResultConflict)
            ));
            record_destination_schedule(
                &mut tx,
                &transfer_id,
                &transfer_id,
                "cus_transfer_destination",
                "sub_sched_transfer",
                None,
                Some("seti_transfer_setup"),
                FoundingDate::anchored_term_end_epoch(1_802_592_000, FoundingOffer::Monthly, 1)
                    .expect("expected monthly destination term"),
            )
            .await
            .expect("record verified future personal schedule");
            record_destination_schedule(
                &mut tx,
                &transfer_id,
                &transfer_id,
                "cus_transfer_destination",
                "sub_sched_transfer",
                None,
                Some("seti_transfer_setup"),
                FoundingDate::anchored_term_end_epoch(1_802_592_000, FoundingOffer::Monthly, 1)
                    .expect("expected monthly destination term"),
            )
            .await
            .expect("replay the same destination schedule");
            assert!(matches!(
                record_destination_schedule(
                    &mut tx,
                    &transfer_id,
                    &transfer_id,
                    "cus_transfer_destination",
                    "sub_sched_other",
                    None,
                    Some("seti_transfer_setup"),
                FoundingDate::anchored_term_end_epoch(
                    1_802_592_000,
                    FoundingOffer::Monthly,
                    1,
                )
                .expect("expected monthly destination term"),
                )
                .await,
                Err(TransferError::ResultConflict)
            ));
            transfer_id
        }
        BeginTransfer::AlreadyExists(_) => panic!("transfer unexpectedly existed"),
    };
    let mut tx = pool.begin().await.expect("begin transfer cleanup");
    mark_failed(&mut tx, &transfer_id, "test_abort")
        .await
        .expect("abort uncommitted provider step");
    tx.commit().await.expect("commit sponsored transfer");

    let mut expired = request.clone();
    expired.idempotency_key = "expired-invitation".into();
    let mut tx = pool.begin().await.expect("begin expired invitation");
    let expired_id = match begin_transfer(&mut tx, &expired, 1_800_000_000)
        .await
        .expect("create expired invitation")
    {
        BeginTransfer::Created(intent) => intent.transfer_id,
        BeginTransfer::AlreadyExists(_) => panic!("expired invitation unexpectedly existed"),
    };
    tx.commit().await.expect("commit expired invitation");
    sqlx::query(
        "UPDATE billing_transfer_intents SET consent_expires_at_epoch = $2 \
         WHERE transfer_id = $1",
    )
    .bind(&expired_id)
    .bind(1_800_001_000_i64)
    .execute(&pool)
    .await
    .expect("expire the invitation consent window");

    let mut fresh = request;
    fresh.idempotency_key = "fresh-invitation".into();
    fresh.quote_expires_at_epoch = 1_800_002_000;
    fresh.effective_from = 1_802_592_000;
    let mut tx = pool.begin().await.expect("begin fresh invitation");
    let fresh_id = match begin_transfer(&mut tx, &fresh, 1_800_001_001)
        .await
        .expect("expired invitation should release the slot")
    {
        BeginTransfer::Created(intent) => {
            assert_eq!(intent.state, TransferState::AwaitingConsent);
            intent.transfer_id
        }
        BeginTransfer::AlreadyExists(_) => panic!("fresh invitation unexpectedly existed"),
    };
    let expired_state: (String, String) = sqlx::query_as(
        "SELECT state, result_code FROM billing_transfer_intents WHERE transfer_id = $1",
    )
    .bind(&expired_id)
    .fetch_one(&mut *tx)
    .await
    .expect("read expired invitation");
    assert_eq!(expired_state, ("failed".into(), "consent_expired".into()));
    let withdrawn = withdraw_transfer(&mut tx, &fresh_id, &admin, 1_800_001_001)
        .await
        .expect("actor can withdraw invitation");
    assert_eq!(withdrawn.state, TransferState::Failed);
    assert_eq!(withdrawn.result_code.as_deref(), Some("consent_withdrawn"));
    tx.commit().await.expect("commit withdrawn invitation");
    cleanup(&pool, &fixture).await;
}
