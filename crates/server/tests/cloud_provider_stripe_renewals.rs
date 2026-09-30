//! Loopback-backed tests for signed personal renewal failure evidence.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;
use sotto_server::cloud_provider::{PayerKind, ProviderEnvironment};
use sotto_server::cloud_provider_stripe::{
    StripeAllocationBinding, StripeContractError, StripeCoverageConfig,
};
use sotto_server::cloud_provider_stripe_http::{
    StripePersonalInvoiceHistory, StripePersonalInvoiceHistoryResult, StripeReadClient,
    StripeReadLimits,
};
use sotto_server::cloud_provider_stripe_renewals::{
    decode_personal_renewal_failure, StripeRenewalFailureNeedsEvidence, StripeRenewalFailureResult,
};
use tokio::net::TcpListener;
use url::Url;

const SECRET: &str = "whsec_renewal_test";
const NOW: i64 = 3_100;

#[derive(Clone)]
struct MockState {
    responses: Arc<Mutex<HashMap<String, Vec<Value>>>>,
}

struct MockServer {
    origin: Url,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn handler(State(state): State<MockState>, request: Request<Body>) -> Response<Body> {
    let path = request.uri().path().to_owned();
    let value = state
        .responses
        .lock()
        .unwrap()
        .get_mut(&path)
        .and_then(|responses| (!responses.is_empty()).then(|| responses.remove(0)))
        .unwrap_or_else(|| json!({"error":"unexpected request"}));
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

async fn mock_server(responses: HashMap<String, Vec<Value>>) -> MockServer {
    let state = MockState {
        responses: Arc::new(Mutex::new(responses)),
    };
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new().fallback(any(handler)).with_state(state);
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    MockServer {
        origin: Url::parse(&format!("http://{address}/")).unwrap(),
        task,
    }
}

fn config() -> StripeCoverageConfig {
    StripeCoverageConfig::new(
        "acct_test_renewal",
        ProviderEnvironment::Test,
        "price_month",
        "price_year",
    )
    .unwrap()
}

fn binding() -> StripeAllocationBinding {
    StripeAllocationBinding::new(
        "alloc_renewal",
        "cus_renewal",
        "sub_renewal",
        "si_renewal",
        PayerKind::Personal,
    )
    .unwrap()
}

fn limits() -> StripeReadLimits {
    StripeReadLimits {
        request_timeout: std::time::Duration::from_secs(2),
        session_timeout: std::time::Duration::from_secs(5),
        max_response_bytes: 64 * 1024,
        max_total_response_bytes: 256 * 1024,
        max_pages: 8,
        max_requests: 32,
        max_records: 100,
        max_retries: 0,
        max_retry_after: std::time::Duration::from_millis(10),
    }
}

fn list(data: Vec<Value>, has_more: bool) -> Value {
    json!({"object":"list","data":data,"has_more":has_more})
}

fn paid_invoice() -> Value {
    json!({
        "id":"in_paid",
        "customer":"cus_renewal",
        "parent":{"type":"subscription_details","subscription_details":{"subscription":"sub_renewal"}},
        "status":"paid","currency":"gbp","amount_paid":299,"amount_due":299,
        "amount_overpaid":0,"amount_paid_off_stripe":0,"livemode":false,
        "metadata":{"sotto_allocation_reference":"alloc_renewal"}
    })
}

fn paid_line() -> Value {
    json!({
        "id":"il_paid","quantity":1,"livemode":false,
        "parent":{"type":"subscription_item_details","subscription_item_details":{
            "subscription":"sub_renewal","subscription_item":"si_renewal"
        }},
        "pricing":{"type":"price_details","price_details":{"price":"price_month"}},
        "period":{"start":1000,"end":2000}
    })
}

fn signed_failure() -> Value {
    json!({
        "id":"evt_failure_1","created":3000,"api_version":"2026-07-29.dahlia",
        "type":"invoice.payment_failed","livemode":false,
        "data":{"object":{
            "object":"invoice","id":"in_failed","customer":"cus_renewal",
            "parent":{"type":"subscription_details","subscription_details":{"subscription":"sub_renewal"}},
            "status":"open","billing_reason":"subscription_cycle",
            "collection_method":"charge_automatically","currency":"gbp",
            "amount_due":299,"amount_remaining":299,"amount_paid":0,
            "amount_overpaid":0,"amount_paid_off_stripe":0,"livemode":false,
            "metadata":{"sotto_allocation_reference":"alloc_renewal"},
            "lines":{"object":"list","has_more":false,"data":[{
                "id":"il_failed","invoice":"in_failed","livemode":false,"quantity":1,
                "parent":{"type":"subscription_item_details","subscription_item_details":{
                    "subscription":"sub_renewal","subscription_item":"si_renewal","proration":false
                }},
                "pricing":{"type":"price_details","price_details":{"price":"price_month"}},
                "period":{"start":2000,"end":3000}
            }]}
        }}
    })
}

fn signed(value: &Value, timestamp: i64) -> (Vec<u8>, String) {
    let payload = serde_json::to_vec(value).unwrap();
    let mut mac = Hmac::<Sha256>::new_from_slice(SECRET.as_bytes()).unwrap();
    mac.update(format!("{timestamp}.{}", String::from_utf8_lossy(&payload)).as_bytes());
    let digest = mac.finalize().into_bytes();
    let signature = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    (payload, format!("t={timestamp},v1={signature}"))
}

async fn history() -> StripePersonalInvoiceHistory {
    let mut responses = HashMap::new();
    responses.insert(
        "/v1/account".into(),
        vec![json!({"id":"acct_test_renewal","object":"account","livemode":false})],
    );
    responses.insert(
        "/v1/subscriptions/sub_renewal".into(),
        vec![
            json!({"id":"sub_renewal","customer":"cus_renewal","status":"active","livemode":false}),
        ],
    );
    responses.insert(
        "/v1/invoices".into(),
        vec![list(vec![paid_invoice()], false)],
    );
    responses.insert("/v1/invoices/in_paid".into(), vec![paid_invoice()]);
    responses.insert(
        "/v1/invoices/in_paid/lines".into(),
        vec![list(vec![paid_line()], false)],
    );
    responses.insert(
        "/v1/invoice_payments".into(),
        vec![list(
            vec![json!({
                "id":"inpay_paid","invoice":"in_paid","status":"paid","amount_paid":299,
                "amount_requested":299,"currency":"gbp","livemode":false,
                "payment":{"type":"payment_intent","payment_intent":"pi_paid"}
            })],
            false,
        )],
    );
    for path in ["/v1/refunds", "/v1/disputes", "/v1/credit_notes"] {
        responses.insert(path.into(), vec![list(Vec::new(), false)]);
    }
    let server = mock_server(responses).await;
    let client = StripeReadClient::for_test(
        "sk_test_renewal",
        &config(),
        server.origin.clone(),
        limits(),
    )
    .unwrap();
    let mut session = client.session();
    let result = client
        .personal_invoice_history(&mut session, &binding())
        .await
        .unwrap();
    let StripePersonalInvoiceHistoryResult::Observed(history) = result else {
        panic!("expected observed history");
    };
    history
}

#[tokio::test]
async fn links_failure_to_exact_paid_predecessor_and_keeps_event_identity_separate() {
    let history = history().await;
    let (raw, signature) = signed(&signed_failure(), NOW);
    let result = decode_personal_renewal_failure(
        &raw,
        &signature,
        SECRET,
        NOW,
        &config(),
        &binding(),
        &history,
    )
    .unwrap();
    let StripeRenewalFailureResult::Linked(evidence) = result else {
        panic!("expected linked failure");
    };
    assert_eq!(evidence.predecessor_invoice_id(), "in_paid");
    assert_eq!(evidence.predecessor_period_end(), 2000);
    assert_eq!(evidence.renewal_period_start(), 2000);
    assert_eq!(evidence.renewal_period_end(), 3000);
    assert_eq!(evidence.invoice_id(), "in_failed");
    assert_eq!(evidence.event_id(), "evt_failure_1");
    assert_ne!(evidence.renewal_id(), evidence.event_id());
}

#[tokio::test]
async fn bad_signature_is_reported_before_malformed_json() {
    let history = history().await;
    let result = decode_personal_renewal_failure(
        b"not json",
        "t=3100,v1=00",
        SECRET,
        NOW,
        &config(),
        &binding(),
        &history,
    );
    assert!(matches!(result, Err(StripeContractError::InvalidSignature)));

    let (raw, signature) = signed(&signed_failure(), NOW);
    assert!(matches!(
        decode_personal_renewal_failure(
            &raw,
            &signature,
            "   ",
            NOW,
            &config(),
            &binding(),
            &history,
        ),
        Err(StripeContractError::InvalidConfig("Stripe webhook secret"))
    ));
}

#[tokio::test]
async fn retries_keep_one_renewal_identity_but_preserve_event_ids() {
    let history = history().await;
    let first_payload = signed_failure();
    let (first_raw, first_signature) = signed(&first_payload, NOW);
    let first = decode_personal_renewal_failure(
        &first_raw,
        &first_signature,
        SECRET,
        NOW,
        &config(),
        &binding(),
        &history,
    )
    .unwrap();
    let StripeRenewalFailureResult::Linked(first) = first else {
        panic!("expected linked first attempt");
    };

    let mut retry_payload = first_payload;
    retry_payload["id"] = json!("evt_failure_retry");
    let (retry_raw, retry_signature) = signed(&retry_payload, NOW);
    let retry = decode_personal_renewal_failure(
        &retry_raw,
        &retry_signature,
        SECRET,
        NOW,
        &config(),
        &binding(),
        &history,
    )
    .unwrap();
    let StripeRenewalFailureResult::Linked(retry) = retry else {
        panic!("expected linked retry");
    };
    assert_eq!(first.renewal_id(), retry.renewal_id());
    assert_ne!(first.event_id(), retry.event_id());
    assert_eq!(first.renewal_period_start(), retry.renewal_period_start());
}

#[tokio::test]
async fn incomplete_or_prorated_lines_never_link() {
    let history = history().await;

    let mut zero_remaining = signed_failure();
    zero_remaining["data"]["object"]["amount_remaining"] = json!(0);
    let (raw, signature) = signed(&zero_remaining, NOW);
    assert!(matches!(
        decode_personal_renewal_failure(
            &raw,
            &signature,
            SECRET,
            NOW,
            &config(),
            &binding(),
            &history,
        ),
        Err(StripeContractError::InvalidField("amount_remaining"))
    ));

    let mut truncated = signed_failure();
    truncated["data"]["object"]["lines"]["has_more"] = json!(true);
    let (raw, signature) = signed(&truncated, NOW);
    assert_eq!(
        decode_personal_renewal_failure(
            &raw,
            &signature,
            SECRET,
            NOW,
            &config(),
            &binding(),
            &history,
        )
        .unwrap(),
        StripeRenewalFailureResult::NeedsEvidence(
            StripeRenewalFailureNeedsEvidence::TruncatedInvoiceLines
        )
    );

    let mut missing_proration = signed_failure();
    let details = &mut missing_proration["data"]["object"]["lines"]["data"][0]["parent"]
        ["subscription_item_details"];
    details.as_object_mut().unwrap().remove("proration");
    let (raw, signature) = signed(&missing_proration, NOW);
    assert_eq!(
        decode_personal_renewal_failure(
            &raw,
            &signature,
            SECRET,
            NOW,
            &config(),
            &binding(),
            &history,
        )
        .unwrap(),
        StripeRenewalFailureResult::NeedsEvidence(
            StripeRenewalFailureNeedsEvidence::MissingProrationProof
        )
    );

    let mut missing_invoice_reference = signed_failure();
    missing_invoice_reference["data"]["object"]["lines"]["data"][0]
        .as_object_mut()
        .unwrap()
        .remove("invoice");
    let (raw, signature) = signed(&missing_invoice_reference, NOW);
    assert!(matches!(
        decode_personal_renewal_failure(
            &raw,
            &signature,
            SECRET,
            NOW,
            &config(),
            &binding(),
            &history,
        ),
        Err(StripeContractError::MissingField("invoice"))
    ));
}

#[tokio::test]
async fn missing_exact_predecessor_does_not_create_partial_evidence() {
    let history = history().await;
    let mut failure = signed_failure();
    failure["data"]["object"]["lines"]["data"][0]["period"]["start"] = json!(2500);
    let (raw, signature) = signed(&failure, NOW);
    let result = decode_personal_renewal_failure(
        &raw,
        &signature,
        SECRET,
        NOW,
        &config(),
        &binding(),
        &history,
    )
    .unwrap();
    assert_eq!(
        result,
        StripeRenewalFailureResult::NeedsEvidence(
            StripeRenewalFailureNeedsEvidence::NoMatchingPaidPredecessor
        )
    );
}
