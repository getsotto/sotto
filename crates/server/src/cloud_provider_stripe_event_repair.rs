//! Durable state for bounded account-level Stripe Events API repair scans.
//!
//! This module records fixed scan windows and redacted event dispositions. It does not decide
//! whether an event changes billing or coverage; callers must supply a sealed, validated event
//! outcome and commit it before moving the resumable cursor.

use std::future::Future;

use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use thiserror::Error;
use uuid::Uuid;

use crate::cloud_provider::ProviderEnvironment;
use crate::cloud_provider_stripe_http::{StripeReadClient, StripeReadError};

const EVENT_HISTORY_SECONDS: i64 = 30 * 24 * 60 * 60;
const WINDOW_SECONDS: i64 = 24 * 60 * 60;
const SETTLEMENT_LAG_SECONDS: i64 = 3 * 24 * 60 * 60;
const LEASE_SECONDS: i64 = 240;
const EVENT_PAGE_SIZE: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeEventRepairWindow {
    account_id: String,
    environment: ProviderEnvironment,
    start: i64,
    end: i64,
    cursor: Option<String>,
    lease_owner: String,
}

impl StripeEventRepairWindow {
    pub const fn start(&self) -> i64 {
        self.start
    }

    pub const fn end(&self) -> i64 {
        self.end
    }

    pub fn cursor(&self) -> Option<&str> {
        self.cursor.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StripeEventRepairDisposition {
    Applied,
    AlreadyApplied,
    Unrelated,
    Rejected(&'static str),
    Unresolved(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StripeEventRepairPayloadProvenance {
    Retrieved {
        api_version: String,
        payload_hash: String,
    },
    DetailUnavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeEventRepairReceipt {
    pub event_id: String,
    pub event_type: String,
    pub created_at: i64,
    pub retrieved_at: i64,
    pub payload: StripeEventRepairPayloadProvenance,
}

#[derive(Debug, Error)]
pub enum StripeEventRepairError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("event repair window is invalid")]
    InvalidWindow,
    #[error("event repair lease is no longer owned")]
    LeaseLost,
    #[error("event repair event disposition conflicts with its stored receipt")]
    ReceiptConflict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[doc(hidden)]
pub enum StripeEventRepairRunOutcome {
    Idle,
    PageCompleted {
        events: usize,
        applied: usize,
        already_applied: usize,
        unrelated: usize,
        rejected: usize,
        unresolved: usize,
        window_completed: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeEventRepairStatus {
    pub scan_pending: bool,
    pub completed_through: Option<i64>,
    pub history_gap: Option<(i64, i64)>,
    pub last_outcome: Option<String>,
    pub last_error_code: Option<String>,
    pub unresolved_count: i64,
    pub rejected_count: i64,
    pub unsupported_count: i64,
    pub pending_scope_count: i64,
    pub oldest_unresolved_created: Option<i64>,
}

#[derive(Debug, Error)]
#[doc(hidden)]
pub enum StripeEventRepairRunError {
    #[error("event repair persistence failed: {0}")]
    Store(#[from] StripeEventRepairError),
    #[error("event repair provider read failed: {0}")]
    Provider(#[from] StripeReadError),
    #[error("event repair application failed: {0}")]
    Processing(&'static str),
}

/// Claim the current fixed one-day scan window, or start the next one.
///
/// First use starts at Stripe's oldest retrievable boundary and stores the older unknown range as
/// a visible history gap. If an unfinished window ages beyond that boundary, its unscanned prefix
/// is recorded as a gap before the window is moved forward; it is never reported as repaired.
pub async fn claim_window(
    pool: &PgPool,
    account_id: &str,
    environment: ProviderEnvironment,
    now_epoch: i64,
) -> Result<Option<StripeEventRepairWindow>, StripeEventRepairError> {
    if account_id.trim().is_empty() || now_epoch < 0 {
        return Err(StripeEventRepairError::InvalidWindow);
    }
    let history_floor = now_epoch.saturating_sub(EVENT_HISTORY_SECONDS).max(0);
    let scan_ceiling = now_epoch.saturating_sub(SETTLEMENT_LAG_SECONDS).max(0);
    let lease_owner = format!("stripe-event-repair:{}", Uuid::new_v4());
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO stripe_event_reconciliation_state \
         (provider_account_id, provider_environment) VALUES ($1, $2) \
         ON CONFLICT (provider_account_id, provider_environment) DO NOTHING",
    )
    .bind(account_id)
    .bind(environment.as_str())
    .execute(&mut *tx)
    .await?;
    let row = sqlx::query(
        "SELECT completed_through, window_start, window_end, starting_after, \
                COALESCE(lease_expires_at > now(), FALSE) AS lease_active \
         FROM stripe_event_reconciliation_state \
         WHERE provider_account_id = $1 AND provider_environment = $2 FOR UPDATE",
    )
    .bind(account_id)
    .bind(environment.as_str())
    .fetch_one(&mut *tx)
    .await?;
    if row.try_get::<bool, _>("lease_active")? {
        tx.commit().await?;
        return Ok(None);
    }

    let completed_through: Option<i64> = row.try_get("completed_through")?;
    let mut start: Option<i64> = row.try_get("window_start")?;
    let mut end: Option<i64> = row.try_get("window_end")?;
    let mut cursor: Option<String> = row.try_get("starting_after")?;
    let scan_start = completed_through
        .unwrap_or(history_floor)
        .max(history_floor);
    let mut gap = None;
    if completed_through.is_none() && history_floor > 0 {
        gap = Some((0, history_floor));
    } else if completed_through.is_some_and(|completed| completed < history_floor) {
        gap = Some((completed_through.unwrap_or_default(), history_floor));
    }

    if let Some(window_start) = start {
        if window_start < history_floor {
            gap = Some((window_start, history_floor));
            start = Some(history_floor);
            end = Some(end.unwrap_or(history_floor).max(history_floor));
            cursor = None;
        }
    } else {
        start = Some(scan_start);
        end = Some(scan_start.saturating_add(WINDOW_SECONDS).min(scan_ceiling));
        cursor = None;
        if end == start && start == Some(scan_ceiling) {
            tx.commit().await?;
            return Ok(None);
        }
    }
    let start = start.ok_or(StripeEventRepairError::InvalidWindow)?;
    let mut end = end.ok_or(StripeEventRepairError::InvalidWindow)?;
    if end > scan_ceiling {
        end = scan_ceiling;
    }
    if end < start {
        return Err(StripeEventRepairError::InvalidWindow);
    }
    if start >= scan_ceiling {
        tx.commit().await?;
        return Ok(None);
    }
    if end == start {
        end = start.saturating_add(WINDOW_SECONDS).min(scan_ceiling);
    }
    if let Some((gap_start, gap_end)) = gap {
        sqlx::query(
            "UPDATE stripe_event_reconciliation_state SET \
             history_gap_start = LEAST(COALESCE(history_gap_start, $3), $3), \
             history_gap_end = GREATEST(COALESCE(history_gap_end, $4), $4) \
             WHERE provider_account_id = $1 AND provider_environment = $2",
        )
        .bind(account_id)
        .bind(environment.as_str())
        .bind(gap_start.min(gap_end))
        .bind(gap_end)
        .execute(&mut *tx)
        .await?;
    }
    let lease = sqlx::query(
        "UPDATE stripe_event_reconciliation_state SET \
         window_start = $3, window_end = $4, starting_after = $5, \
         lease_owner = $6, lease_expires_at = now() + ($7 * interval '1 second'), \
         last_attempt_at = now(), updated_at = now() \
         WHERE provider_account_id = $1 AND provider_environment = $2 \
         RETURNING window_start, window_end, starting_after",
    )
    .bind(account_id)
    .bind(environment.as_str())
    .bind(start)
    .bind(end)
    .bind(cursor.take())
    .bind(&lease_owner)
    .bind(LEASE_SECONDS)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Some(StripeEventRepairWindow {
        account_id: account_id.to_owned(),
        environment,
        start: lease.try_get("window_start")?,
        end: lease.try_get("window_end")?,
        cursor: lease.try_get("starting_after")?,
        lease_owner,
    }))
}

/// Extend the active lease before handing a retrieved event to a potentially slow repair path.
pub async fn renew_window_lease(
    pool: &PgPool,
    window: &StripeEventRepairWindow,
) -> Result<(), StripeEventRepairError> {
    let updated = sqlx::query(
        "UPDATE stripe_event_reconciliation_state SET \
         lease_expires_at = now() + ($5 * interval '1 second'), updated_at = now() \
         WHERE provider_account_id = $1 AND provider_environment = $2 \
           AND window_start = $3 AND window_end = $4 AND lease_owner = $6 \
           AND lease_expires_at > now()",
    )
    .bind(&window.account_id)
    .bind(window.environment.as_str())
    .bind(window.start)
    .bind(window.end)
    .bind(LEASE_SECONDS)
    .bind(&window.lease_owner)
    .execute(pool)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(StripeEventRepairError::LeaseLost);
    }
    Ok(())
}

/// Persist a candidate's immutable API-read provenance and move the cursor after its disposition.
pub async fn record_event_disposition(
    pool: &PgPool,
    window: &StripeEventRepairWindow,
    receipt: &StripeEventRepairReceipt,
    disposition: StripeEventRepairDisposition,
) -> Result<(), StripeEventRepairError> {
    let payload_provenance_valid = match &receipt.payload {
        StripeEventRepairPayloadProvenance::Retrieved {
            api_version,
            payload_hash,
        } => {
            !api_version.trim().is_empty()
                && payload_hash.len() == 64
                && payload_hash
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        }
        StripeEventRepairPayloadProvenance::DetailUnavailable => {
            matches!(&disposition, StripeEventRepairDisposition::Unresolved(_))
        }
    };
    if receipt.event_id.trim().is_empty()
        || receipt.created_at < window.start
        || receipt.created_at >= window.end
        || !payload_provenance_valid
        || receipt.retrieved_at < 0
        || !matches!(
            receipt.event_type.as_str(),
            "invoice.paid" | "invoice.payment_failed"
        )
    {
        return Err(StripeEventRepairError::InvalidWindow);
    }
    let (disposition, reason) = match disposition {
        StripeEventRepairDisposition::Applied => ("applied", None),
        StripeEventRepairDisposition::AlreadyApplied => ("already_applied", None),
        StripeEventRepairDisposition::Unrelated => ("unrelated", None),
        StripeEventRepairDisposition::Rejected(code) => ("rejected", Some(code)),
        StripeEventRepairDisposition::Unresolved(code) => ("unresolved", Some(code)),
    };
    let (retrieval_status, api_version, payload_hash) = match &receipt.payload {
        StripeEventRepairPayloadProvenance::Retrieved {
            api_version,
            payload_hash,
        } => (
            "retrieved",
            Some(api_version.as_str()),
            Some(payload_hash.as_str()),
        ),
        StripeEventRepairPayloadProvenance::DetailUnavailable => ("detail_unavailable", None, None),
    };
    let mut tx = pool.begin().await?;
    let insert = sqlx::query(
        "INSERT INTO stripe_event_repair_receipts \
         (provider_account_id, provider_environment, event_id, event_type, provider_created_at, \
          retrieval_status, api_version, payload_hash, retrieved_at, disposition, reason_code) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) \
         ON CONFLICT (provider_account_id, provider_environment, event_id) DO NOTHING",
    )
    .bind(&window.account_id)
    .bind(window.environment.as_str())
    .bind(&receipt.event_id)
    .bind(&receipt.event_type)
    .bind(receipt.created_at)
    .bind(retrieval_status)
    .bind(api_version)
    .bind(payload_hash)
    .bind(receipt.retrieved_at)
    .bind(disposition)
    .bind(reason)
    .execute(&mut *tx)
    .await?;
    if insert.rows_affected() == 0 {
        let same: bool = sqlx::query_scalar(
            "SELECT event_type = $4 AND provider_created_at = $5 AND retrieval_status = $6 \
                AND api_version IS NOT DISTINCT FROM $7 \
                AND payload_hash IS NOT DISTINCT FROM $8 \
                AND disposition = $9 \
                AND reason_code IS NOT DISTINCT FROM $10 \
             FROM stripe_event_repair_receipts \
             WHERE provider_account_id = $1 AND provider_environment = $2 AND event_id = $3",
        )
        .bind(&window.account_id)
        .bind(window.environment.as_str())
        .bind(&receipt.event_id)
        .bind(&receipt.event_type)
        .bind(receipt.created_at)
        .bind(retrieval_status)
        .bind(api_version)
        .bind(payload_hash)
        .bind(disposition)
        .bind(reason)
        .fetch_one(&mut *tx)
        .await?;
        if !same {
            return Err(StripeEventRepairError::ReceiptConflict);
        }
    }
    if matches!(disposition, "rejected" | "unresolved") {
        sqlx::query(
            "UPDATE stripe_event_reconciliation_state SET \
             oldest_unresolved_created = LEAST( \
                 COALESCE(oldest_unresolved_created, $3), $3 \
             ), updated_at = now() \
             WHERE provider_account_id = $1 AND provider_environment = $2",
        )
        .bind(&window.account_id)
        .bind(window.environment.as_str())
        .bind(receipt.created_at)
        .execute(&mut *tx)
        .await?;
    }
    let updated = sqlx::query(
        "UPDATE stripe_event_reconciliation_state SET starting_after = $5, updated_at = now() \
         WHERE provider_account_id = $1 AND provider_environment = $2 \
           AND window_start = $3 AND window_end = $4 AND lease_owner = $6 \
           AND lease_expires_at > now()",
    )
    .bind(&window.account_id)
    .bind(window.environment.as_str())
    .bind(window.start)
    .bind(window.end)
    .bind(&receipt.event_id)
    .bind(&window.lease_owner)
    .execute(&mut *tx)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(StripeEventRepairError::LeaseLost);
    }
    tx.commit().await?;
    Ok(())
}

/// Finish a scanned page and either release the lease or atomically advance the completed window.
pub async fn finish_page(
    pool: &PgPool,
    window: &StripeEventRepairWindow,
    has_more: bool,
) -> Result<(), StripeEventRepairError> {
    let mut tx = pool.begin().await?;
    let result = if has_more {
        sqlx::query(
            "UPDATE stripe_event_reconciliation_state SET lease_owner = NULL, \
             lease_expires_at = NULL, updated_at = now() \
             WHERE provider_account_id = $1 AND provider_environment = $2 \
               AND window_start = $3 AND window_end = $4 AND lease_owner = $5 \
               AND lease_expires_at > now()",
        )
        .bind(&window.account_id)
        .bind(window.environment.as_str())
        .bind(window.start)
        .bind(window.end)
        .bind(&window.lease_owner)
        .execute(&mut *tx)
        .await?
    } else {
        sqlx::query(
            "UPDATE stripe_event_reconciliation_state SET completed_through = window_end, \
             window_start = NULL, window_end = NULL, starting_after = NULL, \
             lease_owner = NULL, lease_expires_at = NULL, last_outcome = 'complete', \
             last_error_code = NULL, updated_at = now() \
             WHERE provider_account_id = $1 AND provider_environment = $2 \
               AND window_start = $3 AND window_end = $4 AND lease_owner = $5 \
               AND lease_expires_at > now()",
        )
        .bind(&window.account_id)
        .bind(window.environment.as_str())
        .bind(window.start)
        .bind(window.end)
        .bind(&window.lease_owner)
        .execute(&mut *tx)
        .await?
    };
    if result.rows_affected() != 1 {
        return Err(StripeEventRepairError::LeaseLost);
    }
    tx.commit().await?;
    Ok(())
}

pub async fn fail_window(
    pool: &PgPool,
    window: &StripeEventRepairWindow,
    code: &'static str,
    outcome: &'static str,
) -> Result<(), StripeEventRepairError> {
    if !matches!(
        outcome,
        "retryable_failure" | "history_gap" | "bound_exceeded" | "unresolved"
    ) {
        return Err(StripeEventRepairError::InvalidWindow);
    }
    let result = sqlx::query(
        "UPDATE stripe_event_reconciliation_state SET lease_owner = NULL, \
         lease_expires_at = NULL, last_outcome = $6, last_error_code = $7, \
         oldest_unresolved_created = CASE WHEN $6 IN ('unresolved', 'bound_exceeded') \
             THEN LEAST(COALESCE(oldest_unresolved_created, $3), $3) \
             ELSE oldest_unresolved_created END, updated_at = now() \
         WHERE provider_account_id = $1 AND provider_environment = $2 \
           AND window_start = $3 AND window_end = $4 AND lease_owner = $5",
    )
    .bind(&window.account_id)
    .bind(window.environment.as_str())
    .bind(window.start)
    .bind(window.end)
    .bind(&window.lease_owner)
    .bind(outcome)
    .bind(code)
    .execute(pool)
    .await?;
    if result.rows_affected() != 1 {
        return Err(StripeEventRepairError::LeaseLost);
    }
    Ok(())
}

/// Read aggregate repair health without returning provider or customer identifiers.
pub async fn status_snapshot(
    pool: &PgPool,
    account_id: &str,
    environment: ProviderEnvironment,
) -> Result<Option<StripeEventRepairStatus>, StripeEventRepairError> {
    let row = sqlx::query(
        "SELECT state.window_start IS NOT NULL AS scan_pending, state.completed_through, \
                state.history_gap_start, state.history_gap_end, state.last_outcome, \
                state.oldest_unresolved_created, state.last_error_code, \
                (SELECT count(*) FROM stripe_event_repair_receipts receipt \
                 WHERE receipt.provider_account_id = state.provider_account_id \
                   AND receipt.provider_environment = state.provider_environment \
                   AND receipt.disposition = 'unresolved') AS unresolved_count, \
                (SELECT count(*) FROM stripe_event_repair_receipts receipt \
                 WHERE receipt.provider_account_id = state.provider_account_id \
                   AND receipt.provider_environment = state.provider_environment \
                   AND receipt.disposition = 'rejected') AS rejected_count, \
                (SELECT count(*) FROM stripe_event_repair_receipts receipt \
                 WHERE receipt.provider_account_id = state.provider_account_id \
                   AND receipt.provider_environment = state.provider_environment \
                   AND receipt.reason_code LIKE '%unsupported%') AS unsupported_count, \
                (SELECT count(*) FROM cloud_provider_event_scopes scope \
                 WHERE scope.provider_namespace = 'stripe' \
                   AND scope.provider_account_id = state.provider_account_id \
                   AND scope.provider_environment = state.provider_environment \
                   AND scope.status = 'pending') AS pending_scope_count \
         FROM stripe_event_reconciliation_state state \
         WHERE state.provider_account_id = $1 AND state.provider_environment = $2",
    )
    .bind(account_id)
    .bind(environment.as_str())
    .fetch_optional(pool)
    .await?;
    row.map(|row| {
        let gap_start: Option<i64> = row.try_get("history_gap_start")?;
        let gap_end: Option<i64> = row.try_get("history_gap_end")?;
        Ok(StripeEventRepairStatus {
            scan_pending: row.try_get("scan_pending")?,
            completed_through: row.try_get("completed_through")?,
            history_gap: gap_start.zip(gap_end),
            last_outcome: row.try_get("last_outcome")?,
            last_error_code: row.try_get("last_error_code")?,
            unresolved_count: row.try_get("unresolved_count")?,
            rejected_count: row.try_get("rejected_count")?,
            unsupported_count: row.try_get("unsupported_count")?,
            pending_scope_count: row.try_get("pending_scope_count")?,
            oldest_unresolved_created: row.try_get("oldest_unresolved_created")?,
        })
    })
    .transpose()
}

/// Process at most one bounded page. The caller classifies each authenticated event; a callback
/// error leaves that event's cursor unadvanced so the next lease retries it.
#[doc(hidden)]
pub async fn run_once<F, Fut>(
    pool: &PgPool,
    stripe: &StripeReadClient,
    account_id: &str,
    environment: ProviderEnvironment,
    now_epoch: i64,
    mut apply: F,
) -> Result<StripeEventRepairRunOutcome, StripeEventRepairRunError>
where
    F: FnMut(crate::cloud_provider_stripe_http::AuthenticatedStripeEvent, i64) -> Fut,
    Fut: Future<Output = Result<StripeEventRepairDisposition, StripeEventRepairRunError>>,
{
    let Some(window) = claim_window(pool, account_id, environment, now_epoch).await? else {
        return Ok(StripeEventRepairRunOutcome::Idle);
    };
    let mut session = stripe.session();
    let page = match stripe
        .invoice_events_page(
            &mut session,
            window.start,
            window.end,
            window.cursor.as_deref(),
            EVENT_PAGE_SIZE,
        )
        .await
    {
        Ok(page) => page,
        Err(error) => {
            let code = stripe_error_code(&error);
            fail_window(pool, &window, code, stripe_error_outcome(code)).await?;
            return Err(error.into());
        }
    };
    let mut applied = 0;
    let mut already_applied = 0;
    let mut unrelated = 0;
    let mut rejected = 0;
    let mut unresolved = 0;
    for summary in &page.events {
        let event = match stripe.event_payload(&mut session, &summary.id).await {
            Ok(event) => event,
            Err(error) => {
                if let Some(reason) = event_detail_unavailable_reason(&error) {
                    let disposition = StripeEventRepairDisposition::Unresolved(reason);
                    unresolved += 1;
                    if let Err(error) = record_event_disposition(
                        pool,
                        &window,
                        &StripeEventRepairReceipt {
                            event_id: summary.id.clone(),
                            event_type: summary.event_type.clone(),
                            created_at: summary.created,
                            retrieved_at: epoch_now(),
                            payload: StripeEventRepairPayloadProvenance::DetailUnavailable,
                        },
                        disposition,
                    )
                    .await
                    {
                        let _ = fail_window(
                            pool,
                            &window,
                            "event_receipt_persistence",
                            "retryable_failure",
                        )
                        .await;
                        return Err(error.into());
                    }
                    continue;
                }
                let code = stripe_error_code(&error);
                fail_window(pool, &window, code, stripe_error_outcome(code)).await?;
                return Err(error.into());
            }
        };
        let payload = event.payload();
        let api_version = payload
            .get("api_version")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("missing")
            .to_owned();
        let event_type = payload
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_owned();
        let created = payload
            .get("created")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(summary.created);
        let bytes =
            serde_json::to_vec(payload).map_err(|_| StripeEventRepairError::InvalidWindow)?;
        let payload_hash = hex_lower(&Sha256::digest(bytes));
        let retrieved_at = epoch_now();
        let disposition = if event_type != summary.event_type || created != summary.created {
            StripeEventRepairDisposition::Unresolved("event_list_detail_mismatch")
        } else {
            if let Err(error) = renew_window_lease(pool, &window).await {
                let _ = fail_window(
                    pool,
                    &window,
                    "event_application_lease_renewal",
                    "retryable_failure",
                )
                .await;
                return Err(error.into());
            }
            match apply(event, retrieved_at).await {
                Ok(disposition) => disposition,
                Err(error) => {
                    let _ = fail_window(
                        pool,
                        &window,
                        "event_application_retryable",
                        "retryable_failure",
                    )
                    .await;
                    return Err(error);
                }
            }
        };
        match &disposition {
            StripeEventRepairDisposition::Applied => applied += 1,
            StripeEventRepairDisposition::AlreadyApplied => already_applied += 1,
            StripeEventRepairDisposition::Unrelated => unrelated += 1,
            StripeEventRepairDisposition::Rejected(_) => rejected += 1,
            StripeEventRepairDisposition::Unresolved(_) => unresolved += 1,
        }
        if let Err(error) = record_event_disposition(
            pool,
            &window,
            &StripeEventRepairReceipt {
                event_id: summary.id.clone(),
                event_type: summary.event_type.clone(),
                created_at: summary.created,
                retrieved_at,
                payload: StripeEventRepairPayloadProvenance::Retrieved {
                    api_version,
                    payload_hash,
                },
            },
            disposition,
        )
        .await
        {
            let _ = fail_window(
                pool,
                &window,
                "event_receipt_persistence",
                "retryable_failure",
            )
            .await;
            return Err(error.into());
        }
    }
    finish_page(pool, &window, page.has_more).await?;
    Ok(StripeEventRepairRunOutcome::PageCompleted {
        events: page.events.len(),
        applied,
        already_applied,
        unrelated,
        rejected,
        unresolved,
        window_completed: !page.has_more,
    })
}

fn stripe_error_code(error: &StripeReadError) -> &'static str {
    match error {
        StripeReadError::Authentication { .. } => "stripe_authentication",
        StripeReadError::Permission { .. } => "stripe_permission",
        StripeReadError::ContextMismatch | StripeReadError::AccountMismatch => "stripe_context",
        StripeReadError::PageBoundExceeded
        | StripeReadError::RequestBoundExceeded
        | StripeReadError::RecordBoundExceeded
        | StripeReadError::ResponseTooLarge
        | StripeReadError::SessionBytesExceeded => "stripe_bound_exceeded",
        _ if error.is_retryable() => "stripe_read_retryable",
        _ => "stripe_read_invalid",
    }
}

fn stripe_error_outcome(code: &str) -> &'static str {
    match code {
        "stripe_bound_exceeded" => "bound_exceeded",
        "stripe_read_invalid" => "unresolved",
        _ => "retryable_failure",
    }
}

fn event_detail_unavailable_reason(error: &StripeReadError) -> Option<&'static str> {
    match error {
        StripeReadError::MalformedResponse(_) => Some("event_detail_malformed"),
        StripeReadError::ResourceMissing => Some("event_detail_missing"),
        StripeReadError::ContextMismatch | StripeReadError::AccountMismatch => {
            Some("event_detail_context_mismatch")
        }
        _ => None,
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn epoch_now() -> i64 {
    crate::clock::business_epoch_seconds()
}

#[cfg(test)]
mod tests {
    use super::{event_detail_unavailable_reason, stripe_error_outcome};
    use crate::cloud_provider_stripe_http::StripeReadError;

    #[test]
    fn scan_failures_keep_bounds_and_invalid_shapes_distinct() {
        assert_eq!(
            stripe_error_outcome("stripe_bound_exceeded"),
            "bound_exceeded"
        );
        assert_eq!(stripe_error_outcome("stripe_read_invalid"), "unresolved");
        assert_eq!(
            stripe_error_outcome("stripe_read_retryable"),
            "retryable_failure"
        );
    }

    #[test]
    fn missing_event_details_are_durable_unresolved_candidates() {
        assert_eq!(
            event_detail_unavailable_reason(&StripeReadError::ResourceMissing),
            Some("event_detail_missing")
        );
    }
}
