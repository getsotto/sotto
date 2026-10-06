//! Database-backed checks for durable lifecycle notice identity and delivery.
//!
//! These tests are opt-in because they create rows in a disposable local Postgres database.

use async_trait::async_trait;
use sqlx::postgres::PgConnectOptions;
use sqlx::{PgPool, Row};
use std::str::FromStr;
use std::sync::OnceLock;
use tokio::sync::Mutex;

use sotto_server::db;
use sotto_server::notifications::{
    self, DeliveryOutcome, NoticeChannel, NoticeContent, NoticeError, NoticeIntent, NoticeKind,
    NoticeSender,
};

fn notification_test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

async fn pool_or_skip() -> Option<PgPool> {
    if std::env::var("SOTTO_RUN_DB_TESTS").as_deref() != Ok("1") {
        eprintln!("skipping notification tests: set SOTTO_RUN_DB_TESTS=1");
        return None;
    }
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL for opted-in database tests");
    let options = PgConnectOptions::from_str(&url).expect("parse DATABASE_URL");
    assert!(
        matches!(options.get_host(), "localhost" | "127.0.0.1" | "::1"),
        "refusing notification tests against non-local host: {}",
        options.get_host()
    );
    let pool = db::connect(&url).await.expect("connect to test database");
    db::migrate(&pool).await.expect("apply migrations");
    Some(pool)
}

async fn seed_user(pool: &PgPool, user_id: &str) {
    sqlx::query(
        "INSERT INTO users (id, oauth_provider, oauth_subject) VALUES ($1, 'notification-test', $1) \
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(user_id)
    .execute(pool)
    .await
    .expect("seed notification user");
}

async fn cleanup(pool: &PgPool, user_id: &str) {
    sqlx::query("DELETE FROM cloud_notice_outbox WHERE recipient_user_id=$1")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("clean notification outbox");
    sqlx::query("DELETE FROM cloud_verified_contacts WHERE user_id=$1")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("clean notification contacts");
    sqlx::query("DELETE FROM users WHERE id=$1")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("clean notification user");
}

fn content() -> NoticeContent {
    NoticeContent {
        title: "Renewal failed".into(),
        detail: "Recovery is available until the displayed deadline.".into(),
        effective_at_epoch: Some(1_800_000_000),
        deadline_epoch: Some(1_800_100_000),
        amount_pence: Some(299),
    }
}

fn in_app_intent(user_id: &str, event_key: &str, due_at_epoch: i64) -> NoticeIntent {
    NoticeIntent {
        recipient_user_id: user_id.into(),
        event_key: event_key.into(),
        policy_key: "renewal-failure-v1".into(),
        kind: NoticeKind::FailedRenewal,
        channel: NoticeChannel::InApp,
        contact_id: None,
        due_at_epoch,
        content: content(),
    }
}

#[tokio::test]
async fn enqueue_is_idempotent_and_cancelled_notice_is_not_listed() {
    let _guard = notification_test_lock().lock().await;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    const USER_ID: &str = "notification-test-idempotency";
    cleanup(&pool, USER_ID).await;
    seed_user(&pool, USER_ID).await;

    let first = notifications::enqueue(&pool, &in_app_intent(USER_ID, "event-1", 1_800_000_000))
        .await
        .expect("enqueue first notice");
    let second = notifications::enqueue(&pool, &in_app_intent(USER_ID, "event-1", 1_900_000_000))
        .await
        .expect("enqueue duplicate notice");
    assert_eq!(first, notifications::EnqueueOutcome::Enqueued);
    assert_eq!(second, notifications::EnqueueOutcome::AlreadyQueued);

    let row = sqlx::query(
        "SELECT extract(epoch FROM due_at)::bigint AS due_at, status, attempt_count \
         FROM cloud_notice_outbox WHERE recipient_user_id=$1 AND event_key='event-1'",
    )
    .bind(USER_ID)
    .fetch_one(&pool)
    .await
    .expect("load idempotent notice");
    assert_eq!(row.get::<i64, _>("due_at"), 1_800_000_000);
    assert_eq!(row.get::<String, _>("status"), "pending");
    assert_eq!(row.get::<i32, _>("attempt_count"), 0);

    assert_eq!(
        notifications::cancel(&pool, USER_ID, "event-1", "renewal-failure-v1")
            .await
            .unwrap(),
        1
    );
    assert!(notifications::list_for_user(&pool, USER_ID)
        .await
        .unwrap()
        .is_empty());
    cleanup(&pool, USER_ID).await;
}

struct RetrySender;

#[async_trait]
impl NoticeSender for RetrySender {
    async fn send(&mut self, _delivery: notifications::NoticeDelivery) -> DeliveryOutcome {
        DeliveryOutcome::Retry {
            code: "provider_busy".into(),
        }
    }
}

#[tokio::test]
async fn retry_keeps_lifecycle_due_date_and_records_provider_error() {
    let _guard = notification_test_lock().lock().await;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    const USER_ID: &str = "notification-test-retry";
    cleanup(&pool, USER_ID).await;
    seed_user(&pool, USER_ID).await;
    notifications::record_verified_email(
        &pool,
        "contact-retry",
        USER_ID,
        "person@example.test",
        1_800_000_000,
    )
    .await
    .expect("record verified contact");
    let intent = NoticeIntent {
        recipient_user_id: USER_ID.into(),
        event_key: "event-retry".into(),
        policy_key: "price-change-v1".into(),
        kind: NoticeKind::PriceChange,
        channel: NoticeChannel::Email,
        contact_id: Some("contact-retry".into()),
        due_at_epoch: 1,
        content: content(),
    };
    notifications::enqueue(&pool, &intent)
        .await
        .expect("enqueue retry notice");
    let mut sender = RetrySender;
    assert!(notifications::run_once(&pool, "worker-retry", &mut sender)
        .await
        .unwrap());

    let row = sqlx::query(
        "SELECT extract(epoch FROM due_at)::bigint AS due_at, status, attempt_count, last_error_code \
         FROM cloud_notice_outbox WHERE recipient_user_id=$1 AND event_key='event-retry'",
    )
    .bind(USER_ID)
    .fetch_one(&pool)
    .await
    .expect("load retried notice");
    assert_eq!(row.get::<i64, _>("due_at"), 1);
    assert_eq!(row.get::<String, _>("status"), "pending");
    assert_eq!(row.get::<i32, _>("attempt_count"), 1);
    assert_eq!(row.get::<String, _>("last_error_code"), "provider_busy");
    cleanup(&pool, USER_ID).await;
}

#[tokio::test]
async fn verified_contact_cannot_be_reassigned_to_another_user() {
    let _guard = notification_test_lock().lock().await;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    const OWNER: &str = "notification-test-contact-owner";
    const OTHER: &str = "notification-test-contact-other";
    cleanup(&pool, OWNER).await;
    cleanup(&pool, OTHER).await;
    seed_user(&pool, OWNER).await;
    seed_user(&pool, OTHER).await;
    notifications::record_verified_email(
        &pool,
        "contact-owned",
        OWNER,
        "owner@example.test",
        1_800_000_000,
    )
    .await
    .expect("record owner contact");
    assert!(matches!(
        notifications::record_verified_email(
            &pool,
            "contact-owned",
            OTHER,
            "other@example.test",
            1_800_000_001
        )
        .await,
        Err(NoticeError::InvalidKey("contact belongs to another user"))
    ));
    cleanup(&pool, OWNER).await;
    cleanup(&pool, OTHER).await;
}

#[tokio::test]
async fn deleted_contact_fails_queued_email_without_breaking_the_outbox_row() {
    let _guard = notification_test_lock().lock().await;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    const USER_ID: &str = "notification-test-deleted-contact";
    cleanup(&pool, USER_ID).await;
    seed_user(&pool, USER_ID).await;
    notifications::record_verified_email(
        &pool,
        "contact-deleted",
        USER_ID,
        "person@example.test",
        1_800_000_000,
    )
    .await
    .expect("record verified contact");
    let intent = NoticeIntent {
        recipient_user_id: USER_ID.into(),
        event_key: "event-deleted-contact".into(),
        policy_key: "recovery-v1".into(),
        kind: NoticeKind::RecoveryEnd,
        channel: NoticeChannel::Email,
        contact_id: Some("contact-deleted".into()),
        due_at_epoch: 1,
        content: content(),
    };
    notifications::enqueue(&pool, &intent)
        .await
        .expect("enqueue email notice");
    sqlx::query("DELETE FROM cloud_verified_contacts WHERE contact_id='contact-deleted'")
        .execute(&pool)
        .await
        .expect("delete verified contact");

    let mut sender = RetrySender;
    assert!(
        notifications::run_once(&pool, "worker-deleted-contact", &mut sender)
            .await
            .expect("mark deleted contact unavailable")
    );
    let row = sqlx::query(
        "SELECT status, last_error_code, contact_id FROM cloud_notice_outbox \
         WHERE recipient_user_id=$1 AND event_key='event-deleted-contact'",
    )
    .bind(USER_ID)
    .fetch_one(&pool)
    .await
    .expect("load deleted-contact notice");
    assert_eq!(row.get::<String, _>("status"), "failed");
    assert_eq!(row.get::<String, _>("last_error_code"), "contact_missing");
    assert_eq!(row.get::<Option<String>, _>("contact_id"), None);
    cleanup(&pool, USER_ID).await;
}
