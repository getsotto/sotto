//! Durable storage for verified person-level Cloud coverage projections.
//!
//! This module stores complete snapshots and does not interpret provider events. Callers own the
//! transaction so a future adapter can commit its provider cursor and this projection together.
//! The pure evaluator remains in [`crate::cloud_coverage`].

use std::fmt;

use sqlx::{PgPool, Postgres, Row, Transaction};
use thiserror::Error;

use crate::cloud_coverage::{
    normalise_confirmed_intervals, ConfirmedPaidInterval, InvalidCoverage, PersonCoverage,
};

/// Why a trusted producer could not publish a complete coverage history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnavailableReason {
    NeedsReconciliation,
    ConflictingEvidence,
}

impl UnavailableReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::NeedsReconciliation => "needs_reconciliation",
            Self::ConflictingEvidence => "conflicting_evidence",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "needs_reconciliation" => Some(Self::NeedsReconciliation),
            "conflicting_evidence" => Some(Self::ConflictingEvidence),
            _ => None,
        }
    }
}

impl fmt::Display for UnavailableReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The complete or explicitly unavailable result of a trusted projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoverageProjection {
    Complete {
        paid_intervals: Vec<ConfirmedPaidInterval>,
    },
    Unavailable {
        reason: UnavailableReason,
    },
}

/// The result of publishing one projection operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublicationReceipt {
    pub revision: i64,
    pub outcome: PublicationOutcome,
}

/// Whether the operation was newly applied or was an exact replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicationOutcome {
    Applied,
    AlreadyApplied,
}

/// A complete snapshot loaded from one committed database revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedCoverage {
    pub revision: i64,
    pub coverage: PersonCoverage,
}

/// Storage and projection contract failures.
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("beneficiary_id must not be empty")]
    EmptyBeneficiary,
    #[error("operation_id must not be empty")]
    EmptyOperationId,
    #[error("evidence_reference must not be empty")]
    EmptyEvidenceReference,
    #[error("expected revision must be positive")]
    InvalidExpectedRevision,
    #[error("invalid coverage: {0}")]
    InvalidCoverage(#[from] InvalidCoverage),
    #[error("projection revision is exhausted")]
    RevisionOverflow,
    #[error("projection is missing")]
    ProjectionMissing,
    #[error("projection is unavailable: {0}")]
    ProjectionUnavailable(UnavailableReason),
    #[error("publication operation conflicts with an existing request")]
    OperationConflict,
    #[error("projection revision conflict: expected {expected:?}, current {actual:?}")]
    RevisionConflict {
        expected: Option<i64>,
        actual: Option<i64>,
    },
    #[error("stored projection is corrupt: {0}")]
    CorruptProjection(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CanonicalProjection {
    status: &'static str,
    unavailable_reason: Option<UnavailableReason>,
    paid_intervals: Vec<ConfirmedPaidInterval>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StoredOperation {
    revision: i64,
    expected_revision: Option<i64>,
    evidence_reference: String,
    projection: CanonicalProjection,
}

/// Publish one complete or unavailable snapshot inside the caller-owned transaction.
///
/// The returned receipt is not durable until the caller commits `tx`. The beneficiary head and
/// immutable revision are changed atomically within that transaction.
/// Callers must roll back `tx` whenever this function returns an error.
pub async fn publish(
    tx: &mut Transaction<'_, Postgres>,
    beneficiary_id: &str,
    expected_revision: Option<i64>,
    operation_id: &str,
    evidence_reference: &str,
    projection: &CoverageProjection,
) -> Result<PublicationReceipt, StoreError> {
    validate_identifier(beneficiary_id, StoreError::EmptyBeneficiary)?;
    validate_identifier(operation_id, StoreError::EmptyOperationId)?;
    validate_identifier(evidence_reference, StoreError::EmptyEvidenceReference)?;
    if expected_revision.is_some_and(|revision| revision <= 0) {
        return Err(StoreError::InvalidExpectedRevision);
    }

    let canonical = canonical_projection(beneficiary_id, projection)?;
    let head_insert = sqlx::query(
        "INSERT INTO cloud_coverage_heads (beneficiary_id, current_revision) VALUES ($1, NULL) \
         ON CONFLICT (beneficiary_id) DO NOTHING",
    )
    .bind(beneficiary_id)
    .execute(&mut **tx)
    .await?;
    let mut head_inserted = head_insert.rows_affected() == 1;

    loop {
        let head = sqlx::query_scalar::<_, String>(
            "SELECT beneficiary_id FROM cloud_coverage_heads WHERE beneficiary_id = $1 FOR UPDATE",
        )
        .bind(beneficiary_id)
        .fetch_optional(&mut **tx)
        .await?;
        if head.is_some() {
            break;
        }

        // A concurrent first publisher can delete its empty head after returning a
        // revision conflict. Recreate it if this transaction's earlier insert saw
        // that uncommitted row and therefore did not insert a replacement.
        let retry_insert = sqlx::query(
            "INSERT INTO cloud_coverage_heads (beneficiary_id, current_revision) VALUES ($1, NULL) \
             ON CONFLICT (beneficiary_id) DO NOTHING",
        )
        .bind(beneficiary_id)
        .execute(&mut **tx)
        .await?;
        head_inserted |= retry_insert.rows_affected() == 1;
    }

    let existing = sqlx::query(
        "SELECT revision, expected_revision, evidence_reference, status, unavailable_reason, fact_count \
         FROM cloud_coverage_revisions \
         WHERE beneficiary_id = $1 AND operation_id = $2",
    )
    .bind(beneficiary_id)
    .bind(operation_id)
    .fetch_optional(&mut **tx)
    .await?;

    let current_revision: Option<i64> = sqlx::query_scalar(
        "SELECT current_revision FROM cloud_coverage_heads WHERE beneficiary_id = $1",
    )
    .bind(beneficiary_id)
    .fetch_one(&mut **tx)
    .await?;
    if current_revision.is_none() && !head_inserted {
        return Err(StoreError::CorruptProjection(
            "head has no current revision".into(),
        ));
    }

    if let Some(row) = existing {
        let stored = load_operation(tx, beneficiary_id, row).await?;
        if stored.expected_revision == expected_revision
            && stored.evidence_reference == evidence_reference
            && stored.projection == canonical
        {
            return Ok(PublicationReceipt {
                revision: stored.revision,
                outcome: PublicationOutcome::AlreadyApplied,
            });
        }
        return Err(StoreError::OperationConflict);
    }

    if current_revision != expected_revision {
        if head_inserted {
            sqlx::query(
                "DELETE FROM cloud_coverage_heads \
                 WHERE beneficiary_id = $1 AND current_revision IS NULL",
            )
            .bind(beneficiary_id)
            .execute(&mut **tx)
            .await?;
        }
        return Err(StoreError::RevisionConflict {
            expected: expected_revision,
            actual: current_revision,
        });
    }
    let revision = current_revision
        .unwrap_or(0)
        .checked_add(1)
        .ok_or(StoreError::RevisionOverflow)?;
    let (status, unavailable_reason) = (
        canonical.status,
        canonical.unavailable_reason.map(UnavailableReason::as_str),
    );

    sqlx::query(
        "INSERT INTO cloud_coverage_revisions \
         (beneficiary_id, revision, operation_id, expected_revision, evidence_reference, status, \
          unavailable_reason, fact_count) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(beneficiary_id)
    .bind(revision)
    .bind(operation_id)
    .bind(expected_revision)
    .bind(evidence_reference)
    .bind(status)
    .bind(unavailable_reason)
    .bind(canonical.paid_intervals.len() as i64)
    .execute(&mut **tx)
    .await?;

    for interval in &canonical.paid_intervals {
        sqlx::query(
            "INSERT INTO cloud_coverage_revision_facts \
             (beneficiary_id, revision, coverage_id, source_id, starts_at, paid_until, failed_renewal_id) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(beneficiary_id)
        .bind(revision)
        .bind(&interval.coverage_id)
        .bind(&interval.source_id)
        .bind(interval.starts_at)
        .bind(interval.paid_until)
        .bind(&interval.failed_renewal_id)
        .execute(&mut **tx)
        .await?;
    }

    sqlx::query("UPDATE cloud_coverage_heads SET current_revision = $2 WHERE beneficiary_id = $1")
        .bind(beneficiary_id)
        .bind(revision)
        .execute(&mut **tx)
        .await?;

    Ok(PublicationReceipt {
        revision,
        outcome: PublicationOutcome::Applied,
    })
}

/// Load one committed snapshot using a repeatable-read database snapshot.
pub async fn load(pool: &PgPool, beneficiary_id: &str) -> Result<LoadedCoverage, StoreError> {
    validate_identifier(beneficiary_id, StoreError::EmptyBeneficiary)?;
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *tx)
        .await?;

    let loaded = load_in_transaction(&mut tx, beneficiary_id).await?;
    tx.commit().await?;
    Ok(loaded)
}

/// Load a projection from the caller's repeatable-read snapshot without committing it.
pub(crate) async fn load_in_transaction(
    tx: &mut Transaction<'_, Postgres>,
    beneficiary_id: &str,
) -> Result<LoadedCoverage, StoreError> {
    validate_identifier(beneficiary_id, StoreError::EmptyBeneficiary)?;

    let current_revision: Option<i64> = sqlx::query_scalar(
        "SELECT current_revision FROM cloud_coverage_heads WHERE beneficiary_id = $1",
    )
    .bind(beneficiary_id)
    .fetch_optional(&mut **tx)
    .await?
    .flatten();
    let Some(revision) = current_revision else {
        let exists = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM cloud_coverage_heads WHERE beneficiary_id = $1)",
        )
        .bind(beneficiary_id)
        .fetch_one(&mut **tx)
        .await?;
        if !exists {
            return Err(StoreError::ProjectionMissing);
        }
        return Err(StoreError::CorruptProjection(
            "head has no current revision".into(),
        ));
    };

    let row = sqlx::query(
        "SELECT expected_revision, evidence_reference, status, unavailable_reason, fact_count \
         FROM cloud_coverage_revisions WHERE beneficiary_id = $1 AND revision = $2",
    )
    .bind(beneficiary_id)
    .bind(revision)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| {
        StoreError::CorruptProjection(format!("head points to missing revision {revision}"))
    })?;
    let status: String = row.try_get("status")?;
    let unavailable_reason: Option<String> = row.try_get("unavailable_reason")?;
    let fact_count: i64 = row.try_get("fact_count")?;
    let projection = projection_metadata(&status, unavailable_reason.as_deref(), fact_count)?;

    let expected_revision: Option<i64> = row.try_get("expected_revision")?;
    if expected_revision.is_some_and(|value| value <= 0) {
        return Err(StoreError::CorruptProjection(
            "expected revision is not positive".into(),
        ));
    }
    let evidence_reference: String = row.try_get("evidence_reference")?;
    if evidence_reference.trim().is_empty() {
        return Err(StoreError::CorruptProjection(
            "evidence reference is empty".into(),
        ));
    }

    let facts = sqlx::query(
        "SELECT coverage_id, source_id, starts_at, paid_until, failed_renewal_id \
         FROM cloud_coverage_revision_facts \
         WHERE beneficiary_id = $1 AND revision = $2 \
         ORDER BY coverage_id COLLATE \"C\"",
    )
    .bind(beneficiary_id)
    .bind(revision)
    .fetch_all(&mut **tx)
    .await?;
    if fact_count != facts.len() as i64 {
        return Err(StoreError::CorruptProjection(format!(
            "revision {revision} declares {fact_count} facts but stores {}",
            facts.len()
        )));
    }
    let mut paid_intervals = Vec::with_capacity(facts.len());
    for fact in facts {
        paid_intervals.push(ConfirmedPaidInterval {
            coverage_id: fact.try_get("coverage_id")?,
            source_id: fact.try_get("source_id")?,
            starts_at: fact.try_get("starts_at")?,
            paid_until: fact.try_get("paid_until")?,
            failed_renewal_id: fact.try_get("failed_renewal_id")?,
        });
    }
    let coverage = PersonCoverage {
        beneficiary_id: beneficiary_id.to_owned(),
        paid_intervals,
    };
    let canonical = normalise_confirmed_intervals(&coverage)
        .map_err(|error| StoreError::CorruptProjection(error.to_string()))?;
    if canonical != coverage.paid_intervals {
        return Err(StoreError::CorruptProjection(
            "stored facts are not in canonical order".into(),
        ));
    }
    if let Some(reason) = projection.unavailable_reason {
        return Err(StoreError::ProjectionUnavailable(reason));
    }
    Ok(LoadedCoverage { revision, coverage })
}

/// Read the committed projection revision inside a caller-owned transaction.
///
/// `None` means that no projection head exists. A head with a null revision is an invalid
/// committed state and is reported as corruption rather than being treated as empty coverage.
pub(crate) async fn current_revision(
    tx: &mut Transaction<'_, Postgres>,
    beneficiary_id: &str,
) -> Result<Option<i64>, StoreError> {
    validate_identifier(beneficiary_id, StoreError::EmptyBeneficiary)?;
    let head: Option<Option<i64>> = sqlx::query_scalar(
        "SELECT current_revision FROM cloud_coverage_heads WHERE beneficiary_id = $1",
    )
    .bind(beneficiary_id)
    .fetch_optional(&mut **tx)
    .await?;
    match head {
        None => Ok(None),
        Some(Some(revision)) => Ok(Some(revision)),
        Some(None) => Err(StoreError::CorruptProjection(
            "head has no current revision".into(),
        )),
    }
}

fn canonical_projection(
    beneficiary_id: &str,
    projection: &CoverageProjection,
) -> Result<CanonicalProjection, StoreError> {
    match projection {
        CoverageProjection::Complete { paid_intervals } => {
            let coverage = PersonCoverage {
                beneficiary_id: beneficiary_id.to_owned(),
                paid_intervals: paid_intervals.clone(),
            };
            Ok(CanonicalProjection {
                status: "complete",
                unavailable_reason: None,
                paid_intervals: normalise_confirmed_intervals(&coverage)?,
            })
        }
        CoverageProjection::Unavailable { reason } => Ok(CanonicalProjection {
            status: "unavailable",
            unavailable_reason: Some(*reason),
            paid_intervals: Vec::new(),
        }),
    }
}

fn projection_metadata(
    status: &str,
    unavailable_reason: Option<&str>,
    fact_count: i64,
) -> Result<CanonicalProjection, StoreError> {
    if fact_count < 0 {
        return Err(StoreError::CorruptProjection(
            "fact count is negative".into(),
        ));
    }
    match (
        status,
        unavailable_reason.and_then(UnavailableReason::parse),
    ) {
        ("complete", None) => Ok(CanonicalProjection {
            status: "complete",
            unavailable_reason: None,
            paid_intervals: Vec::new(),
        }),
        ("unavailable", Some(reason)) if fact_count == 0 => Ok(CanonicalProjection {
            status: "unavailable",
            unavailable_reason: Some(reason),
            paid_intervals: Vec::new(),
        }),
        _ => Err(StoreError::CorruptProjection(
            "invalid projection status or unavailable reason".into(),
        )),
    }
}

async fn load_operation(
    tx: &mut Transaction<'_, Postgres>,
    beneficiary_id: &str,
    row: sqlx::postgres::PgRow,
) -> Result<StoredOperation, StoreError> {
    let revision: i64 = row.try_get("revision")?;
    let expected_revision: Option<i64> = row.try_get("expected_revision")?;
    let evidence_reference: String = row.try_get("evidence_reference")?;
    let status: String = row.try_get("status")?;
    let unavailable_reason: Option<String> = row.try_get("unavailable_reason")?;
    let fact_count: i64 = row.try_get("fact_count")?;
    let facts = sqlx::query(
        "SELECT coverage_id, source_id, starts_at, paid_until, failed_renewal_id \
         FROM cloud_coverage_revision_facts WHERE beneficiary_id = $1 AND revision = $2 \
         ORDER BY coverage_id COLLATE \"C\"",
    )
    .bind(beneficiary_id)
    .bind(revision)
    .fetch_all(&mut **tx)
    .await?;
    let mut paid_intervals = Vec::with_capacity(facts.len());
    for fact in facts {
        paid_intervals.push(ConfirmedPaidInterval {
            coverage_id: fact.try_get("coverage_id")?,
            source_id: fact.try_get("source_id")?,
            starts_at: fact.try_get("starts_at")?,
            paid_until: fact.try_get("paid_until")?,
            failed_renewal_id: fact.try_get("failed_renewal_id")?,
        });
    }
    let projection = projection_metadata(&status, unavailable_reason.as_deref(), fact_count)?;
    if fact_count != paid_intervals.len() as i64 {
        return Err(StoreError::CorruptProjection(format!(
            "operation revision {revision} declares {fact_count} facts but stores {}",
            paid_intervals.len()
        )));
    }
    Ok(StoredOperation {
        revision,
        expected_revision,
        evidence_reference,
        projection: CanonicalProjection {
            paid_intervals,
            ..projection
        },
    })
}

fn validate_identifier<T>(value: &str, error: T) -> Result<(), T> {
    if value.trim().is_empty() {
        Err(error)
    } else {
        Ok(())
    }
}
