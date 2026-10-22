//! Freshness-bound authority derived only from complete, generation-fenced collections.

use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Row, Transaction};
use thiserror::Error;

use crate::{
    cloud_coverage_reconciliation::SourceBinding,
    cloud_coverage_store::{self, LoadedCoverage, StoreError},
};

pub const POLICY_VERSION: i32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthorityState {
    Current {
        loaded: LoadedCoverage,
        fresh_until_epoch: i64,
    },
    Missing,
    Stale,
}

#[derive(Debug, Error)]
pub enum AuthorityError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("coverage store error: {0}")]
    Store(#[from] StoreError),
    #[error("authority receipt is corrupt: {0}")]
    Corrupt(String),
}

/// Load coverage and its authority receipt from one repeatable-read snapshot.
pub async fn load_current(
    pool: &PgPool,
    beneficiary_id: &str,
    now_epoch: i64,
) -> Result<AuthorityState, AuthorityError> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *tx)
        .await?;
    let result = load_current_in_transaction(&mut tx, beneficiary_id, now_epoch).await?;
    tx.commit().await?;
    Ok(result)
}

pub(crate) async fn load_current_in_transaction(
    tx: &mut Transaction<'_, Postgres>,
    beneficiary_id: &str,
    now_epoch: i64,
) -> Result<AuthorityState, AuthorityError> {
    let loaded = match cloud_coverage_store::load_in_transaction(tx, beneficiary_id).await {
        Ok(loaded) => loaded,
        Err(StoreError::ProjectionMissing) => {
            return Ok(AuthorityState::Missing);
        }
        Err(StoreError::ProjectionUnavailable(_)) => {
            return Ok(AuthorityState::Stale);
        }
        Err(error) => return Err(AuthorityError::Store(error)),
    };
    let row = sqlx::query(
        "SELECT receipt.provider_namespace, receipt.provider_account_id, \
                receipt.provider_environment, receipt.source_set_generation, \
                receipt.provider_invalidation_generation, receipt.policy_version, \
                receipt.local_correction_digest, \
                extract(epoch FROM receipt.fresh_until)::bigint AS fresh_until, \
                extract(epoch FROM receipt.observed_at)::bigint AS observed_at, \
                attempt.status, attempt.projection_revision, attempt.source_set_generation AS attempt_source_generation, \
                attempt.provider_invalidation_generation AS attempt_invalidation_generation, \
                attempt.source_bindings::text AS source_bindings, \
                coordinator.source_set_generation AS current_source_generation, \
                coordinator.provider_invalidation_generation AS current_invalidation_generation \
         FROM cloud_coverage_authority_receipts AS receipt \
         JOIN cloud_coverage_collection_attempts AS attempt \
           ON attempt.beneficiary_id = receipt.beneficiary_id \
          AND attempt.attempt_id = receipt.attempt_id \
         JOIN cloud_coverage_coordinators AS coordinator \
           ON coordinator.beneficiary_id = receipt.beneficiary_id \
         WHERE receipt.beneficiary_id = $1 AND receipt.projection_revision = $2",
    )
    .bind(beneficiary_id)
    .bind(loaded.revision)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row else {
        return Ok(AuthorityState::Stale);
    };

    let fresh_until: i64 = row.try_get("fresh_until")?;
    let observed_at: i64 = row.try_get("observed_at")?;
    let provider_namespace: String = row.try_get("provider_namespace")?;
    let provider_account_id: String = row.try_get("provider_account_id")?;
    let provider_environment: String = row.try_get("provider_environment")?;
    let valid = provider_namespace == "stripe"
        && matches!(provider_environment.as_str(), "test" | "live")
        && row.try_get::<i64, _>("source_set_generation")?
            == row.try_get::<i64, _>("current_source_generation")?
        && row.try_get::<i64, _>("provider_invalidation_generation")?
            == row.try_get::<i64, _>("current_invalidation_generation")?
        && row.try_get::<i32, _>("policy_version")? == POLICY_VERSION
        && row.try_get::<String, _>("status")? == "completed"
        && row.try_get::<Option<i64>, _>("projection_revision")? == Some(loaded.revision)
        && row.try_get::<i64, _>("attempt_source_generation")?
            == row.try_get::<i64, _>("source_set_generation")?
        && row.try_get::<Option<i64>, _>("attempt_invalidation_generation")?
            == Some(row.try_get::<i64, _>("provider_invalidation_generation")?)
        && row.try_get::<String, _>("local_correction_digest")?
            == current_local_correction_digest(tx, beneficiary_id).await?
        && observed_at > 0
        && observed_at <= now_epoch
        && now_epoch < fresh_until
        && source_set_matches(
            tx,
            beneficiary_id,
            row.try_get("source_bindings")?,
            &provider_namespace,
            &provider_account_id,
            &provider_environment,
        )
        .await?;
    if valid {
        Ok(AuthorityState::Current {
            loaded,
            fresh_until_epoch: fresh_until,
        })
    } else {
        Ok(AuthorityState::Stale)
    }
}

async fn current_local_correction_digest(
    tx: &mut Transaction<'_, Postgres>,
    beneficiary_id: &str,
) -> Result<String, AuthorityError> {
    let rows = sqlx::query(
        "SELECT source.source_id, MIN(request.effective_at_epoch) AS effective_at_epoch \
         FROM cloud_coverage_sources AS source \
         JOIN cloud_provider_allocations AS allocation \
           ON allocation.coverage_source_id = source.source_id \
          AND allocation.beneficiary_id = source.beneficiary_id \
         JOIN cloud_provider_payers AS payer ON payer.payer_id = allocation.payer_id \
         LEFT JOIN billing_correction_requests AS request \
           ON request.beneficiary_id = allocation.beneficiary_id \
          AND payer.payer_kind = 'personal' \
          AND request.payer_kind = 'personal' \
          AND request.subscription_id = allocation.provider_subscription_id \
          AND request.state = 'refunded' AND request.full_refund_requested \
          AND NOT request.preserve_paid_term \
          AND request.early_termination_confirmed_at_epoch IS NOT NULL \
          AND request.effective_at_epoch IS NOT NULL \
         WHERE source.beneficiary_id = $1 \
           AND NOT EXISTS (SELECT 1 FROM cloud_coverage_source_retirements AS retirement \
                           WHERE retirement.beneficiary_id = source.beneficiary_id \
                             AND retirement.source_id = source.source_id) \
         GROUP BY source.source_id ORDER BY source.source_id COLLATE \"C\"",
    )
    .bind(beneficiary_id)
    .fetch_all(&mut **tx)
    .await?;
    let mut material = String::new();
    for row in rows {
        let source_id: String = row.try_get("source_id")?;
        let effective_at: Option<i64> = row.try_get("effective_at_epoch")?;
        material.push_str(&format!("{}:{}\n", source_id.len(), source_id));
        match effective_at {
            Some(epoch) => material.push_str(&format!("{epoch}\n")),
            None => material.push_str("none\n"),
        }
    }
    let digest = Sha256::digest(material.as_bytes());
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

async fn source_set_matches(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    beneficiary_id: &str,
    source_bindings_json: String,
    provider_namespace: &str,
    provider_account_id: &str,
    provider_environment: &str,
) -> Result<bool, AuthorityError> {
    let expected: Vec<SourceBinding> = serde_json::from_str(&source_bindings_json)
        .map_err(|error| AuthorityError::Corrupt(error.to_string()))?;
    let rows = sqlx::query(
        "SELECT source.beneficiary_id, source.source_id, source.provider_namespace, \
                allocation.external_allocation_reference, allocation.ownership_evidence_reference, \
                allocation.provider_namespace AS allocation_namespace, \
                allocation.provider_account_id, allocation.provider_environment \
         FROM cloud_coverage_sources AS source \
         JOIN cloud_provider_allocations AS allocation \
           ON allocation.coverage_source_id = source.source_id \
          AND allocation.beneficiary_id = source.beneficiary_id \
         WHERE source.beneficiary_id = $1 \
           AND NOT EXISTS (SELECT 1 FROM cloud_coverage_source_retirements AS retirement \
                           WHERE retirement.beneficiary_id = source.beneficiary_id \
                             AND retirement.source_id = source.source_id) \
         ORDER BY source.source_id COLLATE \"C\"",
    )
    .bind(beneficiary_id)
    .fetch_all(&mut **tx)
    .await?;
    let mut context_matches = true;
    let current = rows
        .into_iter()
        .map(|row| {
            context_matches &= row.try_get::<String, _>("allocation_namespace")?
                == provider_namespace
                && row.try_get::<String, _>("provider_account_id")? == provider_account_id
                && row.try_get::<String, _>("provider_environment")? == provider_environment;
            Ok(SourceBinding {
                beneficiary_id: row.try_get("beneficiary_id")?,
                source_id: row.try_get("source_id")?,
                provider_namespace: row.try_get("provider_namespace")?,
                external_allocation_reference: row.try_get("external_allocation_reference")?,
                ownership_evidence_reference: row.try_get("ownership_evidence_reference")?,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()?;
    Ok(context_matches && expected == current)
}
