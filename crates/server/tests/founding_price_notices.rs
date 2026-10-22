//! Database-backed founding-price notice lifecycle coverage.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use sotto_server::{db, founding_price_notices, founding_schedule, notifications};
use sqlx::{Column, PgPool, Row};
use tower::ServiceExt;

#[allow(dead_code)]
mod support {
    pub mod migrations;
}

use support::migrations::DisposableDatabase;

fn app(pool: PgPool, operator_token: Option<&str>) -> Router {
    use sotto_server::config::{DeploymentMode, DEFAULT_ORGANISATION_DELETION_RETENTION_DAYS};
    use sotto_server::state::AppState;

    sotto_server::app(AppState {
        pool,
        deployment_mode: DeploymentMode::Cloud,
        oauth: None,
        oauth_config: None,
        billing: None,
        telemetry_ingest: false,
        contact_verification: None,
        cloud_notice_dispatch_enabled: false,
        resend_webhook_secret: None,
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
        billing_transfer_operator_token: operator_token.map(str::to_owned),
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
    })
}

async fn seed_personal_award(pool: &PgPool, award_id: &str, user_id: &str, boundary: i64) {
    sqlx::query(
        "INSERT INTO users (id, oauth_provider, oauth_subject) VALUES ($1,'price-notice-test',$1)",
    )
    .bind(user_id)
    .execute(pool)
    .await
    .expect("seed user");
    sqlx::query(
        "INSERT INTO billing_operations \
         (operation_id,idempotency_key,request_hash,actor_user_id,payer_id,beneficiary_id,offer,quote_version,quote_expires_at_epoch,provider_idempotency_key) \
         VALUES ($1,$1,'hash',$2,$2,$2,'founding_monthly',1,1900000000,$1)",
    )
    .bind(format!("operation-{award_id}")).bind(user_id).execute(pool).await.expect("seed billing operation");
    sqlx::query(
        "INSERT INTO billing_personal_accounts \
         (user_id,operation_id,offer,stripe_customer_id,stripe_subscription_id,state,pending_expires_at_epoch) \
         VALUES ($1,$2,'founding_monthly',$3,$4,'active',1900000000)",
    )
    .bind(user_id)
    .bind(format!("operation-{award_id}"))
    .bind(format!("cus-{award_id}"))
    .bind(format!("sub-{award_id}"))
    .execute(pool).await.expect("seed personal account");
    sqlx::query(
        "INSERT INTO billing_founding_awards \
         (award_id,beneficiary_id,payer_kind,payer_id,offer,cohort_ordinal,original_start_date,original_end_date,original_start_epoch,original_end_epoch) \
         VALUES ($1,$2,'personal',$2,'founding_monthly',$3,'2026-01-01','2027-01-01',$4,$5)",
    )
    .bind(award_id).bind(user_id).bind(award_ordinal(award_id)).bind(boundary - 366 * 24 * 60 * 60).bind(boundary)
    .execute(pool).await.expect("seed founding award");
    sqlx::query(
        "INSERT INTO billing_founding_schedule_jobs \
         (job_id,award_id,beneficiary_id,payer_kind,payer_id,provider_subscription_id,provider_idempotency_key,state,provider_schedule_id,price_notice_eligible) \
         VALUES ($1,$2,$3,'personal',$3,$4,$5,'scheduled','sched_notice',TRUE)",
    )
    .bind(format!("schedule-{award_id}"))
    .bind(award_id)
    .bind(user_id)
    .bind(format!("sub-{award_id}"))
    .bind(format!("schedule-key-{award_id}"))
    .execute(pool).await.expect("seed verified schedule");
}

fn award_ordinal(award_id: &str) -> i64 {
    // Keep the fixture deterministic while remaining unique across its two cases.
    if award_id.ends_with("late") {
        2
    } else {
        1
    }
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

#[tokio::test]
async fn verified_personal_transition_queues_one_exact_future_notice_and_late_discovery_is_reviewable(
) {
    let Some(database) = DisposableDatabase::create().await else {
        eprintln!("skipping founding price notice tests: set SOTTO_RUN_DB_TESTS=1");
        return;
    };
    let pool = &database.pool;
    db::migrate(pool).await.expect("apply migrations");
    let now = now_epoch();
    let boundary = now + 120 * 24 * 60 * 60;
    seed_personal_award(pool, "award-price-notice", "user-price-notice", boundary).await;

    assert!(founding_price_notices::run_once(pool, now)
        .await
        .expect("reconcile scheduled award"));
    assert!(!founding_price_notices::run_once(pool, now)
        .await
        .expect("idempotent replay"));
    let rows = sqlx::query(
        "SELECT kind, event_key, extract(epoch from due_at)::bigint AS due_at, payload, status FROM cloud_notice_outbox WHERE recipient_user_id='user-price-notice'",
    )
    .fetch_all(pool).await.expect("load queued notice");
    assert_eq!(
        rows.len(),
        1,
        "replays must not create duplicate notice rows"
    );
    assert_eq!(rows[0].get::<String, _>("kind"), "price_change");
    assert_eq!(
        rows[0].get::<String, _>("event_key"),
        format!("founding-price-change:award-price-notice:{boundary}")
    );
    assert_eq!(rows[0].get::<String, _>("status"), "pending");
    assert_eq!(
        rows[0].get::<i64, _>("due_at"),
        boundary - notifications::STANDARD_MONTHLY_PRICE_NOTICE_LEAD_SECONDS
    );
    let payload: serde_json::Value = rows[0].get("payload");
    assert_eq!(payload["amount_pence"], 299);
    assert_eq!(payload["price_interval"], "month");
    assert_eq!(payload["effective_at_epoch"], boundary);
    assert!(
        notifications::list_for_user(pool, "user-price-notice")
            .await
            .unwrap()
            .is_empty(),
        "future notice must remain hidden"
    );

    let notice_due_at = boundary - notifications::STANDARD_MONTHLY_PRICE_NOTICE_LEAD_SECONDS;
    sqlx::query(
        "UPDATE billing_founding_price_notice_state SET next_reconcile_at=now() \
         WHERE award_id='award-price-notice'",
    )
    .execute(pool)
    .await
    .expect("make queued notice due for reconciliation");
    assert!(founding_price_notices::run_once(pool, notice_due_at + 31)
        .await
        .expect("reconcile after the notice due time"));
    let due_state: String = sqlx::query_scalar(
        "SELECT outcome FROM billing_founding_price_notice_state \
         WHERE award_id='award-price-notice'",
    )
    .fetch_one(pool)
    .await
    .expect("load due notice state");
    assert_eq!(due_state, "queued");
    let outbox_status: String = sqlx::query_scalar(
        "SELECT status FROM cloud_notice_outbox WHERE recipient_user_id='user-price-notice'",
    )
    .fetch_one(pool)
    .await
    .expect("load due outbox status");
    assert_eq!(outbox_status, "pending");

    // Simulate the notice's scheduled time passing in the database clock as well as the
    // worker's explicit clock, then prove it remains visible to the recipient.
    sqlx::query(
        "UPDATE cloud_notice_outbox SET due_at=now()-interval '1 second', \
             available_at=now()-interval '1 second' \
         WHERE recipient_user_id='user-price-notice'",
    )
    .execute(pool)
    .await
    .expect("make the notice visible");
    let visible = notifications::list_for_user(pool, "user-price-notice")
        .await
        .expect("list due notice");
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].content.title, "Your founding price is changing");

    let late_boundary = now + notifications::STANDARD_MONTHLY_PRICE_NOTICE_LEAD_SECONDS - 1;
    seed_personal_award(
        pool,
        "award-price-notice-late",
        "user-price-notice-late",
        late_boundary,
    )
    .await;
    let inventory = sqlx::query(include_str!(
        "../../../scripts/founding-price-notice-inventory.sql"
    ))
    .fetch_all(pool)
    .await
    .expect("aggregate notice inventory query is valid");
    assert_eq!(
        inventory[0]
            .columns()
            .iter()
            .map(|column| column.name())
            .collect::<Vec<_>>(),
        vec![
            "cohort",
            "award_count",
            "not_eligible_for_automatic_notice",
            "notice_queued",
            "notice_late_unresolved",
            "notice_schedule_unresolved",
        ],
        "the operator inventory exposes aggregate fields only"
    );
    assert_eq!(
        inventory
            .iter()
            .map(|row| row.get::<i64, _>("award_count"))
            .sum::<i64>(),
        2
    );
    let router = app(pool.clone(), Some("founding-notice-operator"));
    let denied = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/ops/billing/founding-price-notices")
                .header("authorization", "Bearer wrong-token")
                .body(Body::empty())
                .expect("build unauthorised operator request"),
        )
        .await
        .expect("operator response");
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
    let response = router
        .oneshot(
            Request::builder()
                .uri("/ops/billing/founding-price-notices")
                .header("authorization", "Bearer founding-notice-operator")
                .body(Body::empty())
                .expect("build operator summary request"),
        )
        .await
        .expect("operator summary response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 16 * 1024)
        .await
        .expect("read operator summary");
    let summary: serde_json::Value =
        serde_json::from_slice(&body).expect("decode aggregate-only operator summary");
    assert_eq!(summary["queued"], 1);
    assert_eq!(summary["late_unresolved"], 0);
    assert_eq!(
        summary
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        vec![
            "late_reasons",
            "late_unresolved",
            "queued",
            "retired",
            "schedule_unresolved",
        ],
        "the response contains aggregate counts only"
    );
    assert!(founding_price_notices::run_once(pool, now)
        .await
        .expect("reconcile late boundary"));
    let late: (String, String) = sqlx::query_as(
        "SELECT outcome, reason_code FROM billing_founding_price_notice_state WHERE award_id='award-price-notice-late'",
    )
    .fetch_one(pool).await.expect("load late review state");
    assert_eq!(
        late,
        ("late_unresolved".into(), "notice_window_elapsed".into())
    );
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM cloud_notice_outbox WHERE recipient_user_id='user-price-notice-late'",
    )
    .fetch_one(pool)
    .await
    .expect("late awards never get a false notice");
    assert_eq!(count, 0);

    database.cleanup().await;
}

#[tokio::test]
async fn eligible_award_without_boundary_is_recorded_and_removed_from_reconciliation() {
    let Some(database) = DisposableDatabase::create().await else {
        eprintln!("skipping founding price notice tests: set SOTTO_RUN_DB_TESTS=1");
        return;
    };
    let pool = &database.pool;
    db::migrate(pool).await.expect("apply migrations");
    let now = now_epoch();
    seed_personal_award(
        pool,
        "award-price-notice-missing",
        "user-price-notice-missing",
        now + 120 * 24 * 60 * 60,
    )
    .await;
    sqlx::query(
        "UPDATE billing_founding_awards SET original_start_epoch=NULL, original_end_epoch=NULL \
         WHERE award_id='award-price-notice-missing'",
    )
    .execute(pool)
    .await
    .expect("simulate a malformed verified award");

    assert!(founding_price_notices::run_once(pool, now)
        .await
        .expect("record malformed award for operator review"));
    let state: (Option<i64>, String, String) = sqlx::query_as(
        "SELECT transition_epoch, outcome, reason_code \
         FROM billing_founding_price_notice_state \
         WHERE award_id='award-price-notice-missing'",
    )
    .fetch_one(pool)
    .await
    .expect("load malformed award review state");
    assert_eq!(
        state,
        (
            None,
            "schedule_unresolved".into(),
            "schedule_not_verified".into()
        )
    );
    let eligible: bool = sqlx::query_scalar(
        "SELECT price_notice_eligible FROM billing_founding_schedule_jobs \
         WHERE award_id='award-price-notice-missing'",
    )
    .fetch_one(pool)
    .await
    .expect("load malformed award eligibility");
    assert!(!eligible);
    assert!(!founding_price_notices::run_once(pool, now)
        .await
        .expect("malformed award no longer occupies a batch slot"));

    database.cleanup().await;
}

#[tokio::test]
async fn one_award_failure_does_not_stop_the_rest_of_the_notice_batch() {
    let Some(database) = DisposableDatabase::create().await else {
        eprintln!("skipping founding price notice tests: set SOTTO_RUN_DB_TESTS=1");
        return;
    };
    let pool = &database.pool;
    db::migrate(pool).await.expect("apply migrations");
    let now = now_epoch();
    let boundary = now + 120 * 24 * 60 * 60;
    seed_personal_award(pool, "award-batch-a", "user-batch-a", boundary).await;
    seed_personal_award(pool, "award-batch-late", "user-batch-late", boundary).await;
    sqlx::query(
        "UPDATE billing_founding_schedule_jobs SET updated_at='2000-01-01' \
         WHERE award_id IN ('award-batch-a','award-batch-late')",
    )
    .execute(pool)
    .await
    .expect("make candidate order deterministic");
    sqlx::query(
        "CREATE FUNCTION reject_one_test_notice() RETURNS trigger AS $$ \
         BEGIN IF NEW.recipient_user_id='user-batch-a' THEN \
           RAISE EXCEPTION 'synthetic notice enqueue failure'; \
         END IF; RETURN NEW; END; $$ LANGUAGE plpgsql",
    )
    .execute(pool)
    .await
    .expect("create scoped enqueue fault");
    sqlx::query(
        "CREATE TRIGGER reject_one_test_notice BEFORE INSERT ON cloud_notice_outbox \
         FOR EACH ROW EXECUTE FUNCTION reject_one_test_notice()",
    )
    .execute(pool)
    .await
    .expect("install scoped enqueue fault");

    assert!(founding_price_notices::run_once(pool, now).await.is_err());
    let second_outcome: String = sqlx::query_scalar(
        "SELECT outcome FROM billing_founding_price_notice_state \
         WHERE award_id='award-batch-late'",
    )
    .fetch_one(pool)
    .await
    .expect("later award is reconciled despite the first failure");
    assert_eq!(second_outcome, "queued");
    let failed_state_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM billing_founding_price_notice_state \
         WHERE award_id='award-batch-a'",
    )
    .fetch_one(pool)
    .await
    .expect("failed transaction leaves no partial state");
    assert_eq!(failed_state_count, 0);

    database.cleanup().await;
}

#[tokio::test]
async fn sponsored_transition_notifies_only_current_billing_admins() {
    let Some(database) = DisposableDatabase::create().await else {
        eprintln!("skipping founding price notice tests: set SOTTO_RUN_DB_TESTS=1");
        return;
    };
    let pool = &database.pool;
    db::migrate(pool).await.expect("apply migrations");
    let now = now_epoch();
    let boundary = now + 150 * 24 * 60 * 60;
    for user in [
        "notice-owner",
        "notice-admin",
        "notice-member",
        "notice-beneficiary",
    ] {
        sqlx::query("INSERT INTO users (id, oauth_provider, oauth_subject) VALUES ($1,'price-notice-test',$1)")
            .bind(user).execute(pool).await.expect("seed sponsor user");
    }
    sqlx::query("INSERT INTO organizations (id, enc_name, created_by) VALUES ('notice-org',decode('00','hex'),'notice-owner')")
        .execute(pool).await.expect("seed sponsor organisation");
    for (user, role) in [
        ("notice-owner", "owner"),
        ("notice-admin", "admin"),
        ("notice-member", "member"),
    ] {
        sqlx::query("INSERT INTO organization_memberships (org_id,user_id,role) VALUES ('notice-org',$1,$2)")
            .bind(user).bind(role).execute(pool).await.expect("seed sponsor role");
    }
    sqlx::query("INSERT INTO billing_sponsored_subscriptions (organization_id,provider_customer_id,provider_subscription_id,status) VALUES ('notice-org','cus-notice-org','sub-notice-org','active')")
        .execute(pool).await.expect("seed active sponsored subscription");
    sqlx::query(
        "INSERT INTO billing_sponsored_operations \
         (operation_id,organization_id,actor_user_id,idempotency_key,request_hash,action,beneficiary_id,offer,quote_version,quote_expires_at_epoch,effective_from,provider_idempotency_key,provider_subscription_id,state,result_code) \
         VALUES ('notice-seat-op','notice-org','notice-owner','notice-seat-key','hash','add','notice-beneficiary','founding_monthly',1,1900000000,1800000000,'notice-seat-provider-key','sub-notice-org','active','seat_active')",
    )
    .execute(pool).await.expect("seed paid seat operation");
    sqlx::query("INSERT INTO billing_sponsored_seats (seat_id,organization_id,beneficiary_id,offer,effective_from,state,operation_id) VALUES ('notice-seat','notice-org','notice-beneficiary','founding_monthly',1800000000,'active','notice-seat-op')")
        .execute(pool).await.expect("seed active seat");
    sqlx::query(
        "INSERT INTO billing_founding_awards \
         (award_id,beneficiary_id,payer_kind,payer_id,offer,cohort_ordinal,original_start_date,original_end_date,original_start_epoch,original_end_epoch) \
         VALUES ('award-sponsor-notice','notice-beneficiary','sponsor','notice-org','founding_monthly',3,'2026-01-01','2027-01-01',$1,$2)",
    )
    .bind(boundary - 366 * 24 * 60 * 60).bind(boundary)
    .execute(pool).await.expect("seed sponsor founding award");
    sqlx::query(
        "INSERT INTO billing_founding_schedule_jobs \
         (job_id,award_id,beneficiary_id,payer_kind,payer_id,provider_subscription_id,provider_idempotency_key,state,provider_schedule_id,price_notice_eligible) \
         VALUES ('schedule-sponsor-notice','award-sponsor-notice','notice-beneficiary','sponsor','notice-org','sub-notice-org','schedule-sponsor-key','scheduled','sched-sponsor',TRUE)",
    )
    .execute(pool).await.expect("seed verified sponsor schedule");

    assert!(founding_price_notices::run_once(pool, now)
        .await
        .expect("reconcile sponsor schedule"));
    let recipients: Vec<String> = sqlx::query_scalar("SELECT recipient_user_id FROM cloud_notice_outbox WHERE kind='price_change' ORDER BY recipient_user_id")
        .fetch_all(pool).await.expect("load sponsor notice recipients");
    assert_eq!(recipients, vec!["notice-admin", "notice-owner"]);
    assert!(notifications::list_for_user(pool, "notice-member")
        .await
        .unwrap()
        .is_empty());
    sqlx::query("INSERT INTO users (id, oauth_provider, oauth_subject) VALUES ('notice-next-owner','price-notice-test','notice-next-owner')")
        .execute(pool).await.expect("seed next payer owner");
    sqlx::query("INSERT INTO organizations (id, enc_name, created_by) VALUES ('notice-org-next',decode('00','hex'),'notice-next-owner')")
        .execute(pool).await.expect("seed next payer organisation");
    sqlx::query("INSERT INTO organization_memberships (org_id,user_id,role) VALUES ('notice-org-next','notice-next-owner','owner')")
        .execute(pool).await.expect("seed next payer role");
    sqlx::query("INSERT INTO billing_sponsored_subscriptions (organization_id,provider_customer_id,provider_subscription_id,status) VALUES ('notice-org-next','cus-notice-org-next','sub-notice-org-next','active')")
        .execute(pool).await.expect("seed next sponsored subscription");
    sqlx::query(
        "INSERT INTO billing_sponsored_operations \
         (operation_id,organization_id,actor_user_id,idempotency_key,request_hash,action,beneficiary_id,offer,quote_version,quote_expires_at_epoch,effective_from,provider_idempotency_key,provider_subscription_id,state,result_code) \
         VALUES ('notice-seat-op-next','notice-org-next','notice-next-owner','notice-seat-key-next','hash','add','notice-beneficiary','founding_monthly',1,1900000000,1800000000,'notice-seat-provider-key-next','sub-notice-org-next','active','seat_active')",
    )
    .execute(pool).await.expect("seed next paid seat operation");
    sqlx::query("INSERT INTO billing_sponsored_seats (seat_id,organization_id,beneficiary_id,offer,effective_from,state,operation_id) VALUES ('notice-seat-next','notice-org-next','notice-beneficiary','founding_monthly',1800000000,'active','notice-seat-op-next')")
        .execute(pool).await.expect("seed next active seat");
    sqlx::query("UPDATE billing_founding_awards SET payer_id='notice-org-next' WHERE award_id='award-sponsor-notice'")
        .execute(pool).await.expect("complete sponsor payer transfer");
    sqlx::query("UPDATE billing_founding_schedule_jobs SET payer_id='notice-org-next', provider_subscription_id='sub-notice-org-next', provider_schedule_id=NULL, state='pending', version=version+1 WHERE award_id='award-sponsor-notice'")
        .execute(pool).await.expect("retarget pending schedule");
    assert!(founding_price_notices::run_once(pool, now)
        .await
        .expect("retire old payer notice while schedule is pending"));
    assert!(notifications::list_for_user(pool, "notice-owner")
        .await
        .unwrap()
        .is_empty());
    let former_status: String = sqlx::query_scalar("SELECT status FROM cloud_notice_outbox WHERE recipient_user_id='notice-owner' AND kind='price_change'")
        .fetch_one(pool).await.expect("former payer notice remains as cancelled audit");
    assert_eq!(former_status, "cancelled");
    let schedule_job = founding_schedule::claim_due(pool)
        .await
        .expect("claim payer transfer schedule")
        .expect("payer transfer schedule is due");
    assert!(
        founding_schedule::mark_scheduled(pool, &schedule_job, "sched-notice-next")
            .await
            .expect("persist verified payer transfer schedule")
    );
    assert!(founding_price_notices::run_once(pool, now)
        .await
        .expect("create notice for current payer"));
    let next_status: String = sqlx::query_scalar("SELECT status FROM cloud_notice_outbox WHERE recipient_user_id='notice-next-owner' AND kind='price_change'")
        .fetch_one(pool).await.expect("current payer notice queued");
    assert_eq!(next_status, "pending");
    sqlx::query(
        "UPDATE cloud_notice_outbox SET due_at=now()-interval '1 second' WHERE recipient_user_id='notice-next-owner' AND kind='price_change'",
    )
    .execute(pool)
    .await
    .expect("make fixture notices due");
    assert_eq!(
        notifications::list_for_user(pool, "notice-next-owner")
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(notifications::list_for_user(pool, "notice-owner")
        .await
        .unwrap()
        .is_empty());
    assert!(notifications::list_for_user(pool, "notice-admin")
        .await
        .unwrap()
        .is_empty());

    sqlx::query(
        "UPDATE billing_sponsored_seats SET effective_until=$1 WHERE seat_id='notice-seat-next'",
    )
    .bind(boundary)
    .execute(pool)
    .await
    .expect("end sponsored seat before transition");
    assert!(
        notifications::list_for_user(pool, "notice-next-owner")
            .await
            .unwrap()
            .is_empty(),
        "a seat ending at the founding boundary cannot retain a visible price-change notice"
    );
    sqlx::query("UPDATE billing_founding_price_notice_state SET next_reconcile_at=now() WHERE award_id='award-sponsor-notice'")
        .execute(pool).await.expect("make removal reconciliation due");
    assert!(founding_price_notices::run_once(pool, now)
        .await
        .expect("retire after seat removal"));
    assert!(notifications::list_for_user(pool, "notice-next-owner")
        .await
        .unwrap()
        .is_empty());
    let outcome: String = sqlx::query_scalar("SELECT outcome FROM billing_founding_price_notice_state WHERE award_id='award-sponsor-notice'")
        .fetch_one(pool).await.expect("load retired notice state");
    assert_eq!(outcome, "retired");

    database.cleanup().await;
}
