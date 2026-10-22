//! Sponsored founding awards are issued only after the paid renewal invoice is verified.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use sqlx::{postgres::PgConnectOptions, PgPool};
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::{sleep, Instant};
use tower::ServiceExt;
use uuid::Uuid;

use sotto_server::billing::{BillingState, STRIPE_API_VERSION};
use sotto_server::billing_catalogue::BillingPriceIds;
use sotto_server::config::{BillingConfig, DEFAULT_ORGANISATION_DELETION_RETENTION_DAYS};
use sotto_server::db;
use sotto_server::founding_allocator::{
    reserve_with_payer_kind_until, FoundingOffer, FoundingPayerKind, ReservationOutcome,
};
use sotto_server::sponsored_billing;
use sotto_server::state::AppState;

const WEBHOOK_SECRET: &str = "whsec_sponsored_founding_test";

async fn pool_or_skip() -> Option<PgPool> {
    if std::env::var("SOTTO_RUN_DB_TESTS").as_deref() != Ok("1") {
        eprintln!("skipping sponsored founding test: SOTTO_RUN_DB_TESTS=1 not set");
        return None;
    }
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL for opted-in database tests");
    let options = PgConnectOptions::from_str(&url).expect("parse DATABASE_URL");
    assert!(
        matches!(options.get_host(), "localhost" | "127.0.0.1" | "::1"),
        "refusing sponsored founding tests against non-local host: {}",
        options.get_host()
    );
    let pool = db::connect(&url).await.expect("connect to test database");
    db::migrate(&pool).await.expect("apply migrations");
    Some(pool)
}

fn app(pool: PgPool) -> Router {
    let state = AppState {
        deployment_mode: sotto_server::config::DeploymentMode::SelfHosted,
        telemetry_ingest: false,
        contact_verification: None,
        cloud_notice_dispatch_enabled: false,
        resend_webhook_secret: None,
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
        pool,
        oauth: None,
        oauth_config: None,
        billing: Some(BillingState::from_config(BillingConfig {
            api_key: "rk_test_never_called".into(),
            webhook_secret: WEBHOOK_SECRET.into(),
            price_id: "price_legacy_unused".into(),
            price_catalogue: Some(BillingPriceIds {
                standard_monthly: "price_standard_monthly".into(),
                standard_annual: "price_standard_annual".into(),
                founding_monthly: "price_founding_monthly".into(),
                founding_annual: "price_founding_annual".into(),
            }),
            cloud_portal_configuration_id: Some("bpc_test".into()),
            cloud_sales_enabled: true,
            return_url: "https://app.sotto.test".into(),
        })),
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
    };
    Router::new()
        .merge(sotto_server::billing::router())
        .with_state(state)
}

fn signature(payload: &str) -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after unix epoch")
        .as_secs();
    let mut mac = Hmac::<Sha256>::new_from_slice(WEBHOOK_SECRET.as_bytes()).expect("HMAC key");
    mac.update(format!("{timestamp}.{payload}").as_bytes());
    let digest: String = mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("t={timestamp},v1={digest}")
}

async fn post_webhook(app: &Router, payload: &str) -> StatusCode {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/billing/webhook")
                .header("content-type", "application/json")
                .header("Stripe-Signature", signature(payload))
                .body(Body::from(payload.to_owned()))
                .expect("webhook request"),
        )
        .await
        .expect("webhook response");
    response.status()
}

async fn wait_for_capacity_waiter(pool: &PgPool, holder_pid: i32) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let waiter_found: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM pg_stat_activity \
             WHERE $1 = ANY(pg_blocking_pids(pid)) \
               AND query LIKE '%billing_founding_capacity%')",
        )
        .bind(holder_pid)
        .fetch_one(pool)
        .await
        .expect("find webhook waiting on founding capacity");
        if waiter_found {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "webhook did not block on the founding-capacity lock"
        );
        sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn verified_sponsored_invoice_awards_founder_before_activating_the_seat() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    std::env::set_var("SOTTO_SPONSORED_BILLING_ENABLED", "1");

    let fixture = Uuid::new_v4().simple().to_string();
    let owner_id = format!("founding-owner-{fixture}");
    let beneficiary_id = format!("founding-person-{fixture}");
    let refunded_beneficiary_id = format!("founding-refunded-person-{fixture}");
    let expired_standard_beneficiary_id = format!("founding-standard-expired-person-{fixture}");
    let organization_id = format!("founding-org-{fixture}");
    let operation_id = format!("sponsored:founding-operation-{fixture}");
    let refunded_operation_id = format!("sponsored:founding-refunded-operation-{fixture}");
    let expired_standard_operation_id =
        format!("sponsored:founding-standard-expired-operation-{fixture}");
    let reservation_id = format!("founding:{operation_id}");
    let refunded_reservation_id = format!("founding:{refunded_operation_id}");
    let subscription_id = format!("sub_founding_{fixture}");
    let customer_id = format!("cus_founding_{fixture}");
    let provider_item_id = format!("si_founding_{fixture}");
    let event_id = format!("evt_founding_{fixture}");
    let replay_event_id = format!("evt_founding_replay_{fixture}");
    let terminal_event_id = format!("evt_founding_terminal_{fixture}");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after unix epoch")
        .as_secs() as i64;
    let quote_expiry = now + 1_800;
    let late_quote_expiry = now - 90;
    let period_end = now + 30 * 24 * 60 * 60;

    sqlx::query("INSERT INTO users (id, oauth_provider, oauth_subject) VALUES ($1, 'github', $1), ($2, 'github', $2), ($3, 'github', $3), ($4, 'github', $4)")
        .bind(&owner_id)
        .bind(&beneficiary_id)
        .bind(&refunded_beneficiary_id)
        .bind(&expired_standard_beneficiary_id)
        .execute(&pool)
        .await
        .expect("seed users");
    sqlx::query("INSERT INTO organizations (id, enc_name, tier) VALUES ($1, $2, 'team')")
        .bind(&organization_id)
        .bind(b"opaque".as_slice())
        .execute(&pool)
        .await
        .expect("seed organisation");
    sqlx::query(
        "INSERT INTO organization_memberships (org_id, user_id, role) VALUES ($1, $2, 'owner')",
    )
    .bind(&organization_id)
    .bind(&owner_id)
    .execute(&pool)
    .await
    .expect("seed owner membership");
    sqlx::query(
        "INSERT INTO billing_sponsored_subscriptions \
         (organization_id, provider_customer_id, provider_subscription_id, status, current_period_end) \
         VALUES ($1, $2, $3, 'active', $4)",
    )
    .bind(&organization_id)
    .bind(&customer_id)
    .bind(&subscription_id)
    .bind(period_end)
    .execute(&pool)
    .await
    .expect("seed sponsored subscription");
    sqlx::query(
        "INSERT INTO billing_sponsored_operations \
         (operation_id, organization_id, actor_user_id, idempotency_key, request_hash, action, \
          beneficiary_id, offer, quote_version, quote_expires_at_epoch, effective_from, \
         provider_idempotency_key, provider_subscription_id, state) \
         VALUES ($1, $2, $3, $4, $5, 'add', $6, 'founding_monthly', 1, $7, $8, $9, $10, 'provider_pending')",
    )
    .bind(&operation_id)
    .bind(&organization_id)
    .bind(&owner_id)
    .bind(format!("idempotency-{fixture}"))
    .bind(format!("request-{fixture}"))
    .bind(&beneficiary_id)
    .bind(quote_expiry)
    .bind(now - 60)
    .bind(format!("provider-{fixture}"))
    .bind(&subscription_id)
    .execute(&pool)
    .await
    .expect("seed sponsored operation");
    sqlx::query(
        "INSERT INTO billing_sponsored_operations \
         (operation_id, organization_id, actor_user_id, idempotency_key, request_hash, action, \
          beneficiary_id, offer, quote_version, quote_expires_at_epoch, effective_from, \
         provider_idempotency_key, provider_subscription_id, state) \
         VALUES ($1, $2, $3, $4, $5, 'add', $6, 'founding_monthly', 1, $7, $8, $9, $10, 'provider_pending')",
    )
    .bind(&refunded_operation_id)
    .bind(&organization_id)
    .bind(&owner_id)
    .bind(format!("idempotency-refunded-{fixture}"))
    .bind(format!("request-refunded-{fixture}"))
    .bind(&refunded_beneficiary_id)
    .bind(late_quote_expiry)
    .bind(now - 30)
    .bind(format!("provider-refunded-{fixture}"))
    .bind(&subscription_id)
    .execute(&pool)
    .await
    .expect("seed late sponsored operation");
    sqlx::query(
        "INSERT INTO billing_sponsored_operations \
         (operation_id, organization_id, actor_user_id, idempotency_key, request_hash, action, \
          beneficiary_id, offer, quote_version, quote_expires_at_epoch, effective_from, \
          provider_idempotency_key, state, result_code) \
         VALUES ($1, $2, $3, $4, $5, 'add', $6, 'founding_monthly', 1, $7, $8, $9, 'failed', 'checkout_expired')",
    )
    .bind(&expired_standard_operation_id)
    .bind(&organization_id)
    .bind(&owner_id)
    .bind(format!("idempotency-standard-expired-{fixture}"))
    .bind(format!("request-standard-expired-{fixture}"))
    .bind(&expired_standard_beneficiary_id)
    .bind(quote_expiry)
    .bind(now - 20)
    .bind(format!("provider-standard-expired-{fixture}"))
    .execute(&pool)
    .await
    .expect("seed expired founding Checkout operation");
    sqlx::query(
        "INSERT INTO billing_founding_reservations \
         (reservation_id, operation_id, beneficiary_id, payer_kind, payer_id, offer, quote_version, \
          quote_expires_at_epoch, reservation_expires_at_epoch, status) \
         VALUES ($1, $2, $3, 'sponsor', $4, 'founding_monthly', 1, $5, $5, 'released')",
    )
    .bind(format!("founding:{expired_standard_operation_id}"))
    .bind(&expired_standard_operation_id)
    .bind(&expired_standard_beneficiary_id)
    .bind(&organization_id)
    .bind(quote_expiry)
    .execute(&pool)
    .await
    .expect("seed released founding reservation for expired Checkout");
    sqlx::query(
        "INSERT INTO billing_sponsored_seats \
         (seat_id, organization_id, beneficiary_id, offer, effective_from, state, operation_id) \
         VALUES ($1, $2, $3, 'founding_monthly', $4, 'pending', $5)",
    )
    .bind(format!("seat-{fixture}"))
    .bind(&organization_id)
    .bind(&beneficiary_id)
    .bind(now - 60)
    .bind(&operation_id)
    .execute(&pool)
    .await
    .expect("seed pending sponsored seat");
    sqlx::query(
        "INSERT INTO billing_sponsored_seats \
         (seat_id, organization_id, beneficiary_id, offer, effective_from, state, operation_id) \
         VALUES ($1, $2, $3, 'founding_monthly', $4, 'pending', $5)",
    )
    .bind(format!("seat-refunded-{fixture}"))
    .bind(&organization_id)
    .bind(&refunded_beneficiary_id)
    .bind(now - 30)
    .bind(&refunded_operation_id)
    .execute(&pool)
    .await
    .expect("seed late pending sponsored seat");
    let mut tx = pool.begin().await.expect("begin founding reservation");
    assert!(matches!(
        reserve_with_payer_kind_until(
            &mut tx,
            &reservation_id,
            &operation_id,
            &beneficiary_id,
            FoundingPayerKind::Sponsor,
            &organization_id,
            FoundingOffer::Monthly,
            1,
            quote_expiry,
            period_end + sotto_server::founding_allocator::SETTLEMENT_RECOVERY_SECONDS,
            now,
        )
        .await
        .expect("reserve shared founding place"),
        ReservationOutcome::Created(_)
    ));
    tx.commit().await.expect("commit founding reservation");
    let mut late_reservation_tx = pool.begin().await.expect("begin late reservation");
    assert!(matches!(
        reserve_with_payer_kind_until(
            &mut late_reservation_tx,
            &refunded_reservation_id,
            &refunded_operation_id,
            &refunded_beneficiary_id,
            FoundingPayerKind::Sponsor,
            &organization_id,
            FoundingOffer::Monthly,
            1,
            late_quote_expiry,
            now - 1,
            now - 100,
        )
        .await
        .expect("reserve second founding place"),
        ReservationOutcome::Created(_)
    ));
    late_reservation_tx
        .commit()
        .await
        .expect("commit late settlement reservation");

    let app = app(pool.clone());
    let invoice = serde_json::json!({
        "id": event_id,
        "created": now,
        "api_version": STRIPE_API_VERSION,
        "type": "invoice.paid",
        "data": { "object": {
            "id": format!("in_founding_{fixture}"),
            "customer": customer_id,
            "payment_intent": format!("pi_founding_{fixture}"),
            "status_transitions": { "paid_at": now },
            "lines": { "data": [{
                "type": "subscription",
                "subscription_item": provider_item_id,
                "price": { "id": "price_founding_monthly" },
                "quantity": 2,
                "period": { "start": now - 60, "end": period_end },
                "parent": { "type": "subscription_item_details" }
            }]},
            "parent": { "type": "subscription_details", "subscription_details": { "subscription": subscription_id } }
        }}
    })
    .to_string();

    // Hold the allocator row while a real invoice webhook reaches settlement. It must wait
    // before locking the subscription row, so checkout, expiry and renewal all share one lock
    // order (capacity first).
    let mut capacity_holder = pool.begin().await.expect("begin capacity lock holder");
    let holder_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *capacity_holder)
        .await
        .expect("read capacity lock holder pid");
    sotto_server::founding_allocator::lock_capacity(&mut capacity_holder)
        .await
        .expect("hold founding capacity");
    let first_invoice = invoice.clone();
    let app_for_invoice = app.clone();
    let invoice_task =
        tokio::spawn(async move { post_webhook(&app_for_invoice, &first_invoice).await });
    wait_for_capacity_waiter(&pool, holder_pid).await;
    let mut subscription_probe = pool.begin().await.expect("begin subscription lock probe");
    let subscription_lock = sqlx::query_scalar::<_, String>(
        "SELECT provider_subscription_id FROM billing_sponsored_subscriptions \
         WHERE provider_subscription_id = $1 FOR UPDATE NOWAIT",
    )
    .bind(&subscription_id)
    .fetch_one(&mut *subscription_probe)
    .await;
    assert_eq!(
        subscription_lock.expect("webhook must wait for capacity before locking subscription"),
        subscription_id
    );
    subscription_probe
        .rollback()
        .await
        .expect("release subscription lock probe");
    capacity_holder
        .commit()
        .await
        .expect("release capacity lock");
    assert_eq!(
        invoice_task.await.expect("invoice webhook task"),
        StatusCode::OK
    );
    assert_eq!(post_webhook(&app, &invoice).await, StatusCode::OK);
    let award: (String, String, String, String) = sqlx::query_as(
        "SELECT beneficiary_id, payer_kind, payer_id, offer FROM billing_founding_awards WHERE beneficiary_id = $1",
    )
    .bind(&beneficiary_id)
    .fetch_one(&pool)
    .await
    .expect("sponsored founder award");
    assert_eq!(
        award,
        (
            beneficiary_id.clone(),
            "sponsor".into(),
            organization_id.clone(),
            "founding_monthly".into()
        )
    );
    let award_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM billing_founding_awards WHERE beneficiary_id = $1",
    )
    .bind(&beneficiary_id)
    .fetch_one(&pool)
    .await
    .expect("count sponsored founder awards after replay");
    assert_eq!(award_count, 1);
    let settled: (String, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT operation.state, operation.provider_payment_reference, seat.provider_item_id \
         FROM billing_sponsored_operations AS operation \
         JOIN billing_sponsored_seats AS seat USING (operation_id) WHERE operation.operation_id = $1",
    )
    .bind(&operation_id)
    .fetch_one(&pool)
    .await
    .expect("settled sponsored operation");
    assert_eq!(settled.0, "active");
    assert_eq!(
        settled.1.as_deref(),
        Some(format!("pi_founding_{fixture}").as_str())
    );
    assert_eq!(settled.2.as_deref(), Some(provider_item_id.as_str()));
    let sponsorship_notice_recipients: Vec<String> = sqlx::query_scalar(
        "SELECT recipient_user_id FROM cloud_notice_outbox WHERE event_key=$1 \
         AND kind='sponsorship_change' AND channel='in_app' ORDER BY recipient_user_id",
    )
    .bind(&operation_id)
    .fetch_all(&pool)
    .await
    .expect("load sponsorship outcome notices from paid invoice handler");
    let mut expected_sponsorship_notice_recipients = vec![owner_id.clone(), beneficiary_id.clone()];
    expected_sponsorship_notice_recipients.sort();
    assert_eq!(
        sponsorship_notice_recipients,
        expected_sponsorship_notice_recipients
    );
    let refunded_state: (String, Option<String>, String) = sqlx::query_as(
        "SELECT operation.state, operation.provider_payment_reference, seat.state \
         FROM billing_sponsored_operations AS operation \
         JOIN billing_sponsored_seats AS seat USING (operation_id) WHERE operation.operation_id = $1",
    )
    .bind(&refunded_operation_id)
    .fetch_one(&pool)
    .await
    .expect("refunded sponsored operation");
    assert_eq!(refunded_state.0, "failed");
    assert_eq!(
        refunded_state.1.as_deref(),
        Some(format!("pi_founding_{fixture}").as_str())
    );
    assert_eq!(refunded_state.2, "canceled");
    let refunded_awards: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM billing_founding_awards WHERE beneficiary_id = $1",
    )
    .bind(&refunded_beneficiary_id)
    .fetch_one(&pool)
    .await
    .expect("count non-awarded late payment");
    assert_eq!(refunded_awards, 0);

    let receipt = sqlx::query_scalar::<_, String>(
        "SELECT status FROM billing_founding_reservations WHERE reservation_id = $1",
    )
    .bind(&reservation_id)
    .fetch_one(&pool)
    .await
    .expect("confirmed shared reservation");
    assert_eq!(receipt, "confirmed");

    // A distinct, later invoice must not retry a terminal refund or try to complete a standard
    // checkout that was already expired. Either row would otherwise make the webhook fail.
    let mut replay_invoice: serde_json::Value =
        serde_json::from_str(&invoice).expect("parse invoice payload");
    replay_invoice["id"] = replay_event_id.clone().into();
    replay_invoice["created"] = (now + 1).into();
    replay_invoice["data"]["object"]["id"] = format!("in_founding_replay_{fixture}").into();
    replay_invoice["data"]["object"]["payment_intent"] =
        format!("pi_founding_replay_{fixture}").into();
    replay_invoice["data"]["object"]["status_transitions"]["paid_at"] = (now + 1).into();
    replay_invoice["data"]["object"]["lines"]["data"][0]["period"]["end"] =
        (period_end + 30 * 24 * 60 * 60).into();
    assert_eq!(
        post_webhook(&app, &replay_invoice.to_string()).await,
        StatusCode::OK
    );
    let late_operation: (String, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT state, result_code, provider_payment_reference \
         FROM billing_sponsored_operations WHERE operation_id = $1",
    )
    .bind(&refunded_operation_id)
    .fetch_one(&pool)
    .await
    .expect("load late operation after later invoice");
    assert_eq!(
        late_operation,
        (
            "failed".into(),
            Some("founding_refund_required".into()),
            Some(format!("pi_founding_{fixture}")),
        )
    );
    let expired_checkout: (String, Option<String>, Option<String>, String) = sqlx::query_as(
        "SELECT operation.state, operation.result_code, operation.provider_payment_reference, reservation.status \
         FROM billing_sponsored_operations AS operation \
         JOIN billing_founding_reservations AS reservation USING (operation_id) \
         WHERE operation.operation_id = $1",
    )
    .bind(&expired_standard_operation_id)
    .fetch_one(&pool)
    .await
    .expect("load expired checkout after later invoice");
    assert_eq!(
        expired_checkout,
        (
            "failed".into(),
            Some("checkout_expired".into()),
            None,
            "released".into(),
        )
    );

    let abandoned_operation_id = format!("sponsored:founding-abandoned-checkout-{fixture}");
    let abandoned_reservation_id = format!("founding:{abandoned_operation_id}");
    let abandoned_deadline = now - sotto_server::founding_allocator::RESERVATION_SECONDS - 60;
    sqlx::query(
        "INSERT INTO billing_sponsored_operations \
         (operation_id, organization_id, actor_user_id, idempotency_key, request_hash, action, \
          beneficiary_id, offer, quote_version, quote_expires_at_epoch, effective_from, \
          provider_idempotency_key, state) \
         VALUES ($1, $2, $3, $4, $5, 'add', $6, 'founding_monthly', 1, $7, $8, $9, 'pending')",
    )
    .bind(&abandoned_operation_id)
    .bind(&organization_id)
    .bind(&owner_id)
    .bind(format!("idempotency-abandoned-{fixture}"))
    .bind(format!("request-abandoned-{fixture}"))
    .bind(&expired_standard_beneficiary_id)
    .bind(abandoned_deadline)
    .bind(abandoned_deadline)
    .bind(format!("provider-abandoned-{fixture}"))
    .execute(&pool)
    .await
    .expect("seed abandoned checkout operation");
    sqlx::query(
        "INSERT INTO billing_sponsored_seats \
         (seat_id, organization_id, beneficiary_id, offer, effective_from, state, operation_id) \
         VALUES ($1, $2, $3, 'founding_monthly', $4, 'pending', $5)",
    )
    .bind(format!("seat-abandoned-{fixture}"))
    .bind(&organization_id)
    .bind(&expired_standard_beneficiary_id)
    .bind(abandoned_deadline)
    .bind(&abandoned_operation_id)
    .execute(&pool)
    .await
    .expect("seed abandoned checkout seat");
    sqlx::query(
        "INSERT INTO billing_founding_reservations \
         (reservation_id, operation_id, beneficiary_id, payer_kind, payer_id, offer, quote_version, \
          quote_expires_at_epoch, reservation_expires_at_epoch, status) \
         VALUES ($1, $2, $3, 'sponsor', $4, 'founding_monthly', 1, $5, $5, 'reserved')",
    )
    .bind(&abandoned_reservation_id)
    .bind(&abandoned_operation_id)
    .bind(&expired_standard_beneficiary_id)
    .bind(&organization_id)
    .bind(abandoned_deadline)
    .execute(&pool)
    .await
    .expect("seed expired founding reservation");
    let mut abandoned_tx = pool.begin().await.expect("begin abandoned cleanup");
    sotto_server::founding_allocator::lock_capacity(&mut abandoned_tx)
        .await
        .expect("lock capacity before abandoning checkout");
    assert_eq!(
        sponsored_billing::cancel_expired_pending_checkouts(
            &mut abandoned_tx,
            &organization_id,
            &expired_standard_beneficiary_id,
            now,
        )
        .await
        .expect("release expired checkout attempt"),
        1
    );
    abandoned_tx
        .commit()
        .await
        .expect("commit abandoned checkout cleanup");
    let abandoned_state: (String, Option<String>, String, String) = sqlx::query_as(
        "SELECT operation.state, operation.result_code, reservation.status, seat.state \
         FROM billing_sponsored_operations AS operation \
         JOIN billing_founding_reservations AS reservation USING (operation_id) \
         JOIN billing_sponsored_seats AS seat USING (operation_id) \
         WHERE operation.operation_id = $1",
    )
    .bind(&abandoned_operation_id)
    .fetch_one(&pool)
    .await
    .expect("load expired checkout cleanup result");
    assert_eq!(
        abandoned_state,
        (
            "failed".into(),
            Some("checkout_reservation_expired".into()),
            "released".into(),
            "canceled".into(),
        )
    );

    let terminal_operation_id = format!("sponsored:founding-terminal-operation-{fixture}");
    let terminal_reservation_id = format!("founding:{terminal_operation_id}");
    sqlx::query(
        "INSERT INTO billing_sponsored_operations \
         (operation_id, organization_id, actor_user_id, idempotency_key, request_hash, action, \
          beneficiary_id, offer, quote_version, quote_expires_at_epoch, effective_from, \
          provider_idempotency_key, provider_subscription_id, state) \
         VALUES ($1, $2, $3, $4, $5, 'add', $6, 'founding_monthly', 1, $7, $8, $9, $10, 'pending')",
    )
    .bind(&terminal_operation_id)
    .bind(&organization_id)
    .bind(&owner_id)
    .bind(format!("idempotency-terminal-{fixture}"))
    .bind(format!("request-terminal-{fixture}"))
    .bind(&expired_standard_beneficiary_id)
    .bind(quote_expiry)
    .bind(now + 30)
    .bind(format!("provider-terminal-{fixture}"))
    .bind(&subscription_id)
    .execute(&pool)
    .await
    .expect("seed pending sponsored operation for terminal subscription");
    let mut terminal_reservation_tx = pool.begin().await.expect("begin terminal reservation");
    assert!(matches!(
        reserve_with_payer_kind_until(
            &mut terminal_reservation_tx,
            &terminal_reservation_id,
            &terminal_operation_id,
            &expired_standard_beneficiary_id,
            FoundingPayerKind::Sponsor,
            &organization_id,
            FoundingOffer::Monthly,
            1,
            quote_expiry,
            period_end + sotto_server::founding_allocator::SETTLEMENT_RECOVERY_SECONDS,
            now,
        )
        .await
        .expect("reserve founding place before subscription deletion"),
        ReservationOutcome::Created(_)
    ));
    terminal_reservation_tx
        .commit()
        .await
        .expect("commit terminal reservation");

    let subscription_deleted = serde_json::json!({
        "id": terminal_event_id,
        "created": now + 2,
        "api_version": STRIPE_API_VERSION,
        "type": "customer.subscription.deleted",
        "data": { "object": {
            "id": subscription_id,
            "customer": customer_id,
            "status": "canceled",
            "metadata": { "organization_id": organization_id }
        }}
    })
    .to_string();
    assert_eq!(
        post_webhook(&app, &subscription_deleted).await,
        StatusCode::OK
    );
    let terminal_result: (String, Option<String>, String, String) = sqlx::query_as(
        "SELECT operation.state, operation.result_code, reservation.status, subscription.status \
         FROM billing_sponsored_operations AS operation \
         JOIN billing_founding_reservations AS reservation ON reservation.operation_id = operation.operation_id \
         JOIN billing_sponsored_subscriptions AS subscription USING (organization_id) \
         WHERE operation.operation_id = $1",
    )
    .bind(&terminal_operation_id)
    .fetch_one(&pool)
    .await
    .expect("load operation after terminal subscription event");
    assert_eq!(
        terminal_result,
        (
            "failed".into(),
            Some("subscription_ended".into()),
            "released".into(),
            "canceled".into(),
        )
    );

    sqlx::query("DELETE FROM billing_sponsored_seats WHERE organization_id = $1")
        .bind(&organization_id)
        .execute(&pool)
        .await
        .expect("clean sponsored seat");
    sqlx::query("DELETE FROM billing_sponsored_operations WHERE organization_id = $1")
        .bind(&organization_id)
        .execute(&pool)
        .await
        .expect("clean sponsored operation");
    sqlx::query("DELETE FROM billing_sponsored_subscription_items WHERE organization_id = $1")
        .bind(&organization_id)
        .execute(&pool)
        .await
        .expect("clean subscription item");
    sqlx::query("DELETE FROM billing_sponsored_subscriptions WHERE organization_id = $1")
        .bind(&organization_id)
        .execute(&pool)
        .await
        .expect("clean sponsored subscription");
    sqlx::query("DELETE FROM stripe_subscription_watermarks WHERE subscription_id = $1")
        .bind(&subscription_id)
        .execute(&pool)
        .await
        .expect("clean webhook watermark");
    sqlx::query("DELETE FROM stripe_webhook_events WHERE event_id = $1")
        .bind(event_id)
        .execute(&pool)
        .await
        .expect("clean webhook receipt");
    sqlx::query("DELETE FROM stripe_webhook_events WHERE event_id = $1")
        .bind(replay_event_id)
        .execute(&pool)
        .await
        .expect("clean replay webhook receipt");
    sqlx::query("DELETE FROM stripe_webhook_events WHERE event_id = $1")
        .bind(terminal_event_id)
        .execute(&pool)
        .await
        .expect("clean terminal webhook receipt");
    sqlx::query("DELETE FROM billing_founding_reservations WHERE beneficiary_id = $1")
        .bind(&beneficiary_id)
        .execute(&pool)
        .await
        .expect("clean founding reservation");
    sqlx::query("DELETE FROM billing_founding_reservations WHERE beneficiary_id = $1")
        .bind(&refunded_beneficiary_id)
        .execute(&pool)
        .await
        .expect("clean released founding reservation");
    sqlx::query("DELETE FROM billing_founding_reservations WHERE beneficiary_id = $1")
        .bind(&expired_standard_beneficiary_id)
        .execute(&pool)
        .await
        .expect("clean expired and terminal founding reservations");
    sqlx::query("DELETE FROM billing_founding_reservations WHERE operation_id = $1")
        .bind(&terminal_operation_id)
        .execute(&pool)
        .await
        .expect("clean terminal founding reservation");
    sqlx::query("DELETE FROM billing_founding_schedule_jobs WHERE beneficiary_id = $1")
        .bind(&beneficiary_id)
        .execute(&pool)
        .await
        .expect("clean founding schedule job");
    sqlx::query("DELETE FROM billing_founding_awards WHERE beneficiary_id = $1")
        .bind(&beneficiary_id)
        .execute(&pool)
        .await
        .expect("clean founding award");
    sqlx::query("DELETE FROM organization_memberships WHERE org_id = $1")
        .bind(&organization_id)
        .execute(&pool)
        .await
        .expect("clean membership");
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(&organization_id)
        .execute(&pool)
        .await
        .expect("clean organisation");
    sqlx::query("DELETE FROM users WHERE id IN ($1, $2, $3, $4)")
        .bind(&owner_id)
        .bind(&beneficiary_id)
        .bind(&refunded_beneficiary_id)
        .bind(&expired_standard_beneficiary_id)
        .execute(&pool)
        .await
        .expect("clean users");
}
