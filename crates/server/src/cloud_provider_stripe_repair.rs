//! Authenticated Stripe event evidence for repairing a missed renewal delivery.
//!
//! This boundary is deliberately narrower than the signed webhook decoder. The API client has
//! authenticated the account and mode, so this module validates the returned event shape and
//! records that transport provenance. It does not claim a paid predecessor; the later repair
//! linker must supply bounded history and perform that exact boundary check.

#![allow(dead_code)]

use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::billing::webhook_version_accepted;
use crate::cloud_provider::ProviderEnvironment;
use crate::cloud_provider_stripe::{
    StripeAllocationBinding, StripeContractError, StripeCoverageConfig, StripeInterval,
    STRIPE_ALLOCATION_METADATA_KEY,
};
use crate::cloud_provider_stripe_http::{StripeReadClient, StripeReadError, StripeReadSession};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StripeRepairProvenance {
    event_id: String,
    event_created_at: i64,
    api_version: String,
    account_id: String,
    environment: ProviderEnvironment,
    payload_hash: String,
    retrieved_at: i64,
}

impl StripeRepairProvenance {
    pub(crate) fn event_id(&self) -> &str {
        &self.event_id
    }
    pub(crate) fn event_created_at(&self) -> i64 {
        self.event_created_at
    }
    pub(crate) fn api_version(&self) -> &str {
        &self.api_version
    }
    pub(crate) fn account_id(&self) -> &str {
        &self.account_id
    }
    pub(crate) fn environment(&self) -> ProviderEnvironment {
        self.environment
    }
    pub(crate) fn payload_hash(&self) -> &str {
        &self.payload_hash
    }
    pub(crate) fn retrieved_at(&self) -> i64 {
        self.retrieved_at
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StripeRepairCandidate {
    provenance: StripeRepairProvenance,
    invoice_id: String,
    invoice_line_id: String,
    customer_id: String,
    subscription_id: String,
    provider_item_id: String,
    allocation_reference: String,
    period_start: i64,
    period_end: i64,
    interval: StripeInterval,
}

impl StripeRepairCandidate {
    pub(crate) fn provenance(&self) -> &StripeRepairProvenance {
        &self.provenance
    }
    pub(crate) fn invoice_id(&self) -> &str {
        &self.invoice_id
    }
    pub(crate) fn invoice_line_id(&self) -> &str {
        &self.invoice_line_id
    }
    pub(crate) fn customer_id(&self) -> &str {
        &self.customer_id
    }
    pub(crate) fn subscription_id(&self) -> &str {
        &self.subscription_id
    }
    pub(crate) fn provider_item_id(&self) -> &str {
        &self.provider_item_id
    }
    pub(crate) fn allocation_reference(&self) -> &str {
        &self.allocation_reference
    }
    pub(crate) fn period_start(&self) -> i64 {
        self.period_start
    }
    pub(crate) fn period_end(&self) -> i64 {
        self.period_end
    }
    pub(crate) fn interval(&self) -> StripeInterval {
        self.interval
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn for_test(
        event_id: &str,
        event_created_at: i64,
        account_id: &str,
        environment: ProviderEnvironment,
        customer_id: &str,
        subscription_id: &str,
        provider_item_id: &str,
        allocation_reference: &str,
        invoice_id: &str,
        invoice_line_id: &str,
        period_start: i64,
        period_end: i64,
        interval: StripeInterval,
    ) -> Self {
        Self {
            provenance: StripeRepairProvenance {
                event_id: event_id.into(),
                event_created_at,
                api_version: crate::billing::STRIPE_API_VERSION.into(),
                account_id: account_id.into(),
                environment,
                payload_hash: "0".repeat(64),
                retrieved_at: event_created_at,
            },
            invoice_id: invoice_id.into(),
            invoice_line_id: invoice_line_id.into(),
            customer_id: customer_id.into(),
            subscription_id: subscription_id.into(),
            provider_item_id: provider_item_id.into(),
            allocation_reference: allocation_reference.into(),
            period_start,
            period_end,
            interval,
        }
    }
}

#[derive(Debug, Error)]
pub(crate) enum StripeRepairError {
    #[error("repair event is malformed: {0}")]
    Malformed(&'static str),
    #[error("repair event is not a supported automatic renewal: {0}")]
    Unsupported(&'static str),
    #[error("repair event does not belong to the allocation")]
    OwnershipMismatch,
    #[error("repair event has an unsupported API version: {0}")]
    UnsupportedApiVersion(String),
    #[error("repair event contains an invalid field: {0}")]
    InvalidField(&'static str),
    #[error("repair event provider context is invalid")]
    ContextMismatch,
    #[error("repair event provider configuration is invalid: {0}")]
    Config(#[from] StripeContractError),
    #[error("repair event read failed: {0}")]
    Read(#[from] StripeReadError),
}

/// Decode an event returned by an authenticated Stripe API read.
pub(crate) fn decode_authenticated_repair_event(
    event: &Value,
    retrieved_at: i64,
    config: &StripeCoverageConfig,
    binding: &StripeAllocationBinding,
) -> Result<StripeRepairCandidate, StripeRepairError> {
    config.validate()?;
    if retrieved_at < 0 {
        return Err(StripeRepairError::InvalidField("retrieved_at"));
    }
    if binding.payer_kind() != crate::cloud_provider::PayerKind::Personal {
        return Err(StripeRepairError::Unsupported(
            "personal allocation required",
        ));
    }
    let event = event
        .as_object()
        .ok_or(StripeRepairError::Malformed("event"))?;
    let event_id = required_string(&Value::Object(event.clone()), "id")?;
    let event_created_at = required_i64(&Value::Object(event.clone()), "created")?;
    if event_created_at < 0 {
        return Err(StripeRepairError::InvalidField("created"));
    }
    let api_version = required_string(&Value::Object(event.clone()), "api_version")?;
    if !webhook_version_accepted(Some(&api_version)) {
        return Err(StripeRepairError::UnsupportedApiVersion(api_version));
    }
    if required_string(&Value::Object(event.clone()), "type")? != "invoice.payment_failed" {
        return Err(StripeRepairError::Unsupported("event type"));
    }
    if required_bool(&Value::Object(event.clone()), "livemode")?
        != matches!(config.environment, ProviderEnvironment::Live)
        || event.get("account").is_some_and(|value| !value.is_null())
        || event.get("context").is_some_and(|value| !value.is_null())
    {
        return Err(StripeRepairError::ContextMismatch);
    }
    let invoice = event
        .get("data")
        .and_then(Value::as_object)
        .and_then(|data| data.get("object"))
        .ok_or(StripeRepairError::Malformed("data.object"))?;
    if invoice.get("object").and_then(Value::as_str) != Some("invoice") {
        return Err(StripeRepairError::Unsupported("invoice object"));
    }
    if required_bool(invoice, "livemode")?
        != matches!(config.environment, ProviderEnvironment::Live)
    {
        return Err(StripeRepairError::ContextMismatch);
    }
    if required_string(invoice, "billing_reason")? != "subscription_cycle"
        || required_string(invoice, "collection_method")? != "charge_automatically"
    {
        return Err(StripeRepairError::Unsupported("automatic renewal"));
    }
    if required_string(invoice, "status")? != "open"
        || !required_string(invoice, "currency")?.eq_ignore_ascii_case("gbp")
    {
        return Err(StripeRepairError::Unsupported("failed invoice state"));
    }
    for field in [
        "amount_due",
        "amount_remaining",
        "amount_paid",
        "amount_overpaid",
        "amount_paid_off_stripe",
    ] {
        if required_i64(invoice, field)? < 0 {
            return Err(StripeRepairError::InvalidField(field));
        }
    }
    if required_i64(invoice, "amount_due")? == 0
        || required_i64(invoice, "amount_remaining")? != required_i64(invoice, "amount_due")?
        || required_i64(invoice, "amount_paid")? != 0
        || required_i64(invoice, "amount_overpaid")? != 0
        || required_i64(invoice, "amount_paid_off_stripe")? != 0
    {
        return Err(StripeRepairError::Unsupported("invoice settlement"));
    }
    let invoice_id = required_ref(invoice, "id")?;
    let customer_id = required_ref(invoice, "customer")?;
    if customer_id != binding.customer_id() {
        return Err(StripeRepairError::OwnershipMismatch);
    }
    let allocation_reference = invoice
        .get("metadata")
        .and_then(Value::as_object)
        .and_then(|metadata| metadata.get(STRIPE_ALLOCATION_METADATA_KEY))
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or(StripeRepairError::Malformed("allocation metadata"))?
        .to_owned();
    if allocation_reference != binding.allocation_reference() {
        return Err(StripeRepairError::OwnershipMismatch);
    }
    let parent = invoice
        .get("parent")
        .and_then(Value::as_object)
        .ok_or(StripeRepairError::Malformed("parent"))?;
    if parent.get("type").and_then(Value::as_str) != Some("subscription_details") {
        return Err(StripeRepairError::Unsupported("subscription parent"));
    }
    let subscription_id = parent
        .get("subscription_details")
        .and_then(Value::as_object)
        .and_then(|details| details.get("subscription"))
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or(StripeRepairError::Malformed("subscription"))?
        .to_owned();
    if subscription_id != binding.subscription_id() {
        return Err(StripeRepairError::OwnershipMismatch);
    }
    let lines = invoice
        .get("lines")
        .and_then(Value::as_object)
        .ok_or(StripeRepairError::Malformed("lines"))?;
    if lines.get("has_more").and_then(Value::as_bool) != Some(false) {
        return Err(StripeRepairError::Unsupported("truncated lines"));
    }
    let data = lines
        .get("data")
        .and_then(Value::as_array)
        .ok_or(StripeRepairError::Malformed("lines.data"))?;
    if data.len() != 1 {
        return Err(StripeRepairError::Unsupported("invoice quantity"));
    }
    let line = &data[0];
    if required_bool(line, "livemode")? != matches!(config.environment, ProviderEnvironment::Live)
        || required_i64(line, "quantity")? != 1
    {
        return Err(StripeRepairError::Unsupported("invoice line"));
    }
    let invoice_line_id = required_ref(line, "id")?;
    if required_ref(line, "invoice")? != invoice_id {
        return Err(StripeRepairError::OwnershipMismatch);
    }
    let details = line
        .get("parent")
        .and_then(Value::as_object)
        .and_then(|parent| parent.get("subscription_item_details"))
        .and_then(Value::as_object)
        .ok_or(StripeRepairError::Malformed("subscription item details"))?;
    let provider_item_id = required_ref(&Value::Object(details.clone()), "subscription_item")?;
    if provider_item_id != binding.provider_item_id() {
        return Err(StripeRepairError::OwnershipMismatch);
    }
    if details.get("proration").and_then(Value::as_bool) != Some(false) {
        return Err(StripeRepairError::Unsupported("proration"));
    }
    let price_id = line
        .get("pricing")
        .and_then(Value::as_object)
        .and_then(|pricing| pricing.get("price_details"))
        .and_then(Value::as_object)
        .and_then(|details| details.get("price"))
        .and_then(Value::as_str)
        .ok_or(StripeRepairError::Malformed("price"))?;
    let interval = if price_id == config.monthly_price_id {
        StripeInterval::Month
    } else if price_id == config.annual_price_id {
        StripeInterval::Year
    } else {
        return Err(StripeRepairError::Unsupported("price"));
    };
    let period = line
        .get("period")
        .and_then(Value::as_object)
        .ok_or(StripeRepairError::Malformed("period"))?;
    let period_start = required_i64(&Value::Object(period.clone()), "start")?;
    let period_end = required_i64(&Value::Object(period.clone()), "end")?;
    if period_start < 0 || period_end <= period_start {
        return Err(StripeRepairError::InvalidField("period"));
    }
    let payload_bytes = serde_json::to_vec(&Value::Object(event.clone()))
        .map_err(|_| StripeRepairError::Malformed("event encoding"))?;
    let payload_hash = hex_lower(&Sha256::digest(payload_bytes));
    Ok(StripeRepairCandidate {
        provenance: StripeRepairProvenance {
            event_id,
            event_created_at,
            api_version,
            account_id: config.account_id.clone(),
            environment: config.environment,
            payload_hash,
            retrieved_at,
        },
        invoice_id,
        invoice_line_id,
        customer_id,
        subscription_id,
        provider_item_id,
        allocation_reference,
        period_start,
        period_end,
        interval,
    })
}

/// Fetch and decode one event without holding a database connection across the provider read.
pub(crate) async fn fetch_authenticated_repair_candidate(
    client: &StripeReadClient,
    session: &mut StripeReadSession,
    event_id: &str,
    retrieved_at: i64,
    config: &StripeCoverageConfig,
    binding: &StripeAllocationBinding,
) -> Result<StripeRepairCandidate, StripeRepairError> {
    let event = client.event_payload(session, event_id).await?;
    decode_authenticated_repair_event(&event, retrieved_at, config, binding)
}

fn required_string(value: &Value, field: &'static str) -> Result<String, StripeRepairError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .ok_or(StripeRepairError::Malformed(field))
}
fn required_ref(value: &Value, field: &'static str) -> Result<String, StripeRepairError> {
    required_string(value, field)
}
fn required_i64(value: &Value, field: &'static str) -> Result<i64, StripeRepairError> {
    value
        .get(field)
        .and_then(Value::as_i64)
        .ok_or(StripeRepairError::Malformed(field))
}
fn required_bool(value: &Value, field: &'static str) -> Result<bool, StripeRepairError> {
    value
        .get(field)
        .and_then(Value::as_bool)
        .ok_or(StripeRepairError::Malformed(field))
}
fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::billing::STRIPE_API_VERSION;
    use crate::cloud_provider::PayerKind;
    use serde_json::json;

    fn config() -> StripeCoverageConfig {
        StripeCoverageConfig::new(
            "acct_test",
            ProviderEnvironment::Test,
            "price_month",
            "price_year",
        )
        .unwrap()
    }

    fn binding() -> StripeAllocationBinding {
        StripeAllocationBinding::new(
            "allocation:one",
            "cus_one",
            "sub_one",
            "si_one",
            PayerKind::Personal,
        )
        .unwrap()
    }

    fn event() -> Value {
        json!({
            "id": "evt_repaired",
            "created": 200,
            "api_version": STRIPE_API_VERSION,
            "type": "invoice.payment_failed",
            "livemode": false,
            "data": { "object": {
                "object": "invoice", "id": "in_failed", "customer": "cus_one",
                "livemode": false,
                "metadata": { "sotto_allocation_reference": "allocation:one" },
                "billing_reason": "subscription_cycle", "collection_method": "charge_automatically",
                "status": "open", "currency": "gbp", "amount_due": 1000,
                "amount_remaining": 1000, "amount_paid": 0, "amount_overpaid": 0,
                "amount_paid_off_stripe": 0, "parent": { "type": "subscription_details",
                    "subscription_details": { "subscription": "sub_one" } },
                "lines": { "object": "list", "has_more": false, "data": [{
                    "id": "il_failed", "invoice": "in_failed", "livemode": false, "quantity": 1,
                    "parent": { "type": "subscription_item_details", "subscription_item_details": {
                        "subscription": "sub_one", "subscription_item": "si_one", "proration": false } },
                    "pricing": { "type": "price_details", "price_details": { "price": "price_month" } },
                    "period": { "start": 100, "end": 200 }
                }] }
            }}
        })
    }

    #[test]
    fn authenticated_event_produces_bounded_repair_candidate() {
        let candidate =
            decode_authenticated_repair_event(&event(), 300, &config(), &binding()).unwrap();
        assert_eq!(candidate.invoice_id(), "in_failed");
        assert_eq!(candidate.period_start(), 100);
        assert_eq!(candidate.period_end(), 200);
        assert_eq!(candidate.interval(), StripeInterval::Month);
        assert_eq!(candidate.provenance().event_id(), "evt_repaired");
        assert_eq!(candidate.provenance().account_id(), "acct_test");
        assert!(!candidate.provenance().payload_hash().is_empty());
    }

    #[test]
    fn account_mode_and_open_shape_are_required() {
        let mut wrong_mode = event();
        wrong_mode["livemode"] = Value::Bool(true);
        assert!(matches!(
            decode_authenticated_repair_event(&wrong_mode, 300, &config(), &binding()),
            Err(StripeRepairError::ContextMismatch)
        ));

        let mut wrong_invoice_mode = event();
        wrong_invoice_mode["data"]["object"]["livemode"] = Value::Bool(true);
        assert!(matches!(
            decode_authenticated_repair_event(&wrong_invoice_mode, 300, &config(), &binding()),
            Err(StripeRepairError::ContextMismatch)
        ));

        let mut wrong_state = event();
        wrong_state["data"]["object"]["status"] = Value::String("paid".into());
        assert!(matches!(
            decode_authenticated_repair_event(&wrong_state, 300, &config(), &binding()),
            Err(StripeRepairError::Unsupported("failed invoice state"))
        ));
    }

    #[test]
    fn truncated_lines_and_initial_purchase_never_become_repair_candidates() {
        let mut truncated = event();
        truncated["data"]["object"]["lines"]["has_more"] = Value::Bool(true);
        assert!(matches!(
            decode_authenticated_repair_event(&truncated, 300, &config(), &binding()),
            Err(StripeRepairError::Unsupported("truncated lines"))
        ));

        let mut initial = event();
        initial["data"]["object"]["billing_reason"] = Value::String("subscription_create".into());
        assert!(matches!(
            decode_authenticated_repair_event(&initial, 300, &config(), &binding()),
            Err(StripeRepairError::Unsupported("automatic renewal"))
        ));
    }
}
