//! Credentialed, read-only Stripe test-mode coverage contract verification.
//!
//! This test is deliberately opt-in. Normal CI uses the loopback fixtures; an explicitly
//! configured sandbox run uses the same bounded `StripeReadClient` that the dormant adapter will
//! call and writes only a field-presence summary. It never creates, updates, or cancels a Stripe
//! resource.

use std::{env, fs, path::Path};

use serde_json::json;
use sha2::{Digest, Sha256};
use sotto_server::billing::STRIPE_API_VERSION;
use sotto_server::cloud_provider::{PayerKind, ProviderEnvironment};
use sotto_server::cloud_provider_stripe::{StripeAllocationBinding, StripeCoverageConfig};
use sotto_server::cloud_provider_stripe_http::{
    StripePersonalInvoiceHistoryEntry, StripePersonalInvoiceHistoryResult, StripeReadClient,
    StripeReadLimits,
};

const RUN_SANDBOX_TESTS: &str = "SOTTO_RUN_STRIPE_SANDBOX_TESTS";

fn required(name: &str) -> String {
    env::var(name).unwrap_or_else(|_| panic!("{name} is required for an opt-in sandbox run"))
}

fn fingerprint(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn limits() -> StripeReadLimits {
    StripeReadLimits {
        request_timeout: std::time::Duration::from_secs(10),
        session_timeout: std::time::Duration::from_secs(120),
        max_response_bytes: 1024 * 1024,
        max_total_response_bytes: 16 * 1024 * 1024,
        max_pages: 64,
        max_requests: 256,
        max_records: 10_000,
        max_retries: 2,
        max_retry_after: std::time::Duration::from_secs(30),
    }
}

#[tokio::test]
async fn reads_the_configured_stripe_test_contract_without_mutation() {
    if env::var(RUN_SANDBOX_TESTS).as_deref() != Ok("1") {
        eprintln!(
            "skipping Stripe sandbox contract test: set {RUN_SANDBOX_TESTS}=1 with the required test-mode variables"
        );
        return;
    }

    let api_key = required("STRIPE_SANDBOX_READ_KEY");
    assert!(
        api_key.starts_with("rk_test_"),
        "STRIPE_SANDBOX_READ_KEY must be a restricted Stripe test-mode key"
    );
    let account_id = required("STRIPE_SANDBOX_ACCOUNT_ID");
    let customer_id = required("STRIPE_SANDBOX_CUSTOMER_ID");
    let subscription_id = required("STRIPE_SANDBOX_SUBSCRIPTION_ID");
    let monthly_price_id = required("STRIPE_SANDBOX_MONTHLY_PRICE_ID");
    let annual_price_id = required("STRIPE_SANDBOX_ANNUAL_PRICE_ID");
    let allocation_reference = required("STRIPE_SANDBOX_ALLOCATION_REFERENCE");
    let provider_item_id = required("STRIPE_SANDBOX_PROVIDER_ITEM_ID");

    let config = StripeCoverageConfig::new(
        account_id.clone(),
        ProviderEnvironment::Test,
        monthly_price_id,
        annual_price_id,
    )
    .expect("sandbox Stripe coverage configuration must be valid");
    let binding = StripeAllocationBinding::new(
        allocation_reference,
        customer_id.clone(),
        subscription_id.clone(),
        provider_item_id,
        PayerKind::Personal,
    )
    .expect("sandbox Stripe allocation binding must be valid");
    let client = StripeReadClient::new(api_key, &config, limits())
        .expect("sandbox Stripe read client must be valid");
    let mut session = client.session();

    let account = client
        .account(&mut session)
        .await
        .expect("read Stripe account");
    assert!(!account.livemode, "sandbox account must not be live mode");
    assert!(
        account.id == account_id,
        "sandbox account identity mismatch"
    );

    let subscription = client
        .subscription(&mut session, &subscription_id, &customer_id)
        .await
        .expect("read configured Stripe subscription");
    assert!(
        subscription.id == subscription_id,
        "sandbox subscription identity mismatch"
    );
    assert!(
        subscription.customer_id.as_deref() == Some(customer_id.as_str()),
        "sandbox subscription customer mismatch"
    );
    assert_eq!(subscription.livemode, Some(false));
    let status = subscription
        .status
        .as_deref()
        .expect("sandbox subscription must include status");
    assert!(
        matches!(
            status,
            "active"
                | "canceled"
                | "incomplete"
                | "incomplete_expired"
                | "past_due"
                | "paused"
                | "trialing"
                | "unpaid"
        ),
        "sandbox returned an unsupported subscription status"
    );

    let history = client
        .personal_invoice_history(&mut session, &binding)
        .await
        .expect("read configured Stripe invoice history");
    let history = match history {
        StripePersonalInvoiceHistoryResult::Observed(history) => history,
        StripePersonalInvoiceHistoryResult::NeedsEvidence(needs) => panic!(
            "sandbox invoice history needs evidence for {} invoice(s)",
            needs.reasons().len()
        ),
    };
    let paid = history
        .entries()
        .iter()
        .filter(|entry| matches!(entry, StripePersonalInvoiceHistoryEntry::Paid(_)))
        .count();
    let non_paid = history
        .entries()
        .iter()
        .filter(|entry| matches!(entry, StripePersonalInvoiceHistoryEntry::NonPaid(_)))
        .count();
    assert!(paid > 0, "sandbox history must contain a paid invoice");
    assert!(
        non_paid > 0,
        "sandbox history must contain a non-paid invoice"
    );

    let report = json!({
        "schema": "sotto-stripe-sandbox-contract-v1",
        "api_version": STRIPE_API_VERSION,
        "account": {
            "id_fingerprint": fingerprint(&account.id),
            "livemode": account.livemode,
        },
        "subscription": {
            "id_fingerprint": fingerprint(&subscription.id),
            "customer_fingerprint": subscription.customer_id.as_deref().map(fingerprint),
            "livemode": subscription.livemode,
            "status": status,
        },
        "history": {
            "subscription_fingerprint": fingerprint(history.subscription_id()),
            "customer_fingerprint": fingerprint(history.customer_id()),
            "paid_entries": paid,
            "non_paid_entries": non_paid,
            "entry_count": history.entries().len(),
        },
        "bounds": {
            "requests": "shared session budget",
            "pages": "shared session budget",
            "records": "shared session budget",
            "bytes": "shared session budget",
            "deadline": "shared session budget",
        },
    });
    if let Ok(path) = env::var("STRIPE_SANDBOX_REPORT") {
        let path = Path::new(&path);
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).expect("create sandbox report directory");
        }
        fs::write(
            path,
            serde_json::to_vec_pretty(&report).expect("serialize sandbox report"),
        )
        .expect("write sanitized sandbox report");
    }
    println!(
        "{}",
        serde_json::to_string(&report).expect("serialize sandbox summary")
    );
}
