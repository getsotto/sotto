//! Black-box tests for the verified Stripe evidence boundary.

use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;
use sotto_server::cloud_provider::ProviderEnvironment;
use sotto_server::cloud_provider_stripe::{
    decode_paid_invoice, StripeContractError, StripeCoverageConfig, StripeInterval,
};

const SECRET: &str = "whsec_contract_test";
const NOW: i64 = 1_700_000_000;

fn config() -> StripeCoverageConfig {
    StripeCoverageConfig::new(
        "acct_test_sotto_contract",
        ProviderEnvironment::Test,
        "price_contract_month",
        "price_contract_year",
    )
    .unwrap()
}

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/stripe/invoice_paid_personal.json")).unwrap()
}

fn signed_payload(value: &Value, timestamp: i64) -> (Vec<u8>, String) {
    let payload = serde_json::to_vec(value).unwrap();
    let mut mac = Hmac::<Sha256>::new_from_slice(SECRET.as_bytes()).unwrap();
    mac.update(format!("{timestamp}.{}", String::from_utf8_lossy(&payload)).as_bytes());
    let signature = mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    (payload, format!("t={timestamp},v1={signature}"))
}

fn decode(
    value: &Value,
) -> Result<sotto_server::cloud_provider_stripe::StripeCoverageEvidence, StripeContractError> {
    let (payload, signature) = signed_payload(value, NOW);
    decode_paid_invoice(&payload, &signature, SECRET, NOW, &config())
}

#[test]
fn paid_personal_invoice_accepts_expanded_references_and_normalizes_evidence() {
    let evidence = decode(&fixture()).unwrap();
    assert_eq!(evidence.invoice_id, "in_contract_invoice");
    assert_eq!(evidence.customer_id, "cus_contract_person");
    assert_eq!(evidence.subscription_id, "sub_contract_person");
    assert_eq!(evidence.provider_item_id, "si_contract_person");
    assert_eq!(evidence.price_id, "price_contract_month");
    assert_eq!(evidence.allocation_reference, "allocation_contract_person");
    assert_eq!(evidence.currency, "gbp");
    assert_eq!(evidence.amount_paid, 299);
    assert_eq!(evidence.interval, StripeInterval::Month);
    assert_eq!(evidence.period_start, NOW);
    assert_eq!(evidence.period_end, 1702592000);
    assert_eq!(evidence.event.event_type, "invoice.paid");
    assert_eq!(
        evidence.event.subscription_id.as_deref(),
        Some("sub_contract_person")
    );
    assert_eq!(
        evidence.event.allocation_reference.as_deref(),
        Some("allocation_contract_person")
    );
    assert_eq!(
        evidence.evidence_reference,
        "stripe:invoice:in_contract_invoice:line:si_contract_person"
    );
}

#[test]
fn annual_standard_price_maps_to_a_year_interval() {
    let mut annual = fixture();
    annual["data"]["object"]["lines"]["data"][0]["price"]["id"] = json!("price_contract_year");
    annual["data"]["object"]["lines"]["data"][0]["price"]["recurring"]["interval"] = json!("year");
    let evidence = decode(&annual).unwrap();
    assert_eq!(evidence.interval, StripeInterval::Year);
    assert_eq!(evidence.price_id, "price_contract_year");
}

#[test]
fn irrelevant_payload_changes_do_not_change_the_event_hash() {
    let original = fixture();
    let mut changed = fixture();
    changed["data"]["object"]["metadata"]["irrelevant_customer_note"] = json!("a different value");
    changed["data"]["object"]["description"] = json!("ignored by the contract");
    let first = decode(&original).unwrap();
    let second = decode(&changed).unwrap();
    assert_eq!(
        first.event.normalized_payload_hash,
        second.event.normalized_payload_hash
    );
}

#[test]
fn signature_and_timestamp_fail_before_payload_is_interpreted() {
    let payload = fixture();
    let (raw, signature) = signed_payload(&payload, NOW);
    assert!(matches!(
        decode_paid_invoice(raw.as_slice(), "t=1700000000,v1=00", SECRET, NOW, &config()),
        Err(StripeContractError::InvalidSignature)
    ));
    assert!(matches!(
        decode_paid_invoice(&raw, &signature, SECRET, NOW + 301, &config()),
        Err(StripeContractError::InvalidSignature)
    ));
}

#[test]
fn unsupported_versions_types_and_contexts_fail_closed() {
    let mut unsupported_version = fixture();
    unsupported_version["api_version"] = json!("2099-01-01.dahlia");
    assert!(matches!(
        decode(&unsupported_version),
        Err(StripeContractError::UnsupportedApiVersion(_))
    ));

    let mut unsupported_type = fixture();
    unsupported_type["type"] = json!("invoice.payment_failed");
    assert!(matches!(
        decode(&unsupported_type),
        Err(StripeContractError::UnsupportedEventType(_))
    ));

    let mut wrong_account = fixture();
    wrong_account["account"] = json!("acct_other");
    assert!(matches!(
        decode(&wrong_account),
        Err(StripeContractError::ContextMismatch)
    ));

    let mut wrong_mode = fixture();
    wrong_mode["livemode"] = json!(true);
    assert!(matches!(
        decode(&wrong_mode),
        Err(StripeContractError::ContextMismatch)
    ));
}

#[test]
fn unpaid_quantity_price_and_missing_allocation_are_not_coverage() {
    let mut unpaid = fixture();
    unpaid["data"]["object"]["paid"] = json!(false);
    assert!(matches!(
        decode(&unpaid),
        Err(StripeContractError::UnpaidInvoice)
    ));

    let mut multiple = fixture();
    multiple["data"]["object"]["lines"]["data"]
        .as_array_mut()
        .unwrap()
        .push(json!({
            "id": "si_second",
            "quantity": 1,
            "price": {"id": "price_contract_month", "recurring": {"interval": "month"}},
            "period": {"start": 1700000000, "end": 1702592000}
        }));
    assert!(matches!(
        decode(&multiple),
        Err(StripeContractError::UnsupportedQuantity)
    ));

    let mut wrong_price = fixture();
    wrong_price["data"]["object"]["lines"]["data"][0]["price"]["id"] = json!("price_other");
    assert!(matches!(
        decode(&wrong_price),
        Err(StripeContractError::UnsupportedPrice)
    ));

    let mut missing_allocation = fixture();
    missing_allocation["data"]["object"]["metadata"] = json!({});
    assert!(matches!(
        decode(&missing_allocation),
        Err(StripeContractError::MissingField(
            "metadata.sotto_allocation_reference"
        ))
    ));
}

#[test]
fn malformed_and_invalid_configuration_fail_closed() {
    let mut malformed = fixture();
    malformed["data"]["object"]["currency"] = json!("usd");
    assert!(matches!(
        decode(&malformed),
        Err(StripeContractError::InvalidField("currency"))
    ));

    assert!(matches!(
        StripeCoverageConfig::new("", ProviderEnvironment::Test, "price_month", "price_year"),
        Err(StripeContractError::InvalidConfig("Stripe account"))
    ));
    assert!(matches!(
        StripeCoverageConfig::new(
            "acct_test",
            ProviderEnvironment::Test,
            "price_same",
            "price_same"
        ),
        Err(StripeContractError::InvalidConfig(
            "monthly and annual Stripe prices must differ"
        ))
    ));

    let (raw, signature) = signed_payload(&json!({"not": "an event"}), NOW);
    assert!(matches!(
        decode_paid_invoice(&raw, &signature, SECRET, NOW, &config()),
        Err(StripeContractError::MalformedPayload)
    ));
}

#[test]
fn provider_context_is_stripe_and_operator_selected() {
    let context = config().provider_context().unwrap();
    assert_eq!(context.namespace, "stripe");
    assert_eq!(context.account_id, "acct_test_sotto_contract");
    assert_eq!(context.environment, ProviderEnvironment::Test);
}
