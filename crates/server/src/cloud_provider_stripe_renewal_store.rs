//! Caller-owned persistence for verified personal Stripe renewal failures.
//!
//! This module accepts only the sealed result of the signed renewal decoder. It records the
//! generic receipt, accepts the #471 invalidation fence, and stores the Stripe-specific
//! predecessor relationship in one transaction. The caller must commit only after this function
//! returns successfully; every error requires rollback.

use sqlx::{PgPool, Postgres, Row, Transaction};
use thiserror::Error;

use crate::cloud_provider::{
    accept_provider_invalidation, record_verified_event, InvalidationDisposition, PayerKind,
    ProviderAdapterError, ProviderContext, VerifiedAllocation,
};
use crate::cloud_provider_stripe::STRIPE_NAMESPACE;
use crate::cloud_provider_stripe_renewals::{
    StoredStripeRenewalFailure, StripeRenewalFailureEvidence,
};

/// Whether this event advanced the beneficiary fence or replayed an existing acceptance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StripeRenewalFailureDisposition {
    Accepted,
    AlreadyAccepted,
}

/// The durable identity returned after a renewal failure is accepted or replayed.
///
/// `accepted_generation` is a local invalidation fence generation, not a paid term, entitlement
/// revision, or provider ordering claim. A successful acceptance still requires the caller to
/// commit its transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeRenewalFailureAcceptance {
    pub event_id: String,
    pub evidence_reference: String,
    pub accepted_generation: i64,
    pub disposition: StripeRenewalFailureDisposition,
}

#[derive(Debug, Error)]
pub enum StripeRenewalFailureStoreError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Provider(#[from] ProviderAdapterError),
    #[error("stored Stripe renewal evidence conflicts with the accepted event")]
    EvidenceConflict,
    #[error("accepted Stripe invalidation has no renewal evidence row")]
    EvidenceMissing,
}

/// Bounds for one allocation-scoped historical renewal read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StripeRenewalFailureLoadLimits {
    pub max_rows: usize,
    pub max_evidence_bytes: usize,
}

impl Default for StripeRenewalFailureLoadLimits {
    fn default() -> Self {
        Self {
            max_rows: 64,
            max_evidence_bytes: 256 * 1024,
        }
    }
}

impl StripeRenewalFailureLoadLimits {
    fn validate(self) -> Result<i64, StripeRenewalFailureLoadError> {
        if self.max_rows == 0 || self.max_evidence_bytes == 0 {
            return Err(StripeRenewalFailureLoadError::InvalidLimits);
        }
        let limit_plus_one = self
            .max_rows
            .checked_add(1)
            .and_then(|limit| i64::try_from(limit).ok())
            .ok_or(StripeRenewalFailureLoadError::InvalidLimits)?;
        Ok(limit_plus_one)
    }
}

#[derive(Debug, Error)]
pub enum StripeRenewalFailureLoadError {
    #[error("renewal failure load limits must be nonzero and fit in a database integer")]
    InvalidLimits,
    #[error("renewal failure history exceeded the row bound of {limit}")]
    TooManyRows { limit: usize },
    #[error("renewal failure history exceeded the evidence byte bound of {limit}")]
    BoundExceeded { limit: usize },
    /// The stored row or its joined owner/association failed validation. No partial list is
    /// returned, and this path never repairs the database.
    #[error("renewal failure storage is corrupt: {0}")]
    Corrupt(String),
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
}

/// Load all accepted renewal failures for one registered allocation in a bounded snapshot.
///
/// The joined statement sees either a committed association/evidence pair or neither. Rows are
/// ordered by the scoped event id using bytewise collation, and limit-plus-one detection prevents
/// returning a prefix when the local history exceeds either configured bound.
pub async fn load_personal_renewal_failures(
    pool: &PgPool,
    context: &ProviderContext,
    allocation: &VerifiedAllocation,
    limits: StripeRenewalFailureLoadLimits,
) -> Result<Vec<StripeRenewalFailureEvidence>, StripeRenewalFailureLoadError> {
    let row_limit = limits.validate()?;
    if context.namespace != STRIPE_NAMESPACE || allocation.payer_kind != PayerKind::Personal {
        return Err(StripeRenewalFailureLoadError::Corrupt(
            "renewal failures require a personal Stripe allocation".into(),
        ));
    }
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *tx)
        .await?;
    let preflight = sqlx::query(
        "WITH candidates AS ( \
         SELECT failure.evidence_reference, failure.renewal_id, failure.event_id, \
                failure.invoice_id, failure.invoice_line_id, failure.predecessor_invoice_id, \
                failure.predecessor_evidence_reference, failure.provider_account_id, \
                failure.provider_environment, failure.allocation_reference, \
                failure.provider_customer_id, failure.subscription_id, failure.provider_item_id, \
                failure.predecessor_period_start, failure.predecessor_period_end, \
                failure.renewal_period_start, failure.renewal_period_end, failure.event_created_at, \
                failure.interval \
         FROM cloud_provider_stripe_renewal_failures AS failure \
         JOIN cloud_provider_invalidation_associations AS association \
           ON association.provider_namespace = failure.provider_namespace \
          AND association.provider_account_id = failure.provider_account_id \
          AND association.provider_environment = failure.provider_environment \
          AND association.event_id = failure.event_id \
         JOIN cloud_provider_allocations AS allocation \
           ON allocation.allocation_id = association.allocation_id \
         JOIN cloud_provider_payers AS payer ON payer.payer_id = allocation.payer_id \
         JOIN cloud_coverage_sources AS source ON source.source_id = association.coverage_source_id \
         WHERE association.provider_namespace = $1 \
           AND association.provider_account_id = $2 \
           AND association.provider_environment = $3 \
           AND association.beneficiary_id = $4 \
           AND association.allocation_id = $5 \
           AND association.coverage_source_id = $6 \
         ORDER BY failure.event_id COLLATE \"C\" ASC \
         LIMIT $7 \
       ) \
       SELECT count(*)::BIGINT AS row_count, \
              COALESCE(SUM( \
                  octet_length(evidence_reference)::BIGINT + octet_length(renewal_id) \
                + octet_length(event_id) + octet_length(invoice_id) \
                + octet_length(invoice_line_id) + octet_length(predecessor_invoice_id) \
                + octet_length(predecessor_evidence_reference) \
                + octet_length(provider_account_id) + octet_length(provider_environment) \
                + octet_length(allocation_reference) + octet_length(provider_customer_id) \
                + octet_length(subscription_id) + octet_length(provider_item_id) \
                + octet_length(interval) + 5 * 8 \
              ), 0)::TEXT AS evidence_bytes \
       FROM candidates",
    )
    .bind(&context.namespace)
    .bind(&context.account_id)
    .bind(context.environment.as_str())
    .bind(&allocation.beneficiary_id)
    .bind(&allocation.allocation_id)
    .bind(&allocation.source_id)
    .bind(row_limit)
    .fetch_one(&mut *tx)
    .await?;
    let candidate_rows = preflight.try_get::<i64, _>("row_count")?;
    if candidate_rows > limits.max_rows as i64 {
        return Err(StripeRenewalFailureLoadError::TooManyRows {
            limit: limits.max_rows,
        });
    }
    let candidate_bytes = preflight
        .try_get::<String, _>("evidence_bytes")?
        .parse::<u128>()
        .map_err(|_| {
            StripeRenewalFailureLoadError::Corrupt(
                "stored renewal evidence byte count is invalid".into(),
            )
        })?;
    if candidate_bytes > limits.max_evidence_bytes as u128 {
        return Err(StripeRenewalFailureLoadError::BoundExceeded {
            limit: limits.max_evidence_bytes,
        });
    }
    let rows = sqlx::query(
        "SELECT failure.provider_namespace, failure.provider_account_id, \
                failure.provider_environment, failure.event_id, failure.evidence_version, \
                failure.evidence_reference, failure.renewal_id, failure.invoice_id, \
                failure.invoice_line_id, failure.predecessor_invoice_id, \
                failure.predecessor_evidence_reference, failure.provider_customer_id, \
                failure.subscription_id, failure.provider_item_id, failure.allocation_reference, \
                failure.beneficiary_id, failure.allocation_id, failure.coverage_source_id, \
                failure.predecessor_period_start, failure.predecessor_period_end, \
                failure.renewal_period_start, failure.renewal_period_end, failure.event_created_at, \
                failure.interval, failure.accepted_generation, \
                association.event_type AS association_event_type, \
                association.provider_created_at AS association_created_at, \
                association.normalized_payload_hash AS association_hash, \
                association.beneficiary_id AS association_beneficiary_id, \
                association.allocation_id AS association_allocation_id, \
                association.coverage_source_id AS association_source_id, \
                association.accepted_generation AS association_generation, \
                allocation.payer_id AS allocation_payer_id, \
                allocation.beneficiary_id AS allocation_beneficiary_id, \
                allocation.provider_namespace AS allocation_namespace, \
                allocation.provider_account_id AS allocation_account_id, \
                allocation.provider_environment AS allocation_environment, \
                allocation.provider_subscription_id AS allocation_subscription_id, \
                allocation.provider_item_id AS allocation_item_id, \
                allocation.external_allocation_reference AS allocation_reference_durable, \
                allocation.coverage_source_id AS allocation_source_durable, \
                allocation.effective_from AS allocation_effective_from, \
                allocation.effective_until AS allocation_effective_until, \
                allocation.state AS allocation_state, \
                allocation.ownership_evidence_reference AS allocation_ownership_reference, \
                payer.provider_namespace AS payer_namespace, \
                payer.provider_account_id AS payer_account_id, \
                payer.provider_environment AS payer_environment, \
                payer.provider_customer_id AS payer_customer_id, \
                payer.payer_kind AS payer_kind, \
                source.beneficiary_id AS source_beneficiary_id, \
                source.provider_namespace AS source_namespace, \
                source.external_allocation_reference AS source_allocation_reference, \
                source.ownership_evidence_reference AS source_ownership_reference \
         FROM cloud_provider_stripe_renewal_failures AS failure \
         JOIN cloud_provider_invalidation_associations AS association \
           ON association.provider_namespace = failure.provider_namespace \
          AND association.provider_account_id = failure.provider_account_id \
          AND association.provider_environment = failure.provider_environment \
          AND association.event_id = failure.event_id \
         JOIN cloud_provider_allocations AS allocation \
           ON allocation.allocation_id = association.allocation_id \
         JOIN cloud_provider_payers AS payer ON payer.payer_id = allocation.payer_id \
         JOIN cloud_coverage_sources AS source ON source.source_id = association.coverage_source_id \
         WHERE association.provider_namespace = $1 \
           AND association.provider_account_id = $2 \
           AND association.provider_environment = $3 \
           AND association.beneficiary_id = $4 \
           AND association.allocation_id = $5 \
           AND association.coverage_source_id = $6 \
         ORDER BY failure.event_id COLLATE \"C\" ASC \
         LIMIT $7",
    )
    .bind(&context.namespace)
    .bind(&context.account_id)
    .bind(context.environment.as_str())
    .bind(&allocation.beneficiary_id)
    .bind(&allocation.allocation_id)
    .bind(&allocation.source_id)
    .bind(row_limit)
    .fetch_all(&mut *tx)
    .await?;
    if rows.len() > limits.max_rows {
        return Err(StripeRenewalFailureLoadError::TooManyRows {
            limit: limits.max_rows,
        });
    }

    let mut total_bytes = 0usize;
    let mut evidence = Vec::with_capacity(rows.len());
    for row in &rows {
        let item = stored_evidence(row, context, allocation)?;
        let bytes = evidence_size(&item);
        total_bytes =
            total_bytes
                .checked_add(bytes)
                .ok_or(StripeRenewalFailureLoadError::BoundExceeded {
                    limit: limits.max_evidence_bytes,
                })?;
        if total_bytes > limits.max_evidence_bytes {
            return Err(StripeRenewalFailureLoadError::BoundExceeded {
                limit: limits.max_evidence_bytes,
            });
        }
        evidence.push(item);
    }
    tx.commit().await?;
    Ok(evidence)
}

fn stored_evidence(
    row: &sqlx::postgres::PgRow,
    context: &ProviderContext,
    allocation: &VerifiedAllocation,
) -> Result<StripeRenewalFailureEvidence, StripeRenewalFailureLoadError> {
    let stored = StoredStripeRenewalFailure {
        version: row.try_get("evidence_version")?,
        evidence_reference: row.try_get("evidence_reference")?,
        renewal_id: row.try_get("renewal_id")?,
        event_id: row.try_get("event_id")?,
        invoice_id: row.try_get("invoice_id")?,
        invoice_line_id: row.try_get("invoice_line_id")?,
        predecessor_invoice_id: row.try_get("predecessor_invoice_id")?,
        predecessor_evidence_reference: row.try_get("predecessor_evidence_reference")?,
        provider_account_id: row.try_get("provider_account_id")?,
        environment: context.environment,
        allocation_reference: row.try_get("allocation_reference")?,
        customer_id: row.try_get("provider_customer_id")?,
        subscription_id: row.try_get("subscription_id")?,
        provider_item_id: row.try_get("provider_item_id")?,
        predecessor_period_start: row.try_get("predecessor_period_start")?,
        predecessor_period_end: row.try_get("predecessor_period_end")?,
        renewal_period_start: row.try_get("renewal_period_start")?,
        renewal_period_end: row.try_get("renewal_period_end")?,
        event_created_at: row.try_get("event_created_at")?,
        interval: row.try_get("interval")?,
    };
    let evidence = StripeRenewalFailureEvidence::from_stored(stored)
        .map_err(|error| StripeRenewalFailureLoadError::Corrupt(error.to_string()))?;
    let event = evidence
        .verified_event()
        .map_err(|error| StripeRenewalFailureLoadError::Corrupt(error.to_string()))?;
    let association_matches = row.try_get::<String, _>("association_event_type")?
        == event.event_type
        && row.try_get::<i64, _>("association_created_at")? == event.provider_created_at
        && row.try_get::<String, _>("association_hash")? == event.normalized_payload_hash
        && row.try_get::<String, _>("association_beneficiary_id")? == allocation.beneficiary_id
        && row.try_get::<String, _>("association_allocation_id")? == allocation.allocation_id
        && row.try_get::<String, _>("association_source_id")? == allocation.source_id
        && row.try_get::<i64, _>("association_generation")?
            == row.try_get::<i64, _>("accepted_generation")?;
    let allocation_matches = row.try_get::<String, _>("allocation_payer_id")?
        == allocation.payer_id
        && row.try_get::<String, _>("allocation_beneficiary_id")? == allocation.beneficiary_id
        && row.try_get::<String, _>("allocation_namespace")? == context.namespace
        && row.try_get::<String, _>("allocation_account_id")? == context.account_id
        && row.try_get::<String, _>("allocation_environment")? == context.environment.as_str()
        && row.try_get::<String, _>("allocation_subscription_id")? == allocation.subscription_id
        && row.try_get::<String, _>("allocation_item_id")? == allocation.provider_item_id
        && row.try_get::<String, _>("allocation_reference_durable")?
            == allocation.external_allocation_reference
        && row.try_get::<String, _>("allocation_source_durable")? == allocation.source_id
        && row.try_get::<i64, _>("allocation_effective_from")? == allocation.effective_from
        && row.try_get::<Option<i64>, _>("allocation_effective_until")?
            == allocation.effective_until
        && row.try_get::<String, _>("allocation_state")? == allocation_state(allocation)
        && row.try_get::<String, _>("allocation_ownership_reference")?
            == allocation.ownership_evidence_reference;
    let payer_matches = row.try_get::<String, _>("payer_namespace")? == context.namespace
        && row.try_get::<String, _>("payer_account_id")? == context.account_id
        && row.try_get::<String, _>("payer_environment")? == context.environment.as_str()
        && row.try_get::<String, _>("payer_customer_id")? == evidence.customer_id()
        && row.try_get::<String, _>("payer_kind")? == "personal";
    let source_matches = row.try_get::<String, _>("source_beneficiary_id")?
        == allocation.beneficiary_id
        && row.try_get::<String, _>("source_namespace")? == context.namespace
        && row.try_get::<String, _>("source_allocation_reference")?
            == allocation.external_allocation_reference
        && row.try_get::<String, _>("source_ownership_reference")?
            == allocation.ownership_evidence_reference;
    let failure_matches = row.try_get::<String, _>("provider_namespace")? == context.namespace
        && row.try_get::<String, _>("provider_account_id")? == context.account_id
        && row.try_get::<String, _>("provider_environment")? == context.environment.as_str()
        && row.try_get::<String, _>("beneficiary_id")? == allocation.beneficiary_id
        && row.try_get::<String, _>("allocation_id")? == allocation.allocation_id
        && row.try_get::<String, _>("coverage_source_id")? == allocation.source_id;
    if evidence.provider_account_id() != context.account_id
        || evidence.environment() != context.environment
        || evidence.allocation_reference() != allocation.external_allocation_reference
        || evidence.customer_id() != allocation.provider_customer_id
        || evidence.subscription_id() != allocation.subscription_id
        || evidence.provider_item_id() != allocation.provider_item_id
        || !association_matches
        || !allocation_matches
        || !payer_matches
        || !source_matches
        || !failure_matches
    {
        return Err(StripeRenewalFailureLoadError::Corrupt(
            "stored Stripe renewal owner or association does not match".into(),
        ));
    }
    Ok(evidence)
}

fn allocation_state(allocation: &VerifiedAllocation) -> &'static str {
    match allocation.state {
        crate::cloud_provider::AllocationState::Pending => "pending",
        crate::cloud_provider::AllocationState::Active => "active",
        crate::cloud_provider::AllocationState::Ended => "ended",
    }
}

fn evidence_size(evidence: &StripeRenewalFailureEvidence) -> usize {
    evidence.renewal_id().len()
        + evidence.event_id().len()
        + evidence.invoice_id().len()
        + evidence.invoice_line_id().len()
        + evidence.predecessor_invoice_id().len()
        + evidence.predecessor_evidence_reference().len()
        + evidence.provider_account_id().len()
        + evidence.allocation_reference().len()
        + evidence.customer_id().len()
        + evidence.subscription_id().len()
        + evidence.provider_item_id().len()
        + evidence.evidence_reference().len()
        + evidence.environment().as_str().len()
        + evidence.interval().to_string().len()
        + (5 * std::mem::size_of::<i64>())
}

/// Atomically persist one decoder-linked renewal failure and its provider invalidation.
///
/// The transaction belongs to the caller. A successful return does not commit it; callers must
/// commit explicitly. If this function returns an error after an event or association was written,
/// the caller must roll the transaction back rather than attempting a partial recovery.
pub async fn accept_personal_renewal_failure(
    tx: &mut Transaction<'_, Postgres>,
    context: &ProviderContext,
    allocation: &VerifiedAllocation,
    evidence: &StripeRenewalFailureEvidence,
    ingest_enabled: bool,
) -> Result<StripeRenewalFailureAcceptance, StripeRenewalFailureStoreError> {
    validate_evidence_context(context, allocation, evidence)?;
    let event = evidence.verified_event()?;
    let _event_disposition = record_verified_event(tx, context, &event).await?;
    let invalidation =
        accept_provider_invalidation(tx, context, &event, allocation, ingest_enabled).await?;
    let accepted_generation = match invalidation {
        InvalidationDisposition::Accepted { generation }
        | InvalidationDisposition::AlreadyAccepted { generation } => generation,
    };
    let disposition = match invalidation {
        InvalidationDisposition::Accepted { .. } => StripeRenewalFailureDisposition::Accepted,
        InvalidationDisposition::AlreadyAccepted { .. } => {
            StripeRenewalFailureDisposition::AlreadyAccepted
        }
    };

    let inserted = sqlx::query(
        "INSERT INTO cloud_provider_stripe_renewal_failures \
         (provider_namespace, provider_account_id, provider_environment, event_id, \
          evidence_version, evidence_reference, renewal_id, invoice_id, invoice_line_id, \
          predecessor_invoice_id, predecessor_evidence_reference, provider_customer_id, \
          subscription_id, provider_item_id, allocation_reference, beneficiary_id, allocation_id, \
          coverage_source_id, predecessor_period_start, predecessor_period_end, \
          renewal_period_start, renewal_period_end, event_created_at, interval, accepted_generation) \
         VALUES ('stripe', $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, \
                 $16, $17, $18, $19, $20, $21, $22, $23, $24) \
         ON CONFLICT (provider_namespace, provider_account_id, provider_environment, event_id) \
         DO NOTHING",
    )
    .bind(evidence.provider_account_id())
    .bind(evidence.environment().as_str())
    .bind(evidence.event_id())
    .bind(crate::cloud_provider_stripe_renewals::RENEWAL_EVIDENCE_VERSION)
    .bind(evidence.evidence_reference())
    .bind(evidence.renewal_id())
    .bind(evidence.invoice_id())
    .bind(evidence.invoice_line_id())
    .bind(evidence.predecessor_invoice_id())
    .bind(evidence.predecessor_evidence_reference())
    .bind(evidence.customer_id())
    .bind(evidence.subscription_id())
    .bind(evidence.provider_item_id())
    .bind(evidence.allocation_reference())
    .bind(&allocation.beneficiary_id)
    .bind(&allocation.allocation_id)
    .bind(&allocation.source_id)
    .bind(evidence.predecessor_period_start())
    .bind(evidence.predecessor_period_end())
    .bind(evidence.renewal_period_start())
    .bind(evidence.renewal_period_end())
    .bind(evidence.event_created_at())
    .bind(evidence.interval().to_string())
    .bind(accepted_generation)
    .execute(&mut **tx)
    .await;

    match inserted {
        Ok(result) if result.rows_affected() == 1 => Ok(StripeRenewalFailureAcceptance {
            event_id: evidence.event_id().to_owned(),
            evidence_reference: evidence.evidence_reference().to_owned(),
            accepted_generation,
            disposition,
        }),
        Ok(_) => {
            compare_existing(
                tx,
                context,
                allocation,
                evidence,
                accepted_generation,
                disposition,
            )
            .await
        }
        Err(sqlx::Error::Database(database)) if database.code().as_deref() == Some("23505") => {
            Err(StripeRenewalFailureStoreError::EvidenceConflict)
        }
        Err(error) => Err(ProviderAdapterError::Database(error).into()),
    }
}

fn validate_evidence_context(
    context: &ProviderContext,
    allocation: &VerifiedAllocation,
    evidence: &StripeRenewalFailureEvidence,
) -> Result<(), ProviderAdapterError> {
    if context.namespace != STRIPE_NAMESPACE
        || allocation.payer_kind != PayerKind::Personal
        || evidence.provider_account_id() != context.account_id
        || evidence.environment() != context.environment
        || allocation.provider_customer_id != evidence.customer_id()
        || allocation.subscription_id != evidence.subscription_id()
        || allocation.provider_item_id != evidence.provider_item_id()
        || allocation.external_allocation_reference != evidence.allocation_reference()
    {
        return Err(ProviderAdapterError::ProviderContextMismatch);
    }
    Ok(())
}

async fn compare_existing(
    tx: &mut Transaction<'_, Postgres>,
    context: &ProviderContext,
    allocation: &VerifiedAllocation,
    evidence: &StripeRenewalFailureEvidence,
    accepted_generation: i64,
    disposition: StripeRenewalFailureDisposition,
) -> Result<StripeRenewalFailureAcceptance, StripeRenewalFailureStoreError> {
    let row = sqlx::query(
        "SELECT evidence_version, evidence_reference, renewal_id, invoice_id, invoice_line_id, \
                predecessor_invoice_id, predecessor_evidence_reference, provider_customer_id, \
                subscription_id, provider_item_id, allocation_reference, beneficiary_id, \
                allocation_id, coverage_source_id, predecessor_period_start, predecessor_period_end, \
                renewal_period_start, renewal_period_end, event_created_at, interval, \
                accepted_generation \
         FROM cloud_provider_stripe_renewal_failures \
         WHERE provider_namespace = 'stripe' AND provider_account_id = $1 \
           AND provider_environment = $2 AND event_id = $3",
    )
    .bind(&context.account_id)
    .bind(context.environment.as_str())
    .bind(evidence.event_id())
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(StripeRenewalFailureStoreError::EvidenceMissing)?;

    let same = row.try_get::<i16, _>("evidence_version")?
        == crate::cloud_provider_stripe_renewals::RENEWAL_EVIDENCE_VERSION
        && row.try_get::<String, _>("evidence_reference")? == evidence.evidence_reference()
        && row.try_get::<String, _>("renewal_id")? == evidence.renewal_id()
        && row.try_get::<String, _>("invoice_id")? == evidence.invoice_id()
        && row.try_get::<String, _>("invoice_line_id")? == evidence.invoice_line_id()
        && row.try_get::<String, _>("predecessor_invoice_id")? == evidence.predecessor_invoice_id()
        && row.try_get::<String, _>("predecessor_evidence_reference")?
            == evidence.predecessor_evidence_reference()
        && row.try_get::<String, _>("provider_customer_id")? == evidence.customer_id()
        && row.try_get::<String, _>("subscription_id")? == evidence.subscription_id()
        && row.try_get::<String, _>("provider_item_id")? == evidence.provider_item_id()
        && row.try_get::<String, _>("allocation_reference")? == evidence.allocation_reference()
        && row.try_get::<String, _>("beneficiary_id")? == allocation.beneficiary_id
        && row.try_get::<String, _>("allocation_id")? == allocation.allocation_id
        && row.try_get::<String, _>("coverage_source_id")? == allocation.source_id
        && row.try_get::<i64, _>("predecessor_period_start")?
            == evidence.predecessor_period_start()
        && row.try_get::<i64, _>("predecessor_period_end")? == evidence.predecessor_period_end()
        && row.try_get::<i64, _>("renewal_period_start")? == evidence.renewal_period_start()
        && row.try_get::<i64, _>("renewal_period_end")? == evidence.renewal_period_end()
        && row.try_get::<i64, _>("event_created_at")? == evidence.event_created_at()
        && row.try_get::<String, _>("interval")? == evidence.interval().to_string()
        && row.try_get::<i64, _>("accepted_generation")? == accepted_generation;
    if !same {
        return Err(StripeRenewalFailureStoreError::EvidenceConflict);
    }
    Ok(StripeRenewalFailureAcceptance {
        event_id: evidence.event_id().to_owned(),
        evidence_reference: evidence.evidence_reference().to_owned(),
        accepted_generation,
        disposition,
    })
}
