//! Opt-in Postgres coverage for the founding-price allocator.

use std::str::FromStr;
use std::sync::OnceLock;

use sqlx::{postgres::PgConnectOptions, PgPool};
use tokio::sync::Mutex;

use sotto_server::billing_catalogue::BillingOffer;
use sotto_server::db;
use sotto_server::founding_allocator::{
    confirm_payment, load_capacity_status, quote_status, record_async_payment_checkout_completion,
    record_pending_checkout_completion, reserve, reserve_with_payer_kind,
    reserve_with_payer_kind_until, select_offer, ConfirmationOutcome, FoundingDate, FoundingOffer,
    FoundingPayerKind, ReservationOutcome, FOUNDING_CAPACITY,
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
async fn server_selects_founder_for_unawarded_person_and_standard_when_full() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let _guard = TEST_LOCK.get_or_init(|| Mutex::const_new(())).lock().await;
    let fixture = "offer-selection";
    cleanup(&pool, fixture).await;
    let now = 1_800_000_000;
    let person = format!("founding-test-{fixture}-person");

    let mut tx = pool.begin().await.expect("begin selection");
    assert_eq!(
        select_offer(&mut tx, &person, FoundingOffer::Monthly, None, now)
            .await
            .expect("select founding price"),
        BillingOffer::FoundingMonthly
    );
    tx.commit().await.expect("commit selection");

    let mut reserve_tx = pool.begin().await.expect("begin scheduled reservation");
    let reservation = reserve_with_payer_kind(
        &mut reserve_tx,
        "founding-test-offer-selection-reservation",
        "founding-test-offer-selection-operation",
        &person,
        FoundingPayerKind::Sponsor,
        "founding-test-offer-selection-org",
        FoundingOffer::Monthly,
        1,
        now + 1_800,
        now,
    )
    .await
    .expect("reserve sponsored founding offer");
    assert!(matches!(reservation, ReservationOutcome::Created(_)));
    reserve_tx
        .commit()
        .await
        .expect("commit scheduled reservation");

    let mut quote_tx = pool.begin().await.expect("begin quote replay");
    assert_eq!(
        select_offer(
            &mut quote_tx,
            &person,
            FoundingOffer::Monthly,
            Some("founding-test-offer-selection-operation"),
            now + 1,
        )
        .await
        .expect("reserved operation retains its offer"),
        BillingOffer::FoundingMonthly
    );
    assert_eq!(
        select_offer(
            &mut quote_tx,
            &person,
            FoundingOffer::Monthly,
            Some("founding-test-offer-selection-other-operation"),
            now + 1,
        )
        .await
        .expect("another operation cannot reuse the reserved offer"),
        BillingOffer::StandardMonthly
    );
    quote_tx.rollback().await.expect("rollback quote replay");

    let awarded_person = format!("founding-test-{fixture}-awarded-person");
    sqlx::query(
        "INSERT INTO billing_founding_awards \
         (award_id, beneficiary_id, payer_id, offer, cohort_ordinal, original_start_date, original_end_date) \
         VALUES ($1, $2, $2, 'founding_monthly', 100, '2026-01-01', '2026-02-01')",
    )
    .bind(format!("founding-test-{fixture}-existing-award"))
    .bind(&awarded_person)
    .execute(&pool)
    .await
    .expect("seed existing personal award");
    let mut award_tx = pool.begin().await.expect("begin awarded-person quote");
    assert_eq!(
        select_offer(
            &mut award_tx,
            &awarded_person,
            FoundingOffer::Monthly,
            None,
            now
        )
        .await
        .expect("an awarded person cannot claim a second founding offer"),
        BillingOffer::StandardMonthly
    );
    award_tx
        .rollback()
        .await
        .expect("rollback awarded-person quote");

    seed_awards(&pool, fixture, FOUNDING_CAPACITY - 1).await;
    let mut tx = pool.begin().await.expect("begin full selection");
    assert_eq!(
        select_offer(&mut tx, &person, FoundingOffer::Monthly, None, now)
            .await
            .expect("select standard price when full"),
        BillingOffer::StandardMonthly
    );
    tx.commit().await.expect("commit full selection");
    cleanup(&pool, fixture).await;
}

#[tokio::test]
async fn scheduled_reservation_holds_capacity_and_late_payment_requires_refund_after_release() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let _guard = TEST_LOCK.get_or_init(|| Mutex::const_new(())).lock().await;
    let fixture = "scheduled-reservation";
    cleanup(&pool, fixture).await;
    let now = 1_800_000_000;
    let quote_expiry = now + 1_800;
    let reservation_expiry = now + 60 * 60 * 24 * 30;

    let mut reserve_tx = pool.begin().await.expect("begin scheduled reservation");
    let reservation = sotto_server::founding_allocator::reserve_with_payer_kind_until(
        &mut reserve_tx,
        "founding-test-scheduled-reservation-reservation",
        "founding-test-scheduled-reservation-operation",
        "founding-test-scheduled-reservation-person",
        FoundingPayerKind::Sponsor,
        "founding-test-scheduled-reservation-payer",
        FoundingOffer::Annual,
        1,
        quote_expiry,
        reservation_expiry,
        now,
    )
    .await
    .expect("create scheduled reservation");
    let ReservationOutcome::Created(reservation) = reservation else {
        panic!("scheduled reservation should occupy capacity");
    };
    assert_eq!(reservation.quote_expires_at_epoch, quote_expiry);
    assert_eq!(reservation.reservation_expires_at_epoch, reservation_expiry);
    reserve_tx
        .commit()
        .await
        .expect("commit scheduled reservation");

    let mut status_tx = pool.begin().await.expect("begin capacity status");
    let status = load_capacity_status(&mut status_tx, quote_expiry + 1)
        .await
        .expect("load capacity during sponsored settlement window");
    assert_eq!(status.active_reservations, 1);
    status_tx
        .rollback()
        .await
        .expect("rollback capacity status");

    let mut release_tx = pool.begin().await.expect("begin reservation release");
    sotto_server::founding_allocator::release_reservation(
        &mut release_tx,
        "founding-test-scheduled-reservation-operation",
    )
    .await
    .expect("release failed provider operation");
    release_tx
        .commit()
        .await
        .expect("commit reservation release");

    let mut late_tx = pool.begin().await.expect("begin late payment");
    assert_eq!(
        confirm_payment(
            &mut late_tx,
            "founding-test-scheduled-reservation-reservation",
            "founding-test-scheduled-reservation-payment",
            FoundingDate::new(2026, 1, 1).unwrap(),
            quote_expiry + 2,
        )
        .await
        .expect("late payment must not claim released place"),
        ConfirmationOutcome::RefundRequired
    );
    late_tx
        .commit()
        .await
        .expect("commit refund-required status");
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
        let outcome = reserve_with_payer_kind(
            &mut tx,
            "founding-test-capacity-reservation-b",
            "founding-test-capacity-operation-b",
            "founding-test-capacity-person-b",
            FoundingPayerKind::Sponsor,
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
async fn quote_to_checkout_race_cannot_assign_place_one_hundred_and_one() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let _guard = TEST_LOCK.get_or_init(|| Mutex::const_new(())).lock().await;
    let fixture = "quote-checkout-race";
    cleanup(&pool, fixture).await;
    seed_awards(&pool, fixture, FOUNDING_CAPACITY - 1).await;

    let now = 1_800_000_000;
    let quote_expiry = now + 1_800;
    let reservation_expiry = quote_expiry + 1_800;
    let people = [
        format!("founding-test-{fixture}-personal"),
        format!("founding-test-{fixture}-sponsored"),
    ];
    let sponsored_payer = format!("founding-test-{fixture}-org");
    for person in &people {
        let mut quote_tx = pool.begin().await.expect("begin founding quote");
        assert_eq!(
            select_offer(&mut quote_tx, person, FoundingOffer::Monthly, None, now)
                .await
                .expect("quote founding offer"),
            BillingOffer::FoundingMonthly
        );
        quote_tx.rollback().await.expect("rollback founding quote");
    }

    let personal_pool = pool.clone();
    let sponsored_pool = pool.clone();
    let personal_person = people[0].clone();
    let sponsored_person = people[1].clone();
    let personal = tokio::spawn(async move {
        let mut tx = personal_pool
            .begin()
            .await
            .expect("begin personal checkout claim");
        let selected = select_offer(
            &mut tx,
            &personal_person,
            FoundingOffer::Monthly,
            Some("founding-test-quote-checkout-race-personal-operation"),
            now,
        )
        .await
        .expect("revalidate personal quote");
        if selected != BillingOffer::FoundingMonthly {
            tx.rollback().await.expect("rollback stale personal quote");
            return false;
        }
        let outcome = reserve_with_payer_kind_until(
            &mut tx,
            "founding-test-quote-checkout-race-personal-reservation",
            "founding-test-quote-checkout-race-personal-operation",
            &personal_person,
            FoundingPayerKind::Personal,
            &personal_person,
            FoundingOffer::Monthly,
            1,
            quote_expiry,
            reservation_expiry,
            now,
        )
        .await
        .expect("reserve personal checkout");
        if matches!(outcome, ReservationOutcome::Created(_)) {
            tx.commit().await.expect("commit personal reservation");
            true
        } else {
            tx.rollback().await.expect("rollback personal reservation");
            false
        }
    });
    let sponsored = tokio::spawn(async move {
        let mut tx = sponsored_pool
            .begin()
            .await
            .expect("begin sponsored checkout claim");
        let selected = select_offer(
            &mut tx,
            &sponsored_person,
            FoundingOffer::Monthly,
            Some("founding-test-quote-checkout-race-sponsored-operation"),
            now,
        )
        .await
        .expect("revalidate sponsored quote");
        if selected != BillingOffer::FoundingMonthly {
            tx.rollback().await.expect("rollback stale sponsored quote");
            return false;
        }
        let outcome = reserve_with_payer_kind_until(
            &mut tx,
            "founding-test-quote-checkout-race-sponsored-reservation",
            "founding-test-quote-checkout-race-sponsored-operation",
            &sponsored_person,
            FoundingPayerKind::Sponsor,
            &sponsored_payer,
            FoundingOffer::Monthly,
            1,
            quote_expiry,
            reservation_expiry,
            now,
        )
        .await
        .expect("reserve sponsored checkout");
        if matches!(outcome, ReservationOutcome::Created(_)) {
            tx.commit().await.expect("commit sponsored reservation");
            true
        } else {
            tx.rollback().await.expect("rollback sponsored reservation");
            false
        }
    });
    let outcomes = [
        personal.await.expect("personal checkout task"),
        sponsored.await.expect("sponsored checkout task"),
    ];
    assert_eq!(outcomes.iter().filter(|reserved| **reserved).count(), 1);

    let mut status_tx = pool.begin().await.expect("begin final capacity check");
    let status = load_capacity_status(&mut status_tx, now)
        .await
        .expect("load final capacity");
    assert_eq!(status.confirmed_awards, FOUNDING_CAPACITY - 1);
    assert_eq!(status.active_reservations, 1);
    assert_eq!(status.remaining_places, 0);
    status_tx
        .rollback()
        .await
        .expect("rollback final capacity check");
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
async fn expired_reservation_is_refunded_even_when_founding_capacity_remains() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let _guard = TEST_LOCK.get_or_init(|| Mutex::const_new(())).lock().await;
    let fixture = "expired-open-capacity";
    cleanup(&pool, fixture).await;
    let now = 1_800_000_000;
    let mut tx = pool.begin().await.expect("begin expiring reservation");
    assert!(matches!(
        reserve_with_payer_kind_until(
            &mut tx,
            "founding-test-expired-open-capacity-reservation",
            "founding-test-expired-open-capacity-operation",
            "founding-test-expired-open-capacity-person",
            FoundingPayerKind::Personal,
            "founding-test-expired-open-capacity-payer",
            FoundingOffer::Monthly,
            1,
            now + 1,
            now + 1,
            now,
        )
        .await
        .expect("create short reservation"),
        ReservationOutcome::Created(_)
    ));
    assert!(!record_pending_checkout_completion(
        &mut tx,
        "founding-test-expired-open-capacity-operation",
        now + 2,
        now + 2,
    )
    .await
    .expect("late Checkout completion cannot extend an expired reservation"));
    assert_eq!(
        confirm_payment(
            &mut tx,
            "founding-test-expired-open-capacity-reservation",
            "founding-test-expired-open-capacity-payment",
            FoundingDate::new(2026, 2, 1).unwrap(),
            now + 2,
        )
        .await
        .expect("late payment outcome"),
        ConfirmationOutcome::RefundRequired
    );
    tx.commit().await.expect("commit expired refund decision");
    cleanup(&pool, fixture).await;
}

#[tokio::test]
async fn late_checkout_webhook_does_not_reclaim_a_place_reserved_by_another_buyer() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let _guard = TEST_LOCK.get_or_init(|| Mutex::const_new(())).lock().await;
    let fixture = "late-webhook-capacity";
    cleanup(&pool, fixture).await;
    let checkout_created_at = 1_800_000_000;
    let checkout_completed_at = checkout_created_at + 1_700;
    let checkout_expiry = checkout_created_at + 100 + 1_800;
    let webhook_received_at = checkout_expiry + 200;

    let mut original_tx = pool.begin().await.expect("begin original reservation");
    assert!(matches!(
        reserve_with_payer_kind_until(
            &mut original_tx,
            "founding-test-late-webhook-capacity-original-reservation",
            "founding-test-late-webhook-capacity-original-operation",
            "founding-test-late-webhook-capacity-original-person",
            FoundingPayerKind::Personal,
            "founding-test-late-webhook-capacity-payer",
            FoundingOffer::Monthly,
            1,
            checkout_created_at + 100,
            checkout_expiry,
            checkout_created_at,
        )
        .await
        .expect("create original reservation"),
        ReservationOutcome::Created(_)
    ));
    original_tx
        .commit()
        .await
        .expect("commit original reservation");
    seed_awards(&pool, fixture, FOUNDING_CAPACITY - 1).await;

    let mut later_tx = pool.begin().await.expect("begin later reservation");
    assert!(matches!(
        reserve_with_payer_kind_until(
            &mut later_tx,
            "founding-test-late-webhook-capacity-later-reservation",
            "founding-test-late-webhook-capacity-later-operation",
            "founding-test-late-webhook-capacity-later-person",
            FoundingPayerKind::Personal,
            "founding-test-late-webhook-capacity-payer",
            FoundingOffer::Monthly,
            1,
            webhook_received_at + 1_000,
            webhook_received_at + 1_800,
            webhook_received_at,
        )
        .await
        .expect("the later buyer reserves the expired place"),
        ReservationOutcome::Created(_)
    ));
    later_tx.commit().await.expect("commit later reservation");

    let mut webhook_tx = pool.begin().await.expect("begin delayed webhook");
    assert!(!record_pending_checkout_completion(
        &mut webhook_tx,
        "founding-test-late-webhook-capacity-original-operation",
        checkout_completed_at,
        webhook_received_at,
    )
    .await
    .expect("do not revive an expired reservation after capacity was claimed"));
    assert!(!record_async_payment_checkout_completion(
        &mut webhook_tx,
        "founding-test-late-webhook-capacity-original-operation",
        checkout_expiry,
        true,
        webhook_received_at,
    )
    .await
    .expect("do not revive a completed Session after capacity was claimed"));
    let stored_expiry: i64 = sqlx::query_scalar(
        "SELECT reservation_expires_at_epoch FROM billing_founding_reservations \
         WHERE operation_id = $1",
    )
    .bind("founding-test-late-webhook-capacity-original-operation")
    .fetch_one(&mut *webhook_tx)
    .await
    .expect("read original reservation expiry");
    assert_eq!(stored_expiry, checkout_expiry);
    webhook_tx.commit().await.expect("commit delayed webhook");

    let mut original_confirm_tx = pool.begin().await.expect("begin original payment result");
    assert_eq!(
        confirm_payment(
            &mut original_confirm_tx,
            "founding-test-late-webhook-capacity-original-reservation",
            "founding-test-late-webhook-capacity-original-payment",
            FoundingDate::new(2026, 1, 1).unwrap(),
            webhook_received_at,
        )
        .await
        .expect("original payment confirmation"),
        ConfirmationOutcome::RefundRequired
    );
    assert!(matches!(
        confirm_payment(
            &mut original_confirm_tx,
            "founding-test-late-webhook-capacity-later-reservation",
            "founding-test-late-webhook-capacity-later-payment",
            FoundingDate::new(2026, 1, 1).unwrap(),
            webhook_received_at,
        )
        .await
        .expect("confirm later buyer"),
        ConfirmationOutcome::Awarded(_)
    ));
    original_confirm_tx
        .commit()
        .await
        .expect("commit payment outcomes");
    cleanup(&pool, fixture).await;
}

#[tokio::test]
async fn timely_checkout_completion_keeps_delayed_founding_payment_reserved() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let _guard = TEST_LOCK.get_or_init(|| Mutex::const_new(())).lock().await;
    let fixture = "delayed-checkout-payment";
    cleanup(&pool, fixture).await;
    let now = 1_800_000_000;
    let reservation_expires = now + 1_800;
    let completed_at = now + 1_700;
    let mut reserve_tx = pool.begin().await.expect("begin reservation");
    assert!(matches!(
        reserve(
            &mut reserve_tx,
            "founding-test-delayed-checkout-payment-reservation",
            "founding-test-delayed-checkout-payment-operation",
            "founding-test-delayed-checkout-payment-person",
            "founding-test-delayed-checkout-payment-payer",
            FoundingOffer::Annual,
            1,
            reservation_expires,
            now,
        )
        .await
        .expect("create reservation"),
        ReservationOutcome::Created(_)
    ));
    reserve_tx.commit().await.expect("commit reservation");

    let mut completed_tx = pool.begin().await.expect("begin Checkout completion");
    assert!(record_pending_checkout_completion(
        &mut completed_tx,
        "founding-test-delayed-checkout-payment-operation",
        completed_at,
        now,
    )
    .await
    .expect("record timely Checkout completion"));
    assert!(record_pending_checkout_completion(
        &mut completed_tx,
        "founding-test-delayed-checkout-payment-operation",
        completed_at,
        now,
    )
    .await
    .expect("replay same Checkout completion"));
    let deadline: i64 = sqlx::query_scalar(
        "SELECT reservation_expires_at_epoch FROM billing_founding_reservations \
         WHERE operation_id = $1",
    )
    .bind("founding-test-delayed-checkout-payment-operation")
    .fetch_one(&mut *completed_tx)
    .await
    .expect("read extended reservation deadline");
    assert_eq!(
        deadline,
        completed_at + sotto_server::founding_allocator::SETTLEMENT_RECOVERY_SECONDS
    );
    completed_tx
        .commit()
        .await
        .expect("commit Checkout completion");

    let paid_at = completed_at + 5 * 24 * 60 * 60;
    let mut confirm_tx = pool.begin().await.expect("begin delayed confirmation");
    let outcome = confirm_payment(
        &mut confirm_tx,
        "founding-test-delayed-checkout-payment-reservation",
        "founding-test-delayed-checkout-payment-reference",
        FoundingDate::from_unix_seconds(paid_at).unwrap(),
        paid_at,
    )
    .await
    .expect("confirm delayed payment within the bounded settlement window");
    assert!(matches!(outcome, ConfirmationOutcome::Awarded(_)));
    confirm_tx
        .commit()
        .await
        .expect("commit delayed confirmation");

    // Stripe can deliver the async-success event before its separate Checkout-completed event.
    // The completed Session's original expiry proves it could only have completed in time.
    let out_of_order_operation = "founding-test-delayed-checkout-payment-out-of-order-operation";
    let out_of_order_reservation =
        "founding-test-delayed-checkout-payment-out-of-order-reservation";
    let out_of_order_quote_expiry = now + 1_800;
    let out_of_order_expiry = out_of_order_quote_expiry + 1_800;
    let mut out_of_order_reserve_tx = pool.begin().await.expect("begin out-of-order reservation");
    assert!(matches!(
        reserve_with_payer_kind_until(
            &mut out_of_order_reserve_tx,
            out_of_order_reservation,
            out_of_order_operation,
            "founding-test-delayed-checkout-payment-out-of-order-person",
            FoundingPayerKind::Personal,
            "founding-test-delayed-checkout-payment-payer",
            FoundingOffer::Annual,
            1,
            out_of_order_quote_expiry,
            out_of_order_expiry,
            now,
        )
        .await
        .expect("create out-of-order reservation"),
        ReservationOutcome::Created(_)
    ));
    out_of_order_reserve_tx
        .commit()
        .await
        .expect("commit out-of-order reservation");
    let mut out_of_order_tx = pool.begin().await.expect("begin async-success recording");
    assert!(!record_async_payment_checkout_completion(
        &mut out_of_order_tx,
        out_of_order_operation,
        out_of_order_expiry + 1,
        true,
        out_of_order_expiry + 1,
    )
    .await
    .expect("a different Session expiry cannot claim this reservation"));
    assert!(!record_async_payment_checkout_completion(
        &mut out_of_order_tx,
        out_of_order_operation,
        out_of_order_expiry,
        false,
        out_of_order_expiry + 1,
    )
    .await
    .expect("an incomplete Session cannot extend the reservation"));
    assert!(record_async_payment_checkout_completion(
        &mut out_of_order_tx,
        out_of_order_operation,
        out_of_order_expiry,
        true,
        out_of_order_expiry + 1,
    )
    .await
    .expect("recover timely completion from completed Session"));
    let out_of_order_paid_at = out_of_order_expiry + 5 * 24 * 60 * 60;
    assert!(matches!(
        confirm_payment(
            &mut out_of_order_tx,
            out_of_order_reservation,
            "founding-test-delayed-checkout-payment-out-of-order-reference",
            FoundingDate::from_unix_seconds(out_of_order_paid_at).unwrap(),
            out_of_order_paid_at,
        )
        .await
        .expect("confirm out-of-order delayed payment"),
        ConfirmationOutcome::Awarded(_)
    ));
    assert!(record_pending_checkout_completion(
        &mut out_of_order_tx,
        out_of_order_operation,
        out_of_order_expiry - 100,
        out_of_order_expiry + 5 * 24 * 60 * 60,
    )
    .await
    .expect("later Checkout-completed event replays after async payment success"));
    out_of_order_tx
        .commit()
        .await
        .expect("commit out-of-order delayed payment");
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
        now + 1,
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
            now + 2,
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
