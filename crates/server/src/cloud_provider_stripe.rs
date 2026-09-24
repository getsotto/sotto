//! Stripe's verified evidence boundary for person-level Cloud coverage.
//!
//! This module parses one supported personal-seat invoice event into provider-neutral evidence.
//! It deliberately does not perform HTTP, SQL, checkout, or entitlement work. The next adapter
//! slice can use the returned [`StripeCoverageEvidence`] to construct a verified allocation and
//! invoke [`crate::cloud_provider_refresh::refresh_verified_event`].

use std::fmt;

use serde::Deserialize;
use serde_json::{json, Value};
use thiserror::Error;

use crate::billing::webhook_version_accepted;
use crate::cloud_provider::{
    PayerKind, ProviderAdapterError, ProviderContext, ProviderEnvironment, VerifiedProviderEvent,
};

pub const STRIPE_NAMESPACE: &str = "stripe";
pub const STRIPE_CURRENCY: &str = "gbp";
pub const STRIPE_ALLOCATION_METADATA_KEY: &str = "sotto_allocation_reference";

/// The first transport contract supports one standard personal seat per invoice line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StripeInterval {
    Month,
    Year,
}

impl StripeInterval {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Month => "month",
            Self::Year => "year",
        }
    }
}

impl fmt::Display for StripeInterval {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Operator-selected Stripe identity and the two supported standard prices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeCoverageConfig {
    pub account_id: String,
    pub environment: ProviderEnvironment,
    pub monthly_price_id: String,
    pub annual_price_id: String,
}

impl StripeCoverageConfig {
    pub fn new(
        account_id: impl Into<String>,
        environment: ProviderEnvironment,
        monthly_price_id: impl Into<String>,
        annual_price_id: impl Into<String>,
    ) -> Result<Self, StripeContractError> {
        let config = Self {
            account_id: account_id.into(),
            environment,
            monthly_price_id: monthly_price_id.into(),
            annual_price_id: annual_price_id.into(),
        };
        config.validate()?;
        Ok(config)
    }

    pub fn provider_context(&self) -> Result<ProviderContext, StripeContractError> {
        ProviderContext::new(STRIPE_NAMESPACE, self.account_id.clone(), self.environment)
            .map_err(StripeContractError::ProviderContext)
    }

    fn validate(&self) -> Result<(), StripeContractError> {
        for (value, name) in [
            (&self.account_id, "Stripe account"),
            (&self.monthly_price_id, "monthly Stripe price"),
            (&self.annual_price_id, "annual Stripe price"),
        ] {
            if value.trim().is_empty() {
                return Err(StripeContractError::InvalidConfig(name));
            }
        }
        if self.monthly_price_id == self.annual_price_id {
            return Err(StripeContractError::InvalidConfig(
                "monthly and annual Stripe prices must differ",
            ));
        }
        Ok(())
    }
}

/// Normalised, signature-verified evidence for one personal subscription seat.
///
/// No raw JSON, webhook signature, customer name, email, or payment secret crosses this boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeCoverageEvidence {
    pub event: VerifiedProviderEvent,
    pub account_provenance: StripeAccountProvenance,
    pub invoice_id: String,
    pub customer_id: String,
    pub subscription_id: String,
    pub provider_item_id: String,
    pub price_id: String,
    pub allocation_reference: String,
    pub payment_intent_id: String,
    pub currency: String,
    pub amount_paid: i64,
    pub interval: StripeInterval,
    pub period_start: i64,
    pub period_end: i64,
    pub evidence_reference: String,
}

/// How the event identified the Stripe account that supplied it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StripeAccountProvenance {
    OperatorAccount,
    DeclaredAccount(String),
}

/// Durable personal ownership resolved by the caller before evidence can authorise coverage.
///
/// The value copied from invoice metadata is only a claim. It must match this trusted binding;
/// metadata alone never establishes a beneficiary or allocation. Sponsor allocations are rejected
/// until their quantity and multi-beneficiary receipt contract is implemented.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeAllocationBinding {
    allocation_reference: String,
    customer_id: String,
    subscription_id: String,
    provider_item_id: String,
    payer_kind: PayerKind,
}

impl StripeAllocationBinding {
    pub fn new(
        allocation_reference: impl Into<String>,
        customer_id: impl Into<String>,
        subscription_id: impl Into<String>,
        provider_item_id: impl Into<String>,
        payer_kind: PayerKind,
    ) -> Result<Self, StripeContractError> {
        if payer_kind != PayerKind::Personal {
            return Err(StripeContractError::UnsupportedPayerKind);
        }
        let binding = Self {
            allocation_reference: allocation_reference.into(),
            customer_id: customer_id.into(),
            subscription_id: subscription_id.into(),
            provider_item_id: provider_item_id.into(),
            payer_kind,
        };
        for (value, name) in [
            (&binding.allocation_reference, "allocation reference"),
            (&binding.customer_id, "customer"),
            (&binding.subscription_id, "subscription"),
            (&binding.provider_item_id, "provider item"),
        ] {
            if value.trim().is_empty() {
                return Err(StripeContractError::InvalidConfig(name));
            }
        }
        Ok(binding)
    }

    pub const fn payer_kind(&self) -> PayerKind {
        self.payer_kind
    }
}

/// Errors are intentionally typed so the caller can reject permanent evidence failures and retry
/// transport failures separately once the concrete history client is added.
#[derive(Debug, Error)]
pub enum StripeContractError {
    #[error("invalid Stripe configuration: {0}")]
    InvalidConfig(&'static str),
    #[error("Stripe webhook signature is invalid")]
    InvalidSignature,
    #[error("Stripe webhook payload is malformed")]
    MalformedPayload,
    #[error("Stripe webhook API version is unsupported: {0}")]
    UnsupportedApiVersion(String),
    #[error("Stripe event type is unsupported: {0}")]
    UnsupportedEventType(String),
    #[error("Stripe event context does not match the configured account or mode")]
    ContextMismatch,
    #[error("Stripe field is missing: {0}")]
    MissingField(&'static str),
    #[error("Stripe field is invalid: {0}")]
    InvalidField(&'static str),
    #[error("Stripe invoice is not paid")]
    UnpaidInvoice,
    #[error("Stripe invoice must contain exactly one personal seat line")]
    UnsupportedQuantity,
    #[error("Stripe price is not one of the configured standard prices")]
    UnsupportedPrice,
    #[error("provider context is invalid: {0}")]
    ProviderContext(ProviderAdapterError),
    #[error("normalised Stripe evidence could not be serialised")]
    NormalizationSerialization,
    #[error("Stripe payment settlement is unsupported or ambiguous: {0}")]
    UnsupportedSettlement(&'static str),
    #[error("Stripe evidence does not match the trusted allocation binding")]
    OwnershipMismatch,
    #[error("Stripe coverage currently supports personal allocations only")]
    UnsupportedPayerKind,
}

#[derive(Debug, Deserialize)]
struct RawStripeEvent {
    id: String,
    created: i64,
    api_version: Option<String>,
    #[serde(rename = "type")]
    event_type: String,
    account: Option<String>,
    context: Option<Value>,
    livemode: bool,
    data: RawEventData,
}

#[derive(Debug, Deserialize)]
struct RawEventData {
    object: Value,
}

/// Verify and normalise a supported `invoice.paid` event.
pub fn decode_paid_invoice(
    raw_payload: &[u8],
    signature_header: &str,
    webhook_secret: &str,
    now: i64,
    config: &StripeCoverageConfig,
    binding: &StripeAllocationBinding,
) -> Result<StripeCoverageEvidence, StripeContractError> {
    let payload =
        std::str::from_utf8(raw_payload).map_err(|_| StripeContractError::MalformedPayload)?;
    if !crate::billing::verify_signature(webhook_secret, signature_header, payload, now) {
        return Err(StripeContractError::InvalidSignature);
    }

    let event: RawStripeEvent =
        serde_json::from_slice(raw_payload).map_err(|_| StripeContractError::MalformedPayload)?;
    let account_provenance = validate_event_context(&event, config)?;
    let api_version = event
        .api_version
        .as_deref()
        .ok_or_else(|| StripeContractError::UnsupportedApiVersion("missing".into()))?;
    if !webhook_version_accepted(Some(api_version)) {
        return Err(StripeContractError::UnsupportedApiVersion(
            api_version.into(),
        ));
    }
    if event.event_type != "invoice.paid" {
        return Err(StripeContractError::UnsupportedEventType(event.event_type));
    }

    let invoice = &event.data.object;
    let invoice_id = required_ref(invoice, "id")?;
    let customer_id = required_ref(invoice, "customer")?;
    let subscription_id = required_ref(invoice, "subscription")?;
    if invoice.get("status").and_then(Value::as_str) != Some("paid")
        || invoice.get("paid").and_then(Value::as_bool) != Some(true)
    {
        return Err(StripeContractError::UnpaidInvoice);
    }
    let currency = required_string(invoice, "currency")?.to_ascii_lowercase();
    if currency != STRIPE_CURRENCY {
        return Err(StripeContractError::InvalidField("currency"));
    }
    let amount_paid = required_i64(invoice, "amount_paid")?;
    let amount_due = required_i64(invoice, "amount_due")?;
    if amount_paid <= 0 || amount_due <= 0 || amount_paid != amount_due {
        return Err(StripeContractError::UnsupportedSettlement(
            "payment must settle the full positive invoice amount",
        ));
    }
    let payment_intent_id = required_payment_intent(invoice, amount_paid, &currency)?;

    let lines = invoice
        .get("lines")
        .and_then(|value| value.get("data"))
        .and_then(Value::as_array)
        .ok_or(StripeContractError::MissingField("lines.data"))?;
    if lines.len() != 1 {
        return Err(StripeContractError::UnsupportedQuantity);
    }
    let line = &lines[0];
    let provider_item_id = required_ref(line, "id")?;
    if required_i64(line, "quantity")? != 1 {
        return Err(StripeContractError::UnsupportedQuantity);
    }
    let price = line
        .get("price")
        .and_then(Value::as_object)
        .ok_or(StripeContractError::MissingField("lines.data[0].price"))?;
    let price_id = required_ref(&Value::Object(price.clone()), "id")?;
    let interval = match price
        .get("recurring")
        .and_then(|value| value.get("interval"))
        .and_then(Value::as_str)
    {
        Some("month") if price_id == config.monthly_price_id => StripeInterval::Month,
        Some("year") if price_id == config.annual_price_id => StripeInterval::Year,
        Some("month" | "year") => return Err(StripeContractError::UnsupportedPrice),
        _ => {
            return Err(StripeContractError::InvalidField(
                "price.recurring.interval",
            ))
        }
    };
    let period = line
        .get("period")
        .and_then(Value::as_object)
        .ok_or(StripeContractError::MissingField("lines.data[0].period"))?;
    let period_start = required_i64(&Value::Object(period.clone()), "start")?;
    let period_end = required_i64(&Value::Object(period.clone()), "end")?;
    if period_start < 0 || period_end <= period_start {
        return Err(StripeContractError::InvalidField("period"));
    }
    let allocation_reference = invoice
        .get("metadata")
        .and_then(Value::as_object)
        .and_then(|metadata| metadata.get(STRIPE_ALLOCATION_METADATA_KEY))
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .ok_or(StripeContractError::MissingField(
            "metadata.sotto_allocation_reference",
        ))?;
    if customer_id != binding.customer_id
        || subscription_id != binding.subscription_id
        || provider_item_id != binding.provider_item_id
        || allocation_reference != binding.allocation_reference
    {
        return Err(StripeContractError::OwnershipMismatch);
    }

    // Hash only event identity and trusted ownership. Invoice amounts and periods can change when
    // the provider corrects a remote object; they belong to refreshed history, not event identity.
    let normalized = json!({
        "allocation_reference": allocation_reference,
        "event_id": event.id,
        "event_type": event.event_type,
        "provider_created_at": event.created,
        "subscription_id": subscription_id,
    });
    let normalized_bytes = serde_json::to_vec(&normalized)
        .map_err(|_| StripeContractError::NormalizationSerialization)?;
    let verified_event = VerifiedProviderEvent::from_payload(
        event.id.clone(),
        event.event_type,
        event.created,
        Some(subscription_id.clone()),
        Some(allocation_reference.clone()),
        &normalized_bytes,
    )
    .map_err(StripeContractError::ProviderContext)?;

    Ok(StripeCoverageEvidence {
        event: verified_event,
        account_provenance,
        invoice_id: invoice_id.clone(),
        customer_id,
        subscription_id,
        provider_item_id: provider_item_id.clone(),
        price_id,
        allocation_reference,
        payment_intent_id,
        currency,
        amount_paid,
        interval,
        period_start,
        period_end,
        evidence_reference: format!("stripe:invoice:{invoice_id}:line:{provider_item_id}"),
    })
}

fn validate_event_context(
    event: &RawStripeEvent,
    config: &StripeCoverageConfig,
) -> Result<StripeAccountProvenance, StripeContractError> {
    if event
        .context
        .as_ref()
        .is_some_and(|context| !context.is_null())
        || event.livemode != matches!(config.environment, ProviderEnvironment::Live)
    {
        return Err(StripeContractError::ContextMismatch);
    }
    match event.account.as_deref() {
        Some(account) if account == config.account_id => {
            Ok(StripeAccountProvenance::DeclaredAccount(account.into()))
        }
        Some(_) => Err(StripeContractError::ContextMismatch),
        None => Ok(StripeAccountProvenance::OperatorAccount),
    }
}

fn required_payment_intent(
    invoice: &Value,
    amount_paid: i64,
    invoice_currency: &str,
) -> Result<String, StripeContractError> {
    let payment_intent = invoice
        .get("payment_intent")
        .ok_or(StripeContractError::MissingField("payment_intent"))?;
    let payment_intent =
        payment_intent
            .as_object()
            .ok_or(StripeContractError::UnsupportedSettlement(
                "payment intent must be expanded before settlement can be trusted",
            ))?;
    let payment_intent = Value::Object(payment_intent.clone());
    let payment_intent_id = required_ref(&payment_intent, "id")?;
    if payment_intent.get("status").and_then(Value::as_str) != Some("succeeded") {
        return Err(StripeContractError::UnsupportedSettlement(
            "payment intent has not succeeded",
        ));
    }
    let amount_received = required_i64(&payment_intent, "amount_received")?;
    let payment_currency = required_string(&payment_intent, "currency")?.to_ascii_lowercase();
    if amount_received <= 0
        || amount_received != amount_paid
        || payment_currency != invoice_currency
    {
        return Err(StripeContractError::UnsupportedSettlement(
            "payment intent does not settle the invoice in full",
        ));
    }
    Ok(payment_intent_id)
}

fn required_string(object: &Value, field: &'static str) -> Result<String, StripeContractError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .ok_or(StripeContractError::MissingField(field))
}

fn required_ref(object: &Value, field: &'static str) -> Result<String, StripeContractError> {
    let value = object
        .get(field)
        .ok_or(StripeContractError::MissingField(field))?;
    if let Some(value) = value.as_str().filter(|value| !value.trim().is_empty()) {
        return Ok(value.to_owned());
    }
    if let Some(value) = value
        .get("id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
    {
        return Ok(value.to_owned());
    }
    Err(StripeContractError::InvalidField(field))
}

fn required_i64(object: &Value, field: &'static str) -> Result<i64, StripeContractError> {
    object
        .get(field)
        .and_then(Value::as_i64)
        .ok_or(StripeContractError::MissingField(field))
}
