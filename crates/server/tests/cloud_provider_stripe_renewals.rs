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
use sotto_server::cloud_provider_stripe_coverage::{
    compose_personal_coverage, StripeCoverageCompositionResult, StripeCoverageRenewalState,
};
use sotto_server::cloud_provider_stripe_http::{
    StripePersonalInvoiceHistory, StripePersonalInvoiceHistoryEntry,
    StripePersonalInvoiceHistoryResult, StripeReadClient, StripeReadError, StripeReadLimits,
    StripeRenewalCurrentState, StripeRenewalNeedsEvidence, StripeRenewalObservationResult,
};
use sotto_server::cloud_provider_stripe_renewals::{
    decode_personal_renewal_failure, StripeRenewalFailureNeedsEvidence, StripeRenewalFailureResult,
};
use tokio::net::TcpListener;
use tokio::sync::Notify;
use url::Url;

const SECRET: &str = "whsec_renewal_test";
const NOW: i64 = 3_100;

#[derive(Clone)]
struct MockState {
    responses: Arc<Mutex<HashMap<String, Vec<Value>>>>,
    requests: Arc<Mutex<Vec<String>>>,
    pending_path: Arc<Mutex<Option<String>>>,
    entered: Arc<Notify>,
}

struct MockServer {
    origin: Url,
    task: tokio::task::JoinHandle<()>,
    responses: Arc<Mutex<HashMap<String, Vec<Value>>>>,
    requests: Arc<Mutex<Vec<String>>>,
    entered: Arc<Notify>,
}

impl MockServer {
    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    fn replace_responses(&self, path: &str, responses: Vec<Value>) {
        self.responses
            .lock()
            .unwrap()
            .insert(path.to_owned(), responses);
    }

    async fn wait_for_pending_request(&self) {
        self.entered.notified().await;
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn handler(State(state): State<MockState>, request: Request<Body>) -> Response<Body> {
    let path = request.uri().path().to_owned();
    state
        .requests
        .lock()
        .unwrap()
        .push(format!("{} {}", request.method(), request.uri()));
    let pending = state
        .pending_path
        .lock()
        .unwrap()
        .as_deref()
        .is_some_and(|pending| pending == path);
    if pending {
        state.entered.notify_one();
        std::future::pending::<()>().await;
    }
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
    mock_server_with_pending(responses, None).await
}

async fn mock_server_with_pending(
    responses: HashMap<String, Vec<Value>>,
    pending_path: Option<&str>,
) -> MockServer {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let entered = Arc::new(Notify::new());
    let response_store = Arc::new(Mutex::new(responses));
    let state = MockState {
        responses: Arc::clone(&response_store),
        requests: Arc::clone(&requests),
        pending_path: Arc::new(Mutex::new(pending_path.map(str::to_owned))),
        entered: Arc::clone(&entered),
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
        responses: response_store,
        requests,
        entered,
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
    paid_line_with("price_month", 1000, 2000)
}

fn paid_line_with(price: &str, period_start: i64, period_end: i64) -> Value {
    json!({
        "id":"il_paid","quantity":1,"livemode":false,
        "parent":{"type":"subscription_item_details","subscription_item_details":{
            "subscription":"sub_renewal","subscription_item":"si_renewal","proration":false
        }},
        "pricing":{"type":"price_details","price_details":{"price":price}},
        "period":{"start":period_start,"end":period_end}
    })
}

fn generic_open_invoice() -> Value {
    json!({
        "id":"in_open_generic",
        "customer":"cus_renewal",
        "parent":{"type":"subscription_details","subscription_details":{"subscription":"sub_renewal"}},
        "status":"open","billing_reason":"manual","collection_method":"send_invoice",
        "currency":"gbp","amount_paid":0,"amount_due":299,"amount_remaining":299,
        "amount_overpaid":0,"amount_paid_off_stripe":0,"livemode":false,
        "metadata":{"sotto_allocation_reference":"alloc_renewal"}
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
    history_with_line(paid_line()).await
}

async fn history_with_line(line: Value) -> StripePersonalInvoiceHistory {
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
        vec![list(vec![paid_invoice(), generic_open_invoice()], false)],
    );
    responses.insert("/v1/invoices/in_paid".into(), vec![paid_invoice()]);
    responses.insert(
        "/v1/invoices/in_failed".into(),
        vec![current_invoice("open")],
    );
    responses.insert(
        "/v1/invoices/in_failed/lines".into(),
        vec![list(vec![current_line()], false)],
    );
    responses.insert(
        "/v1/invoices/in_paid/lines".into(),
        vec![list(vec![line], false)],
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

async fn history_with_current_invoice(line: Value) -> StripePersonalInvoiceHistory {
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
        vec![list(
            vec![
                paid_invoice(),
                current_invoice("open"),
                generic_open_invoice(),
            ],
            false,
        )],
    );
    responses.insert("/v1/invoices/in_paid".into(), vec![paid_invoice()]);
    responses.insert(
        "/v1/invoices/in_failed".into(),
        vec![current_invoice("open")],
    );
    responses.insert(
        "/v1/invoices/in_failed/lines".into(),
        vec![list(vec![current_line()], false)],
    );
    responses.insert(
        "/v1/invoices/in_paid/lines".into(),
        vec![list(vec![line], false)],
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
async fn generic_history_keeps_non_cycle_invoices_without_current_billing_fields() {
    let history = history().await;
    assert!(history.entries().iter().any(|entry| matches!(
        entry,
        StripePersonalInvoiceHistoryEntry::NonPaid(invoice)
            if invoice.invoice_id() == "in_open_generic" && invoice.status() == "open"
    )));
    assert!(history.entries().iter().any(|entry| matches!(
        entry,
        StripePersonalInvoiceHistoryEntry::Paid(term)
            if term.invoice_id() == "in_paid"
    )));
}

#[tokio::test]
async fn loopback_history_signed_failure_current_open_composes_personal_candidate() {
    let history = history_with_current_invoice(paid_line()).await;
    let (raw, signature) = signed(&signed_failure(), NOW);
    let StripeRenewalFailureResult::Linked(failure) = decode_personal_renewal_failure(
        &raw,
        &signature,
        SECRET,
        NOW,
        &config(),
        &binding(),
        &history,
    )
    .unwrap() else {
        panic!("expected linked failure");
    };
    let (client, mut session, _server) = current_client("open", None).await;
    let StripeRenewalObservationResult::Observed(observation) = client
        .personal_renewal_observation(&mut session, &binding(), &failure)
        .await
        .unwrap()
    else {
        panic!("expected current observation");
    };
    let result =
        compose_personal_coverage(&config(), &binding(), &history, &[(*failure, *observation)])
            .unwrap();
    let StripeCoverageCompositionResult::Candidate(candidate) = result else {
        panic!("expected coverage candidate");
    };
    assert_eq!(candidate.paid_terms().len(), 1);
    assert_eq!(candidate.paid_terms()[0].invoice_id(), "in_paid");
    assert!(candidate
        .non_paid_invoices()
        .iter()
        .any(|invoice| invoice.invoice_id() == "in_open_generic"));
    assert_eq!(candidate.renewals().len(), 1);
    let renewal = &candidate.renewals()[0];
    assert!(matches!(renewal.state(), StripeCoverageRenewalState::Open));
    assert_eq!(renewal.predecessor_invoice_id(), "in_paid");
    assert_eq!(
        renewal.predecessor_evidence_reference(),
        "stripe:invoice:in_paid:line:il_paid"
    );
    assert_eq!(renewal.interval().to_string(), "month");
    assert!(candidate
        .semantic_reference()
        .starts_with("stripe-personal-coverage-v1:"));
}

#[tokio::test]
async fn verified_retry_events_merge_without_changing_candidate_identity() {
    let history = history_with_current_invoice(paid_line()).await;
    let first_payload = signed_failure();
    let (first_raw, first_signature) = signed(&first_payload, NOW);
    let StripeRenewalFailureResult::Linked(first_failure) = decode_personal_renewal_failure(
        &first_raw,
        &first_signature,
        SECRET,
        NOW,
        &config(),
        &binding(),
        &history,
    )
    .unwrap() else {
        panic!("expected first linked failure");
    };
    let mut retry_payload = first_payload;
    retry_payload["id"] = json!("evt_failure_retry");
    let (retry_raw, retry_signature) = signed(&retry_payload, NOW);
    let StripeRenewalFailureResult::Linked(retry_failure) = decode_personal_renewal_failure(
        &retry_raw,
        &retry_signature,
        SECRET,
        NOW,
        &config(),
        &binding(),
        &history,
    )
    .unwrap() else {
        panic!("expected retry linked failure");
    };
    let (first_client, mut first_session, _first_server) = current_client("open", None).await;
    let (retry_client, mut retry_session, _retry_server) = current_client("open", None).await;
    let StripeRenewalObservationResult::Observed(first_observation) = first_client
        .personal_renewal_observation(&mut first_session, &binding(), &first_failure)
        .await
        .unwrap()
    else {
        panic!("expected first observation");
    };
    let StripeRenewalObservationResult::Observed(retry_observation) = retry_client
        .personal_renewal_observation(&mut retry_session, &binding(), &retry_failure)
        .await
        .unwrap()
    else {
        panic!("expected retry observation");
    };
    let merged = compose_personal_coverage(
        &config(),
        &binding(),
        &history,
        &[
            ((*first_failure).clone(), (*first_observation).clone()),
            ((*retry_failure).clone(), (*retry_observation).clone()),
        ],
    )
    .unwrap();
    let single = compose_personal_coverage(
        &config(),
        &binding(),
        &history,
        &[((*first_failure).clone(), (*first_observation).clone())],
    )
    .unwrap();
    let (
        StripeCoverageCompositionResult::Candidate(merged),
        StripeCoverageCompositionResult::Candidate(single),
    ) = (merged, single)
    else {
        panic!("expected candidates");
    };
    assert_eq!(merged.semantic_reference(), single.semantic_reference());
    assert_eq!(
        merged.renewals()[0].event_ids(),
        &["evt_failure_1", "evt_failure_retry"]
    );
    assert_eq!(merged.renewals()[0].predecessor_invoice_id(), "in_paid");
}

#[tokio::test]
async fn verified_paid_transition_replaces_the_historical_non_paid_invoice() {
    let history = history_with_current_invoice(paid_line()).await;
    let (raw, signature) = signed(&signed_failure(), NOW);
    let StripeRenewalFailureResult::Linked(failure) = decode_personal_renewal_failure(
        &raw,
        &signature,
        SECRET,
        NOW,
        &config(),
        &binding(),
        &history,
    )
    .unwrap() else {
        panic!("expected linked failure");
    };
    let (client, mut session, _server) = current_client("paid", None).await;
    let StripeRenewalObservationResult::Observed(observation) = client
        .personal_renewal_observation(&mut session, &binding(), &failure)
        .await
        .unwrap()
    else {
        panic!("expected paid observation");
    };
    let result =
        compose_personal_coverage(&config(), &binding(), &history, &[(*failure, *observation)])
            .unwrap();
    let StripeCoverageCompositionResult::Candidate(candidate) = result else {
        panic!("expected candidate");
    };
    assert_eq!(candidate.paid_terms().len(), 2);
    assert!(!candidate
        .non_paid_invoices()
        .iter()
        .any(|invoice| invoice.invoice_id() == "in_failed"));
    assert!(matches!(
        candidate.renewals()[0].state(),
        StripeCoverageRenewalState::Paid
    ));
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
async fn annual_history_and_signed_failure_keep_annual_price_provenance() {
    let history = history_with_line(paid_line_with("price_year", 1000, 2000)).await;
    let mut failure = signed_failure();
    failure["data"]["object"]["lines"]["data"][0]["pricing"]["price_details"]["price"] =
        json!("price_year");
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
    let StripeRenewalFailureResult::Linked(evidence) = result else {
        panic!("expected annual linked failure");
    };
    assert_eq!(
        evidence.interval(),
        sotto_server::cloud_provider_stripe::StripeInterval::Year
    );
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

async fn linked_failure(
) -> sotto_server::cloud_provider_stripe_renewals::StripeRenewalFailureEvidence {
    let history = history().await;
    let (raw, signature) = signed(&signed_failure(), NOW);
    let StripeRenewalFailureResult::Linked(evidence) = decode_personal_renewal_failure(
        &raw,
        &signature,
        SECRET,
        NOW,
        &config(),
        &binding(),
        &history,
    )
    .unwrap() else {
        panic!("expected linked failure");
    };
    *evidence
}

fn current_invoice(status: &str) -> Value {
    json!({
        "id":"in_failed", "customer":"cus_renewal",
        "parent":{"type":"subscription_details","subscription_details":{"subscription":"sub_renewal"}},
        "status":status,"billing_reason":"subscription_cycle",
        "collection_method":"charge_automatically","currency":"gbp",
        "amount_paid": if status == "paid" { 299 } else { 0 },"amount_due":299,
        "amount_remaining": if status == "open" { 299 } else { 0 },
        "amount_overpaid":0,"amount_paid_off_stripe":0,"livemode":false,
        "metadata":{"sotto_allocation_reference":"alloc_renewal"}
    })
}

fn current_line() -> Value {
    json!({
        "id":"il_failed","invoice":"in_failed","quantity":1,"livemode":false,
        "parent":{"type":"subscription_item_details","subscription_item_details":{
            "subscription":"sub_renewal","subscription_item":"si_renewal","proration":false
        }},
        "pricing":{"type":"price_details","price_details":{"price":"price_month"}},
        "period":{"start":2000,"end":3000}
    })
}

fn current_subscription(cancel_at_period_end: bool) -> Value {
    json!({"id":"sub_renewal","customer":"cus_renewal","status":"active","livemode":false,
        "cancel_at_period_end":cancel_at_period_end,"cancel_at":null,"canceled_at":null,"ended_at":null})
}

fn refund(status: &str) -> Value {
    json!({
        "object":"refund","id":"re_current","payment_intent":"pi_current",
        "amount":100,"currency":"gbp","created":3200,"status":status,"livemode":false
    })
}

fn dispute(status: &str) -> Value {
    json!({
        "object":"dispute","id":"dp_current","payment_intent":"pi_current",
        "charge":"ch_current","amount":100,"currency":"gbp","created":3200,
        "status":status,"livemode":false
    })
}

async fn current_client(
    status: &str,
    invoice_second: Option<Value>,
) -> (
    StripeReadClient,
    sotto_server::cloud_provider_stripe_http::StripeReadSession,
    MockServer,
) {
    let invoice = current_invoice(status);
    let second = invoice_second.unwrap_or_else(|| invoice.clone());
    current_client_with_parts(
        invoice,
        second,
        current_line(),
        current_line(),
        current_subscription(false),
        current_subscription(false),
    )
    .await
}

async fn current_client_with_parts(
    invoice: Value,
    second_invoice: Value,
    first_line: Value,
    second_line: Value,
    first_subscription: Value,
    second_subscription: Value,
) -> (
    StripeReadClient,
    sotto_server::cloud_provider_stripe_http::StripeReadSession,
    MockServer,
) {
    current_client_with_parts_and_limits(
        invoice,
        second_invoice,
        first_line,
        second_line,
        first_subscription,
        second_subscription,
        limits(),
    )
    .await
}

async fn current_client_with_parts_and_limits(
    invoice: Value,
    second_invoice: Value,
    first_line: Value,
    second_line: Value,
    first_subscription: Value,
    second_subscription: Value,
    read_limits: StripeReadLimits,
) -> (
    StripeReadClient,
    sotto_server::cloud_provider_stripe_http::StripeReadSession,
    MockServer,
) {
    current_client_with_line_lists_and_limits(
        invoice,
        second_invoice,
        vec![first_line],
        vec![second_line],
        first_subscription,
        second_subscription,
        read_limits,
    )
    .await
}

async fn current_client_with_line_lists_and_limits(
    invoice: Value,
    second_invoice: Value,
    first_lines: Vec<Value>,
    second_lines: Vec<Value>,
    first_subscription: Value,
    second_subscription: Value,
    read_limits: StripeReadLimits,
) -> (
    StripeReadClient,
    sotto_server::cloud_provider_stripe_http::StripeReadSession,
    MockServer,
) {
    current_client_with_line_lists_and_limits_pending(
        invoice,
        second_invoice,
        first_lines,
        second_lines,
        first_subscription,
        second_subscription,
        read_limits,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn current_client_with_line_lists_and_limits_pending(
    invoice: Value,
    second_invoice: Value,
    first_lines: Vec<Value>,
    second_lines: Vec<Value>,
    first_subscription: Value,
    second_subscription: Value,
    read_limits: StripeReadLimits,
    pending_path: Option<&str>,
) -> (
    StripeReadClient,
    sotto_server::cloud_provider_stripe_http::StripeReadSession,
    MockServer,
) {
    let mut responses = HashMap::new();
    responses.insert(
        "/v1/account".into(),
        vec![json!({"id":"acct_test_renewal","livemode":false})],
    );
    responses.insert(
        "/v1/subscriptions/sub_renewal".into(),
        vec![first_subscription, second_subscription],
    );
    responses.insert(
        "/v1/invoices/in_failed".into(),
        vec![invoice, second_invoice],
    );
    responses.insert(
        "/v1/invoices/in_failed/lines".into(),
        vec![list(first_lines, false), list(second_lines, false)],
    );
    responses.insert(
        "/v1/invoice_payments".into(),
        vec![list(
            vec![json!({
                "id":"inpay_current","invoice":"in_failed","status":"paid","amount_paid":299,
                "amount_requested":299,"currency":"gbp","livemode":false,
                "payment":{"type":"payment_intent","payment_intent":"pi_current"}
            })],
            false,
        )],
    );
    for path in ["/v1/refunds", "/v1/disputes", "/v1/credit_notes"] {
        responses.insert(path.into(), vec![list(Vec::new(), false)]);
    }
    let server = mock_server_with_pending(responses, pending_path).await;
    let client = StripeReadClient::for_test(
        "sk_test_renewal",
        &config(),
        server.origin.clone(),
        read_limits,
    )
    .unwrap();
    let session = client.session();
    (client, session, server)
}

#[tokio::test]
async fn current_paid_invoice_supersedes_historical_failure_and_preserves_cancellation_facts() {
    let failure = linked_failure().await;
    let (client, mut session, server) = current_client("paid", None).await;
    let result = client
        .personal_renewal_observation(&mut session, &binding(), &failure)
        .await
        .unwrap();
    let StripeRenewalObservationResult::Observed(observation) = result else {
        panic!("expected observation")
    };
    assert_eq!(observation.invoice_id(), "in_failed");
    assert_eq!(observation.event_id(), "evt_failure_1");
    assert_eq!(observation.provider_account_id(), "acct_test_renewal");
    assert_eq!(observation.allocation_reference(), "alloc_renewal");
    assert_eq!(observation.customer_id(), "cus_renewal");
    assert_eq!(observation.subscription_id(), "sub_renewal");
    assert_eq!(observation.provider_item_id(), "si_renewal");
    assert_eq!(observation.renewal_id(), failure.renewal_id());
    assert_eq!(observation.period_start(), 2000);
    assert_eq!(observation.period_end(), 3000);
    assert!(matches!(
        observation.state(),
        StripeRenewalCurrentState::Paid { .. }
    ));
    assert!(!observation.cancellation().cancel_at_period_end());
    assert_eq!(
        server.requests(),
        vec![
            "GET /v1/account",
            "GET /v1/subscriptions/sub_renewal",
            "GET /v1/invoices/in_failed",
            "GET /v1/invoices/in_failed/lines?limit=100",
            "GET /v1/invoice_payments?invoice=in_failed&limit=100",
            "GET /v1/refunds?payment_intent=pi_current&limit=100",
            "GET /v1/disputes?payment_intent=pi_current&limit=100",
            "GET /v1/credit_notes?invoice=in_failed&limit=100",
            "GET /v1/invoices/in_failed",
            "GET /v1/subscriptions/sub_renewal",
        ]
    );
}

#[tokio::test]
async fn subscription_cancellation_facts_round_trip_for_active_scheduled_and_ended_states() {
    let failure = linked_failure().await;
    for (status, cancel_at_period_end, cancel_at, canceled_at, ended_at) in [
        ("active", false, None, None, None),
        ("active", true, Some(3_500_i64), None, None),
        ("canceled", false, None, Some(3_600_i64), Some(3_700_i64)),
    ] {
        let mut subscription = current_subscription(cancel_at_period_end);
        subscription["status"] = json!(status);
        subscription["cancel_at"] = cancel_at.map_or(Value::Null, |value| json!(value));
        subscription["canceled_at"] = canceled_at.map_or(Value::Null, |value| json!(value));
        subscription["ended_at"] = ended_at.map_or(Value::Null, |value| json!(value));
        let second_subscription = subscription.clone();
        let (client, mut session, _server) = current_client_with_parts(
            current_invoice("paid"),
            current_invoice("paid"),
            current_line(),
            current_line(),
            subscription,
            second_subscription,
        )
        .await;
        let result = client
            .personal_renewal_observation(&mut session, &binding(), &failure)
            .await
            .unwrap();
        let StripeRenewalObservationResult::Observed(observation) = result else {
            panic!("expected observation");
        };
        assert_eq!(observation.cancellation().status(), Some(status));
        assert_eq!(
            observation.cancellation().cancel_at_period_end(),
            cancel_at_period_end
        );
        assert_eq!(observation.cancellation().cancel_at(), cancel_at);
        assert_eq!(observation.cancellation().canceled_at(), canceled_at);
        assert_eq!(observation.cancellation().ended_at(), ended_at);
    }
}

#[tokio::test]
async fn paid_observation_reuses_the_validated_line_and_rejects_nonzero_remaining() {
    let failure = linked_failure().await;
    let mut changed_line = current_line();
    changed_line["period"]["start"] = json!(4000);
    changed_line["period"]["end"] = json!(5000);
    let mut nonzero_remaining = current_invoice("paid");
    nonzero_remaining["amount_remaining"] = json!(1);
    let (client, mut session, server) = current_client_with_parts(
        nonzero_remaining,
        current_invoice("paid"),
        current_line(),
        changed_line,
        current_subscription(false),
        current_subscription(false),
    )
    .await;
    let result = client
        .personal_renewal_observation(&mut session, &binding(), &failure)
        .await
        .unwrap();
    assert!(matches!(
        result,
        StripeRenewalObservationResult::NeedsEvidence(
            StripeRenewalNeedsEvidence::UnsupportedSettlement
        )
    ));
    assert_eq!(
        server
            .requests()
            .iter()
            .filter(|request| request.contains("GET /v1/invoices/in_failed/lines"))
            .count(),
        1
    );
}

#[tokio::test]
async fn paid_observation_rejects_missing_null_and_negative_remaining() {
    let failure = linked_failure().await;
    for remaining in [None, Some(json!(null)), Some(json!(-1))] {
        let mut invoice = current_invoice("paid");
        match remaining {
            None => {
                invoice.as_object_mut().unwrap().remove("amount_remaining");
            }
            Some(value) => invoice["amount_remaining"] = value,
        }
        let (client, mut session, _server) = current_client_with_parts(
            invoice,
            current_invoice("paid"),
            current_line(),
            current_line(),
            current_subscription(false),
            current_subscription(false),
        )
        .await;
        let result = client
            .personal_renewal_observation(&mut session, &binding(), &failure)
            .await
            .unwrap();
        assert!(matches!(
            result,
            StripeRenewalObservationResult::NeedsEvidence(
                StripeRenewalNeedsEvidence::UnsupportedSettlement
            )
        ));
    }
}

#[tokio::test]
async fn paid_observation_keeps_the_first_line_interval() {
    let failure = linked_failure().await;
    let mut changed_line = current_line();
    changed_line["period"]["start"] = json!(4000);
    changed_line["period"]["end"] = json!(5000);
    let (client, mut session, server) = current_client_with_parts(
        current_invoice("paid"),
        current_invoice("paid"),
        current_line(),
        changed_line,
        current_subscription(false),
        current_subscription(false),
    )
    .await;
    let result = client
        .personal_renewal_observation(&mut session, &binding(), &failure)
        .await
        .unwrap();
    let StripeRenewalObservationResult::Observed(observation) = result else {
        panic!("expected observation");
    };
    assert_eq!(observation.period_start(), failure.renewal_period_start());
    assert_eq!(observation.period_end(), failure.renewal_period_end());
    assert_eq!(
        server
            .requests()
            .iter()
            .filter(|request| request.contains("GET /v1/invoices/in_failed/lines"))
            .count(),
        1
    );
}

#[tokio::test]
async fn paid_observation_retains_the_validated_term_and_does_not_read_the_second_line() {
    let failure = linked_failure().await;
    let mut alternate = current_line();
    alternate["pricing"]["price_details"]["price"] = json!("price_year");
    alternate["period"]["start"] = json!(9000);
    alternate["period"]["end"] = json!(10000);
    let (client, mut session, server) = current_client_with_parts(
        current_invoice("paid"),
        current_invoice("paid"),
        current_line(),
        alternate,
        current_subscription(false),
        current_subscription(false),
    )
    .await;
    let result = client
        .personal_renewal_observation(&mut session, &binding(), &failure)
        .await
        .unwrap();
    let StripeRenewalObservationResult::Observed(observation) = result else {
        panic!("expected observation");
    };
    let StripeRenewalCurrentState::Paid { term, .. } = observation.state() else {
        panic!("expected paid state");
    };
    assert_eq!(term.invoice_id(), observation.invoice_id());
    assert_eq!(
        term.allocation_reference(),
        observation.allocation_reference()
    );
    assert_eq!(term.customer_id(), observation.customer_id());
    assert_eq!(term.subscription_id(), observation.subscription_id());
    assert_eq!(term.provider_item_id(), observation.provider_item_id());
    assert_eq!(term.period_start(), observation.period_start());
    assert_eq!(term.period_end(), observation.period_end());
    assert_eq!(term.period_start(), failure.renewal_period_start());
    assert_eq!(term.period_end(), failure.renewal_period_end());
    assert_eq!(term.interval(), failure.interval());
    assert_eq!(
        server
            .requests()
            .iter()
            .filter(|request| request.contains("GET /v1/invoices/in_failed/lines"))
            .count(),
        1
    );
}

#[tokio::test]
async fn current_invoice_requires_an_automatic_subscription_cycle() {
    let failure = linked_failure().await;
    for (field, value) in [
        ("billing_reason", json!("manual")),
        ("billing_reason", json!("future_billing_reason")),
        ("collection_method", json!("send_invoice")),
        ("collection_method", json!("future_collection_method")),
    ] {
        let mut invoice = current_invoice("paid");
        invoice[field] = value.clone();
        let (client, mut session, _server) = current_client_with_parts(
            invoice,
            current_invoice("paid"),
            current_line(),
            current_line(),
            current_subscription(false),
            current_subscription(false),
        )
        .await;
        let result = client
            .personal_renewal_observation(&mut session, &binding(), &failure)
            .await
            .unwrap();
        assert!(matches!(
            result,
            StripeRenewalObservationResult::NeedsEvidence(
                StripeRenewalNeedsEvidence::UnsupportedInvoiceField {
                    field: actual,
                    value: ref actual_value,
                }
            ) if actual == field && actual_value == value.as_str().unwrap()
        ));
    }
}

#[tokio::test]
async fn current_invoice_requires_named_billing_fields() {
    let failure = linked_failure().await;
    for field in ["billing_reason", "collection_method"] {
        for (remove, replacement) in [
            (true, None),
            (false, Some(json!(null))),
            (false, Some(json!(42))),
        ] {
            let mut invoice = current_invoice("paid");
            if remove {
                invoice.as_object_mut().unwrap().remove(field);
            } else {
                invoice[field] = replacement.unwrap();
            }
            let (client, mut session, _server) = current_client_with_parts(
                invoice,
                current_invoice("paid"),
                current_line(),
                current_line(),
                current_subscription(false),
                current_subscription(false),
            )
            .await;
            assert!(matches!(
                client
                    .personal_renewal_observation(&mut session, &binding(), &failure)
                    .await,
                Err(StripeReadError::MalformedResponse(actual)) if actual == format!("invoice.{field}")
            ));
        }
    }
}

#[tokio::test]
async fn current_open_invoice_is_unresolved_and_does_not_become_recovery() {
    let failure = linked_failure().await;
    let (client, mut session, _server) = current_client("open", None).await;
    let result = client
        .personal_renewal_observation(&mut session, &binding(), &failure)
        .await
        .unwrap();
    let StripeRenewalObservationResult::Observed(observation) = result else {
        panic!("expected observation")
    };
    assert!(matches!(
        observation.state(),
        StripeRenewalCurrentState::Open
    ));
}

#[tokio::test]
async fn closed_invoice_is_observed_without_shortening_a_paid_term() {
    let failure = linked_failure().await;
    let (client, mut session, _server) = current_client("void", None).await;
    let result = client
        .personal_renewal_observation(&mut session, &binding(), &failure)
        .await
        .unwrap();
    let StripeRenewalObservationResult::Observed(observation) = result else {
        panic!("expected observation");
    };
    assert!(matches!(
        observation.state(),
        StripeRenewalCurrentState::ClosedUnpaid { status } if status == "void"
    ));
}

#[tokio::test]
async fn current_invoice_status_and_settlement_boundaries_are_explicit() {
    let failure = linked_failure().await;
    for status in ["void", "uncollectible"] {
        let mut invoice = current_invoice(status);
        invoice["amount_remaining"] = json!(100);
        let second_invoice = invoice.clone();
        let (client, mut session, _server) = current_client_with_parts(
            invoice,
            second_invoice,
            current_line(),
            current_line(),
            current_subscription(false),
            current_subscription(false),
        )
        .await;
        let result = client
            .personal_renewal_observation(&mut session, &binding(), &failure)
            .await
            .unwrap();
        assert!(matches!(
            result,
            StripeRenewalObservationResult::Observed(observation)
                if matches!(observation.state(), StripeRenewalCurrentState::ClosedUnpaid { status: actual } if actual == status)
        ));
    }

    let mut partial_open = current_invoice("open");
    partial_open["amount_remaining"] = json!(100);
    let (client, mut session, _server) = current_client_with_parts(
        partial_open,
        current_invoice("open"),
        current_line(),
        current_line(),
        current_subscription(false),
        current_subscription(false),
    )
    .await;
    assert!(matches!(
        client
            .personal_renewal_observation(&mut session, &binding(), &failure)
            .await,
        Ok(StripeRenewalObservationResult::NeedsEvidence(
            StripeRenewalNeedsEvidence::UnsupportedSettlement
        ))
    ));

    for status in ["draft", "processing", "unknown"] {
        let (client, mut session, _server) = current_client(status, None).await;
        assert!(matches!(
            client
                .personal_renewal_observation(&mut session, &binding(), &failure)
                .await,
            Ok(StripeRenewalObservationResult::NeedsEvidence(
                StripeRenewalNeedsEvidence::UnsupportedStatus(actual)
            )) if actual == status
        ));
    }

    let mut unknown_subscription = current_subscription(false);
    unknown_subscription["status"] = json!("future_status");
    let (client, mut session, _server) = current_client_with_parts(
        current_invoice("paid"),
        current_invoice("paid"),
        current_line(),
        current_line(),
        unknown_subscription,
        current_subscription(false),
    )
    .await;
    assert!(matches!(
        client
            .personal_renewal_observation(&mut session, &binding(), &failure)
            .await,
        Ok(StripeRenewalObservationResult::NeedsEvidence(
            StripeRenewalNeedsEvidence::UnsupportedSubscriptionStatus(actual)
        )) if actual == "future_status"
    ));
}

#[tokio::test]
async fn settlement_requires_complete_nonnegative_amounts_and_no_off_stripe_value() {
    let failure = linked_failure().await;
    for field in ["amount_due", "amount_remaining"] {
        let mut invoice = current_invoice("void");
        invoice.as_object_mut().unwrap().remove(field);
        let (client, mut session, _server) = current_client_with_parts(
            invoice,
            current_invoice("void"),
            current_line(),
            current_line(),
            current_subscription(false),
            current_subscription(false),
        )
        .await;
        assert!(matches!(
            client
                .personal_renewal_observation(&mut session, &binding(), &failure)
                .await,
            Ok(StripeRenewalObservationResult::NeedsEvidence(
                StripeRenewalNeedsEvidence::UnsupportedSettlement
            ))
        ));
    }
    for field in ["amount_due", "amount_remaining"] {
        let mut invoice = current_invoice("void");
        invoice[field] = json!(-1);
        let (client, mut session, _server) = current_client_with_parts(
            invoice,
            current_invoice("void"),
            current_line(),
            current_line(),
            current_subscription(false),
            current_subscription(false),
        )
        .await;
        assert!(matches!(
            client
                .personal_renewal_observation(&mut session, &binding(), &failure)
                .await,
            Ok(StripeRenewalObservationResult::NeedsEvidence(
                StripeRenewalNeedsEvidence::UnsupportedSettlement
            ))
        ));
    }
    for field in ["amount_overpaid", "amount_paid_off_stripe"] {
        let mut invoice = current_invoice("paid");
        invoice[field] = json!(1);
        let (client, mut session, _server) = current_client_with_parts(
            invoice,
            current_invoice("paid"),
            current_line(),
            current_line(),
            current_subscription(false),
            current_subscription(false),
        )
        .await;
        assert!(matches!(
            client
                .personal_renewal_observation(&mut session, &binding(), &failure)
                .await,
            Err(StripeReadError::Observation(
                StripeContractError::UnsupportedSettlement(_)
            ))
        ));
    }
}

#[tokio::test]
async fn invoice_change_between_reads_returns_no_partial_observation() {
    let failure = linked_failure().await;
    let mut changed = current_invoice("paid");
    changed["metadata"]["sotto_allocation_reference"] = json!("other");
    let (client, mut session, _server) = current_client("paid", Some(changed)).await;
    let result = client
        .personal_renewal_observation(&mut session, &binding(), &failure)
        .await
        .unwrap();
    assert!(matches!(
        result,
        StripeRenewalObservationResult::NeedsEvidence(
            sotto_server::cloud_provider_stripe_http::StripeRenewalNeedsEvidence::ChangedDuringRead {
                resource: "invoice",
                ref fields,
            }
        ) if fields == &["allocation_reference"]
    ));
}

#[tokio::test]
async fn billing_and_cancellation_changes_name_the_provider_fields() {
    let failure = linked_failure().await;
    let mut changed_invoice = current_invoice("paid");
    changed_invoice["billing_reason"] = json!("subscription_update");
    let mut changed_subscription = current_subscription(false);
    changed_subscription
        .as_object_mut()
        .unwrap()
        .remove("canceled_at");
    let (client, mut session, _server) = current_client_with_parts(
        current_invoice("paid"),
        changed_invoice,
        current_line(),
        current_line(),
        current_subscription(false),
        changed_subscription,
    )
    .await;
    let result = client
        .personal_renewal_observation(&mut session, &binding(), &failure)
        .await
        .unwrap();
    assert!(matches!(
        result,
        StripeRenewalObservationResult::NeedsEvidence(
            StripeRenewalNeedsEvidence::ChangedDuringRead {
                resource: "invoice",
                ref fields,
            }
        ) if fields == &["billing_reason"]
    ));

    let (client, mut session, _server) = current_client_with_parts(
        current_invoice("paid"),
        current_invoice("paid"),
        current_line(),
        current_line(),
        current_subscription(false),
        {
            let mut subscription = current_subscription(false);
            subscription.as_object_mut().unwrap().remove("canceled_at");
            subscription
        },
    )
    .await;
    let result = client
        .personal_renewal_observation(&mut session, &binding(), &failure)
        .await
        .unwrap();
    assert!(matches!(
        result,
        StripeRenewalObservationResult::NeedsEvidence(
            StripeRenewalNeedsEvidence::ChangedDuringRead {
                resource: "subscription",
                ref fields,
            }
        )
        if fields == &["canceled_at"]
    ));
}

#[tokio::test]
async fn cancellation_fields_require_presence_and_valid_values() {
    let failure = linked_failure().await;
    let cases = [
        ("cancel_at", json!(null), true),
        ("cancel_at_period_end", json!(0), false),
        ("canceled_at", json!(-1), false),
    ];
    for (field, value, remove) in cases {
        let mut subscription = current_subscription(false);
        if remove {
            subscription.as_object_mut().unwrap().remove(field);
        } else {
            subscription[field] = value;
        }
        let (client, mut session, _server) = current_client_with_parts(
            current_invoice("paid"),
            current_invoice("paid"),
            current_line(),
            current_line(),
            subscription,
            current_subscription(false),
        )
        .await;
        assert!(matches!(
            client
                .personal_renewal_observation(&mut session, &binding(), &failure)
                .await,
            Err(StripeReadError::MalformedResponse(actual)) if actual == format!("subscription.{field}")
        ));
    }
}

#[tokio::test]
async fn every_nullable_cancellation_timestamp_rejects_missing_wrong_type_and_negative_values() {
    let failure = linked_failure().await;
    for field in ["cancel_at", "canceled_at", "ended_at"] {
        for value in [json!("not a timestamp"), json!(-1)] {
            let mut subscription = current_subscription(false);
            subscription[field] = value;
            let (client, mut session, _server) = current_client_with_parts(
                current_invoice("paid"),
                current_invoice("paid"),
                current_line(),
                current_line(),
                subscription,
                current_subscription(false),
            )
            .await;
            assert!(matches!(
                client
                    .personal_renewal_observation(&mut session, &binding(), &failure)
                    .await,
                Err(StripeReadError::MalformedResponse(actual)) if actual == format!("subscription.{field}")
            ));
        }
        let mut subscription = current_subscription(false);
        subscription.as_object_mut().unwrap().remove(field);
        let (client, mut session, _server) = current_client_with_parts(
            current_invoice("paid"),
            current_invoice("paid"),
            current_line(),
            current_line(),
            subscription,
            current_subscription(false),
        )
        .await;
        assert!(matches!(
            client
                .personal_renewal_observation(&mut session, &binding(), &failure)
                .await,
            Err(StripeReadError::MalformedResponse(actual)) if actual == format!("subscription.{field}")
        ));
    }
}

#[tokio::test]
async fn collection_method_change_is_reported_by_name() {
    let failure = linked_failure().await;
    let mut changed_invoice = current_invoice("paid");
    changed_invoice["collection_method"] = json!("send_invoice");
    let (client, mut session, _server) = current_client_with_parts(
        current_invoice("paid"),
        changed_invoice,
        current_line(),
        current_line(),
        current_subscription(false),
        current_subscription(false),
    )
    .await;
    let result = client
        .personal_renewal_observation(&mut session, &binding(), &failure)
        .await
        .unwrap();
    assert!(matches!(
        result,
        StripeRenewalObservationResult::NeedsEvidence(
            StripeRenewalNeedsEvidence::ChangedDuringRead {
                resource: "invoice",
                ref fields,
            }
        ) if fields == &["collection_method"]
    ));
}

#[tokio::test]
async fn contradictory_legacy_line_ownership_is_rejected() {
    let failure = linked_failure().await;
    let mut line = current_line();
    line["subscription"] = json!("sub_other");
    let (client, mut session, _server) = current_client_with_parts(
        current_invoice("paid"),
        current_invoice("paid"),
        line,
        current_line(),
        current_subscription(false),
        current_subscription(false),
    )
    .await;
    assert!(matches!(
        client
            .personal_renewal_observation(&mut session, &binding(), &failure)
            .await,
        Err(StripeReadError::Observation(
            StripeContractError::ContextMismatch
        ))
    ));
}

#[tokio::test]
async fn matching_legacy_line_ownership_remains_supported() {
    let failure = linked_failure().await;
    let mut line = current_line();
    line["subscription"] = json!("sub_renewal");
    line["subscription_item"] = json!("si_renewal");
    let (client, mut session, _server) = current_client_with_parts(
        current_invoice("paid"),
        current_invoice("paid"),
        line,
        current_line(),
        current_subscription(false),
        current_subscription(false),
    )
    .await;
    assert!(matches!(
        client
            .personal_renewal_observation(&mut session, &binding(), &failure)
            .await,
        Ok(StripeRenewalObservationResult::Observed(_))
    ));
}

#[tokio::test]
async fn closed_invoice_rejects_remaining_balance_above_due() {
    let failure = linked_failure().await;
    let mut invoice = current_invoice("void");
    invoice["amount_remaining"] = json!(300);
    let (client, mut session, _server) = current_client_with_parts(
        invoice,
        current_invoice("void"),
        current_line(),
        current_line(),
        current_subscription(false),
        current_subscription(false),
    )
    .await;
    let result = client
        .personal_renewal_observation(&mut session, &binding(), &failure)
        .await
        .unwrap();
    assert!(matches!(
        result,
        StripeRenewalObservationResult::NeedsEvidence(
            StripeRenewalNeedsEvidence::UnsupportedSettlement
        )
    ));
}

#[tokio::test]
async fn foreign_binding_and_session_are_rejected_before_resource_reads() {
    let failure = linked_failure().await;
    let (client, mut session, server) = current_client("paid", None).await;
    let foreign = StripeAllocationBinding::new(
        "alloc_other",
        "cus_other",
        "sub_other",
        "si_other",
        PayerKind::Personal,
    )
    .unwrap();
    assert!(matches!(
        client
            .personal_renewal_observation(&mut session, &foreign, &failure)
            .await,
        Err(StripeReadError::ContextMismatch)
    ));
    assert!(server.requests().is_empty());

    let (other_client, other_session, other_server) = current_client("paid", None).await;
    let mut foreign_session = other_session;
    assert!(matches!(
        client
            .personal_renewal_observation(&mut foreign_session, &binding(), &failure)
            .await,
        Err(StripeReadError::SessionClientMismatch)
    ));
    assert!(server.requests().is_empty());
    assert!(other_server.requests().is_empty());
    drop(other_client);
}

#[tokio::test]
async fn client_account_and_environment_mismatches_stop_before_subscription_reads() {
    let failure = linked_failure().await;
    let (_base, _session, server) = current_client("paid", None).await;
    let other_config = StripeCoverageConfig::new(
        "acct_other",
        ProviderEnvironment::Test,
        "price_month",
        "price_year",
    )
    .unwrap();
    let client = StripeReadClient::for_test(
        "sk_test_renewal",
        &other_config,
        server.origin.clone(),
        limits(),
    )
    .unwrap();
    let mut session = client.session();
    assert!(matches!(
        client
            .personal_renewal_observation(&mut session, &binding(), &failure)
            .await,
        Err(StripeReadError::ContextMismatch)
    ));
    assert!(server
        .requests()
        .iter()
        .all(|request| !request.contains("/v1/subscriptions/")));

    let (_base, _session, server) = current_client("paid", None).await;
    let live_config = StripeCoverageConfig::new(
        "acct_test_renewal",
        ProviderEnvironment::Live,
        "price_month",
        "price_year",
    )
    .unwrap();
    let client = StripeReadClient::for_test(
        "sk_live_renewal",
        &live_config,
        server.origin.clone(),
        limits(),
    )
    .unwrap();
    let mut session = client.session();
    assert!(matches!(
        client
            .personal_renewal_observation(&mut session, &binding(), &failure)
            .await,
        Err(StripeReadError::ContextMismatch)
    ));
    assert!(server
        .requests()
        .iter()
        .all(|request| !request.contains("/v1/subscriptions/")));
}

#[tokio::test]
async fn each_binding_component_is_checked_before_any_resource_read() {
    let failure = linked_failure().await;
    let cases = [
        (
            "allocation",
            "alloc_other",
            "cus_renewal",
            "sub_renewal",
            "si_renewal",
        ),
        (
            "customer",
            "alloc_renewal",
            "cus_other",
            "sub_renewal",
            "si_renewal",
        ),
        (
            "subscription",
            "alloc_renewal",
            "cus_renewal",
            "sub_other",
            "si_renewal",
        ),
        (
            "provider item",
            "alloc_renewal",
            "cus_renewal",
            "sub_renewal",
            "si_other",
        ),
    ];
    for (field, allocation, customer, subscription, item) in cases {
        let (client, mut session, server) = current_client("paid", None).await;
        let foreign = StripeAllocationBinding::new(
            allocation,
            customer,
            subscription,
            item,
            PayerKind::Personal,
        )
        .unwrap();
        assert!(
            matches!(
                client
                    .personal_renewal_observation(&mut session, &foreign, &failure)
                    .await,
                Err(StripeReadError::ContextMismatch)
            ),
            "{field}"
        );
        assert!(server.requests().is_empty(), "{field}");
    }

    assert!(matches!(
        StripeAllocationBinding::new(
            "alloc_renewal",
            "cus_renewal",
            "sub_renewal",
            "si_renewal",
            PayerKind::Sponsor,
        ),
        Err(StripeContractError::UnsupportedPayerKind)
    ));

    for legacy_field in ["subscription", "subscription_item"] {
        let mut line = current_line();
        line[legacy_field] = json!("not a valid ref");
        let (client, mut session, server) = current_client_with_parts(
            current_invoice("paid"),
            current_invoice("paid"),
            line,
            current_line(),
            current_subscription(false),
            current_subscription(false),
        )
        .await;
        assert!(
            matches!(
                client
                    .personal_renewal_observation(&mut session, &binding(), &failure)
                    .await,
                Err(StripeReadError::MalformedResponse(_))
            ),
            "{legacy_field}"
        );
        assert!(server
            .requests()
            .iter()
            .all(|request| !request.contains("/v1/invoice_payments")));
    }
}

#[tokio::test]
async fn each_current_invoice_context_component_is_checked_before_settlement_reads() {
    let failure = linked_failure().await;
    let mut cases = Vec::new();
    let mut customer = current_invoice("paid");
    customer["customer"] = json!("cus_other");
    cases.push(customer);
    let mut allocation = current_invoice("paid");
    allocation["metadata"]["sotto_allocation_reference"] = json!("alloc_other");
    cases.push(allocation);
    let mut mode = current_invoice("paid");
    mode["livemode"] = json!(true);
    cases.push(mode);
    let mut parent_type = current_invoice("paid");
    parent_type["parent"]["type"] = json!("invoice_item_details");
    cases.push(parent_type);
    let mut nested_parent = current_invoice("paid");
    nested_parent["parent"]["subscription_details"]["subscription"] = json!("sub_other");
    cases.push(nested_parent);
    for invoice in cases {
        let (client, mut session, server) = current_client_with_parts(
            invoice.clone(),
            invoice,
            current_line(),
            current_line(),
            current_subscription(false),
            current_subscription(false),
        )
        .await;
        assert!(matches!(
            client
                .personal_renewal_observation(&mut session, &binding(), &failure)
                .await,
            Err(StripeReadError::ContextMismatch)
        ));
        assert!(server
            .requests()
            .iter()
            .all(|request| !request.contains("/v1/invoice_payments")));
    }
}

#[tokio::test]
async fn open_to_paid_header_transition_returns_changed_fields_without_retrying() {
    let failure = linked_failure().await;
    let mut paid = current_invoice("paid");
    paid["amount_remaining"] = json!(0);
    let (client, mut session, server) = current_client_with_parts(
        current_invoice("open"),
        paid,
        current_line(),
        current_line(),
        current_subscription(false),
        current_subscription(false),
    )
    .await;
    let result = client
        .personal_renewal_observation(&mut session, &binding(), &failure)
        .await
        .unwrap();
    assert!(matches!(
        result,
        StripeRenewalObservationResult::NeedsEvidence(
            StripeRenewalNeedsEvidence::ChangedDuringRead {
                resource: "invoice",
                ref fields,
            }
        ) if fields == &["status", "amount_paid", "amount_remaining"]
    ));
    assert_eq!(
        server
            .requests()
            .iter()
            .filter(|request| request.as_str() == "GET /v1/invoices/in_failed")
            .count(),
        2
    );
}

#[tokio::test]
async fn stable_paid_observation_matches_across_sessions() {
    let failure = linked_failure().await;
    let (client, mut first_session, server) = current_client("paid", None).await;
    server.replace_responses(
        "/v1/account",
        vec![
            json!({"id":"acct_test_renewal","livemode":false}),
            json!({"id":"acct_test_renewal","livemode":false}),
        ],
    );
    server.replace_responses(
        "/v1/subscriptions/sub_renewal",
        vec![
            current_subscription(false),
            current_subscription(false),
            current_subscription(false),
            current_subscription(false),
        ],
    );
    server.replace_responses(
        "/v1/invoices/in_failed",
        vec![
            current_invoice("paid"),
            current_invoice("paid"),
            current_invoice("paid"),
            current_invoice("paid"),
        ],
    );
    server.replace_responses(
        "/v1/invoices/in_failed/lines",
        vec![
            list(vec![current_line()], false),
            list(vec![current_line()], false),
        ],
    );
    server.replace_responses(
        "/v1/invoice_payments",
        vec![
            list(
                vec![json!({
                    "id":"inpay_current","invoice":"in_failed","status":"paid","amount_paid":299,
                    "amount_requested":299,"currency":"gbp","livemode":false,
                    "payment":{"type":"payment_intent","payment_intent":"pi_current"}
                })],
                false,
            ),
            list(
                vec![json!({
                    "id":"inpay_current","invoice":"in_failed","status":"paid","amount_paid":299,
                    "amount_requested":299,"currency":"gbp","livemode":false,
                    "payment":{"type":"payment_intent","payment_intent":"pi_current"}
                })],
                false,
            ),
        ],
    );
    for path in ["/v1/refunds", "/v1/disputes", "/v1/credit_notes"] {
        server.replace_responses(path, vec![list(Vec::new(), false), list(Vec::new(), false)]);
    }
    let first = client
        .personal_renewal_observation(&mut first_session, &binding(), &failure)
        .await
        .unwrap();
    let mut second_session = client.session();
    let second = client
        .personal_renewal_observation(&mut second_session, &binding(), &failure)
        .await
        .unwrap();
    let (
        StripeRenewalObservationResult::Observed(first),
        StripeRenewalObservationResult::Observed(second),
    ) = (first, second)
    else {
        panic!("expected stable observations");
    };
    assert_eq!(first, second);
}

#[tokio::test]
async fn current_line_contract_rejects_proration_quantity_period_and_price_changes() {
    let failure = linked_failure().await;
    let mut cases = Vec::new();
    let mut prorated = current_line();
    prorated["parent"]["subscription_item_details"]["proration"] = json!(true);
    cases.push(prorated);
    let mut wrong_quantity = current_line();
    wrong_quantity["quantity"] = json!(2);
    cases.push(wrong_quantity);
    let mut wrong_period = current_line();
    wrong_period["period"]["start"] = json!(2100);
    cases.push(wrong_period);
    let mut wrong_price = current_line();
    wrong_price["pricing"]["price_details"]["price"] = json!("price_other");
    cases.push(wrong_price);
    for line in cases {
        let (client, mut session, _server) = current_client_with_parts(
            current_invoice("paid"),
            current_invoice("paid"),
            line,
            current_line(),
            current_subscription(false),
            current_subscription(false),
        )
        .await;
        let result = client
            .personal_renewal_observation(&mut session, &binding(), &failure)
            .await;
        assert!(matches!(
            result,
            Err(StripeReadError::Observation(
                StripeContractError::ContextMismatch
            )) | Err(StripeReadError::MalformedResponse("line.price"))
        ));
    }
}

#[tokio::test]
async fn current_line_contract_rejects_list_shape_and_missing_period_evidence() {
    let failure = linked_failure().await;
    let cases = [
        (Vec::new(), "empty line list"),
        (
            {
                let mut second = current_line();
                second["id"] = json!("il_other");
                vec![current_line(), second]
            },
            "multiple lines",
        ),
    ];
    for (lines, label) in cases {
        let (client, mut session, _server) = current_client_with_line_lists_and_limits(
            current_invoice("paid"),
            current_invoice("paid"),
            lines,
            vec![current_line()],
            current_subscription(false),
            current_subscription(false),
            limits(),
        )
        .await;
        assert!(
            matches!(
                client
                    .personal_renewal_observation(&mut session, &binding(), &failure)
                    .await,
                Err(StripeReadError::Observation(
                    StripeContractError::UnsupportedQuantity
                )),
            ),
            "{label}"
        );
    }

    for field in [
        "id",
        "period",
        "period_start",
        "invoice",
        "parent",
        "parent_type",
        "pricing",
    ] {
        let mut line = current_line();
        if field == "id" {
            line.as_object_mut().unwrap().remove("id");
        } else if field == "period" {
            line["period"].as_object_mut().unwrap().remove("end");
        } else if field == "period_start" {
            line["period"].as_object_mut().unwrap().remove("start");
        } else if field == "invoice" {
            line["invoice"] = json!("in_other");
        } else if field == "parent" {
            line["parent"]["subscription_item_details"]
                .as_object_mut()
                .unwrap()
                .remove("proration");
        } else if field == "parent_type" {
            line["parent"]["type"] = json!("invoice_item_details");
        } else {
            line["pricing"]["price_details"]
                .as_object_mut()
                .unwrap()
                .remove("price");
        }
        let (client, mut session, _server) = current_client_with_parts(
            current_invoice("paid"),
            current_invoice("paid"),
            line,
            current_line(),
            current_subscription(false),
            current_subscription(false),
        )
        .await;
        let result = client
            .personal_renewal_observation(&mut session, &binding(), &failure)
            .await;
        match field {
            "id" => assert!(matches!(
                result,
                Err(StripeReadError::MalformedResponse("list.data.id"))
            )),
            "pricing" => assert!(matches!(
                result,
                Err(StripeReadError::MalformedResponse("line.price"))
            )),
            _ => assert!(matches!(
                result,
                Err(StripeReadError::Observation(
                    StripeContractError::ContextMismatch
                ))
            )),
        }
    }

    let (client, mut session, server) = current_client("paid", None).await;
    let mut second_page_line = current_line();
    second_page_line["id"] = json!("il_other");
    server.replace_responses(
        "/v1/invoices/in_failed/lines",
        vec![
            list(vec![current_line()], true),
            list(vec![second_page_line], false),
        ],
    );
    assert!(matches!(
        client
            .personal_renewal_observation(&mut session, &binding(), &failure)
            .await,
        Err(StripeReadError::Observation(
            StripeContractError::UnsupportedQuantity
        ))
    ));
    assert!(server.requests().iter().any(|request| request
        .contains("/v1/invoices/in_failed/lines?limit=100&starting_after=il_failed")));
}

#[tokio::test]
async fn current_observation_consumes_one_shared_request_budget() {
    let failure = linked_failure().await;
    let mut read_limits = limits();
    read_limits.max_requests = 9;
    let (client, mut session, _server) = current_client_with_parts_and_limits(
        current_invoice("paid"),
        current_invoice("paid"),
        current_line(),
        current_line(),
        current_subscription(false),
        current_subscription(false),
        read_limits,
    )
    .await;
    assert!(matches!(
        client
            .personal_renewal_observation(&mut session, &binding(), &failure)
            .await,
        Err(StripeReadError::RequestBoundExceeded)
    ));
}

#[tokio::test]
async fn history_and_renewal_share_page_budget_and_stop_before_a_new_correction_read() {
    let failure = linked_failure().await;
    let mut responses = HashMap::new();
    responses.insert(
        "/v1/account".into(),
        vec![json!({"id":"acct_test_renewal","livemode":false})],
    );
    responses.insert(
        "/v1/subscriptions/sub_renewal".into(),
        vec![
            json!({"id":"sub_renewal","customer":"cus_renewal","status":"active","livemode":false}),
            json!({"id":"sub_renewal","customer":"cus_renewal","status":"active","livemode":false,"cancel_at_period_end":false,"cancel_at":null,"canceled_at":null,"ended_at":null}),
            json!({"id":"sub_renewal","customer":"cus_renewal","status":"active","livemode":false,"cancel_at_period_end":false,"cancel_at":null,"canceled_at":null,"ended_at":null}),
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
        "/v1/invoices/in_failed".into(),
        vec![current_invoice("paid"), current_invoice("paid")],
    );
    responses.insert(
        "/v1/invoices/in_failed/lines".into(),
        vec![list(vec![current_line()], false)],
    );
    responses.insert(
        "/v1/invoice_payments".into(),
        vec![
            list(
                vec![json!({
                    "id":"inpay_paid","invoice":"in_paid","status":"paid","amount_paid":299,
                    "amount_requested":299,"currency":"gbp","livemode":false,
                    "payment":{"type":"payment_intent","payment_intent":"pi_paid"}
                })],
                false,
            ),
            list(
                vec![json!({
                    "id":"inpay_current","invoice":"in_failed","status":"paid","amount_paid":299,
                    "amount_requested":299,"currency":"gbp","livemode":false,
                    "payment":{"type":"payment_intent","payment_intent":"pi_current"}
                })],
                false,
            ),
        ],
    );
    for path in ["/v1/refunds", "/v1/disputes", "/v1/credit_notes"] {
        responses.insert(
            path.into(),
            vec![list(Vec::new(), false), list(Vec::new(), false)],
        );
    }
    let server = mock_server(responses).await;
    let mut read_limits = limits();
    read_limits.max_pages = 10;
    let client = StripeReadClient::for_test(
        "sk_test_renewal",
        &config(),
        server.origin.clone(),
        read_limits,
    )
    .unwrap();
    let mut session = client.session();
    assert!(matches!(
        client
            .personal_invoice_history(&mut session, &binding())
            .await,
        Ok(StripePersonalInvoiceHistoryResult::Observed(_))
    ));
    assert!(matches!(
        client
            .personal_renewal_observation(&mut session, &binding(), &failure)
            .await,
        Err(StripeReadError::PageBoundExceeded)
    ));
    let requests = server.requests();
    assert!(requests
        .iter()
        .any(|request| request.contains("GET /v1/disputes?payment_intent=pi_current")));
    assert!(!requests
        .iter()
        .any(|request| request.contains("GET /v1/credit_notes?invoice=in_failed")));
    assert_eq!(
        requests.last().map(String::as_str),
        Some("GET /v1/disputes?payment_intent=pi_current&limit=100")
    );
}

#[tokio::test]
async fn late_pending_response_hits_the_shared_deadline_without_partial_observation() {
    let failure = linked_failure().await;
    let mut read_limits = limits();
    read_limits.session_timeout = std::time::Duration::from_millis(50);
    let (client, session, server) = current_client_with_line_lists_and_limits_pending(
        current_invoice("paid"),
        current_invoice("paid"),
        vec![current_line()],
        vec![current_line()],
        current_subscription(false),
        current_subscription(false),
        read_limits,
        Some("/v1/invoices/in_failed/lines"),
    )
    .await;
    let binding = binding();
    let task = tokio::spawn(async move {
        let mut session = session;
        client
            .personal_renewal_observation(&mut session, &binding, &failure)
            .await
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        server.wait_for_pending_request(),
    )
    .await
    .expect("pending request entered");
    let result = tokio::time::timeout(std::time::Duration::from_secs(1), task)
        .await
        .expect("bounded teardown")
        .expect("observation task");
    assert!(matches!(result, Err(StripeReadError::Timeout)));
    assert!(server
        .requests()
        .iter()
        .all(|request| !request.contains("/v1/invoice_payments")));
}

#[tokio::test]
async fn associated_refund_preserves_the_paid_term_and_unknown_correction_stays_unresolved() {
    let failure = linked_failure().await;
    let (client, mut session, server) = current_client("paid", None).await;
    server.replace_responses("/v1/refunds", vec![list(vec![refund("succeeded")], false)]);
    let result = client
        .personal_renewal_observation(&mut session, &binding(), &failure)
        .await
        .unwrap();
    let StripeRenewalObservationResult::Observed(observation) = result else {
        panic!("expected observed paid term");
    };
    let StripeRenewalCurrentState::Paid { term, .. } = observation.state() else {
        panic!("expected paid state");
    };
    assert_eq!(term.period_start(), failure.renewal_period_start());
    assert_eq!(term.period_end(), failure.renewal_period_end());
    assert_eq!(term.invoice_id(), "in_failed");
    assert!(server
        .requests()
        .iter()
        .any(|request| request.contains("GET /v1/refunds?payment_intent=pi_current")));

    let (client, mut session, server) = current_client("paid", None).await;
    server.replace_responses(
        "/v1/disputes",
        vec![list(vec![dispute("under_review")], false)],
    );
    let result = client
        .personal_renewal_observation(&mut session, &binding(), &failure)
        .await
        .unwrap();
    let StripeRenewalObservationResult::Observed(observation) = result else {
        panic!("expected observed paid term with dispute");
    };
    let StripeRenewalCurrentState::Paid { term, .. } = observation.state() else {
        panic!("expected paid state with dispute");
    };
    assert_eq!(term.period_end(), failure.renewal_period_end());
    assert_eq!(observation.cancellation().status(), Some("active"));

    let (client, mut session, server) = current_client("paid", None).await;
    server.replace_responses(
        "/v1/refunds",
        vec![list(vec![refund("future_status")], false)],
    );
    let result = client
        .personal_renewal_observation(&mut session, &binding(), &failure)
        .await
        .unwrap();
    assert!(matches!(
        result,
        StripeRenewalObservationResult::NeedsEvidence(
            StripeRenewalNeedsEvidence::UnsupportedSettlement
        )
    ));
}
