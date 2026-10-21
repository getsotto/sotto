use std::str::FromStr;

use sqlx::{postgres::PgConnectOptions, PgPool};
use uuid::Uuid;

use sotto_server::billing_refunds::{
    begin_provider_refund, claim_or_park_provider_refund_create, confirm_early_termination,
    create_request, park_ambiguous_refund_create, provider_refund_create_claim_is_current,
    record_provider_pending, record_provider_refund, record_provider_termination,
    record_provider_unknown, refund_create_replay_allowed, review_request, BillingRefundError,
    CorrectionReason, CorrectionRequest, CorrectionState, PayerKind, RefundCreateClaim,
    RequestDisposition, REFUND_CREATE_REPLAY_WINDOW_SECONDS,
};
use sotto_server::db;

async fn pool_or_skip() -> Option<PgPool> {
    if std::env::var("SOTTO_RUN_DB_TESTS").as_deref() != Ok("1") {
        eprintln!("skipping billing refund tests: SOTTO_RUN_DB_TESTS=1 not set");
        return None;
    }
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL for opted-in database tests");
    let options = PgConnectOptions::from_str(&url).expect("parse DATABASE_URL");
    assert!(
        matches!(options.get_host(), "localhost" | "127.0.0.1" | "::1"),
        "refusing billing refund tests against non-local host: {}",
        options.get_host()
    );
    let pool = db::connect(&url).await.expect("connect to test database");
    db::migrate(&pool).await.expect("apply migrations");
    Some(pool)
}

async fn cleanup(pool: &PgPool, fixture: &str) {
    let pattern = format!("billing-refund-test-{fixture}-%");
    sqlx::query("DELETE FROM billing_correction_requests WHERE requester_user_id LIKE $1")
        .bind(&pattern)
        .execute(pool)
        .await
        .expect("clean correction requests");
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
    sqlx::query("DELETE FROM users WHERE id LIKE $1")
        .bind(&pattern)
        .execute(pool)
        .await
        .expect("clean users");
}

async fn seed_personal_account(pool: &PgPool, user: &str, operation: &str) {
    sqlx::query(
        "INSERT INTO users (id, oauth_provider, oauth_subject) VALUES ($1, 'refund-test', $1)",
    )
    .bind(user)
    .execute(pool)
    .await
    .expect("insert refund user");
    sqlx::query(
        "INSERT INTO billing_operations
         (operation_id, idempotency_key, request_hash, actor_user_id, payer_id, beneficiary_id,
          offer, quote_version, quote_expires_at_epoch, provider_idempotency_key)
         VALUES ($1, $1, $1, $2, $2, $2, 'standard_monthly', 1, 1900001000, $1)",
    )
    .bind(operation)
    .bind(user)
    .execute(pool)
    .await
    .expect("insert refund operation");
    sqlx::query(
        "INSERT INTO billing_personal_accounts
         (user_id, operation_id, offer, stripe_customer_id, stripe_subscription_id, state,
          pending_expires_at_epoch, paid_through_epoch, paid_through_date, payment_reference)
         VALUES ($1, $2, 'standard_monthly', $3, $4, 'active', 1900000000, 1901000000,
                 '2030-03-02', $5)",
    )
    .bind(user)
    .bind(operation)
    .bind(format!("cus_{user}"))
    .bind(format!("sub_{user}"))
    .bind(format!("pi_{user}"))
    .execute(pool)
    .await
    .expect("insert paid personal account");
}

fn request(user: &str, key: &str, full_refund_requested: bool) -> CorrectionRequest {
    CorrectionRequest {
        requester_user_id: user.into(),
        beneficiary_id: user.into(),
        organization_id: None,
        payer_kind: PayerKind::Personal,
        payment_reference: format!("pi_{user}"),
        subscription_id: format!("sub_{user}"),
        amount_pence: (!full_refund_requested).then_some(299),
        reason: CorrectionReason::BillingError,
        policy_version: "2026-10-04".into(),
        idempotency_key: key.into(),
        full_refund_requested,
    }
}

#[tokio::test]
async fn correction_replays_preserve_terms_until_confirmed_refund_and_early_end() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let fixture = Uuid::new_v4().to_string();
    let user = format!("billing-refund-test-{fixture}-user");
    let operation = format!("billing-refund-test-{fixture}-operation");
    cleanup(&pool, &fixture).await;
    seed_personal_account(&pool, &user, &operation).await;

    let mut tx = pool.begin().await.expect("begin correction");
    let mut missing_amount = request(&user, "missing-amount", false);
    missing_amount.amount_pence = None;
    assert!(matches!(
        create_request(&mut tx, &missing_amount).await,
        Err(BillingRefundError::InvalidField("amount_pence"))
    ));
    let mut invalid_reference = request(&user, "invalid-reference", false);
    invalid_reference.payment_reference = "cs_session".into();
    assert!(matches!(
        create_request(&mut tx, &invalid_reference).await,
        Err(BillingRefundError::InvalidField("payment_reference"))
    ));
    let (disposition, partial) = create_request(&mut tx, &request(&user, "partial", false))
        .await
        .expect("create partial correction");
    assert_eq!(disposition, RequestDisposition::Created);
    assert!(partial.preserve_paid_term);
    assert!(matches!(
        confirm_early_termination(&mut tx, &partial.request_id, &user, 1_900_000_000).await,
        Err(BillingRefundError::InvalidTransition)
    ));
    let (_, partial_replay) = create_request(&mut tx, &request(&user, "partial", false))
        .await
        .expect("replay partial correction");
    assert_eq!(partial_replay.request_id, partial.request_id);
    review_request(&mut tx, &partial.request_id, true, None)
        .await
        .expect("approve partial correction");
    let first_attempt = begin_provider_refund(&mut tx, &partial.request_id)
        .await
        .expect("start partial refund");
    let attempt_replay = begin_provider_refund(&mut tx, &partial.request_id)
        .await
        .expect("replay provider-pending operation");
    assert_eq!(
        attempt_replay.provider_refund_started_at_epoch,
        first_attempt.provider_refund_started_at_epoch
    );
    let unknown = record_provider_unknown(
        &mut tx,
        &partial.request_id,
        "re_partial_pending",
        "provider_status_unrecognised",
    )
    .await
    .expect("persist an unrecognised provider result for later lookup");
    assert_eq!(unknown.state, CorrectionState::Unknown);
    assert!(matches!(
        record_provider_pending(&mut tx, &partial.request_id, "re_other_pending").await,
        Err(BillingRefundError::RequestConflict)
    ));
    record_provider_pending(&mut tx, &partial.request_id, "re_partial_pending")
        .await
        .expect("record a later pending read for the same refund id");
    let partial = record_provider_refund(
        &mut tx,
        &partial.request_id,
        "re_partial_pending",
        true,
        None,
    )
    .await
    .expect("record partial refund");
    assert_eq!(partial.state, CorrectionState::Refunded);
    tx.commit().await.expect("commit partial correction");
    let paid_through: i64 = sqlx::query_scalar(
        "SELECT paid_through_epoch FROM billing_personal_accounts WHERE user_id = $1",
    )
    .bind(&user)
    .fetch_one(&pool)
    .await
    .expect("read preserved paid term");
    assert_eq!(paid_through, 1_901_000_000);

    let mut tx = pool.begin().await.expect("begin early correction");
    let (_, early) = create_request(&mut tx, &request(&user, "early", true))
        .await
        .expect("create early correction");
    confirm_early_termination(&mut tx, &early.request_id, &user, 1_900_000_000)
        .await
        .expect("confirm early termination");
    review_request(&mut tx, &early.request_id, true, None)
        .await
        .expect("approve early correction");
    begin_provider_refund(&mut tx, &early.request_id)
        .await
        .expect("start early refund");
    let early = record_provider_refund(&mut tx, &early.request_id, "re_early", true, None)
        .await
        .expect("record early refund");
    assert_eq!(early.state, CorrectionState::TerminationPending);
    assert!(!early.preserve_paid_term);
    let replay = record_provider_refund(&mut tx, &early.request_id, "re_early", true, None)
        .await
        .expect("replay early refund");
    assert_eq!(replay.request_id, early.request_id);
    let early = record_provider_termination(&mut tx, &early.request_id)
        .await
        .expect("record early termination");
    assert_eq!(early.state, CorrectionState::Refunded);
    tx.commit().await.expect("commit early correction");

    let account: (String, i64) = sqlx::query_as(
        "SELECT state, paid_through_epoch FROM billing_personal_accounts WHERE user_id = $1",
    )
    .bind(&user)
    .fetch_one(&pool)
    .await
    .expect("read early-ended account");
    assert_eq!(account, ("canceled".into(), 1_900_000_000));
    cleanup(&pool, &fixture).await;
}

#[tokio::test]
async fn ambiguous_refund_create_parks_before_provider_idempotency_expires() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let fixture = Uuid::new_v4().to_string();
    let user = format!("billing-refund-test-{fixture}-user");
    let operation = format!("billing-refund-test-{fixture}-operation");
    cleanup(&pool, &fixture).await;
    seed_personal_account(&pool, &user, &operation).await;

    let mut tx = pool.begin().await.expect("begin ambiguous refund");
    let (_, request) = create_request(&mut tx, &request(&user, "ambiguous", false))
        .await
        .expect("create correction");
    review_request(&mut tx, &request.request_id, true, None)
        .await
        .expect("approve correction");
    let pending = begin_provider_refund(&mut tx, &request.request_id)
        .await
        .expect("persist first provider attempt before I/O");
    let first_attempt = pending
        .provider_refund_started_at_epoch
        .expect("first attempt timestamp is persisted");
    assert!(refund_create_replay_allowed(
        Some(first_attempt),
        first_attempt + REFUND_CREATE_REPLAY_WINDOW_SECONDS - 1
    ));
    assert!(!refund_create_replay_allowed(
        Some(first_attempt),
        first_attempt + REFUND_CREATE_REPLAY_WINDOW_SECONDS
    ));
    assert!(!refund_create_replay_allowed(None, first_attempt));
    tx.commit()
        .await
        .expect("commit provider-pending correction");

    let mut tx = pool.begin().await.expect("begin horizon parking");
    let parked = park_ambiguous_refund_create(
        &mut tx,
        &request.request_id,
        first_attempt + REFUND_CREATE_REPLAY_WINDOW_SECONDS,
    )
    .await
    .expect("park ambiguous outcome without a second payout");
    assert_eq!(parked.state, CorrectionState::Unknown);
    assert_eq!(
        parked.result_code.as_deref(),
        Some("refund_create_horizon_expired")
    );
    assert_eq!(parked.provider_refund_id, None);
    let replay = park_ambiguous_refund_create(
        &mut tx,
        &request.request_id,
        first_attempt + REFUND_CREATE_REPLAY_WINDOW_SECONDS,
    )
    .await
    .expect("parking is idempotent");
    assert_eq!(replay.request_id, parked.request_id);
    tx.commit().await.expect("commit parked correction");

    let account: (String, i64) = sqlx::query_as(
        "SELECT state, paid_through_epoch FROM billing_personal_accounts WHERE user_id = $1",
    )
    .bind(&user)
    .fetch_one(&pool)
    .await
    .expect("read account after ambiguous refund");
    assert_eq!(account, ("active".into(), 1_901_000_000));

    let mut tx = pool.begin().await.expect("begin operator resolution");
    let resolved = record_provider_pending(&mut tx, &request.request_id, "re_found_by_operator")
        .await
        .expect("accept only the refund id found and verified by the operator route");
    assert_eq!(resolved.state, CorrectionState::ProviderPending);
    assert_eq!(
        resolved.provider_refund_id.as_deref(),
        Some("re_found_by_operator")
    );
    assert_eq!(
        resolved.provider_refund_started_at_epoch,
        Some(first_attempt)
    );
    tx.commit().await.expect("commit verified refund reference");
    cleanup(&pool, &fixture).await;
}

#[tokio::test]
async fn refund_create_claim_serialises_posts_and_prevents_parking_in_flight() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let fixture = Uuid::new_v4().to_string();
    let user = format!("billing-refund-test-{fixture}-user");
    let operation = format!("billing-refund-test-{fixture}-operation");
    cleanup(&pool, &fixture).await;
    seed_personal_account(&pool, &user, &operation).await;

    let mut tx = pool.begin().await.expect("begin correction setup");
    let (_, request) = create_request(&mut tx, &request(&user, "claim-race", false))
        .await
        .expect("create correction");
    review_request(&mut tx, &request.request_id, true, None)
        .await
        .expect("approve correction");
    let pending = begin_provider_refund(&mut tx, &request.request_id)
        .await
        .expect("persist refund attempt");
    let first_attempt = pending
        .provider_refund_started_at_epoch
        .expect("first attempt time is stored");
    tx.commit().await.expect("commit correction setup");

    let first_token = Uuid::new_v4();
    let mut tx = pool.begin().await.expect("begin first claim");
    let first = claim_or_park_provider_refund_create(&mut tx, &request.request_id, first_token)
        .await
        .expect("claim the provider create");
    assert!(matches!(
        first,
        RefundCreateClaim::Claimed { token, .. } if token == first_token
    ));
    tx.commit().await.expect("commit first claim");

    let second_token = Uuid::new_v4();
    let mut tx = pool.begin().await.expect("begin competing claim");
    let second = claim_or_park_provider_refund_create(&mut tx, &request.request_id, second_token)
        .await
        .expect("observe the in-flight create");
    assert!(matches!(second, RefundCreateClaim::InFlight(_)));
    assert!(
        provider_refund_create_claim_is_current(&mut tx, &request.request_id, first_token)
            .await
            .expect("first token remains the current claim")
    );
    tx.commit().await.expect("commit competing observation");

    sqlx::query(
        "UPDATE billing_correction_requests \
         SET provider_refund_started_at_epoch = $2 \
         WHERE request_id = $1",
    )
    .bind(&request.request_id)
    .bind(first_attempt - REFUND_CREATE_REPLAY_WINDOW_SECONDS - 1)
    .execute(&pool)
    .await
    .expect("simulate the replay horizon expiring during a claimed request");
    let mut tx = pool
        .begin()
        .await
        .expect("begin expired-horizon observation");
    let still_in_flight =
        claim_or_park_provider_refund_create(&mut tx, &request.request_id, second_token)
            .await
            .expect("a live create claim prevents horizon parking");
    assert!(matches!(still_in_flight, RefundCreateClaim::InFlight(_)));
    assert!(
        !provider_refund_create_claim_is_current(&mut tx, &request.request_id, first_token)
            .await
            .expect("an expired original replay window no longer authorises a send")
    );
    tx.commit()
        .await
        .expect("commit expired-horizon observation");

    sqlx::query(
        "UPDATE billing_correction_requests \
         SET provider_refund_claimed_until = now() - interval '1 second' \
         WHERE request_id = $1",
    )
    .bind(&request.request_id)
    .execute(&pool)
    .await
    .expect("expire the create lease");
    let mut tx = pool.begin().await.expect("begin horizon parking");
    let parked = claim_or_park_provider_refund_create(&mut tx, &request.request_id, second_token)
        .await
        .expect("park after no provider create still owns a live claim");
    let RefundCreateClaim::Parked(parked) = parked else {
        panic!("expired ambiguous create should be parked");
    };
    assert_eq!(parked.state, CorrectionState::Unknown);
    assert_eq!(
        parked.result_code.as_deref(),
        Some("refund_create_horizon_expired")
    );
    tx.commit().await.expect("commit horizon parking");

    let account: (String, i64) = sqlx::query_as(
        "SELECT state, paid_through_epoch FROM billing_personal_accounts WHERE user_id = $1",
    )
    .bind(&user)
    .fetch_one(&pool)
    .await
    .expect("read account while refund outcome remains unknown");
    assert_eq!(account, ("active".into(), 1_901_000_000));
    cleanup(&pool, &fixture).await;
}
