//! Black-box tests for the verified Stripe evidence boundary.
//!
//! The included invoice is a synthetic, sanitised fixture; it contains no Stripe sandbox data.

use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;
use sotto_server::cloud_provider::{PayerKind, ProviderEnvironment};
use sotto_server::cloud_provider_stripe::{
    decode_invoice_payment, decode_paid_invoice, StripeAccountProvenance, StripeAllocationBinding,
    StripeContractError, StripeCoverageConfig, StripeInterval, StripePaymentSettlement,
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

fn binding() -> StripeAllocationBinding {
    StripeAllocationBinding::new(
        "allocation_contract_person",
        "cus_contract_person",
        "sub_contract_person",
        "si_contract_person",
        PayerKind::Personal,
    )
    .unwrap()
}

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/stripe/invoice_paid_personal.json")).unwrap()
}

fn payment_fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/stripe/invoice_payment_paid.json")).unwrap()
}

fn settlement() -> StripePaymentSettlement {
    let payment = serde_json::to_vec(&payment_fixture()).unwrap();
    decode_invoice_payment(&payment, &config()).unwrap()
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
    decode_with(value, &settlement())
}

fn decode_with(
    value: &Value,
    settlement: &StripePaymentSettlement,
) -> Result<sotto_server::cloud_provider_stripe::StripeCoverageEvidence, StripeContractError> {
    let (payload, signature) = signed_payload(value, NOW);
    decode_paid_invoice(
        &payload,
        &signature,
        SECRET,
        NOW,
        &config(),
        &binding(),
        settlement,
    )
}

#[test]
fn paid_personal_invoice_accepts_expanded_references_and_normalises_evidence() {
    let evidence = decode(&fixture()).unwrap();
    assert_eq!(
        evidence.account_provenance,
        StripeAccountProvenance::OperatorAccount
    );
    assert_eq!(evidence.invoice_id, "in_contract_invoice");
    assert_eq!(evidence.customer_id, "cus_contract_person");
    assert_eq!(evidence.subscription_id, "sub_contract_person");
    assert_eq!(evidence.provider_item_id, "si_contract_person");
    assert_eq!(evidence.price_id, "price_contract_month");
    assert_eq!(evidence.allocation_reference, "allocation_contract_person");
    assert_eq!(evidence.currency, "gbp");
    assert_eq!(evidence.amount_paid, 299);
    assert_eq!(evidence.payment_intent_id, "pi_contract_payment");
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
        "stripe:invoice:in_contract_invoice:line:il_contract_line"
    );
}

#[test]
fn annual_standard_price_maps_to_a_year_interval() {
    let mut annual = fixture();
    annual["data"]["object"]["lines"]["data"][0]["pricing"]["price_details"]["price"] =
        json!("price_contract_year");
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
    changed["data"]["object"]["amount_paid"] = json!(598);
    changed["data"]["object"]["amount_due"] = json!(598);
    changed["data"]["object"]["amount_overpaid"] = json!(0);
    changed["data"]["object"]["lines"]["data"][0]["period"]["end"] = json!(1705184000);
    let mut changed_payment = payment_fixture();
    changed_payment["amount_paid"] = json!(598);
    changed_payment["amount_requested"] = json!(598);
    let changed_payment = serde_json::to_vec(&changed_payment).unwrap();
    let changed_settlement = decode_invoice_payment(&changed_payment, &config()).unwrap();
    let first = decode(&original).unwrap();
    let second = decode_with(&changed, &changed_settlement).unwrap();
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
        decode_paid_invoice(
            raw.as_slice(),
            "t=1700000000,v1=00",
            SECRET,
            NOW,
            &config(),
            &binding(),
            &settlement(),
        ),
        Err(StripeContractError::InvalidSignature)
    ));
    assert!(matches!(
        decode_paid_invoice(
            &raw,
            &signature,
            SECRET,
            NOW + 301,
            &config(),
            &binding(),
            &settlement(),
        ),
        Err(StripeContractError::InvalidSignature)
    ));
}

#[test]
fn unsupported_versions_types_and_contexts_fail_closed() {
    let mut missing_version = fixture();
    missing_version["api_version"] = Value::Null;
    assert!(matches!(
        decode(&missing_version),
        Err(StripeContractError::UnsupportedApiVersion(version)) if version == "missing"
    ));

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

    let mut connect_account = fixture();
    connect_account["account"] = json!("acct_test_sotto_contract");
    assert!(matches!(
        decode(&connect_account),
        Err(StripeContractError::ContextMismatch)
    ));

    let mut wrong_mode = fixture();
    wrong_mode["livemode"] = json!(true);
    assert!(matches!(
        decode(&wrong_mode),
        Err(StripeContractError::ContextMismatch)
    ));

    let mut unsupported_connect_context = fixture();
    unsupported_connect_context["context"] = json!("acct_connected");
    assert!(matches!(
        decode(&unsupported_connect_context),
        Err(StripeContractError::ContextMismatch)
    ));

    let mut own_account = fixture();
    own_account["account"] = Value::Null;
    let own_evidence = decode(&own_account).unwrap();
    assert_eq!(
        own_evidence.account_provenance,
        StripeAccountProvenance::OperatorAccount
    );
}

#[test]
fn unpaid_quantity_price_and_missing_allocation_are_not_coverage() {
    let mut unpaid = fixture();
    unpaid["data"]["object"]["status"] = json!("open");
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
    wrong_price["data"]["object"]["lines"]["data"][0]["pricing"]["price_details"]["price"] =
        json!("price_other");
    assert!(matches!(
        decode(&wrong_price),
        Err(StripeContractError::UnsupportedPrice)
    ));

    let mut missing_subscription_item = fixture();
    missing_subscription_item["data"]["object"]["lines"]["data"][0]["parent"]
        ["subscription_item_details"]["subscription_item"] = Value::Null;
    assert!(matches!(
        decode(&missing_subscription_item),
        Err(StripeContractError::InvalidField("subscription_item"))
    ));

    let mut missing_allocation = fixture();
    missing_allocation["data"]["object"]["metadata"] = json!({});
    assert!(matches!(
        decode(&missing_allocation),
        Err(StripeContractError::MissingField(
            "metadata.sotto_allocation_reference"
        ))
    ));

    let mut null_customer = fixture();
    null_customer["data"]["object"]["customer"] = Value::Null;
    assert!(matches!(
        decode(&null_customer),
        Err(StripeContractError::InvalidField("customer"))
    ));

    let mut missing_payment = payment_fixture();
    missing_payment["payment"] = Value::Null;
    let missing_payment = serde_json::to_vec(&missing_payment).unwrap();
    assert!(matches!(
        decode_invoice_payment(&missing_payment, &config()),
        Err(StripeContractError::MissingField("payment"))
    ));

    let mut unsupported_payment = payment_fixture();
    unsupported_payment["payment"]["type"] = json!("out_of_band");
    let unsupported_payment = serde_json::to_vec(&unsupported_payment).unwrap();
    assert!(matches!(
        decode_invoice_payment(&unsupported_payment, &config()),
        Err(StripeContractError::UnsupportedSettlement(_))
    ));

    let mut failed_payment = payment_fixture();
    failed_payment["status"] = json!("open");
    let failed_payment = serde_json::to_vec(&failed_payment).unwrap();
    assert!(matches!(
        decode_invoice_payment(&failed_payment, &config()),
        Err(StripeContractError::UnsupportedSettlement(_))
    ));

    let mut partial_payment = payment_fixture();
    partial_payment["amount_paid"] = json!(100);
    let partial_payment = serde_json::to_vec(&partial_payment).unwrap();
    assert!(matches!(
        decode_invoice_payment(&partial_payment, &config()),
        Err(StripeContractError::UnsupportedSettlement(_))
    ));

    let mut zero_payment = payment_fixture();
    zero_payment["amount_paid"] = json!(0);
    zero_payment["amount_requested"] = json!(0);
    let zero_payment = serde_json::to_vec(&zero_payment).unwrap();
    assert!(matches!(
        decode_invoice_payment(&zero_payment, &config()),
        Err(StripeContractError::UnsupportedSettlement(_))
    ));

    let mut forged_metadata = fixture();
    forged_metadata["data"]["object"]["metadata"]["sotto_allocation_reference"] =
        json!("allocation_forged");
    assert!(matches!(
        decode(&forged_metadata),
        Err(StripeContractError::OwnershipMismatch)
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
    assert!(matches!(
        StripeAllocationBinding::new(
            "allocation_sponsor",
            "cus_sponsor",
            "sub_sponsor",
            "si_sponsor",
            PayerKind::Sponsor,
        ),
        Err(StripeContractError::UnsupportedPayerKind)
    ));

    let (raw, signature) = signed_payload(&json!({"not": "an event"}), NOW);
    assert!(matches!(
        decode_paid_invoice(
            &raw,
            &signature,
            SECRET,
            NOW,
            &config(),
            &binding(),
            &settlement(),
        ),
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
