//! Coordination for trusted person-level Cloud coverage sources.
//!
//! This module records which external allocations belong to a beneficiary and serialises later
//! complete collections. It does not call a provider or decide whether an allocation is paid.
//! Callers provide evidence that has already passed the provider-specific checks.

use std::{collections::BTreeMap, fmt};

use serde::{Deserialize, Serialize};
use sqlx::{Postgres, Row, Transaction};
use thiserror::Error;

use crate::cloud_coverage::{
    normalise_confirmed_intervals, ConfirmedPaidInterval, InvalidCoverage, PersonCoverage,
};
use crate::cloud_coverage_store::{
    current_revision, publish, CoverageProjection, PublicationOutcome, StoreError,
    UnavailableReason,
};

/// The immutable identity that binds one external allocation to a beneficiary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceBinding {
    pub beneficiary_id: String,
    pub source_id: String,
    pub provider_namespace: String,
    pub external_allocation_reference: String,
    pub ownership_evidence_reference: String,
}

/// Whether a source registration was newly applied or exactly replayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistrationOutcome {
    Applied,
    AlreadyApplied,
}

/// The durable result of registering one source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistrationReceipt {
    pub source_id: String,
    pub source_set_generation: i64,
    pub projection_revision: Option<i64>,
    pub outcome: RegistrationOutcome,
}

/// The lifecycle state of a durable collection attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CollectionStatus {
    Pending,
    Superseded,
    Completed,
}

/// A source set snapshot and projection revision captured before provider collection begins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectionTicket {
    pub beneficiary_id: String,
    pub attempt_id: String,
    pub collection_epoch: i64,
    pub source_set_generation: i64,
    pub expected_projection_revision: Option<i64>,
    pub source_bindings: Vec<SourceBinding>,
    pub status: CollectionStatus,
    pub completed_revision: Option<i64>,
}

/// A complete or explicitly unavailable observation for one registered source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceObservation {
    Complete {
        source_id: String,
        evidence_reference: String,
        paid_intervals: Vec<ConfirmedPaidInterval>,
    },
    Unavailable {
        source_id: String,
        evidence_reference: String,
        reason: UnavailableReason,
    },
}

/// The result of completing a collection attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconciliationReceipt {
    pub attempt_id: String,
    pub revision: i64,
    pub outcome: PublicationOutcome,
}

/// Source coordination and validation failures.
#[derive(Debug, Error)]
pub enum ReconciliationError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("coverage store error: {0}")]
    Store(#[from] StoreError),
    #[error("invalid source binding: {0}")]
    InvalidSource(String),
    #[error("invalid identifier: {0}")]
    InvalidIdentifier(String),
    #[error("registration operation_id must not be empty")]
    EmptyOperationId,
    #[error("source registration conflicts with an existing binding")]
    RegistrationConflict,
    #[error("existing coverage is not owned by the reconciliation coordinator")]
    BootstrapConflict,
    #[error("stored source registration receipt is incomplete")]
    CorruptRegistration,
    #[error("source allocation is already bound to another beneficiary")]
    SourceBindingConflict,
    #[error("collection attempt_id must not be empty")]
    EmptyAttemptId,
    #[error("no registered coverage sources exist for this beneficiary")]
    NoSources,
    #[error("collection attempt does not exist")]
    AttemptMissing,
    #[error("collection attempt is superseded")]
    AttemptSuperseded,
    #[error("collection attempt conflicts with current source or projection state")]
    CollectionConflict,
    #[error("collection operation conflicts with a completed replay")]
    OperationConflict,
    #[error("stored collection attempt is corrupt: {0}")]
    CorruptAttempt(CorruptAttemptReason),
    #[error("collection source batch does not match the registered source set")]
    SourceBatchMismatch,
    #[error("source observations conflict: {0}")]
    SourceObservationConflict(String),
    #[error("invalid source observation: {0}")]
    InvalidObservation(#[from] InvalidCoverage),
    #[error("collection epoch is exhausted")]
    EpochOverflow,
    #[error("source set generation is exhausted")]
    GenerationOverflow,
    #[error("serialised reconciliation value is invalid: {0}")]
    Serialization(#[from] serde_json::Error),
}

/// The durable collection field that failed validation.
///
/// These categories deliberately do not include stored values. They are safe to surface to an
/// operator without exposing provider identifiers or evidence references.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorruptAttemptReason {
    BindingShape,
    BindingOwnership,
    BindingSourceSet,
    CompletionReceipt,
    ResultShape,
    ResultEvidence,
    ResultCanonical,
}

impl fmt::Display for CorruptAttemptReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::BindingShape => "source bindings",
            Self::BindingOwnership => "source binding ownership",
            Self::BindingSourceSet => "registered source snapshot",
            Self::CompletionReceipt => "completion receipt",
            Self::ResultShape => "collection result shape",
            Self::ResultEvidence => "collection result evidence",
            Self::ResultCanonical => "collection result canonical form",
        })
    }
}

impl fmt::Display for RegistrationOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Applied => "applied",
            Self::AlreadyApplied => "already_applied",
        })
    }
}

/// Register a source and invalidate completeness in one caller-owned transaction.
///
/// The caller must commit on success and roll back on error. The first registration publishes an
/// unavailable projection, and adding any later source advances the source-set generation and
/// publishes a new unavailable revision. No provider call is made while the coordinator is locked.
pub async fn register_source(
    tx: &mut Transaction<'_, Postgres>,
    operation_id: &str,
    binding: &SourceBinding,
) -> Result<RegistrationReceipt, ReconciliationError> {
    validate_operation_id(operation_id)?;
    validate_binding(binding)?;

    let coordinator_exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM cloud_coverage_coordinators WHERE beneficiary_id = $1)",
    )
    .bind(&binding.beneficiary_id)
    .fetch_one(&mut **tx)
    .await?;
    let existing_projection_revision = current_revision(tx, &binding.beneficiary_id).await?;
    if !coordinator_exists && existing_projection_revision.is_some() {
        return Err(ReconciliationError::BootstrapConflict);
    }

    sqlx::query(
        "INSERT INTO cloud_coverage_coordinators (beneficiary_id) VALUES ($1) \
         ON CONFLICT (beneficiary_id) DO NOTHING",
    )
    .bind(&binding.beneficiary_id)
    .execute(&mut **tx)
    .await
    .map_err(ReconciliationError::Database)?;

    let coordinator = sqlx::query(
        "SELECT source_set_generation FROM cloud_coverage_coordinators \
         WHERE beneficiary_id = $1 FOR UPDATE",
    )
    .bind(&binding.beneficiary_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(ReconciliationError::Database)?;
    let generation: i64 = coordinator
        .try_get("source_set_generation")
        .map_err(ReconciliationError::Database)?;
    // Re-read after taking the coordinator lock. A direct publisher can commit between the
    // initial bootstrap check and the insert/lock above; the fresh value must not be adopted.
    let expected_revision = current_revision(tx, &binding.beneficiary_id).await?;
    if generation == 0 && expected_revision.is_some() {
        return Err(ReconciliationError::BootstrapConflict);
    }

    if let Some(row) = sqlx::query(
        "SELECT beneficiary_id, source_id, provider_namespace, external_allocation_reference, \
                ownership_evidence_reference, registration_source_set_generation, \
                registration_projection_revision \
         FROM cloud_coverage_sources \
         WHERE beneficiary_id = $1 AND registration_operation_id = $2",
    )
    .bind(&binding.beneficiary_id)
    .bind(operation_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(ReconciliationError::Database)?
    {
        let stored = source_binding_from_row(&row)?;
        if &stored == binding {
            let projection_revision: Option<i64> =
                row.try_get("registration_projection_revision")?;
            let Some(projection_revision) = projection_revision else {
                return Err(ReconciliationError::CorruptRegistration);
            };
            return Ok(RegistrationReceipt {
                source_id: stored.source_id,
                source_set_generation: row.try_get("registration_source_set_generation")?,
                projection_revision: Some(projection_revision),
                outcome: RegistrationOutcome::AlreadyApplied,
            });
        }
        return Err(ReconciliationError::RegistrationConflict);
    }

    let source_id_exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM cloud_coverage_sources WHERE source_id = $1)",
    )
    .bind(&binding.source_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(ReconciliationError::Database)?;
    let external_exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM cloud_coverage_sources \
                        WHERE provider_namespace = $1 AND external_allocation_reference = $2)",
    )
    .bind(&binding.provider_namespace)
    .bind(&binding.external_allocation_reference)
    .fetch_one(&mut **tx)
    .await
    .map_err(ReconciliationError::Database)?;
    if source_id_exists || external_exists {
        return Err(ReconciliationError::SourceBindingConflict);
    }

    let next_generation = generation
        .checked_add(1)
        .ok_or(ReconciliationError::GenerationOverflow)?;

    sqlx::query(
        "INSERT INTO cloud_coverage_sources \
         (source_id, beneficiary_id, provider_namespace, external_allocation_reference, \
          ownership_evidence_reference, registration_operation_id, registration_source_set_generation) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(&binding.source_id)
    .bind(&binding.beneficiary_id)
    .bind(&binding.provider_namespace)
    .bind(&binding.external_allocation_reference)
    .bind(&binding.ownership_evidence_reference)
    .bind(operation_id)
    .bind(next_generation)
    .execute(&mut **tx)
    .await
    .map_err(map_source_insert_error)?;

    sqlx::query(
        "UPDATE cloud_coverage_collection_attempts SET status = 'superseded' \
         WHERE beneficiary_id = $1 AND status = 'pending'",
    )
    .bind(&binding.beneficiary_id)
    .execute(&mut **tx)
    .await
    .map_err(ReconciliationError::Database)?;
    sqlx::query(
        "UPDATE cloud_coverage_coordinators \
         SET source_set_generation = $2, current_attempt_id = NULL \
         WHERE beneficiary_id = $1",
    )
    .bind(&binding.beneficiary_id)
    .bind(next_generation)
    .execute(&mut **tx)
    .await
    .map_err(ReconciliationError::Database)?;

    let publication = publish(
        tx,
        &binding.beneficiary_id,
        expected_revision,
        &format!("source-registration:{operation_id}"),
        &binding.ownership_evidence_reference,
        &unavailable_projection(),
    )
    .await?;

    let updated = sqlx::query(
        "UPDATE cloud_coverage_sources SET registration_projection_revision = $2 \
         WHERE source_id = $1",
    )
    .bind(&binding.source_id)
    .bind(publication.revision)
    .execute(&mut **tx)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(ReconciliationError::CorruptRegistration);
    }

    Ok(RegistrationReceipt {
        source_id: binding.source_id.clone(),
        source_set_generation: next_generation,
        projection_revision: Some(publication.revision),
        outcome: projection_outcome(publication.outcome),
    })
}

/// Begin a collection against the current complete source set.
///
/// The returned ticket is durable only after the caller commits. The caller must roll back on
/// error. A new attempt supersedes any pending attempt for the same beneficiary. No provider call
/// belongs inside this transaction.
pub async fn begin_collection(
    tx: &mut Transaction<'_, Postgres>,
    beneficiary_id: &str,
    attempt_id: &str,
) -> Result<CollectionTicket, ReconciliationError> {
    validate_identifier(beneficiary_id, "beneficiary_id")?;
    validate_attempt_id(attempt_id)?;

    let coordinator = sqlx::query(
        "SELECT source_set_generation, collection_epoch, current_attempt_id \
         FROM cloud_coverage_coordinators WHERE beneficiary_id = $1 FOR UPDATE",
    )
    .bind(beneficiary_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(coordinator) = coordinator else {
        return Err(ReconciliationError::NoSources);
    };
    let generation: i64 = coordinator.try_get("source_set_generation")?;
    let epoch: i64 = coordinator.try_get("collection_epoch")?;
    let current_attempt_id: Option<String> = coordinator.try_get("current_attempt_id")?;

    if let Some(existing) = sqlx::query(
        "SELECT attempt_id, beneficiary_id, collection_epoch, source_set_generation, \
                expected_projection_revision, source_bindings::text AS source_bindings, status, \
                aggregate_evidence_reference, canonical_result::text AS canonical_result, \
                projection_revision \
         FROM cloud_coverage_collection_attempts \
         WHERE beneficiary_id = $1 AND attempt_id = $2",
    )
    .bind(beneficiary_id)
    .bind(attempt_id)
    .fetch_optional(&mut **tx)
    .await?
    {
        let ticket = collection_ticket_from_row(&existing)?;
        validate_stored_attempt(tx, &ticket, &existing).await?;
        return Ok(ticket);
    }

    let source_rows = sqlx::query(
        "SELECT beneficiary_id, source_id, provider_namespace, external_allocation_reference, \
                ownership_evidence_reference \
         FROM cloud_coverage_sources WHERE beneficiary_id = $1 ORDER BY source_id COLLATE \"C\"",
    )
    .bind(beneficiary_id)
    .fetch_all(&mut **tx)
    .await?;
    if source_rows.is_empty() || generation == 0 {
        return Err(ReconciliationError::NoSources);
    }
    let source_bindings = source_rows
        .iter()
        .map(source_binding_from_row)
        .collect::<Result<Vec<_>, _>>()?;
    let source_bindings_json = serde_json::to_string(&source_bindings)?;
    let expected_projection_revision = current_revision(tx, beneficiary_id).await?;
    let next_epoch = epoch
        .checked_add(1)
        .ok_or(ReconciliationError::EpochOverflow)?;

    if let Some(previous_attempt_id) = current_attempt_id {
        sqlx::query(
            "UPDATE cloud_coverage_collection_attempts SET status = 'superseded' \
             WHERE beneficiary_id = $1 AND attempt_id = $2 AND status = 'pending'",
        )
        .bind(beneficiary_id)
        .bind(previous_attempt_id)
        .execute(&mut **tx)
        .await?;
    }

    sqlx::query(
        "INSERT INTO cloud_coverage_collection_attempts \
         (attempt_id, beneficiary_id, collection_epoch, source_set_generation, \
          expected_projection_revision, source_bindings, status) \
         VALUES ($1, $2, $3, $4, $5, $6::jsonb, 'pending')",
    )
    .bind(attempt_id)
    .bind(beneficiary_id)
    .bind(next_epoch)
    .bind(generation)
    .bind(expected_projection_revision)
    .bind(source_bindings_json)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "UPDATE cloud_coverage_coordinators SET collection_epoch = $2, current_attempt_id = $3 \
         WHERE beneficiary_id = $1",
    )
    .bind(beneficiary_id)
    .bind(next_epoch)
    .bind(attempt_id)
    .execute(&mut **tx)
    .await?;

    Ok(CollectionTicket {
        beneficiary_id: beneficiary_id.into(),
        attempt_id: attempt_id.into(),
        collection_epoch: next_epoch,
        source_set_generation: generation,
        expected_projection_revision,
        source_bindings,
        status: CollectionStatus::Pending,
        completed_revision: None,
    })
}

/// Finish a collection, combine every registered source and publish one atomic projection.
///
/// The caller must commit on success and roll back on error. Provider work must be complete before
/// this function is called. A pending attempt is never converted into an empty result by timeout.
pub async fn finish_collection(
    tx: &mut Transaction<'_, Postgres>,
    ticket: &CollectionTicket,
    aggregate_evidence_reference: &str,
    observations: &[SourceObservation],
) -> Result<ReconciliationReceipt, ReconciliationError> {
    validate_identifier(&ticket.beneficiary_id, "beneficiary_id")?;
    validate_attempt_id(&ticket.attempt_id)?;

    let coordinator = sqlx::query(
        "SELECT source_set_generation, collection_epoch, current_attempt_id \
         FROM cloud_coverage_coordinators WHERE beneficiary_id = $1 FOR UPDATE",
    )
    .bind(&ticket.beneficiary_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(ReconciliationError::NoSources)?;
    let current_generation: i64 = coordinator.try_get("source_set_generation")?;
    let current_epoch: i64 = coordinator.try_get("collection_epoch")?;
    let current_attempt: Option<String> = coordinator.try_get("current_attempt_id")?;

    let attempt = sqlx::query(
        "SELECT attempt_id, beneficiary_id, collection_epoch, source_set_generation, \
                expected_projection_revision, source_bindings::text AS source_bindings, status, \
                aggregate_evidence_reference, canonical_result::text AS canonical_result, \
                projection_revision \
         FROM cloud_coverage_collection_attempts \
         WHERE beneficiary_id = $1 AND attempt_id = $2",
    )
    .bind(&ticket.beneficiary_id)
    .bind(&ticket.attempt_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(ReconciliationError::AttemptMissing)?;
    let stored_ticket = collection_ticket_from_row(&attempt)?;
    let stored_completion = validate_stored_attempt(tx, &stored_ticket, &attempt).await?;
    if stored_ticket.beneficiary_id != ticket.beneficiary_id
        || stored_ticket.collection_epoch != ticket.collection_epoch
        || stored_ticket.source_set_generation != ticket.source_set_generation
        || stored_ticket.expected_projection_revision != ticket.expected_projection_revision
        || stored_ticket.source_bindings != ticket.source_bindings
    {
        return Err(ReconciliationError::CollectionConflict);
    }

    let status = stored_ticket.status;
    if status == CollectionStatus::Completed {
        let canonical_result = canonical_collection(
            &ticket.beneficiary_id,
            &ticket.source_bindings,
            observations,
            aggregate_evidence_reference,
        )
        .map(|(canonical_result, _)| canonical_result)
        .map_err(|_| ReconciliationError::OperationConflict)?;
        let Some(stored_completion) = stored_completion else {
            return Err(corrupt(CorruptAttemptReason::CompletionReceipt));
        };
        if stored_completion.evidence == aggregate_evidence_reference
            && stored_completion.canonical_result == canonical_result
        {
            return Ok(ReconciliationReceipt {
                attempt_id: ticket.attempt_id.clone(),
                revision: stored_completion.revision,
                outcome: PublicationOutcome::AlreadyApplied,
            });
        }
        return Err(ReconciliationError::OperationConflict);
    }
    if status == CollectionStatus::Superseded {
        return Err(ReconciliationError::AttemptSuperseded);
    }
    validate_identifier(aggregate_evidence_reference, "aggregate_evidence_reference")?;
    let (canonical_result, projection) = canonical_collection(
        &ticket.beneficiary_id,
        &ticket.source_bindings,
        observations,
        aggregate_evidence_reference,
    )?;
    if current_attempt.as_deref() != Some(ticket.attempt_id.as_str())
        || current_generation != ticket.source_set_generation
        || current_epoch != ticket.collection_epoch
    {
        return Err(ReconciliationError::CollectionConflict);
    }
    let actual_revision = current_revision(tx, &ticket.beneficiary_id).await?;
    if actual_revision != ticket.expected_projection_revision {
        return Err(ReconciliationError::Store(StoreError::RevisionConflict {
            expected: ticket.expected_projection_revision,
            actual: actual_revision,
        }));
    }

    let publication = publish(
        tx,
        &ticket.beneficiary_id,
        ticket.expected_projection_revision,
        &format!("collection:{}", ticket.attempt_id),
        aggregate_evidence_reference,
        &projection,
    )
    .await?;
    let canonical_result_json = serde_json::to_string(&canonical_result)?;
    let updated = sqlx::query(
        "UPDATE cloud_coverage_collection_attempts SET status = 'completed', \
         aggregate_evidence_reference = $3, canonical_result = $4::jsonb, \
         projection_revision = $5, completed_at = now() \
         WHERE beneficiary_id = $1 AND attempt_id = $2 AND status = 'pending'",
    )
    .bind(&ticket.beneficiary_id)
    .bind(&ticket.attempt_id)
    .bind(aggregate_evidence_reference)
    .bind(canonical_result_json)
    .bind(publication.revision)
    .execute(&mut **tx)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(ReconciliationError::CollectionConflict);
    }
    sqlx::query(
        "UPDATE cloud_coverage_coordinators SET current_attempt_id = NULL \
         WHERE beneficiary_id = $1 AND current_attempt_id = $2",
    )
    .bind(&ticket.beneficiary_id)
    .bind(&ticket.attempt_id)
    .execute(&mut **tx)
    .await?;
    Ok(ReconciliationReceipt {
        attempt_id: ticket.attempt_id.clone(),
        revision: publication.revision,
        outcome: publication.outcome,
    })
}

fn canonical_collection(
    beneficiary_id: &str,
    bindings: &[SourceBinding],
    observations: &[SourceObservation],
    aggregate_evidence_reference: &str,
) -> Result<(serde_json::Value, CoverageProjection), ReconciliationError> {
    if observations.len() != bindings.len() {
        return Err(ReconciliationError::SourceBatchMismatch);
    }
    let expected = bindings
        .iter()
        .map(|binding| (binding.source_id.as_str(), binding))
        .collect::<BTreeMap<_, _>>();
    let mut seen = BTreeMap::new();
    let mut canonical_observations = Vec::with_capacity(observations.len());
    let mut complete_facts = Vec::new();
    let mut fact_sources = BTreeMap::new();
    let mut renewal_sources = BTreeMap::new();
    let mut unavailable_reason = None;

    for observation in observations {
        let (source_id, evidence_reference) = match observation {
            SourceObservation::Complete {
                source_id,
                evidence_reference,
                ..
            }
            | SourceObservation::Unavailable {
                source_id,
                evidence_reference,
                ..
            } => (source_id, evidence_reference),
        };
        if evidence_reference.trim().is_empty()
            || !expected.contains_key(source_id.as_str())
            || seen.insert(source_id.clone(), ()).is_some()
        {
            return Err(ReconciliationError::SourceBatchMismatch);
        }
        match observation {
            SourceObservation::Complete { paid_intervals, .. } => {
                let normalised = normalise_confirmed_intervals(&PersonCoverage {
                    beneficiary_id: beneficiary_id.into(),
                    paid_intervals: paid_intervals.clone(),
                })?;
                for interval in &normalised {
                    if interval.source_id != *source_id {
                        return Err(ReconciliationError::SourceObservationConflict(
                            "coverage fact source_id does not match its registered source".into(),
                        ));
                    }
                    if fact_sources
                        .insert(interval.coverage_id.clone(), source_id.clone())
                        .is_some()
                    {
                        return Err(ReconciliationError::SourceObservationConflict(
                            "coverage_id appears in multiple sources".into(),
                        ));
                    }
                    if let Some(renewal_id) = interval.failed_renewal_id.as_deref() {
                        if renewal_sources
                            .insert(renewal_id.to_owned(), source_id.clone())
                            .is_some()
                        {
                            return Err(ReconciliationError::SourceObservationConflict(
                                "failed_renewal_id appears in multiple sources".into(),
                            ));
                        }
                    }
                }
                complete_facts.extend(normalised.iter().cloned());
                canonical_observations.push(serde_json::json!({
                    "source_id": source_id,
                    "evidence_reference": evidence_reference,
                    "status": "complete",
                    "paid_intervals": normalised,
                }));
            }
            SourceObservation::Unavailable { reason, .. } => {
                if *reason == UnavailableReason::ConflictingEvidence {
                    unavailable_reason = Some(UnavailableReason::ConflictingEvidence);
                } else if unavailable_reason.is_none() {
                    unavailable_reason = Some(UnavailableReason::NeedsReconciliation);
                }
                canonical_observations.push(serde_json::json!({
                    "source_id": source_id,
                    "evidence_reference": evidence_reference,
                    "status": "unavailable",
                    "reason": reason.to_string(),
                }));
            }
        }
    }
    if seen.len() != expected.len() {
        return Err(ReconciliationError::SourceBatchMismatch);
    }
    canonical_observations.sort_by(|left, right| {
        left["source_id"]
            .as_str()
            .unwrap_or_default()
            .cmp(right["source_id"].as_str().unwrap_or_default())
    });
    let projection = match unavailable_reason {
        Some(reason) => CoverageProjection::Unavailable { reason },
        None => {
            complete_facts.sort_by(|left, right| left.coverage_id.cmp(&right.coverage_id));
            CoverageProjection::Complete {
                paid_intervals: complete_facts,
            }
        }
    };
    Ok((
        serde_json::json!({
            "aggregate_evidence_reference": aggregate_evidence_reference,
            "sources": canonical_observations,
        }),
        projection,
    ))
}

fn validate_operation_id(operation_id: &str) -> Result<(), ReconciliationError> {
    if operation_id.trim().is_empty() {
        Err(ReconciliationError::EmptyOperationId)
    } else {
        Ok(())
    }
}

fn validate_attempt_id(attempt_id: &str) -> Result<(), ReconciliationError> {
    if attempt_id.trim().is_empty() {
        Err(ReconciliationError::EmptyAttemptId)
    } else {
        Ok(())
    }
}

fn validate_identifier(value: &str, name: &str) -> Result<(), ReconciliationError> {
    if value.trim().is_empty() {
        Err(ReconciliationError::InvalidIdentifier(format!(
            "{name} must not be empty"
        )))
    } else {
        Ok(())
    }
}

fn collection_ticket_from_row(
    row: &sqlx::postgres::PgRow,
) -> Result<CollectionTicket, ReconciliationError> {
    let status: String = row.try_get("status")?;
    let status = match status.as_str() {
        "pending" => CollectionStatus::Pending,
        "superseded" => CollectionStatus::Superseded,
        "completed" => CollectionStatus::Completed,
        _ => {
            return Err(ReconciliationError::CollectionConflict);
        }
    };
    let beneficiary_id: String = row.try_get("beneficiary_id")?;
    let source_bindings_json: String = row.try_get("source_bindings")?;
    let source_bindings = parse_stored_bindings(&source_bindings_json, &beneficiary_id)?;
    let projection_revision: Option<i64> = row.try_get("projection_revision")?;
    Ok(CollectionTicket {
        beneficiary_id,
        attempt_id: row.try_get("attempt_id")?,
        collection_epoch: row.try_get("collection_epoch")?,
        source_set_generation: row.try_get("source_set_generation")?,
        expected_projection_revision: row.try_get("expected_projection_revision")?,
        source_bindings,
        status,
        completed_revision: projection_revision,
    })
}

fn parse_stored_bindings(
    source_bindings_json: &str,
    beneficiary_id: &str,
) -> Result<Vec<SourceBinding>, ReconciliationError> {
    let value: serde_json::Value = serde_json::from_str(source_bindings_json)
        .map_err(|_| corrupt(CorruptAttemptReason::BindingShape))?;
    let bindings = value
        .as_array()
        .ok_or_else(|| corrupt(CorruptAttemptReason::BindingShape))?;
    if bindings.is_empty() {
        return Err(corrupt(CorruptAttemptReason::BindingShape));
    }
    let mut parsed = Vec::with_capacity(bindings.len());
    for value in bindings {
        let object = value
            .as_object()
            .ok_or_else(|| corrupt(CorruptAttemptReason::BindingShape))?;
        if object.len() != 5
            || ![
                "beneficiary_id",
                "source_id",
                "provider_namespace",
                "external_allocation_reference",
                "ownership_evidence_reference",
            ]
            .iter()
            .all(|field| object.contains_key(*field))
        {
            return Err(corrupt(CorruptAttemptReason::BindingShape));
        }
        let binding = SourceBinding {
            beneficiary_id: stored_string(
                object,
                "beneficiary_id",
                CorruptAttemptReason::BindingShape,
            )?,
            source_id: stored_string(object, "source_id", CorruptAttemptReason::BindingShape)?,
            provider_namespace: stored_string(
                object,
                "provider_namespace",
                CorruptAttemptReason::BindingShape,
            )?,
            external_allocation_reference: stored_string(
                object,
                "external_allocation_reference",
                CorruptAttemptReason::BindingShape,
            )?,
            ownership_evidence_reference: stored_string(
                object,
                "ownership_evidence_reference",
                CorruptAttemptReason::BindingShape,
            )?,
        };
        if binding.beneficiary_id != beneficiary_id {
            return Err(corrupt(CorruptAttemptReason::BindingOwnership));
        }
        validate_binding(&binding).map_err(|_| corrupt(CorruptAttemptReason::BindingShape))?;
        parsed.push(binding);
    }
    if parsed
        .windows(2)
        .any(|pair| pair[0].source_id >= pair[1].source_id)
    {
        return Err(corrupt(CorruptAttemptReason::BindingShape));
    }
    Ok(parsed)
}

async fn validate_stored_bindings(
    tx: &mut Transaction<'_, Postgres>,
    ticket: &CollectionTicket,
) -> Result<(), ReconciliationError> {
    let rows = sqlx::query(
        "SELECT beneficiary_id, source_id, provider_namespace, external_allocation_reference, \
                ownership_evidence_reference, registration_source_set_generation \
         FROM cloud_coverage_sources \
         WHERE beneficiary_id = $1 AND registration_source_set_generation <= $2 \
         ORDER BY source_id COLLATE \"C\"",
    )
    .bind(&ticket.beneficiary_id)
    .bind(ticket.source_set_generation)
    .fetch_all(&mut **tx)
    .await?;
    let authoritative = rows
        .iter()
        .map(source_binding_from_row)
        .collect::<Result<Vec<_>, _>>()?;
    let latest_generation = rows
        .iter()
        .map(|row| row.try_get("registration_source_set_generation"))
        .collect::<Result<Vec<i64>, sqlx::Error>>()?
        .into_iter()
        .max();
    if latest_generation != Some(ticket.source_set_generation)
        || authoritative.is_empty()
        || authoritative != ticket.source_bindings
    {
        return Err(corrupt(CorruptAttemptReason::BindingSourceSet));
    }
    Ok(())
}

async fn validate_stored_attempt(
    tx: &mut Transaction<'_, Postgres>,
    ticket: &CollectionTicket,
    row: &sqlx::postgres::PgRow,
) -> Result<Option<StoredCompletionReceipt>, ReconciliationError> {
    validate_stored_bindings(tx, ticket).await?;
    let receipt = stored_completion_receipt(
        ticket.status,
        row.try_get("aggregate_evidence_reference")?,
        row.try_get("canonical_result")?,
        row.try_get("projection_revision")?,
    )?;
    receipt
        .map(|(evidence, result, revision)| {
            let canonical_result = validate_stored_result(ticket, &evidence, &result)?;
            Ok(StoredCompletionReceipt {
                evidence,
                canonical_result,
                revision,
            })
        })
        .transpose()
}

struct StoredCompletionReceipt {
    evidence: String,
    canonical_result: serde_json::Value,
    revision: i64,
}

fn stored_completion_receipt(
    status: CollectionStatus,
    evidence: Option<String>,
    result: Option<String>,
    revision: Option<i64>,
) -> Result<Option<(String, String, i64)>, ReconciliationError> {
    match (status, evidence, result, revision) {
        (CollectionStatus::Completed, Some(evidence), Some(result), Some(revision)) => {
            Ok(Some((evidence, result, revision)))
        }
        (CollectionStatus::Pending | CollectionStatus::Superseded, None, None, None) => Ok(None),
        _ => Err(corrupt(CorruptAttemptReason::CompletionReceipt)),
    }
}

fn validate_stored_result(
    ticket: &CollectionTicket,
    evidence: &str,
    stored_result: &str,
) -> Result<serde_json::Value, ReconciliationError> {
    if evidence.trim().is_empty() {
        return Err(corrupt(CorruptAttemptReason::ResultEvidence));
    }
    let value: serde_json::Value = serde_json::from_str(stored_result)
        .map_err(|_| corrupt(CorruptAttemptReason::ResultShape))?;
    let object = value
        .as_object()
        .ok_or_else(|| corrupt(CorruptAttemptReason::ResultShape))?;
    if object.len() != 2
        || !object.contains_key("aggregate_evidence_reference")
        || !object.contains_key("sources")
    {
        return Err(corrupt(CorruptAttemptReason::ResultShape));
    }
    let stored_evidence = stored_string(
        object,
        "aggregate_evidence_reference",
        CorruptAttemptReason::ResultShape,
    )?;
    if stored_evidence != evidence || stored_evidence.trim().is_empty() {
        return Err(corrupt(CorruptAttemptReason::ResultEvidence));
    }
    let sources = object
        .get("sources")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| corrupt(CorruptAttemptReason::ResultShape))?;
    let observations = sources
        .iter()
        .map(parse_stored_observation)
        .collect::<Result<Vec<_>, _>>()?;
    let (canonical, _) = canonical_collection(
        &ticket.beneficiary_id,
        &ticket.source_bindings,
        &observations,
        evidence,
    )
    .map_err(|_| corrupt(CorruptAttemptReason::ResultCanonical))?;
    if value != canonical {
        return Err(corrupt(CorruptAttemptReason::ResultCanonical));
    }
    Ok(canonical)
}

fn parse_stored_observation(
    value: &serde_json::Value,
) -> Result<SourceObservation, ReconciliationError> {
    let object = value
        .as_object()
        .ok_or_else(|| corrupt(CorruptAttemptReason::ResultShape))?;
    let status = stored_string(object, "status", CorruptAttemptReason::ResultShape)?;
    let source_id = stored_string(object, "source_id", CorruptAttemptReason::ResultShape)?;
    let evidence_reference = stored_string(
        object,
        "evidence_reference",
        CorruptAttemptReason::ResultShape,
    )?;
    match status.as_str() {
        "complete" => {
            if object.len() != 4 || !object.contains_key("paid_intervals") {
                return Err(corrupt(CorruptAttemptReason::ResultShape));
            }
            let intervals = object
                .get("paid_intervals")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| corrupt(CorruptAttemptReason::ResultShape))?
                .iter()
                .map(parse_stored_interval)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(SourceObservation::Complete {
                source_id,
                evidence_reference,
                paid_intervals: intervals,
            })
        }
        "unavailable" => {
            if object.len() != 4 || !object.contains_key("reason") {
                return Err(corrupt(CorruptAttemptReason::ResultShape));
            }
            let reason = match stored_string(object, "reason", CorruptAttemptReason::ResultShape)?
                .as_str()
            {
                "needs_reconciliation" => UnavailableReason::NeedsReconciliation,
                "conflicting_evidence" => UnavailableReason::ConflictingEvidence,
                _ => return Err(corrupt(CorruptAttemptReason::ResultShape)),
            };
            Ok(SourceObservation::Unavailable {
                source_id,
                evidence_reference,
                reason,
            })
        }
        _ => Err(corrupt(CorruptAttemptReason::ResultShape)),
    }
}

fn parse_stored_interval(
    value: &serde_json::Value,
) -> Result<ConfirmedPaidInterval, ReconciliationError> {
    let object = value
        .as_object()
        .ok_or_else(|| corrupt(CorruptAttemptReason::ResultShape))?;
    if object.len() != 5
        || ![
            "coverage_id",
            "source_id",
            "starts_at",
            "paid_until",
            "failed_renewal_id",
        ]
        .iter()
        .all(|field| object.contains_key(*field))
    {
        return Err(corrupt(CorruptAttemptReason::ResultShape));
    }
    let failed_renewal_id = match object.get("failed_renewal_id") {
        Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(value)) => Some(value.clone()),
        _ => return Err(corrupt(CorruptAttemptReason::ResultShape)),
    };
    Ok(ConfirmedPaidInterval {
        coverage_id: stored_string(object, "coverage_id", CorruptAttemptReason::ResultShape)?,
        source_id: stored_string(object, "source_id", CorruptAttemptReason::ResultShape)?,
        starts_at: stored_i64(object, "starts_at")?,
        paid_until: stored_i64(object, "paid_until")?,
        failed_renewal_id,
    })
}

fn stored_string(
    object: &serde_json::Map<String, serde_json::Value>,
    field: &str,
    reason: CorruptAttemptReason,
) -> Result<String, ReconciliationError> {
    object
        .get(field)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| corrupt(reason))
}

fn stored_i64(
    object: &serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Result<i64, ReconciliationError> {
    object
        .get(field)
        .and_then(serde_json::Value::as_i64)
        .ok_or_else(|| corrupt(CorruptAttemptReason::ResultShape))
}

fn corrupt(reason: CorruptAttemptReason) -> ReconciliationError {
    ReconciliationError::CorruptAttempt(reason)
}

fn validate_binding(binding: &SourceBinding) -> Result<(), ReconciliationError> {
    for (name, value) in [
        ("beneficiary_id", binding.beneficiary_id.as_str()),
        ("source_id", binding.source_id.as_str()),
        ("provider_namespace", binding.provider_namespace.as_str()),
        (
            "external_allocation_reference",
            binding.external_allocation_reference.as_str(),
        ),
        (
            "ownership_evidence_reference",
            binding.ownership_evidence_reference.as_str(),
        ),
    ] {
        if value.trim().is_empty() {
            return Err(ReconciliationError::InvalidSource(format!(
                "{name} must not be empty"
            )));
        }
    }
    Ok(())
}

fn source_binding_from_row(
    row: &sqlx::postgres::PgRow,
) -> Result<SourceBinding, ReconciliationError> {
    Ok(SourceBinding {
        beneficiary_id: row.try_get("beneficiary_id")?,
        source_id: row.try_get("source_id")?,
        provider_namespace: row.try_get("provider_namespace")?,
        external_allocation_reference: row.try_get("external_allocation_reference")?,
        ownership_evidence_reference: row.try_get("ownership_evidence_reference")?,
    })
}

fn map_source_insert_error(error: sqlx::Error) -> ReconciliationError {
    let is_binding_conflict = matches!(
        &error,
        sqlx::Error::Database(database)
            if matches!(
                database.constraint(),
                Some(
                    "cloud_coverage_sources_provider_allocation_key"
                        | "cloud_coverage_sources_pkey"
                )
            )
    );
    if is_binding_conflict {
        ReconciliationError::SourceBindingConflict
    } else {
        ReconciliationError::Database(error)
    }
}

#[allow(dead_code)]
fn projection_outcome(outcome: PublicationOutcome) -> RegistrationOutcome {
    match outcome {
        PublicationOutcome::Applied => RegistrationOutcome::Applied,
        PublicationOutcome::AlreadyApplied => RegistrationOutcome::AlreadyApplied,
    }
}

#[allow(dead_code)]
fn unavailable_projection() -> CoverageProjection {
    CoverageProjection::Unavailable {
        reason: UnavailableReason::NeedsReconciliation,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_receipt_matrix_covers_every_nullable_combination() {
        for status in [
            CollectionStatus::Pending,
            CollectionStatus::Superseded,
            CollectionStatus::Completed,
        ] {
            for present in 0..8u8 {
                let evidence = (present & 0b100 != 0).then(|| "aggregate-evidence".to_string());
                let result = (present & 0b010 != 0).then(|| "{}".to_string());
                let revision = (present & 0b001 != 0).then_some(1);
                let receipt = stored_completion_receipt(status, evidence, result, revision);
                let complete = status == CollectionStatus::Completed;
                if complete && present == 0b111 {
                    assert_eq!(
                        receipt
                            .expect("accept complete receipt")
                            .expect("return complete receipt"),
                        ("aggregate-evidence".to_string(), "{}".to_string(), 1)
                    );
                } else if !complete && present == 0 {
                    assert_eq!(receipt.expect("accept empty incomplete receipt"), None);
                } else {
                    assert!(
                        matches!(
                            receipt,
                            Err(ReconciliationError::CorruptAttempt(
                                CorruptAttemptReason::CompletionReceipt
                            ))
                        ),
                        "status {status:?} with fields {present:03b} must be a receipt corruption"
                    );
                }
            }
        }
    }

    #[test]
    fn stored_bindings_reject_unparseable_and_non_array_json() {
        for raw in [
            "not json{{",
            "{}",
            "\"bindings\"",
            "42",
            "null",
            "true",
            "[]",
            "[42]",
        ] {
            assert!(
                matches!(
                    parse_stored_bindings(raw, "beneficiary"),
                    Err(ReconciliationError::CorruptAttempt(
                        CorruptAttemptReason::BindingShape
                    ))
                ),
                "stored bindings {raw} must be a shape corruption"
            );
        }
    }

    #[test]
    fn stored_result_rejects_blank_evidence_and_non_object_json() {
        let ticket = CollectionTicket {
            beneficiary_id: "beneficiary".into(),
            attempt_id: "attempt".into(),
            collection_epoch: 1,
            source_set_generation: 1,
            expected_projection_revision: None,
            source_bindings: Vec::new(),
            status: CollectionStatus::Completed,
            completed_revision: Some(1),
        };
        assert!(matches!(
            validate_stored_result(&ticket, "  ", "{}"),
            Err(ReconciliationError::CorruptAttempt(
                CorruptAttemptReason::ResultEvidence
            ))
        ));
        for raw in ["[]", "\"result\"", "42", "null", "not json{{"] {
            assert!(
                matches!(
                    validate_stored_result(&ticket, "aggregate-evidence", raw),
                    Err(ReconciliationError::CorruptAttempt(
                        CorruptAttemptReason::ResultShape
                    ))
                ),
                "stored result {raw} must be a shape corruption"
            );
        }
    }
}
