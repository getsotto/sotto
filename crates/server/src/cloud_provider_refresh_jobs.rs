//! Durable, leased work for provider refreshes.
//!
//! Event acceptance writes the receipt, invalidation fence, and queue row in one caller-owned
//! transaction.  Workers claim rows with `SKIP LOCKED`; a crashed worker is recoverable after the
//! lease expires, and repeated failures eventually become visible poison rather than an endless
//! retry loop.  The module intentionally does not perform provider I/O: the refresh adapter owns
//! that policy and can consume a lease in a later runtime slice.

use std::time::Duration;

use sqlx::{PgPool, Postgres, Row, Transaction};
use thiserror::Error;
use uuid::Uuid;

use crate::cloud_provider::{ProviderContext, ProviderEnvironment, VerifiedProviderEvent};

const RETRY_DELAYS: [Duration; 6] = [
    Duration::from_secs(60),
    Duration::from_secs(5 * 60),
    Duration::from_secs(30 * 60),
    Duration::from_secs(2 * 60 * 60),
    Duration::from_secs(6 * 60 * 60),
    Duration::from_secs(24 * 60 * 60),
];

#[derive(Debug, Error)]
pub enum RefreshJobError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("refresh worker id is empty")]
    EmptyWorkerId,
    #[error("refresh job ownership identifiers must not be empty")]
    EmptyOwnership,
    #[error("refresh job is not owned by this worker")]
    LeaseLost,
    #[error("refresh job retry code is empty")]
    EmptyErrorCode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueDisposition {
    Enqueued,
    AlreadyQueued,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshJobLease {
    pub job_id: String,
    pub context: ProviderContext,
    pub event_id: String,
    pub beneficiary_id: String,
    pub allocation_id: String,
    pub source_id: String,
    pub worker_id: String,
    pub attempt_count: i32,
}

/// Retry delays after a failed lease. `attempt_count` is the count after claiming the job.
pub fn retry_delay(attempt_count: i32) -> Option<Duration> {
    if attempt_count <= 0 {
        return Some(RETRY_DELAYS[0]);
    }
    RETRY_DELAYS.get(attempt_count as usize - 1).copied()
}

/// Insert the event job after its receipt and invalidation have been validated.
pub async fn enqueue_event(
    tx: &mut Transaction<'_, Postgres>,
    context: &ProviderContext,
    event: &VerifiedProviderEvent,
    beneficiary_id: &str,
    allocation_id: &str,
    source_id: &str,
) -> Result<EnqueueDisposition, RefreshJobError> {
    if beneficiary_id.trim().is_empty()
        || allocation_id.trim().is_empty()
        || source_id.trim().is_empty()
    {
        return Err(RefreshJobError::EmptyOwnership);
    }
    let job_id = format!("provider-refresh:{}", Uuid::new_v4());
    let inserted = sqlx::query(
        "INSERT INTO cloud_provider_refresh_jobs \
         (job_id, provider_namespace, provider_account_id, provider_environment, event_id, \
          beneficiary_id, allocation_id, coverage_source_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
         ON CONFLICT (provider_namespace, provider_account_id, provider_environment, event_id) \
         DO NOTHING",
    )
    .bind(job_id)
    .bind(&context.namespace)
    .bind(&context.account_id)
    .bind(context.environment.as_str())
    .bind(&event.event_id)
    .bind(beneficiary_id)
    .bind(allocation_id)
    .bind(source_id)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    Ok(if inserted == 1 {
        EnqueueDisposition::Enqueued
    } else {
        EnqueueDisposition::AlreadyQueued
    })
}

/// Claim one pending or expired leased job. Database locking makes this safe across instances.
pub async fn claim_due(
    pool: &PgPool,
    worker_id: &str,
) -> Result<Option<RefreshJobLease>, RefreshJobError> {
    if worker_id.trim().is_empty() {
        return Err(RefreshJobError::EmptyWorkerId);
    }
    let mut tx = pool.begin().await?;
    let row = sqlx::query(
        "SELECT job_id FROM cloud_provider_refresh_jobs \
         WHERE available_at <= now() AND (status = 'pending' \
             OR (status = 'leased' AND lease_expires_at <= now())) \
         ORDER BY available_at, created_at, job_id \
         LIMIT 1 FOR UPDATE SKIP LOCKED",
    )
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        tx.commit().await?;
        return Ok(None);
    };
    let job_id: String = row.try_get("job_id")?;
    let row = sqlx::query(
        "UPDATE cloud_provider_refresh_jobs \
         SET status = 'leased', lease_owner = $1, lease_expires_at = now() + interval '5 minutes', \
             attempt_count = attempt_count + 1, updated_at = now() \
         WHERE job_id = $2 AND (status = 'pending' OR (status = 'leased' AND lease_expires_at <= now())) \
         RETURNING provider_namespace, provider_account_id, provider_environment, event_id, \
                   beneficiary_id, allocation_id, coverage_source_id, attempt_count",
    )
    .bind(worker_id)
    .bind(&job_id)
    .fetch_one(&mut *tx)
    .await?;
    let context = ProviderContext::new(
        row.try_get::<String, _>("provider_namespace")?,
        row.try_get::<String, _>("provider_account_id")?,
        ProviderEnvironment::parse(row.try_get::<String, _>("provider_environment")?.as_str())
            .map_err(|error| sqlx::Error::Protocol(error.to_string()))?,
    )
    .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
    let lease = RefreshJobLease {
        job_id,
        context,
        event_id: row.try_get("event_id")?,
        beneficiary_id: row.try_get("beneficiary_id")?,
        allocation_id: row.try_get("allocation_id")?,
        source_id: row.try_get("coverage_source_id")?,
        worker_id: worker_id.into(),
        attempt_count: row.try_get("attempt_count")?,
    };
    tx.commit().await?;
    Ok(Some(lease))
}

/// Mark a successfully consumed job. A stale worker cannot complete a replacement lease.
pub async fn complete(pool: &PgPool, lease: &RefreshJobLease) -> Result<(), RefreshJobError> {
    let affected = sqlx::query(
        "UPDATE cloud_provider_refresh_jobs \
         SET status = 'completed', lease_owner = NULL, lease_expires_at = NULL, \
             completed_at = now(), updated_at = now() \
         WHERE job_id = $1 AND status = 'leased' AND lease_owner = $2 \
           AND lease_expires_at > now()",
    )
    .bind(&lease.job_id)
    .bind(&lease.worker_id)
    .execute(pool)
    .await?
    .rows_affected();
    if affected != 1 {
        return Err(RefreshJobError::LeaseLost);
    }
    Ok(())
}

/// Record a failure, keeping bounded retry state in the durable row.
pub async fn fail(
    pool: &PgPool,
    lease: &RefreshJobLease,
    error_code: &str,
) -> Result<bool, RefreshJobError> {
    if error_code.trim().is_empty() {
        return Err(RefreshJobError::EmptyErrorCode);
    }
    let Some(delay) = retry_delay(lease.attempt_count) else {
        let affected = sqlx::query(
            "UPDATE cloud_provider_refresh_jobs \
             SET status = 'poisoned', lease_owner = NULL, lease_expires_at = NULL, \
                 last_error_code = $1, updated_at = now() \
             WHERE job_id = $2 AND status = 'leased' AND lease_owner = $3 \
               AND lease_expires_at > now()",
        )
        .bind(error_code)
        .bind(&lease.job_id)
        .bind(&lease.worker_id)
        .execute(pool)
        .await?
        .rows_affected();
        if affected != 1 {
            return Err(RefreshJobError::LeaseLost);
        }
        return Ok(false);
    };
    let affected = sqlx::query(
        "UPDATE cloud_provider_refresh_jobs \
         SET status = 'pending', available_at = now() + $1::interval, lease_owner = NULL, \
             lease_expires_at = NULL, last_error_code = $2, updated_at = now() \
         WHERE job_id = $3 AND status = 'leased' AND lease_owner = $4 \
           AND lease_expires_at > now()",
    )
    .bind(format!("{} seconds", delay.as_secs()))
    .bind(error_code)
    .bind(&lease.job_id)
    .bind(&lease.worker_id)
    .execute(pool)
    .await?
    .rows_affected();
    if affected != 1 {
        return Err(RefreshJobError::LeaseLost);
    }
    Ok(true)
}

/// Release a lease during graceful shutdown so another worker can reclaim it immediately.
pub async fn release(pool: &PgPool, lease: &RefreshJobLease) -> Result<(), RefreshJobError> {
    let affected = sqlx::query(
        "UPDATE cloud_provider_refresh_jobs \
         SET status = 'pending', available_at = now(), lease_owner = NULL, \
             lease_expires_at = NULL, updated_at = now() \
         WHERE job_id = $1 AND status = 'leased' AND lease_owner = $2 \
           AND lease_expires_at > now()",
    )
    .bind(&lease.job_id)
    .bind(&lease.worker_id)
    .execute(pool)
    .await?
    .rows_affected();
    if affected != 1 {
        return Err(RefreshJobError::LeaseLost);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::retry_delay;
    use std::time::Duration;

    #[test]
    fn retry_schedule_is_bounded_and_ordered() {
        assert_eq!(retry_delay(1), Some(Duration::from_secs(60)));
        assert_eq!(retry_delay(2), Some(Duration::from_secs(5 * 60)));
        assert_eq!(retry_delay(6), Some(Duration::from_secs(24 * 60 * 60)));
        assert_eq!(retry_delay(7), None);
    }

    #[test]
    fn first_failure_uses_the_short_delay() {
        assert_eq!(retry_delay(0), Some(Duration::from_secs(60)));
    }
}
