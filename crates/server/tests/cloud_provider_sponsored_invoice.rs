//! End-to-end sponsored invoice intake against controlled Stripe reads and PostgreSQL.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use sqlx::postgres::PgConnectOptions;
use sqlx::PgPool;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;
use uuid::Uuid;

use sotto_server::billing::{BillingState, STRIPE_API_VERSION};
use sotto_server::billing_catalogue::{BillingOffer, BillingPriceIds};
use sotto_server::cloud_provider::{
    register_verified_allocation, AllocationState, PayerKind, ProviderContext, ProviderEnvironment,
    VerifiedAllocation,
};
use sotto_server::cloud_provider_refresh_jobs::RefreshJobLease;
use sotto_server::cloud_provider_stripe::StripeCoverageConfig;
use sotto_server::cloud_provider_stripe_http::{StripeReadClient, StripeReadLimits};
use sotto_server::cloud_provider_stripe_sponsored_store::record_sponsored_allocation_term;
use sotto_server::config::{BillingConfig, DEFAULT_ORGANISATION_DELETION_RETENTION_DAYS};
use sotto_server::db;
use sotto_server::founding_allocator::{
    reserve_with_payer_kind_until, FoundingOffer, FoundingPayerKind, ReservationOutcome,
};
use sotto_server::sponsored_billing::{
    begin_operation, record_provider_update, SponsoredSeatAction, SponsoredSeatRequest,
};
use sotto_server::state::AppState;
use tokio::net::TcpListener;
use url::Url;

const WEBHOOK_SECRET: &str = "whsec_sponsored_coverage_test";

async fn pool() -> Option<PgPool> {
    if std::env::var("SOTTO_RUN_DB_TESTS").as_deref() != Ok("1") {
        eprintln!("skipping sponsored invoice coverage test: SOTTO_RUN_DB_TESTS=1 not set");
        return None;
    }
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL is required");
    let options = PgConnectOptions::from_str(&url).expect("parse DATABASE_URL");
    assert!(
        matches!(options.get_host(), "localhost" | "127.0.0.1" | "::1"),
        "refusing sponsored invoice coverage test against non-local host: {}",
        options.get_host()
    );
    let pool = db::connect(&url).await.expect("connect");
    db::migrate(&pool).await.expect("migrate");
    Some(pool)
}

#[derive(Clone)]
struct StripeResponses(Arc<Mutex<HashMap<String, serde_json::Value>>>);

async fn stripe_handler(
    axum::extract::State(responses): axum::extract::State<StripeResponses>,
    request: Request<Body>,
) -> Response<Body> {
    let query = request
        .uri()
        .query()
        .map(|query| {
            url::form_urlencoded::parse(query.as_bytes())
                .into_owned()
                .collect::<HashMap<_, _>>()
        })
        .unwrap_or_default();
    let path = request.uri().path();
    let scoped_key = match path {
        "/v1/invoices" => query
            .get("subscription")
            .map(|subscription| format!("/v1/invoices?subscription={subscription}")),
        "/v1/invoice_payments" | "/v1/credit_notes" => query
            .get("invoice")
            .map(|invoice| format!("{path}?invoice={invoice}")),
        "/v1/refunds" | "/v1/disputes" => query
            .get("payment_intent")
            .map(|payment_intent| format!("{path}?payment_intent={payment_intent}")),
        _ => None,
    };
    let responses = responses.0.lock().unwrap();
    let value = scoped_key
        .as_deref()
        .and_then(|key| responses.get(key))
        .or_else(|| responses.get(path))
        .cloned()
        .unwrap_or_else(|| serde_json::json!({"error":{"message":"not found"}}));
    let status = if value.get("error").is_some() {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::OK
    };
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&value).unwrap()))
        .unwrap()
}

async fn stripe_server(
    responses: HashMap<String, serde_json::Value>,
) -> (Url, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/{*path}", any(stripe_handler))
                .with_state(StripeResponses(Arc::new(Mutex::new(responses)))),
        )
        .await
        .unwrap();
    });
    (Url::parse(&format!("http://{address}/")).unwrap(), task)
}

fn signature(payload: &str) -> String {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut mac = Hmac::<Sha256>::new_from_slice(WEBHOOK_SECRET.as_bytes()).unwrap();
    mac.update(format!("{timestamp}.{payload}").as_bytes());
    let digest = mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("t={timestamp},v1={digest}")
}

async fn post_webhook(app: &Router, payload: &str) -> StatusCode {
    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/billing/webhook")
                .header("content-type", "application/json")
                .header("Stripe-Signature", signature(payload))
                .body(Body::from(payload.to_owned()))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

struct SponsoredOperationFixture<'a> {
    organization_id: &'a str,
    owner_id: &'a str,
    beneficiary_id: &'a str,
    offer: BillingOffer,
    item_id: Option<&'a str>,
    customer_id: &'a str,
    subscription_id: &'a str,
    start: i64,
    end: i64,
    now: i64,
}

async fn seed_sponsored_operation(pool: &PgPool, fixture: SponsoredOperationFixture<'_>) -> String {
    let SponsoredOperationFixture {
        organization_id,
        owner_id,
        beneficiary_id,
        offer,
        item_id,
        customer_id,
        subscription_id,
        start,
        end,
        now,
    } = fixture;
    let request = SponsoredSeatRequest {
        action: SponsoredSeatAction::Add,
        beneficiary_id: beneficiary_id.to_owned(),
        replacement_beneficiary_id: None,
        offer,
        quote_version: 1,
        quote_expires_at_epoch: now + 1_800,
        effective_from: start,
        effective_until: None,
        idempotency_key: format!("invoice-test-{beneficiary_id}"),
    };
    let mut tx = pool.begin().await.unwrap();
    let operation = begin_operation(&mut tx, organization_id, owner_id, &request, now)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    if let Some(founding_offer) = match offer {
        BillingOffer::FoundingMonthly => Some(FoundingOffer::Monthly),
        BillingOffer::FoundingAnnual => Some(FoundingOffer::Annual),
        _ => None,
    } {
        let mut tx = pool.begin().await.unwrap();
        let outcome = reserve_with_payer_kind_until(
            &mut tx,
            &format!("founding:{}", operation.operation_id),
            &operation.operation_id,
            beneficiary_id,
            FoundingPayerKind::Sponsor,
            organization_id,
            founding_offer,
            1,
            now + 1_800,
            end + sotto_server::founding_allocator::SETTLEMENT_RECOVERY_SECONDS,
            now,
        )
        .await
        .unwrap();
        assert!(matches!(outcome, ReservationOutcome::Created(_)));
        tx.commit().await.unwrap();
    }

    let mut tx = pool.begin().await.unwrap();
    record_provider_update(
        &mut tx,
        &operation.operation_id,
        Some(customer_id),
        subscription_id,
        None,
        Some(start),
        item_id,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    operation.operation_id
}

async fn execute_own_job(
    pool: &PgPool,
    event_id: &str,
    beneficiary_id: &str,
    worker_id: &str,
    executor: &mut sotto_server::cloud_provider_stripe_refresh_executor::StripeRefreshExecutor,
) {
    let (job_id, allocation_id, source_id): (String, String, String) = sqlx::query_as(
        "SELECT job_id, allocation_id, coverage_source_id FROM cloud_provider_refresh_jobs \
         WHERE event_id = $1 AND beneficiary_id = $2 AND status = 'pending'",
    )
    .bind(event_id)
    .bind(beneficiary_id)
    .fetch_one(pool)
    .await
    .unwrap();
    let claimed = sqlx::query(
        "UPDATE cloud_provider_refresh_jobs SET status = 'leased', attempt_count = attempt_count + 1, \
         lease_owner = $2, lease_expires_at = now() + interval '5 minutes' \
         WHERE job_id = $1 AND status = 'pending'",
    )
    .bind(&job_id)
    .bind(worker_id)
    .execute(pool)
    .await
    .unwrap();
    assert_eq!(claimed.rows_affected(), 1);
    let lease = RefreshJobLease {
        job_id,
        context: sotto_server::cloud_provider::ProviderContext::new(
            "stripe",
            "acct_sponsored_test",
            sotto_server::cloud_provider::ProviderEnvironment::Test,
        )
        .unwrap(),
        event_id: event_id.to_owned(),
        beneficiary_id: beneficiary_id.to_owned(),
        allocation_id,
        source_id,
        worker_id: worker_id.to_owned(),
        attempt_count: 1,
    };
    let inputs = sotto_server::cloud_provider_refresh_inputs::load(pool, &lease)
        .await
        .expect("load beneficiary-scoped refresh inputs");
    sotto_server::cloud_provider_refresh_worker::RefreshJobExecutor::execute(executor, &inputs)
        .await
        .unwrap_or_else(|error| panic!("execute provider job: {error:?}"));
    sotto_server::cloud_provider_refresh_jobs::complete(pool, &lease)
        .await
        .expect("complete beneficiary-scoped refresh job");
}

#[tokio::test]
async fn paid_sponsored_invoice_fans_out_one_event_to_each_named_beneficiary() {
    // This is a dedicated integration-test binary, so its single test can enable the opt-in
    // billing branch without sharing process environment with other tests.
    std::env::set_var("SOTTO_SPONSORED_BILLING_ENABLED", "1");
    let Some(pool) = pool().await else {
        return;
    };
    let fixture = Uuid::new_v4().simple().to_string();
    let organization_id = format!("sponsored-coverage-org-{fixture}");
    let owner_id = format!("sponsored-coverage-owner-{fixture}");
    let monthly_user = format!("sponsored-coverage-monthly-{fixture}");
    let founding_user = format!("sponsored-coverage-founding-{fixture}");
    let unbound_item_user = format!("sponsored-coverage-unbound-item-{fixture}");
    let other_subscription_user = format!("sponsored-coverage-other-subscription-{fixture}");
    sqlx::query(
        "INSERT INTO users (id, oauth_provider, oauth_subject) \
         VALUES ($1, 'sponsored-coverage-test', $1), \
                ($2, 'sponsored-coverage-test', $2), \
                ($3, 'sponsored-coverage-test', $3), \
                ($4, 'sponsored-coverage-test', $4), \
                ($5, 'sponsored-coverage-test', $5)",
    )
    .bind(&owner_id)
    .bind(&monthly_user)
    .bind(&founding_user)
    .bind(&unbound_item_user)
    .bind(&other_subscription_user)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO organizations (id, enc_name, created_by) \
         VALUES ($1, decode('6f7267','hex'), $2)",
    )
    .bind(&organization_id)
    .bind(&owner_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO organization_memberships (org_id, user_id, role) \
         VALUES ($1, $2, 'owner')",
    )
    .bind(&organization_id)
    .bind(&owner_id)
    .execute(&pool)
    .await
    .unwrap();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let period_start = now + 60;
    let period_end = period_start + 2_592_000;
    let customer_id = format!("cus_sponsored_{fixture}");
    let subscription_id = format!("sub_sponsored_{fixture}");
    let monthly_item = format!("si_month_{fixture}");
    let founding_item = format!("si_found_{fixture}");
    seed_sponsored_operation(
        &pool,
        SponsoredOperationFixture {
            organization_id: &organization_id,
            owner_id: &owner_id,
            beneficiary_id: &monthly_user,
            offer: BillingOffer::StandardMonthly,
            item_id: Some(&monthly_item),
            customer_id: &customer_id,
            subscription_id: &subscription_id,
            start: period_start,
            end: period_end,
            now,
        },
    )
    .await;
    seed_sponsored_operation(
        &pool,
        SponsoredOperationFixture {
            organization_id: &organization_id,
            owner_id: &owner_id,
            beneficiary_id: &unbound_item_user,
            offer: BillingOffer::StandardMonthly,
            // Production records a provider update before Stripe returns an item binding.
            item_id: None,
            customer_id: &customer_id,
            subscription_id: &subscription_id,
            start: period_start,
            end: period_end,
            now,
        },
    )
    .await;
    let old_subscription_id = format!("sub_old_{fixture}");
    seed_sponsored_operation(
        &pool,
        SponsoredOperationFixture {
            organization_id: &organization_id,
            owner_id: &owner_id,
            beneficiary_id: &other_subscription_user,
            offer: BillingOffer::StandardMonthly,
            item_id: Some(&monthly_item),
            customer_id: &customer_id,
            subscription_id: &old_subscription_id,
            start: period_start,
            end: period_end,
            now,
        },
    )
    .await;
    seed_sponsored_operation(
        &pool,
        SponsoredOperationFixture {
            organization_id: &organization_id,
            owner_id: &owner_id,
            beneficiary_id: &founding_user,
            offer: BillingOffer::FoundingMonthly,
            // A multi price first Checkout also has no item binding before this invoice.
            item_id: None,
            customer_id: &customer_id,
            subscription_id: &subscription_id,
            start: period_start,
            end: period_end,
            now,
        },
    )
    .await;

    // This person previously paid personally for the preceding term. Keep that old source in
    // their registered set so the sponsored refresh must combine both payer-specific readers.
    let personal_customer = format!("cus_personal_{fixture}");
    let personal_subscription = format!("sub_personal_{fixture}");
    let personal_item = format!("si_personal_{fixture}");
    let personal_invoice_id = format!("in_personal_{fixture}");
    let personal_period_start = period_start - 2_592_000;
    let personal_reference = format!("personal-reference-{fixture}");
    let personal_allocation = VerifiedAllocation::new(
        format!("personal-allocation-{fixture}"),
        format!("personal-payer-{fixture}"),
        &personal_customer,
        PayerKind::Personal,
        &monthly_user,
        &personal_subscription,
        &personal_item,
        &personal_reference,
        format!("personal-source-{fixture}"),
        personal_period_start,
        Some(period_start),
        AllocationState::Ended,
        format!("stripe:personal-owner:{fixture}"),
    )
    .unwrap();
    let previous_sponsor_customer = format!("cus_previous_sponsor_{fixture}");
    let previous_sponsor_subscription = format!("sub_previous_sponsor_{fixture}");
    let previous_sponsor_item = format!("si_previous_sponsor_{fixture}");
    let previous_sponsor_reference = format!("sponsored-previous-reference-{fixture}");
    let previous_sponsor_start = personal_period_start - 2_592_000;
    let previous_sponsor_allocation = VerifiedAllocation::new(
        format!("previous-sponsor-allocation-{fixture}"),
        format!("previous-sponsor-payer-{fixture}"),
        &previous_sponsor_customer,
        PayerKind::Sponsor,
        &monthly_user,
        &previous_sponsor_subscription,
        &previous_sponsor_item,
        &previous_sponsor_reference,
        format!("previous-sponsor-source-{fixture}"),
        previous_sponsor_start,
        Some(personal_period_start),
        AllocationState::Ended,
        format!("stripe:previous-sponsor-owner:{fixture}"),
    )
    .unwrap();
    let context =
        ProviderContext::new("stripe", "acct_sponsored_test", ProviderEnvironment::Test).unwrap();
    let mut tx = pool.begin().await.unwrap();
    register_verified_allocation(&mut tx, &context, &personal_allocation)
        .await
        .expect("register the person's historical personal source");
    register_verified_allocation(&mut tx, &context, &previous_sponsor_allocation)
        .await
        .expect("register the person's earlier sponsored source");
    record_sponsored_allocation_term(
        &mut tx,
        &context,
        &previous_sponsor_allocation,
        "price_sponsored_standard_month",
    )
    .await
    .expect("record the earlier sponsored term");
    tx.commit().await.unwrap();

    let catalogue = BillingPriceIds {
        standard_monthly: "price_sponsored_standard_month".into(),
        standard_annual: "price_sponsored_standard_year".into(),
        founding_monthly: "price_sponsored_founding_month".into(),
        founding_annual: "price_sponsored_founding_year".into(),
    };
    let invoice_id = format!("in_sponsored_{fixture}");
    let payment_id = format!("pi_sponsored_{fixture}");
    let line = |line_id: String, item_id: String, price_id: String, quantity: u64| {
        serde_json::json!({
            "id":line_id, "object":"line_item", "invoice":invoice_id,
            "quantity":quantity, "livemode":false,
            "parent":{"type":"subscription_item_details","subscription_item_details":{
                "subscription":subscription_id, "subscription_item":item_id, "proration":false
            }},
            "pricing":{"type":"price_details","price_details":{"price":price_id}},
            "period":{"start":period_start,"end":period_end}
        })
    };
    let invoice_response = serde_json::json!({
        "id":invoice_id, "object":"invoice", "customer":customer_id,
        "parent":{"type":"subscription_details","subscription_details":{
            "subscription":subscription_id, "metadata":{"organization_id":organization_id}
        }},
        "status":"paid", "currency":"gbp", "amount_paid":797, "amount_due":797,
        "amount_remaining":0, "amount_overpaid":0, "amount_paid_off_stripe":0,
        "livemode":false,
        "status_transitions":{"paid_at":now + 10},
        "lines":{"data":[
            line(format!("il_month_{fixture}"),monthly_item.clone(),catalogue.standard_monthly.clone(), 2),
            line(format!("il_found_{fixture}"),founding_item.clone(),catalogue.founding_monthly.clone(), 1)
        ]}
    });
    let personal_line = serde_json::json!({
        "id":format!("il_personal_{fixture}"), "object":"line_item", "invoice":personal_invoice_id,
        "quantity":1, "livemode":false,
        "parent":{"type":"subscription_item_details","subscription_item_details":{
            "subscription":personal_subscription, "subscription_item":personal_item, "proration":false
        }},
        "pricing":{"type":"price_details","price_details":{"price":catalogue.standard_monthly}},
        "metadata":{"sotto_allocation_reference":personal_reference},
        "period":{"start":personal_period_start,"end":period_start}
    });
    let personal_invoice = serde_json::json!({
        "id":personal_invoice_id, "object":"invoice", "customer":personal_customer,
        "parent":{"type":"subscription_details","subscription_details":{
            "subscription":personal_subscription,
            "metadata":{"sotto_allocation_reference":personal_reference}
        }},
        "status":"paid", "currency":"gbp", "amount_paid":299, "amount_due":299,
        "amount_remaining":0, "amount_overpaid":0, "amount_paid_off_stripe":0,
        "livemode":false, "lines":{"data":[personal_line]}
    });
    let previous_sponsor_invoice_id = format!("in_previous_sponsor_{fixture}");
    let previous_sponsor_line = serde_json::json!({
        "id":format!("il_previous_sponsor_{fixture}"), "object":"line_item",
        "invoice":previous_sponsor_invoice_id, "quantity":1, "livemode":false,
        "parent":{"type":"subscription_item_details","subscription_item_details":{
            "subscription":previous_sponsor_subscription,
            "subscription_item":previous_sponsor_item, "proration":false
        }},
        "pricing":{"type":"price_details","price_details":{"price":catalogue.standard_monthly}},
        "period":{"start":previous_sponsor_start,"end":personal_period_start}
    });
    let previous_sponsor_invoice = serde_json::json!({
        "id":previous_sponsor_invoice_id, "object":"invoice",
        "customer":previous_sponsor_customer,
        "parent":{"type":"subscription_details","subscription_details":{
            "subscription":previous_sponsor_subscription
        }},
        "status":"paid", "currency":"gbp", "amount_paid":299, "amount_due":299,
        "amount_remaining":0, "amount_overpaid":0, "amount_paid_off_stripe":0,
        "livemode":false, "lines":{"data":[previous_sponsor_line]}
    });
    let lines_response = serde_json::json!({
        "object":"list", "has_more":false,
        "data":invoice_response["lines"]["data"].as_array().unwrap()
    });
    let payment = serde_json::json!({
        "id":format!("inpay_{fixture}"), "object":"invoice_payment", "invoice":invoice_id,
        "status":"paid", "amount_paid":797, "amount_requested":797, "currency":"gbp",
        "livemode":false, "payment":{"type":"payment_intent","payment_intent":payment_id}
    });
    let history_response = serde_json::json!({
        "object":"list", "has_more":false, "data":[invoice_response.clone()]
    });
    let personal_history_response = serde_json::json!({
        "object":"list", "has_more":false, "data":[personal_invoice.clone()]
    });
    let previous_sponsor_history_response = serde_json::json!({
        "object":"list", "has_more":false, "data":[previous_sponsor_invoice.clone()]
    });
    let invalid_invoice_id = format!("in_sponsored_unpaid_{fixture}");
    let mut invalid_invoice_response = invoice_response.clone();
    invalid_invoice_response["id"] = serde_json::json!(invalid_invoice_id);
    invalid_invoice_response["status"] = serde_json::json!("draft");
    let (origin, stripe_task) = stripe_server(HashMap::from([
        (
            "/v1/account".into(),
            serde_json::json!({"id":"acct_sponsored_test","livemode":false}),
        ),
        ("/v1/invoices".into(), history_response),
        (
            format!("/v1/invoices?subscription={personal_subscription}"),
            personal_history_response,
        ),
        (
            format!("/v1/invoices?subscription={previous_sponsor_subscription}"),
            previous_sponsor_history_response,
        ),
        (
            format!("/v1/invoices/{invoice_id}"),
            invoice_response.clone(),
        ),
        (
            format!("/v1/invoices/{personal_invoice_id}"),
            personal_invoice,
        ),
        (
            format!("/v1/invoices/{previous_sponsor_invoice_id}"),
            previous_sponsor_invoice,
        ),
        (
            format!("/v1/invoices/{personal_invoice_id}/lines"),
            serde_json::json!({"object":"list","has_more":false,"data":[personal_line]}),
        ),
        (
            format!("/v1/invoices/{previous_sponsor_invoice_id}/lines"),
            serde_json::json!({
                "object":"list","has_more":false,"data":[previous_sponsor_line]
            }),
        ),
        (
            format!("/v1/subscriptions/{personal_subscription}"),
            serde_json::json!({
                "id":personal_subscription,"customer":personal_customer,
                "status":"active","livemode":false
            }),
        ),
        (
            format!("/v1/invoice_payments?invoice={previous_sponsor_invoice_id}"),
            serde_json::json!({"object":"list","has_more":false,"data":[{
                "id":format!("inpay_previous_sponsor_{fixture}"), "object":"invoice_payment",
                "invoice":previous_sponsor_invoice_id, "status":"paid", "amount_paid":299,
                "amount_requested":299, "currency":"gbp", "livemode":false,
                "payment":{"type":"payment_intent","payment_intent":format!("pi_previous_sponsor_{fixture}")}
            }]}),
        ),
        (
            format!("/v1/invoices/{invalid_invoice_id}"),
            invalid_invoice_response,
        ),
        (format!("/v1/invoices/{invoice_id}/lines"), lines_response),
        (
            "/v1/invoice_payments".into(),
            serde_json::json!({"object":"list","has_more":false,"data":[payment]}),
        ),
        (
            format!("/v1/invoice_payments?invoice={personal_invoice_id}"),
            serde_json::json!({"object":"list","has_more":false,"data":[{
                "id":format!("inpay_personal_{fixture}"), "object":"invoice_payment",
                "invoice":personal_invoice_id, "status":"paid", "amount_paid":299,
                "amount_requested":299, "currency":"gbp", "livemode":false,
                "payment":{"type":"payment_intent","payment_intent":format!("pi_personal_{fixture}")}
            }]}),
        ),
        (
            "/v1/credit_notes".into(),
            serde_json::json!({"object":"list","has_more":false,"data":[]}),
        ),
        (
            "/v1/refunds".into(),
            serde_json::json!({"object":"list","has_more":false,"data":[]}),
        ),
        (
            "/v1/disputes".into(),
            serde_json::json!({"object":"list","has_more":false,"data":[]}),
        ),
    ]))
    .await;
    let coverage = StripeCoverageConfig::new(
        "acct_sponsored_test",
        sotto_server::cloud_provider::ProviderEnvironment::Test,
        catalogue.standard_monthly.clone(),
        catalogue.standard_annual.clone(),
    )
    .unwrap()
    .with_founding_prices(
        catalogue.founding_monthly.clone(),
        catalogue.founding_annual.clone(),
    )
    .unwrap();
    let stripe = StripeReadClient::for_test(
        "sk_test_sponsored_coverage",
        &coverage,
        origin,
        StripeReadLimits::default(),
    )
    .unwrap();
    let worker_stripe = stripe.clone();
    let sponsored_coverage =
        sotto_server::cloud_provider_stripe_sponsored::SponsoredStripeCoverageConfig::new(
            "acct_sponsored_test",
            sotto_server::cloud_provider::ProviderEnvironment::Test,
            "price_sponsored_standard_month",
            "price_sponsored_standard_year",
            "price_sponsored_founding_month",
            "price_sponsored_founding_year",
        )
        .unwrap();
    let billing = BillingState::from_config(BillingConfig {
        api_key: "rk_test_sponsored_coverage".into(),
        webhook_secret: WEBHOOK_SECRET.into(),
        price_id: "price_legacy_test".into(),
        price_catalogue: Some(catalogue),
        cloud_portal_configuration_id: None,
        cloud_sales_enabled: false,
        return_url: "https://app.sotto.test".into(),
    })
    .with_personal_coverage(coverage.clone(), stripe, true);
    let state = AppState {
        deployment_mode: sotto_server::config::DeploymentMode::Cloud,
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
        pool: pool.clone(),
        oauth: None,
        oauth_config: None,
        billing: Some(billing),
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
    let app = Router::new()
        .merge(sotto_server::billing::router())
        .with_state(state);
    let event_id = format!("evt_sponsored_coverage_{fixture}");
    let event = serde_json::json!({
        "id":event_id, "created":now + 10, "api_version":STRIPE_API_VERSION,
        "type":"invoice.paid", "livemode":false, "data":{"object":invoice_response}
    })
    .to_string();

    assert_eq!(post_webhook(&app, &event).await, StatusCode::OK);
    let provider_state: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT \
           (SELECT count(*) FROM cloud_provider_event_receipts WHERE event_id = $1), \
           (SELECT count(*) FROM cloud_provider_event_scopes WHERE event_id = $1), \
           (SELECT count(*) FROM cloud_provider_refresh_jobs WHERE event_id = $1), \
           (SELECT count(*) FROM cloud_provider_allocations WHERE provider_subscription_id = $2)",
    )
    .bind(&event_id)
    .bind(&subscription_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(provider_state, (1, 3, 3, 3));
    let scoped_beneficiaries: Vec<String> = sqlx::query_scalar(
        "SELECT beneficiary_id FROM cloud_provider_event_scopes WHERE event_id = $1 ORDER BY beneficiary_id",
    )
    .bind(&event_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    let mut expected_beneficiaries = vec![
        monthly_user.clone(),
        unbound_item_user.clone(),
        founding_user.clone(),
    ];
    expected_beneficiaries.sort();
    assert_eq!(scoped_beneficiaries, expected_beneficiaries);

    let mut executor =
        sotto_server::cloud_provider_stripe_refresh_executor::StripeRefreshExecutor::new(
            pool.clone(),
            worker_stripe,
            coverage,
            sponsored_coverage,
            Default::default(),
        );
    execute_own_job(
        &pool,
        &event_id,
        &monthly_user,
        &format!("sponsored-worker-one-{fixture}"),
        &mut executor,
    )
    .await;
    execute_own_job(
        &pool,
        &event_id,
        &unbound_item_user,
        &format!("sponsored-worker-unbound-{fixture}"),
        &mut executor,
    )
    .await;
    let first_job_state: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT status, last_error_code FROM cloud_provider_refresh_jobs WHERE event_id = $1",
    )
    .bind(&event_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(first_job_state
        .iter()
        .any(|(status, _)| status == "completed"));
    let intermediate: (String, i64, i64) = sqlx::query_as(
        "SELECT receipt.status, \
           (SELECT count(*) FROM cloud_provider_event_scopes WHERE event_id = $1 AND status = 'applied'), \
           (SELECT count(*) FROM cloud_provider_event_scopes WHERE event_id = $1 AND status = 'pending') \
         FROM cloud_provider_event_receipts AS receipt WHERE receipt.event_id = $1",
    )
    .bind(&event_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(intermediate, ("pending".into(), 2, 1));

    execute_own_job(
        &pool,
        &event_id,
        &founding_user,
        &format!("sponsored-worker-two-{fixture}"),
        &mut executor,
    )
    .await;
    let final_receipt: (String, i64) = sqlx::query_as(
        "SELECT status, (SELECT count(*) FROM cloud_provider_event_scopes \
         WHERE event_id = $1 AND status = 'applied') \
         FROM cloud_provider_event_receipts WHERE event_id = $1",
    )
    .bind(&event_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(final_receipt, ("applied".into(), 3));

    // Duplicate delivery reuses the same global event and leaves all beneficiary work unchanged.
    assert_eq!(post_webhook(&app, &event).await, StatusCode::OK);
    let replay_counts: (i64, i64, i64) = sqlx::query_as(
        "SELECT \
           (SELECT count(*) FROM cloud_provider_event_receipts WHERE event_id = $1), \
           (SELECT count(*) FROM cloud_provider_event_scopes WHERE event_id = $1), \
           (SELECT count(*) FROM cloud_provider_refresh_jobs WHERE event_id = $1)",
    )
    .bind(&event_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(replay_counts, (1, 3, 3));

    // Stripe's current invoice snapshot can be permanently unusable even though the signed
    // webhook reports payment. Financial settlement still completes, while coverage records a
    // durable rejection and creates no beneficiary scopes.
    let rejected_event_id = format!("evt_sponsored_unpaid_{fixture}");
    let mut rejected_event_invoice = invoice_response.clone();
    rejected_event_invoice["id"] = serde_json::json!(invalid_invoice_id);
    let rejected_event = serde_json::json!({
        "id":rejected_event_id, "created":now + 20, "api_version":STRIPE_API_VERSION,
        "type":"invoice.paid", "livemode":false, "data":{"object":rejected_event_invoice}
    })
    .to_string();
    assert_eq!(post_webhook(&app, &rejected_event).await, StatusCode::OK);
    let rejected_state: (String, String, i64, i64) = sqlx::query_as(
        "SELECT status, rejection_code, \
           (SELECT count(*) FROM stripe_webhook_events WHERE event_id = $1 AND processed_at IS NOT NULL), \
           (SELECT count(*) FROM cloud_provider_event_scopes WHERE event_id = $1) \
         FROM cloud_provider_event_receipts WHERE event_id = $1",
    )
    .bind(&rejected_event_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(rejected_state.0, "rejected");
    assert_eq!(rejected_state.1, "sponsored_invoice_evidence_rejected");
    assert_eq!((rejected_state.2, rejected_state.3), (1, 0));

    stripe_task.abort();

    let beneficiary_ids = vec![
        monthly_user.clone(),
        founding_user.clone(),
        unbound_item_user,
        other_subscription_user,
    ];
    let event_ids = vec![event_id.clone(), rejected_event_id];
    let subscription_ids = vec![
        subscription_id.clone(),
        personal_subscription.clone(),
        previous_sponsor_subscription.clone(),
    ];
    let customer_ids = vec![
        customer_id.clone(),
        personal_customer.clone(),
        previous_sponsor_customer.clone(),
    ];
    let mut cleanup = pool.begin().await.unwrap();
    sqlx::query(
        "DELETE FROM stripe_subscription_watermarks \
         WHERE event_id = ANY($1) OR subscription_id = ANY($2)",
    )
    .bind(&event_ids)
    .bind(&subscription_ids)
    .execute(&mut *cleanup)
    .await
    .unwrap();
    sqlx::query("DELETE FROM stripe_webhook_events WHERE event_id = ANY($1)")
        .bind(&event_ids)
        .execute(&mut *cleanup)
        .await
        .unwrap();
    sqlx::query("DELETE FROM cloud_provider_stripe_renewal_failures WHERE event_id = ANY($1)")
        .bind(&event_ids)
        .execute(&mut *cleanup)
        .await
        .unwrap();
    sqlx::query("DELETE FROM cloud_provider_invalidation_associations WHERE event_id = ANY($1)")
        .bind(&event_ids)
        .execute(&mut *cleanup)
        .await
        .unwrap();
    sqlx::query("DELETE FROM cloud_provider_event_receipts WHERE event_id = ANY($1)")
        .bind(&event_ids)
        .execute(&mut *cleanup)
        .await
        .unwrap();
    sqlx::query(
        "DELETE FROM billing_founding_reservations \
         WHERE beneficiary_id = $1 OR payer_id = $2",
    )
    .bind(&founding_user)
    .bind(&organization_id)
    .execute(&mut *cleanup)
    .await
    .unwrap();
    sqlx::query("DELETE FROM billing_founding_schedule_jobs WHERE beneficiary_id = $1")
        .bind(&founding_user)
        .execute(&mut *cleanup)
        .await
        .unwrap();
    sqlx::query("DELETE FROM billing_founding_awards WHERE beneficiary_id = $1")
        .bind(&founding_user)
        .execute(&mut *cleanup)
        .await
        .unwrap();
    sqlx::query("DELETE FROM billing_sponsored_seats WHERE organization_id = $1")
        .bind(&organization_id)
        .execute(&mut *cleanup)
        .await
        .unwrap();
    sqlx::query("DELETE FROM billing_sponsored_operations WHERE organization_id = $1")
        .bind(&organization_id)
        .execute(&mut *cleanup)
        .await
        .unwrap();
    sqlx::query("DELETE FROM billing_sponsored_subscription_items WHERE organization_id = $1")
        .bind(&organization_id)
        .execute(&mut *cleanup)
        .await
        .unwrap();
    sqlx::query("DELETE FROM billing_sponsored_subscriptions WHERE organization_id = $1")
        .bind(&organization_id)
        .execute(&mut *cleanup)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE cloud_coverage_coordinators SET current_attempt_id = NULL \
         WHERE beneficiary_id = ANY($1)",
    )
    .bind(&beneficiary_ids)
    .execute(&mut *cleanup)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE cloud_coverage_heads SET current_revision = NULL WHERE beneficiary_id = ANY($1)",
    )
    .bind(&beneficiary_ids)
    .execute(&mut *cleanup)
    .await
    .unwrap();
    sqlx::query("DELETE FROM cloud_coverage_revision_facts WHERE beneficiary_id = ANY($1)")
        .bind(&beneficiary_ids)
        .execute(&mut *cleanup)
        .await
        .unwrap();
    sqlx::query("DELETE FROM cloud_coverage_collection_attempts WHERE beneficiary_id = ANY($1)")
        .bind(&beneficiary_ids)
        .execute(&mut *cleanup)
        .await
        .unwrap();
    sqlx::query(
        "DELETE FROM cloud_provider_stripe_renewal_failures WHERE beneficiary_id = ANY($1)",
    )
    .bind(&beneficiary_ids)
    .execute(&mut *cleanup)
    .await
    .unwrap();
    sqlx::query(
        "DELETE FROM cloud_provider_sponsored_allocation_terms \
         WHERE allocation_id IN (SELECT allocation_id FROM cloud_provider_allocations \
             WHERE beneficiary_id = ANY($1))",
    )
    .bind(&beneficiary_ids)
    .execute(&mut *cleanup)
    .await
    .unwrap();
    sqlx::query("DELETE FROM cloud_provider_allocations WHERE beneficiary_id = ANY($1)")
        .bind(&beneficiary_ids)
        .execute(&mut *cleanup)
        .await
        .unwrap();
    sqlx::query("DELETE FROM cloud_coverage_sources WHERE beneficiary_id = ANY($1)")
        .bind(&beneficiary_ids)
        .execute(&mut *cleanup)
        .await
        .unwrap();
    sqlx::query("DELETE FROM cloud_coverage_revisions WHERE beneficiary_id = ANY($1)")
        .bind(&beneficiary_ids)
        .execute(&mut *cleanup)
        .await
        .unwrap();
    sqlx::query("DELETE FROM cloud_coverage_heads WHERE beneficiary_id = ANY($1)")
        .bind(&beneficiary_ids)
        .execute(&mut *cleanup)
        .await
        .unwrap();
    sqlx::query("DELETE FROM cloud_coverage_coordinators WHERE beneficiary_id = ANY($1)")
        .bind(&beneficiary_ids)
        .execute(&mut *cleanup)
        .await
        .unwrap();
    sqlx::query("DELETE FROM cloud_provider_payers WHERE provider_customer_id = ANY($1)")
        .bind(&customer_ids)
        .execute(&mut *cleanup)
        .await
        .unwrap();
    sqlx::query("DELETE FROM organization_memberships WHERE org_id = $1")
        .bind(&organization_id)
        .execute(&mut *cleanup)
        .await
        .unwrap();
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(&organization_id)
        .execute(&mut *cleanup)
        .await
        .unwrap();
    sqlx::query("DELETE FROM users WHERE id = ANY($1)")
        .bind(vec![owner_id, monthly_user, founding_user])
        .execute(&mut *cleanup)
        .await
        .unwrap();
    cleanup.commit().await.unwrap();
}
