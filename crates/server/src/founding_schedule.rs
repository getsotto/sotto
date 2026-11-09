//! Durable retries for Stripe schedules that enact founding-price end dates.
//!
//! The award is the customer contract; this queue records whether Stripe accepted its future
//! phase. Provider I/O always happens after the claim transaction commits.

use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

use crate::billing::{ProviderError, ProviderErrorKind};
use crate::founding_allocator::{FoundingAward, FoundingPayerKind};

const LEASE_SECONDS: i64 = 120;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundingScheduleJob {
    pub job_id: String,
    pub award_id: String,
    pub beneficiary_id: String,
    pub payer_kind: FoundingPayerKind,
    pub payer_id: String,
    pub subscription_id: String,
    pub idempotency_key: String,
    pub lease_token: String,
    pub attempt_count: i32,
}

pub async fn enqueue(
    tx: &mut Transaction<'_, Postgres>,
    award: &FoundingAward,
    subscription_id: &str,
) -> Result<(), sqlx::Error> {
    if subscription_id.trim().is_empty() {
        return Err(sqlx::Error::Protocol(
            "empty founding subscription id".into(),
        ));
    }
    let job_id = format!("founding-schedule:{}", award.award_id);
    let idempotency_key = format!("founding-transition:{}:1", award.award_id);
    let price_notice_eligible = award
        .original_end_epoch
        .is_some_and(|transition_epoch| transition_epoch > 0);
    sqlx::query(
        "INSERT INTO billing_founding_schedule_jobs \
         (job_id, award_id, beneficiary_id, payer_kind, payer_id, provider_subscription_id, \
          provider_idempotency_key, price_notice_eligible) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8) \
         ON CONFLICT (award_id) DO UPDATE SET \
           beneficiary_id = EXCLUDED.beneficiary_id, payer_kind = EXCLUDED.payer_kind, \
           payer_id = EXCLUDED.payer_id, provider_subscription_id = EXCLUDED.provider_subscription_id, \
           price_notice_eligible = EXCLUDED.price_notice_eligible, \
           state = 'pending', provider_schedule_id = NULL, attempt_count = 0, last_error_code = NULL, \
           next_attempt_at = now(), \
           version = billing_founding_schedule_jobs.version + 1, \
           provider_idempotency_key = 'founding-transition:' || billing_founding_schedule_jobs.award_id \
             || ':' || (billing_founding_schedule_jobs.version + 1)::text, \
           lease_token = NULL, lease_until = NULL, updated_at = now() \
         WHERE billing_founding_schedule_jobs.beneficiary_id IS DISTINCT FROM EXCLUDED.beneficiary_id \
            OR billing_founding_schedule_jobs.payer_kind IS DISTINCT FROM EXCLUDED.payer_kind \
            OR billing_founding_schedule_jobs.payer_id IS DISTINCT FROM EXCLUDED.payer_id \
            OR billing_founding_schedule_jobs.provider_subscription_id IS DISTINCT FROM EXCLUDED.provider_subscription_id",
    )
    .bind(job_id)
    .bind(&award.award_id)
    .bind(&award.beneficiary_id)
    .bind(award.payer_kind.as_str())
    .bind(&award.payer_id)
    .bind(subscription_id)
    .bind(idempotency_key)
    .bind(price_notice_eligible)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub async fn mark_invoice_unresolved(
    tx: &mut Transaction<'_, Postgres>,
    award_id: &str,
    error_code: &'static str,
) -> Result<(), sqlx::Error> {
    if !matches!(
        error_code,
        "invoice_price_mismatch" | "invoice_boundary_missing"
    ) {
        return Err(sqlx::Error::Protocol(
            "invalid founding invoice transition error code".into(),
        ));
    }
    sqlx::query(
        "UPDATE billing_founding_schedule_jobs SET state = 'unresolved', \
           provider_schedule_id = NULL, last_error_code = $2, lease_token = NULL, \
           lease_until = NULL, next_attempt_at = now(), version = version + 1, \
           provider_idempotency_key = 'founding-transition:' || award_id || ':' || (version + 1)::text, \
           updated_at = now() \
         WHERE award_id = $1 AND state <> 'not_required' \
           AND (state <> 'unresolved' OR last_error_code IS DISTINCT FROM $2)",
    )
    .bind(award_id)
    .bind(error_code)
    .execute(&mut **tx)
    .await?;
    crate::founding_price_notices::mark_award_schedule_unresolved(tx, award_id).await?;
    Ok(())
}

pub async fn claim_due(pool: &PgPool) -> Result<Option<FoundingScheduleJob>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query(
        "SELECT job_id, award_id, beneficiary_id, payer_kind, payer_id, provider_subscription_id, \
                provider_idempotency_key, attempt_count \
         FROM billing_founding_schedule_jobs \
         WHERE state IN ('pending', 'unresolved') \
           AND next_attempt_at <= now() \
           AND (lease_until IS NULL OR lease_until <= now()) \
         ORDER BY created_at, job_id LIMIT 1 FOR UPDATE SKIP LOCKED",
    )
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        tx.commit().await?;
        return Ok(None);
    };
    let job_id: String = row.try_get("job_id")?;
    let lease_token = Uuid::new_v4().to_string();
    let row = sqlx::query(
        "UPDATE billing_founding_schedule_jobs SET lease_token = $2, \
           lease_until = now() + ($3 * interval '1 second'), attempt_count = attempt_count + 1, \
           updated_at = now() \
         WHERE job_id = $1 RETURNING job_id, award_id, beneficiary_id, payer_kind, payer_id, \
           provider_subscription_id, provider_idempotency_key, attempt_count",
    )
    .bind(&job_id)
    .bind(&lease_token)
    .bind(LEASE_SECONDS)
    .fetch_one(&mut *tx)
    .await?;
    let job = FoundingScheduleJob {
        job_id: row.try_get("job_id")?,
        award_id: row.try_get("award_id")?,
        beneficiary_id: row.try_get("beneficiary_id")?,
        payer_kind: match row.try_get::<String, _>("payer_kind")?.as_str() {
            "personal" => FoundingPayerKind::Personal,
            "sponsor" => FoundingPayerKind::Sponsor,
            _ => return Err(sqlx::Error::Protocol("invalid founding payer kind".into())),
        },
        payer_id: row.try_get("payer_id")?,
        subscription_id: row.try_get("provider_subscription_id")?,
        idempotency_key: row.try_get("provider_idempotency_key")?,
        lease_token,
        attempt_count: row.try_get("attempt_count")?,
    };
    tx.commit().await?;
    Ok(Some(job))
}

pub async fn mark_scheduled(
    pool: &PgPool,
    job: &FoundingScheduleJob,
    schedule_id: &str,
) -> Result<bool, sqlx::Error> {
    let changed = sqlx::query(
        "UPDATE billing_founding_schedule_jobs SET state = 'scheduled', provider_schedule_id = $3, \
           last_error_code = NULL, lease_token = NULL, lease_until = NULL, \
           next_attempt_at = now(), updated_at = now() \
         WHERE job_id = $1 AND lease_token = $2 AND state IN ('pending', 'unresolved') \
           AND NOT (payer_kind = 'personal' AND EXISTS ( \
             SELECT 1 FROM billing_personal_accounts AS account \
             WHERE account.user_id = billing_founding_schedule_jobs.beneficiary_id \
               AND account.stripe_subscription_id = billing_founding_schedule_jobs.provider_subscription_id \
               AND (account.cancel_at_period_end OR account.cancellation_requested_at IS NOT NULL) \
           ))",
    )
    .bind(&job.job_id)
    .bind(&job.lease_token)
    .bind(schedule_id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(changed == 1)
}

pub async fn mark_unresolved(
    pool: &PgPool,
    job: &FoundingScheduleJob,
    error: &ProviderError,
) -> Result<bool, sqlx::Error> {
    let code = provider_error_code(error);
    let changed = sqlx::query(
        "UPDATE billing_founding_schedule_jobs SET state = 'unresolved', last_error_code = $3, \
           lease_token = NULL, lease_until = NULL, \
           next_attempt_at = now() + make_interval(secs => LEAST(3600, power(2, LEAST(attempt_count, 10))::integer)), \
           updated_at = now() \
         WHERE job_id = $1 AND lease_token = $2 AND state IN ('pending', 'unresolved')",
    )
    .bind(&job.job_id)
    .bind(&job.lease_token)
    .bind(code)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(changed == 1)
}

pub async fn mark_not_required(
    pool: &PgPool,
    job: &FoundingScheduleJob,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let changed = sqlx::query(
        "UPDATE billing_founding_schedule_jobs SET state = 'not_required', \
           last_error_code = 'subscription_terminates_before_transition', lease_token = NULL, \
           lease_until = NULL, updated_at = now(), next_attempt_at = now() \
         WHERE job_id = $1 AND lease_token = $2 AND state IN ('pending', 'unresolved')",
    )
    .bind(&job.job_id)
    .bind(&job.lease_token)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if changed == 1 {
        crate::founding_price_notices::retire_award_notice(
            &mut tx,
            &job.award_id,
            "subscription_ending",
        )
        .await?;
    }
    tx.commit().await?;
    Ok(changed == 1)
}

/// Persist a cancellation request before contacting Stripe and make any founding transition
/// work retryable. The worker checks this marker before and after updating a schedule, closing the
/// race where a schedule update started just before the customer requested cancellation.
pub async fn request_personal_cancellation(
    tx: &mut Transaction<'_, Postgres>,
    beneficiary_id: &str,
    subscription_id: &str,
) -> Result<(), sqlx::Error> {
    let updated = sqlx::query(
        "UPDATE billing_personal_accounts SET cancellation_requested_at = \
           COALESCE(cancellation_requested_at, now()), updated_at = now() \
         WHERE user_id = $1 AND stripe_subscription_id = $2",
    )
    .bind(beneficiary_id)
    .bind(subscription_id)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    if updated != 1 {
        return Err(sqlx::Error::RowNotFound);
    }

    sqlx::query(
        "UPDATE billing_founding_schedule_jobs SET \
           state = CASE WHEN lease_token IS NULL THEN 'pending' ELSE state END, \
           provider_schedule_id = CASE WHEN lease_token IS NULL THEN NULL ELSE provider_schedule_id END, \
           last_error_code = CASE WHEN lease_token IS NULL THEN NULL ELSE last_error_code END, \
           next_attempt_at = CASE WHEN lease_token IS NULL THEN now() ELSE next_attempt_at END, \
           updated_at = now() \
         WHERE beneficiary_id = $1 AND payer_kind = 'personal' \
           AND provider_subscription_id = $2 AND state <> 'not_required'",
    )
    .bind(beneficiary_id)
    .bind(subscription_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Record that Stripe has accepted the end-of-period cancellation. A retrying worker may call
/// this after the HTTP request has already completed, so both updates are idempotent.
pub async fn record_personal_cancellation(
    pool: &PgPool,
    beneficiary_id: &str,
    subscription_id: &str,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    let updated = sqlx::query(
        "UPDATE billing_personal_accounts SET cancel_at_period_end = TRUE, \
           cancellation_requested_at = COALESCE(cancellation_requested_at, now()), \
           updated_at = now() WHERE user_id = $1 AND stripe_subscription_id = $2",
    )
    .bind(beneficiary_id)
    .bind(subscription_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if updated != 1 {
        return Err(sqlx::Error::RowNotFound);
    }
    sqlx::query(
        "UPDATE billing_founding_schedule_jobs SET \
           state = CASE WHEN lease_token IS NULL THEN 'not_required' ELSE state END, \
           last_error_code = CASE WHEN lease_token IS NULL \
             THEN 'subscription_terminates_before_transition' ELSE last_error_code END, \
           updated_at = now(), next_attempt_at = now() \
         WHERE beneficiary_id = $1 AND payer_kind = 'personal' \
           AND provider_subscription_id = $2 AND state <> 'not_required'",
    )
    .bind(beneficiary_id)
    .bind(subscription_id)
    .execute(&mut *tx)
    .await?;
    let award_id: Option<String> = sqlx::query_scalar(
        "SELECT award.award_id FROM billing_founding_awards award \
         JOIN billing_personal_accounts account ON account.user_id=award.beneficiary_id \
         WHERE award.beneficiary_id=$1 AND award.payer_kind='personal' AND award.payer_id=$1 \
           AND award.original_end_epoch IS NOT NULL \
           AND award.original_end_epoch >= COALESCE(account.paid_through_epoch, 0)",
    )
    .bind(beneficiary_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(award_id) = award_id {
        crate::founding_price_notices::retire_award_notice(
            &mut tx,
            &award_id,
            "subscription_ending",
        )
        .await?;
    }
    tx.commit().await
}

/// Requeue the founding transition when Stripe confirms that a scheduled cancellation was lifted.
/// A fresh provider idempotency key is required because the prior schedule update may have been
/// cached with `end_behavior=cancel`.
pub async fn resume_personal_transition(
    tx: &mut Transaction<'_, Postgres>,
    beneficiary_id: &str,
    subscription_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE billing_founding_schedule_jobs SET state = 'pending', \
           provider_schedule_id = NULL, last_error_code = NULL, next_attempt_at = now(), \
           version = version + 1, provider_idempotency_key = \
             'founding-transition:' || award_id || ':' || (version + 1)::text, \
           updated_at = now() \
         WHERE beneficiary_id = $1 AND payer_kind = 'personal' \
           AND provider_subscription_id = $2 AND state IN ('not_required', 'scheduled') \
           AND lease_token IS NULL",
    )
    .bind(beneficiary_id)
    .bind(subscription_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn provider_error_code(error: &ProviderError) -> &'static str {
    match error.code.as_deref() {
        Some("local_binding_missing") => return "local_binding_missing",
        Some("local_evidence_invalid") => return "local_evidence_invalid",
        Some("malformed_response") => return "provider_malformed",
        _ => {}
    }
    match error.kind {
        ProviderErrorKind::Authentication => "provider_authentication",
        ProviderErrorKind::ResourceMissing => "provider_missing",
        ProviderErrorKind::Retryable => "provider_retryable",
        ProviderErrorKind::Unknown => "provider_unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_failures_are_reduced_to_fixed_redacted_codes() {
        let error = ProviderError {
            status: Some(401),
            code: Some("secret provider message".into()),
            kind: ProviderErrorKind::Authentication,
        };
        assert_eq!(provider_error_code(&error), "provider_authentication");

        let malformed = ProviderError {
            status: Some(500),
            code: Some("malformed_response".into()),
            kind: ProviderErrorKind::Unknown,
        };
        assert_eq!(provider_error_code(&malformed), "provider_malformed");

        let local = ProviderError {
            status: None,
            code: Some("local_binding_missing".into()),
            kind: ProviderErrorKind::Unknown,
        };
        assert_eq!(provider_error_code(&local), "local_binding_missing");
    }
}
