//! Orchestration for provider-backed Cloud coverage refreshes.
//!
//! The lower-level provider adapter deliberately exposes caller-owned transactions. This module
//! owns the lifecycle around those primitives: prepare and commit a durable ticket, collect
//! provider history without a SQL connection, then complete and commit the exact ticket.

use sqlx::PgPool;
use thiserror::Error;
use uuid::Uuid;

use crate::cloud_provider::{
    collect_provider_history, complete_verified_event, prepare_verified_event, ApplyReceipt,
    CollectionLimits, ProviderAdapterError, ProviderCollectionError, ProviderContext,
    ProviderHistoryClient, VerifiedAllocation, VerifiedProviderEvent,
};

/// Failure returned by one provider refresh lifecycle.
///
/// The operation preserves the lower-level error categories so callers can distinguish a
/// superseded attempt, a malformed provider page, and a database failure without parsing text.
#[derive(Debug, Error)]
pub enum ProviderRefreshError {
    #[error("provider refresh preparation failed: {0}")]
    Preparation(ProviderAdapterError),
    #[error("provider refresh preparation rollback failed after {error}: {rollback}")]
    PreparationRollback {
        error: ProviderAdapterError,
        rollback: sqlx::Error,
    },
    #[error("provider refresh preparation commit failed: {0}")]
    PreparationCommit(sqlx::Error),
    #[error("provider history collection failed: {0}")]
    Collection(ProviderCollectionError),
    #[error("provider refresh completion failed: {0}")]
    Completion(ProviderAdapterError),
    #[error("provider refresh completion rollback failed after {error}: {rollback}")]
    CompletionRollback {
        error: ProviderAdapterError,
        rollback: sqlx::Error,
    },
    #[error("provider refresh completion commit failed: {0}")]
    CompletionCommit(sqlx::Error),
    #[error("provider refresh database transaction could not start: {0}")]
    Transaction(sqlx::Error),
}

/// Prepare, collect, and complete one provider coverage refresh.
///
/// Preparation and completion use separate short transactions. No transaction or checked-out
/// connection remains alive while the provider client performs network I/O. Each invocation gets
/// a fresh run identity; a caller that needs to retry after collection failure must invoke this
/// operation again rather than reusing the old ticket.
pub async fn refresh_verified_event<C: ProviderHistoryClient + ?Sized>(
    pool: &PgPool,
    client: &mut C,
    context: &ProviderContext,
    event: &VerifiedProviderEvent,
    allocation: &VerifiedAllocation,
    limits: CollectionLimits,
) -> Result<ApplyReceipt, ProviderRefreshError> {
    limits
        .validate()
        .map_err(ProviderRefreshError::Collection)?;

    let run_id = format!("provider-refresh:{}", Uuid::new_v4());
    let mut preparation_tx = pool
        .begin()
        .await
        .map_err(ProviderRefreshError::Transaction)?;
    let preparation = match prepare_verified_event(
        &mut preparation_tx,
        context,
        event,
        allocation,
        &run_id,
    )
    .await
    {
        Ok(preparation) => preparation,
        Err(error) => {
            return match preparation_tx.rollback().await {
                Ok(()) => Err(ProviderRefreshError::Preparation(error)),
                Err(rollback) => Err(ProviderRefreshError::PreparationRollback { error, rollback }),
            };
        }
    };
    preparation_tx
        .commit()
        .await
        .map_err(ProviderRefreshError::PreparationCommit)?;

    let collection =
        collect_provider_history(client, context, &preparation.ticket.source_bindings, limits)
            .await
            .map_err(ProviderRefreshError::Collection)?;

    let mut completion_tx = pool
        .begin()
        .await
        .map_err(ProviderRefreshError::Transaction)?;
    let receipt = match complete_verified_event(
        &mut completion_tx,
        context,
        event,
        allocation,
        &preparation,
        &collection,
    )
    .await
    {
        Ok(receipt) => receipt,
        Err(error) => {
            return match completion_tx.rollback().await {
                Ok(()) => Err(ProviderRefreshError::Completion(error)),
                Err(rollback) => Err(ProviderRefreshError::CompletionRollback { error, rollback }),
            };
        }
    };
    completion_tx
        .commit()
        .await
        .map_err(ProviderRefreshError::CompletionCommit)?;
    Ok(receipt)
}
