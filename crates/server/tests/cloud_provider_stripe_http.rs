//! Loopback tests for bounded Stripe coverage reads.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use serde_json::{json, Value};
use sotto_server::cloud_provider::{PayerKind, ProviderEnvironment};
use sotto_server::cloud_provider_stripe::{StripeAllocationBinding, StripeCoverageConfig};
use sotto_server::cloud_provider_stripe_http::{
    StripeReadClient, StripeReadError, StripeReadLimits, StripeRefundResource, StripeRefundStatus,
};
use tokio::net::TcpListener;
use url::Url;

const API_KEY: &str = "sk_test_transport";

#[derive(Clone)]
struct MockState {
    responses: Arc<Mutex<HashMap<String, Vec<MockResponse>>>>,
    calls: Arc<Mutex<Vec<Call>>>,
}

#[derive(Clone)]
struct MockResponse {
    status: StatusCode,
    headers: Vec<(&'static str, String)>,
    body: String,
    streamed: bool,
}

#[derive(Clone, Debug)]
struct Call {
    path_and_query: String,
    authorization: Option<String>,
    version: Option<String>,
}

impl MockResponse {
    fn json(value: Value) -> Self {
        Self {
            status: StatusCode::OK,
            headers: vec![("content-type", "application/json".into())],
            body: serde_json::to_string(&value).unwrap(),
            streamed: false,
        }
    }

    fn status(status: StatusCode, body: &str) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.into(),
            streamed: false,
        }
    }

    fn redirect(target: &str) -> Self {
        Self {
            status: StatusCode::FOUND,
            headers: vec![("location", target.into())],
            body: String::new(),
            streamed: false,
        }
    }

    fn streamed(mut self) -> Self {
        self.streamed = true;
        self
    }

    fn with_header(mut self, name: &'static str, value: &str) -> Self {
        self.headers.push((name, value.into()));
        self
    }
}

struct MockServer {
    origin: Url,
    state: MockState,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn mock_handler(State(state): State<MockState>, request: Request<Body>) -> Response<Body> {
    let path_and_query = request
        .uri()
        .path_and_query()
        .map(|value| value.as_str().to_owned())
        .unwrap_or_else(|| "/".into());
    state.calls.lock().unwrap().push(Call {
        path_and_query: path_and_query.clone(),
        authorization: request
            .headers()
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned),
        version: request
            .headers()
            .get("stripe-version")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned),
    });
    let path = request.uri().path().to_owned();
    let response = state
        .responses
        .lock()
        .unwrap()
        .get_mut(&path)
        .and_then(|responses| {
            if responses.is_empty() {
                None
            } else {
                Some(responses.remove(0))
            }
        })
        .unwrap_or_else(|| MockResponse::status(StatusCode::NOT_FOUND, "{}"));
    let mut builder = Response::builder().status(response.status);
    for (name, value) in response.headers {
        builder = builder.header(name, value);
    }
    if response.streamed {
        builder
            .body(Body::from_stream(futures_util::stream::iter([Ok::<
                _,
                std::convert::Infallible,
            >(
                response.body,
            )])))
            .unwrap()
    } else {
        builder.body(Body::from(response.body)).unwrap()
    }
}

async fn mock_server(responses: HashMap<String, Vec<MockResponse>>) -> MockServer {
    let state = MockState {
        responses: Arc::new(Mutex::new(responses)),
        calls: Arc::new(Mutex::new(Vec::new())),
    };
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new()
        .fallback(any(mock_handler))
        .with_state(state.clone());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    MockServer {
        origin: Url::parse(&format!("http://{address}/")).unwrap(),
        state,
        task,
    }
}

fn config() -> StripeCoverageConfig {
    StripeCoverageConfig::new(
        "acct_test_transport",
        ProviderEnvironment::Test,
        "price_month",
        "price_year",
    )
    .unwrap()
}

fn account() -> Value {
    json!({"id":"acct_test_transport","object":"account","livemode":false})
}

fn list(data: Vec<Value>, has_more: bool) -> Value {
    json!({"object":"list","data":data,"has_more":has_more})
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
        max_retries: 1,
        max_retry_after: std::time::Duration::from_millis(10),
    }
}

fn personal_binding() -> StripeAllocationBinding {
    StripeAllocationBinding::new("alloc_1", "cus_1", "sub_1", "si_1", PayerKind::Personal).unwrap()
}

fn paid_invoice() -> Value {
    json!({
        "id":"in_1",
        "customer":"cus_1",
        "subscription":"sub_1",
        "status":"paid",
        "currency":"gbp",
        "amount_paid":299,
        "amount_due":299,
        "amount_overpaid":0,
        "amount_paid_off_stripe":0,
        "livemode":false,
        "metadata":{"sotto_allocation_reference":"alloc_1"}
    })
}

fn personal_line(id: &str) -> Value {
    json!({
        "id":id,
        "quantity":1,
        "livemode":false,
        "parent":{
            "type":"subscription_item_details",
            "subscription_item_details":{
                "subscription":"sub_1",
                "subscription_item":"si_1"
            }
        },
        "pricing":{
            "type":"price_details",
            "price_details":{"price":"price_month"}
        },
        "period":{"start":1700000000,"end":1702592000}
    })
}

fn paid_payment() -> Value {
    json!({
        "id":"inpay_1",
        "invoice":"in_1",
        "status":"paid",
        "amount_paid":299,
        "amount_requested":299,
        "currency":"gbp",
        "livemode":false,
        "payment":{"type":"payment_intent","payment_intent":"pi_1"}
    })
}

fn observation_responses(
    invoice: Value,
    line: Value,
    payment: Value,
) -> HashMap<String, Vec<MockResponse>> {
    let mut responses = HashMap::new();
    responses.insert("/v1/account".into(), vec![MockResponse::json(account())]);
    responses.insert(
        "/v1/invoices/in_1".into(),
        vec![MockResponse::json(invoice)],
    );
    responses.insert(
        "/v1/invoices/in_1/lines".into(),
        vec![MockResponse::json(list(vec![line], false))],
    );
    responses.insert(
        "/v1/invoice_payments".into(),
        vec![MockResponse::json(list(vec![payment], false))],
    );
    responses
}

#[tokio::test]
async fn assembles_a_personal_invoice_observation_from_complete_reads() {
    let mut responses = HashMap::new();
    responses.insert("/v1/account".into(), vec![MockResponse::json(account())]);
    responses.insert(
        "/v1/invoices/in_1".into(),
        vec![MockResponse::json(paid_invoice())],
    );
    responses.insert(
        "/v1/invoices/in_1/lines".into(),
        vec![MockResponse::json(list(vec![personal_line("il_1")], false))],
    );
    responses.insert(
        "/v1/invoice_payments".into(),
        vec![MockResponse::json(list(vec![paid_payment()], false))],
    );
    let server = mock_server(responses).await;
    let client =
        StripeReadClient::for_test(API_KEY, &config(), server.origin.clone(), limits()).unwrap();
    let mut session = client.session();

    let observation = client
        .personal_invoice_observation(&mut session, "in_1", &personal_binding())
        .await
        .unwrap();

    assert_eq!(observation.invoice_id(), "in_1");
    assert_eq!(observation.customer_id(), "cus_1");
    assert_eq!(observation.subscription_id(), "sub_1");
    assert_eq!(observation.provider_item_id(), "si_1");
    assert_eq!(observation.allocation_reference(), "alloc_1");
    assert_eq!(observation.payment_intent_id(), "pi_1");
    assert_eq!(observation.currency(), "gbp");
    assert_eq!(observation.amount_paid(), 299);
    assert_eq!(observation.interval().to_string(), "month");
    assert_eq!(observation.period_start(), 1_700_000_000);
    assert_eq!(observation.period_end(), 1_702_592_000);
    assert_eq!(
        observation.evidence_reference(),
        "stripe:invoice:in_1:line:il_1"
    );
}

#[tokio::test]
async fn reads_all_payment_pages_before_rejecting_ambiguous_settlement() {
    let mut responses = HashMap::new();
    responses.insert("/v1/account".into(), vec![MockResponse::json(account())]);
    responses.insert(
        "/v1/invoices/in_1".into(),
        vec![MockResponse::json(paid_invoice())],
    );
    responses.insert(
        "/v1/invoices/in_1/lines".into(),
        vec![MockResponse::json(list(vec![personal_line("il_1")], false))],
    );
    responses.insert(
        "/v1/invoice_payments".into(),
        vec![
            MockResponse::json(list(vec![paid_payment()], true)),
            MockResponse::json(list(
                vec![json!({
                    "id":"inpay_2",
                    "invoice":"in_1",
                    "status":"canceled",
                    "amount_paid":null,
                    "amount_requested":299,
                    "currency":"gbp",
                    "livemode":false,
                    "payment":{"type":"payment_intent","payment_intent":"pi_2"}
                })],
                false,
            )),
        ],
    );
    let server = mock_server(responses).await;
    let client =
        StripeReadClient::for_test(API_KEY, &config(), server.origin.clone(), limits()).unwrap();
    let mut session = client.session();

    assert!(matches!(
        client
            .personal_invoice_observation(&mut session, "in_1", &personal_binding())
            .await,
        Err(StripeReadError::Observation(
            sotto_server::cloud_provider_stripe::StripeContractError::UnsupportedSettlement(_)
        ))
    ));
    assert!(server
        .state
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|call| call.path_and_query.contains("starting_after=inpay_1")));
}

#[tokio::test]
async fn rejects_amount_period_and_settlement_contract_violations() {
    let mut invoice = paid_invoice();
    invoice["amount_due"] = json!(300);
    let server = mock_server(observation_responses(
        invoice,
        personal_line("il_1"),
        paid_payment(),
    ))
    .await;
    let client =
        StripeReadClient::for_test(API_KEY, &config(), server.origin.clone(), limits()).unwrap();
    let mut session = client.session();
    assert!(matches!(
        client
            .personal_invoice_observation(&mut session, "in_1", &personal_binding())
            .await,
        Err(StripeReadError::Observation(
            sotto_server::cloud_provider_stripe::StripeContractError::UnsupportedSettlement(_)
        ))
    ));

    let mut line = personal_line("il_1");
    line["period"]["end"] = json!(1700000000);
    let server = mock_server(observation_responses(paid_invoice(), line, paid_payment())).await;
    let client =
        StripeReadClient::for_test(API_KEY, &config(), server.origin.clone(), limits()).unwrap();
    let mut session = client.session();
    assert!(matches!(
        client
            .personal_invoice_observation(&mut session, "in_1", &personal_binding())
            .await,
        Err(StripeReadError::Observation(
            sotto_server::cloud_provider_stripe::StripeContractError::InvalidField("period")
        ))
    ));

    let mut payment = paid_payment();
    payment["amount_paid"] = json!(298);
    payment["amount_requested"] = json!(298);
    let server = mock_server(observation_responses(
        paid_invoice(),
        personal_line("il_1"),
        payment,
    ))
    .await;
    let client =
        StripeReadClient::for_test(API_KEY, &config(), server.origin.clone(), limits()).unwrap();
    let mut session = client.session();
    assert!(matches!(
        client
            .personal_invoice_observation(&mut session, "in_1", &personal_binding())
            .await,
        Err(StripeReadError::Observation(
            sotto_server::cloud_provider_stripe::StripeContractError::UnsupportedSettlement(_)
        ))
    ));
}

#[tokio::test]
async fn rejects_unpaid_invoice_and_untrusted_binding_before_observation() {
    let mut responses = HashMap::new();
    responses.insert("/v1/account".into(), vec![MockResponse::json(account())]);
    let mut unpaid_invoice = paid_invoice();
    unpaid_invoice["status"] = json!("open");
    responses.insert(
        "/v1/invoices/in_1".into(),
        vec![MockResponse::json(unpaid_invoice)],
    );
    let server = mock_server(responses).await;
    let client =
        StripeReadClient::for_test(API_KEY, &config(), server.origin.clone(), limits()).unwrap();
    let mut session = client.session();
    assert!(matches!(
        client
            .personal_invoice_observation(&mut session, "in_1", &personal_binding())
            .await,
        Err(StripeReadError::Observation(
            sotto_server::cloud_provider_stripe::StripeContractError::UnpaidInvoice
        ))
    ));

    let mut responses = HashMap::new();
    responses.insert("/v1/account".into(), vec![MockResponse::json(account())]);
    let mut contradictory_invoice = paid_invoice();
    contradictory_invoice["subscription"] = json!("sub_other");
    responses.insert(
        "/v1/invoices/in_1".into(),
        vec![MockResponse::json(contradictory_invoice)],
    );
    responses.insert(
        "/v1/invoices/in_1/lines".into(),
        vec![MockResponse::json(list(vec![personal_line("il_1")], false))],
    );
    responses.insert(
        "/v1/invoice_payments".into(),
        vec![MockResponse::json(list(vec![paid_payment()], false))],
    );
    let server = mock_server(responses).await;
    let client =
        StripeReadClient::for_test(API_KEY, &config(), server.origin.clone(), limits()).unwrap();
    let mut session = client.session();
    assert!(matches!(
        client
            .personal_invoice_observation(&mut session, "in_1", &personal_binding())
            .await,
        Err(StripeReadError::Observation(
            sotto_server::cloud_provider_stripe::StripeContractError::OwnershipMismatch
        ))
    ));

    let mut responses = HashMap::new();
    responses.insert("/v1/account".into(), vec![MockResponse::json(account())]);
    responses.insert(
        "/v1/invoices/in_1".into(),
        vec![MockResponse::json(paid_invoice())],
    );
    responses.insert(
        "/v1/invoices/in_1/lines".into(),
        vec![MockResponse::json(list(vec![personal_line("il_1")], false))],
    );
    responses.insert(
        "/v1/invoice_payments".into(),
        vec![MockResponse::json(list(vec![paid_payment()], false))],
    );
    let server = mock_server(responses).await;
    let client =
        StripeReadClient::for_test(API_KEY, &config(), server.origin.clone(), limits()).unwrap();
    let mut session = client.session();
    let wrong_binding =
        StripeAllocationBinding::new("alloc_other", "cus_1", "sub_1", "si_1", PayerKind::Personal)
            .unwrap();
    assert!(matches!(
        client
            .personal_invoice_observation(&mut session, "in_1", &wrong_binding)
            .await,
        Err(StripeReadError::Observation(
            sotto_server::cloud_provider_stripe::StripeContractError::OwnershipMismatch
        ))
    ));
}

#[tokio::test]
async fn rejects_malformed_present_invoice_fields_instead_of_treating_them_as_absent() {
    let mut responses = HashMap::new();
    responses.insert("/v1/account".into(), vec![MockResponse::json(account())]);
    let mut malformed_invoice = paid_invoice();
    malformed_invoice["amount_paid"] = json!("299");
    responses.insert(
        "/v1/invoices/in_1".into(),
        vec![MockResponse::json(malformed_invoice)],
    );
    let server = mock_server(responses).await;
    let client =
        StripeReadClient::for_test(API_KEY, &config(), server.origin.clone(), limits()).unwrap();
    let mut session = client.session();

    assert!(matches!(
        client
            .personal_invoice_observation(&mut session, "in_1", &personal_binding())
            .await,
        Err(StripeReadError::MalformedResponse("invoice.amount_paid"))
    ));
}

#[tokio::test]
async fn rejects_mixed_invoice_and_payment_currency_casing() {
    let mut responses = HashMap::new();
    responses.insert("/v1/account".into(), vec![MockResponse::json(account())]);
    let mut invoice = paid_invoice();
    invoice["currency"] = json!("GBP");
    responses.insert(
        "/v1/invoices/in_1".into(),
        vec![MockResponse::json(invoice)],
    );
    responses.insert(
        "/v1/invoices/in_1/lines".into(),
        vec![MockResponse::json(list(vec![personal_line("il_1")], false))],
    );
    responses.insert(
        "/v1/invoice_payments".into(),
        vec![MockResponse::json(list(vec![paid_payment()], false))],
    );
    let server = mock_server(responses).await;
    let client =
        StripeReadClient::for_test(API_KEY, &config(), server.origin.clone(), limits()).unwrap();
    let mut session = client.session();

    assert!(matches!(
        client
            .personal_invoice_observation(&mut session, "in_1", &personal_binding())
            .await,
        Err(StripeReadError::Observation(
            sotto_server::cloud_provider_stripe::StripeContractError::UnsupportedSettlement(_)
        ))
    ));
}

#[tokio::test]
async fn reads_resources_with_authentication_and_complete_pagination() {
    let mut responses = HashMap::new();
    responses.insert("/v1/account".into(), vec![MockResponse::json(account())]);
    responses.insert(
        "/v1/subscriptions/sub_1".into(),
        vec![MockResponse::json(json!({
            "id":"sub_1","customer":"cus_1","status":"active","livemode":false
        }))],
    );
    responses.insert(
        "/v1/invoices".into(),
        vec![
            MockResponse::json(list(
                vec![json!({"id":"in_1","customer":"cus_1","livemode":false})],
                true,
            )),
            MockResponse::json(list(
                vec![json!({"id":"in_2","customer":"cus_1","livemode":false})],
                false,
            )),
        ],
    );
    responses.insert(
        "/v1/invoices/in_1/lines".into(),
        vec![MockResponse::json(list(
            vec![json!({
                "id":"il_1","quantity":1,
                "parent":{"subscription_item_details":{"subscription":"sub_1","subscription_item":"si_1"}},
                "pricing":{"price_details":{"price":"price_month"}},
                "period":{"start":1,"end":2}
            })],
            false,
        ))],
    );
    responses.insert(
        "/v1/invoice_payments".into(),
        vec![MockResponse::json(list(
            vec![json!({
                "id":"inpay_1","invoice":"in_1","status":"paid",
                "amount_paid":299,"amount_requested":299,"currency":"gbp","livemode":false,
                "payment":{"type":"payment_intent","payment_intent":"pi_1"}
            })],
            false,
        ))],
    );
    let server = mock_server(responses).await;
    let client =
        StripeReadClient::for_test(API_KEY, &config(), server.origin.clone(), limits()).unwrap();
    let mut session = client.session();

    assert_eq!(
        client.account(&mut session).await.unwrap().id,
        "acct_test_transport"
    );
    assert_eq!(
        client
            .subscription(&mut session, "sub_1", "cus_1")
            .await
            .unwrap()
            .id,
        "sub_1"
    );
    assert_eq!(
        client
            .subscription_invoices(&mut session, "sub_1", Some("cus_1"))
            .await
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        client
            .invoice_lines(&mut session, "in_1")
            .await
            .unwrap()
            .len(),
        1
    );
    let payments = client.invoice_payments(&mut session, "in_1").await.unwrap();
    assert_eq!(payments[0].payment_intent_id.as_deref(), Some("pi_1"));
    assert!(payments[0].settlement(&config()).is_ok());

    let calls = server.state.calls.lock().unwrap();
    assert!(calls
        .iter()
        .all(|call| call.authorization.as_deref() == Some("Bearer sk_test_transport")));
    assert!(calls
        .iter()
        .all(|call| call.version.as_deref() == Some("2026-07-29.dahlia")));
    assert!(calls
        .iter()
        .any(|call| call.path_and_query.contains("starting_after=in_1")));
}

#[tokio::test]
async fn rejects_wrong_account_mode_redirect_and_invalid_pagination() {
    let mut responses = HashMap::new();
    responses.insert(
        "/v1/account".into(),
        vec![MockResponse::json(
            json!({"id":"acct_other","livemode":false}),
        )],
    );
    let server = mock_server(responses).await;
    let client =
        StripeReadClient::for_test(API_KEY, &config(), server.origin.clone(), limits()).unwrap();
    let mut session = client.session();
    assert!(matches!(
        client.account(&mut session).await,
        Err(StripeReadError::AccountMismatch)
    ));

    let mut responses = HashMap::new();
    responses.insert(
        "/v1/account".into(),
        vec![MockResponse::json(
            json!({"id":"acct_test_transport","livemode":true}),
        )],
    );
    let server = mock_server(responses).await;
    let client =
        StripeReadClient::for_test(API_KEY, &config(), server.origin.clone(), limits()).unwrap();
    let mut session = client.session();
    assert!(matches!(
        client.account(&mut session).await,
        Err(StripeReadError::ContextMismatch)
    ));

    let mut responses = HashMap::new();
    responses.insert(
        "/v1/account".into(),
        vec![MockResponse::redirect(
            "http://127.0.0.1:9/should-not-receive-credentials",
        )],
    );
    let server = mock_server(responses).await;
    let client =
        StripeReadClient::for_test(API_KEY, &config(), server.origin.clone(), limits()).unwrap();
    let mut session = client.session();
    assert!(matches!(
        client.account(&mut session).await,
        Err(StripeReadError::RedirectRejected)
    ));

    let mut responses = HashMap::new();
    responses.insert(
        "/v1/account".into(),
        vec![MockResponse::json(
            json!({"id":"acct_test_transport","livemode":false}),
        )],
    );
    responses.insert(
        "/v1/invoices".into(),
        vec![MockResponse::json(list(Vec::new(), true))],
    );
    let server = mock_server(responses).await;
    let client =
        StripeReadClient::for_test(API_KEY, &config(), server.origin.clone(), limits()).unwrap();
    let mut session = client.session();
    assert!(matches!(
        client
            .subscription_invoices(&mut session, "sub_1", None)
            .await,
        Err(StripeReadError::InvalidPagination(_))
    ));

    let mut responses = HashMap::new();
    responses.insert("/v1/account".into(), vec![MockResponse::json(account())]);
    responses.insert(
        "/v1/invoices".into(),
        vec![MockResponse::json(list(
            vec![json!({
                "id":"in_1","customer":"cus_1","subscription":"sub_other"
            })],
            false,
        ))],
    );
    let server = mock_server(responses).await;
    let client =
        StripeReadClient::for_test(API_KEY, &config(), server.origin.clone(), limits()).unwrap();
    let mut session = client.session();
    assert!(matches!(
        client
            .subscription_invoices(&mut session, "sub_1", Some("cus_1"))
            .await,
        Err(StripeReadError::ContextMismatch)
    ));

    let mut responses = HashMap::new();
    responses.insert(
        "/v1/account".into(),
        vec![MockResponse::status(
            StatusCode::FORBIDDEN,
            "permission denied",
        )],
    );
    let server = mock_server(responses).await;
    let client =
        StripeReadClient::for_test(API_KEY, &config(), server.origin.clone(), limits()).unwrap();
    let mut session = client.session();
    assert!(matches!(
        client.account(&mut session).await,
        Err(StripeReadError::Permission { status: 403 })
    ));
}

#[tokio::test]
async fn rejects_reused_sessions_and_unverified_resource_context() {
    let mut responses = HashMap::new();
    responses.insert("/v1/account".into(), vec![MockResponse::json(account())]);
    let server = mock_server(responses).await;
    let client =
        StripeReadClient::for_test(API_KEY, &config(), server.origin.clone(), limits()).unwrap();
    let other_client =
        StripeReadClient::for_test(API_KEY, &config(), server.origin.clone(), limits()).unwrap();
    let mut session = client.session();
    client.account(&mut session).await.unwrap();
    assert!(matches!(
        other_client.account(&mut session).await,
        Err(StripeReadError::SessionClientMismatch)
    ));

    let mut responses = HashMap::new();
    responses.insert("/v1/account".into(), vec![MockResponse::json(account())]);
    responses.insert(
        "/v1/subscriptions/sub_1".into(),
        vec![MockResponse::json(json!({
            "id":"sub_1","customer":"cus_1","status":"active"
        }))],
    );
    let server = mock_server(responses).await;
    let client =
        StripeReadClient::for_test(API_KEY, &config(), server.origin.clone(), limits()).unwrap();
    let mut session = client.session();
    assert!(matches!(
        client.subscription(&mut session, "sub_1", "cus_1").await,
        Err(StripeReadError::MalformedResponse("livemode"))
    ));

    let mut responses = HashMap::new();
    responses.insert("/v1/account".into(), vec![MockResponse::json(account())]);
    responses.insert(
        "/v1/invoices".into(),
        vec![MockResponse::json(list(
            vec![json!({"id":"in_1","customer":{}})],
            false,
        ))],
    );
    let server = mock_server(responses).await;
    let client =
        StripeReadClient::for_test(API_KEY, &config(), server.origin.clone(), limits()).unwrap();
    let mut session = client.session();
    assert!(matches!(
        client
            .subscription_invoices(&mut session, "sub_1", None)
            .await,
        Err(StripeReadError::MalformedResponse("invoice.customer"))
    ));
}

#[tokio::test]
async fn retries_transient_statuses_without_resetting_the_session_budget() {
    let mut responses = HashMap::new();
    responses.insert(
        "/v1/account".into(),
        vec![
            MockResponse::status(StatusCode::INTERNAL_SERVER_ERROR, "{}"),
            MockResponse::json(account()),
        ],
    );
    let server = mock_server(responses).await;
    let client =
        StripeReadClient::for_test(API_KEY, &config(), server.origin.clone(), limits()).unwrap();
    let mut session = client.session();
    assert_eq!(
        client.account(&mut session).await.unwrap().id,
        "acct_test_transport"
    );
    assert_eq!(server.state.calls.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn rejects_a_response_body_before_it_can_exceed_the_bound() {
    let mut responses = HashMap::new();
    responses.insert(
        "/v1/account".into(),
        vec![MockResponse::status(StatusCode::OK, &"x".repeat(100))],
    );
    let mut bounded = limits();
    bounded.max_response_bytes = 32;
    let server = mock_server(responses).await;
    let client =
        StripeReadClient::for_test(API_KEY, &config(), server.origin.clone(), bounded).unwrap();
    let mut session = client.session();
    assert!(!format!("{client:?}").contains(API_KEY));
    assert!(matches!(
        client.account(&mut session).await,
        Err(StripeReadError::ResponseTooLarge)
    ));
}

#[tokio::test]
async fn rejects_a_streamed_response_without_content_length() {
    let mut responses = HashMap::new();
    responses.insert(
        "/v1/account".into(),
        vec![MockResponse::status(StatusCode::OK, &"x".repeat(100)).streamed()],
    );
    let mut bounded = limits();
    bounded.max_response_bytes = 32;
    let server = mock_server(responses).await;
    let client =
        StripeReadClient::for_test(API_KEY, &config(), server.origin.clone(), bounded).unwrap();
    let mut session = client.session();
    assert!(matches!(
        client.account(&mut session).await,
        Err(StripeReadError::ResponseTooLarge)
    ));
}

#[tokio::test]
async fn retries_rate_limits_but_does_not_retry_authentication_or_leak_error_bodies() {
    let mut responses = HashMap::new();
    responses.insert(
        "/v1/account".into(),
        vec![
            MockResponse::status(
                StatusCode::TOO_MANY_REQUESTS,
                "rate secret sk_test_transport",
            )
            .with_header("retry-after", "0"),
            MockResponse::json(account()),
        ],
    );
    let server = mock_server(responses).await;
    let client =
        StripeReadClient::for_test(API_KEY, &config(), server.origin.clone(), limits()).unwrap();
    let mut session = client.session();
    assert!(client.account(&mut session).await.is_ok());
    assert_eq!(server.state.calls.lock().unwrap().len(), 2);

    let mut responses = HashMap::new();
    responses.insert(
        "/v1/account".into(),
        vec![MockResponse::status(
            StatusCode::UNAUTHORIZED,
            "authentication secret sk_test_transport",
        )],
    );
    let server = mock_server(responses).await;
    let client =
        StripeReadClient::for_test(API_KEY, &config(), server.origin.clone(), limits()).unwrap();
    let mut session = client.session();
    let error = client.account(&mut session).await.unwrap_err();
    assert!(matches!(
        error,
        StripeReadError::Authentication { status: 401 }
    ));
    assert!(!error.to_string().contains(API_KEY));
    assert_eq!(server.state.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn cumulative_response_bytes_are_shared_across_resource_reads() {
    let mut responses = HashMap::new();
    responses.insert("/v1/account".into(), vec![MockResponse::json(account())]);
    responses.insert(
        "/v1/subscriptions/sub_1".into(),
        vec![MockResponse::json(json!({
            "id":"sub_1","customer":"cus_1","status":"active","livemode":false
        }))],
    );
    let server = mock_server(responses).await;
    let mut bounded = limits();
    bounded.max_total_response_bytes = 100;
    let client =
        StripeReadClient::for_test(API_KEY, &config(), server.origin.clone(), bounded).unwrap();
    let mut session = client.session();
    assert!(client.account(&mut session).await.is_ok());
    assert!(matches!(
        client.subscription(&mut session, "sub_1", "cus_1").await,
        Err(StripeReadError::SessionBytesExceeded)
    ));
}

#[tokio::test]
async fn enforces_page_record_and_request_bounds() {
    let mut responses = HashMap::new();
    responses.insert("/v1/account".into(), vec![MockResponse::json(account())]);
    responses.insert(
        "/v1/invoices".into(),
        vec![MockResponse::json(list(
            vec![json!({"id":"in_1","customer":"cus_1"})],
            true,
        ))],
    );
    let server = mock_server(responses).await;
    let mut bounded = limits();
    bounded.max_pages = 1;
    let client =
        StripeReadClient::for_test(API_KEY, &config(), server.origin.clone(), bounded).unwrap();
    let mut session = client.session();
    assert!(matches!(
        client
            .subscription_invoices(&mut session, "sub_1", None)
            .await,
        Err(StripeReadError::PageBoundExceeded)
    ));

    let mut responses = HashMap::new();
    responses.insert("/v1/account".into(), vec![MockResponse::json(account())]);
    responses.insert(
        "/v1/invoices".into(),
        vec![MockResponse::json(list(
            vec![
                json!({"id":"in_1","customer":"cus_1"}),
                json!({"id":"in_2","customer":"cus_1"}),
            ],
            false,
        ))],
    );
    let server = mock_server(responses).await;
    let mut bounded = limits();
    bounded.max_records = 1;
    let client =
        StripeReadClient::for_test(API_KEY, &config(), server.origin.clone(), bounded).unwrap();
    let mut session = client.session();
    assert!(matches!(
        client
            .subscription_invoices(&mut session, "sub_1", None)
            .await,
        Err(StripeReadError::RecordBoundExceeded)
    ));

    let mut responses = HashMap::new();
    responses.insert("/v1/account".into(), vec![MockResponse::json(account())]);
    responses.insert(
        "/v1/subscriptions/sub_1".into(),
        vec![MockResponse::json(json!({
            "id":"sub_1","customer":"cus_1","status":"active","livemode":false
        }))],
    );
    let server = mock_server(responses).await;
    let mut bounded = limits();
    bounded.max_requests = 1;
    let client =
        StripeReadClient::for_test(API_KEY, &config(), server.origin.clone(), bounded).unwrap();
    let mut session = client.session();
    client.account(&mut session).await.unwrap();
    assert!(matches!(
        client.subscription(&mut session, "sub_1", "cus_1").await,
        Err(StripeReadError::RequestBoundExceeded)
    ));
}

fn correction_responses(
    path: &str,
    pages: Vec<MockResponse>,
) -> HashMap<String, Vec<MockResponse>> {
    let mut responses = HashMap::new();
    responses.insert("/v1/account".into(), vec![MockResponse::json(account())]);
    responses.insert(path.into(), pages);
    responses
}

fn query_pairs(call: &Call) -> Vec<(String, String)> {
    Url::parse(&format!("http://loopback{}", call.path_and_query))
        .unwrap()
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect()
}

/// Every request to `path` must carry exactly the parent filter, the page size and the expected
/// cursor, so a filter dropped after the first page fails here rather than widening the read.
fn assert_filtered_pages(
    server: &MockServer,
    path: &str,
    filter: (&str, &str),
    cursors: &[Option<&str>],
) {
    let calls = server.state.calls.lock().unwrap();
    assert_eq!(
        calls.first().map(|call| call.path_and_query.as_str()),
        Some("/v1/account"),
        "the account must be verified before any correction read"
    );
    let pages: Vec<&Call> = calls
        .iter()
        .filter(|call| call.path_and_query.split('?').next() == Some(path))
        .collect();
    assert_eq!(pages.len(), cursors.len(), "{pages:?}");
    for (call, cursor) in pages.iter().zip(cursors) {
        let mut expected = vec![
            (filter.0.to_owned(), filter.1.to_owned()),
            ("limit".to_owned(), "100".to_owned()),
        ];
        if let Some(cursor) = cursor {
            expected.push(("starting_after".to_owned(), (*cursor).to_owned()));
        }
        assert_eq!(query_pairs(call), expected, "{}", call.path_and_query);
        assert_eq!(
            call.authorization.as_deref(),
            Some("Bearer sk_test_transport")
        );
        assert_eq!(call.version.as_deref(), Some("2026-07-29.dahlia"));
    }
}

/// Apply `(field, value)` edits to a fixture. `None` removes the field.
fn with_fields(mut resource: Value, edits: &[(&str, Option<Value>)]) -> Value {
    for (field, value) in edits {
        match value {
            Some(value) => resource[*field] = value.clone(),
            None => {
                resource.as_object_mut().unwrap().remove(*field);
            }
        }
    }
    resource
}

fn refund(id: &str, status: Value) -> Value {
    json!({
        "id":id,
        "object":"refund",
        "amount":299,
        "balance_transaction":"txn_1",
        "charge":"ch_1",
        "created":1_700_000_100,
        "currency":"gbp",
        "metadata":{},
        "payment_intent":"pi_1",
        "reason":null,
        "receipt_number":null,
        "status":status
    })
}

async fn read_refunds(
    pages: Vec<MockResponse>,
) -> Result<Vec<StripeRefundResource>, StripeReadError> {
    let server = mock_server(correction_responses("/v1/refunds", pages)).await;
    let client =
        StripeReadClient::for_test(API_KEY, &config(), server.origin.clone(), limits()).unwrap();
    let mut session = client.session();
    client.payment_intent_refunds(&mut session, "pi_1").await
}

#[tokio::test]
async fn reads_every_refund_page_with_its_payment_intent_filter() {
    let expanded = with_fields(
        refund("re_2", json!("requires_action")),
        &[
            (
                "payment_intent",
                Some(json!({"id":"pi_1","object":"payment_intent"})),
            ),
            ("charge", Some(json!({"id":"ch_1","object":"charge"}))),
        ],
    );
    let unlinked = with_fields(
        refund("re_6", Value::Null),
        &[
            ("payment_intent", Some(Value::Null)),
            ("charge", Some(Value::Null)),
        ],
    );
    let server = mock_server(correction_responses(
        "/v1/refunds",
        vec![
            MockResponse::json(list(
                vec![
                    refund("re_1", json!("pending")),
                    expanded,
                    refund("re_3", json!("succeeded")),
                ],
                true,
            )),
            MockResponse::json(list(
                vec![
                    refund("re_4", json!("failed")),
                    refund("re_5", json!("canceled")),
                    unlinked,
                    refund("re_7", json!("returned_by_bank")),
                ],
                false,
            )),
        ],
    ))
    .await;
    let client =
        StripeReadClient::for_test(API_KEY, &config(), server.origin.clone(), limits()).unwrap();
    let mut session = client.session();

    let refunds = client
        .payment_intent_refunds(&mut session, "pi_1")
        .await
        .unwrap();

    assert_eq!(
        refunds
            .iter()
            .map(|refund| refund.status.clone())
            .collect::<Vec<_>>(),
        vec![
            Some(StripeRefundStatus::Pending),
            Some(StripeRefundStatus::RequiresAction),
            Some(StripeRefundStatus::Succeeded),
            Some(StripeRefundStatus::Failed),
            Some(StripeRefundStatus::Canceled),
            None,
            Some(StripeRefundStatus::Unknown("returned_by_bank".into())),
        ]
    );
    assert_eq!(
        refunds[1],
        StripeRefundResource {
            id: "re_2".into(),
            payment_intent_id: Some("pi_1".into()),
            charge_id: Some("ch_1".into()),
            amount: 299,
            currency: "gbp".into(),
            created: 1_700_000_100,
            status: Some(StripeRefundStatus::RequiresAction),
            livemode: None,
        }
    );
    // Stripe may omit the parent; the filter must not be copied into the evidence as proof.
    assert_eq!(refunds[5].payment_intent_id, None);
    assert_eq!(refunds[5].charge_id, None);
    assert_filtered_pages(
        &server,
        "/v1/refunds",
        ("payment_intent", "pi_1"),
        &[None, Some("re_3")],
    );
}

#[tokio::test]
async fn an_empty_refund_list_is_not_a_failed_refund_read() {
    let server = mock_server(correction_responses(
        "/v1/refunds",
        vec![MockResponse::json(list(Vec::new(), false))],
    ))
    .await;
    let client =
        StripeReadClient::for_test(API_KEY, &config(), server.origin.clone(), limits()).unwrap();
    let mut session = client.session();
    assert_eq!(
        client
            .payment_intent_refunds(&mut session, "pi_1")
            .await
            .unwrap(),
        Vec::new()
    );
    assert_filtered_pages(&server, "/v1/refunds", ("payment_intent", "pi_1"), &[None]);

    assert!(matches!(
        read_refunds(vec![MockResponse::status(StatusCode::NOT_FOUND, "{}")]).await,
        Err(StripeReadError::ResourceMissing)
    ));
    assert!(matches!(
        read_refunds(vec![MockResponse::status(StatusCode::FORBIDDEN, "{}")]).await,
        Err(StripeReadError::Permission { status: 403 })
    ));
    assert!(matches!(
        read_refunds(vec![MockResponse::status(StatusCode::UNAUTHORIZED, "{}")]).await,
        Err(StripeReadError::Authentication { status: 401 })
    ));

    // An account that cannot be verified stops the read before any refund request is made.
    let mut responses = correction_responses(
        "/v1/refunds",
        vec![MockResponse::json(list(Vec::new(), false))],
    );
    responses.insert(
        "/v1/account".into(),
        vec![MockResponse::status(StatusCode::FORBIDDEN, "{}")],
    );
    let server = mock_server(responses).await;
    let client =
        StripeReadClient::for_test(API_KEY, &config(), server.origin.clone(), limits()).unwrap();
    let mut session = client.session();
    assert!(matches!(
        client.payment_intent_refunds(&mut session, "pi_1").await,
        Err(StripeReadError::Permission { status: 403 })
    ));
    assert!(server
        .state
        .calls
        .lock()
        .unwrap()
        .iter()
        .all(|call| !call.path_and_query.starts_with("/v1/refunds")));
}

#[tokio::test]
async fn rejects_refunds_that_contradict_their_parent_or_mode() {
    for payment_intent in [json!("pi_other"), json!({"id":"pi_other"})] {
        let contradictory = with_fields(
            refund("re_1", json!("succeeded")),
            &[("payment_intent", Some(payment_intent))],
        );
        assert!(matches!(
            read_refunds(vec![MockResponse::json(list(vec![contradictory], false))]).await,
            Err(StripeReadError::ParentMismatch)
        ));
    }

    let live = with_fields(
        refund("re_1", json!("succeeded")),
        &[("livemode", Some(json!(true)))],
    );
    assert!(matches!(
        read_refunds(vec![MockResponse::json(list(vec![live], false))]).await,
        Err(StripeReadError::ContextMismatch)
    ));

    let test_mode = with_fields(
        refund("re_1", json!("succeeded")),
        &[("livemode", Some(json!(false)))],
    );
    let refunds = read_refunds(vec![MockResponse::json(list(vec![test_mode], false))])
        .await
        .unwrap();
    assert_eq!(refunds[0].livemode, Some(false));
}

#[tokio::test]
async fn rejects_malformed_refund_fields_by_name() {
    let cases: Vec<(&str, Option<Value>, &str)> = vec![
        ("id", None, "list.data.id"),
        ("object", None, "refund.object"),
        ("object", Some(json!("charge")), "refund.object"),
        ("amount", None, "refund.amount"),
        ("amount", Some(Value::Null), "refund.amount"),
        ("amount", Some(json!("299")), "refund.amount"),
        ("amount", Some(json!(2.5)), "refund.amount"),
        ("amount", Some(json!(-1)), "refund.amount"),
        ("created", None, "refund.created"),
        ("created", Some(json!(-1)), "refund.created"),
        ("currency", None, "refund.currency"),
        ("currency", Some(json!("GBP")), "refund.currency"),
        ("payment_intent", Some(json!({})), "refund.payment_intent"),
        ("payment_intent", Some(json!("")), "refund.payment_intent"),
        ("charge", Some(json!(7)), "refund.charge"),
        ("status", Some(json!("")), "refund.status"),
        ("status", Some(json!(3)), "refund.status"),
        (
            "status",
            Some(json!("Refunded after a support call")),
            "refund.status",
        ),
        ("livemode", Some(json!("false")), "refund.livemode"),
    ];
    for (field, value, expected) in cases {
        let malformed = with_fields(refund("re_1", json!("succeeded")), &[(field, value)]);
        let result = read_refunds(vec![MockResponse::json(list(vec![malformed], false))]).await;
        assert!(
            matches!(result, Err(StripeReadError::MalformedResponse(name)) if name == expected),
            "{field}: {result:?}"
        );
    }

    // Zero is a value Stripe sent, not a stand-in for a missing amount.
    let zero = with_fields(
        refund("re_1", json!("canceled")),
        &[("amount", Some(json!(0)))],
    );
    let refunds = read_refunds(vec![MockResponse::json(list(vec![zero], false))])
        .await
        .unwrap();
    assert_eq!(refunds[0].amount, 0);
}

#[tokio::test]
async fn refund_reads_keep_metadata_and_free_text_out_of_evidence() {
    let sensitive = with_fields(
        refund("re_1", json!("succeeded")),
        &[
            ("metadata", Some(json!({"note":"sentinel-metadata"}))),
            ("description", Some(json!("sentinel-description"))),
            ("reason", Some(json!("requested_by_customer"))),
            ("receipt_number", Some(json!("sentinel-receipt"))),
            (
                "instructions_email",
                Some(json!("sentinel@example.invalid")),
            ),
            (
                "destination_details",
                Some(json!({"type":"card","card":{"reference":"sentinel-reference"}})),
            ),
        ],
    );
    let refunds = read_refunds(vec![MockResponse::json(list(
        vec![sensitive.clone()],
        false,
    ))])
    .await
    .unwrap();
    let rendered = format!("{refunds:?}");
    assert!(!rendered.contains("sentinel"), "{rendered}");
    assert!(!rendered.contains("requested_by_customer"), "{rendered}");

    let contradictory = with_fields(sensitive, &[("payment_intent", Some(json!("pi_other")))]);
    let error = read_refunds(vec![MockResponse::json(list(vec![contradictory], false))])
        .await
        .unwrap_err();
    let rendered = format!("{error:?} {error}");
    assert!(!rendered.contains("sentinel"), "{rendered}");
    assert!(!rendered.contains(API_KEY), "{rendered}");
}
