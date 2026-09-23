//! Verified provider evidence for person-level Cloud coverage.
//!
//! Provider HTTP and signature verification happen outside this module. This module accepts only
//! normalized, verified evidence, records an idempotent receipt, and applies the evidence through
//! the existing caller-owned reconciliation transaction. It never stores raw provider payloads.

use std::fmt;

use sha2::{Digest, Sha256};
use sqlx::{Postgres, Row, Transaction};
use thiserror::Error;

use crate::cloud_coverage_reconciliation::{
    begin_collection, finish_collection, register_source, ReconciliationError, SourceBinding,
    SourceObservation,
};
use crate::cloud_coverage_store::PublicationOutcome;

/// The provider deployment mode is part of the identity boundary. Test evidence must never enter
/// a live projection, and vice versa.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderEnvironment {
    Test,
    Live,
}

impl ProviderEnvironment {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Test => "test",
            Self::Live => "live",
        }
    }

    pub fn parse(value: &str) -> Result<Self, ProviderAdapterError> {
        match value {
            "test" => Ok(Self::Test),
            "live" => Ok(Self::Live),
            _ => Err(ProviderAdapterError::InvalidEvidence(
                "provider environment must be test or live".into(),
            )),
        }
    }
}

impl fmt::Display for ProviderEnvironment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Provider account identity configured by the operator, never selected by a client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderContext {
    pub namespace: String,
    pub account_id: String,
    pub environment: ProviderEnvironment,
}

impl ProviderContext {
    pub fn new(
        namespace: impl Into<String>,
        account_id: impl Into<String>,
        environment: ProviderEnvironment,
    ) -> Result<Self, ProviderAdapterError> {
        let context = Self {
            namespace: namespace.into(),
            account_id: account_id.into(),
            environment,
        };
        context.validate()?;
        Ok(context)
    }

    fn validate(&self) -> Result<(), ProviderAdapterError> {
        validate_identifier(&self.namespace, "provider namespace")?;
        validate_identifier(&self.account_id, "provider account")
    }
}

/// A signature-verified, normalized provider event. The payload itself is intentionally absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedProviderEvent {
    pub event_id: String,
    pub event_type: String,
    pub provider_created_at: i64,
    pub subscription_id: Option<String>,
    pub allocation_reference: Option<String>,
    pub normalized_payload_hash: String,
}

impl VerifiedProviderEvent {
    pub fn from_payload(
        event_id: impl Into<String>,
        event_type: impl Into<String>,
        provider_created_at: i64,
        subscription_id: Option<String>,
        allocation_reference: Option<String>,
        normalized_payload: &[u8],
    ) -> Result<Self, ProviderAdapterError> {
        let event = Self {
            event_id: event_id.into(),
            event_type: event_type.into(),
            provider_created_at,
            subscription_id,
            allocation_reference,
            normalized_payload_hash: hex_sha256(normalized_payload),
        };
        event.validate()?;
        Ok(event)
    }

    fn validate(&self) -> Result<(), ProviderAdapterError> {
        validate_identifier(&self.event_id, "provider event")?;
        validate_identifier(&self.event_type, "provider event type")?;
        if self.provider_created_at < 0 {
            return Err(ProviderAdapterError::InvalidEvidence(
                "provider event timestamp must not be negative".into(),
            ));
        }
        validate_optional_identifier(self.subscription_id.as_deref(), "subscription")?;
        validate_optional_identifier(self.allocation_reference.as_deref(), "allocation")?;
        if self.normalized_payload_hash.len() != 64
            || !self
                .normalized_payload_hash
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(ProviderAdapterError::InvalidEvidence(
                "normalized payload hash must be lowercase sha256".into(),
            ));
        }
        Ok(())
    }
}

/// One named provider allocation that can authorize one coverage source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedAllocation {
    pub allocation_id: String,
    pub payer_id: String,
    pub provider_customer_id: String,
    pub payer_kind: PayerKind,
    pub beneficiary_id: String,
    pub subscription_id: String,
    pub provider_item_id: String,
    pub external_allocation_reference: String,
    pub source_id: String,
    pub effective_from: i64,
    pub effective_until: Option<i64>,
    pub state: AllocationState,
    pub ownership_evidence_reference: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayerKind {
    Personal,
    Sponsor,
}

impl PayerKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Personal => "personal",
            Self::Sponsor => "sponsor",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllocationState {
    Pending,
    Active,
    Ended,
}

impl AllocationState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Active => "active",
            Self::Ended => "ended",
        }
    }
}

impl VerifiedAllocation {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        allocation_id: impl Into<String>,
        payer_id: impl Into<String>,
        provider_customer_id: impl Into<String>,
        payer_kind: PayerKind,
        beneficiary_id: impl Into<String>,
        subscription_id: impl Into<String>,
        provider_item_id: impl Into<String>,
        external_allocation_reference: impl Into<String>,
        source_id: impl Into<String>,
        effective_from: i64,
        effective_until: Option<i64>,
        state: AllocationState,
        ownership_evidence_reference: impl Into<String>,
    ) -> Result<Self, ProviderAdapterError> {
        let allocation = Self {
            allocation_id: allocation_id.into(),
            payer_id: payer_id.into(),
            provider_customer_id: provider_customer_id.into(),
            payer_kind,
            beneficiary_id: beneficiary_id.into(),
            subscription_id: subscription_id.into(),
            provider_item_id: provider_item_id.into(),
            external_allocation_reference: external_allocation_reference.into(),
            source_id: source_id.into(),
            effective_from,
            effective_until,
            state,
            ownership_evidence_reference: ownership_evidence_reference.into(),
        };
        allocation.validate()?;
        Ok(allocation)
    }

    fn validate(&self) -> Result<(), ProviderAdapterError> {
        for (value, name) in [
            (&self.allocation_id, "allocation"),
            (&self.payer_id, "payer"),
            (&self.provider_customer_id, "provider customer"),
            (&self.beneficiary_id, "beneficiary"),
            (&self.subscription_id, "subscription"),
            (&self.provider_item_id, "provider item"),
            (&self.external_allocation_reference, "external allocation"),
            (&self.source_id, "coverage source"),
            (&self.ownership_evidence_reference, "ownership evidence"),
        ] {
            validate_identifier(value, name)?;
        }
        if self.effective_from < 0
            || self
                .effective_until
                .is_some_and(|until| until <= self.effective_from)
        {
            return Err(ProviderAdapterError::InvalidEvidence(
                "allocation effective interval is invalid".into(),
            ));
        }
        Ok(())
    }
}

/// Normalized provider facts handed to the existing reconciliation boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedCollection {
    pub aggregate_evidence_reference: String,
    pub observations: Vec<SourceObservation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventDisposition {
    Pending,
    AlreadyApplied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyDisposition {
    Applied,
    AlreadyApplied,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyReceipt {
    pub event_id: String,
    pub allocation_id: String,
    pub source_id: String,
    pub revision: i64,
    pub outcome: ApplyDisposition,
}

#[derive(Debug, Error)]
pub enum ProviderAdapterError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("reconciliation error: {0}")]
    Reconciliation(#[from] ReconciliationError),
    #[error("invalid provider evidence: {0}")]
    InvalidEvidence(String),
    #[error("provider event conflicts with an existing receipt")]
    EventConflict,
    #[error("provider event is not pending")]
    EventNotPending,
    #[error("provider allocation conflicts with an existing owner")]
    AllocationConflict,
    #[error("provider event is missing")]
    EventMissing,
}

/// Record verified event identity before provider history collection begins.
pub async fn record_verified_event(
    tx: &mut Transaction<'_, Postgres>,
    context: &ProviderContext,
    event: &VerifiedProviderEvent,
) -> Result<EventDisposition, ProviderAdapterError> {
    context.validate()?;
    event.validate()?;
    let inserted = sqlx::query(
        "INSERT INTO cloud_provider_event_receipts \
         (provider_namespace, provider_account_id, provider_environment, event_id, event_type, \
          provider_created_at, subscription_id, allocation_reference, normalized_payload_hash, status) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'pending') \
         ON CONFLICT (provider_namespace, provider_account_id, provider_environment, event_id) \
         DO NOTHING",
    )
    .bind(&context.namespace)
    .bind(&context.account_id)
    .bind(context.environment.as_str())
    .bind(&event.event_id)
    .bind(&event.event_type)
    .bind(event.provider_created_at)
    .bind(event.subscription_id.as_deref())
    .bind(event.allocation_reference.as_deref())
    .bind(&event.normalized_payload_hash)
    .execute(&mut **tx)
    .await?
    .rows_affected()
        == 1;
    if inserted {
        return Ok(EventDisposition::Pending);
    }

    let row = sqlx::query(
        "SELECT event_type, provider_created_at, subscription_id, allocation_reference, \
                normalized_payload_hash, status \
         FROM cloud_provider_event_receipts \
         WHERE provider_namespace = $1 AND provider_account_id = $2 \
           AND provider_environment = $3 AND event_id = $4",
    )
    .bind(&context.namespace)
    .bind(&context.account_id)
    .bind(context.environment.as_str())
    .bind(&event.event_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(ProviderAdapterError::EventMissing)?;
    let stored_hash: String = row.try_get("normalized_payload_hash")?;
    let stored_type: String = row.try_get("event_type")?;
    let stored_created: i64 = row.try_get("provider_created_at")?;
    let stored_subscription: Option<String> = row.try_get("subscription_id")?;
    let stored_allocation: Option<String> = row.try_get("allocation_reference")?;
    if stored_hash != event.normalized_payload_hash
        || stored_type != event.event_type
        || stored_created != event.provider_created_at
        || stored_subscription != event.subscription_id
        || stored_allocation != event.allocation_reference
    {
        return Err(ProviderAdapterError::EventConflict);
    }
    let status: String = row.try_get("status")?;
    if status == "applied" {
        Ok(EventDisposition::AlreadyApplied)
    } else if status == "pending" {
        Ok(EventDisposition::Pending)
    } else {
        Err(ProviderAdapterError::EventNotPending)
    }
}

/// Apply one pending event and normalized collection atomically with reconciliation.
pub async fn apply_verified_event(
    tx: &mut Transaction<'_, Postgres>,
    context: &ProviderContext,
    event: &VerifiedProviderEvent,
    allocation: &VerifiedAllocation,
    collection: &VerifiedCollection,
) -> Result<ApplyReceipt, ProviderAdapterError> {
    context.validate()?;
    event.validate()?;
    allocation.validate()?;
    validate_collection(collection)?;
    if event
        .subscription_id
        .as_deref()
        .is_some_and(|subscription_id| subscription_id != allocation.subscription_id)
    {
        return Err(ProviderAdapterError::EventConflict);
    }
    if event
        .allocation_reference
        .as_deref()
        .is_some_and(|reference| reference != allocation.external_allocation_reference)
    {
        return Err(ProviderAdapterError::EventConflict);
    }

    let receipt = sqlx::query(
        "SELECT event_type, provider_created_at, subscription_id, allocation_reference, \
                normalized_payload_hash, status, allocation_id, coverage_source_id, \
                projection_revision \
         FROM cloud_provider_event_receipts \
         WHERE provider_namespace = $1 AND provider_account_id = $2 \
           AND provider_environment = $3 AND event_id = $4 \
         FOR UPDATE",
    )
    .bind(&context.namespace)
    .bind(&context.account_id)
    .bind(context.environment.as_str())
    .bind(&event.event_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(ProviderAdapterError::EventMissing)?;
    let stored_hash: String = receipt.try_get("normalized_payload_hash")?;
    let stored_type: String = receipt.try_get("event_type")?;
    let stored_created: i64 = receipt.try_get("provider_created_at")?;
    let stored_subscription: Option<String> = receipt.try_get("subscription_id")?;
    let stored_allocation: Option<String> = receipt.try_get("allocation_reference")?;
    if stored_hash != event.normalized_payload_hash
        || stored_type != event.event_type
        || stored_created != event.provider_created_at
        || stored_subscription != event.subscription_id
        || stored_allocation != event.allocation_reference
    {
        return Err(ProviderAdapterError::EventConflict);
    }
    let status: String = receipt.try_get("status")?;
    if status == "applied" {
        let allocation_id: String = receipt.try_get("allocation_id")?;
        let source_id: String = receipt.try_get("coverage_source_id")?;
        let revision: i64 = receipt.try_get("projection_revision")?;
        if allocation_id != allocation.allocation_id || source_id != allocation.source_id {
            return Err(ProviderAdapterError::AllocationConflict);
        }
        return Ok(ApplyReceipt {
            event_id: event.event_id.clone(),
            allocation_id,
            source_id,
            revision,
            outcome: ApplyDisposition::AlreadyApplied,
        });
    }
    if status != "pending" {
        return Err(ProviderAdapterError::EventNotPending);
    }

    ensure_payer(tx, context, allocation).await?;
    ensure_allocation(tx, context, allocation).await?;
    let binding = SourceBinding {
        beneficiary_id: allocation.beneficiary_id.clone(),
        source_id: allocation.source_id.clone(),
        provider_namespace: context.namespace.clone(),
        external_allocation_reference: allocation.external_allocation_reference.clone(),
        ownership_evidence_reference: allocation.ownership_evidence_reference.clone(),
    };
    let registration =
        register_source(tx, &format!("provider-event:{}", event.event_id), &binding).await?;
    let ticket = begin_collection(
        tx,
        &allocation.beneficiary_id,
        &format!("provider-event:{}", event.event_id),
    )
    .await?;
    let completed = finish_collection(
        tx,
        &ticket,
        &collection.aggregate_evidence_reference,
        &collection.observations,
    )
    .await?;
    let updated = sqlx::query(
        "UPDATE cloud_provider_event_receipts SET status = 'applied', processed_at = now(), \
                allocation_id = $5, coverage_source_id = $6, projection_revision = $7 \
         WHERE provider_namespace = $1 AND provider_account_id = $2 \
           AND provider_environment = $3 AND event_id = $4 AND status = 'pending'",
    )
    .bind(&context.namespace)
    .bind(&context.account_id)
    .bind(context.environment.as_str())
    .bind(&event.event_id)
    .bind(&allocation.allocation_id)
    .bind(&allocation.source_id)
    .bind(completed.revision)
    .execute(&mut **tx)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(ProviderAdapterError::EventNotPending);
    }
    let outcome = match (registration.outcome, completed.outcome) {
        (_, PublicationOutcome::AlreadyApplied) => ApplyDisposition::AlreadyApplied,
        _ => ApplyDisposition::Applied,
    };
    Ok(ApplyReceipt {
        event_id: event.event_id.clone(),
        allocation_id: allocation.allocation_id.clone(),
        source_id: allocation.source_id.clone(),
        revision: completed.revision,
        outcome,
    })
}

async fn ensure_payer(
    tx: &mut Transaction<'_, Postgres>,
    context: &ProviderContext,
    allocation: &VerifiedAllocation,
) -> Result<(), ProviderAdapterError> {
    let result = sqlx::query(
        "INSERT INTO cloud_provider_payers \
         (payer_id, provider_namespace, provider_account_id, provider_environment, \
          provider_customer_id, payer_kind) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (payer_id) DO UPDATE SET \
           provider_namespace = EXCLUDED.provider_namespace, \
           provider_account_id = EXCLUDED.provider_account_id, \
           provider_environment = EXCLUDED.provider_environment, \
           provider_customer_id = EXCLUDED.provider_customer_id, \
           payer_kind = EXCLUDED.payer_kind, updated_at = now() \
         WHERE cloud_provider_payers.provider_namespace = EXCLUDED.provider_namespace \
           AND cloud_provider_payers.provider_account_id = EXCLUDED.provider_account_id \
           AND cloud_provider_payers.provider_environment = EXCLUDED.provider_environment \
           AND cloud_provider_payers.provider_customer_id = EXCLUDED.provider_customer_id \
           AND cloud_provider_payers.payer_kind = EXCLUDED.payer_kind",
    )
    .bind(&allocation.payer_id)
    .bind(&context.namespace)
    .bind(&context.account_id)
    .bind(context.environment.as_str())
    .bind(&allocation.provider_customer_id)
    .bind(allocation.payer_kind.as_str())
    .execute(&mut **tx)
    .await?;
    if result.rows_affected() == 1 {
        return Ok(());
    }
    let row = sqlx::query(
        "SELECT provider_namespace, provider_account_id, provider_environment, \
                provider_customer_id, payer_kind \
         FROM cloud_provider_payers WHERE payer_id = $1",
    )
    .bind(&allocation.payer_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(ProviderAdapterError::AllocationConflict)?;
    let same = row.try_get::<String, _>("provider_namespace")? == context.namespace
        && row.try_get::<String, _>("provider_account_id")? == context.account_id
        && row.try_get::<String, _>("provider_environment")? == context.environment.as_str()
        && row.try_get::<String, _>("provider_customer_id")? == allocation.provider_customer_id
        && row.try_get::<String, _>("payer_kind")? == allocation.payer_kind.as_str();
    if same {
        Ok(())
    } else {
        Err(ProviderAdapterError::AllocationConflict)
    }
}

async fn ensure_allocation(
    tx: &mut Transaction<'_, Postgres>,
    context: &ProviderContext,
    allocation: &VerifiedAllocation,
) -> Result<(), ProviderAdapterError> {
    let result = sqlx::query(
        "INSERT INTO cloud_provider_allocations \
         (allocation_id, payer_id, beneficiary_id, provider_namespace, provider_account_id, \
          provider_environment, provider_subscription_id, provider_item_id, \
          external_allocation_reference, coverage_source_id, effective_from, effective_until, \
          state, ownership_evidence_reference) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14) \
         ON CONFLICT (allocation_id) DO NOTHING",
    )
    .bind(&allocation.allocation_id)
    .bind(&allocation.payer_id)
    .bind(&allocation.beneficiary_id)
    .bind(&context.namespace)
    .bind(&context.account_id)
    .bind(context.environment.as_str())
    .bind(&allocation.subscription_id)
    .bind(&allocation.provider_item_id)
    .bind(&allocation.external_allocation_reference)
    .bind(&allocation.source_id)
    .bind(allocation.effective_from)
    .bind(allocation.effective_until)
    .bind(allocation.state.as_str())
    .bind(&allocation.ownership_evidence_reference)
    .execute(&mut **tx)
    .await;
    match result {
        Ok(result) if result.rows_affected() == 1 => Ok(()),
        Ok(_) => {
            let row = sqlx::query(
                "SELECT payer_id, beneficiary_id, provider_namespace, provider_account_id, \
                        provider_environment, provider_subscription_id, provider_item_id, \
                        external_allocation_reference, coverage_source_id, effective_from, \
                        effective_until, state, ownership_evidence_reference \
                 FROM cloud_provider_allocations WHERE allocation_id = $1",
            )
            .bind(&allocation.allocation_id)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(ProviderAdapterError::AllocationConflict)?;
            let same = row.try_get::<String, _>("payer_id")? == allocation.payer_id
                && row.try_get::<String, _>("beneficiary_id")? == allocation.beneficiary_id
                && row.try_get::<String, _>("provider_namespace")? == context.namespace
                && row.try_get::<String, _>("provider_account_id")? == context.account_id
                && row.try_get::<String, _>("provider_environment")?
                    == context.environment.as_str()
                && row.try_get::<String, _>("provider_subscription_id")?
                    == allocation.subscription_id
                && row.try_get::<String, _>("provider_item_id")? == allocation.provider_item_id
                && row.try_get::<String, _>("external_allocation_reference")?
                    == allocation.external_allocation_reference
                && row.try_get::<String, _>("coverage_source_id")? == allocation.source_id;
            let same = same
                && row.try_get::<i64, _>("effective_from")? == allocation.effective_from
                && row.try_get::<Option<i64>, _>("effective_until")? == allocation.effective_until
                && row.try_get::<String, _>("state")? == allocation.state.as_str()
                && row.try_get::<String, _>("ownership_evidence_reference")?
                    == allocation.ownership_evidence_reference;
            if same {
                Ok(())
            } else {
                Err(ProviderAdapterError::AllocationConflict)
            }
        }
        Err(sqlx::Error::Database(_)) => Err(ProviderAdapterError::AllocationConflict),
        Err(error) => Err(ProviderAdapterError::Database(error)),
    }
}

fn validate_collection(collection: &VerifiedCollection) -> Result<(), ProviderAdapterError> {
    validate_identifier(
        &collection.aggregate_evidence_reference,
        "aggregate evidence",
    )?;
    if collection.observations.is_empty() {
        return Err(ProviderAdapterError::InvalidEvidence(
            "provider collection must contain one observation per registered source".into(),
        ));
    }
    Ok(())
}

fn validate_identifier(value: &str, name: &str) -> Result<(), ProviderAdapterError> {
    if value.trim().is_empty() {
        return Err(ProviderAdapterError::InvalidEvidence(format!(
            "{name} must not be empty"
        )));
    }
    Ok(())
}

fn validate_optional_identifier(
    value: Option<&str>,
    name: &str,
) -> Result<(), ProviderAdapterError> {
    if let Some(value) = value {
        validate_identifier(value, name)?;
    }
    Ok(())
}

fn hex_sha256(payload: &[u8]) -> String {
    let digest = Sha256::digest(payload);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::{ProviderContext, ProviderEnvironment, VerifiedProviderEvent};

    #[test]
    fn hashes_normalized_payload_without_retaining_it() {
        let event = VerifiedProviderEvent::from_payload(
            "evt_1",
            "invoice.paid",
            10,
            Some("sub_1".into()),
            Some("allocation_1".into()),
            br#"{"status":"paid"}"#,
        )
        .unwrap();
        assert_eq!(event.normalized_payload_hash.len(), 64);
        assert_eq!(event.event_id, "evt_1");
    }

    #[test]
    fn rejects_invalid_context_and_event_timestamps() {
        assert!(ProviderContext::new("stripe", "", ProviderEnvironment::Test).is_err());
        assert!(VerifiedProviderEvent::from_payload("evt", "paid", -1, None, None, b"{}").is_err());
    }

    #[test]
    fn accepts_only_known_environments() {
        assert_eq!(
            ProviderEnvironment::parse("test").unwrap(),
            ProviderEnvironment::Test
        );
        assert!(ProviderEnvironment::parse("sandbox").is_err());
    }
}
