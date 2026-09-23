//! Verified provider evidence for person-level Cloud coverage.
//!
//! Provider HTTP and signature verification happen outside this module. This module accepts only
//! normalized, verified evidence, records an idempotent receipt, and applies the evidence through
//! the existing caller-owned reconciliation transaction. It never stores raw provider payloads.

use std::{collections::BTreeSet, fmt, time::Duration};

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Row, Transaction};
use thiserror::Error;
use tokio::time::timeout;

use crate::cloud_coverage::{normalise_confirmed_intervals, ConfirmedPaidInterval, PersonCoverage};
use crate::cloud_coverage_reconciliation::{
    begin_collection, finish_collection, register_source, CollectionStatus, ReconciliationError,
    SourceBinding, SourceObservation,
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
///
/// `observations` is a complete beneficiary snapshot: it must contain one observation for every
/// registered source. A provider event for one allocation therefore carries verified, carried
/// forward observations for sibling allocations rather than publishing a partial projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedCollection {
    pub aggregate_evidence_reference: String,
    pub observations: Vec<SourceObservation>,
}

/// The durable ticket that must be captured before provider history is fetched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectionPreparation {
    pub run_id: String,
    pub attempt_id: String,
    pub ticket: crate::cloud_coverage_reconciliation::CollectionTicket,
}

/// Bounds applied to one provider history collection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CollectionLimits {
    pub total_timeout: Duration,
    pub max_pages_per_source: usize,
    pub max_sources: usize,
    pub max_facts: usize,
    pub max_evidence_bytes: usize,
    pub request_timeout: Duration,
}

impl Default for CollectionLimits {
    fn default() -> Self {
        // Keep one collection bounded even when an adapter does not provide a tighter policy.
        Self {
            total_timeout: Duration::from_secs(120),
            max_pages_per_source: 64,
            max_sources: 32,
            max_facts: 10_000,
            max_evidence_bytes: 1024 * 1024,
            request_timeout: Duration::from_secs(10),
        }
    }
}

impl CollectionLimits {
    fn validate(self) -> Result<(), ProviderCollectionError> {
        if self.max_pages_per_source == 0
            || self.max_sources == 0
            || self.max_facts == 0
            || self.max_evidence_bytes == 0
            || self.total_timeout.is_zero()
            || self.request_timeout.is_zero()
        {
            return Err(ProviderCollectionError::InvalidLimits);
        }
        Ok(())
    }
}

/// One normalized page from a provider history endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderHistoryPage {
    pub context: ProviderContext,
    pub source_id: String,
    pub evidence_reference: String,
    pub paid_intervals: Vec<ConfirmedPaidInterval>,
    pub next_cursor: Option<String>,
    pub authoritative_end: bool,
}

/// Provider-neutral history transport. Implementations perform no database work.
#[async_trait]
pub trait ProviderHistoryClient: Send {
    async fn fetch_page(
        &mut self,
        context: &ProviderContext,
        binding: &SourceBinding,
        cursor: Option<&str>,
    ) -> Result<ProviderHistoryPage, ProviderCollectionError>;
}

#[derive(Debug, Error)]
pub enum ProviderCollectionError {
    #[error("provider collection limits must be nonzero")]
    InvalidLimits,
    #[error("provider history request timed out")]
    Timeout,
    #[error("provider history fetch failed: {0}")]
    Fetch(String),
    #[error("provider history page has the wrong context")]
    ContextMismatch,
    #[error("provider history page has the wrong source")]
    SourceMismatch,
    #[error("provider history pagination is incomplete")]
    MissingEnd,
    #[error("provider history cursor repeated or did not advance")]
    RepeatedCursor,
    #[error("provider history collection exceeded its configured bound")]
    BoundExceeded,
    #[error("provider history evidence is invalid: {0}")]
    InvalidEvidence(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventDisposition {
    Pending,
    AlreadyApplied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectionDisposition {
    Rejected,
    AlreadyRejected,
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
    #[error("provider collection must include one observation for every registered source")]
    IncompleteCollection,
    #[error("provider collection attempt was superseded")]
    CollectionSuperseded,
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

/// Permanently reject a recorded event after verified processing determines it cannot be applied.
///
/// Rejection is explicit and idempotent so poison events do not remain retry loops. The caller
/// owns the transaction and must commit the returned state transition.
pub async fn reject_verified_event(
    tx: &mut Transaction<'_, Postgres>,
    context: &ProviderContext,
    event: &VerifiedProviderEvent,
    rejection_code: &str,
) -> Result<RejectionDisposition, ProviderAdapterError> {
    context.validate()?;
    event.validate()?;
    validate_identifier(rejection_code, "rejection code")?;
    let receipt = sqlx::query(
        "SELECT event_type, provider_created_at, subscription_id, allocation_reference, \
                normalized_payload_hash, status, rejection_code \
         FROM cloud_provider_event_receipts \
         WHERE provider_namespace = $1 AND provider_account_id = $2 \
           AND provider_environment = $3 AND event_id = $4 FOR UPDATE",
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
    match status.as_str() {
        "pending" => {
            let updated = sqlx::query(
                "UPDATE cloud_provider_event_receipts SET status = 'rejected', \
                        rejection_code = $5, processed_at = now() \
                 WHERE provider_namespace = $1 AND provider_account_id = $2 \
                   AND provider_environment = $3 AND event_id = $4 AND status = 'pending'",
            )
            .bind(&context.namespace)
            .bind(&context.account_id)
            .bind(context.environment.as_str())
            .bind(&event.event_id)
            .bind(rejection_code)
            .execute(&mut **tx)
            .await?;
            if updated.rows_affected() != 1 {
                return Err(ProviderAdapterError::EventNotPending);
            }
            Ok(RejectionDisposition::Rejected)
        }
        "rejected" => {
            let stored_code: String = receipt.try_get("rejection_code")?;
            if stored_code == rejection_code {
                Ok(RejectionDisposition::AlreadyRejected)
            } else {
                Err(ProviderAdapterError::EventConflict)
            }
        }
        _ => Err(ProviderAdapterError::EventNotPending),
    }
}

/// Fetch a complete normalized history for every captured source without holding a SQL transaction.
pub async fn collect_provider_history<C: ProviderHistoryClient + ?Sized>(
    client: &mut C,
    context: &ProviderContext,
    bindings: &[SourceBinding],
    limits: CollectionLimits,
) -> Result<VerifiedCollection, ProviderCollectionError> {
    context
        .validate()
        .map_err(|error| ProviderCollectionError::InvalidEvidence(error.to_string()))?;
    limits.validate()?;
    timeout(
        limits.total_timeout,
        collect_provider_history_inner(client, context, bindings, limits),
    )
    .await
    .map_err(|_| ProviderCollectionError::Timeout)?
}

async fn collect_provider_history_inner<C: ProviderHistoryClient + ?Sized>(
    client: &mut C,
    context: &ProviderContext,
    bindings: &[SourceBinding],
    limits: CollectionLimits,
) -> Result<VerifiedCollection, ProviderCollectionError> {
    if bindings.is_empty() || bindings.len() > limits.max_sources {
        return Err(ProviderCollectionError::BoundExceeded);
    }
    let expected_sources = bindings
        .iter()
        .map(|binding| binding.source_id.as_str())
        .collect::<BTreeSet<_>>();
    if expected_sources.len() != bindings.len() {
        return Err(ProviderCollectionError::InvalidEvidence(
            "provider collection contains duplicate sources".into(),
        ));
    }

    let mut observations = Vec::with_capacity(bindings.len());
    let mut total_fact_count = 0;
    for binding in bindings {
        if binding.provider_namespace != context.namespace {
            return Err(ProviderCollectionError::ContextMismatch);
        }
        if binding.beneficiary_id.trim().is_empty()
            || binding.source_id.trim().is_empty()
            || binding.provider_namespace.trim().is_empty()
            || binding.external_allocation_reference.trim().is_empty()
            || binding.ownership_evidence_reference.trim().is_empty()
        {
            return Err(ProviderCollectionError::InvalidEvidence(
                "provider source binding contains an empty identifier".into(),
            ));
        }
        let mut cursor = None;
        let mut seen_cursors = BTreeSet::new();
        let mut page_count = 0;
        let mut fact_count = 0;
        let mut intervals = Vec::new();
        let mut evidence_references = Vec::new();
        loop {
            page_count += 1;
            if page_count > limits.max_pages_per_source {
                return Err(ProviderCollectionError::BoundExceeded);
            }
            let page = timeout(
                limits.request_timeout,
                client.fetch_page(context, binding, cursor.as_deref()),
            )
            .await
            .map_err(|_| ProviderCollectionError::Timeout)??;
            if page.context != *context {
                return Err(ProviderCollectionError::ContextMismatch);
            }
            if page.source_id != binding.source_id {
                return Err(ProviderCollectionError::SourceMismatch);
            }
            if page.evidence_reference.trim().is_empty()
                || page.paid_intervals.iter().any(|interval| {
                    interval.source_id != binding.source_id
                        || interval.coverage_id.trim().is_empty()
                        || interval.starts_at >= interval.paid_until
                })
            {
                return Err(ProviderCollectionError::InvalidEvidence(
                    "provider history page contains invalid source facts".into(),
                ));
            }
            if page.paid_intervals.is_empty() && page.next_cursor.is_some() {
                return Err(ProviderCollectionError::MissingEnd);
            }
            fact_count += page.paid_intervals.len();
            total_fact_count += page.paid_intervals.len();
            if fact_count > limits.max_facts || total_fact_count > limits.max_facts {
                return Err(ProviderCollectionError::BoundExceeded);
            }
            intervals.extend(page.paid_intervals);
            evidence_references.push(page.evidence_reference);
            match (page.authoritative_end, page.next_cursor) {
                (true, None) => break,
                (true, Some(_)) => return Err(ProviderCollectionError::MissingEnd),
                (false, None) => return Err(ProviderCollectionError::MissingEnd),
                (false, Some(next_cursor)) => {
                    if next_cursor.trim().is_empty()
                        || cursor.as_deref() == Some(next_cursor.as_str())
                        || !seen_cursors.insert(next_cursor.clone())
                    {
                        return Err(ProviderCollectionError::RepeatedCursor);
                    }
                    cursor = Some(next_cursor);
                }
            }
        }
        let normalized = normalise_confirmed_intervals(&PersonCoverage {
            beneficiary_id: binding.beneficiary_id.clone(),
            paid_intervals: intervals,
        })
        .map_err(|error| ProviderCollectionError::InvalidEvidence(error.to_string()))?;
        let evidence_material = serde_json::to_vec(&evidence_references)
            .map_err(|error| ProviderCollectionError::InvalidEvidence(error.to_string()))?;
        observations.push(SourceObservation::Complete {
            source_id: binding.source_id.clone(),
            evidence_reference: format!("provider-source-v1:{}", hex_sha256(&evidence_material)),
            paid_intervals: normalized,
        });
    }

    let aggregate_material = observations
        .iter()
        .map(|observation| match observation {
            SourceObservation::Complete {
                source_id,
                evidence_reference,
                paid_intervals,
            } => serde_json::json!({
                "source_id": source_id,
                "evidence_reference": evidence_reference,
                "paid_intervals": paid_intervals,
            }),
            SourceObservation::Unavailable { .. } => serde_json::json!({
                "source_id": "unavailable",
            }),
        })
        .collect::<Vec<_>>();
    let aggregate_material = serde_json::to_vec(&aggregate_material)
        .map_err(|error| ProviderCollectionError::InvalidEvidence(error.to_string()))?;
    if aggregate_material.len() > limits.max_evidence_bytes {
        return Err(ProviderCollectionError::BoundExceeded);
    }
    Ok(VerifiedCollection {
        aggregate_evidence_reference: format!(
            "provider-collection-v1:{}",
            hex_sha256(&aggregate_material)
        ),
        observations,
    })
}

/// Prepare and durably capture a collection attempt before any provider history is fetched.
pub async fn prepare_verified_event(
    tx: &mut Transaction<'_, Postgres>,
    context: &ProviderContext,
    event: &VerifiedProviderEvent,
    allocation: &VerifiedAllocation,
    run_id: &str,
) -> Result<CollectionPreparation, ProviderAdapterError> {
    context.validate()?;
    event.validate()?;
    allocation.validate()?;
    validate_identifier(run_id, "collection run")?;
    validate_event_allocation(event, allocation)?;
    let receipt = sqlx::query(
        "SELECT event_type, provider_created_at, subscription_id, allocation_reference, \
                normalized_payload_hash, status, collection_beneficiary_id, \
                collection_attempt_id, collection_run_id \
         FROM cloud_provider_event_receipts \
         WHERE provider_namespace = $1 AND provider_account_id = $2 \
           AND provider_environment = $3 AND event_id = $4 FOR UPDATE",
    )
    .bind(&context.namespace)
    .bind(&context.account_id)
    .bind(context.environment.as_str())
    .bind(&event.event_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(ProviderAdapterError::EventMissing)?;
    verify_stored_event(&receipt, event)?;
    let status: String = receipt.try_get("status")?;
    if status != "pending" {
        return Err(ProviderAdapterError::EventNotPending);
    }

    let attempt_id = scoped_identity(
        "provider-collection-v1",
        &[
            &context.namespace,
            &context.account_id,
            context.environment.as_str(),
            &event.event_id,
            run_id,
        ],
    );
    ensure_payer(tx, context, allocation).await?;
    ensure_allocation(tx, context, allocation).await?;
    let binding = source_binding(context, allocation);
    register_source(
        tx,
        &scoped_identity("provider-source-v1", &[&allocation.allocation_id]),
        &binding,
    )
    .await?;
    let ticket = begin_collection(tx, &allocation.beneficiary_id, &attempt_id).await?;
    if ticket.status != crate::cloud_coverage_reconciliation::CollectionStatus::Pending {
        return Err(ProviderAdapterError::EventConflict);
    }
    let updated = sqlx::query(
        "UPDATE cloud_provider_event_receipts SET collection_beneficiary_id = $5, \
                collection_attempt_id = $6, collection_run_id = $7 \
         WHERE provider_namespace = $1 AND provider_account_id = $2 \
           AND provider_environment = $3 AND event_id = $4 AND status = 'pending'",
    )
    .bind(&context.namespace)
    .bind(&context.account_id)
    .bind(context.environment.as_str())
    .bind(&event.event_id)
    .bind(&allocation.beneficiary_id)
    .bind(&attempt_id)
    .bind(run_id)
    .execute(&mut **tx)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(ProviderAdapterError::EventNotPending);
    }
    Ok(CollectionPreparation {
        run_id: run_id.into(),
        attempt_id,
        ticket,
    })
}

/// Complete the exact prepared attempt after provider history collection has committed outside SQL.
pub async fn complete_verified_event(
    tx: &mut Transaction<'_, Postgres>,
    context: &ProviderContext,
    event: &VerifiedProviderEvent,
    allocation: &VerifiedAllocation,
    preparation: &CollectionPreparation,
    collection: &VerifiedCollection,
) -> Result<ApplyReceipt, ProviderAdapterError> {
    context.validate()?;
    event.validate()?;
    allocation.validate()?;
    validate_collection(collection)?;
    validate_event_allocation(event, allocation)?;
    if preparation.ticket.status != CollectionStatus::Pending
        || preparation.ticket.completed_revision.is_some()
    {
        return Err(ProviderAdapterError::CollectionSuperseded);
    }
    let receipt = sqlx::query(
        "SELECT event_type, provider_created_at, subscription_id, allocation_reference, \
                normalized_payload_hash, status, collection_beneficiary_id, \
                collection_attempt_id, collection_run_id \
         FROM cloud_provider_event_receipts \
         WHERE provider_namespace = $1 AND provider_account_id = $2 \
           AND provider_environment = $3 AND event_id = $4 FOR UPDATE",
    )
    .bind(&context.namespace)
    .bind(&context.account_id)
    .bind(context.environment.as_str())
    .bind(&event.event_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(ProviderAdapterError::EventMissing)?;
    verify_stored_event(&receipt, event)?;
    let status: String = receipt.try_get("status")?;
    if status != "pending" {
        return Err(ProviderAdapterError::EventNotPending);
    }
    let stored_beneficiary: Option<String> = receipt.try_get("collection_beneficiary_id")?;
    let stored_attempt: Option<String> = receipt.try_get("collection_attempt_id")?;
    let stored_run: Option<String> = receipt.try_get("collection_run_id")?;
    if stored_beneficiary.as_deref() != Some(allocation.beneficiary_id.as_str())
        || stored_attempt.as_deref() != Some(preparation.attempt_id.as_str())
        || stored_run.as_deref() != Some(preparation.run_id.as_str())
        || preparation.ticket.attempt_id != preparation.attempt_id
    {
        return Err(ProviderAdapterError::CollectionSuperseded);
    }
    ensure_payer(tx, context, allocation).await?;
    ensure_allocation(tx, context, allocation).await?;
    validate_collection_sources(collection, &preparation.ticket.source_bindings)?;
    let completed = finish_collection(
        tx,
        &preparation.ticket,
        &collection.aggregate_evidence_reference,
        &collection.observations,
    )
    .await
    .map_err(|error| match error {
        ReconciliationError::AttemptSuperseded | ReconciliationError::CollectionConflict => {
            ProviderAdapterError::CollectionSuperseded
        }
        other => ProviderAdapterError::Reconciliation(other),
    })?;
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
    Ok(ApplyReceipt {
        event_id: event.event_id.clone(),
        allocation_id: allocation.allocation_id.clone(),
        source_id: allocation.source_id.clone(),
        revision: completed.revision,
        outcome: match completed.outcome {
            PublicationOutcome::Applied => ApplyDisposition::Applied,
            PublicationOutcome::AlreadyApplied => ApplyDisposition::AlreadyApplied,
        },
    })
}

/// Replay an already-applied event without fetching or creating a new collection attempt.
pub async fn replay_verified_event(
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
    validate_event_allocation(event, allocation)?;

    let receipt = sqlx::query(
        "SELECT event_type, provider_created_at, subscription_id, allocation_reference, \
                normalized_payload_hash, status, allocation_id, coverage_source_id, \
                projection_revision, collection_beneficiary_id, collection_attempt_id \
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
    if status != "applied" {
        return Err(ProviderAdapterError::EventNotPending);
    }
    let allocation_id: String = receipt.try_get("allocation_id")?;
    let source_id: String = receipt.try_get("coverage_source_id")?;
    let revision: i64 = receipt.try_get("projection_revision")?;
    let beneficiary_id: String = receipt
        .try_get::<Option<String>, _>("collection_beneficiary_id")?
        .ok_or(ProviderAdapterError::EventConflict)?;
    let attempt_id: String = receipt
        .try_get::<Option<String>, _>("collection_attempt_id")?
        .ok_or(ProviderAdapterError::EventConflict)?;
    if allocation_id != allocation.allocation_id
        || source_id != allocation.source_id
        || beneficiary_id != allocation.beneficiary_id
    {
        return Err(ProviderAdapterError::AllocationConflict);
    }
    ensure_payer(tx, context, allocation).await?;
    ensure_allocation(tx, context, allocation).await?;
    let ticket = begin_collection(tx, &beneficiary_id, &attempt_id).await?;
    if ticket.status != CollectionStatus::Completed {
        return Err(ProviderAdapterError::EventConflict);
    }
    validate_collection_sources(collection, &ticket.source_bindings)?;
    let completed = finish_collection(
        tx,
        &ticket,
        &collection.aggregate_evidence_reference,
        &collection.observations,
    )
    .await?;
    if completed.revision != revision || completed.outcome != PublicationOutcome::AlreadyApplied {
        return Err(ProviderAdapterError::EventConflict);
    }
    Ok(ApplyReceipt {
        event_id: event.event_id.clone(),
        allocation_id,
        source_id,
        revision,
        outcome: ApplyDisposition::AlreadyApplied,
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
    .await
    .map_err(map_provider_database_error)?;
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
        Err(error) => Err(map_provider_database_error(error)),
    }
}

fn map_provider_database_error(error: sqlx::Error) -> ProviderAdapterError {
    let code = match &error {
        sqlx::Error::Database(database) => database.code().map(|code| code.into_owned()),
        _ => None,
    };
    match code.as_deref() {
        Some("23505") => ProviderAdapterError::AllocationConflict,
        Some("23503" | "23514") => ProviderAdapterError::InvalidEvidence(
            "provider allocation references missing or invalid ownership data".into(),
        ),
        _ => ProviderAdapterError::Database(error),
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

fn validate_event_allocation(
    event: &VerifiedProviderEvent,
    allocation: &VerifiedAllocation,
) -> Result<(), ProviderAdapterError> {
    if event
        .subscription_id
        .as_deref()
        .is_some_and(|subscription_id| subscription_id != allocation.subscription_id)
        || event
            .allocation_reference
            .as_deref()
            .is_some_and(|reference| reference != allocation.external_allocation_reference)
    {
        return Err(ProviderAdapterError::EventConflict);
    }
    Ok(())
}

fn source_binding(context: &ProviderContext, allocation: &VerifiedAllocation) -> SourceBinding {
    SourceBinding {
        beneficiary_id: allocation.beneficiary_id.clone(),
        source_id: allocation.source_id.clone(),
        provider_namespace: context.namespace.clone(),
        external_allocation_reference: allocation.external_allocation_reference.clone(),
        ownership_evidence_reference: allocation.ownership_evidence_reference.clone(),
    }
}

fn verify_stored_event(
    row: &sqlx::postgres::PgRow,
    event: &VerifiedProviderEvent,
) -> Result<(), ProviderAdapterError> {
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
    Ok(())
}

fn validate_collection_sources(
    collection: &VerifiedCollection,
    bindings: &[SourceBinding],
) -> Result<(), ProviderAdapterError> {
    if collection.observations.len() != bindings.len() {
        return Err(ProviderAdapterError::IncompleteCollection);
    }
    let expected = bindings
        .iter()
        .map(|binding| binding.source_id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let observed = collection
        .observations
        .iter()
        .map(|observation| match observation {
            SourceObservation::Complete { source_id, .. }
            | SourceObservation::Unavailable { source_id, .. } => source_id.as_str(),
        })
        .collect::<std::collections::BTreeSet<_>>();
    if observed != expected {
        return Err(ProviderAdapterError::IncompleteCollection);
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

fn scoped_identity(prefix: &str, parts: &[&str]) -> String {
    let mut material = Vec::new();
    for part in parts {
        material.extend_from_slice(&(part.len() as u64).to_be_bytes());
        material.extend_from_slice(part.as_bytes());
    }
    format!("{prefix}:{}", hex_sha256(&material))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use async_trait::async_trait;

    use super::{
        collect_provider_history, CollectionLimits, ProviderCollectionError, ProviderContext,
        ProviderEnvironment, ProviderHistoryClient, ProviderHistoryPage, SourceBinding,
        VerifiedProviderEvent,
    };
    use crate::cloud_coverage::ConfirmedPaidInterval;

    fn binding() -> SourceBinding {
        SourceBinding {
            beneficiary_id: "person_1".into(),
            source_id: "source_1".into(),
            provider_namespace: "stripe".into(),
            external_allocation_reference: "allocation_1".into(),
            ownership_evidence_reference: "ownership_1".into(),
        }
    }

    fn limits() -> CollectionLimits {
        CollectionLimits {
            total_timeout: Duration::from_secs(2),
            max_pages_per_source: 4,
            max_sources: 2,
            max_facts: 8,
            max_evidence_bytes: 4096,
            request_timeout: Duration::from_secs(1),
        }
    }

    struct FakeClient {
        pages: Vec<ProviderHistoryPage>,
        index: usize,
    }

    #[async_trait]
    impl ProviderHistoryClient for FakeClient {
        async fn fetch_page(
            &mut self,
            _context: &ProviderContext,
            _binding: &SourceBinding,
            _cursor: Option<&str>,
        ) -> Result<ProviderHistoryPage, ProviderCollectionError> {
            let page = self.pages[self.index].clone();
            self.index += 1;
            Ok(page)
        }
    }

    fn page(
        context: &ProviderContext,
        evidence_reference: &str,
        paid_until: i64,
        next_cursor: Option<&str>,
        authoritative_end: bool,
    ) -> ProviderHistoryPage {
        ProviderHistoryPage {
            context: context.clone(),
            source_id: "source_1".into(),
            evidence_reference: evidence_reference.into(),
            paid_intervals: vec![ConfirmedPaidInterval {
                coverage_id: format!("coverage_{paid_until}"),
                source_id: "source_1".into(),
                starts_at: paid_until - 10,
                paid_until,
                failed_renewal_id: None,
            }],
            next_cursor: next_cursor.map(str::to_owned),
            authoritative_end,
        }
    }

    fn empty_page(
        context: &ProviderContext,
        next_cursor: Option<&str>,
        authoritative_end: bool,
    ) -> ProviderHistoryPage {
        ProviderHistoryPage {
            context: context.clone(),
            source_id: "source_1".into(),
            evidence_reference: "empty-evidence".into(),
            paid_intervals: Vec::new(),
            next_cursor: next_cursor.map(str::to_owned),
            authoritative_end,
        }
    }

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

    #[tokio::test]
    async fn collects_every_page_to_an_authoritative_end() {
        let context =
            ProviderContext::new("stripe", "acct_test", ProviderEnvironment::Test).unwrap();
        let mut client = FakeClient {
            pages: vec![
                page(&context, "evidence_1", 10, Some("cursor_1"), false),
                page(&context, "evidence_2", 20, None, true),
            ],
            index: 0,
        };
        let collection = collect_provider_history(&mut client, &context, &[binding()], limits())
            .await
            .unwrap();
        assert_eq!(collection.observations.len(), 1);
        assert!(collection
            .aggregate_evidence_reference
            .starts_with("provider-collection-v1:"));
        match &collection.observations[0] {
            crate::cloud_coverage_reconciliation::SourceObservation::Complete {
                paid_intervals,
                ..
            } => assert_eq!(paid_intervals.len(), 2),
            _ => panic!("expected complete source observation"),
        }
    }

    #[tokio::test]
    async fn rejects_non_progressing_pagination() {
        let context =
            ProviderContext::new("stripe", "acct_test", ProviderEnvironment::Test).unwrap();
        let mut client = FakeClient {
            pages: vec![
                page(&context, "evidence_1", 10, Some("cursor_1"), false),
                page(&context, "evidence_2", 20, Some("cursor_1"), false),
            ],
            index: 0,
        };
        let error = collect_provider_history(&mut client, &context, &[binding()], limits())
            .await
            .unwrap_err();
        assert!(matches!(error, ProviderCollectionError::RepeatedCursor));
    }

    #[tokio::test]
    async fn rejects_bindings_from_another_provider_namespace() {
        let context =
            ProviderContext::new("stripe", "acct_test", ProviderEnvironment::Test).unwrap();
        let mut mismatched = binding();
        mismatched.provider_namespace = "github".into();
        let mut client = FakeClient {
            pages: vec![],
            index: 0,
        };
        let error = collect_provider_history(&mut client, &context, &[mismatched], limits())
            .await
            .unwrap_err();
        assert!(matches!(error, ProviderCollectionError::ContextMismatch));
    }

    struct SlowClient;

    #[async_trait]
    impl ProviderHistoryClient for SlowClient {
        async fn fetch_page(
            &mut self,
            _context: &ProviderContext,
            _binding: &SourceBinding,
            _cursor: Option<&str>,
        ) -> Result<ProviderHistoryPage, ProviderCollectionError> {
            tokio::time::sleep(Duration::from_millis(20)).await;
            unreachable!("the request should be cancelled by the collection timeout");
        }
    }

    #[tokio::test]
    async fn bounds_each_provider_request() {
        let context =
            ProviderContext::new("stripe", "acct_test", ProviderEnvironment::Test).unwrap();
        let mut client = SlowClient;
        let short_limits = CollectionLimits {
            request_timeout: Duration::from_millis(1),
            ..limits()
        };
        let error = collect_provider_history(&mut client, &context, &[binding()], short_limits)
            .await
            .unwrap_err();
        assert!(matches!(error, ProviderCollectionError::Timeout));
    }

    #[tokio::test]
    async fn accepts_authoritative_empty_history_and_rejects_empty_continuation() {
        let context =
            ProviderContext::new("stripe", "acct_test", ProviderEnvironment::Test).unwrap();
        let mut empty = FakeClient {
            pages: vec![empty_page(&context, None, true)],
            index: 0,
        };
        let collection = collect_provider_history(&mut empty, &context, &[binding()], limits())
            .await
            .unwrap();
        match &collection.observations[0] {
            crate::cloud_coverage_reconciliation::SourceObservation::Complete {
                paid_intervals,
                ..
            } => assert!(paid_intervals.is_empty()),
            _ => panic!("expected complete empty source observation"),
        }

        let mut incomplete = FakeClient {
            pages: vec![empty_page(&context, Some("cursor_1"), false)],
            index: 0,
        };
        let error = collect_provider_history(&mut incomplete, &context, &[binding()], limits())
            .await
            .unwrap_err();
        assert!(matches!(error, ProviderCollectionError::MissingEnd));
    }
}
