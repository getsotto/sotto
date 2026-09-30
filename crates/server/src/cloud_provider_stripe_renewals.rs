//! Pure, signed evidence for one failed automatic personal subscription renewal.
//!
//! The decoder consumes an authenticated invoice history and a signed
//! `invoice.payment_failed` snapshot. It never reads Stripe, consults a clock, writes a receipt,
//! or decides current entitlement. A linked result is historical evidence only.

use std::fmt;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::billing::webhook_version_accepted;
use crate::cloud_provider::ProviderEnvironment;
use crate::cloud_provider_stripe::{
    verify_webhook_signature, StripeAllocationBinding, StripeContractError, StripeCoverageConfig,
    StripeInterval, STRIPE_ALLOCATION_METADATA_KEY,
};
use crate::cloud_provider_stripe_http::{
    StripePersonalInvoiceHistory, StripePersonalInvoiceHistoryEntry,
};

/// A complete historical failure linked to the paid term immediately before it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeRenewalFailureEvidence {
    renewal_id: String,
    event_id: String,
    invoice_id: String,
    invoice_line_id: String,
    predecessor_invoice_id: String,
    predecessor_evidence_reference: String,
    provider_account_id: String,
    environment: ProviderEnvironment,
    allocation_reference: String,
    customer_id: String,
    subscription_id: String,
    provider_item_id: String,
    predecessor_period_start: i64,
    predecessor_period_end: i64,
    renewal_period_start: i64,
    renewal_period_end: i64,
    event_created_at: i64,
}

impl StripeRenewalFailureEvidence {
    pub fn renewal_id(&self) -> &str {
        &self.renewal_id
    }

    pub fn event_id(&self) -> &str {
        &self.event_id
    }

    pub fn invoice_id(&self) -> &str {
        &self.invoice_id
    }

    pub fn invoice_line_id(&self) -> &str {
        &self.invoice_line_id
    }

    pub fn predecessor_invoice_id(&self) -> &str {
        &self.predecessor_invoice_id
    }

    pub fn predecessor_evidence_reference(&self) -> &str {
        &self.predecessor_evidence_reference
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

    pub const fn predecessor_period_start(&self) -> i64 {
        self.predecessor_period_start
    }

    pub const fn predecessor_period_end(&self) -> i64 {
        self.predecessor_period_end
    }

    pub const fn renewal_period_start(&self) -> i64 {
        self.renewal_period_start
    }

    pub const fn renewal_period_end(&self) -> i64 {
        self.renewal_period_end
    }

    pub const fn event_created_at(&self) -> i64 {
        self.event_created_at
    }
}

/// A well-formed failure that cannot be linked without inventing historical evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StripeRenewalFailureNeedsEvidence {
    TruncatedInvoiceLines,
    MissingProrationProof,
    NoMatchingPaidPredecessor,
    AmbiguousPaidPredecessor,
}

impl fmt::Display for StripeRenewalFailureNeedsEvidence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::TruncatedInvoiceLines => "invoice line list is truncated",
            Self::MissingProrationProof => "invoice line is missing explicit non-proration proof",
            Self::NoMatchingPaidPredecessor => "no paid predecessor ends at the renewal boundary",
            Self::AmbiguousPaidPredecessor => {
                "multiple paid predecessors end at the renewal boundary"
            }
        };
        formatter.write_str(message)
    }
}

/// The decoder's typed result. Needs-evidence outcomes never expose a usable partial link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StripeRenewalFailureResult {
    Linked(Box<StripeRenewalFailureEvidence>),
    NeedsEvidence(StripeRenewalFailureNeedsEvidence),
}

/// Verify and link one signed automatic renewal failure to the exact prior paid term.
pub fn decode_personal_renewal_failure(
    raw_payload: &[u8],
    signature_header: &str,
    webhook_secret: &str,
    verification_now: i64,
    config: &StripeCoverageConfig,
    binding: &StripeAllocationBinding,
    history: &StripePersonalInvoiceHistory,
) -> Result<StripeRenewalFailureResult, StripeContractError> {
    config.validate()?;
    if verification_now < 0 {
        return Err(StripeContractError::InvalidField("verification_now"));
    }
    if history.account_id() != config.account_id
        || history.environment() != config.environment
        || history.subscription_id() != binding.subscription_id()
        || history.customer_id() != binding.customer_id()
    {
        return Err(StripeContractError::ContextMismatch);
    }
    if binding.payer_kind() != crate::cloud_provider::PayerKind::Personal {
        return Err(StripeContractError::UnsupportedPayerKind);
    }
    for entry in history.entries() {
        if let StripePersonalInvoiceHistoryEntry::Paid(term) = entry {
            if term.allocation_reference() != binding.allocation_reference()
                || term.customer_id() != binding.customer_id()
                || term.subscription_id() != binding.subscription_id()
                || term.provider_item_id() != binding.provider_item_id()
            {
                return Err(StripeContractError::OwnershipMismatch);
            }
        }
    }

    let payload =
        std::str::from_utf8(raw_payload).map_err(|_| StripeContractError::MalformedPayload)?;
    verify_webhook_signature(webhook_secret, signature_header, payload, verification_now)?;
    let event: Value =
        serde_json::from_slice(raw_payload).map_err(|_| StripeContractError::MalformedPayload)?;
    let event = event
        .as_object()
        .ok_or(StripeContractError::MalformedPayload)?;

    let event_id = required_string(&Value::Object(event.clone()), "id")?;
    let event_created_at = required_i64(&Value::Object(event.clone()), "created")?;
    if event_created_at < 0 {
        return Err(StripeContractError::InvalidField("created"));
    }
    let api_version = event
        .get("api_version")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| StripeContractError::UnsupportedApiVersion("missing".into()))?
        .to_owned();
    if !webhook_version_accepted(Some(&api_version)) {
        return Err(StripeContractError::UnsupportedApiVersion(api_version));
    }
    if required_string(&Value::Object(event.clone()), "type")? != "invoice.payment_failed" {
        return Err(StripeContractError::UnsupportedEventType(required_string(
            &Value::Object(event.clone()),
            "type",
        )?));
    }
    let livemode = required_bool(&Value::Object(event.clone()), "livemode")?;
    if livemode != is_live(config.environment)
        || event.get("account").is_some_and(|value| !value.is_null())
        || event.get("context").is_some_and(|value| !value.is_null())
    {
        return Err(StripeContractError::ContextMismatch);
    }

    let data = event
        .get("data")
        .and_then(Value::as_object)
        .ok_or(StripeContractError::MissingField("data"))?;
    let invoice = data
        .get("object")
        .ok_or(StripeContractError::MissingField("data.object"))?;
    if invoice.get("object").and_then(Value::as_str) != Some("invoice") {
        return Err(StripeContractError::UnsupportedRenewal(
            "event object is not an invoice",
        ));
    }

    let invoice_id = required_ref(invoice, "id")?;
    let customer_id = required_ref(invoice, "customer")?;
    if customer_id != binding.customer_id() {
        return Err(StripeContractError::OwnershipMismatch);
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
    if allocation_reference != binding.allocation_reference() {
        return Err(StripeContractError::OwnershipMismatch);
    }
    if required_string(invoice, "billing_reason")? != "subscription_cycle" {
        return Err(StripeContractError::UnsupportedRenewal(
            "invoice is not an automatic subscription cycle",
        ));
    }
    if required_string(invoice, "collection_method")? != "charge_automatically" {
        return Err(StripeContractError::UnsupportedRenewal(
            "invoice is not automatically collected",
        ));
    }
    if required_string(invoice, "status")? != "open" {
        return Err(StripeContractError::UnsupportedRenewal(
            "failed invoice is not open",
        ));
    }
    if !required_string(invoice, "currency")?.eq_ignore_ascii_case("gbp") {
        return Err(StripeContractError::UnsupportedSettlement(
            "renewal invoice currency is not GBP",
        ));
    }
    let amount_due = required_i64(invoice, "amount_due")?;
    let amount_remaining = required_i64(invoice, "amount_remaining")?;
    let amount_paid = required_i64(invoice, "amount_paid")?;
    let amount_overpaid = required_i64(invoice, "amount_overpaid")?;
    let amount_paid_off_stripe = required_i64(invoice, "amount_paid_off_stripe")?;
    for (amount, field) in [
        (amount_due, "amount_due"),
        (amount_remaining, "amount_remaining"),
        (amount_paid, "amount_paid"),
        (amount_overpaid, "amount_overpaid"),
        (amount_paid_off_stripe, "amount_paid_off_stripe"),
    ] {
        if amount < 0 {
            return Err(StripeContractError::InvalidField(field));
        }
    }
    if amount_due == 0 {
        return Err(StripeContractError::InvalidField("amount_due"));
    }
    if amount_remaining == 0 {
        return Err(StripeContractError::InvalidField("amount_remaining"));
    }
    if amount_paid != 0 || amount_overpaid != 0 || amount_paid_off_stripe != 0 {
        return Err(StripeContractError::UnsupportedSettlement(
            "renewal invoice has a partial or off-Stripe settlement",
        ));
    }
    if amount_remaining != amount_due {
        return Err(StripeContractError::UnsupportedSettlement(
            "renewal invoice remaining amount differs from amount due",
        ));
    }

    let parent = invoice
        .get("parent")
        .and_then(Value::as_object)
        .ok_or(StripeContractError::MissingField("parent"))?;
    if parent.get("type").and_then(Value::as_str) != Some("subscription_details") {
        return Err(StripeContractError::UnsupportedLine(
            "invoice is not generated by a subscription",
        ));
    }
    let subscription_details = parent
        .get("subscription_details")
        .and_then(Value::as_object)
        .ok_or(StripeContractError::MissingField(
            "parent.subscription_details",
        ))?;
    let subscription_id =
        required_ref(&Value::Object(subscription_details.clone()), "subscription")?;
    if subscription_id != binding.subscription_id() {
        return Err(StripeContractError::OwnershipMismatch);
    }
    if let Some(invoice_subscription_id) = optional_ref(invoice, "subscription")? {
        if invoice_subscription_id != subscription_id {
            return Err(StripeContractError::OwnershipMismatch);
        }
    }

    let lines = invoice
        .get("lines")
        .and_then(Value::as_object)
        .ok_or(StripeContractError::MissingField("lines"))?;
    if lines.get("object").and_then(Value::as_str) != Some("list") {
        return Err(StripeContractError::UnsupportedLine("lines is not a list"));
    }
    if lines.get("has_more").and_then(Value::as_bool) != Some(false) {
        if lines.get("has_more").is_some_and(Value::is_boolean) {
            return Ok(StripeRenewalFailureResult::NeedsEvidence(
                StripeRenewalFailureNeedsEvidence::TruncatedInvoiceLines,
            ));
        }
        return Err(StripeContractError::MissingField("lines.has_more"));
    }
    let line_values = lines
        .get("data")
        .and_then(Value::as_array)
        .ok_or(StripeContractError::MissingField("lines.data"))?;
    if line_values.len() != 1 {
        return Err(StripeContractError::UnsupportedQuantity);
    }
    let line = &line_values[0];
    let invoice_line_id = required_ref(line, "id")?;
    let line_invoice_id = required_ref(line, "invoice")?;
    if line_invoice_id != invoice_id {
        return Err(StripeContractError::OwnershipMismatch);
    }
    if required_bool(line, "livemode")? != is_live(config.environment) {
        return Err(StripeContractError::ContextMismatch);
    }
    if required_bool(invoice, "livemode")? != is_live(config.environment) {
        return Err(StripeContractError::ContextMismatch);
    }
    if required_i64(line, "quantity")? != 1 {
        return Err(StripeContractError::UnsupportedQuantity);
    }
    let line_parent = line
        .get("parent")
        .and_then(Value::as_object)
        .ok_or(StripeContractError::MissingField("lines.data[0].parent"))?;
    if line_parent.get("type").and_then(Value::as_str) != Some("subscription_item_details") {
        return Err(StripeContractError::UnsupportedLine(
            "line is not generated by a subscription item",
        ));
    }
    let line_details = line_parent
        .get("subscription_item_details")
        .and_then(Value::as_object)
        .ok_or(StripeContractError::MissingField(
            "lines.data[0].parent.subscription_item_details",
        ))?;
    let line_subscription_id = required_ref(&Value::Object(line_details.clone()), "subscription")?;
    let provider_item_id = required_ref(&Value::Object(line_details.clone()), "subscription_item")?;
    if line_subscription_id != subscription_id || line_subscription_id != binding.subscription_id()
    {
        return Err(StripeContractError::OwnershipMismatch);
    }
    if provider_item_id != binding.provider_item_id() {
        return Err(StripeContractError::OwnershipMismatch);
    }
    if let Some(top_level_subscription) = optional_ref(line, "subscription")? {
        if top_level_subscription != line_subscription_id {
            return Err(StripeContractError::OwnershipMismatch);
        }
    }
    if let Some(top_level_item) = optional_ref(line, "subscription_item")? {
        if top_level_item != provider_item_id {
            return Err(StripeContractError::OwnershipMismatch);
        }
    }
    let proration_proof = match line_details.get("proration") {
        None => None,
        Some(value) => Some(value.as_bool().ok_or(StripeContractError::InvalidField(
            "lines.data[0].parent.subscription_item_details.proration",
        ))?),
    };
    if proration_proof == Some(true) {
        return Err(StripeContractError::UnsupportedRenewal("proration"));
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
    if price_id != config.monthly_price_id && price_id != config.annual_price_id {
        return Err(StripeContractError::UnsupportedPrice);
    }
    let expected_interval = if price_id == config.monthly_price_id {
        StripeInterval::Month
    } else {
        StripeInterval::Year
    };
    let period = line
        .get("period")
        .and_then(Value::as_object)
        .ok_or(StripeContractError::MissingField("lines.data[0].period"))?;
    let renewal_period_start = required_i64(&Value::Object(period.clone()), "start")?;
    let renewal_period_end = required_i64(&Value::Object(period.clone()), "end")?;
    if renewal_period_start < 0 {
        return Err(StripeContractError::InvalidField(
            "lines.data[0].period.start",
        ));
    }
    if renewal_period_end <= renewal_period_start {
        return Err(StripeContractError::InvalidField(
            "lines.data[0].period.end",
        ));
    }
    if proration_proof.is_none() {
        return Ok(StripeRenewalFailureResult::NeedsEvidence(
            StripeRenewalFailureNeedsEvidence::MissingProrationProof,
        ));
    }

    let mut candidates = history
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            StripePersonalInvoiceHistoryEntry::Paid(term)
                if term.invoice_id() != invoice_id
                    && term.allocation_reference() == binding.allocation_reference()
                    && term.customer_id() == binding.customer_id()
                    && term.subscription_id() == binding.subscription_id()
                    && term.provider_item_id() == binding.provider_item_id()
                    && term.interval() == expected_interval
                    && term.period_end() == renewal_period_start =>
            {
                Some(term)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| left.invoice_id().cmp(right.invoice_id()));
    candidates.dedup_by(|left, right| left.invoice_id() == right.invoice_id());
    let term = match candidates.as_slice() {
        [] => {
            return Ok(StripeRenewalFailureResult::NeedsEvidence(
                StripeRenewalFailureNeedsEvidence::NoMatchingPaidPredecessor,
            ));
        }
        [term] => *term,
        _ => {
            return Ok(StripeRenewalFailureResult::NeedsEvidence(
                StripeRenewalFailureNeedsEvidence::AmbiguousPaidPredecessor,
            ));
        }
    };

    Ok(StripeRenewalFailureResult::Linked(Box::new(
        StripeRenewalFailureEvidence {
            renewal_id: renewal_identity(config, binding, &invoice_id),
            event_id,
            invoice_id,
            invoice_line_id,
            predecessor_invoice_id: term.invoice_id().to_owned(),
            predecessor_evidence_reference: term.evidence_reference().to_owned(),
            provider_account_id: config.account_id.clone(),
            environment: config.environment,
            allocation_reference,
            customer_id,
            subscription_id,
            provider_item_id,
            predecessor_period_start: term.period_start(),
            predecessor_period_end: term.period_end(),
            renewal_period_start,
            renewal_period_end,
            event_created_at,
        },
    )))
}

fn renewal_identity(
    config: &StripeCoverageConfig,
    binding: &StripeAllocationBinding,
    invoice_id: &str,
) -> String {
    let parts = [
        "stripe",
        "renewal",
        "v1",
        &config.account_id,
        config.environment.as_str(),
        binding.allocation_reference(),
        binding.subscription_id(),
        binding.provider_item_id(),
        invoice_id,
    ];
    let mut canonical = String::new();
    for part in parts {
        canonical.push_str(&part.len().to_string());
        canonical.push(':');
        canonical.push_str(part);
    }
    let mut digest = Sha256::new();
    digest.update(canonical.as_bytes());
    format!("stripe:renewal:{}", hex_lower(&digest.finalize()))
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn is_live(environment: ProviderEnvironment) -> bool {
    matches!(environment, ProviderEnvironment::Live)
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

fn required_i64(object: &Value, field: &'static str) -> Result<i64, StripeContractError> {
    object
        .get(field)
        .and_then(Value::as_i64)
        .ok_or(StripeContractError::MissingField(field))
}

fn required_bool(object: &Value, field: &'static str) -> Result<bool, StripeContractError> {
    object
        .get(field)
        .and_then(Value::as_bool)
        .ok_or(StripeContractError::MissingField(field))
}
