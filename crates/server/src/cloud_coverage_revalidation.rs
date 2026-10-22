//! Durable fairness queue for full-source Cloud authority revalidation.

use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::cloud_provider_stripe_refresh_executor::StripeRefreshExecutor;

const LEASE_SECONDS: i64 = 300;
const MAX_BACKOFF_SECONDS: i64 = 6 * 60 * 60;

pub async fn run_once(
    pool: &PgPool,
    worker_id: &str,
    executor: &mut StripeRefreshExecutor,
) -> Result<bool, sqlx::Error> {
    let lease_token = Uuid::new_v4().to_string();
    let row = sqlx::query(
        "WITH candidate AS ( \
             SELECT beneficiary_id FROM cloud_coverage_revalidation_jobs \
             WHERE (status = 'pending' AND next_attempt_at <= now()) \
                OR (status = 'leased' AND lease_expires_at <= now()) \
             ORDER BY next_attempt_at, beneficiary_id COLLATE \"C\" \
             FOR UPDATE SKIP LOCKED LIMIT 1 \
         ) \
         UPDATE cloud_coverage_revalidation_jobs AS job \
         SET status = 'leased', lease_owner = $1, lease_token = $2, \
             lease_expires_at = now() + ($3 * interval '1 second'), updated_at = now() \
         FROM candidate WHERE job.beneficiary_id = candidate.beneficiary_id \
         RETURNING job.beneficiary_id, job.lease_token, job.attempt_count",
    )
    .bind(worker_id)
    .bind(&lease_token)
    .bind(LEASE_SECONDS)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(false);
    };
    let beneficiary_id: String = row.try_get("beneficiary_id")?;
    let token: String = row.try_get("lease_token")?;
    let previous_attempts: i32 = row.try_get("attempt_count")?;

    let result = executor.revalidate_beneficiary(&beneficiary_id).await;
    match result {
        Ok(()) => {
            sqlx::query(
                "UPDATE cloud_coverage_revalidation_jobs SET status = 'pending', \
                 next_attempt_at = now() + ($3 * (1 + random() * 0.25) * interval '1 second'), \
                 attempt_count = 0, \
                 lease_owner = NULL, lease_token = NULL, lease_expires_at = NULL, \
                 last_error_code = NULL, updated_at = now() \
                 WHERE beneficiary_id = $1 AND lease_token = $2 AND status = 'leased'",
            )
            .bind(&beneficiary_id)
            .bind(&token)
            .bind(executor.authority_refresh_interval_seconds())
            .execute(pool)
            .await?;
        }
        Err(code) => {
            let attempts = previous_attempts.saturating_add(1);
            let exponent = attempts.clamp(1, 16) as u32;
            let delay = (30_i64.saturating_mul(1_i64 << exponent)).min(MAX_BACKOFF_SECONDS);
            sqlx::query(
                "UPDATE cloud_coverage_revalidation_jobs SET status = 'pending', \
                 next_attempt_at = now() + ($3 * interval '1 second') \
                     + (random() * 30 * interval '1 second'), attempt_count = $4, \
                 lease_owner = NULL, lease_token = NULL, lease_expires_at = NULL, \
                 last_error_code = $5, updated_at = now() \
                 WHERE beneficiary_id = $1 AND lease_token = $2 AND status = 'leased'",
            )
            .bind(&beneficiary_id)
            .bind(&token)
            .bind(delay)
            .bind(attempts)
            .bind(code)
            .execute(pool)
            .await?;
        }
    }
    Ok(true)
}
