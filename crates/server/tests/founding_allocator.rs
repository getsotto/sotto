//! Opt-in Postgres coverage for the founding-price allocator.

use std::str::FromStr;
use std::sync::OnceLock;

use sqlx::{postgres::PgConnectOptions, PgPool};
use tokio::sync::Mutex;

use sotto_server::billing_catalogue::BillingOffer;
use sotto_server::db;
use sotto_server::founding_allocator::{
    confirm_payment, load_capacity_status, quote_status, reserve, ConfirmationOutcome,
    FoundingDate, FoundingOffer, ReservationOutcome, FOUNDING_CAPACITY,
};

static TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

async fn pool_or_skip() -> Option<PgPool> {
    if std::env::var("SOTTO_RUN_DB_TESTS").as_deref() != Ok("1") {
        eprintln!("skipping founding allocator tests: set SOTTO_RUN_DB_TESTS=1");
        return None;
    }
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL for opted-in database tests");
    let options = PgConnectOptions::from_str(&url).expect("parse DATABASE_URL");
    assert!(
        matches!(options.get_host(), "localhost" | "127.0.0.1" | "::1"),
        "refusing founding allocator tests against non-local host: {}",
        options.get_host()
    );
    let pool = db::connect(&url).await.expect("connect to test database");
    db::migrate(&pool).await.expect("apply migrations");
    Some(pool)
}

async fn cleanup(pool: &PgPool, fixture: &str) {
    sqlx::query(
        "DELETE FROM billing_founding_reservations WHERE beneficiary_id LIKE $1 OR payer_id LIKE $1",
    )
    .bind(format!("founding-test-{fixture}-%"))
    .execute(pool)
    .await
    .expect("clean founding reservations");
    sqlx::query(
        "DELETE FROM billing_founding_awards WHERE beneficiary_id LIKE $1 OR payer_id LIKE $1",
    )
    .bind(format!("founding-test-{fixture}-%"))
    .execute(pool)
    .await
    .expect("clean founding awards");
}

async fn seed_awards(pool: &PgPool, fixture: &str, count: i64) {
    for ordinal in 1..=count {
        let id = format!("founding-test-{fixture}-award-{ordinal}");
        let person = format!("founding-test-{fixture}-person-{ordinal}");
        sqlx::query(
            "INSERT INTO billing_founding_awards
             (award_id, beneficiary_id, payer_id, offer, cohort_ordinal, original_start_date,
              original_end_date)
             VALUES ($1, $2, $2, 'founding_monthly', $3, '2026-01-01', '2026-02-01')",
        )
        .bind(id)
        .bind(person)
        .bind(ordinal)
        .execute(pool)
        .await
        .expect("seed founding award");
    }
}

#[tokio::test]
async fn reservation_is_idempotent_and_quote_exposes_standard_price() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let _guard = TEST_LOCK.get_or_init(|| Mutex::const_new(())).lock().await;
    let fixture = "idempotency";
    cleanup(&pool, fixture).await;

    let now = 1_800_000_000;
    let expires = now + 1_800;
    let mut tx = pool.begin().await.expect("begin reservation");
    let first = reserve(
        &mut tx,
        "founding-test-idempotency-reservation",
        "founding-test-idempotency-operation",
        "founding-test-idempotency-person",
        "founding-test-idempotency-payer",
        FoundingOffer::Monthly,
        1,
        expires,
        now,
    )
    .await
    .expect("create reservation");
    assert!(matches!(first, ReservationOutcome::Created(_)));
    let replay = reserve(
        &mut tx,
        "founding-test-idempotency-different-id",
        "founding-test-idempotency-operation",
        "founding-test-idempotency-person",
        "founding-test-idempotency-payer",
        FoundingOffer::Monthly,
        1,
        expires,
        now,
    )
    .await
    .expect("replay reservation");
    assert!(matches!(replay, ReservationOutcome::AlreadyExists(_)));
    let quote = quote_status(&mut tx, FoundingOffer::Monthly, now)
        .await
        .expect("read quote status");
    assert_eq!(quote.remaining_places, FOUNDING_CAPACITY - 1);
    assert_eq!(quote.founding_amount_pence, 199);
    assert_eq!(quote.standard_amount_pence, 299);
    let expired_replay = reserve(
        &mut tx,
        "founding-test-idempotency-expired-replay-id",
        "founding-test-idempotency-operation",
        "founding-test-idempotency-person",
        "founding-test-idempotency-payer",
        FoundingOffer::Monthly,
        1,
        expires,
        expires + 1,
    )
    .await
    .expect("replay expired reservation");
    assert!(matches!(
        expired_replay,
        ReservationOutcome::AlreadyExists(_)
    ));
    tx.rollback().await.expect("rollback fixture");
    cleanup(&pool, fixture).await;
}

#[tokio::test]
async fn final_live_reservation_can_settle_and_concurrent_claims_cannot_exceed_capacity() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let _guard = TEST_LOCK.get_or_init(|| Mutex::const_new(())).lock().await;
    let fixture = "capacity";
    cleanup(&pool, fixture).await;
    seed_awards(&pool, fixture, FOUNDING_CAPACITY - 1).await;

    let now = 1_800_000_000;
    let expires = now + 1_800;
    let pool_a = pool.clone();
    let pool_b = pool.clone();
    let reserve_a = tokio::spawn(async move {
        let mut tx = pool_a.begin().await.expect("begin first reservation");
        let outcome = reserve(
            &mut tx,
            "founding-test-capacity-reservation-a",
            "founding-test-capacity-operation-a",
            "founding-test-capacity-person-a",
            "founding-test-capacity-payer-a",
            FoundingOffer::Annual,
            1,
            expires,
            now,
        )
        .await
        .expect("first reservation");
        tx.commit().await.expect("commit first reservation");
        ("a", outcome)
    });
    let reserve_b = tokio::spawn(async move {
        let mut tx = pool_b.begin().await.expect("begin second reservation");
        let outcome = reserve(
            &mut tx,
            "founding-test-capacity-reservation-b",
            "founding-test-capacity-operation-b",
            "founding-test-capacity-person-b",
            "founding-test-capacity-payer-b",
            FoundingOffer::Monthly,
            1,
            expires,
            now,
        )
        .await
        .expect("second reservation");
        tx.commit().await.expect("commit second reservation");
        ("b", outcome)
    });
    let outcomes = [
        reserve_a.await.expect("first task"),
        reserve_b.await.expect("second task"),
    ];
    assert_eq!(
        outcomes
            .iter()
            .filter(|(_, outcome)| matches!(outcome, ReservationOutcome::Created(_)))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|(_, outcome)| matches!(outcome, ReservationOutcome::Full))
            .count(),
        1
    );

    let winning_reservation = outcomes
        .iter()
        .find_map(|(label, outcome)| {
            matches!(outcome, ReservationOutcome::Created(_)).then_some(if *label == "a" {
                "founding-test-capacity-reservation-a"
            } else {
                "founding-test-capacity-reservation-b"
            })
        })
        .expect("one reservation wins");
    let mut tx = pool.begin().await.expect("begin final confirmation");
    let confirmation = confirm_payment(
        &mut tx,
        winning_reservation,
        "founding-test-capacity-payment",
        FoundingDate::new(2026, 1, 31).unwrap(),
        now + 1,
    )
    .await;
    if confirmation.is_err() {
        tx.rollback()
            .await
            .expect("rollback unavailable reservation");
    } else {
        let confirmation = confirmation.expect("confirm final reservation");
        assert!(matches!(confirmation, ConfirmationOutcome::Awarded(_)));
        tx.commit().await.expect("commit final award");
    }
    let mut status_tx = pool.begin().await.expect("begin capacity status");
    let status = load_capacity_status(&mut status_tx, now + 1)
        .await
        .expect("load capacity status");
    assert_eq!(status.confirmed_awards, FOUNDING_CAPACITY);
    assert_eq!(status.remaining_places, 0);
    status_tx.rollback().await.expect("rollback status");
    cleanup(&pool, fixture).await;
}

#[tokio::test]
async fn late_payment_is_refunded_when_capacity_was_consumed_after_reservation() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let _guard = TEST_LOCK.get_or_init(|| Mutex::const_new(())).lock().await;
    let fixture = "late-refund";
    cleanup(&pool, fixture).await;
    let now = 1_800_000_000;
    let mut tx = pool.begin().await.expect("begin late reservation");
    let reservation = reserve(
        &mut tx,
        "founding-test-late-refund-reservation",
        "founding-test-late-refund-operation",
        "founding-test-late-refund-person",
        "founding-test-late-refund-payer",
        FoundingOffer::Monthly,
        1,
        now + 1_800,
        now,
    )
    .await
    .expect("create late reservation");
    assert!(matches!(reservation, ReservationOutcome::Created(_)));
    tx.commit().await.expect("commit late reservation");
    seed_awards(&pool, fixture, FOUNDING_CAPACITY).await;

    let mut confirm_tx = pool.begin().await.expect("begin late confirmation");
    let outcome = confirm_payment(
        &mut confirm_tx,
        "founding-test-late-refund-reservation",
        "founding-test-late-refund-payment",
        FoundingDate::new(2026, 2, 1).unwrap(),
        now + 1_801,
    )
    .await
    .expect("late payment outcome");
    assert_eq!(outcome, ConfirmationOutcome::RefundRequired);
    confirm_tx.commit().await.expect("commit refund decision");
    cleanup(&pool, fixture).await;
}

#[tokio::test]
async fn expired_payment_cannot_take_a_place_held_by_a_live_reservation() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let _guard = TEST_LOCK.get_or_init(|| Mutex::const_new(())).lock().await;
    let fixture = "expired-live-interleave";
    cleanup(&pool, fixture).await;
    seed_awards(&pool, fixture, FOUNDING_CAPACITY - 1).await;

    let now = 1_800_000_000;
    let mut expired_tx = pool.begin().await.expect("begin expired reservation");
    let expired = reserve(
        &mut expired_tx,
        "founding-test-expired-live-interleave-expired",
        "founding-test-expired-live-interleave-expired-operation",
        "founding-test-expired-live-interleave-expired-person",
        "founding-test-expired-live-interleave-expired-payer",
        FoundingOffer::Monthly,
        1,
        now - 1,
        now - 1_801,
    )
    .await
    .expect("create expired reservation");
    assert!(matches!(expired, ReservationOutcome::Created(_)));
    expired_tx
        .commit()
        .await
        .expect("commit expired reservation");

    let mut live_tx = pool.begin().await.expect("begin live reservation");
    let live = reserve(
        &mut live_tx,
        "founding-test-expired-live-interleave-live",
        "founding-test-expired-live-interleave-live-operation",
        "founding-test-expired-live-interleave-live-person",
        "founding-test-expired-live-interleave-live-payer",
        FoundingOffer::Monthly,
        1,
        now + 1_800,
        now,
    )
    .await
    .expect("create live reservation");
    assert!(matches!(live, ReservationOutcome::Created(_)));
    live_tx.commit().await.expect("commit live reservation");

    let mut expired_confirm_tx = pool.begin().await.expect("begin expired confirmation");
    assert_eq!(
        confirm_payment(
            &mut expired_confirm_tx,
            "founding-test-expired-live-interleave-expired",
            "founding-test-expired-live-interleave-expired-payment",
            FoundingDate::new(2026, 1, 1).unwrap(),
            now,
        )
        .await
        .expect("late expired outcome"),
        ConfirmationOutcome::RefundRequired
    );
    expired_confirm_tx
        .commit()
        .await
        .expect("commit expired refund");

    let mut live_confirm_tx = pool.begin().await.expect("begin live confirmation");
    assert!(matches!(
        confirm_payment(
            &mut live_confirm_tx,
            "founding-test-expired-live-interleave-live",
            "founding-test-expired-live-interleave-live-payment",
            FoundingDate::new(2026, 1, 1).unwrap(),
            now,
        )
        .await
        .expect("live outcome"),
        ConfirmationOutcome::Awarded(_)
    ));
    live_confirm_tx.commit().await.expect("commit live award");
    cleanup(&pool, fixture).await;
}

#[tokio::test]
async fn late_duplicate_beneficiary_payment_becomes_refund_required() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let _guard = TEST_LOCK.get_or_init(|| Mutex::const_new(())).lock().await;
    let fixture = "duplicate-beneficiary";
    cleanup(&pool, fixture).await;
    let now = 1_800_000_000;

    let mut expired_tx = pool
        .begin()
        .await
        .expect("begin expired beneficiary reservation");
    let expired = reserve(
        &mut expired_tx,
        "founding-test-duplicate-beneficiary-expired",
        "founding-test-duplicate-beneficiary-expired-operation",
        "founding-test-duplicate-beneficiary-person",
        "founding-test-duplicate-beneficiary-payer-a",
        FoundingOffer::Monthly,
        1,
        now - 1,
        now - 1_801,
    )
    .await
    .expect("create expired beneficiary reservation");
    assert!(matches!(expired, ReservationOutcome::Created(_)));
    expired_tx
        .commit()
        .await
        .expect("commit expired reservation");

    let mut current_tx = pool
        .begin()
        .await
        .expect("begin current beneficiary reservation");
    let current = reserve(
        &mut current_tx,
        "founding-test-duplicate-beneficiary-current",
        "founding-test-duplicate-beneficiary-current-operation",
        "founding-test-duplicate-beneficiary-person",
        "founding-test-duplicate-beneficiary-payer-b",
        FoundingOffer::Monthly,
        1,
        now + 1_800,
        now,
    )
    .await
    .expect("create current beneficiary reservation");
    assert!(matches!(current, ReservationOutcome::Created(_)));
    current_tx
        .commit()
        .await
        .expect("commit current reservation");

    let mut current_confirm_tx = pool.begin().await.expect("begin current confirmation");
    assert!(matches!(
        confirm_payment(
            &mut current_confirm_tx,
            "founding-test-duplicate-beneficiary-current",
            "founding-test-duplicate-beneficiary-current-payment",
            FoundingDate::new(2026, 1, 1).unwrap(),
            now,
        )
        .await
        .expect("confirm current beneficiary payment"),
        ConfirmationOutcome::Awarded(_)
    ));
    current_confirm_tx
        .commit()
        .await
        .expect("commit current award");

    let mut expired_confirm_tx = pool.begin().await.expect("begin duplicate confirmation");
    assert_eq!(
        confirm_payment(
            &mut expired_confirm_tx,
            "founding-test-duplicate-beneficiary-expired",
            "founding-test-duplicate-beneficiary-expired-payment",
            FoundingDate::new(2026, 1, 1).unwrap(),
            now,
        )
        .await
        .expect("duplicate beneficiary outcome"),
        ConfirmationOutcome::RefundRequired
    );
    expired_confirm_tx
        .commit()
        .await
        .expect("commit duplicate refund");

    let mut replay_tx = pool.begin().await.expect("begin refund replay");
    assert_eq!(
        confirm_payment(
            &mut replay_tx,
            "founding-test-duplicate-beneficiary-expired",
            "founding-test-duplicate-beneficiary-expired-payment",
            FoundingDate::new(2026, 1, 1).unwrap(),
            now + 1,
        )
        .await
        .expect("replay refund"),
        ConfirmationOutcome::RefundRequired
    );
    assert!(matches!(
        confirm_payment(
            &mut replay_tx,
            "founding-test-duplicate-beneficiary-expired",
            "founding-test-duplicate-beneficiary-other-payment",
            FoundingDate::new(2026, 1, 1).unwrap(),
            now + 1,
        )
        .await,
        Err(sotto_server::founding_allocator::FoundingAllocatorError::ConfirmationConflict)
    ));
    replay_tx.rollback().await.expect("rollback refund replay");

    let mut other_reservation_tx = pool.begin().await.expect("begin payment reuse reservation");
    let other = reserve(
        &mut other_reservation_tx,
        "founding-test-duplicate-beneficiary-other",
        "founding-test-duplicate-beneficiary-other-operation",
        "founding-test-duplicate-beneficiary-other-person",
        "founding-test-duplicate-beneficiary-other-payer",
        FoundingOffer::Monthly,
        1,
        now + 1_800,
        now,
    )
    .await
    .expect("create payment reuse reservation");
    assert!(matches!(other, ReservationOutcome::Created(_)));
    other_reservation_tx
        .commit()
        .await
        .expect("commit payment reuse reservation");
    let mut payment_reuse_tx = pool.begin().await.expect("begin payment reuse");
    assert!(matches!(
        confirm_payment(
            &mut payment_reuse_tx,
            "founding-test-duplicate-beneficiary-other",
            "founding-test-duplicate-beneficiary-expired-payment",
            FoundingDate::new(2026, 1, 1).unwrap(),
            now,
        )
        .await,
        Err(sotto_server::founding_allocator::FoundingAllocatorError::PaymentConflict)
    ));
    payment_reuse_tx
        .rollback()
        .await
        .expect("rollback payment reuse");
    cleanup(&pool, fixture).await;
}

#[tokio::test]
async fn founding_award_preserves_person_and_anniversary_across_payer_transfer() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let _guard = TEST_LOCK.get_or_init(|| Mutex::const_new(())).lock().await;
    let fixture = "transfer";
    cleanup(&pool, fixture).await;
    let now = 1_800_000_000;
    let mut tx = pool.begin().await.expect("begin award");
    let reservation = reserve(
        &mut tx,
        "founding-test-transfer-reservation",
        "founding-test-transfer-operation",
        "founding-test-transfer-person",
        "founding-test-transfer-payer-a",
        FoundingOffer::Annual,
        1,
        now + 1_800,
        now,
    )
    .await
    .expect("create transfer reservation");
    assert!(matches!(reservation, ReservationOutcome::Created(_)));
    let award = match confirm_payment(
        &mut tx,
        "founding-test-transfer-reservation",
        "founding-test-transfer-payment",
        FoundingDate::new(2024, 2, 29).unwrap(),
        now + 1_801,
    )
    .await
    .expect("confirm transfer award")
    {
        ConfirmationOutcome::Awarded(award) => award,
        other => panic!("unexpected confirmation outcome: {other:?}"),
    };
    assert_eq!(award.original_end.to_string(), "2025-02-28");
    assert!(matches!(
        confirm_payment(
            &mut tx,
            "founding-test-transfer-reservation",
            "founding-test-transfer-payment",
            FoundingDate::new(2024, 2, 29).unwrap(),
            now + 1_802,
        )
        .await
        .expect("replay founding payment"),
        ConfirmationOutcome::AlreadyAwarded(_)
    ));
    assert!(matches!(
        confirm_payment(
            &mut tx,
            "founding-test-transfer-reservation",
            "founding-test-transfer-other-payment",
            FoundingDate::new(2024, 2, 29).unwrap(),
            now + 1_802,
        )
        .await,
        Err(sotto_server::founding_allocator::FoundingAllocatorError::ConfirmationConflict)
    ));
    let transferred = sotto_server::founding_allocator::transfer_payer(
        &mut tx,
        &award.award_id,
        "founding-test-transfer-payer-b",
    )
    .await
    .expect("transfer payer");
    assert_eq!(transferred.beneficiary_id, "founding-test-transfer-person");
    assert_eq!(transferred.payer_id, "founding-test-transfer-payer-b");
    assert_eq!(transferred.original_start, award.original_start);
    assert_eq!(transferred.original_end, award.original_end);
    tx.rollback().await.expect("rollback transfer fixture");
    cleanup(&pool, fixture).await;
}

#[test]
fn founding_offer_mapping_rejects_standard_offers() {
    assert_eq!(
        sotto_server::founding_allocator::FoundingOffer::from_billing_offer(
            BillingOffer::StandardAnnual
        ),
        None
    );
}
