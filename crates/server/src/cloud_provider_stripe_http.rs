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
use crate::billing_catalogue::{BillingInterval, StripePriceObservation};
use crate::cloud_provider::ProviderEnvironment;
use crate::cloud_provider_stripe::{
    decode_invoice_payment, validate_personal_invoice_observation, StripeAllocationBinding,
    StripeContractError, StripeCoverageConfig, StripeInterval, StripePersonalInvoiceFacts,
    StripePersonalInvoiceObservation, STRIPE_ALLOCATION_METADATA_KEY,
};
use crate::cloud_provider_stripe_corrections::{
    evaluate_personal_invoice_access, StripePersonalInvoiceAccessDecision,
    StripePersonalInvoiceCorrectionEvidence, StripeRetainedPaidTerm, StripeUnresolvedCorrections,
};
use crate::cloud_provider_stripe_renewals::StripeRenewalFailureEvidence;
use crate::cloud_provider_stripe_sponsored::{
    SponsoredInvoiceLine, SponsoredInvoiceSettlement, SponsoredStripeCoverageConfig,
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
    #[error("Stripe resource named a different parent than the one requested")]
    ParentMismatch,
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
    #[error("Stripe invoice observation was unsupported: {0}")]
    Observation(#[source] StripeContractError),
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
    coverage: StripeCoverageConfig,
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
            coverage: config.clone(),
        })
    }

    pub fn session(&self) -> StripeReadSession {
        StripeReadSession {
            deadline: Instant::now() + self.limits.session_timeout,
            attempts: 0,
            bytes: 0,
            pages: 0,
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

    /// Read one configured Price through the authenticated, bounded API session.
    #[doc(hidden)]
    pub async fn price(
        &self,
        session: &mut StripeReadSession,
        price_id: &str,
    ) -> Result<StripePriceResource, StripeReadError> {
        self.ensure_account(session).await?;
        validate_identifier(price_id)?;
        let path = format!("v1/prices/{price_id}");
        let value = self.request_json(session, Method::GET, &path, &[]).await?;
        let resource = parse_price(&value)?;
        if resource.id != price_id {
            return Err(StripeReadError::ContextMismatch);
        }
        validate_mode(Some(resource.livemode), self.environment)?;
        Ok(resource)
    }

    /// Read a configured Price and bind its observation to this authenticated account and mode.
    #[doc(hidden)]
    pub async fn price_observation(
        &self,
        session: &mut StripeReadSession,
        price_id: &str,
    ) -> Result<StripePriceObservation, StripeReadError> {
        let resource = self.price(session, price_id).await?;
        Ok(resource.observation(self.account_id.clone(), self.environment))
    }

    /// Retrieve one immutable event through the authenticated, bounded API session.
    ///
    /// The repair decoder owns event-shape validation. This method only binds the response to the
    /// requested event identifier and preserves the session's request, byte and deadline bounds.
    #[doc(hidden)]
    pub async fn event_payload(
        &self,
        session: &mut StripeReadSession,
        event_id: &str,
    ) -> Result<Value, StripeReadError> {
        self.ensure_account(session).await?;
        validate_identifier(event_id)?;
        let path = format!("v1/events/{event_id}");
        let value = self.request_json(session, Method::GET, &path, &[]).await?;
        if value.get("object").and_then(Value::as_str) != Some("event")
            || value.get("id").and_then(Value::as_str) != Some(event_id)
        {
            return Err(StripeReadError::ContextMismatch);
        }
        Ok(value)
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
        self.validate_subscription_invoices(&invoices, subscription_id, customer_id)
    }

    /// Enumerate subscription invoices with a caller-owned record bound.
    ///
    /// The bound is enforced while pagination is happening. The result is a recent prefix when
    /// more invoices exist, so a long-lived subscription does not force an unbounded history read.
    #[doc(hidden)]
    pub async fn subscription_invoices_bounded(
        &self,
        session: &mut StripeReadSession,
        subscription_id: &str,
        customer_id: Option<&str>,
        max_invoices: usize,
    ) -> Result<Vec<StripeInvoiceResource>, StripeReadError> {
        self.ensure_account(session).await?;
        validate_identifier(subscription_id)?;
        if max_invoices == 0 {
            return Err(StripeReadError::InvalidConfig(
                "subscription invoice bound must be nonzero",
            ));
        }
        if let Some(customer_id) = customer_id {
            validate_identifier(customer_id)?;
        }
        let mut query = vec![("subscription".to_owned(), subscription_id.to_owned())];
        if let Some(customer_id) = customer_id {
            query.push(("customer".to_owned(), customer_id.to_owned()));
        }
        let invoices = self
            .list_bounded(session, "v1/invoices", query, parse_invoice, max_invoices)
            .await?;
        self.validate_subscription_invoices(&invoices, subscription_id, customer_id)
    }

    fn validate_subscription_invoices(
        &self,
        invoices: &[StripeInvoiceResource],
        subscription_id: &str,
        customer_id: Option<&str>,
    ) -> Result<Vec<StripeInvoiceResource>, StripeReadError> {
        for invoice in invoices {
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
        Ok(invoices.to_vec())
    }

    pub async fn invoice_lines(
        &self,
        session: &mut StripeReadSession,
        invoice_id: &str,
    ) -> Result<Vec<StripeInvoiceLineResource>, StripeReadError> {
        self.ensure_account(session).await?;
        validate_identifier(invoice_id)?;
        let path = format!("v1/invoices/{invoice_id}/lines");
        // Stripe binds this collection to the validated invoice path. The line's livemode is
        // preserved and checked by personal_invoice_observation against the account context.
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

    /// Read one paid sponsored invoice and normalise its grouped subscription-item lines.
    ///
    /// The allocation manifest is intentionally supplied by the caller and joined later by the
    /// pure sponsored composer. This method only authenticates the invoice, its lines and its
    /// payment settlement under the existing bounded session.
    #[doc(hidden)]
    pub async fn sponsored_invoice_settlement(
        &self,
        session: &mut StripeReadSession,
        invoice_id: &str,
        customer_id: &str,
        subscription_id: &str,
        sponsored: &SponsoredStripeCoverageConfig,
    ) -> Result<SponsoredInvoiceSettlement, StripeReadError> {
        if sponsored.account_id() != self.account_id || sponsored.environment() != self.environment
        {
            return Err(StripeReadError::ContextMismatch);
        }
        let invoice = self.invoice(session, invoice_id).await?;
        if invoice.parent_type.as_deref() != Some("subscription_details")
            || invoice.customer_id.as_deref() != Some(customer_id)
            || invoice.subscription_id.as_deref() != Some(subscription_id)
            || invoice.status.as_deref() != Some("paid")
        {
            return Err(StripeReadError::Observation(
                StripeContractError::ContextMismatch,
            ));
        }
        let lines = self.invoice_lines(session, invoice_id).await?;
        let payments = self.invoice_payments(session, invoice_id).await?;
        if payments.len() != 1 {
            return Err(StripeReadError::Observation(
                StripeContractError::UnsupportedSettlement(
                    "sponsored invoice has an ambiguous payment list",
                ),
            ));
        }
        let payment = payments[0].settlement(&self.coverage)?;
        let period_start = lines
            .iter()
            .filter_map(|line| line.period_start)
            .min()
            .ok_or(StripeReadError::MalformedResponse(
                "lines.data.period.start",
            ))?;
        let period_end = lines
            .iter()
            .filter_map(|line| line.period_end)
            .max()
            .ok_or(StripeReadError::MalformedResponse("lines.data.period.end"))?;
        let amount_due = invoice
            .amount_due
            .ok_or(StripeReadError::MalformedResponse("invoice.amount_due"))?;
        let amount_paid = invoice
            .amount_paid
            .ok_or(StripeReadError::MalformedResponse("invoice.amount_paid"))?;
        let amount_remaining =
            invoice
                .amount_remaining
                .ok_or(StripeReadError::MalformedResponse(
                    "invoice.amount_remaining",
                ))?;
        let amount_overpaid = invoice
            .amount_overpaid
            .ok_or(StripeReadError::MalformedResponse(
                "invoice.amount_overpaid",
            ))?;
        let amount_paid_off_stripe =
            invoice
                .amount_paid_off_stripe
                .ok_or(StripeReadError::MalformedResponse(
                    "invoice.amount_paid_off_stripe",
                ))?;
        let currency = invoice
            .currency
            .clone()
            .ok_or(StripeReadError::MalformedResponse("invoice.currency"))?;
        if amount_remaining != 0 || amount_overpaid != 0 || amount_paid_off_stripe != 0 {
            return Err(StripeReadError::Observation(
                StripeContractError::UnsupportedSettlement(
                    "sponsored invoice has unresolved financial amounts",
                ),
            ));
        }
        if payment.invoice_id() != invoice_id
            || payment.amount_requested() != amount_due
            || payment.amount_paid() != amount_paid
            || payment.currency() != currency
        {
            return Err(StripeReadError::Observation(
                StripeContractError::UnsupportedSettlement("invoice and payment amounts differ"),
            ));
        }
        let mut normalized_lines = Vec::with_capacity(lines.len());
        for line in lines {
            if line.parent_type.as_deref() != Some("subscription_item_details")
                || line.pricing_type.as_deref() != Some("price_details")
                || line.livemode != Some(matches!(self.environment, ProviderEnvironment::Live))
                || line.subscription_id.as_deref() != Some(subscription_id)
                || line
                    .invoice_id
                    .as_deref()
                    .is_some_and(|line_invoice_id| line_invoice_id != invoice_id)
            {
                return Err(StripeReadError::Observation(
                    StripeContractError::ContextMismatch,
                ));
            }
            let quantity = line.quantity.filter(|quantity| *quantity > 0).ok_or(
                StripeReadError::Observation(StripeContractError::UnsupportedQuantity),
            )? as u64;
            normalized_lines.push(
                SponsoredInvoiceLine::new(
                    line.id,
                    line.subscription_item_id
                        .ok_or(StripeReadError::MalformedResponse("line.subscription_item"))?,
                    line.price_id
                        .ok_or(StripeReadError::MalformedResponse("line.price"))?,
                    line.period_start
                        .ok_or(StripeReadError::MalformedResponse("line.period.start"))?,
                    line.period_end
                        .ok_or(StripeReadError::MalformedResponse("line.period.end"))?,
                    quantity,
                    line.proration
                        .ok_or(StripeReadError::MalformedResponse("line.proration"))?,
                )
                .map_err(|_| StripeReadError::MalformedResponse("sponsored invoice line"))?,
            );
        }
        SponsoredInvoiceSettlement::new(
            invoice.id,
            customer_id,
            subscription_id,
            period_start,
            period_end,
            currency,
            amount_due,
            amount_paid,
            format!(
                "stripe:invoice:{invoice_id}:payment:{}",
                payment.payment_intent_id()
            ),
            normalized_lines,
        )
        .map_err(|_| StripeReadError::MalformedResponse("sponsored invoice settlement"))
    }

    /// Enumerate every refund Stripe returns for one PaymentIntent, whatever its status.
    ///
    /// Each page repeats the `payment_intent` filter. An empty result means only that this
    /// filtered enumeration returned no refunds. It is not a consistent snapshot: refunds can be
    /// created or change state during or after the read.
    pub async fn payment_intent_refunds(
        &self,
        session: &mut StripeReadSession,
        payment_intent_id: &str,
    ) -> Result<Vec<StripeRefundResource>, StripeReadError> {
        self.ensure_account(session).await?;
        validate_identifier(payment_intent_id)?;
        let query = vec![("payment_intent".to_owned(), payment_intent_id.to_owned())];
        self.list(session, "v1/refunds", query, |value| {
            let refund = parse_refund(value)?;
            if refund
                .payment_intent_id
                .as_deref()
                .is_some_and(|id| id != payment_intent_id)
            {
                return Err(StripeReadError::ParentMismatch);
            }
            // Stripe documents no mode on refunds, so the verified account supplies it. A mode
            // that is present anyway must still agree with that account.
            if refund.livemode.is_some() {
                validate_mode(refund.livemode, self.environment)?;
            }
            Ok(refund)
        })
        .await
    }

    /// Enumerate every dispute Stripe returns for one PaymentIntent, whatever its status.
    ///
    /// Each page repeats the `payment_intent` filter. An empty result means only that this
    /// filtered enumeration returned no disputes. It is not a consistent snapshot: disputes can be
    /// opened or change state during or after the read.
    pub async fn payment_intent_disputes(
        &self,
        session: &mut StripeReadSession,
        payment_intent_id: &str,
    ) -> Result<Vec<StripeDisputeResource>, StripeReadError> {
        self.ensure_account(session).await?;
        validate_identifier(payment_intent_id)?;
        let query = vec![("payment_intent".to_owned(), payment_intent_id.to_owned())];
        self.list(session, "v1/disputes", query, |value| {
            let dispute = parse_dispute(value)?;
            if dispute
                .payment_intent_id
                .as_deref()
                .is_some_and(|id| id != payment_intent_id)
            {
                return Err(StripeReadError::ParentMismatch);
            }
            validate_mode(Some(dispute.livemode), self.environment)?;
            Ok(dispute)
        })
        .await
    }

    /// Enumerate every credit note Stripe returns for one invoice, issued or void.
    ///
    /// Each page repeats the `invoice` filter. An empty result means only that this filtered
    /// enumeration returned no credit notes. It is not a consistent snapshot: notes can be issued
    /// or voided during or after the read.
    pub async fn invoice_credit_notes(
        &self,
        session: &mut StripeReadSession,
        invoice_id: &str,
    ) -> Result<Vec<StripeCreditNoteResource>, StripeReadError> {
        self.ensure_account(session).await?;
        validate_identifier(invoice_id)?;
        let query = vec![("invoice".to_owned(), invoice_id.to_owned())];
        self.list(session, "v1/credit_notes", query, |value| {
            let note = parse_credit_note(value)?;
            if note.invoice_id != invoice_id {
                return Err(StripeReadError::ParentMismatch);
            }
            validate_mode(Some(note.livemode), self.environment)?;
            Ok(note)
        })
        .await
    }

    pub async fn personal_invoice_observation(
        &self,
        session: &mut StripeReadSession,
        invoice_id: &str,
        binding: &StripeAllocationBinding,
    ) -> Result<StripePersonalInvoiceObservation, StripeReadError> {
        let invoice = self.invoice(session, invoice_id).await?;
        self.personal_invoice_observation_from_invoice(session, invoice, binding)
            .await
    }

    async fn personal_invoice_observation_from_invoice(
        &self,
        session: &mut StripeReadSession,
        invoice: StripeInvoiceResource,
        binding: &StripeAllocationBinding,
    ) -> Result<StripePersonalInvoiceObservation, StripeReadError> {
        if invoice.parent_type.as_deref() != Some("subscription_details")
            || invoice.subscription_id.is_none()
        {
            return Err(StripeReadError::Observation(
                StripeContractError::ContextMismatch,
            ));
        }
        if invoice.status.as_deref() != Some("paid") {
            return Err(StripeReadError::Observation(
                StripeContractError::UnpaidInvoice,
            ));
        }
        let lines = self.invoice_lines(session, &invoice.id).await?;
        self.personal_invoice_observation_from_invoice_and_lines(session, invoice, binding, lines)
            .await
    }

    async fn personal_invoice_observation_from_invoice_and_lines(
        &self,
        session: &mut StripeReadSession,
        invoice: StripeInvoiceResource,
        binding: &StripeAllocationBinding,
        lines: Vec<StripeInvoiceLineResource>,
    ) -> Result<StripePersonalInvoiceObservation, StripeReadError> {
        if lines.len() != 1 {
            return Err(StripeReadError::Observation(
                StripeContractError::UnsupportedQuantity,
            ));
        }
        let line = &lines[0];
        if line.quantity != Some(1) {
            return Err(StripeReadError::Observation(
                StripeContractError::UnsupportedQuantity,
            ));
        }
        match line.livemode {
            Some(mode)
                if mode == matches!(self.coverage.environment, ProviderEnvironment::Live) => {}
            Some(_) => {
                return Err(StripeReadError::Observation(
                    StripeContractError::ContextMismatch,
                ));
            }
            None => {
                return Err(StripeReadError::Observation(
                    StripeContractError::MissingField("lines.data[0].livemode"),
                ));
            }
        }
        if line.parent_type.as_deref() != Some("subscription_item_details") {
            return Err(StripeReadError::Observation(
                StripeContractError::UnsupportedLine(
                    "line is not generated by a subscription item",
                ),
            ));
        }
        if line.pricing_type.as_deref() != Some("price_details") {
            return Err(StripeReadError::Observation(
                StripeContractError::UnsupportedPrice,
            ));
        }
        let payments = self.invoice_payments(session, &invoice.id).await?;
        if payments.len() != 1 {
            return Err(StripeReadError::Observation(
                StripeContractError::UnsupportedSettlement("invoice has an ambiguous payment list"),
            ));
        }
        let settlement = payments[0].settlement(&self.coverage)?;
        validate_personal_invoice_observation(
            &self.coverage,
            binding,
            StripePersonalInvoiceFacts {
                invoice_id: invoice.id,
                customer_id: invoice
                    .customer_id
                    .ok_or(StripeReadError::MalformedResponse("invoice.customer"))?,
                invoice_subscription_id: invoice.subscription_id,
                subscription_id: line
                    .subscription_id
                    .clone()
                    .ok_or(StripeReadError::MalformedResponse("line.subscription"))?,
                provider_item_id: line
                    .subscription_item_id
                    .clone()
                    .ok_or(StripeReadError::MalformedResponse("line.subscription_item"))?,
                invoice_line_id: line.id.clone(),
                allocation_reference: invoice.allocation_reference.ok_or(
                    StripeReadError::MalformedResponse(
                        "invoice.metadata.sotto_allocation_reference",
                    ),
                )?,
                price_id: line
                    .price_id
                    .clone()
                    .ok_or(StripeReadError::MalformedResponse("line.price"))?,
                currency: invoice
                    .currency
                    .ok_or(StripeReadError::MalformedResponse("invoice.currency"))?,
                amount_paid: invoice
                    .amount_paid
                    .ok_or(StripeReadError::MalformedResponse("invoice.amount_paid"))?,
                amount_due: invoice
                    .amount_due
                    .ok_or(StripeReadError::MalformedResponse("invoice.amount_due"))?,
                amount_overpaid: invoice.amount_overpaid.ok_or(
                    StripeReadError::MalformedResponse("invoice.amount_overpaid"),
                )?,
                amount_paid_off_stripe: invoice.amount_paid_off_stripe.ok_or(
                    StripeReadError::MalformedResponse("invoice.amount_paid_off_stripe"),
                )?,
                period_start: line
                    .period_start
                    .ok_or(StripeReadError::MalformedResponse("line.period.start"))?,
                period_end: line
                    .period_end
                    .ok_or(StripeReadError::MalformedResponse("line.period.end"))?,
                settlement,
            },
        )
        .map_err(StripeReadError::Observation)
    }

    /// Enumerate every invoice returned for one trusted personal subscription under one session.
    ///
    /// Non-paid invoices remain observations and do not become coverage or renewal recovery.
    /// Exhausting pagination does not establish an atomic Stripe snapshot.
    #[doc(hidden)]
    pub async fn personal_invoice_history(
        &self,
        session: &mut StripeReadSession,
        binding: &StripeAllocationBinding,
    ) -> Result<StripePersonalInvoiceHistoryResult, StripeReadError> {
        self.subscription(session, binding.subscription_id(), binding.customer_id())
            .await?;
        let invoices = self
            .subscription_invoices(
                session,
                binding.subscription_id(),
                Some(binding.customer_id()),
            )
            .await?;
        let mut entries = Vec::new();
        let mut unresolved = Vec::new();

        for listed in invoices {
            let invoice_id = listed.id.clone();
            if listed.customer_id.is_none() {
                unresolved.push(
                    StripePersonalInvoiceHistoryUnresolved::InvoiceMissingCustomer {
                        invoice_id: invoice_id.clone(),
                    },
                );
            }
            if listed.parent_type.is_none()
                || (listed.parent_type.as_deref() == Some("subscription_details")
                    && listed.subscription_id.is_none())
            {
                unresolved.push(
                    StripePersonalInvoiceHistoryUnresolved::InvoiceMissingSubscriptionParent {
                        invoice_id: invoice_id.clone(),
                    },
                );
            } else if listed.parent_type.as_deref() != Some("subscription_details") {
                unresolved.push(
                    StripePersonalInvoiceHistoryUnresolved::InvoiceUnknownSubscriptionParent {
                        invoice_id: invoice_id.clone(),
                        parent_type: listed.parent_type.clone(),
                    },
                );
            }
            if listed
                .customer_id
                .as_deref()
                .is_some_and(|customer_id| customer_id != binding.customer_id())
            {
                return Err(StripeReadError::ContextMismatch);
            }
            if listed
                .subscription_id
                .as_deref()
                .is_some_and(|subscription_id| subscription_id != binding.subscription_id())
            {
                return Err(StripeReadError::ContextMismatch);
            }

            let Some(status) = listed.status.as_deref() else {
                unresolved.push(
                    StripePersonalInvoiceHistoryUnresolved::InvoiceMissingStatus { invoice_id },
                );
                continue;
            };
            match status {
                "draft" | "open" | "void" | "uncollectible" => {
                    if unresolved
                        .iter()
                        .any(|reason| history_reason_invoice_id(reason) == listed.id.as_str())
                    {
                        continue;
                    }
                    entries.push(StripePersonalInvoiceHistoryEntry::NonPaid(
                        StripeNonPaidInvoice {
                            invoice_id: listed.id,
                            status: status.to_owned(),
                        },
                    ));
                }
                "paid" => {
                    if unresolved
                        .iter()
                        .any(|reason| history_reason_invoice_id(reason) == listed.id.as_str())
                    {
                        continue;
                    }
                    let detailed = self.invoice(session, &listed.id).await?;
                    validate_invoice_ownership(&detailed, binding)?;
                    if !invoice_headers_match(&listed, &detailed) {
                        unresolved.push(
                            StripePersonalInvoiceHistoryUnresolved::InvoiceChangedDuringRead {
                                invoice_id: listed.id,
                            },
                        );
                        continue;
                    }
                    let evidence = self
                        .personal_invoice_correction_evidence_from_invoice(
                            session, detailed, binding,
                        )
                        .await?;
                    match evaluate_personal_invoice_access(&evidence) {
                        StripePersonalInvoiceAccessDecision::RetainPaidTerm(term) => {
                            entries.push(StripePersonalInvoiceHistoryEntry::Paid(term));
                        }
                        StripePersonalInvoiceAccessDecision::NeedsEvidence(evidence) => {
                            unresolved.push(
                                StripePersonalInvoiceHistoryUnresolved::InvoiceCorrections {
                                    invoice_id: listed.id,
                                    evidence,
                                },
                            );
                        }
                    }
                }
                status => unresolved.push(
                    StripePersonalInvoiceHistoryUnresolved::InvoiceUnknownStatus {
                        invoice_id,
                        status: status.to_owned(),
                    },
                ),
            }
        }

        if !unresolved.is_empty() {
            unresolved.sort_by(|left, right| left.sort_key().cmp(&right.sort_key()));
            return Ok(StripePersonalInvoiceHistoryResult::NeedsEvidence(
                StripePersonalInvoiceHistoryNeedsEvidence {
                    subscription_id: binding.subscription_id().to_owned(),
                    customer_id: binding.customer_id().to_owned(),
                    reasons: unresolved,
                },
            ));
        }
        entries.sort_by(|left, right| {
            history_entry_invoice_id(left).cmp(history_entry_invoice_id(right))
        });
        Ok(StripePersonalInvoiceHistoryResult::Observed(
            StripePersonalInvoiceHistory {
                account_id: self.account_id.clone(),
                environment: self.environment,
                subscription_id: binding.subscription_id().to_owned(),
                customer_id: binding.customer_id().to_owned(),
                entries,
            },
        ))
    }

    /// Assemble one invoice and its bounded correction reads without deciding access eligibility.
    #[doc(hidden)]
    pub async fn personal_invoice_correction_evidence(
        &self,
        session: &mut StripeReadSession,
        invoice_id: &str,
        binding: &StripeAllocationBinding,
    ) -> Result<
        crate::cloud_provider_stripe_corrections::StripePersonalInvoiceCorrectionEvidence,
        StripeReadError,
    > {
        let observation = self
            .personal_invoice_observation(session, invoice_id, binding)
            .await?;
        self.personal_invoice_correction_evidence_from_observation(session, observation)
            .await
    }

    /// Observe the current state of a historically failed personal renewal under one bounded
    /// session. This is evidence only: it never assigns recovery or writes coverage.
    #[doc(hidden)]
    pub async fn personal_renewal_observation(
        &self,
        session: &mut StripeReadSession,
        binding: &StripeAllocationBinding,
        failure: &StripeRenewalFailureEvidence,
    ) -> Result<StripeRenewalObservationResult, StripeReadError> {
        if failure.provider_account_id() != self.account_id
            || failure.environment() != self.environment
            || failure.allocation_reference() != binding.allocation_reference()
            || failure.customer_id() != binding.customer_id()
            || failure.subscription_id() != binding.subscription_id()
            || failure.provider_item_id() != binding.provider_item_id()
            || binding.payer_kind() != crate::cloud_provider::PayerKind::Personal
        {
            return Err(StripeReadError::ContextMismatch);
        }
        let first_subscription = self
            .subscription(session, binding.subscription_id(), binding.customer_id())
            .await?;
        if first_subscription.status.is_none() {
            return Ok(StripeRenewalObservationResult::NeedsEvidence(
                StripeRenewalNeedsEvidence::IncompleteCurrentShape("subscription.status"),
            ));
        }
        if !supported_subscription_status(first_subscription.status.as_deref().unwrap()) {
            return Ok(StripeRenewalObservationResult::NeedsEvidence(
                StripeRenewalNeedsEvidence::UnsupportedSubscriptionStatus(
                    first_subscription.status.clone().unwrap_or_default(),
                ),
            ));
        }
        let cancellation = cancellation_facts(&first_subscription)?;
        let first_invoice = self.invoice(session, failure.invoice_id()).await?;
        validate_current_invoice(&first_invoice, binding)?;
        if let Err(error) = validate_current_invoice_cycle(&first_invoice) {
            match error {
                CurrentInvoiceCycleError::Malformed(field) => {
                    return Err(StripeReadError::MalformedResponse(field));
                }
                CurrentInvoiceCycleError::Unsupported { field, value } => {
                    return Ok(StripeRenewalObservationResult::NeedsEvidence(
                        StripeRenewalNeedsEvidence::UnsupportedInvoiceField { field, value },
                    ));
                }
            }
        }
        let lines = self.invoice_lines(session, &first_invoice.id).await?;
        validate_current_line(&lines, failure, binding, &self.coverage, self.environment)?;

        let state = match first_invoice.status.as_deref() {
            Some("paid") => {
                if first_invoice.amount_remaining != Some(0) {
                    return Ok(StripeRenewalObservationResult::NeedsEvidence(
                        StripeRenewalNeedsEvidence::UnsupportedSettlement,
                    ));
                }
                let evidence = self
                    .personal_invoice_correction_evidence_from_invoice_and_lines(
                        session,
                        first_invoice.clone(),
                        binding,
                        lines,
                    )
                    .await?;
                let term = match evaluate_personal_invoice_access(&evidence) {
                    StripePersonalInvoiceAccessDecision::RetainPaidTerm(term) => term,
                    StripePersonalInvoiceAccessDecision::NeedsEvidence(_) => {
                        return Ok(StripeRenewalObservationResult::NeedsEvidence(
                            StripeRenewalNeedsEvidence::UnsupportedSettlement,
                        ));
                    }
                };
                StripeRenewalCurrentState::Paid {
                    evidence: Box::new(evidence),
                    term: Box::new(term),
                }
            }
            Some("open") => {
                if !open_invoice_amounts_valid(&first_invoice) {
                    return Ok(StripeRenewalObservationResult::NeedsEvidence(
                        StripeRenewalNeedsEvidence::UnsupportedSettlement,
                    ));
                }
                StripeRenewalCurrentState::Open
            }
            Some("void") | Some("uncollectible") => {
                if !closed_invoice_amounts_valid(&first_invoice) {
                    return Ok(StripeRenewalObservationResult::NeedsEvidence(
                        StripeRenewalNeedsEvidence::UnsupportedSettlement,
                    ));
                }
                StripeRenewalCurrentState::ClosedUnpaid {
                    status: first_invoice.status.clone().unwrap_or_default(),
                }
            }
            Some(status) => {
                return Ok(StripeRenewalObservationResult::NeedsEvidence(
                    StripeRenewalNeedsEvidence::UnsupportedStatus(status.to_owned()),
                ));
            }
            None => {
                return Ok(StripeRenewalObservationResult::NeedsEvidence(
                    StripeRenewalNeedsEvidence::IncompleteCurrentShape("invoice.status"),
                ));
            }
        };
        let second_invoice = self.invoice(session, failure.invoice_id()).await?;
        let second_subscription = self
            .subscription(session, binding.subscription_id(), binding.customer_id())
            .await?;
        if first_invoice != second_invoice {
            let fields = invoice_diff_fields(&first_invoice, &second_invoice);
            return Ok(StripeRenewalObservationResult::NeedsEvidence(
                StripeRenewalNeedsEvidence::ChangedDuringRead {
                    resource: "invoice",
                    fields,
                },
            ));
        }
        if first_subscription != second_subscription {
            let fields = subscription_diff_fields(&first_subscription, &second_subscription);
            return Ok(StripeRenewalObservationResult::NeedsEvidence(
                StripeRenewalNeedsEvidence::ChangedDuringRead {
                    resource: "subscription",
                    fields,
                },
            ));
        }
        Ok(StripeRenewalObservationResult::Observed(Box::new(
            StripeRenewalObservation {
                renewal_id: failure.renewal_id().to_owned(),
                invoice_id: failure.invoice_id().to_owned(),
                event_id: failure.event_id().to_owned(),
                provider_account_id: failure.provider_account_id().to_owned(),
                environment: failure.environment(),
                allocation_reference: failure.allocation_reference().to_owned(),
                customer_id: failure.customer_id().to_owned(),
                subscription_id: failure.subscription_id().to_owned(),
                provider_item_id: failure.provider_item_id().to_owned(),
                period_start: failure.renewal_period_start(),
                period_end: failure.renewal_period_end(),
                state,
                cancellation,
            },
        )))
    }

    pub(crate) async fn personal_invoice_correction_evidence_from_invoice(
        &self,
        session: &mut StripeReadSession,
        invoice: StripeInvoiceResource,
        binding: &StripeAllocationBinding,
    ) -> Result<StripePersonalInvoiceCorrectionEvidence, StripeReadError> {
        let observation = self
            .personal_invoice_observation_from_invoice(session, invoice, binding)
            .await?;
        self.personal_invoice_correction_evidence_from_observation(session, observation)
            .await
    }

    async fn personal_invoice_correction_evidence_from_invoice_and_lines(
        &self,
        session: &mut StripeReadSession,
        invoice: StripeInvoiceResource,
        binding: &StripeAllocationBinding,
        lines: Vec<StripeInvoiceLineResource>,
    ) -> Result<StripePersonalInvoiceCorrectionEvidence, StripeReadError> {
        let observation = self
            .personal_invoice_observation_from_invoice_and_lines(session, invoice, binding, lines)
            .await?;
        self.personal_invoice_correction_evidence_from_observation(session, observation)
            .await
    }

    async fn personal_invoice_correction_evidence_from_observation(
        &self,
        session: &mut StripeReadSession,
        observation: StripePersonalInvoiceObservation,
    ) -> Result<StripePersonalInvoiceCorrectionEvidence, StripeReadError> {
        let refunds = self
            .payment_intent_refunds(session, observation.payment_intent_id())
            .await?;
        let disputes = self
            .payment_intent_disputes(session, observation.payment_intent_id())
            .await?;
        let credit_notes = self
            .invoice_credit_notes(session, observation.invoice_id())
            .await?;
        crate::cloud_provider_stripe_corrections::assemble(
            observation,
            refunds,
            disputes,
            credit_notes,
            self.environment,
        )
    }

    async fn list_bounded<T, F>(
        &self,
        session: &mut StripeReadSession,
        path: &str,
        base_query: Vec<(String, String)>,
        parse: F,
        max_records: usize,
    ) -> Result<Vec<T>, StripeReadError>
    where
        F: Fn(&Value) -> Result<T, StripeReadError> + Copy,
    {
        let mut output = Vec::new();
        let mut ids = HashSet::new();
        let mut cursor: Option<String> = None;
        loop {
            session.add_page(self.limits.max_pages)?;
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
            let remaining = max_records.saturating_sub(output.len());
            let page_data = &data[..data.len().min(remaining)];
            session.add_records(page_data.len(), self.limits.max_records)?;
            let mut parsed = Vec::with_capacity(page_data.len());
            for item in page_data {
                let id = required_id(item, "list.data.id")?;
                if !ids.insert(id.clone()) {
                    return Err(StripeReadError::InvalidPagination("duplicate record id"));
                }
                parsed.push((id, parse(item)?));
            }
            let last_id = parsed.last().map(|(id, _)| id.clone());
            output.extend(parsed.into_iter().map(|(_, value)| value));
            if !has_more || output.len() >= max_records {
                return Ok(output);
            }
            let Some(last_id) = last_id else {
                return Err(StripeReadError::InvalidPagination(
                    "missing cursor after nonempty page",
                ));
            };
            if cursor.as_deref() == Some(last_id.as_str()) {
                return Err(StripeReadError::InvalidPagination("cursor did not advance"));
            }
            cursor = Some(last_id);
        }
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
        loop {
            session.add_page(self.limits.max_pages)?;
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
                Err(error)
                    if method == Method::GET
                        && error.retryable()
                        && retries < self.limits.max_retries =>
                {
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
        serde_json::from_slice(&body).map_err(|_| StripeReadError::MalformedResponse("json"))
    }
}

pub struct StripeReadSession {
    deadline: Instant,
    attempts: usize,
    bytes: usize,
    pages: usize,
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
            .field("pages", &self.pages)
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

    fn add_page(&mut self, limit: usize) -> Result<(), StripeReadError> {
        self.pages = self
            .pages
            .checked_add(1)
            .ok_or(StripeReadError::PageBoundExceeded)?;
        if self.pages > limit {
            return Err(StripeReadError::PageBoundExceeded);
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
    pub cancel_at_period_end: Option<bool>,
    pub cancel_at: Option<i64>,
    pub canceled_at: Option<i64>,
    pub ended_at: Option<i64>,
    cancel_at_present: bool,
    canceled_at_present: bool,
    ended_at_present: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeInvoiceResource {
    pub id: String,
    pub customer_id: Option<String>,
    pub subscription_id: Option<String>,
    pub legacy_subscription_id: Option<String>,
    pub parent_type: Option<String>,
    pub billing_reason: Option<String>,
    pub collection_method: Option<String>,
    pub status: Option<String>,
    pub currency: Option<String>,
    pub amount_paid: Option<i64>,
    pub amount_due: Option<i64>,
    pub amount_remaining: Option<i64>,
    pub amount_overpaid: Option<i64>,
    pub amount_paid_off_stripe: Option<i64>,
    pub allocation_reference: Option<String>,
    pub livemode: Option<bool>,
    billing_reason_present: bool,
    collection_method_present: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripePriceResource {
    pub id: String,
    pub active: bool,
    pub livemode: bool,
    pub currency: String,
    pub unit_amount: Option<i64>,
    pub interval: Option<BillingInterval>,
    pub interval_count: Option<i64>,
    pub usage_type: Option<String>,
}

impl StripePriceResource {
    /// Convert the authenticated transport result into the catalogue's provider observation.
    fn observation(
        &self,
        account_id: impl Into<String>,
        environment: ProviderEnvironment,
    ) -> StripePriceObservation {
        StripePriceObservation::new(
            self.id.clone(),
            account_id,
            environment,
            self.active,
            self.livemode,
            self.currency.clone(),
            self.unit_amount,
            self.interval,
            self.interval_count,
            self.usage_type.clone(),
        )
    }
}

/// A non-paid invoice retained by personal history collection without becoming coverage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeNonPaidInvoice {
    invoice_id: String,
    status: String,
}

impl StripeNonPaidInvoice {
    pub fn invoice_id(&self) -> &str {
        &self.invoice_id
    }

    pub fn status(&self) -> &str {
        &self.status
    }
}

/// One invoice accounted for by a bounded personal history traversal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StripePersonalInvoiceHistoryEntry {
    Paid(StripeRetainedPaidTerm),
    NonPaid(StripeNonPaidInvoice),
}

/// Every invoice returned by Stripe was accounted for under one shared read session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripePersonalInvoiceHistory {
    account_id: String,
    environment: ProviderEnvironment,
    subscription_id: String,
    customer_id: String,
    entries: Vec<StripePersonalInvoiceHistoryEntry>,
}

impl StripePersonalInvoiceHistory {
    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    pub const fn environment(&self) -> ProviderEnvironment {
        self.environment
    }

    pub fn subscription_id(&self) -> &str {
        &self.subscription_id
    }

    pub fn customer_id(&self) -> &str {
        &self.customer_id
    }

    pub fn entries(&self) -> &[StripePersonalInvoiceHistoryEntry] {
        &self.entries
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        account_id: &str,
        environment: ProviderEnvironment,
        subscription_id: &str,
        customer_id: &str,
        entries: Vec<StripePersonalInvoiceHistoryEntry>,
    ) -> Self {
        Self {
            account_id: account_id.into(),
            environment,
            subscription_id: subscription_id.into(),
            customer_id: customer_id.into(),
            entries,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StripePersonalInvoiceHistoryUnresolved {
    InvoiceMissingCustomer {
        invoice_id: String,
    },
    InvoiceMissingSubscriptionParent {
        invoice_id: String,
    },
    InvoiceUnknownSubscriptionParent {
        invoice_id: String,
        parent_type: Option<String>,
    },
    InvoiceMissingStatus {
        invoice_id: String,
    },
    InvoiceUnknownStatus {
        invoice_id: String,
        status: String,
    },
    InvoiceCorrections {
        invoice_id: String,
        evidence: StripeUnresolvedCorrections,
    },
    InvoiceChangedDuringRead {
        invoice_id: String,
    },
}

impl StripePersonalInvoiceHistoryUnresolved {
    fn sort_key(&self) -> (&str, u8) {
        match self {
            Self::InvoiceMissingCustomer { invoice_id } => (invoice_id, 0),
            Self::InvoiceMissingSubscriptionParent { invoice_id } => (invoice_id, 1),
            Self::InvoiceUnknownSubscriptionParent { invoice_id, .. } => (invoice_id, 2),
            Self::InvoiceMissingStatus { invoice_id } => (invoice_id, 3),
            Self::InvoiceUnknownStatus { invoice_id, .. } => (invoice_id, 4),
            Self::InvoiceCorrections { invoice_id, .. } => (invoice_id, 5),
            Self::InvoiceChangedDuringRead { invoice_id } => (invoice_id, 6),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripePersonalInvoiceHistoryNeedsEvidence {
    subscription_id: String,
    customer_id: String,
    reasons: Vec<StripePersonalInvoiceHistoryUnresolved>,
}

impl StripePersonalInvoiceHistoryNeedsEvidence {
    pub fn subscription_id(&self) -> &str {
        &self.subscription_id
    }

    pub fn customer_id(&self) -> &str {
        &self.customer_id
    }

    pub fn reasons(&self) -> &[StripePersonalInvoiceHistoryUnresolved] {
        &self.reasons
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StripePersonalInvoiceHistoryResult {
    Observed(StripePersonalInvoiceHistory),
    NeedsEvidence(StripePersonalInvoiceHistoryNeedsEvidence),
}

fn history_reason_invoice_id(reason: &StripePersonalInvoiceHistoryUnresolved) -> &str {
    match reason {
        StripePersonalInvoiceHistoryUnresolved::InvoiceMissingCustomer { invoice_id }
        | StripePersonalInvoiceHistoryUnresolved::InvoiceMissingSubscriptionParent { invoice_id }
        | StripePersonalInvoiceHistoryUnresolved::InvoiceUnknownSubscriptionParent {
            invoice_id,
            ..
        }
        | StripePersonalInvoiceHistoryUnresolved::InvoiceMissingStatus { invoice_id }
        | StripePersonalInvoiceHistoryUnresolved::InvoiceUnknownStatus { invoice_id, .. }
        | StripePersonalInvoiceHistoryUnresolved::InvoiceCorrections { invoice_id, .. }
        | StripePersonalInvoiceHistoryUnresolved::InvoiceChangedDuringRead { invoice_id } => {
            invoice_id
        }
    }
}

fn history_entry_invoice_id(entry: &StripePersonalInvoiceHistoryEntry) -> &str {
    match entry {
        StripePersonalInvoiceHistoryEntry::Paid(term) => term.invoice_id(),
        StripePersonalInvoiceHistoryEntry::NonPaid(invoice) => invoice.invoice_id(),
    }
}

fn invoice_headers_match(left: &StripeInvoiceResource, right: &StripeInvoiceResource) -> bool {
    left.id == right.id
        && left.customer_id == right.customer_id
        && left.subscription_id == right.subscription_id
        && left.legacy_subscription_id == right.legacy_subscription_id
        && left.parent_type == right.parent_type
        && left.status == right.status
        && left.currency == right.currency
        && left.amount_paid == right.amount_paid
        && left.amount_due == right.amount_due
        && left.amount_remaining == right.amount_remaining
        && left.amount_overpaid == right.amount_overpaid
        && left.amount_paid_off_stripe == right.amount_paid_off_stripe
        && left.allocation_reference == right.allocation_reference
        && left.livemode == right.livemode
}

fn cancellation_facts(
    subscription: &StripeSubscriptionResource,
) -> Result<StripeRenewalCancellationFacts, StripeReadError> {
    if !subscription.cancel_at_present {
        return Err(StripeReadError::MalformedResponse("subscription.cancel_at"));
    }
    if !subscription.canceled_at_present {
        return Err(StripeReadError::MalformedResponse(
            "subscription.canceled_at",
        ));
    }
    if !subscription.ended_at_present {
        return Err(StripeReadError::MalformedResponse("subscription.ended_at"));
    }
    let cancel_at_period_end =
        subscription
            .cancel_at_period_end
            .ok_or(StripeReadError::MalformedResponse(
                "subscription.cancel_at_period_end",
            ))?;
    for (value, field) in [
        (subscription.cancel_at, "subscription.cancel_at"),
        (subscription.canceled_at, "subscription.canceled_at"),
        (subscription.ended_at, "subscription.ended_at"),
    ] {
        if value.is_some_and(|timestamp| timestamp < 0) {
            return Err(StripeReadError::MalformedResponse(field));
        }
    }
    Ok(StripeRenewalCancellationFacts {
        status: subscription.status.clone(),
        cancel_at_period_end,
        cancel_at: subscription.cancel_at,
        canceled_at: subscription.canceled_at,
        ended_at: subscription.ended_at,
    })
}

fn validate_current_invoice(
    invoice: &StripeInvoiceResource,
    binding: &StripeAllocationBinding,
) -> Result<(), StripeReadError> {
    if invoice.customer_id.as_deref() != Some(binding.customer_id())
        || invoice.subscription_id.as_deref() != Some(binding.subscription_id())
        || invoice.parent_type.as_deref() != Some("subscription_details")
        || invoice.allocation_reference.as_deref() != Some(binding.allocation_reference())
        || !invoice
            .currency
            .as_deref()
            .is_some_and(|currency| currency.eq_ignore_ascii_case("gbp"))
    {
        return Err(StripeReadError::ContextMismatch);
    }
    Ok(())
}

enum CurrentInvoiceCycleError {
    Malformed(&'static str),
    Unsupported { field: &'static str, value: String },
}

fn validate_current_invoice_cycle(
    invoice: &StripeInvoiceResource,
) -> Result<(), CurrentInvoiceCycleError> {
    if !invoice.billing_reason_present || invoice.billing_reason.is_none() {
        return Err(CurrentInvoiceCycleError::Malformed(
            "invoice.billing_reason",
        ));
    }
    match invoice.billing_reason.as_deref() {
        Some("subscription_cycle") => {}
        Some(value) => {
            return Err(CurrentInvoiceCycleError::Unsupported {
                field: "billing_reason",
                value: value.to_owned(),
            });
        }
        None => unreachable!(),
    }
    if !invoice.collection_method_present || invoice.collection_method.is_none() {
        return Err(CurrentInvoiceCycleError::Malformed(
            "invoice.collection_method",
        ));
    }
    if let Some(value) = invoice.collection_method.as_deref() {
        if value != "charge_automatically" {
            return Err(CurrentInvoiceCycleError::Unsupported {
                field: "collection_method",
                value: value.to_owned(),
            });
        }
    }
    Ok(())
}

fn validate_current_line(
    lines: &[StripeInvoiceLineResource],
    failure: &StripeRenewalFailureEvidence,
    binding: &StripeAllocationBinding,
    config: &StripeCoverageConfig,
    environment: ProviderEnvironment,
) -> Result<(), StripeReadError> {
    if lines.len() != 1 {
        return Err(StripeReadError::Observation(
            StripeContractError::UnsupportedQuantity,
        ));
    }
    let line = &lines[0];
    if line.id != failure.invoice_line_id()
        || line.invoice_id.as_deref() != Some(failure.invoice_id())
        || line.quantity != Some(1)
        || line.subscription_id.as_deref() != Some(binding.subscription_id())
        || line.subscription_item_id.as_deref() != Some(binding.provider_item_id())
        || line.period_start != Some(failure.renewal_period_start())
        || line.period_end != Some(failure.renewal_period_end())
        || line.parent_type.as_deref() != Some("subscription_item_details")
        || line.pricing_type.as_deref() != Some("price_details")
        || line.livemode != Some(matches!(environment, ProviderEnvironment::Live))
        || line.proration != Some(false)
        || line
            .legacy_subscription_id
            .as_deref()
            .is_some_and(|id| id != binding.subscription_id())
        || line
            .legacy_subscription_item_id
            .as_deref()
            .is_some_and(|id| id != binding.provider_item_id())
    {
        return Err(StripeReadError::Observation(
            StripeContractError::ContextMismatch,
        ));
    }
    let expected_price = match failure.interval() {
        StripeInterval::Month => &config.monthly_price_id,
        StripeInterval::Year => &config.annual_price_id,
    };
    if line.price_id.as_deref() != Some(expected_price.as_str()) {
        return Err(StripeReadError::MalformedResponse("line.price"));
    }
    Ok(())
}

fn open_invoice_amounts_valid(invoice: &StripeInvoiceResource) -> bool {
    matches!(
        (invoice.amount_due, invoice.amount_remaining, invoice.amount_paid, invoice.amount_overpaid, invoice.amount_paid_off_stripe),
        (Some(due), Some(remaining), Some(0), Some(0), Some(0)) if due > 0 && remaining == due
    )
}

fn closed_invoice_amounts_valid(invoice: &StripeInvoiceResource) -> bool {
    matches!(
        (
            invoice.amount_due,
            invoice.amount_remaining,
            invoice.amount_paid,
            invoice.amount_overpaid,
            invoice.amount_paid_off_stripe
        ),
        (Some(due), Some(remaining), Some(0), Some(0), Some(0))
            if due >= 0 && remaining >= 0 && remaining <= due
    )
}

fn supported_subscription_status(status: &str) -> bool {
    matches!(
        status,
        "active"
            | "trialing"
            | "past_due"
            | "canceled"
            | "unpaid"
            | "incomplete"
            | "incomplete_expired"
            | "paused"
    )
}

fn invoice_diff_fields(
    left: &StripeInvoiceResource,
    right: &StripeInvoiceResource,
) -> Vec<&'static str> {
    let mut fields = Vec::new();
    macro_rules! compare {
        ($field:ident) => {
            if left.$field != right.$field {
                fields.push(stringify!($field));
            }
        };
    }
    compare!(id);
    compare!(customer_id);
    compare!(subscription_id);
    compare!(legacy_subscription_id);
    compare!(parent_type);
    compare!(billing_reason);
    compare!(collection_method);
    compare!(status);
    compare!(currency);
    compare!(amount_paid);
    compare!(amount_due);
    compare!(amount_remaining);
    compare!(amount_overpaid);
    compare!(amount_paid_off_stripe);
    compare!(allocation_reference);
    compare!(livemode);
    if left.billing_reason_present != right.billing_reason_present
        && left.billing_reason == right.billing_reason
    {
        fields.push("billing_reason");
    }
    if left.collection_method_present != right.collection_method_present
        && left.collection_method == right.collection_method
    {
        fields.push("collection_method");
    }
    fields
}

fn subscription_diff_fields(
    left: &StripeSubscriptionResource,
    right: &StripeSubscriptionResource,
) -> Vec<&'static str> {
    let mut fields = Vec::new();
    macro_rules! compare {
        ($field:ident) => {
            if left.$field != right.$field {
                fields.push(stringify!($field));
            }
        };
    }
    compare!(id);
    compare!(customer_id);
    compare!(status);
    compare!(livemode);
    compare!(cancel_at_period_end);
    compare!(cancel_at);
    compare!(canceled_at);
    compare!(ended_at);
    if left.cancel_at_present != right.cancel_at_present && left.cancel_at == right.cancel_at {
        fields.push("cancel_at");
    }
    if left.canceled_at_present != right.canceled_at_present
        && left.canceled_at == right.canceled_at
    {
        fields.push("canceled_at");
    }
    if left.ended_at_present != right.ended_at_present && left.ended_at == right.ended_at {
        fields.push("ended_at");
    }
    fields
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeInvoiceLineResource {
    pub id: String,
    pub quantity: Option<i64>,
    pub subscription_id: Option<String>,
    pub subscription_item_id: Option<String>,
    pub legacy_subscription_id: Option<String>,
    pub legacy_subscription_item_id: Option<String>,
    pub price_id: Option<String>,
    pub parent_type: Option<String>,
    pub pricing_type: Option<String>,
    pub livemode: Option<bool>,
    pub period_start: Option<i64>,
    pub period_end: Option<i64>,
    pub invoice_id: Option<String>,
    pub proration: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeRenewalCancellationFacts {
    status: Option<String>,
    cancel_at_period_end: bool,
    cancel_at: Option<i64>,
    canceled_at: Option<i64>,
    ended_at: Option<i64>,
}

impl StripeRenewalCancellationFacts {
    pub fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }

    pub const fn cancel_at_period_end(&self) -> bool {
        self.cancel_at_period_end
    }
    pub const fn cancel_at(&self) -> Option<i64> {
        self.cancel_at
    }
    pub const fn canceled_at(&self) -> Option<i64> {
        self.canceled_at
    }
    pub const fn ended_at(&self) -> Option<i64> {
        self.ended_at
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StripeRenewalCurrentState {
    Paid {
        evidence: Box<StripePersonalInvoiceCorrectionEvidence>,
        term: Box<StripeRetainedPaidTerm>,
    },
    Open,
    ClosedUnpaid {
        status: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeRenewalObservation {
    renewal_id: String,
    invoice_id: String,
    event_id: String,
    provider_account_id: String,
    environment: ProviderEnvironment,
    allocation_reference: String,
    customer_id: String,
    subscription_id: String,
    provider_item_id: String,
    period_start: i64,
    period_end: i64,
    state: StripeRenewalCurrentState,
    cancellation: StripeRenewalCancellationFacts,
}

impl StripeRenewalObservation {
    pub fn renewal_id(&self) -> &str {
        &self.renewal_id
    }
    pub fn invoice_id(&self) -> &str {
        &self.invoice_id
    }
    pub fn event_id(&self) -> &str {
        &self.event_id
    }
    pub fn provider_account_id(&self) -> &str {
        &self.provider_account_id
    }
    pub const fn environment(&self) -> ProviderEnvironment {
        self.environment
    }
    pub fn allocation_reference(&self) -> &str {
        &self.allocation_reference
    }
    pub fn customer_id(&self) -> &str {
        &self.customer_id
    }
    pub fn subscription_id(&self) -> &str {
        &self.subscription_id
    }
    pub fn provider_item_id(&self) -> &str {
        &self.provider_item_id
    }
    pub const fn period_start(&self) -> i64 {
        self.period_start
    }
    pub const fn period_end(&self) -> i64 {
        self.period_end
    }
    pub fn state(&self) -> &StripeRenewalCurrentState {
        &self.state
    }
    pub fn cancellation(&self) -> &StripeRenewalCancellationFacts {
        &self.cancellation
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StripeRenewalNeedsEvidence {
    UnsupportedStatus(String),
    UnsupportedSubscriptionStatus(String),
    UnsupportedInvoiceField {
        field: &'static str,
        value: String,
    },
    ChangedDuringRead {
        resource: &'static str,
        fields: Vec<&'static str>,
    },
    UnsupportedSettlement,
    IncompleteCurrentShape(&'static str),
}

impl fmt::Display for StripeRenewalNeedsEvidence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedStatus(status) => {
                write!(formatter, "unsupported invoice status {status}")
            }
            Self::UnsupportedSubscriptionStatus(status) => {
                write!(formatter, "unsupported subscription status {status}")
            }
            Self::UnsupportedInvoiceField { field, value } => {
                write!(formatter, "unsupported invoice {field} {value}")
            }
            Self::ChangedDuringRead { resource, fields } => {
                write!(formatter, "{resource} changed during read: {fields:?}")
            }
            Self::UnsupportedSettlement => formatter.write_str("current settlement is unsupported"),
            Self::IncompleteCurrentShape(field) => {
                write!(formatter, "required current field is unavailable: {field}")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StripeRenewalObservationResult {
    Observed(Box<StripeRenewalObservation>),
    NeedsEvidence(StripeRenewalNeedsEvidence),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeInvoicePaymentResource {
    pub id: String,
    pub invoice_id: String,
    pub status: String,
    pub amount_paid: Option<i64>,
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
        // Keep decode_invoice_payment as the single settlement authority. If it later requires a
        // field this transport does not model, the round-trip must fail closed until this DTO is
        // extended.
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

/// One refund returned by [`StripeReadClient::payment_intent_refunds`].
///
/// This is evidence about a single object, not an interpreted correction. The list filter does not
/// prove ownership: `payment_intent_id` is `None` when Stripe omitted the reference, and a later
/// assembler must resolve that link through an authenticated parent read or treat it as
/// unsupported. Charge reconciliation and deduplication against credit notes are also left to
/// that later boundary. Stripe documents no mode on refunds, so `livemode` is normally `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeRefundResource {
    pub id: String,
    pub payment_intent_id: Option<String>,
    pub charge_id: Option<String>,
    pub amount: i64,
    pub currency: String,
    pub created: i64,
    /// Stripe documents this as nullable. `None` is an absent status, never a settled refund.
    pub status: Option<StripeRefundStatus>,
    pub livemode: Option<bool>,
}

/// Refund states stay distinct so that a pending or failed refund cannot read as a completed one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StripeRefundStatus {
    Pending,
    RequiresAction,
    Succeeded,
    Failed,
    Canceled,
    /// A token this contract does not know. It must not be read as no correction.
    Unknown(String),
}

impl StripeRefundStatus {
    fn from_token(token: String) -> Self {
        match token.as_str() {
            "pending" => Self::Pending,
            "requires_action" => Self::RequiresAction,
            "succeeded" => Self::Succeeded,
            "failed" => Self::Failed,
            "canceled" => Self::Canceled,
            _ => Self::Unknown(token),
        }
    }
}

/// One dispute returned by [`StripeReadClient::payment_intent_disputes`].
///
/// This is evidence about a single object, not an access decision. As with refunds, an absent
/// `payment_intent_id` stays absent and must be resolved by a later authenticated read before it
/// can be attributed to the requested PaymentIntent. Evidence, reasons, balance transactions and
/// payment method details are not retained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeDisputeResource {
    pub id: String,
    pub payment_intent_id: Option<String>,
    pub charge_id: String,
    pub amount: i64,
    pub currency: String,
    pub created: i64,
    pub status: StripeDisputeStatus,
    pub livemode: bool,
}

/// Dispute states stay distinct. An inquiry, an open dispute and each outcome can have different
/// consequences, so none of them collapses into a single disputed flag here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StripeDisputeStatus {
    WarningNeedsResponse,
    WarningUnderReview,
    WarningClosed,
    NeedsResponse,
    UnderReview,
    Won,
    Lost,
    Prevented,
    /// A token this contract does not know. It must not be read as no dispute or as resolved.
    Unknown(String),
}

impl StripeDisputeStatus {
    fn from_token(token: String) -> Self {
        match token.as_str() {
            "warning_needs_response" => Self::WarningNeedsResponse,
            "warning_under_review" => Self::WarningUnderReview,
            "warning_closed" => Self::WarningClosed,
            "needs_response" => Self::NeedsResponse,
            "under_review" => Self::UnderReview,
            "won" => Self::Won,
            "lost" => Self::Lost,
            "prevented" => Self::Prevented,
            _ => Self::Unknown(token),
        }
    }
}

/// One credit note returned by [`StripeReadClient::invoice_credit_notes`].
///
/// A credit note is not automatically a cash refund. `pre_payment_amount` reduced what the invoice
/// asked for, while `post_payment_amount` was refunded, credited to the customer balance or
/// credited outside Stripe. The note's embedded `lines` and `refunds` are previews rather than
/// complete lists, so neither is retained or offered as evidence. Line enumeration, credit
/// allocation and deduplication against refunds belong to a later boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeCreditNoteResource {
    pub id: String,
    pub invoice_id: String,
    pub customer_id: String,
    pub amount: i64,
    pub pre_payment_amount: i64,
    pub post_payment_amount: i64,
    pub currency: String,
    pub created: i64,
    pub status: StripeCreditNoteStatus,
    pub credit_note_type: StripeCreditNoteType,
    pub livemode: bool,
}

/// A voided note must stay distinguishable from an issued one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StripeCreditNoteStatus {
    Issued,
    Void,
    /// A token this contract does not know. It must not be read as void or as issued.
    Unknown(String),
}

impl StripeCreditNoteStatus {
    fn from_token(token: String) -> Self {
        match token.as_str() {
            "issued" => Self::Issued,
            "void" => Self::Void,
            _ => Self::Unknown(token),
        }
    }
}

/// Whether the note was issued before payment, after it, or across both. Stripe's prose names
/// only the first two, but its enum also documents `mixed`, which is kept rather than rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StripeCreditNoteType {
    PrePayment,
    PostPayment,
    Mixed,
    /// A token this contract does not know. It must not be read as either payment phase.
    Unknown(String),
}

impl StripeCreditNoteType {
    fn from_token(token: String) -> Self {
        match token.as_str() {
            "pre_payment" => Self::PrePayment,
            "post_payment" => Self::PostPayment,
            "mixed" => Self::Mixed,
            _ => Self::Unknown(token),
        }
    }
}

fn parse_subscription(value: &Value) -> Result<StripeSubscriptionResource, StripeReadError> {
    Ok(StripeSubscriptionResource {
        id: required_id(value, "subscription.id")?,
        customer_id: optional_validated_ref(value.get("customer"), "subscription.customer")?,
        status: optional_string(value.get("status"), "subscription.status")?,
        livemode: optional_bool(value.get("livemode"), "subscription.livemode")?,
        cancel_at_period_end: optional_bool(
            value.get("cancel_at_period_end"),
            "subscription.cancel_at_period_end",
        )?,
        cancel_at: optional_i64(value.get("cancel_at"), "subscription.cancel_at")?,
        canceled_at: optional_i64(value.get("canceled_at"), "subscription.canceled_at")?,
        ended_at: optional_i64(value.get("ended_at"), "subscription.ended_at")?,
        cancel_at_present: value.get("cancel_at").is_some(),
        canceled_at_present: value.get("canceled_at").is_some(),
        ended_at_present: value.get("ended_at").is_some(),
    })
}

fn parse_invoice(value: &Value) -> Result<StripeInvoiceResource, StripeReadError> {
    let parent = optional_object(value.get("parent"), "invoice.parent")?;
    let parent_type = parent
        .map(|parent| optional_string(parent.get("type"), "invoice.parent.type"))
        .transpose()?
        .flatten();
    let parent_subscription_id = match parent.and_then(|parent| parent.get("subscription_details"))
    {
        None | Some(Value::Null) => None,
        Some(Value::Object(details)) => optional_validated_ref(
            details.get("subscription"),
            "invoice.parent.subscription_details.subscription",
        )?,
        Some(_) => {
            return Err(StripeReadError::MalformedResponse(
                "invoice.parent.subscription_details",
            ));
        }
    };
    let legacy_subscription_id =
        optional_validated_ref(value.get("subscription"), "invoice.subscription")?;
    if parent_subscription_id
        .as_deref()
        .zip(legacy_subscription_id.as_deref())
        .is_some_and(|(nested, legacy)| nested != legacy)
    {
        return Err(StripeReadError::ContextMismatch);
    }
    Ok(StripeInvoiceResource {
        id: required_id(value, "invoice.id")?,
        customer_id: optional_validated_ref(value.get("customer"), "invoice.customer")?,
        subscription_id: parent_subscription_id,
        legacy_subscription_id,
        parent_type,
        billing_reason: optional_string(value.get("billing_reason"), "invoice.billing_reason")?,
        collection_method: optional_string(
            value.get("collection_method"),
            "invoice.collection_method",
        )?,
        status: optional_string(value.get("status"), "invoice.status")?,
        currency: optional_string(value.get("currency"), "invoice.currency")?,
        amount_paid: optional_i64(value.get("amount_paid"), "invoice.amount_paid")?,
        amount_due: optional_i64(value.get("amount_due"), "invoice.amount_due")?,
        amount_remaining: optional_i64(value.get("amount_remaining"), "invoice.amount_remaining")?,
        amount_overpaid: optional_i64(value.get("amount_overpaid"), "invoice.amount_overpaid")?,
        amount_paid_off_stripe: optional_i64(
            value.get("amount_paid_off_stripe"),
            "invoice.amount_paid_off_stripe",
        )?,
        allocation_reference: optional_allocation_reference(value.get("metadata"))?,
        livemode: optional_bool(value.get("livemode"), "invoice.livemode")?,
        billing_reason_present: value.get("billing_reason").is_some(),
        collection_method_present: value.get("collection_method").is_some(),
    })
}

fn parse_price(value: &Value) -> Result<StripePriceResource, StripeReadError> {
    let recurring = optional_object(value.get("recurring"), "price.recurring")?;
    let interval = recurring
        .and_then(|recurring| recurring.get("interval"))
        .map(|value| match value.as_str() {
            Some("month") => Ok(BillingInterval::Month),
            Some("year") => Ok(BillingInterval::Year),
            _ => Err(StripeReadError::MalformedResponse(
                "price.recurring.interval",
            )),
        })
        .transpose()?;
    let interval_count = recurring
        .and_then(|recurring| recurring.get("interval_count"))
        .map(|value| {
            value.as_i64().ok_or(StripeReadError::MalformedResponse(
                "price.recurring.interval_count",
            ))
        })
        .transpose()?;
    let usage_type = recurring
        .and_then(|recurring| recurring.get("usage_type"))
        .map(|value| {
            value
                .as_str()
                .filter(|value| !value.trim().is_empty())
                .map(str::to_owned)
                .ok_or(StripeReadError::MalformedResponse(
                    "price.recurring.usage_type",
                ))
        })
        .transpose()?;
    Ok(StripePriceResource {
        id: required_id(value, "price.id")?,
        active: value
            .get("active")
            .and_then(Value::as_bool)
            .ok_or(StripeReadError::MalformedResponse("price.active"))?,
        livemode: value
            .get("livemode")
            .and_then(Value::as_bool)
            .ok_or(StripeReadError::MalformedResponse("price.livemode"))?,
        currency: required_string(value, "currency")?,
        unit_amount: optional_i64(value.get("unit_amount"), "price.unit_amount")?,
        interval,
        interval_count,
        usage_type,
    })
}

fn validate_invoice_ownership(
    invoice: &StripeInvoiceResource,
    binding: &StripeAllocationBinding,
) -> Result<(), StripeReadError> {
    if invoice
        .customer_id
        .as_deref()
        .is_some_and(|customer_id| customer_id != binding.customer_id())
        || invoice
            .subscription_id
            .as_deref()
            .is_some_and(|subscription_id| subscription_id != binding.subscription_id())
    {
        return Err(StripeReadError::ContextMismatch);
    }
    Ok(())
}

fn parse_invoice_line(value: &Value) -> Result<StripeInvoiceLineResource, StripeReadError> {
    let parent = optional_object(value.get("parent"), "line.parent")?;
    let details = parent
        .and_then(|parent| parent.get("subscription_item_details"))
        .and_then(Value::as_object);
    let pricing = optional_object(value.get("pricing"), "line.pricing")?;
    let price_details = pricing
        .and_then(|pricing| pricing.get("price_details"))
        .and_then(Value::as_object);
    let period = optional_object(value.get("period"), "line.period")?;
    let legacy_subscription_id =
        optional_validated_ref(value.get("subscription"), "line.subscription_legacy")?;
    let legacy_subscription_item_id = optional_validated_ref(
        value.get("subscription_item"),
        "line.subscription_item_legacy",
    )?;
    let subscription_id = details
        .map(|details| optional_validated_ref(details.get("subscription"), "line.subscription"))
        .transpose()?
        .flatten();
    let subscription_item_id = details
        .map(|details| {
            optional_validated_ref(details.get("subscription_item"), "line.subscription_item")
        })
        .transpose()?
        .flatten();
    Ok(StripeInvoiceLineResource {
        id: required_id(value, "invoice line.id")?,
        quantity: optional_i64(value.get("quantity"), "line.quantity")?,
        subscription_id,
        subscription_item_id,
        legacy_subscription_id,
        legacy_subscription_item_id,
        price_id: price_details
            .map(|details| optional_validated_ref(details.get("price"), "line.price"))
            .transpose()?
            .flatten(),
        parent_type: parent
            .map(|parent| optional_string(parent.get("type"), "line.parent.type"))
            .transpose()?
            .flatten(),
        pricing_type: pricing
            .map(|pricing| optional_string(pricing.get("type"), "line.pricing.type"))
            .transpose()?
            .flatten(),
        livemode: optional_bool(value.get("livemode"), "line.livemode")?,
        period_start: period
            .map(|period| optional_i64(period.get("start"), "line.period.start"))
            .transpose()?
            .flatten(),
        period_end: period
            .map(|period| optional_i64(period.get("end"), "line.period.end"))
            .transpose()?
            .flatten(),
        invoice_id: optional_validated_ref(value.get("invoice"), "line.invoice")?,
        proration: details
            .map(|details| optional_bool(details.get("proration"), "line.proration"))
            .transpose()?
            .flatten(),
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
        amount_paid: optional_i64(value.get("amount_paid"), "invoice payment.amount_paid")?,
        amount_requested: required_i64(value, "amount_requested")?,
        currency: required_string(value, "currency")?,
        livemode: optional_bool(value.get("livemode"), "invoice payment.livemode")?,
        payment_type: required_string_object(payment, "type", "payment.type")?,
        payment_intent_id: optional_validated_ref(
            payment.get("payment_intent"),
            "payment.payment_intent",
        )?,
    })
}

fn parse_refund(value: &Value) -> Result<StripeRefundResource, StripeReadError> {
    require_object(value, "refund", "refund.object")?;
    Ok(StripeRefundResource {
        id: required_id(value, "refund.id")?,
        payment_intent_id: optional_validated_ref(
            value.get("payment_intent"),
            "refund.payment_intent",
        )?,
        charge_id: optional_validated_ref(value.get("charge"), "refund.charge")?,
        amount: required_non_negative_i64(value.get("amount"), "refund.amount")?,
        currency: required_currency(value.get("currency"), "refund.currency")?,
        created: required_non_negative_i64(value.get("created"), "refund.created")?,
        status: optional_token(value.get("status"), "refund.status")?
            .map(StripeRefundStatus::from_token),
        livemode: optional_bool(value.get("livemode"), "refund.livemode")?,
    })
}

fn parse_dispute(value: &Value) -> Result<StripeDisputeResource, StripeReadError> {
    require_object(value, "dispute", "dispute.object")?;
    Ok(StripeDisputeResource {
        id: required_id(value, "dispute.id")?,
        payment_intent_id: optional_validated_ref(
            value.get("payment_intent"),
            "dispute.payment_intent",
        )?,
        charge_id: required_validated_ref(value.get("charge"), "dispute.charge")?,
        amount: required_non_negative_i64(value.get("amount"), "dispute.amount")?,
        currency: required_currency(value.get("currency"), "dispute.currency")?,
        created: required_non_negative_i64(value.get("created"), "dispute.created")?,
        status: optional_token(value.get("status"), "dispute.status")?
            .map(StripeDisputeStatus::from_token)
            .ok_or(StripeReadError::MalformedResponse("dispute.status"))?,
        livemode: optional_bool(value.get("livemode"), "dispute.livemode")?
            .ok_or(StripeReadError::MalformedResponse("dispute.livemode"))?,
    })
}

fn parse_credit_note(value: &Value) -> Result<StripeCreditNoteResource, StripeReadError> {
    require_object(value, "credit_note", "credit note.object")?;
    Ok(StripeCreditNoteResource {
        id: required_id(value, "credit note.id")?,
        invoice_id: required_validated_ref(value.get("invoice"), "credit note.invoice")?,
        customer_id: required_validated_ref(value.get("customer"), "credit note.customer")?,
        amount: required_non_negative_i64(value.get("amount"), "credit note.amount")?,
        pre_payment_amount: required_non_negative_i64(
            value.get("pre_payment_amount"),
            "credit note.pre_payment_amount",
        )?,
        post_payment_amount: required_non_negative_i64(
            value.get("post_payment_amount"),
            "credit note.post_payment_amount",
        )?,
        currency: required_currency(value.get("currency"), "credit note.currency")?,
        created: required_non_negative_i64(value.get("created"), "credit note.created")?,
        status: optional_token(value.get("status"), "credit note.status")?
            .map(StripeCreditNoteStatus::from_token)
            .ok_or(StripeReadError::MalformedResponse("credit note.status"))?,
        credit_note_type: optional_token(value.get("type"), "credit note.type")?
            .map(StripeCreditNoteType::from_token)
            .ok_or(StripeReadError::MalformedResponse("credit note.type"))?,
        livemode: optional_bool(value.get("livemode"), "credit note.livemode")?
            .ok_or(StripeReadError::MalformedResponse("credit note.livemode"))?,
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

fn optional_string(
    value: Option<&Value>,
    field: &'static str,
) -> Result<Option<String>, StripeReadError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_str()
            .filter(|value| !value.trim().is_empty())
            .map(str::to_owned)
            .map(Some)
            .ok_or(StripeReadError::MalformedResponse(field)),
    }
}

fn optional_bool(
    value: Option<&Value>,
    field: &'static str,
) -> Result<Option<bool>, StripeReadError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_bool()
            .map(Some)
            .ok_or(StripeReadError::MalformedResponse(field)),
    }
}

fn optional_i64(
    value: Option<&Value>,
    field: &'static str,
) -> Result<Option<i64>, StripeReadError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_i64()
            .map(Some)
            .ok_or(StripeReadError::MalformedResponse(field)),
    }
}

fn optional_object<'a>(
    value: Option<&'a Value>,
    field: &'static str,
) -> Result<Option<&'a serde_json::Map<String, Value>>, StripeReadError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_object()
            .map(Some)
            .ok_or(StripeReadError::MalformedResponse(field)),
    }
}

fn optional_allocation_reference(value: Option<&Value>) -> Result<Option<String>, StripeReadError> {
    let Some(metadata) = optional_object(value, "invoice.metadata")? else {
        return Ok(None);
    };
    optional_string(
        metadata.get(STRIPE_ALLOCATION_METADATA_KEY),
        "invoice.metadata.sotto_allocation_reference",
    )
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

fn required_validated_ref(
    value: Option<&Value>,
    field: &'static str,
) -> Result<String, StripeReadError> {
    optional_validated_ref(value, field)?.ok_or(StripeReadError::MalformedResponse(field))
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

/// Minor-unit amounts and timestamps. Null is rejected rather than read as zero, and a negative
/// value cannot describe a correction amount or a creation time.
fn required_non_negative_i64(
    value: Option<&Value>,
    field: &'static str,
) -> Result<i64, StripeReadError> {
    value
        .and_then(Value::as_i64)
        .filter(|value| *value >= 0)
        .ok_or(StripeReadError::MalformedResponse(field))
}

/// Stripe documents currencies as three-letter lowercase ISO codes. Anything else is shape drift,
/// and accepting other casings would let later comparisons disagree about the same currency.
fn required_currency(
    value: Option<&Value>,
    field: &'static str,
) -> Result<String, StripeReadError> {
    value
        .and_then(Value::as_str)
        .filter(|currency| currency.len() == 3 && currency.bytes().all(|b| b.is_ascii_lowercase()))
        .map(str::to_owned)
        .ok_or(StripeReadError::MalformedResponse(field))
}

fn require_object(
    value: &Value,
    object: &'static str,
    field: &'static str,
) -> Result<(), StripeReadError> {
    if value.get("object").and_then(Value::as_str) == Some(object) {
        Ok(())
    } else {
        Err(StripeReadError::MalformedResponse(field))
    }
}

/// A status or type token. Unknown tokens are kept, so they must look like Stripe enum values:
/// holding arbitrary provider text would let free text into retained evidence and Debug output.
/// Documented tokens are well under 64 bytes, so the bound only stops text riding in as unknown.
fn optional_token(
    value: Option<&Value>,
    field: &'static str,
) -> Result<Option<String>, StripeReadError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_str()
            .filter(|token| {
                !token.is_empty()
                    && token.len() <= 64
                    && token
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            })
            .map(|token| Some(token.to_owned()))
            .ok_or(StripeReadError::MalformedResponse(field)),
    }
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
