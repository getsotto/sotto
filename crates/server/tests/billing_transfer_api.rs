//! Opt-in Postgres/router coverage for the ships-dark payer-transfer API.

use std::str::FromStr;

use async_trait::async_trait;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use sqlx::{postgres::PgConnectOptions, PgPool};
use tower::ServiceExt;
use uuid::Uuid;

use sotto_server::auth::session;
use sotto_server::config::DEFAULT_ORGANISATION_DELETION_RETENTION_DAYS;
use sotto_server::db;
use sotto_server::notifications::{self, DeliveryOutcome, NoticeDelivery, NoticeSender};
use sotto_server::state::AppState;

struct AcceptingNoticeSender {
    calls: usize,
}

#[async_trait]
impl NoticeSender for AcceptingNoticeSender {
    async fn send(&mut self, _delivery: NoticeDelivery) -> DeliveryOutcome {
        self.calls += 1;
        DeliveryOutcome::Accepted {
            provider_message_id: "resend-transfer-invitation-test".into(),
        }
    }
}

async fn pool_or_skip() -> Option<PgPool> {
    if std::env::var("SOTTO_RUN_DB_TESTS").as_deref() != Ok("1") {
        eprintln!("skipping payer transfer API tests: SOTTO_RUN_DB_TESTS=1 not set");
        return None;
    }
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL for opted-in database tests");
    let options = PgConnectOptions::from_str(&url).expect("parse DATABASE_URL");
    assert!(
        matches!(options.get_host(), "localhost" | "127.0.0.1" | "::1"),
        "refusing payer transfer API tests against non-local host: {}",
        options.get_host()
    );
    let pool = db::connect(&url).await.expect("connect to test database");
    db::migrate(&pool).await.expect("apply migrations");
    Some(pool)
}

fn app(pool: PgPool, transfers_enabled: bool) -> Router {
    sotto_server::app(AppState {
        pool,
        deployment_mode: sotto_server::config::DeploymentMode::SelfHosted,
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
        billing_transfer_operator_token: None,
        cloud_action_enforcement_enabled: false,
        cloud_coverage_authority_enforcement_enabled: false,
        cloud_coverage_authority_shadow_enabled: false,
        machine_eligibility_enforcement_enabled: false,
        billing_transfers_enabled: transfers_enabled,
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

async fn cleanup(pool: &PgPool, fixture: &str) {
    let pattern = format!("transfer-api-test-{fixture}-%");
    sqlx::query("DELETE FROM billing_transfer_intents WHERE actor_user_id LIKE $1 OR beneficiary_id LIKE $1 OR counterparty_user_id LIKE $1")
        .bind(&pattern)
        .execute(pool)
        .await
        .expect("clean transfer intents");
    sqlx::query("DELETE FROM billing_personal_accounts WHERE user_id LIKE $1")
        .bind(&pattern)
        .execute(pool)
        .await
        .expect("clean personal accounts");
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
    sqlx::query("DELETE FROM billing_operations WHERE actor_user_id LIKE $1")
        .bind(&pattern)
        .execute(pool)
        .await
        .expect("clean billing operations");
    sqlx::query("DELETE FROM organization_memberships WHERE org_id LIKE $1 OR user_id LIKE $1")
        .bind(&pattern)
        .execute(pool)
        .await
        .expect("clean memberships");
    sqlx::query("DELETE FROM organizations WHERE id LIKE $1")
        .bind(&pattern)
        .execute(pool)
        .await
        .expect("clean organisations");
    sqlx::query("DELETE FROM sessions WHERE user_id LIKE $1")
        .bind(&pattern)
        .execute(pool)
        .await
        .expect("clean sessions");
    sqlx::query("DELETE FROM users WHERE id LIKE $1")
        .bind(&pattern)
        .execute(pool)
        .await
        .expect("clean users");
}

async fn insert_user(pool: &PgPool, user_id: &str) {
    sqlx::query("INSERT INTO users (id, oauth_provider, oauth_subject) VALUES ($1, 'transfer-api-test', $1)")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("insert transfer test user");
}

async fn request(
    app: &Router,
    method: &str,
    uri: &str,
    token: &str,
    body: Option<String>,
) -> (StatusCode, String) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"));
    let request = if let Some(body) = body {
        builder = builder.header("content-type", "application/json");
        builder.body(Body::from(body)).expect("build JSON request")
    } else {
        builder.body(Body::empty()).expect("build request")
    };
    let response = app.clone().oneshot(request).await.expect("router response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read response body");
    (
        status,
        String::from_utf8(bytes.to_vec()).expect("UTF-8 response"),
    )
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_secs() as i64
}

#[tokio::test]
async fn authenticated_transfer_quote_consent_status_and_replay_are_server_derived() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let fixture = Uuid::new_v4().to_string();
    let beneficiary = format!("transfer-api-test-{fixture}-beneficiary");
    let owner = format!("transfer-api-test-{fixture}-owner");
    let stranger = format!("transfer-api-test-{fixture}-stranger");
    let inactive_beneficiary = format!("transfer-api-test-{fixture}-inactive-beneficiary");
    let org_id = format!("transfer-api-test-{fixture}-destination");
    let operation = format!("transfer-api-test-{fixture}-operation");
    cleanup(&pool, &fixture).await;
    insert_user(&pool, &beneficiary).await;
    insert_user(&pool, &owner).await;
    insert_user(&pool, &stranger).await;
    insert_user(&pool, &inactive_beneficiary).await;
    sqlx::query("INSERT INTO organizations (id, enc_name, created_by) VALUES ($1, decode('6f7267', 'hex'), $2)")
        .bind(&org_id)
        .bind(&owner)
        .execute(&pool)
        .await
        .expect("insert destination organisation");
    sqlx::query(
        "INSERT INTO organization_memberships (org_id, user_id, role) VALUES ($1, $2, 'owner')",
    )
    .bind(&org_id)
    .bind(&owner)
    .execute(&pool)
    .await
    .expect("insert destination owner");

    let paid_through = now_epoch() + 86_400;
    sqlx::query(
        "INSERT INTO billing_sponsored_subscriptions \
         (organization_id, provider_customer_id, provider_subscription_id, status, current_period_end) \
         VALUES ($1, $2, $3, 'active', $4)",
    )
    .bind(&org_id)
    .bind(format!("cus_transfer_api_{fixture}_destination"))
    .bind(format!("sub_transfer_api_{fixture}_destination"))
    .bind(paid_through)
    .execute(&pool)
    .await
    .expect("insert destination sponsor subscription at the source boundary");
    sqlx::query(
        "INSERT INTO billing_operations \
         (operation_id, idempotency_key, request_hash, actor_user_id, payer_id, beneficiary_id, \
          offer, quote_version, quote_expires_at_epoch, provider_idempotency_key) \
         VALUES ($1, $1, $1, $2, $2, $2, 'standard_monthly', 1, $3, $1)",
    )
    .bind(&operation)
    .bind(&beneficiary)
    .bind(paid_through)
    .execute(&pool)
    .await
    .expect("insert source billing operation");
    sqlx::query(
        "INSERT INTO billing_personal_accounts \
         (user_id, operation_id, offer, stripe_subscription_id, state, pending_expires_at_epoch, \
          paid_through_epoch, paid_through_date) \
         VALUES ($1, $2, 'standard_monthly', $3, 'active', $4, $4, to_timestamp($4)::date)",
    )
    .bind(&beneficiary)
    .bind(&operation)
    .bind(format!("sub_transfer_api_{fixture}"))
    .bind(paid_through)
    .execute(&pool)
    .await
    .expect("insert active personal source");
    let now = now_epoch();
    notifications::record_verified_email(
        &pool,
        &format!("transfer-api-contact-{fixture}"),
        &owner,
        &format!("transfer-api-owner-{fixture}@example.test"),
        now,
    )
    .await
    .expect("record verified invited payer contact");

    let beneficiary_token = session::issue(&pool, &beneficiary)
        .await
        .expect("issue beneficiary session");
    let owner_token = session::issue(&pool, &owner)
        .await
        .expect("issue owner session");
    let stranger_token = session::issue(&pool, &stranger)
        .await
        .expect("issue stranger session");
    let app = app(pool.clone(), true);
    let quote_for_paid_beneficiary = serde_json::json!({
        "beneficiary_id": beneficiary,
        "counterparty_user_id": owner,
        "destination_kind": "sponsor",
        "destination_organization_id": org_id,
    });
    let (paid_status, paid_body) = request(
        &app,
        "POST",
        "/billing/transfers/quote",
        &stranger_token,
        Some(quote_for_paid_beneficiary.to_string()),
    )
    .await;
    let quote_for_inactive_beneficiary = serde_json::json!({
        "beneficiary_id": inactive_beneficiary,
        "counterparty_user_id": owner,
        "destination_kind": "sponsor",
        "destination_organization_id": org_id,
    });
    let (inactive_status, inactive_body) = request(
        &app,
        "POST",
        "/billing/transfers/quote",
        &stranger_token,
        Some(quote_for_inactive_beneficiary.to_string()),
    )
    .await;
    assert_eq!(paid_status, StatusCode::FORBIDDEN, "{paid_body}");
    assert_eq!(inactive_status, StatusCode::FORBIDDEN, "{inactive_body}");
    assert_eq!(
        paid_body, inactive_body,
        "quote failures must not disclose source billing state"
    );

    let draft = serde_json::json!({
        "beneficiary_id": beneficiary,
        "counterparty_user_id": owner,
        "destination_kind": "sponsor",
        "destination_organization_id": org_id,
    });
    let (status, quote_body) = request(
        &app,
        "POST",
        "/billing/transfers/quote",
        &beneficiary_token,
        Some(draft.to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{quote_body}");
    let quote: serde_json::Value = serde_json::from_str(&quote_body).expect("quote JSON");
    assert_eq!(quote["source_kind"], "personal");
    assert_eq!(quote["destination_kind"], "sponsor");
    assert_eq!(quote["offer"], "standard_monthly");
    assert_eq!(quote["amount_pence"], 299);
    assert_eq!(quote["effective_from"], paid_through);
    assert_eq!(quote["credit_pence"], 0);

    let begin = serde_json::json!({
        "beneficiary_id": beneficiary,
        "counterparty_user_id": owner,
        "destination_kind": "sponsor",
        "destination_organization_id": org_id,
        "quote_id": quote["quote_id"],
        "quote_version": quote["quote_version"],
        "quote_expires_at_epoch": quote["quote_expires_at_epoch"],
        "idempotency_key": "transfer-api-replay-1",
    });
    sqlx::query(
        "UPDATE billing_personal_accounts SET paid_through_epoch = $2, \
         paid_through_date = to_timestamp($2)::date WHERE user_id = $1",
    )
    .bind(&beneficiary)
    .bind(paid_through + 86_400)
    .execute(&pool)
    .await
    .expect("move source boundary to simulate quote drift");
    let (status, stale_body) = request(
        &app,
        "POST",
        "/billing/transfers",
        &beneficiary_token,
        Some(begin.to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{stale_body}");
    let intent_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM billing_transfer_intents WHERE beneficiary_id = $1",
    )
    .bind(&beneficiary)
    .fetch_one(&pool)
    .await
    .expect("count stale quote intents");
    assert_eq!(intent_count, 0, "stale quote must not be persisted");
    sqlx::query(
        "UPDATE billing_personal_accounts SET paid_through_epoch = $2, \
         paid_through_date = to_timestamp($2)::date WHERE user_id = $1",
    )
    .bind(&beneficiary)
    .bind(paid_through)
    .execute(&pool)
    .await
    .expect("restore source boundary");

    let (status, created_body) = request(
        &app,
        "POST",
        "/billing/transfers",
        &beneficiary_token,
        Some(begin.to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created_body}");
    let created: serde_json::Value = serde_json::from_str(&created_body).expect("created JSON");
    assert_eq!(created["state"], "awaiting_consent");
    assert_eq!(created["participant_role"], "initiator");
    let transfer_id = created["transfer_id"].as_str().expect("transfer id");
    let consent_expires_at = created["consent_expires_at_epoch"]
        .as_i64()
        .expect("consent deadline");
    let effective_from = created["effective_from"]
        .as_i64()
        .expect("renewal boundary");
    assert!(consent_expires_at <= effective_from);
    assert!(
        consent_expires_at >= now_epoch() + 7 * 24 * 60 * 60 - 5
            || consent_expires_at == effective_from,
        "consent lasts up to seven days, but never past the renewal boundary"
    );
    assert!(created.get("provider_idempotency_key").is_none());
    assert!(created.get("destination_payment_reference").is_none());
    let notice_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM cloud_notice_outbox \
         WHERE recipient_user_id = $1 AND event_key = $2 \
           AND kind = 'payer_transfer_invitation' AND channel = 'in_app'",
    )
    .bind(&owner)
    .bind(transfer_id)
    .fetch_one(&pool)
    .await
    .expect("count invitation notices");
    assert_eq!(notice_count, 1);
    let mut sender = AcceptingNoticeSender { calls: 0 };
    assert!(
        notifications::run_once(&pool, "worker-live-transfer-invitation", &mut sender)
            .await
            .expect("deliver current transfer invitation")
    );
    assert_eq!(sender.calls, 1, "a valid invitation reaches the sender");
    let delivered_invitation: String = sqlx::query_scalar(
        "SELECT status FROM cloud_notice_outbox WHERE recipient_user_id=$1 AND event_key=$2 \
         AND kind='payer_transfer_invitation' AND channel='email'",
    )
    .bind(&owner)
    .bind(transfer_id)
    .fetch_one(&pool)
    .await
    .expect("load delivered invitation status");
    assert_eq!(delivered_invitation, "accepted");

    let (status, replay_body) = request(
        &app,
        "POST",
        "/billing/transfers",
        &beneficiary_token,
        Some(begin.to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{replay_body}");
    let replay: serde_json::Value = serde_json::from_str(&replay_body).expect("replay JSON");
    assert_eq!(replay["transfer_id"], transfer_id);
    assert_eq!(replay["state"], "awaiting_consent");
    let replay_notice_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM cloud_notice_outbox \
         WHERE recipient_user_id = $1 AND event_key = $2 \
           AND kind = 'payer_transfer_invitation' AND channel = 'in_app'",
    )
    .bind(&owner)
    .bind(transfer_id)
    .fetch_one(&pool)
    .await
    .expect("count invitation notices after replay");
    assert_eq!(
        replay_notice_count, 1,
        "replay must not duplicate the notice"
    );

    let (status, pending_body) = request(
        &app,
        "POST",
        "/billing/transfers/quote",
        &beneficiary_token,
        Some(draft.to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{pending_body}");
    assert!(
        pending_body.contains("the person who started it or the invited payer can withdraw it"),
        "blocked billing changes should identify who can clear an invitation"
    );

    let (status, stranger_body) = request(
        &app,
        "GET",
        &format!("/billing/transfers/{transfer_id}"),
        &stranger_token,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{stranger_body}");

    let (status, stranger_accept_body) = request(
        &app,
        "POST",
        &format!("/billing/transfers/{transfer_id}/accept"),
        &stranger_token,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{stranger_accept_body}");

    sqlx::query(
        "UPDATE billing_transfer_intents SET quote_expires_at_epoch = $2 \
         WHERE transfer_id = $1",
    )
    .bind(transfer_id)
    .bind(now_epoch() - 1)
    .execute(&pool)
    .await
    .expect("expire the quote while leaving consent active");

    for action in ["accept", "withdraw"] {
        let missing_id = format!("transfer:missing-{fixture}");
        let (status, body) = request(
            &app,
            "POST",
            &format!("/billing/transfers/{missing_id}/{action}"),
            &beneficiary_token,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    }

    let (status, accepted_body) = request(
        &app,
        "POST",
        &format!("/billing/transfers/{transfer_id}/accept"),
        &owner_token,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{accepted_body}");
    let accepted: serde_json::Value = serde_json::from_str(&accepted_body).expect("accepted JSON");
    assert_eq!(accepted["state"], "pending");
    assert_eq!(accepted["participant_role"], "counterparty");
    assert_eq!(
        accepted["consent_expires_at_epoch"], created["consent_expires_at_epoch"],
        "acceptance must preserve the original consent deadline"
    );

    let (status, listing) = request(
        &app,
        "GET",
        "/billing/transfers?limit=1",
        &owner_token,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{listing}");
    let listing: serde_json::Value = serde_json::from_str(&listing).expect("list JSON");
    assert_eq!(listing["transfers"].as_array().unwrap().len(), 1);
    assert_eq!(listing["transfers"][0]["transfer_id"], transfer_id);
    assert!(listing["transfers"][0].get("request_hash").is_none());

    let provider_operations: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM billing_sponsored_operations WHERE beneficiary_id = $1",
    )
    .bind(&beneficiary)
    .fetch_one(&pool)
    .await
    .expect("count provider operations");
    assert_eq!(
        provider_operations, 0,
        "consent must not start provider work"
    );

    cleanup(&pool, &fixture).await;
}

#[tokio::test]
async fn sponsored_source_quotes_cover_personal_and_sponsor_destinations() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let fixture = Uuid::new_v4().to_string();
    let beneficiary = format!("transfer-api-test-{fixture}-beneficiary");
    let source_admin = format!("transfer-api-test-{fixture}-source-admin");
    let destination_admin = format!("transfer-api-test-{fixture}-destination-admin");
    let source_org = format!("transfer-api-test-{fixture}-source-org");
    let destination_org = format!("transfer-api-test-{fixture}-destination-org");
    let operation = format!("transfer-api-test-{fixture}-sponsored-operation");
    cleanup(&pool, &fixture).await;
    insert_user(&pool, &beneficiary).await;
    insert_user(&pool, &source_admin).await;
    insert_user(&pool, &destination_admin).await;
    for (org_id, owner) in [
        (&source_org, &source_admin),
        (&destination_org, &destination_admin),
    ] {
        sqlx::query(
            "INSERT INTO organizations (id, enc_name, created_by) \
             VALUES ($1, decode('6f7267', 'hex'), $2)",
        )
        .bind(org_id)
        .bind(owner)
        .execute(&pool)
        .await
        .expect("insert organisation");
        sqlx::query(
            "INSERT INTO organization_memberships (org_id, user_id, role) \
             VALUES ($1, $2, 'owner')",
        )
        .bind(org_id)
        .bind(owner)
        .execute(&pool)
        .await
        .expect("insert organisation owner");
    }
    sqlx::query(
        "INSERT INTO organization_memberships (org_id, user_id, role) \
         VALUES ($1, $2, 'member')",
    )
    .bind(&source_org)
    .bind(&beneficiary)
    .execute(&pool)
    .await
    .expect("insert source beneficiary membership");

    let period_start = now_epoch() - 86_400;
    let period_end = now_epoch() + 86_400;
    sqlx::query(
        "INSERT INTO billing_sponsored_subscriptions \
         (organization_id, provider_customer_id, provider_subscription_id, status, \
          current_period_start, current_period_end) \
         VALUES ($1, $2, $3, 'active', $4, $5)",
    )
    .bind(&destination_org)
    .bind(format!("cus_transfer_api_{fixture}_destination"))
    .bind(format!("sub_transfer_api_{fixture}_destination"))
    .bind(period_start)
    .bind(period_end)
    .execute(&pool)
    .await
    .expect("insert destination sponsor subscription at the source boundary");
    sqlx::query(
        "INSERT INTO billing_sponsored_subscriptions \
         (organization_id, provider_customer_id, provider_subscription_id, status, \
          current_period_start, current_period_end) \
         VALUES ($1, $2, $3, 'active', $4, $5)",
    )
    .bind(&source_org)
    .bind(format!("cus_transfer_api_{fixture}"))
    .bind(format!("sub_transfer_api_{fixture}"))
    .bind(period_start)
    .bind(period_end)
    .execute(&pool)
    .await
    .expect("insert source subscription");
    sqlx::query(
        "INSERT INTO billing_sponsored_operations \
         (operation_id, organization_id, actor_user_id, idempotency_key, request_hash, action, \
          beneficiary_id, offer, quote_version, quote_expires_at_epoch, effective_from, \
          provider_idempotency_key, state, result_code) \
         VALUES ($1, $2, $3, $1, $1, 'add', $4, 'standard_monthly', 1, $5, $6, \
                 $1, 'active', 'active')",
    )
    .bind(&operation)
    .bind(&source_org)
    .bind(&source_admin)
    .bind(&beneficiary)
    .bind(period_end)
    .bind(period_start)
    .execute(&pool)
    .await
    .expect("insert source operation");
    sqlx::query(
        "INSERT INTO billing_sponsored_seats \
         (seat_id, organization_id, beneficiary_id, offer, effective_from, state, operation_id) \
         VALUES ($1, $2, $3, 'standard_monthly', $4, 'active', $5)",
    )
    .bind(format!("transfer-api-test-{fixture}-seat"))
    .bind(&source_org)
    .bind(&beneficiary)
    .bind(period_start)
    .bind(&operation)
    .execute(&pool)
    .await
    .expect("insert source seat");

    let source_admin_token = session::issue(&pool, &source_admin)
        .await
        .expect("issue source admin session");
    let app = app(pool.clone(), true);
    for (counterparty, destination_kind, destination) in [
        (&beneficiary, "personal", None),
        (
            &destination_admin,
            "sponsor",
            Some(destination_org.as_str()),
        ),
    ] {
        let body = serde_json::json!({
            "beneficiary_id": beneficiary,
            "counterparty_user_id": counterparty,
            "destination_kind": destination_kind,
            "destination_organization_id": destination,
        });
        let (status, response_body) = request(
            &app,
            "POST",
            "/billing/transfers/quote",
            &source_admin_token,
            Some(body.to_string()),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{response_body}");
        let quote: serde_json::Value = serde_json::from_str(&response_body).expect("quote JSON");
        assert_eq!(quote["source_kind"], "sponsor");
        assert_eq!(quote["source_organization_id"], source_org);
        assert_eq!(quote["destination_kind"], destination_kind);
        assert_eq!(quote["effective_from"], period_end);
        assert_eq!(quote["offer"], "standard_monthly");
    }

    let transfer_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM billing_transfer_intents WHERE beneficiary_id = $1",
    )
    .bind(&beneficiary)
    .fetch_one(&pool)
    .await
    .expect("count transfer intents");
    assert_eq!(transfer_count, 0, "quoting must not create an invitation");
    cleanup(&pool, &fixture).await;
}

#[tokio::test]
async fn same_person_transfer_requires_authority_for_both_payers() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let fixture = Uuid::new_v4().to_string();
    let beneficiary = format!("transfer-api-test-{fixture}-beneficiary");
    let other_owner = format!("transfer-api-test-{fixture}-other-owner");
    let owned_org = format!("transfer-api-test-{fixture}-owned-org");
    let foreign_org = format!("transfer-api-test-{fixture}-foreign-org");
    let operation = format!("transfer-api-test-{fixture}-operation");
    cleanup(&pool, &fixture).await;
    insert_user(&pool, &beneficiary).await;
    insert_user(&pool, &other_owner).await;

    for (org_id, owner) in [(&owned_org, &beneficiary), (&foreign_org, &other_owner)] {
        sqlx::query(
            "INSERT INTO organizations (id, enc_name, created_by) \
             VALUES ($1, decode('6f7267', 'hex'), $2)",
        )
        .bind(org_id)
        .bind(owner)
        .execute(&pool)
        .await
        .expect("insert organisation");
        sqlx::query(
            "INSERT INTO organization_memberships (org_id, user_id, role) \
             VALUES ($1, $2, 'owner')",
        )
        .bind(org_id)
        .bind(owner)
        .execute(&pool)
        .await
        .expect("insert organisation owner");
    }

    let paid_through = now_epoch() + 86_400;
    sqlx::query(
        "INSERT INTO billing_sponsored_subscriptions \
         (organization_id, provider_customer_id, provider_subscription_id, status, current_period_end) \
         VALUES ($1, $2, $3, 'active', $4)",
    )
    .bind(&owned_org)
    .bind(format!("cus_transfer_api_{fixture}_owned_destination"))
    .bind(format!("sub_transfer_api_{fixture}_owned_destination"))
    .bind(paid_through)
    .execute(&pool)
    .await
    .expect("insert active owned destination sponsor subscription");

    sqlx::query(
        "INSERT INTO billing_operations \
         (operation_id, idempotency_key, request_hash, actor_user_id, payer_id, beneficiary_id, \
          offer, quote_version, quote_expires_at_epoch, provider_idempotency_key) \
         VALUES ($1, $1, $1, $2, $2, $2, 'standard_monthly', 1, $3, $1)",
    )
    .bind(&operation)
    .bind(&beneficiary)
    .bind(paid_through)
    .execute(&pool)
    .await
    .expect("insert source billing operation");
    sqlx::query(
        "INSERT INTO billing_personal_accounts \
         (user_id, operation_id, offer, stripe_subscription_id, state, pending_expires_at_epoch, \
          paid_through_epoch, paid_through_date) \
         VALUES ($1, $2, 'standard_monthly', $3, 'active', $4, $4, to_timestamp($4)::date)",
    )
    .bind(&beneficiary)
    .bind(&operation)
    .bind(format!("sub_transfer_api_{fixture}"))
    .bind(paid_through)
    .execute(&pool)
    .await
    .expect("insert active personal source");

    let token = session::issue(&pool, &beneficiary)
        .await
        .expect("issue beneficiary session");
    let app = app(pool.clone(), true);
    let quote_body = |destination_org: &str| {
        serde_json::json!({
            "beneficiary_id": beneficiary,
            "counterparty_user_id": beneficiary,
            "destination_kind": "sponsor",
            "destination_organization_id": destination_org,
        })
    };

    let (status, body) = request(
        &app,
        "POST",
        "/billing/transfers/quote",
        &token,
        Some(quote_body(&foreign_org).to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    let (status, body) = request(
        &app,
        "POST",
        "/billing/transfers/quote",
        &token,
        Some(quote_body(&owned_org).to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let quote: serde_json::Value = serde_json::from_str(&body).expect("quote JSON");
    let begin = serde_json::json!({
        "beneficiary_id": beneficiary,
        "counterparty_user_id": beneficiary,
        "destination_kind": "sponsor",
        "destination_organization_id": owned_org,
        "quote_id": quote["quote_id"],
        "quote_version": quote["quote_version"],
        "quote_expires_at_epoch": quote["quote_expires_at_epoch"],
        "idempotency_key": format!("transfer-api-test-{fixture}-self-transfer"),
    });
    let (status, body) = request(
        &app,
        "POST",
        "/billing/transfers",
        &token,
        Some(begin.to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let created: serde_json::Value = serde_json::from_str(&body).expect("transfer JSON");
    assert_eq!(created["state"], "pending");

    cleanup(&pool, &fixture).await;
}
