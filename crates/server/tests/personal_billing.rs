//! Opt-in Postgres coverage for durable personal billing state.

use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use sqlx::{postgres::PgConnectOptions, PgPool};

use sotto_server::billing_catalogue::BillingOffer;
use sotto_server::billing_operations::{begin_personal_operation, BeginOperation};
use sotto_server::db;
use sotto_server::personal_billing::{
    advance_paid_through, begin_account, load_account, record_invoice_paid, record_paid_settlement,
    record_refund_required, PersonalBillingError, PersonalBillingState,
};

async fn pool_or_skip() -> Option<PgPool> {
    if std::env::var("SOTTO_RUN_DB_TESTS").as_deref() != Ok("1") {
        eprintln!("skipping personal billing tests: set SOTTO_RUN_DB_TESTS=1");
        return None;
    }
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL for opted-in database tests");
    let options = PgConnectOptions::from_str(&url).expect("parse DATABASE_URL");
    assert!(
        matches!(options.get_host(), "localhost" | "127.0.0.1" | "::1"),
        "refusing personal billing tests against non-local host: {}",
        options.get_host()
    );
    let pool = db::connect(&url).await.expect("connect to test database");
    db::migrate(&pool).await.expect("apply migrations");
    Some(pool)
}

async fn cleanup(pool: &PgPool, user_id: &str) {
    sqlx::query("DELETE FROM billing_personal_events WHERE user_id = $1")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("clean personal events");
    sqlx::query("DELETE FROM billing_personal_accounts WHERE user_id = $1")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("clean personal account");
    sqlx::query("DELETE FROM billing_operations WHERE actor_user_id = $1")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("clean personal operation");
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("clean personal user");
}

#[tokio::test]
async fn personal_settlement_is_idempotent_and_keeps_paid_term() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    const USER_ID: &str = "personal-billing-test-user";
    cleanup(&pool, USER_ID).await;
    sqlx::query("INSERT INTO users (id, oauth_provider, oauth_subject) VALUES ($1, 'test', $1)")
        .bind(USER_ID)
        .execute(&pool)
        .await
        .expect("seed personal user");

    let mut tx = pool.begin().await.expect("begin checkout operation");
    let operation = match begin_personal_operation(
        &mut tx,
        USER_ID,
        "personal-billing-test-operation",
        BillingOffer::FoundingMonthly,
        1,
        1_900_000_000,
        "https://app.sotto.test/billing",
        "https://app.sotto.test/billing",
    )
    .await
    .expect("begin operation")
    {
        BeginOperation::Created(operation) => operation,
        BeginOperation::AlreadyExists(_) => panic!("operation fixture unexpectedly existed"),
    };
    begin_account(
        &mut tx,
        USER_ID,
        &operation.operation_id,
        BillingOffer::FoundingMonthly,
        1_900_000_000,
        1_800_000_000,
    )
    .await
    .expect("begin account");
    tx.commit().await.expect("commit pending account");

    let mut settlement_tx = pool.begin().await.expect("begin settlement");
    record_paid_settlement(
        &mut settlement_tx,
        &operation.operation_id,
        "cus_personal_test",
        "sub_personal_test",
        "pi_personal_test",
        1_800_086_400,
        "2027-01-01",
    )
    .await
    .expect("record settlement");
    record_paid_settlement(
        &mut settlement_tx,
        &operation.operation_id,
        "cus_personal_test",
        "sub_personal_test",
        "pi_personal_test",
        1_800_086_400,
        "2027-01-01",
    )
    .await
    .expect("replay settlement");
    assert!(matches!(
        record_paid_settlement(
            &mut settlement_tx,
            &operation.operation_id,
            "cus_other",
            "sub_other",
            "pi_other",
            1_800_086_400,
            "2027-01-01",
        )
        .await,
        Err(PersonalBillingError::SettlementConflict)
    ));
    settlement_tx.commit().await.expect("commit settlement");

    let mut renewal_tx = pool.begin().await.expect("begin renewal");
    advance_paid_through(
        &mut renewal_tx,
        USER_ID,
        "sub_personal_test",
        1_900_000_000,
        "2030-03-17",
    )
    .await
    .expect("advance paid term");
    record_invoice_paid(
        &mut renewal_tx,
        "sub_personal_test",
        "pi_personal_renewal",
        1_900_000_000,
        "2030-03-17",
    )
    .await
    .expect("record renewal payment");
    renewal_tx.commit().await.expect("commit renewal");

    let mut load_tx = pool.begin().await.expect("begin account load");
    let account = load_account(&mut load_tx, USER_ID)
        .await
        .expect("load account")
        .expect("account exists");
    assert_eq!(account.state, PersonalBillingState::Active);
    assert_eq!(account.paid_through_date.as_deref(), Some("2030-03-17"));
    assert_eq!(
        account.payment_reference.as_deref(),
        Some("pi_personal_renewal")
    );
    load_tx.rollback().await.expect("rollback account load");
    cleanup(&pool, USER_ID).await;
}

#[tokio::test]
async fn abandoned_and_canceled_accounts_can_start_a_new_operation() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    const USER_ID: &str = "personal-billing-reuse-user";
    cleanup(&pool, USER_ID).await;
    sqlx::query("INSERT INTO users (id, oauth_provider, oauth_subject) VALUES ($1, 'test', $1)")
        .bind(USER_ID)
        .execute(&pool)
        .await
        .expect("seed reuse user");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("unix clock")
        .as_secs() as i64;

    let mut first_tx = pool.begin().await.expect("begin first operation");
    let first = match begin_personal_operation(
        &mut first_tx,
        USER_ID,
        "personal-billing-reuse-first",
        BillingOffer::StandardMonthly,
        1,
        now + 100,
        "https://app.sotto.test/billing",
        "https://app.sotto.test/billing",
    )
    .await
    .expect("begin first operation")
    {
        BeginOperation::Created(operation) => operation,
        BeginOperation::AlreadyExists(_) => panic!("first operation unexpectedly existed"),
    };
    begin_account(
        &mut first_tx,
        USER_ID,
        &first.operation_id,
        BillingOffer::StandardMonthly,
        now + 100,
        now,
    )
    .await
    .expect("begin first account");
    first_tx.commit().await.expect("commit first operation");

    let mut second_tx = pool.begin().await.expect("begin second operation");
    let second = match begin_personal_operation(
        &mut second_tx,
        USER_ID,
        "personal-billing-reuse-second",
        BillingOffer::StandardAnnual,
        1,
        now + 300,
        "https://app.sotto.test/billing",
        "https://app.sotto.test/billing",
    )
    .await
    .expect("begin second operation")
    {
        BeginOperation::Created(operation) => operation,
        BeginOperation::AlreadyExists(_) => panic!("second operation unexpectedly existed"),
    };
    begin_account(
        &mut second_tx,
        USER_ID,
        &second.operation_id,
        BillingOffer::StandardAnnual,
        now + 300,
        now + 200,
    )
    .await
    .expect("reuse expired pending account");
    second_tx.commit().await.expect("commit second operation");

    let mut cancel_tx = pool.begin().await.expect("begin canceled transition");
    sqlx::query(
        "UPDATE billing_personal_accounts SET state = 'canceled', stripe_subscription_id = 'sub_old' \
         WHERE user_id = $1",
    )
    .bind(USER_ID)
    .execute(&mut *cancel_tx)
    .await
    .expect("mark account canceled");
    let third = match begin_personal_operation(
        &mut cancel_tx,
        USER_ID,
        "personal-billing-reuse-third",
        BillingOffer::FoundingMonthly,
        1,
        now + 500,
        "https://app.sotto.test/billing",
        "https://app.sotto.test/billing",
    )
    .await
    .expect("begin third operation")
    {
        BeginOperation::Created(operation) => operation,
        BeginOperation::AlreadyExists(_) => panic!("third operation unexpectedly existed"),
    };
    begin_account(
        &mut cancel_tx,
        USER_ID,
        &third.operation_id,
        BillingOffer::FoundingMonthly,
        now + 500,
        now + 400,
    )
    .await
    .expect("reuse canceled account");
    cancel_tx.commit().await.expect("commit canceled reuse");

    let mut load_tx = pool.begin().await.expect("load reused account");
    let account = load_account(&mut load_tx, USER_ID)
        .await
        .expect("load reused account")
        .expect("reused account exists");
    assert_eq!(account.operation_id, third.operation_id);
    assert_eq!(account.state, PersonalBillingState::Pending);
    assert_eq!(account.stripe_subscription_id, None);
    load_tx.rollback().await.expect("rollback reused load");
    cleanup(&pool, USER_ID).await;
}

#[tokio::test]
async fn refund_required_settlement_is_durable_and_idempotent() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    const USER_ID: &str = "personal-billing-refund-user";
    cleanup(&pool, USER_ID).await;
    sqlx::query("INSERT INTO users (id, oauth_provider, oauth_subject) VALUES ($1, 'test', $1)")
        .bind(USER_ID)
        .execute(&pool)
        .await
        .expect("seed refund user");
    let mut tx = pool.begin().await.expect("begin refund operation");
    let operation = match begin_personal_operation(
        &mut tx,
        USER_ID,
        "personal-billing-refund-operation",
        BillingOffer::FoundingMonthly,
        1,
        1_900_000_000,
        "https://app.sotto.test/billing",
        "https://app.sotto.test/billing",
    )
    .await
    .expect("begin refund operation")
    {
        BeginOperation::Created(operation) => operation,
        BeginOperation::AlreadyExists(_) => panic!("refund operation unexpectedly existed"),
    };
    begin_account(
        &mut tx,
        USER_ID,
        &operation.operation_id,
        BillingOffer::FoundingMonthly,
        1_900_000_000,
        1_800_000_000,
    )
    .await
    .expect("begin refund account");
    let first = record_refund_required(
        &mut tx,
        &operation.operation_id,
        "cus_refund",
        "sub_refund",
        "pi_refund",
    )
    .await
    .expect("record refund required");
    assert_eq!(
        first,
        sotto_server::personal_billing::SettlementDisposition::Applied
    );
    let replay = record_refund_required(
        &mut tx,
        &operation.operation_id,
        "cus_refund",
        "sub_refund",
        "pi_refund",
    )
    .await
    .expect("replay refund required");
    assert_eq!(
        replay,
        sotto_server::personal_billing::SettlementDisposition::AlreadyApplied
    );
    tx.commit().await.expect("commit refund state");

    let mut load_tx = pool.begin().await.expect("load refund account");
    let account = load_account(&mut load_tx, USER_ID)
        .await
        .expect("load refund account")
        .expect("refund account exists");
    assert_eq!(account.state, PersonalBillingState::RefundRequired);
    assert_eq!(account.payment_reference.as_deref(), Some("pi_refund"));
    load_tx.rollback().await.expect("rollback refund load");
    cleanup(&pool, USER_ID).await;
}
