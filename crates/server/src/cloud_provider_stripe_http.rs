//! Bounded, authenticated Stripe reads for the coverage adapter.
//!
//! This module owns HTTP transport and pagination only. It does not interpret financial history,
//! persist coverage, or claim that a completed remote enumeration is a consistent Stripe snapshot.

use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::{Method, Response, Url};
use serde_json::Value;
use thiserror::Error;
use tokio::time::{sleep, timeout};

use crate::billing::STRIPE_API_VERSION;
use crate::cloud_provider::ProviderEnvironment;
use crate::cloud_provider_stripe::{
    decode_invoice_payment, StripeContractError, StripeCoverageConfig,
};

const STRIPE_ORIGIN: &str = "https://api.stripe.com/";
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_SESSION_TIMEOUT: Duration = Duration::from_secs(120);
const DEFAULT_RESPONSE_BYTES: usize = 1024 * 1024;
const DEFAULT_TOTAL_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_MAX_PAGES: usize = 64;
const DEFAULT_MAX_REQUESTS: usize = 256;
const DEFAULT_MAX_RECORDS: usize = 10_000;
const DEFAULT_MAX_RETRIES: usize = 2;
const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StripeReadLimits {
    pub request_timeout: Duration,
    pub session_timeout: Duration,
    pub max_response_bytes: usize,
    pub max_total_response_bytes: usize,
    pub max_pages: usize,
    pub max_requests: usize,
    pub max_records: usize,
    pub max_retries: usize,
    pub max_retry_after: Duration,
}

impl Default for StripeReadLimits {
    fn default() -> Self {
        Self {
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            session_timeout: DEFAULT_SESSION_TIMEOUT,
            max_response_bytes: DEFAULT_RESPONSE_BYTES,
            max_total_response_bytes: DEFAULT_TOTAL_BYTES,
            max_pages: DEFAULT_MAX_PAGES,
            max_requests: DEFAULT_MAX_REQUESTS,
            max_records: DEFAULT_MAX_RECORDS,
            max_retries: DEFAULT_MAX_RETRIES,
            max_retry_after: DEFAULT_RETRY_AFTER,
        }
    }
}

impl StripeReadLimits {
    fn validate(self) -> Result<Self, StripeReadError> {
        if self.request_timeout.is_zero()
            || self.session_timeout.is_zero()
            || self.max_response_bytes == 0
            || self.max_total_response_bytes == 0
            || self.max_pages == 0
            || self.max_requests == 0
            || self.max_records == 0
        {
            return Err(StripeReadError::InvalidConfig(
                "read limits must be nonzero",
            ));
        }
        Ok(self)
    }
}

#[derive(Debug, Error)]
pub enum StripeReadError {
    #[error("invalid Stripe read configuration: {0}")]
    InvalidConfig(&'static str),
    #[error("Stripe read identifier is invalid")]
    InvalidIdentifier,
    #[error("Stripe account identity did not match the configured account")]
    AccountMismatch,
    #[error("Stripe resource mode did not match the configured environment")]
    ContextMismatch,
    #[error("Stripe authentication failed with status {status}")]
    Authentication { status: u16 },
    #[error("Stripe permission was denied with status {status}")]
    Permission { status: u16 },
    #[error("Stripe read session belongs to another client")]
    SessionClientMismatch,
    #[error("Stripe resource was not found")]
    ResourceMissing,
    #[error("Stripe rate limit was reached")]
    RateLimited { retry_after: Option<Duration> },
    #[error("Stripe returned a retryable status {status}")]
    Retryable { status: u16 },
    #[error("Stripe returned an unexpected status {status}")]
    UnexpectedStatus { status: u16 },
    #[error("Stripe transport failed")]
    Transport,
    #[error("Stripe request timed out")]
    Timeout,
    #[error("Stripe redirect was rejected")]
    RedirectRejected,
    #[error("Stripe response was malformed: {0}")]
    MalformedResponse(&'static str),
    #[error("Stripe response exceeded its per-response byte bound")]
    ResponseTooLarge,
    #[error("Stripe session exceeded its cumulative byte bound")]
    SessionBytesExceeded,
    #[error("Stripe session exceeded its request bound")]
    RequestBoundExceeded,
    #[error("Stripe session exceeded its page bound")]
    PageBoundExceeded,
    #[error("Stripe session exceeded its record bound")]
    RecordBoundExceeded,
    #[error("Stripe pagination is invalid: {0}")]
    InvalidPagination(&'static str),
    #[error("Stripe payment settlement was not valid: {0}")]
    Settlement(#[source] StripeContractError),
}

impl StripeReadError {
    fn retryable(&self) -> bool {
        matches!(
            self,
            Self::Transport | Self::Timeout | Self::RateLimited { .. } | Self::Retryable { .. }
        )
    }

    fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::RateLimited { retry_after } => *retry_after,
            _ => None,
        }
    }
}

#[derive(Clone)]
pub struct StripeReadClient {
    http: reqwest::Client,
    api_key: String,
    account_id: String,
    environment: ProviderEnvironment,
    api_version: &'static str,
    origin: Url,
    limits: StripeReadLimits,
    identity: Arc<()>,
}

impl fmt::Debug for StripeReadClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StripeReadClient")
            .field("api_key", &"<redacted>")
            .field("account_id", &self.account_id)
            .field("environment", &self.environment)
            .field("api_version", &self.api_version)
            .field("origin", &self.origin)
            .field("limits", &self.limits)
            .finish()
    }
}

impl StripeReadClient {
    pub fn new(
        api_key: impl Into<String>,
        config: &StripeCoverageConfig,
        limits: StripeReadLimits,
    ) -> Result<Self, StripeReadError> {
        Self::build(
            api_key.into(),
            config,
            limits,
            Url::parse(STRIPE_ORIGIN).unwrap(),
        )
    }

    /// Test-only origin injection. Production callers must use [`Self::new`].
    #[doc(hidden)]
    pub fn for_test(
        api_key: impl Into<String>,
        config: &StripeCoverageConfig,
        origin: Url,
        limits: StripeReadLimits,
    ) -> Result<Self, StripeReadError> {
        if origin.scheme() != "http"
            || origin.path() != "/"
            || origin.query().is_some()
            || origin.fragment().is_some()
            || !origin.host_str().is_some_and(|host| {
                host == "localhost"
                    || host
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback())
            })
        {
            return Err(StripeReadError::InvalidConfig(
                "test Stripe origin must be a loopback HTTP origin",
            ));
        }
        Self::build(api_key.into(), config, limits, origin)
    }

    fn build(
        api_key: String,
        config: &StripeCoverageConfig,
        limits: StripeReadLimits,
        origin: Url,
    ) -> Result<Self, StripeReadError> {
        if api_key.trim().is_empty() {
            return Err(StripeReadError::InvalidConfig("Stripe API key"));
        }
        let limits = limits.validate()?;
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(limits.request_timeout)
            .build()
            .map_err(|_| StripeReadError::InvalidConfig("Stripe HTTP client"))?;
        Ok(Self {
            http,
            api_key,
            account_id: config.account_id.clone(),
            environment: config.environment,
            api_version: STRIPE_API_VERSION,
            origin,
            limits,
            identity: Arc::new(()),
        })
    }

    pub fn session(&self) -> StripeReadSession {
        StripeReadSession {
            deadline: Instant::now() + self.limits.session_timeout,
            attempts: 0,
            bytes: 0,
            records: 0,
            account_verified: false,
            identity: Arc::clone(&self.identity),
        }
    }

    pub async fn account(
        &self,
        session: &mut StripeReadSession,
    ) -> Result<StripeAccountResource, StripeReadError> {
        self.ensure_account(session).await
    }

    async fn ensure_account(
        &self,
        session: &mut StripeReadSession,
    ) -> Result<StripeAccountResource, StripeReadError> {
        if !Arc::ptr_eq(&self.identity, &session.identity) {
            return Err(StripeReadError::SessionClientMismatch);
        }
        session.remaining()?;
        if session.account_verified {
            return Ok(StripeAccountResource {
                id: self.account_id.clone(),
                livemode: matches!(self.environment, ProviderEnvironment::Live),
            });
        }
        let value = self
            .request_json(session, Method::GET, "v1/account", &[])
            .await?;
        let id = required_id(&value, "account.id")?;
        if id != self.account_id {
            return Err(StripeReadError::AccountMismatch);
        }
        let livemode = value
            .get("livemode")
            .and_then(Value::as_bool)
            .ok_or(StripeReadError::MalformedResponse("account.livemode"))?;
        if livemode != matches!(self.environment, ProviderEnvironment::Live) {
            return Err(StripeReadError::ContextMismatch);
        }
        session.account_verified = true;
        Ok(StripeAccountResource { id, livemode })
    }

    pub async fn subscription(
        &self,
        session: &mut StripeReadSession,
        subscription_id: &str,
        customer_id: &str,
    ) -> Result<StripeSubscriptionResource, StripeReadError> {
        self.ensure_account(session).await?;
        validate_identifier(subscription_id)?;
        validate_identifier(customer_id)?;
        let path = format!("v1/subscriptions/{subscription_id}");
        let value = self.request_json(session, Method::GET, &path, &[]).await?;
        let resource = parse_subscription(&value)?;
        if resource.id != subscription_id || resource.customer_id.as_deref() != Some(customer_id) {
            return Err(StripeReadError::ContextMismatch);
        }
        validate_mode(resource.livemode, self.environment)?;
        Ok(resource)
    }

    pub async fn invoice(
        &self,
        session: &mut StripeReadSession,
        invoice_id: &str,
    ) -> Result<StripeInvoiceResource, StripeReadError> {
        self.ensure_account(session).await?;
        validate_identifier(invoice_id)?;
        let path = format!("v1/invoices/{invoice_id}");
        let value = self.request_json(session, Method::GET, &path, &[]).await?;
        let resource = parse_invoice(&value)?;
        if resource.id != invoice_id {
            return Err(StripeReadError::ContextMismatch);
        }
        validate_mode(resource.livemode, self.environment)?;
        Ok(resource)
    }

    pub async fn subscription_invoices(
        &self,
        session: &mut StripeReadSession,
        subscription_id: &str,
        customer_id: Option<&str>,
    ) -> Result<Vec<StripeInvoiceResource>, StripeReadError> {
        self.ensure_account(session).await?;
        validate_identifier(subscription_id)?;
        if let Some(customer_id) = customer_id {
            validate_identifier(customer_id)?;
        }
        let mut query = vec![("subscription".to_owned(), subscription_id.to_owned())];
        if let Some(customer_id) = customer_id {
            query.push(("customer".to_owned(), customer_id.to_owned()));
        }
        let invoices = self
            .list(session, "v1/invoices", query, parse_invoice)
            .await?;
        for invoice in &invoices {
            if invoice
                .subscription_id
                .as_deref()
                .is_some_and(|id| id != subscription_id)
            {
                return Err(StripeReadError::ContextMismatch);
            }
            if customer_id.is_some_and(|customer_id| {
                invoice
                    .customer_id
                    .as_deref()
                    .is_some_and(|id| id != customer_id)
            }) {
                return Err(StripeReadError::ContextMismatch);
            }
            validate_mode(invoice.livemode, self.environment)?;
        }
        Ok(invoices)
    }

    pub async fn invoice_lines(
        &self,
        session: &mut StripeReadSession,
        invoice_id: &str,
    ) -> Result<Vec<StripeInvoiceLineResource>, StripeReadError> {
        self.ensure_account(session).await?;
        validate_identifier(invoice_id)?;
        let path = format!("v1/invoices/{invoice_id}/lines");
        self.list(session, &path, Vec::new(), parse_invoice_line)
            .await
    }

    pub async fn invoice_payments(
        &self,
        session: &mut StripeReadSession,
        invoice_id: &str,
    ) -> Result<Vec<StripeInvoicePaymentResource>, StripeReadError> {
        self.ensure_account(session).await?;
        validate_identifier(invoice_id)?;
        let query = vec![("invoice".to_owned(), invoice_id.to_owned())];
        let payments = self
            .list(session, "v1/invoice_payments", query, parse_invoice_payment)
            .await?;
        for payment in &payments {
            if payment.invoice_id != invoice_id {
                return Err(StripeReadError::ContextMismatch);
            }
            validate_mode(payment.livemode, self.environment)?;
        }
        Ok(payments)
    }

    async fn list<T, F>(
        &self,
        session: &mut StripeReadSession,
        path: &str,
        base_query: Vec<(String, String)>,
        parse: F,
    ) -> Result<Vec<T>, StripeReadError>
    where
        F: Fn(&Value) -> Result<T, StripeReadError> + Copy,
    {
        let mut output = Vec::new();
        let mut ids = HashSet::new();
        let mut cursor: Option<String> = None;
        let mut pages = 0usize;
        loop {
            if pages == self.limits.max_pages {
                return Err(StripeReadError::PageBoundExceeded);
            }
            let mut query = base_query.clone();
            query.push(("limit".to_owned(), "100".to_owned()));
            if let Some(cursor) = cursor.as_deref() {
                query.push(("starting_after".to_owned(), cursor.to_owned()));
            }
            let page = self
                .request_json(session, Method::GET, path, &query)
                .await?;
            if page.get("object").and_then(Value::as_str) != Some("list") {
                return Err(StripeReadError::MalformedResponse("list.object"));
            }
            let has_more = page
                .get("has_more")
                .and_then(Value::as_bool)
                .ok_or(StripeReadError::MalformedResponse("list.has_more"))?;
            let data = page
                .get("data")
                .and_then(Value::as_array)
                .ok_or(StripeReadError::MalformedResponse("list.data"))?;
            if data.is_empty() && has_more {
                return Err(StripeReadError::InvalidPagination(
                    "empty page reported with more results",
                ));
            }
            session.add_records(data.len(), self.limits.max_records)?;
            let mut parsed = Vec::with_capacity(data.len());
            for item in data {
                let id = required_id(item, "list.data.id")?;
                if !ids.insert(id.clone()) {
                    return Err(StripeReadError::InvalidPagination("duplicate record id"));
                }
                parsed.push((id, parse(item)?));
            }
            let last_id = parsed.last().map(|(id, _)| id.clone());
            output.extend(parsed.into_iter().map(|(_, value)| value));
            pages += 1;
            if !has_more {
                return Ok(output);
            }
            let Some(last_id) = last_id else {
                return Err(StripeReadError::InvalidPagination("missing next cursor"));
            };
            if cursor.as_deref() == Some(last_id.as_str()) {
                return Err(StripeReadError::InvalidPagination("cursor did not advance"));
            }
            cursor = Some(last_id);
        }
    }

    async fn request_json(
        &self,
        session: &mut StripeReadSession,
        method: Method,
        path: &str,
        query: &[(String, String)],
    ) -> Result<Value, StripeReadError> {
        let mut retries = 0usize;
        loop {
            let remaining = session.remaining()?;
            let request = self.execute_once(session, &method, path, query);
            let result = timeout(remaining.min(self.limits.request_timeout), request).await;
            let result = match result {
                Ok(result) => result,
                Err(_) => Err(StripeReadError::Timeout),
            };
            match result {
                Ok(value) => return Ok(value),
                Err(error) if error.retryable() && retries < self.limits.max_retries => {
                    let backoff = error
                        .retry_after()
                        .unwrap_or_else(|| {
                            Duration::from_millis(
                                50u64.saturating_mul(2u64.saturating_pow(retries.min(63) as u32)),
                            )
                        })
                        .min(self.limits.max_retry_after);
                    retries += 1;
                    if !backoff.is_zero() {
                        timeout(session.remaining()?, sleep(backoff))
                            .await
                            .map_err(|_| StripeReadError::Timeout)?;
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn execute_once(
        &self,
        session: &mut StripeReadSession,
        method: &Method,
        path: &str,
        query: &[(String, String)],
    ) -> Result<Value, StripeReadError> {
        session.add_attempt(self.limits.max_requests)?;
        let url = self
            .origin
            .join(path)
            .map_err(|_| StripeReadError::InvalidConfig("Stripe resource path"))?;
        let mut request = self
            .http
            .request(method.clone(), url)
            .bearer_auth(&self.api_key)
            .header("Stripe-Version", self.api_version)
            .header(reqwest::header::ACCEPT, "application/json");
        if !query.is_empty() {
            request = request.query(query);
        }
        let response = request.send().await.map_err(map_request_error)?;
        self.read_response(session, response).await
    }

    async fn read_response(
        &self,
        session: &mut StripeReadSession,
        response: Response,
    ) -> Result<Value, StripeReadError> {
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_secs);
        if response
            .content_length()
            .is_some_and(|length| length > self.limits.max_response_bytes as u64)
        {
            return Err(StripeReadError::ResponseTooLarge);
        }
        let mut body = Vec::new();
        let mut response = response;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| StripeReadError::Transport)?
        {
            session.add_bytes(chunk.len(), self.limits.max_total_response_bytes)?;
            if body.len().saturating_add(chunk.len()) > self.limits.max_response_bytes {
                return Err(StripeReadError::ResponseTooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        if !(200..300).contains(&status) {
            return Err(match status {
                300..=399 => StripeReadError::RedirectRejected,
                401 => StripeReadError::Authentication { status },
                403 => StripeReadError::Permission { status },
                404 => StripeReadError::ResourceMissing,
                429 => StripeReadError::RateLimited { retry_after },
                500..=599 => StripeReadError::Retryable { status },
                _ => StripeReadError::UnexpectedStatus { status },
            });
        }
        serde_json::from_slice(&body).map_err(|_| StripeReadError::MalformedResponse("json"))
    }
}

pub struct StripeReadSession {
    deadline: Instant,
    attempts: usize,
    bytes: usize,
    records: usize,
    account_verified: bool,
    identity: Arc<()>,
}

impl fmt::Debug for StripeReadSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StripeReadSession")
            .field("attempts", &self.attempts)
            .field("bytes", &self.bytes)
            .field("records", &self.records)
            .field("account_verified", &self.account_verified)
            .finish()
    }
}

impl StripeReadSession {
    fn remaining(&self) -> Result<Duration, StripeReadError> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or(StripeReadError::Timeout)
    }

    fn add_attempt(&mut self, limit: usize) -> Result<(), StripeReadError> {
        self.attempts = self
            .attempts
            .checked_add(1)
            .ok_or(StripeReadError::RequestBoundExceeded)?;
        if self.attempts > limit {
            return Err(StripeReadError::RequestBoundExceeded);
        }
        Ok(())
    }

    fn add_bytes(&mut self, bytes: usize, limit: usize) -> Result<(), StripeReadError> {
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .ok_or(StripeReadError::SessionBytesExceeded)?;
        if self.bytes > limit {
            return Err(StripeReadError::SessionBytesExceeded);
        }
        Ok(())
    }

    fn add_records(&mut self, records: usize, limit: usize) -> Result<(), StripeReadError> {
        self.records = self
            .records
            .checked_add(records)
            .ok_or(StripeReadError::RecordBoundExceeded)?;
        if self.records > limit {
            return Err(StripeReadError::RecordBoundExceeded);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeAccountResource {
    pub id: String,
    pub livemode: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeSubscriptionResource {
    pub id: String,
    pub customer_id: Option<String>,
    pub status: Option<String>,
    pub livemode: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeInvoiceResource {
    pub id: String,
    pub customer_id: Option<String>,
    pub subscription_id: Option<String>,
    pub status: Option<String>,
    pub currency: Option<String>,
    pub amount_paid: Option<i64>,
    pub amount_due: Option<i64>,
    pub livemode: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeInvoiceLineResource {
    pub id: String,
    pub quantity: Option<i64>,
    pub subscription_id: Option<String>,
    pub subscription_item_id: Option<String>,
    pub price_id: Option<String>,
    pub period_start: Option<i64>,
    pub period_end: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeInvoicePaymentResource {
    pub id: String,
    pub invoice_id: String,
    pub status: String,
    pub amount_paid: i64,
    pub amount_requested: i64,
    pub currency: String,
    pub livemode: Option<bool>,
    pub payment_type: String,
    pub payment_intent_id: Option<String>,
}

impl StripeInvoicePaymentResource {
    pub fn settlement(
        &self,
        config: &StripeCoverageConfig,
    ) -> Result<crate::cloud_provider_stripe::StripePaymentSettlement, StripeReadError> {
        let payload = serde_json::json!({
            "object": "invoice_payment",
            "id": self.id,
            "invoice": self.invoice_id,
            "status": self.status,
            "amount_paid": self.amount_paid,
            "amount_requested": self.amount_requested,
            "currency": self.currency,
            "livemode": self.livemode,
            "payment": {
                "type": self.payment_type,
                "payment_intent": self.payment_intent_id,
            },
        });
        decode_invoice_payment(
            &serde_json::to_vec(&payload)
                .map_err(|_| StripeReadError::MalformedResponse("settlement"))?,
            config,
        )
        .map_err(StripeReadError::Settlement)
    }
}

fn parse_subscription(value: &Value) -> Result<StripeSubscriptionResource, StripeReadError> {
    Ok(StripeSubscriptionResource {
        id: required_id(value, "subscription.id")?,
        customer_id: optional_validated_ref(value.get("customer"), "subscription.customer")?,
        status: value
            .get("status")
            .and_then(Value::as_str)
            .map(str::to_owned),
        livemode: value.get("livemode").and_then(Value::as_bool),
    })
}

fn parse_invoice(value: &Value) -> Result<StripeInvoiceResource, StripeReadError> {
    Ok(StripeInvoiceResource {
        id: required_id(value, "invoice.id")?,
        customer_id: optional_validated_ref(value.get("customer"), "invoice.customer")?,
        subscription_id: optional_validated_ref(value.get("subscription"), "invoice.subscription")?,
        status: value
            .get("status")
            .and_then(Value::as_str)
            .map(str::to_owned),
        currency: value
            .get("currency")
            .and_then(Value::as_str)
            .map(str::to_owned),
        amount_paid: value.get("amount_paid").and_then(Value::as_i64),
        amount_due: value.get("amount_due").and_then(Value::as_i64),
        livemode: value.get("livemode").and_then(Value::as_bool),
    })
}

fn parse_invoice_line(value: &Value) -> Result<StripeInvoiceLineResource, StripeReadError> {
    let parent = value.get("parent").and_then(Value::as_object);
    let details = parent
        .and_then(|parent| parent.get("subscription_item_details"))
        .and_then(Value::as_object);
    let pricing = value.get("pricing").and_then(Value::as_object);
    let price_details = pricing
        .and_then(|pricing| pricing.get("price_details"))
        .and_then(Value::as_object);
    let period = value.get("period").and_then(Value::as_object);
    Ok(StripeInvoiceLineResource {
        id: required_id(value, "invoice line.id")?,
        quantity: value.get("quantity").and_then(Value::as_i64),
        subscription_id: details
            .map(|details| optional_validated_ref(details.get("subscription"), "line.subscription"))
            .transpose()?
            .flatten(),
        subscription_item_id: details
            .map(|details| {
                optional_validated_ref(details.get("subscription_item"), "line.subscription_item")
            })
            .transpose()?
            .flatten(),
        price_id: price_details
            .map(|details| optional_validated_ref(details.get("price"), "line.price"))
            .transpose()?
            .flatten(),
        period_start: period.and_then(|period| period.get("start").and_then(Value::as_i64)),
        period_end: period.and_then(|period| period.get("end").and_then(Value::as_i64)),
    })
}

fn parse_invoice_payment(value: &Value) -> Result<StripeInvoicePaymentResource, StripeReadError> {
    let payment = value.get("payment").and_then(Value::as_object).ok_or(
        StripeReadError::MalformedResponse("invoice payment.payment"),
    )?;
    Ok(StripeInvoicePaymentResource {
        id: required_id(value, "invoice payment.id")?,
        invoice_id: required_ref(value, "invoice")?,
        status: required_string(value, "status")?,
        amount_paid: required_i64(value, "amount_paid")?,
        amount_requested: required_i64(value, "amount_requested")?,
        currency: required_string(value, "currency")?,
        livemode: value.get("livemode").and_then(Value::as_bool),
        payment_type: required_string_object(payment, "type", "payment.type")?,
        payment_intent_id: optional_validated_ref(
            payment.get("payment_intent"),
            "payment.payment_intent",
        )?,
    })
}

fn validate_mode(
    mode: Option<bool>,
    environment: ProviderEnvironment,
) -> Result<(), StripeReadError> {
    match mode {
        Some(mode) if mode == matches!(environment, ProviderEnvironment::Live) => Ok(()),
        Some(_) => Err(StripeReadError::ContextMismatch),
        None => Err(StripeReadError::MalformedResponse("livemode")),
    }
}

fn required_id(value: &Value, field: &'static str) -> Result<String, StripeReadError> {
    let id = optional_ref(value.get("id")).ok_or(StripeReadError::MalformedResponse(field))?;
    validate_identifier(&id)?;
    Ok(id)
}

fn required_ref(value: &Value, field: &'static str) -> Result<String, StripeReadError> {
    let reference = optional_ref(Some(
        value
            .get(field)
            .ok_or(StripeReadError::MalformedResponse(field))?,
    ))
    .ok_or(StripeReadError::MalformedResponse(field))?;
    validate_identifier(&reference)?;
    Ok(reference)
}

fn optional_ref(value: Option<&Value>) -> Option<String> {
    value
        .and_then(|value| {
            value
                .as_str()
                .or_else(|| value.get("id").and_then(Value::as_str))
        })
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
}

fn optional_validated_ref(
    value: Option<&Value>,
    field: &'static str,
) -> Result<Option<String>, StripeReadError> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let reference = value
        .as_str()
        .or_else(|| value.get("id").and_then(Value::as_str))
        .filter(|reference| !reference.trim().is_empty())
        .ok_or(StripeReadError::MalformedResponse(field))?;
    validate_identifier(reference).map_err(|_| StripeReadError::MalformedResponse(field))?;
    Ok(Some(reference.to_owned()))
}

fn required_string(value: &Value, field: &'static str) -> Result<String, StripeReadError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .ok_or(StripeReadError::MalformedResponse(field))
}

fn required_string_object(
    value: &serde_json::Map<String, Value>,
    field: &'static str,
    name: &'static str,
) -> Result<String, StripeReadError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .ok_or(StripeReadError::MalformedResponse(name))
}

fn required_i64(value: &Value, field: &'static str) -> Result<i64, StripeReadError> {
    value
        .get(field)
        .and_then(Value::as_i64)
        .ok_or(StripeReadError::MalformedResponse(field))
}

fn validate_identifier(value: &str) -> Result<(), StripeReadError> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(StripeReadError::InvalidIdentifier);
    }
    Ok(())
}

fn map_request_error(error: reqwest::Error) -> StripeReadError {
    if error.is_timeout() {
        StripeReadError::Timeout
    } else if error.is_redirect() {
        StripeReadError::RedirectRejected
    } else {
        StripeReadError::Transport
    }
}
