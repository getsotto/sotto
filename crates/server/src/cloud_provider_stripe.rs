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

use crate::billing::{
    verify_signature_detailed, webhook_version_accepted, SignatureVerificationError,
};
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

/// A settlement fetched from Stripe's authenticated Invoice Payment endpoint.
///
/// This is intentionally separate from the signed webhook bytes: webhook deliveries cannot carry
/// expanded payment objects, so the transport layer must authenticate this response independently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripePaymentSettlement {
    invoice_payment_id: String,
    invoice_id: String,
    payment_intent_id: String,
    amount_paid: i64,
    amount_requested: i64,
    currency: String,
    livemode: bool,
}

/// A validated personal invoice observation assembled from authenticated Stripe resources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripePersonalInvoiceObservation {
    invoice_id: String,
    customer_id: String,
    subscription_id: String,
    provider_item_id: String,
    allocation_reference: String,
    payment_intent_id: String,
    currency: String,
    amount_paid: i64,
    interval: StripeInterval,
    period_start: i64,
    period_end: i64,
    evidence_reference: String,
}

impl StripePersonalInvoiceObservation {
    pub fn invoice_id(&self) -> &str {
        &self.invoice_id
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

    pub fn allocation_reference(&self) -> &str {
        &self.allocation_reference
    }

    pub fn payment_intent_id(&self) -> &str {
        &self.payment_intent_id
    }

    pub fn currency(&self) -> &str {
        &self.currency
    }

    pub const fn amount_paid(&self) -> i64 {
        self.amount_paid
    }

    pub const fn interval(&self) -> StripeInterval {
        self.interval
    }

    pub const fn period_start(&self) -> i64 {
        self.period_start
    }

    pub const fn period_end(&self) -> i64 {
        self.period_end
    }

    pub fn evidence_reference(&self) -> &str {
        &self.evidence_reference
    }
}

/// Parsed invoice facts passed to the shared personal observation validator.
pub(crate) struct StripePersonalInvoiceFacts {
    pub(crate) invoice_id: String,
    pub(crate) customer_id: String,
    pub(crate) invoice_subscription_id: Option<String>,
    pub(crate) subscription_id: String,
    pub(crate) provider_item_id: String,
    pub(crate) invoice_line_id: String,
    pub(crate) allocation_reference: String,
    pub(crate) price_id: String,
    pub(crate) currency: String,
    pub(crate) amount_paid: i64,
    pub(crate) amount_due: i64,
    pub(crate) amount_overpaid: i64,
    pub(crate) amount_paid_off_stripe: i64,
    pub(crate) period_start: i64,
    pub(crate) period_end: i64,
    pub(crate) settlement: StripePaymentSettlement,
}

/// Validate the shared personal invoice contract after each input path has parsed its own shape.
pub(crate) fn validate_personal_invoice_observation(
    config: &StripeCoverageConfig,
    binding: &StripeAllocationBinding,
    facts: StripePersonalInvoiceFacts,
) -> Result<StripePersonalInvoiceObservation, StripeContractError> {
    if binding.payer_kind != PayerKind::Personal
        || facts.customer_id != binding.customer_id
        || facts
            .invoice_subscription_id
            .as_deref()
            .is_some_and(|id| id != binding.subscription_id)
        || facts.subscription_id != binding.subscription_id
        || facts.provider_item_id != binding.provider_item_id
        || facts.allocation_reference != binding.allocation_reference
    {
        return Err(StripeContractError::OwnershipMismatch);
    }
    if facts.amount_paid <= 0
        || facts.amount_due <= 0
        || facts.amount_paid != facts.amount_due
        || facts.amount_overpaid > 0
        || facts.amount_paid_off_stripe > 0
    {
        return Err(StripeContractError::UnsupportedSettlement(
            "payment must settle the full positive invoice amount",
        ));
    }
    let interval = if facts.price_id == config.monthly_price_id {
        StripeInterval::Month
    } else if facts.price_id == config.annual_price_id {
        StripeInterval::Year
    } else {
        return Err(StripeContractError::UnsupportedPrice);
    };
    if facts.period_start < 0 || facts.period_end <= facts.period_start {
        return Err(StripeContractError::InvalidField("period"));
    }
    if facts.currency.to_ascii_lowercase() != STRIPE_CURRENCY {
        return Err(StripeContractError::InvalidField("currency"));
    }
    if facts.settlement.livemode != matches!(config.environment, ProviderEnvironment::Live)
        || facts.settlement.invoice_id != facts.invoice_id
        || facts.settlement.currency != facts.currency
        || facts.settlement.amount_paid != facts.amount_paid
        || facts.settlement.amount_requested != facts.amount_due
    {
        return Err(StripeContractError::UnsupportedSettlement(
            "fetched settlement does not match the paid invoice",
        ));
    }
    Ok(StripePersonalInvoiceObservation {
        evidence_reference: format!(
            "stripe:invoice:{}:line:{}",
            facts.invoice_id, facts.invoice_line_id
        ),
        payment_intent_id: facts.settlement.payment_intent_id.clone(),
        invoice_id: facts.invoice_id,
        customer_id: facts.customer_id,
        subscription_id: facts.subscription_id,
        provider_item_id: facts.provider_item_id,
        allocation_reference: facts.allocation_reference,
        currency: facts.currency.to_ascii_lowercase(),
        amount_paid: facts.amount_paid,
        interval,
        period_start: facts.period_start,
        period_end: facts.period_end,
    })
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
    #[error("Stripe webhook signature is malformed")]
    MalformedSignature,
    #[error("Stripe webhook signature is stale")]
    StaleSignature,
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
    #[error("Stripe invoice line is unsupported: {0}")]
    UnsupportedLine(&'static str),
    #[error("Stripe price is not one of the configured standard prices")]
    UnsupportedPrice,
    #[error("provider context is invalid: {0}")]
    ProviderContext(ProviderAdapterError),
    #[error("provider evidence is invalid: {0}")]
    ProviderEvidence(ProviderAdapterError),
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

/// Verify and normalise a supported `invoice.paid` event with separately fetched settlement.
///
/// The webhook secret binds this event to the configured operator account. Direct events omit
/// `event.account`; any present account or Connect context is rejected by this contract.
pub fn decode_paid_invoice(
    raw_payload: &[u8],
    signature_header: &str,
    webhook_secret: &str,
    now: i64,
    config: &StripeCoverageConfig,
    binding: &StripeAllocationBinding,
    settlement: &StripePaymentSettlement,
) -> Result<StripeCoverageEvidence, StripeContractError> {
    let payload =
        std::str::from_utf8(raw_payload).map_err(|_| StripeContractError::MalformedPayload)?;
    verify_signature_detailed(webhook_secret, signature_header, payload, now).map_err(|error| {
        match error {
            SignatureVerificationError::Malformed => StripeContractError::MalformedSignature,
            SignatureVerificationError::Stale => StripeContractError::StaleSignature,
            SignatureVerificationError::Invalid => StripeContractError::InvalidSignature,
        }
    })?;

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
    if invoice.get("status").and_then(Value::as_str) != Some("paid") {
        return Err(StripeContractError::UnpaidInvoice);
    }
    let currency = required_string(invoice, "currency")?;
    let amount_paid = required_i64(invoice, "amount_paid")?;
    let amount_due = required_i64(invoice, "amount_due")?;
    let amount_overpaid = required_i64(invoice, "amount_overpaid")?;
    let amount_paid_off_stripe = required_i64(invoice, "amount_paid_off_stripe")?;

    let lines = invoice
        .get("lines")
        .and_then(|value| value.get("data"))
        .and_then(Value::as_array)
        .ok_or(StripeContractError::MissingField("lines.data"))?;
    if lines.len() != 1 {
        return Err(StripeContractError::UnsupportedQuantity);
    }
    let line = &lines[0];
    let invoice_line_id = required_ref(line, "id")?;
    let parent = line
        .get("parent")
        .and_then(Value::as_object)
        .ok_or(StripeContractError::MissingField("lines.data[0].parent"))?;
    if parent.get("type").and_then(Value::as_str) != Some("subscription_item_details") {
        return Err(StripeContractError::UnsupportedLine(
            "line is not generated by a subscription item",
        ));
    }
    let parent_details = parent
        .get("subscription_item_details")
        .and_then(Value::as_object)
        .ok_or(StripeContractError::MissingField(
            "lines.data[0].parent.subscription_item_details",
        ))?;
    let subscription_id = required_ref(&Value::Object(parent_details.clone()), "subscription")?;
    let provider_item_id =
        required_ref(&Value::Object(parent_details.clone()), "subscription_item")?;
    if required_i64(line, "quantity")? != 1 {
        return Err(StripeContractError::UnsupportedQuantity);
    }
    let pricing = line
        .get("pricing")
        .and_then(Value::as_object)
        .ok_or(StripeContractError::MissingField("lines.data[0].pricing"))?;
    if pricing.get("type").and_then(Value::as_str) != Some("price_details") {
        return Err(StripeContractError::UnsupportedPrice);
    }
    let price_details = pricing
        .get("price_details")
        .and_then(Value::as_object)
        .ok_or(StripeContractError::MissingField(
            "lines.data[0].pricing.price_details",
        ))?;
    let price_id = required_ref(&Value::Object(price_details.clone()), "price")?;
    let period = line
        .get("period")
        .and_then(Value::as_object)
        .ok_or(StripeContractError::MissingField("lines.data[0].period"))?;
    let period_start = required_i64(&Value::Object(period.clone()), "start")?;
    let period_end = required_i64(&Value::Object(period.clone()), "end")?;
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
    let invoice_subscription_id = optional_ref(invoice, "subscription")?;
    let observation = validate_personal_invoice_observation(
        config,
        binding,
        StripePersonalInvoiceFacts {
            invoice_id: invoice_id.clone(),
            customer_id: customer_id.clone(),
            invoice_subscription_id,
            subscription_id: subscription_id.clone(),
            provider_item_id: provider_item_id.clone(),
            invoice_line_id: invoice_line_id.clone(),
            allocation_reference: allocation_reference.clone(),
            price_id: price_id.clone(),
            currency: currency.clone(),
            amount_paid,
            amount_due,
            amount_overpaid,
            amount_paid_off_stripe,
            period_start,
            period_end,
            settlement: settlement.clone(),
        },
    )?;

    // Hash only event identity and trusted ownership. Invoice amounts and periods can change when
    // the provider corrects a remote object; they belong to refreshed history, not event identity.
    let normalized = json!({
        "allocation_reference": &observation.allocation_reference,
        "event_id": event.id,
        "event_type": event.event_type,
        "provider_created_at": event.created,
        "subscription_id": &observation.subscription_id,
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
    .map_err(StripeContractError::ProviderEvidence)?;

    Ok(StripeCoverageEvidence {
        event: verified_event,
        account_provenance,
        invoice_id: observation.invoice_id,
        customer_id: observation.customer_id,
        subscription_id: observation.subscription_id,
        provider_item_id: observation.provider_item_id,
        price_id,
        allocation_reference: observation.allocation_reference,
        payment_intent_id: observation.payment_intent_id,
        currency: observation.currency,
        amount_paid: observation.amount_paid,
        interval: observation.interval,
        period_start: observation.period_start,
        period_end: observation.period_end,
        evidence_reference: format!("stripe:invoice:{invoice_id}:line:{invoice_line_id}"),
    })
}

/// Decode the current Stripe Invoice Payment response fetched through an authenticated API call.
pub fn decode_invoice_payment(
    raw_payload: &[u8],
    config: &StripeCoverageConfig,
) -> Result<StripePaymentSettlement, StripeContractError> {
    let payment: Value =
        serde_json::from_slice(raw_payload).map_err(|_| StripeContractError::MalformedPayload)?;
    if payment.get("object").and_then(Value::as_str) != Some("invoice_payment") {
        return Err(StripeContractError::MalformedPayload);
    }
    let invoice_payment_id = required_ref(&payment, "id")?;
    let invoice_id = required_ref(&payment, "invoice")?;
    if required_string(&payment, "status")? != "paid" {
        return Err(StripeContractError::UnsupportedSettlement(
            "invoice payment is not paid",
        ));
    }
    let amount_paid = required_i64(&payment, "amount_paid")?;
    let amount_requested = required_i64(&payment, "amount_requested")?;
    let currency = required_string(&payment, "currency")?;
    if amount_paid <= 0 || amount_requested <= 0 || amount_paid != amount_requested {
        return Err(StripeContractError::UnsupportedSettlement(
            "invoice payment does not settle its requested amount",
        ));
    }
    let livemode = payment
        .get("livemode")
        .and_then(Value::as_bool)
        .ok_or(StripeContractError::MissingField("livemode"))?;
    if livemode != matches!(config.environment, ProviderEnvironment::Live) {
        return Err(StripeContractError::ContextMismatch);
    }
    let payment_details = payment
        .get("payment")
        .and_then(Value::as_object)
        .ok_or(StripeContractError::MissingField("payment"))?;
    if payment_details.get("type").and_then(Value::as_str) != Some("payment_intent") {
        return Err(StripeContractError::UnsupportedSettlement(
            "invoice payment is not backed by a PaymentIntent",
        ));
    }
    let payment_intent_id =
        required_ref(&Value::Object(payment_details.clone()), "payment_intent")?;
    Ok(StripePaymentSettlement {
        invoice_payment_id,
        invoice_id,
        payment_intent_id,
        amount_paid,
        amount_requested,
        currency,
        livemode,
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
    if event.account.is_some() {
        return Err(StripeContractError::ContextMismatch);
    }
    Ok(StripeAccountProvenance::OperatorAccount)
}

fn required_string(object: &Value, field: &'static str) -> Result<String, StripeContractError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .ok_or(StripeContractError::MissingField(field))
}

fn optional_ref(
    object: &Value,
    field: &'static str,
) -> Result<Option<String>, StripeContractError> {
    let Some(value) = object.get(field) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    if let Some(value) = value.as_str().filter(|value| !value.trim().is_empty()) {
        return Ok(Some(value.to_owned()));
    }
    if let Some(value) = value
        .get("id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
    {
        return Ok(Some(value.to_owned()));
    }
    Err(StripeContractError::InvalidField(field))
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
