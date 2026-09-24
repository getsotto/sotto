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
use sotto_server::cloud_provider::ProviderEnvironment;
use sotto_server::cloud_provider_stripe::StripeCoverageConfig;
use sotto_server::cloud_provider_stripe_http::{
    StripeReadClient, StripeReadError, StripeReadLimits,
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
        }
    }

    fn status(status: StatusCode, body: &str) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.into(),
        }
    }

    fn redirect(target: &str) -> Self {
        Self {
            status: StatusCode::FOUND,
            headers: vec![("location", target.into())],
            body: String::new(),
        }
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
    builder.body(Body::from(response.body)).unwrap()
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
            MockResponse::json(list(vec![json!({"id":"in_1","customer":"cus_1"})], true)),
            MockResponse::json(list(vec![json!({"id":"in_2","customer":"cus_1"})], false)),
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
