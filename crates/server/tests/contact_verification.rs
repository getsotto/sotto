//! PostgreSQL-backed tests for verified lifecycle contact enrolment.

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgPool, Row};
use std::str::FromStr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::Mutex as AsyncMutex;
use tower::ServiceExt;

use sotto_server::auth::session;
use sotto_server::config::{DeploymentMode, DEFAULT_ORGANISATION_DELETION_RETENTION_DAYS};
use sotto_server::contact_verification::{
    self, ContactVerificationSender, ContactVerificationService, SendOutcome,
    StartVerificationRequest, VerificationMessage,
};
use sotto_server::db;
use sotto_server::notifications;
use sotto_server::state::AppState;

mod support {
    pub mod migrations;
}

use support::migrations::{migrator_before, DisposableDatabase};

fn test_lock() -> &'static AsyncMutex<()> {
    static LOCK: OnceLock<AsyncMutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| AsyncMutex::new(()))
}

async fn pool_or_skip() -> Option<PgPool> {
    if std::env::var("SOTTO_RUN_DB_TESTS").as_deref() != Ok("1") {
        eprintln!("skipping contact verification tests: set SOTTO_RUN_DB_TESTS=1");
        return None;
    }
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL for opted-in database tests");
    let options = PgConnectOptions::from_str(&url).expect("parse DATABASE_URL");
    assert!(
        matches!(options.get_host(), "localhost" | "127.0.0.1" | "::1"),
        "refusing contact tests against non-local host: {}",
        options.get_host()
    );
    let pool = db::connect(&url).await.expect("connect to test database");
    db::migrate(&pool).await.expect("apply migrations");
    Some(pool)
}

async fn seed_user(pool: &PgPool, user_id: &str) {
    sqlx::query("INSERT INTO users(id,oauth_provider,oauth_subject) VALUES($1,'contact-test',$1) ON CONFLICT(id) DO NOTHING")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("seed test user");
}

async fn wait_for_completed_sends(pool: &PgPool, user_id: &str, expected: i64) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let completed: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM cloud_contact_verification_send_attempts AS attempt \
                 JOIN cloud_contact_verification_challenges AS challenge USING (challenge_id) \
                 WHERE challenge.user_id=$1 AND attempt.outcome IN ('accepted','unknown','rejected')",
            )
            .bind(user_id)
            .fetch_one(pool)
            .await
            .expect("count completed contact sends");
            if completed >= expected {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("background contact sends finish");
}

fn app(pool: PgPool, contact_verification: Arc<ContactVerificationService>) -> Router {
    let state = AppState {
        pool,
        deployment_mode: DeploymentMode::Cloud,
        oauth: None,
        oauth_config: None,
        billing: None,
        contact_verification: Some(contact_verification),
        cloud_notice_dispatch_enabled: false,
        resend_webhook_secret: None,
        telemetry_ingest: false,
        organisation_deletion_enabled: false,
        organisation_deletion_retention_days: DEFAULT_ORGANISATION_DELETION_RETENTION_DAYS,
        organisation_deletion_metrics_token: None,
        cloud_operations_metrics_token: None,
        cloud_coverage_repair_token: None,
        provider_refresh_ingest_enabled: false,
        provider_refresh_worker_enabled: false,
        provider_refresh_reconciliation_enabled: false,
        provider_authority_revalidation_enabled: false,
        cloud_sales_enabled: false,
        founding_schedule_worker_enabled: false,
        founding_price_notices_enabled: false,
        billing_recovery_worker_enabled: false,
        sponsored_billing_recovery_worker_enabled: false,
        organisation_deletion_operator_token: None,
        billing_transfer_operator_token: None,
        cloud_action_enforcement_enabled: false,
        cloud_coverage_authority_enforcement_enabled: false,
        cloud_coverage_authority_shadow_enabled: false,
        machine_eligibility_enforcement_enabled: false,
        billing_transfers_enabled: false,
        billing_transfer_execution_enabled: false,
        billing_transfer_setup_enabled: false,
        cloud_retention_dry_run_enabled: false,
        cloud_shared_retention_dry_run_enabled: false,
        cloud_retention_operator_token: None,
        cloud_retention_purge_enabled: false,
        cloud_retention_destructive_token: None,
        cloud_retention_evidence_hmac_key: None,
        cloud_retention_build_sha: None,
    };
    Router::new()
        .merge(contact_verification::router())
        .with_state(state)
}

#[derive(Clone)]
struct RecordingSender {
    pool: PgPool,
    messages: Arc<Mutex<Vec<VerificationMessage>>>,
    outcome: SendOutcome,
    delay: Duration,
}

#[async_trait]
impl ContactVerificationSender for RecordingSender {
    async fn send(&self, message: VerificationMessage) -> SendOutcome {
        tokio::time::sleep(self.delay).await;
        // This query must acquire the only connection. It times out if the caller keeps its
        // pre-send transaction open while awaiting the external sender.
        tokio::time::timeout(
            Duration::from_secs(2),
            sqlx::query_scalar::<_, i32>("SELECT 1").fetch_one(&self.pool),
        )
        .await
        .expect("sender must run after the database transaction releases its connection")
        .expect("sender's independent database probe");
        self.messages
            .lock()
            .expect("sender message lock")
            .push(message);
        self.outcome.clone()
    }
}

fn service(
    pool: PgPool,
    messages: Arc<Mutex<Vec<VerificationMessage>>>,
    outcome: SendOutcome,
) -> ContactVerificationService {
    ContactVerificationService::new(
        true,
        true,
        "https://app.example.test",
        Arc::new(RecordingSender {
            pool,
            messages,
            outcome,
            delay: Duration::ZERO,
        }),
    )
    .expect("test contact service")
}

fn slow_service(
    pool: PgPool,
    messages: Arc<Mutex<Vec<VerificationMessage>>>,
    outcome: SendOutcome,
    delay: Duration,
) -> ContactVerificationService {
    ContactVerificationService::new(
        true,
        true,
        "https://app.example.test",
        Arc::new(RecordingSender {
            pool,
            messages,
            outcome,
            delay,
        }),
    )
    .expect("test contact service")
}

#[tokio::test]
async fn verification_replaces_contact_only_after_user_bound_one_time_proof() {
    let _guard = test_lock().lock().await;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let user_id = format!("contact-user-{}", uuid::Uuid::new_v4());
    let other_user_id = format!("contact-other-{}", uuid::Uuid::new_v4());
    let destination = format!("new-{}@example.test", uuid::Uuid::new_v4());
    let old_destination = format!("old-{}@example.test", uuid::Uuid::new_v4());
    seed_user(&pool, &user_id).await;
    seed_user(&pool, &other_user_id).await;
    notifications::record_verified_email(
        &pool,
        &format!("old-contact-{}", uuid::Uuid::new_v4()),
        &user_id,
        &old_destination,
        1_800_000_000,
    )
    .await
    .expect("seed existing verified contact");

    let messages = Arc::new(Mutex::new(Vec::new()));
    let service = service(
        pool.clone(),
        messages.clone(),
        SendOutcome::Accepted {
            provider_message_id: format!("provider-{}", uuid::Uuid::new_v4()),
        },
    );
    let response = contact_verification::start_verification(
        &pool,
        &service,
        &user_id,
        StartVerificationRequest {
            email: format!(" {} ", destination),
            idempotency_key: format!("start:{}", uuid::Uuid::new_v4()),
        },
    )
    .await
    .expect("start verification");
    assert!(response.accepted);
    wait_for_completed_sends(&pool, &user_id, 1).await;

    let message = messages
        .lock()
        .expect("message lock")
        .pop()
        .expect("message sent");
    let url = url::Url::parse(&message.verification_url).expect("verification URL");
    assert_eq!(url.path(), "/cloud");
    assert!(url.query().is_none());
    let token = url
        .fragment()
        .and_then(|fragment| fragment.strip_prefix("contact_token="))
        .expect("token must be in URL fragment");

    let before = contact_verification::contact_status(&pool, &user_id)
        .await
        .expect("existing contact is still active");
    assert_eq!(
        before.email.as_ref().unwrap().masked_destination,
        "o***@example.test"
    );
    assert!(before.pending_expires_at_epoch.is_some());

    assert!(
        !contact_verification::confirm_verification(&pool, &other_user_id, token)
            .await
            .expect("wrong account is an ordinary invalid token")
    );
    assert!(
        contact_verification::confirm_verification(&pool, &user_id, token)
            .await
            .expect("valid confirmation query")
    );
    assert!(
        !contact_verification::confirm_verification(&pool, &user_id, token)
            .await
            .expect("replay is not accepted")
    );

    let after = contact_verification::contact_status(&pool, &user_id)
        .await
        .expect("new verified contact is active");
    assert_eq!(
        after.email.as_ref().unwrap().masked_destination,
        "n***@example.test"
    );
    assert_eq!(after.pending_expires_at_epoch, None);

    let destinations: Vec<String> = sqlx::query_scalar(
        "SELECT destination FROM cloud_verified_contacts WHERE user_id=$1 AND revoked_at IS NULL",
    )
    .bind(&user_id)
    .fetch_all(&pool)
    .await
    .expect("active contact destinations");
    assert_eq!(destinations, vec![destination]);

    let token_digest: Vec<u8> = sqlx::query_scalar(
        "SELECT token_digest FROM cloud_contact_verification_challenges WHERE user_id=$1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("persisted token digest");
    assert_eq!(token_digest.len(), 32);
    assert_ne!(token_digest.as_slice(), token.as_bytes());
}

#[tokio::test]
async fn large_notice_catchup_does_not_block_contact_verification() {
    let _guard = test_lock().lock().await;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let user_id = format!("contact-large-catchup-{}", uuid::Uuid::new_v4());
    let destination = format!("catchup-{}@example.test", uuid::Uuid::new_v4());
    seed_user(&pool, &user_id).await;
    let now_epoch: i64 = sqlx::query_scalar("SELECT extract(epoch FROM now())::bigint")
        .fetch_one(&pool)
        .await
        .expect("read database time");
    for index in 0..101 {
        notifications::enqueue(
            &pool,
            &notifications::NoticeIntent {
                recipient_user_id: user_id.clone(),
                event_key: format!("catchup-overflow-{index}"),
                policy_key: "catchup-overflow-v1".into(),
                kind: notifications::NoticeKind::CheckoutOutcome,
                channel: notifications::NoticeChannel::InApp,
                contact_id: None,
                scope_id: None,
                due_at_epoch: now_epoch - index,
                content: notifications::NoticeContent {
                    title: "A recent Cloud update".into(),
                    detail: "Review your Cloud account for this update.".into(),
                    effective_at_epoch: None,
                    deadline_epoch: None,
                    amount_pence: None,
                    price_interval: None,
                },
            },
        )
        .await
        .expect("enqueue recent in-app notice");
    }

    let messages = Arc::new(Mutex::new(Vec::new()));
    let service = service(
        pool.clone(),
        messages.clone(),
        SendOutcome::Accepted {
            provider_message_id: format!("provider-{}", uuid::Uuid::new_v4()),
        },
    );
    contact_verification::start_verification(
        &pool,
        &service,
        &user_id,
        StartVerificationRequest {
            email: destination.clone(),
            idempotency_key: format!("large-catchup:{}", uuid::Uuid::new_v4()),
        },
    )
    .await
    .expect("start verification despite a large notice history");
    wait_for_completed_sends(&pool, &user_id, 1).await;
    let token = {
        let sent = messages.lock().expect("message lock");
        url::Url::parse(&sent[0].verification_url)
            .expect("verification URL")
            .fragment()
            .and_then(|fragment| fragment.strip_prefix("contact_token="))
            .expect("token fragment")
            .to_owned()
    };

    assert!(
        contact_verification::confirm_verification(&pool, &user_id, &token)
            .await
            .expect("verification must commit even when the notice catch-up is capped")
    );
    assert_eq!(
        contact_verification::contact_status(&pool, &user_id)
            .await
            .expect("verified contact status")
            .email
            .expect("verified email is active")
            .masked_destination,
        "c***@example.test"
    );
    let caught_up: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM cloud_notice_outbox WHERE recipient_user_id=$1 \
         AND channel='email' AND policy_key='catchup-overflow-v1'",
    )
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("count bounded email catch-up");
    assert_eq!(caught_up, 100);

    sqlx::query("DELETE FROM cloud_notice_outbox WHERE recipient_user_id=$1")
        .bind(&user_id)
        .execute(&pool)
        .await
        .expect("clean catch-up notices");
    sqlx::query(
        "DELETE FROM cloud_contact_verification_send_attempts WHERE challenge_id IN ( \
         SELECT challenge_id FROM cloud_contact_verification_challenges WHERE user_id=$1)",
    )
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("clean verification attempts");
    sqlx::query("DELETE FROM cloud_contact_verification_challenges WHERE user_id=$1")
        .bind(&user_id)
        .execute(&pool)
        .await
        .expect("clean verification challenges");
    sqlx::query("DELETE FROM cloud_verified_contacts WHERE user_id=$1")
        .bind(&user_id)
        .execute(&pool)
        .await
        .expect("clean verified contacts");
    sqlx::query("DELETE FROM users WHERE id=$1")
        .bind(&user_id)
        .execute(&pool)
        .await
        .expect("clean test user");
}

#[tokio::test]
async fn resend_is_idempotent_and_unknown_delivery_remains_confirmable() {
    let _guard = test_lock().lock().await;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let user_id = format!("contact-retry-{}", uuid::Uuid::new_v4());
    let destination = format!("retry-{}@example.test", uuid::Uuid::new_v4());
    seed_user(&pool, &user_id).await;
    let messages = Arc::new(Mutex::new(Vec::new()));
    let service = service(pool.clone(), messages.clone(), SendOutcome::Unknown);
    let key = format!("request:{}", uuid::Uuid::new_v4());
    for _ in 0..2 {
        let outcome = contact_verification::start_verification(
            &pool,
            &service,
            &user_id,
            StartVerificationRequest {
                email: destination.clone(),
                idempotency_key: key.clone(),
            },
        )
        .await
        .expect("idempotent resend request");
        assert!(outcome.accepted);
    }
    wait_for_completed_sends(&pool, &user_id, 1).await;
    let token = {
        let sent = messages.lock().expect("message lock");
        assert_eq!(sent.len(), 1);
        url::Url::parse(&sent[0].verification_url)
            .expect("message URL")
            .fragment()
            .and_then(|fragment| fragment.strip_prefix("contact_token="))
            .expect("token fragment")
            .to_owned()
    };
    let challenge_status: String = sqlx::query_scalar(
        "SELECT status FROM cloud_contact_verification_challenges WHERE user_id=$1",
    )
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("unknown provider result remains available for proof");
    assert_eq!(challenge_status, "delivery_unknown");
    assert!(
        contact_verification::confirm_verification(&pool, &user_id, &token)
            .await
            .expect("delivered despite ambiguous transport result")
    );

    let attempt = sqlx::query(
        "SELECT outcome,message_id FROM cloud_contact_verification_send_attempts \
         WHERE challenge_id=(SELECT challenge_id FROM cloud_contact_verification_challenges WHERE user_id=$1)",
    )
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("durable send attempt");
    assert_eq!(attempt.try_get::<String, _>("outcome").unwrap(), "unknown");
}

#[tokio::test]
async fn starting_with_another_users_active_destination_sends_nothing() {
    let _guard = test_lock().lock().await;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let owner = format!("contact-owner-{}", uuid::Uuid::new_v4());
    let requester = format!("contact-requester-{}", uuid::Uuid::new_v4());
    let destination = format!("taken-{}@example.test", uuid::Uuid::new_v4());
    seed_user(&pool, &owner).await;
    seed_user(&pool, &requester).await;
    notifications::record_verified_email(
        &pool,
        &format!("owned-contact-{}", uuid::Uuid::new_v4()),
        &owner,
        &destination,
        1_800_000_000,
    )
    .await
    .expect("seed owned destination");
    let messages = Arc::new(Mutex::new(Vec::new()));
    let service = service(
        pool.clone(),
        messages.clone(),
        SendOutcome::Accepted {
            provider_message_id: format!("provider-{}", uuid::Uuid::new_v4()),
        },
    );
    let result = contact_verification::start_verification(
        &pool,
        &service,
        &requester,
        StartVerificationRequest {
            email: destination.to_uppercase(),
            idempotency_key: format!("collision:{}", uuid::Uuid::new_v4()),
        },
    )
    .await
    .expect("same public outcome for unavailable destination");
    assert!(result.accepted);
    assert!(messages.lock().expect("messages").is_empty());
    assert!(contact_verification::contact_status(&pool, &requester)
        .await
        .expect("requester's public status")
        .pending_expires_at_epoch
        .is_some());
}

#[tokio::test]
async fn account_and_destination_send_limits_are_enforced() {
    let _guard = test_lock().lock().await;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let user_id = format!("contact-limits-{}", uuid::Uuid::new_v4());
    let run_id = uuid::Uuid::new_v4();
    seed_user(&pool, &user_id).await;
    let messages = Arc::new(Mutex::new(Vec::new()));
    let service = service(
        pool.clone(),
        messages.clone(),
        SendOutcome::Accepted {
            provider_message_id: format!("provider-{}", uuid::Uuid::new_v4()),
        },
    );
    for request in 0..5 {
        contact_verification::start_verification(
            &pool,
            &service,
            &user_id,
            StartVerificationRequest {
                email: format!("person-{run_id}-{request}@example.test"),
                idempotency_key: format!("account-limit:{}:{request}", uuid::Uuid::new_v4()),
            },
        )
        .await
        .expect("five account requests fit the configured hourly limit");
    }
    assert!(matches!(
        contact_verification::start_verification(
            &pool,
            &service,
            &user_id,
            StartVerificationRequest {
                email: "sixth@example.test".into(),
                idempotency_key: format!("account-limit:{}:sixth", uuid::Uuid::new_v4()),
            },
        )
        .await,
        Err(sotto_server::error::Error::RateLimited(_))
    ));
    wait_for_completed_sends(&pool, &user_id, 5).await;
    assert_eq!(messages.lock().expect("messages").len(), 5);

    let destination = format!("contact-destination-{}@example.test", uuid::Uuid::new_v4());
    let mut destination_users = Vec::new();
    for request in 0..3 {
        let next_user = format!("contact-destination-user-{}", uuid::Uuid::new_v4());
        seed_user(&pool, &next_user).await;
        contact_verification::start_verification(
            &pool,
            &service,
            &next_user,
            StartVerificationRequest {
                email: destination.clone(),
                idempotency_key: format!("destination-limit:{}:{request}", uuid::Uuid::new_v4()),
            },
        )
        .await
        .expect("three destination requests fit the configured hourly limit");
        destination_users.push(next_user);
    }
    let last_user = format!("contact-destination-user-{}", uuid::Uuid::new_v4());
    seed_user(&pool, &last_user).await;
    assert!(matches!(
        contact_verification::start_verification(
            &pool,
            &service,
            &last_user,
            StartVerificationRequest {
                email: destination,
                idempotency_key: format!("destination-limit:{}:fourth", uuid::Uuid::new_v4()),
            },
        )
        .await,
        Err(sotto_server::error::Error::RateLimited(_))
    ));
    for destination_user in destination_users {
        wait_for_completed_sends(&pool, &destination_user, 1).await;
    }
}

#[tokio::test]
async fn challenge_expiry_and_contact_revoke_are_user_scoped() {
    let _guard = test_lock().lock().await;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let user_id = format!("contact-revoke-{}", uuid::Uuid::new_v4());
    let other_user_id = format!("contact-revoke-other-{}", uuid::Uuid::new_v4());
    let destination = format!("revoke-{}@example.test", uuid::Uuid::new_v4());
    seed_user(&pool, &user_id).await;
    seed_user(&pool, &other_user_id).await;
    notifications::record_verified_email(
        &pool,
        &format!("revoke-contact-{}", uuid::Uuid::new_v4()),
        &user_id,
        &destination,
        1_800_000_000,
    )
    .await
    .expect("seed contact");
    let contact_id: String = sqlx::query_scalar(
        "SELECT contact_id FROM cloud_verified_contacts WHERE user_id=$1 AND revoked_at IS NULL",
    )
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("contact id");
    contact_verification::revoke_verified_contact(&pool, &other_user_id)
        .await
        .expect("another user revoke is a no-op");
    let still_active: bool = sqlx::query_scalar(
        "SELECT revoked_at IS NULL FROM cloud_verified_contacts WHERE contact_id=$1",
    )
    .bind(&contact_id)
    .fetch_one(&pool)
    .await
    .expect("contact still active");
    assert!(still_active);
    contact_verification::revoke_verified_contact(&pool, &user_id)
        .await
        .expect("owner may revoke");
    let revoked: bool = sqlx::query_scalar(
        "SELECT revoked_at IS NOT NULL FROM cloud_verified_contacts WHERE contact_id=$1",
    )
    .bind(&contact_id)
    .fetch_one(&pool)
    .await
    .expect("contact revoked");
    assert!(revoked);
}

#[tokio::test]
async fn expired_challenges_cannot_be_used_and_idempotency_keys_bind_the_address() {
    let _guard = test_lock().lock().await;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let user_id = format!("contact-expiry-{}", uuid::Uuid::new_v4());
    let destination = format!("expire-{}@example.test", uuid::Uuid::new_v4());
    seed_user(&pool, &user_id).await;
    let messages = Arc::new(Mutex::new(Vec::new()));
    let service = service(
        pool.clone(),
        messages.clone(),
        SendOutcome::Accepted {
            provider_message_id: format!("provider-{}", uuid::Uuid::new_v4()),
        },
    );
    let key = format!("expiring:{}", uuid::Uuid::new_v4());
    contact_verification::start_verification(
        &pool,
        &service,
        &user_id,
        StartVerificationRequest {
            email: destination.clone(),
            idempotency_key: key.clone(),
        },
    )
    .await
    .expect("start challenge");
    wait_for_completed_sends(&pool, &user_id, 1).await;
    assert!(matches!(
        contact_verification::start_verification(
            &pool,
            &service,
            &user_id,
            StartVerificationRequest {
                email: format!("different-{}@example.test", uuid::Uuid::new_v4()),
                idempotency_key: key,
            },
        )
        .await,
        Err(sotto_server::error::Error::Conflict(_))
    ));
    let token = url::Url::parse(
        &messages
            .lock()
            .expect("message lock")
            .last()
            .expect("message sent")
            .verification_url,
    )
    .expect("message URL")
    .fragment()
    .and_then(|fragment| fragment.strip_prefix("contact_token="))
    .expect("fragment token")
    .to_owned();
    sqlx::query(
        "UPDATE cloud_contact_verification_challenges \
         SET created_at=now()-interval '2 seconds', expires_at=now()-interval '1 second' \
         WHERE user_id=$1 AND status='pending'",
    )
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("expire challenge fixture");
    assert!(
        !contact_verification::confirm_verification(&pool, &user_id, &token)
            .await
            .expect("expired token is an ordinary failed proof")
    );
}

#[tokio::test]
async fn provider_io_runs_without_a_held_database_connection() {
    let _guard = test_lock().lock().await;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let options = PgConnectOptions::from_str(&url).expect("parse DATABASE_URL");
    let single_connection = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .expect("single-connection verification pool");
    db::migrate(&single_connection)
        .await
        .expect("migrate single-connection pool");
    let user_id = format!("contact-one-connection-{}", uuid::Uuid::new_v4());
    let destination = format!("pool-{}@example.test", uuid::Uuid::new_v4());
    seed_user(&single_connection, &user_id).await;
    let messages = Arc::new(Mutex::new(Vec::new()));
    let service = service(
        single_connection.clone(),
        messages.clone(),
        SendOutcome::Accepted {
            provider_message_id: format!("provider-{}", uuid::Uuid::new_v4()),
        },
    );
    contact_verification::start_verification(
        &single_connection,
        &service,
        &user_id,
        StartVerificationRequest {
            email: destination,
            idempotency_key: format!("pool:{}", uuid::Uuid::new_v4()),
        },
    )
    .await
    .expect("sender can borrow released connection");
    wait_for_completed_sends(&single_connection, &user_id, 1).await;
    single_connection.close().await;
    pool.close().await;
}

#[tokio::test]
async fn contact_routes_require_a_session_and_send_through_the_authenticated_route() {
    let _guard = test_lock().lock().await;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let user_id = format!("contact-http-{}", uuid::Uuid::new_v4());
    let destination = format!("http-{}@example.test", uuid::Uuid::new_v4());
    seed_user(&pool, &user_id).await;
    let messages = Arc::new(Mutex::new(Vec::new()));
    let service = Arc::new(service(
        pool.clone(),
        messages.clone(),
        SendOutcome::Accepted {
            provider_message_id: format!("provider-{}", uuid::Uuid::new_v4()),
        },
    ));
    let router = app(pool.clone(), service);

    let unauthorized = router
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/account/contacts")
                .body(Body::empty())
                .expect("unauthorized request"),
        )
        .await
        .expect("unauthorized response");
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    assert!(messages.lock().expect("messages").is_empty());

    let token = session::issue(&pool, &user_id)
        .await
        .expect("issue session");
    let status = router
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/account/contacts")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .expect("authenticated status request"),
        )
        .await
        .expect("authenticated status response");
    assert_eq!(status.status(), StatusCode::OK);
    assert_eq!(status.headers()["cache-control"], "no-store");

    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/account/contacts/verification")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "email": destination,
                        "idempotency_key": format!("http:{}", uuid::Uuid::new_v4()),
                    })
                    .to_string(),
                ))
                .expect("authenticated request"),
        )
        .await
        .expect("authenticated response");
    assert_eq!(response.status(), StatusCode::OK);
    wait_for_completed_sends(&pool, &user_id, 1).await;
    assert_eq!(messages.lock().expect("messages").len(), 1);
}

#[tokio::test]
async fn taken_and_available_addresses_return_without_waiting_for_email_delivery() {
    let _guard = test_lock().lock().await;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let owner = format!("contact-timing-owner-{}", uuid::Uuid::new_v4());
    let free_user = format!("contact-timing-free-{}", uuid::Uuid::new_v4());
    let taken_user = format!("contact-timing-taken-{}", uuid::Uuid::new_v4());
    let taken_destination = format!("timing-{}@example.test", uuid::Uuid::new_v4());
    let free_destination = format!("free-{}@example.test", uuid::Uuid::new_v4());
    seed_user(&pool, &owner).await;
    seed_user(&pool, &free_user).await;
    seed_user(&pool, &taken_user).await;
    notifications::record_verified_email(
        &pool,
        &format!("timing-contact-{}", uuid::Uuid::new_v4()),
        &owner,
        &taken_destination,
        1_800_000_000,
    )
    .await
    .expect("seed owned destination");

    let messages = Arc::new(Mutex::new(Vec::new()));
    let service = slow_service(
        pool.clone(),
        messages.clone(),
        SendOutcome::Accepted {
            provider_message_id: format!("provider-{}", uuid::Uuid::new_v4()),
        },
        Duration::from_millis(400),
    );

    let started = Instant::now();
    contact_verification::start_verification(
        &pool,
        &service,
        &free_user,
        StartVerificationRequest {
            email: free_destination,
            idempotency_key: format!("timing-free:{}", uuid::Uuid::new_v4()),
        },
    )
    .await
    .expect("available address request");
    let free_elapsed = started.elapsed();

    let started = Instant::now();
    contact_verification::start_verification(
        &pool,
        &service,
        &taken_user,
        StartVerificationRequest {
            email: taken_destination,
            idempotency_key: format!("timing-taken:{}", uuid::Uuid::new_v4()),
        },
    )
    .await
    .expect("taken address request");
    let taken_elapsed = started.elapsed();

    assert!(
        free_elapsed < Duration::from_millis(300),
        "available destination waited for sender: {free_elapsed:?}"
    );
    assert!(
        taken_elapsed < Duration::from_millis(300),
        "taken destination response was unexpectedly slow: {taken_elapsed:?}"
    );
    assert!(
        free_elapsed.abs_diff(taken_elapsed) < Duration::from_millis(250),
        "destination response times diverged: free={free_elapsed:?}, taken={taken_elapsed:?}"
    );

    wait_for_completed_sends(&pool, &free_user, 1).await;
    assert_eq!(messages.lock().expect("messages").len(), 1);
}

#[tokio::test]
async fn migration_revokes_ambiguous_legacy_email_owners_before_unique_index() {
    let Some(database) = DisposableDatabase::create().await else {
        return;
    };
    migrator_before(68)
        .run(&database.pool)
        .await
        .expect("apply migrations before contact verification");

    seed_user(&database.pool, "legacy-contact-owner-a").await;
    seed_user(&database.pool, "legacy-contact-owner-b").await;
    seed_user(&database.pool, "legacy-contact-owner-c").await;
    for (contact_id, user_id, destination) in [
        (
            "legacy-shared-contact-a",
            "legacy-contact-owner-a",
            "Shared@example.test",
        ),
        (
            "legacy-shared-contact-b",
            "legacy-contact-owner-b",
            "shared@example.test",
        ),
        (
            "legacy-unique-contact",
            "legacy-contact-owner-c",
            "unique@example.test",
        ),
    ] {
        sqlx::query(
            "INSERT INTO cloud_verified_contacts \
             (contact_id,user_id,channel,destination,verified_at) \
             VALUES ($1,$2,'email',$3,now())",
        )
        .bind(contact_id)
        .bind(user_id)
        .bind(destination)
        .execute(&database.pool)
        .await
        .expect("seed legacy verified contact");
    }

    db::migrate(&database.pool)
        .await
        .expect("migration revokes duplicate legacy addresses before indexing");

    let active_ids: Vec<String> = sqlx::query_scalar(
        "SELECT contact_id FROM cloud_verified_contacts WHERE revoked_at IS NULL ORDER BY contact_id",
    )
    .fetch_all(&database.pool)
    .await
    .expect("load active contacts after upgrade");
    assert_eq!(active_ids, ["legacy-unique-contact"]);

    let revoked_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM cloud_verified_contacts \
         WHERE contact_id IN ('legacy-shared-contact-a','legacy-shared-contact-b') \
           AND revoked_at IS NOT NULL",
    )
    .fetch_one(&database.pool)
    .await
    .expect("count revoked ambiguous contacts");
    assert_eq!(revoked_count, 2);

    database.cleanup().await;
}
