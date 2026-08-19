//! Internal organisation-deletion lifecycle and worker operations.
//!
//! The production route is deliberately absent for now. This seam owns the durable state machine,
//! leases, compare-and-set transitions, provider reconciliation, and final purge so the staged
//! HTTP adapter cannot duplicate safety rules.

use std::time::Duration;

use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::audit;
use crate::billing::{
    ProviderErrorKind, PurgeGate, SubscriptionObservation, SubscriptionProvider, SubscriptionStatus,
};
use crate::error::{Error, Result};
use crate::org::{self, LifecycleState, Role};

/// Keep corrupted lifecycle data on the internal-error path rather than treating it as client
/// configuration or input.
fn lifecycle_error(message: impl Into<String>) -> Error {
    Error::Internal(message.into())
}

/// The persisted phases of one deletion attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeletionState {
    Requested,
    CancellingBilling,
    Retention,
    Purging,
    Recovering,
    Failed,
    Cancelled,
    Completed,
}

impl DeletionState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::CancellingBilling => "cancelling_billing",
            Self::Retention => "retention",
            Self::Purging => "purging",
            Self::Recovering => "recovering",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Completed => "completed",
        }
    }

    pub(crate) fn from_db(value: &str) -> Result<Self> {
        match value {
            "requested" => Ok(Self::Requested),
            "cancelling_billing" => Ok(Self::CancellingBilling),
            "retention" => Ok(Self::Retention),
            "purging" => Ok(Self::Purging),
            "recovering" => Ok(Self::Recovering),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            "completed" => Ok(Self::Completed),
            // Never let an unknown persisted value become claimable work.
            other => Err(lifecycle_error(format!(
                "unknown organisation deletion state in db: {other}"
            ))),
        }
    }

    fn can_cancel(self) -> bool {
        matches!(
            self,
            Self::Requested | Self::CancellingBilling | Self::Retention | Self::Failed
        )
    }

    #[cfg(test)]
    fn is_terminal(self) -> bool {
        matches!(self, Self::Cancelled | Self::Completed)
    }
}

/// A small view shared by future handlers and worker callers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeletionView {
    pub id: String,
    pub org_id: String,
    pub state: DeletionState,
}

/// The owner-visible status of one deletion operation. Provider details remain private to this
/// module; [`DeletionStatus::public_error`] exposes only the documented error vocabulary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeletionStatus {
    pub id: String,
    pub org_id: String,
    pub state: DeletionState,
    pub requested_at: String,
    pub recoverable_until: String,
    pub managed_backup_expiry_by: Option<String>,
    pub next_retry_at: Option<String>,
    last_error_code: Option<String>,
}

impl DeletionStatus {
    pub(crate) fn public_error(&self) -> Option<&'static str> {
        let code = self.last_error_code.as_deref()?;
        // A completed phase must not keep displaying a transient provider failure recorded by an
        // earlier retry. Failed operations and scheduled retries remain visible to their owner.
        if self.state != DeletionState::Failed && self.next_retry_at.is_none() {
            return None;
        }
        match code {
            "purge_precondition_failed" => Some("purge_failed"),
            "billing_status_unknown" => Some("billing_unknown"),
            // Unknown internal/provider codes collapse to the least specific public billing error
            // so adding a new diagnostic can never expose it through the owner-facing contract.
            _ => Some("billing_unavailable"),
        }
    }
}

/// Work claimed by one worker.  `state_version` is the compare-and-set token for every result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeletionLease {
    pub id: String,
    pub org_id: String,
    pub state: DeletionState,
    pub subscription_id: Option<String>,
    pub worker_id: String,
    pub state_version: i64,
    pub attempt_count: i32,
}

/// Retry delays from the lifecycle design, in order.  `attempt_count` is the count after a lease
/// was claimed, so the first failed attempt receives the one-minute delay.
const RETRY_DELAYS: [Duration; 6] = [
    Duration::from_secs(60),
    Duration::from_secs(5 * 60),
    Duration::from_secs(30 * 60),
    Duration::from_secs(2 * 60 * 60),
    Duration::from_secs(6 * 60 * 60),
    Duration::from_secs(24 * 60 * 60),
];

/// A shared fifteen-minute window keeps both checks from authorising a destructive purge on stale
/// billing state.
const BILLING_OBSERVATION_MAX_AGE: Duration = Duration::from_secs(15 * 60);

pub(crate) fn retry_delay(attempt_count: i32) -> Option<Duration> {
    if attempt_count <= 0 {
        return Some(RETRY_DELAYS[0]);
    }
    RETRY_DELAYS.get(attempt_count as usize - 1).copied()
}

/// Add a small operation-specific jitter without introducing a random source into the state
/// machine. The stable offset prevents a fleet of workers from retrying every operation together,
/// while deterministic tests can still assert the base ladder.
fn retry_delay_with_jitter(operation_id: &str, attempt_count: i32) -> Option<Duration> {
    let base = retry_delay(attempt_count)?;
    // FNV-1a's fixed prime gives stable per-operation jitter without storing another random value.
    let hash = operation_id.bytes().fold(0_u64, |hash, byte| {
        hash.wrapping_mul(1_099_511_628_211)
            .wrapping_add(byte as u64)
    });
    Some(base + Duration::from_secs(hash % 30))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RetryDecision {
    Retry(Duration),
    Failed,
}

pub(crate) fn provider_retry_decision(
    kind: ProviderErrorKind,
    attempt_count: i32,
) -> RetryDecision {
    if matches!(kind, ProviderErrorKind::Authentication) {
        return RetryDecision::Failed;
    }
    retry_delay(attempt_count).map_or(RetryDecision::Failed, RetryDecision::Retry)
}

pub(crate) fn billing_transition(gate: PurgeGate) -> Option<DeletionState> {
    match gate {
        PurgeGate::Terminal | PurgeGate::Missing => Some(DeletionState::Retention),
        PurgeGate::Blocking | PurgeGate::Unknown => None,
    }
}

/// Accept one confirmed owner request, or return the existing active attempt.
pub async fn request(
    pool: &PgPool,
    org_id: &str,
    actor: &str,
    confirmation_org_id: &str,
) -> Result<DeletionView> {
    if confirmation_org_id != org_id {
        return Err(Error::BadRequest(
            "deletion confirmation does not match the organisation".into(),
        ));
    }

    let mut tx = pool.begin().await?;
    let row: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT lifecycle_state, stripe_subscription_id FROM organizations \
         WHERE id = $1 FOR UPDATE",
    )
    .bind(org_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((lifecycle, subscription_id)) = row else {
        return Err(Error::NotFound("organisation not found".into()));
    };
    let lifecycle = LifecycleState::from_db(&lifecycle)?;
    if lifecycle == LifecycleState::Deleted {
        // The requesting owner may already inspect this terminal operation after memberships are
        // purged, so tell only that actor that the id is permanently unavailable. Everyone else
        // receives the same not-found response as an unknown organisation.
        let requested_by_actor: Option<i32> = sqlx::query_scalar(
            "SELECT 1 FROM organization_deletions \
             WHERE org_id = $1 AND requested_by = $2 AND state = 'completed' LIMIT 1",
        )
        .bind(org_id)
        .bind(actor)
        .fetch_optional(&mut *tx)
        .await?;
        return if requested_by_actor.is_some() {
            Err(Error::Conflict(
                "organisation has already been deleted".into(),
            ))
        } else {
            Err(Error::NotFound("organisation not found".into()))
        };
    }
    require_owner(&mut tx, org_id, actor).await?;

    if lifecycle == LifecycleState::Deleting {
        let existing: Option<(String, String, Option<String>)> = sqlx::query_as(
            "SELECT id::text, state, resume_state FROM organization_deletions \
             WHERE org_id = $1 AND state NOT IN ('cancelled', 'completed') \
             ORDER BY requested_at DESC LIMIT 1 FOR UPDATE",
        )
        .bind(org_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((id, state, resume_state)) = existing else {
            return Err(lifecycle_error(
                "deleting organisation has no active deletion operation",
            ));
        };
        let state = DeletionState::from_db(&state)?;
        if state == DeletionState::Failed {
            let resume_state = resume_state
                .ok_or_else(|| lifecycle_error("failed deletion has no recorded resume state"))?;
            DeletionState::from_db(&resume_state)?;
            sqlx::query(
                "UPDATE organization_deletions \
                 SET state = resume_state, resume_state = NULL, next_attempt_at = now(), \
                     attempt_count = 0, last_error_code = NULL, lease_owner = NULL, \
                     lease_expires_at = NULL, \
                     state_version = state_version + 1 \
                 WHERE id = $1::uuid",
            )
            .bind(&id)
            .execute(&mut *tx)
            .await?;
            audit::record_tx(
                &mut tx,
                org_id,
                actor,
                "org.deletion.retry_requested",
                audit::Context {
                    target: Some(&id),
                    ..Default::default()
                },
            )
            .await?;
            let state = DeletionState::from_db(&resume_state)?;
            tx.commit().await?;
            return Ok(DeletionView {
                id,
                org_id: org_id.into(),
                state,
            });
        }
        tx.commit().await?;
        return Ok(DeletionView {
            id,
            org_id: org_id.into(),
            state,
        });
    }

    let id = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO organization_deletions \
         (id, org_id, state, requested_by, requested_at, purge_after, subscription_id) \
         VALUES ($1::uuid, $2, 'requested', $3, now(), now() + interval '30 days', $4)",
    )
    .bind(&id)
    .bind(org_id)
    .bind(actor)
    .bind(subscription_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE organizations SET lifecycle_state = 'deleting' WHERE id = $1")
        .bind(org_id)
        .execute(&mut *tx)
        .await?;
    audit::record_tx(
        &mut tx,
        org_id,
        actor,
        "org.deletion.requested",
        audit::Context {
            target: Some(&id),
            ..Default::default()
        },
    )
    .await?;
    tx.commit().await?;
    Ok(DeletionView {
        id,
        org_id: org_id.into(),
        state: DeletionState::Requested,
    })
}

type DeletionStatusRow = (
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// Return an owner's current deletion status without changing its state.
pub async fn status(pool: &PgPool, org_id: &str, actor: &str) -> Result<DeletionStatus> {
    let can_read_active = match org::access(pool, org_id, actor).await {
        Ok(access) if access.role() == Role::Owner => true,
        Ok(_) => {
            return Err(Error::Forbidden(
                "only an organisation owner may inspect deletion".into(),
            ));
        }
        // Memberships are gone after purge, so the requesting owner needs a separate terminal
        // lookup. Never let this fallback expose an active operation to a non-member.
        Err(Error::NotFound(_)) => false,
        Err(error) => return Err(error),
    };
    let row: Option<DeletionStatusRow> = if can_read_active {
        sqlx::query_as(
            // Normalising in PostgreSQL keeps the wire timestamps truthful without adding a second
            // time library solely for formatting database values.
            "SELECT id::text, state, \
                    to_char(requested_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), \
                    to_char(purge_after AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), \
                    to_char(managed_backup_expiry_by AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), \
                    to_char(next_attempt_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), \
                    last_error_code \
             FROM organization_deletions \
             WHERE org_id = $1 AND state NOT IN ('cancelled', 'completed') \
             ORDER BY requested_at DESC LIMIT 1",
        )
        .bind(org_id)
        .fetch_optional(pool)
        .await?
    } else {
        None
    };
    let row = match row {
        Some(row) => Some(row),
        None => {
            sqlx::query_as(
                "SELECT id::text, state, \
                        to_char(requested_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), \
                        to_char(purge_after AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), \
                        to_char(managed_backup_expiry_by AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), \
                        to_char(next_attempt_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), \
                        last_error_code \
                 FROM organization_deletions \
                 WHERE org_id = $1 AND state IN ('cancelled', 'completed') \
                   AND ($3 OR requested_by = $2) \
                 ORDER BY requested_at DESC LIMIT 1",
            )
            .bind(org_id)
            .bind(actor)
            .bind(can_read_active)
            .fetch_optional(pool)
            .await?
        }
    };
    let Some((
        id,
        state,
        requested_at,
        recoverable_until,
        managed_backup_expiry_by,
        next_retry_at,
        last_error_code,
    )) = row
    else {
        return Err(Error::NotFound("deletion not found".into()));
    };
    Ok(DeletionStatus {
        id,
        org_id: org_id.into(),
        state: DeletionState::from_db(&state)?,
        requested_at,
        recoverable_until,
        managed_backup_expiry_by,
        next_retry_at,
        last_error_code,
    })
}

/// Begin recovery for an owner, leaving access frozen until billing has been reconciled.
pub async fn cancel(pool: &PgPool, org_id: &str, actor: &str) -> Result<DeletionView> {
    let mut tx = pool.begin().await?;
    let lifecycle: Option<String> =
        sqlx::query_scalar("SELECT lifecycle_state FROM organizations WHERE id = $1 FOR UPDATE")
            .bind(org_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(lifecycle) = lifecycle else {
        return Err(Error::NotFound("organisation not found".into()));
    };
    if LifecycleState::from_db(&lifecycle)? == LifecycleState::Deleted {
        // Memberships no longer exist on a tombstone. Preserve the post-purge conflict only for
        // the original requester, using the same terminal visibility boundary as status reads.
        let requested_completed: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM organization_deletions \
             WHERE org_id = $1 AND requested_by = $2 AND state = 'completed')",
        )
        .bind(org_id)
        .bind(actor)
        .fetch_one(&mut *tx)
        .await?;
        if requested_completed {
            return Err(Error::Conflict(
                "organisation deletion cannot be cancelled after purge has started".into(),
            ));
        }
        return Err(Error::NotFound("organisation not found".into()));
    }
    require_owner(&mut tx, org_id, actor).await?;
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT id::text, state FROM organization_deletions \
         WHERE org_id = $1 AND state NOT IN ('cancelled', 'completed') \
         ORDER BY requested_at DESC LIMIT 1 FOR UPDATE",
    )
    .bind(org_id)
    .fetch_optional(&mut *tx)
    .await?;
    let row = match row {
        Some(row) => Some(row),
        None => {
            // Recovery may finish between repeated client calls. The organisation is active here,
            // so every current owner may repeat cancellation and inspect its reconciled result.
            sqlx::query_as(
                "SELECT id::text, state FROM organization_deletions \
                 WHERE org_id = $1 AND state = 'cancelled' \
                 ORDER BY requested_at DESC LIMIT 1",
            )
            .bind(org_id)
            .fetch_optional(&mut *tx)
            .await?
        }
    };
    let Some((id, state)) = row else {
        return Err(Error::NotFound("deletion not found".into()));
    };
    let state = DeletionState::from_db(&state)?;
    if !state.can_cancel() {
        if state == DeletionState::Purging {
            return Err(Error::Conflict(
                "organisation deletion cannot be cancelled after purge has started".into(),
            ));
        }
        tx.commit().await?;
        return Ok(DeletionView {
            id,
            org_id: org_id.into(),
            state,
        });
    }
    sqlx::query(
        "UPDATE organization_deletions SET state = 'recovering', resume_state = NULL, \
         attempt_count = 0, next_attempt_at = now(), last_error_code = NULL, \
         lease_owner = NULL, lease_expires_at = NULL, state_version = state_version + 1 \
         WHERE id = $1::uuid",
    )
    .bind(&id)
    .execute(&mut *tx)
    .await?;
    audit::record_tx(
        &mut tx,
        org_id,
        actor,
        "org.deletion.cancel_requested",
        audit::Context {
            target: Some(&id),
            ..Default::default()
        },
    )
    .await?;
    tx.commit().await?;
    Ok(DeletionView {
        id,
        org_id: org_id.into(),
        state: DeletionState::Recovering,
    })
}

/// Claim one due attempt. `SKIP LOCKED` lets multiple server instances share the queue safely.
pub async fn claim_due(pool: &PgPool, worker_id: &str) -> Result<Option<DeletionLease>> {
    if worker_id.is_empty() {
        return Err(Error::Internal("deletion worker id is empty".into()));
    }
    let mut tx = pool.begin().await?;
    // Five-minute leases give another worker a bounded recovery window after a crash.
    // Failed rows stay out of this queue because an owner action or the deferred operator retry
    // command, not an automatic retry, must restore their recorded resume state.
    let candidate: Option<String> = sqlx::query_scalar(
        "SELECT id::text FROM organization_deletions \
         WHERE (lease_expires_at IS NULL OR lease_expires_at <= now()) \
           AND (\
             (state = 'purging' AND purge_after <= now()) \
             OR (state = 'retention' AND purge_after <= now() \
                 AND (next_attempt_at IS NULL OR next_attempt_at <= now())) \
             OR (state IN ('requested', 'cancelling_billing', 'recovering') \
                 AND (next_attempt_at IS NULL OR next_attempt_at <= now())) \
           ) \
         ORDER BY CASE WHEN state = 'retention' THEN purge_after \
                       ELSE COALESCE(next_attempt_at, requested_at) END, id \
         LIMIT 1 FOR UPDATE SKIP LOCKED",
    )
    .fetch_optional(&mut *tx)
    .await?;
    let Some(id) = candidate else {
        tx.commit().await?;
        return Ok(None);
    };
    let row: (String, String, Option<String>, i64, i32) = sqlx::query_as(
        "UPDATE organization_deletions \
             SET lease_owner = $1, lease_expires_at = now() + interval '5 minutes', \
             state_version = state_version + 1, \
             attempt_count = attempt_count + CASE \
                 WHEN state IN ('cancelling_billing', 'recovering') \
                      OR (state = 'retention' AND subscription_id IS NOT NULL) \
                 THEN 1 ELSE 0 END \
         WHERE id = $2::uuid \
         RETURNING org_id, state, subscription_id, state_version, attempt_count",
    )
    .bind(worker_id)
    .bind(&id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Some(DeletionLease {
        id,
        org_id: row.0,
        state: DeletionState::from_db(&row.1)?,
        subscription_id: row.2,
        worker_id: worker_id.into(),
        state_version: row.3,
        attempt_count: row.4,
    }))
}

/// Advance one leased attempt. Provider calls happen before opening the transition transaction so
/// a slow upstream cannot hold an organisation row lock. A stale lease returns `None` harmlessly.
pub async fn advance(
    pool: &PgPool,
    lease: &DeletionLease,
    provider: Option<&dyn SubscriptionProvider>,
) -> Result<Option<DeletionView>> {
    match lease.state {
        DeletionState::Requested => {
            transition_state(pool, lease, DeletionState::CancellingBilling).await
        }
        DeletionState::CancellingBilling => advance_billing(pool, lease, provider, false).await,
        DeletionState::Recovering => advance_billing(pool, lease, provider, true).await,
        DeletionState::Retention => advance_retention(pool, lease, provider).await,
        DeletionState::Purging => purge(pool, lease).await,
        DeletionState::Failed | DeletionState::Cancelled | DeletionState::Completed => Ok(None),
    }
}

async fn transition_state(
    pool: &PgPool,
    lease: &DeletionLease,
    next: DeletionState,
) -> Result<Option<DeletionView>> {
    let mut tx = pool.begin().await?;
    let row: Option<(String, String)> = sqlx::query_as(
        "UPDATE organization_deletions SET state = $1, next_attempt_at = now(), \
         lease_owner = NULL, lease_expires_at = NULL, state_version = state_version + 1 \
         WHERE id = $2::uuid AND state_version = $3 AND lease_owner = $4 \
         RETURNING org_id, state",
    )
    .bind(next.as_str())
    .bind(&lease.id)
    .bind(lease.state_version)
    .bind(&lease.worker_id)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    row.map(|row| view_from_row(&lease.id, row)).transpose()
}

async fn advance_billing(
    pool: &PgPool,
    lease: &DeletionLease,
    provider: Option<&dyn SubscriptionProvider>,
    recovering: bool,
) -> Result<Option<DeletionView>> {
    let Some(subscription_id) = lease.subscription_id.as_deref() else {
        return if recovering {
            finish_recovery(pool, lease, SubscriptionObservation::Missing).await
        } else {
            finish_billing(pool, lease, SubscriptionObservation::Missing).await
        };
    };
    let Some(provider) = provider else {
        return fail_attempt(pool, lease, "billing_unavailable").await;
    };
    let observation = if recovering {
        provider.get_subscription(subscription_id).await
    } else {
        provider
            .cancel_subscription(
                subscription_id,
                &format!("org-deletion-{}", lease.id),
                &lease.org_id,
            )
            .await
    };
    match observation {
        Ok(observation) if recovering => finish_recovery(pool, lease, observation).await,
        Ok(observation) => finish_billing(pool, lease, observation).await,
        Err(error) => handle_provider_error(pool, lease, error.kind, error.code).await,
    }
}

async fn advance_retention(
    pool: &PgPool,
    lease: &DeletionLease,
    provider: Option<&dyn SubscriptionProvider>,
) -> Result<Option<DeletionView>> {
    let Some(subscription_id) = lease.subscription_id.as_deref() else {
        return enter_purging(pool, lease).await;
    };
    let Some(provider) = provider else {
        if operator_observation_is_fresh(pool, lease).await? {
            return enter_purging(pool, lease).await;
        }
        return fail_attempt(pool, lease, "billing_unavailable").await;
    };
    match provider.get_subscription(subscription_id).await {
        Ok(observation) => finish_retention_reconciliation(pool, lease, observation).await,
        Err(error) => handle_provider_error(pool, lease, error.kind, error.code).await,
    }
}

async fn operator_observation_is_fresh(pool: &PgPool, lease: &DeletionLease) -> Result<bool> {
    // Fifteen minutes is the documented maximum age for an operator observation at purge time.
    let fresh: Option<bool> = sqlx::query_scalar(
        "SELECT billing_observation_source = 'operator' \
                AND last_billing_state IN ('terminal', 'missing') \
                AND billing_checked_at >= now() - ($1 * interval '1 second') \
         FROM organization_deletions \
         WHERE id = $2::uuid AND state_version = $3 AND lease_owner = $4",
    )
    .bind(BILLING_OBSERVATION_MAX_AGE.as_secs() as i64)
    .bind(&lease.id)
    .bind(lease.state_version)
    .bind(&lease.worker_id)
    .fetch_optional(pool)
    .await?;
    Ok(fresh.unwrap_or(false))
}

/// Record a terminal or missing provider observation after the caller has authenticated the
/// operator. The exact subscription ID, actor, reason, evidence, and observation time are retained
/// so a later operator entrypoint can audit the observation rather than bypassing the purge gate.
pub struct OperatorObservation<'a> {
    pub subscription_id: &'a str,
    pub observed_status: &'a str,
    pub observed_at: &'a str,
    pub reason: &'a str,
    pub evidence: &'a str,
}

pub async fn record_operator_observation(
    pool: &PgPool,
    org_id: &str,
    operator: &str,
    observation: OperatorObservation<'_>,
) -> Result<DeletionView> {
    if operator.is_empty()
        || observation.subscription_id.is_empty()
        || observation.observed_status.is_empty()
        || observation.observed_at.is_empty()
        || observation.reason.is_empty()
        || observation.evidence.is_empty()
    {
        return Err(Error::BadRequest(
            "operator, subscription, status, observed_at, reason, and evidence are required".into(),
        ));
    }
    let gate = if observation.observed_status == "resource_missing" {
        PurgeGate::Missing
    } else {
        SubscriptionStatus::parse(observation.observed_status).purge_gate()
    };
    if !matches!(gate, PurgeGate::Terminal | PurgeGate::Missing) {
        return Err(Error::BadRequest(
            "manual observation must be terminal or missing".into(),
        ));
    }

    let mut tx = pool.begin().await?;
    let row: Option<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT d.id::text, d.state, d.subscription_id \
         FROM organization_deletions d JOIN organizations o ON o.id = d.org_id \
         WHERE d.org_id = $1 AND o.lifecycle_state = 'deleting' \
           AND d.state NOT IN ('cancelled', 'completed') FOR UPDATE OF d, o",
    )
    .bind(org_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((id, state, operation_subscription)) = row else {
        return Err(Error::NotFound("deletion not found".into()));
    };
    if state == DeletionState::Purging.as_str() {
        return Err(Error::Conflict(
            "organisation deletion cannot be observed after purge has started".into(),
        ));
    }
    if operation_subscription.as_deref() != Some(observation.subscription_id) {
        return Err(Error::Conflict(
            "manual observation does not match the deletion subscription".into(),
        ));
    }
    let row: Option<(String, String)> = sqlx::query_as(
        "UPDATE organization_deletions SET state = 'retention', resume_state = NULL, \
         attempt_count = 0, last_billing_state = $1, billing_checked_at = $2::timestamptz, \
         billing_observation_source = 'operator', billing_observed_by = $3, \
         billing_observation_reason = $4, billing_observation_evidence = $5, \
         next_attempt_at = NULL, lease_owner = NULL, lease_expires_at = NULL, \
         state_version = state_version + 1 \
         WHERE id = $6::uuid AND $2::timestamptz <= now() \
         RETURNING org_id, state",
    )
    .bind(billing_state_name(gate))
    .bind(observation.observed_at)
    .bind(operator)
    .bind(observation.reason)
    .bind(observation.evidence)
    .bind(&id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        return Err(Error::BadRequest(
            "manual observation time cannot be in the future".into(),
        ));
    };
    audit::record_tx(
        &mut tx,
        org_id,
        operator,
        "org.deletion.billing_observed",
        audit::Context {
            target: Some(observation.subscription_id),
            detail: Some("source=operator"),
            ..Default::default()
        },
    )
    .await?;
    tx.commit().await?;
    view_from_row(&id, row)
}

async fn finish_retention_reconciliation(
    pool: &PgPool,
    lease: &DeletionLease,
    observation: SubscriptionObservation,
) -> Result<Option<DeletionView>> {
    let gate = observation.purge_gate();
    let (state, retry_now) = match gate {
        PurgeGate::Terminal | PurgeGate::Missing => (DeletionState::Purging, false),
        PurgeGate::Blocking => (DeletionState::CancellingBilling, true),
        PurgeGate::Unknown => {
            return handle_provider_error(
                pool,
                lease,
                ProviderErrorKind::Unknown,
                Some("billing_status_unknown".into()),
            )
            .await;
        }
    };
    let mut tx = pool.begin().await?;
    let row: Option<(String, String)> = sqlx::query_as(
        "UPDATE organization_deletions SET state = $1, last_billing_state = $2, \
         attempt_count = 0, billing_checked_at = now(), billing_observation_source = 'provider', \
         next_attempt_at = CASE WHEN $3 THEN now() ELSE NULL END, \
         lease_owner = NULL, lease_expires_at = NULL, state_version = state_version + 1 \
         WHERE id = $4::uuid AND state_version = $5 AND lease_owner = $6 \
           AND ($1 <> 'purging' OR purge_after <= now()) \
         RETURNING org_id, state",
    )
    .bind(state.as_str())
    .bind(billing_state_name(gate))
    .bind(retry_now)
    .bind(&lease.id)
    .bind(lease.state_version)
    .bind(&lease.worker_id)
    .fetch_optional(&mut *tx)
    .await?;
    if row.is_some() && state == DeletionState::Purging {
        audit::record_tx(
            &mut tx,
            &lease.org_id,
            &lease.worker_id,
            "org.deletion.purge_started",
            audit::Context {
                target: Some(&lease.id),
                ..Default::default()
            },
        )
        .await?;
    }
    tx.commit().await?;
    row.map(|row| view_from_row(&lease.id, row)).transpose()
}

async fn enter_purging(pool: &PgPool, lease: &DeletionLease) -> Result<Option<DeletionView>> {
    let mut tx = pool.begin().await?;
    // A deletion without a subscription was reconciled as missing before retention. Refresh that
    // observation when the retention window ends so the final purge gate measures this transition,
    // rather than the thirty-day-old billing check that made the operation eligible for purge.
    let row: Option<(String, String)> = sqlx::query_as(
        "UPDATE organization_deletions SET state = 'purging', attempt_count = 0, \
         billing_checked_at = CASE WHEN subscription_id IS NULL THEN now() ELSE billing_checked_at END, \
         next_attempt_at = NULL, \
         lease_owner = NULL, lease_expires_at = NULL, state_version = state_version + 1 \
         WHERE id = $1::uuid AND state = 'retention' AND purge_after <= now() \
           AND state_version = $2 AND lease_owner = $3 \
         RETURNING org_id, state",
    )
    .bind(&lease.id)
    .bind(lease.state_version)
    .bind(&lease.worker_id)
    .fetch_optional(&mut *tx)
    .await?;
    if row.is_some() {
        audit::record_tx(
            &mut tx,
            &lease.org_id,
            &lease.worker_id,
            "org.deletion.purge_started",
            audit::Context {
                target: Some(&lease.id),
                ..Default::default()
            },
        )
        .await?;
    }
    tx.commit().await?;
    row.map(|row| view_from_row(&lease.id, row)).transpose()
}

async fn finish_billing(
    pool: &PgPool,
    lease: &DeletionLease,
    observation: SubscriptionObservation,
) -> Result<Option<DeletionView>> {
    let gate = observation.purge_gate();
    if let Some(next) = billing_transition(gate) {
        let state = next.as_str();
        let billing_state = billing_state_name(gate);
        let mut tx = pool.begin().await?;
        let row: Option<(String, String)> = sqlx::query_as(
            "UPDATE organization_deletions SET state = $1, last_billing_state = $2, \
             attempt_count = 0, billing_checked_at = now(), billing_observation_source = 'provider', \
             next_attempt_at = NULL, lease_owner = NULL, lease_expires_at = NULL, \
             state_version = state_version + 1 \
             WHERE id = $3::uuid AND state_version = $4 AND lease_owner = $5 \
             RETURNING org_id, state",
        )
        .bind(state)
        .bind(billing_state)
        .bind(&lease.id)
        .bind(lease.state_version)
        .bind(&lease.worker_id)
        .fetch_optional(&mut *tx)
        .await?;
        if row.is_some() {
            audit::record_tx(
                &mut tx,
                &lease.org_id,
                &lease.worker_id,
                "org.deletion.billing_cancelled",
                audit::Context {
                    target: Some(&lease.id),
                    detail: Some(billing_state),
                    ..Default::default()
                },
            )
            .await?;
        }
        tx.commit().await?;
        return row.map(|row| view_from_row(&lease.id, row)).transpose();
    }
    handle_provider_error(
        pool,
        lease,
        ProviderErrorKind::Unknown,
        Some(
            match gate {
                PurgeGate::Blocking => "billing_still_blocking",
                PurgeGate::Unknown => "billing_status_unknown",
                PurgeGate::Terminal | PurgeGate::Missing => unreachable!(),
            }
            .into(),
        ),
    )
    .await
}

async fn finish_recovery(
    pool: &PgPool,
    lease: &DeletionLease,
    observation: SubscriptionObservation,
) -> Result<Option<DeletionView>> {
    let gate = observation.purge_gate();
    let tier = match &observation {
        SubscriptionObservation::Current(snapshot) if !matches!(gate, PurgeGate::Unknown) => {
            snapshot.status.entitlement_tier()
        }
        SubscriptionObservation::Missing => "free",
        SubscriptionObservation::Current(_) => {
            return handle_provider_error(
                pool,
                lease,
                ProviderErrorKind::Unknown,
                Some("billing_status_unknown".into()),
            )
            .await;
        }
    };
    let mut tx = pool.begin().await?;
    let row: Option<(String, String)> = sqlx::query_as(
        "UPDATE organization_deletions SET state = 'cancelled', cancelled_at = now(), \
             attempt_count = 0, last_billing_state = $1, billing_checked_at = now(), \
             billing_observation_source = 'provider', next_attempt_at = NULL, \
             lease_owner = NULL, lease_expires_at = NULL, state_version = state_version + 1 \
             WHERE id = $2::uuid AND state_version = $3 AND lease_owner = $4 \
             RETURNING org_id, state",
    )
    .bind(billing_state_name(gate))
    .bind(&lease.id)
    .bind(lease.state_version)
    .bind(&lease.worker_id)
    .fetch_optional(&mut *tx)
    .await?;
    if row.is_some() {
        sqlx::query(
            "UPDATE organizations SET lifecycle_state = 'active', tier = $1 \
             WHERE id = $2 AND lifecycle_state = 'deleting'",
        )
        .bind(tier)
        .bind(&lease.org_id)
        .execute(&mut *tx)
        .await?;
        audit::record_tx(
            &mut tx,
            &lease.org_id,
            &lease.worker_id,
            "org.deletion.recovery_completed",
            audit::Context {
                target: Some(&lease.id),
                detail: Some(tier),
                ..Default::default()
            },
        )
        .await?;
    }
    tx.commit().await?;
    row.map(|row| view_from_row(&lease.id, row)).transpose()
}

async fn handle_provider_error(
    pool: &PgPool,
    lease: &DeletionLease,
    kind: ProviderErrorKind,
    code: Option<String>,
) -> Result<Option<DeletionView>> {
    let authentication = matches!(&kind, ProviderErrorKind::Authentication);
    match provider_retry_decision(kind, lease.attempt_count) {
        RetryDecision::Retry(_) => {
            let delay = retry_delay_with_jitter(&lease.id, lease.attempt_count)
                .ok_or_else(|| lifecycle_error("retry delay unexpectedly exhausted"))?;
            schedule_retry(pool, lease, code, delay).await
        }
        RetryDecision::Failed => {
            let code = if authentication {
                "billing_unavailable"
            } else {
                code.as_deref().unwrap_or("provider_error")
            };
            fail_attempt(pool, lease, code).await
        }
    }
}

async fn schedule_retry(
    pool: &PgPool,
    lease: &DeletionLease,
    code: Option<String>,
    delay: Duration,
) -> Result<Option<DeletionView>> {
    let mut tx = pool.begin().await?;
    let row: Option<(String, String)> = sqlx::query_as(
        "UPDATE organization_deletions SET next_attempt_at = now() + ($1 * interval '1 second'), \
         last_error_code = $2, lease_owner = NULL, lease_expires_at = NULL, \
         state_version = state_version + 1 \
         WHERE id = $3::uuid AND state_version = $4 AND lease_owner = $5 \
         RETURNING org_id, state",
    )
    .bind(delay.as_secs() as i64)
    .bind(code)
    .bind(&lease.id)
    .bind(lease.state_version)
    .bind(&lease.worker_id)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    row.map(|row| view_from_row(&lease.id, row)).transpose()
}

async fn fail_attempt(
    pool: &PgPool,
    lease: &DeletionLease,
    code: &str,
) -> Result<Option<DeletionView>> {
    let mut tx = pool.begin().await?;
    let row: Option<(String, String)> = sqlx::query_as(
        "UPDATE organization_deletions SET state = 'failed', resume_state = $1, \
         next_attempt_at = NULL, last_error_code = $2, lease_owner = NULL, \
         lease_expires_at = NULL, state_version = state_version + 1 \
         WHERE id = $3::uuid AND state_version = $4 AND lease_owner = $5 \
         RETURNING org_id, state",
    )
    .bind(lease.state.as_str())
    .bind(code)
    .bind(&lease.id)
    .bind(lease.state_version)
    .bind(&lease.worker_id)
    .fetch_optional(&mut *tx)
    .await?;
    if row.is_some() {
        audit::record_tx(
            &mut tx,
            &lease.org_id,
            &lease.worker_id,
            "org.deletion.failed",
            audit::Context {
                target: Some(&lease.id),
                detail: Some(code),
                ..Default::default()
            },
        )
        .await?;
    }
    tx.commit().await?;
    row.map(|row| view_from_row(&lease.id, row)).transpose()
}

fn billing_state_name(gate: PurgeGate) -> &'static str {
    match gate {
        PurgeGate::Blocking => "blocking",
        PurgeGate::Terminal => "terminal",
        PurgeGate::Missing => "missing",
        PurgeGate::Unknown => "unknown",
    }
}

fn view_from_row(id: &str, (org_id, state): (String, String)) -> Result<DeletionView> {
    Ok(DeletionView {
        id: id.into(),
        org_id,
        state: DeletionState::from_db(&state)?,
    })
}

type PurgeRow = (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    bool,
    bool,
    bool,
    String,
    Option<String>,
);

/// Apply the final purge only after rechecking every safety gate in the same transaction.
async fn purge(pool: &PgPool, lease: &DeletionLease) -> Result<Option<DeletionView>> {
    let mut tx = pool.begin().await?;
    // Provider and operator observations share the fifteen-minute freshness bound after a lost
    // lease, so a stale result cannot satisfy the destructive purge gate.
    let row: Option<PurgeRow> = sqlx::query_as(
        "SELECT d.org_id, d.state, d.subscription_id, d.last_billing_state, \
                d.billing_observation_source, d.billing_checked_at IS NOT NULL, \
                (d.billing_checked_at >= now() - ($1 * interval '1 second') \
                 AND d.billing_observation_source IN ('provider', 'operator')), \
                d.purge_after <= now(), \
                o.lifecycle_state, o.stripe_subscription_id \
         FROM organization_deletions d JOIN organizations o ON o.id = d.org_id \
         WHERE d.id = $2::uuid AND d.state_version = $3 AND d.lease_owner = $4 \
         FOR UPDATE OF d, o",
    )
    .bind(BILLING_OBSERVATION_MAX_AGE.as_secs() as i64)
    .bind(&lease.id)
    .bind(lease.state_version)
    .bind(&lease.worker_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((
        org_id,
        state,
        operation_subscription,
        billing_state,
        _observation_source,
        checked,
        fresh,
        due,
        lifecycle,
        current_subscription,
    )) = row
    else {
        tx.commit().await?;
        return Ok(None);
    };
    if state != DeletionState::Purging.as_str()
        || lifecycle != "deleting"
        || !due
        || !checked
        || !fresh
        || !matches!(billing_state.as_deref(), Some("terminal" | "missing"))
        || (operation_subscription.is_none() && current_subscription.is_some())
        || (operation_subscription.is_some()
            && current_subscription.is_some()
            && current_subscription != operation_subscription)
    {
        let failed: Option<String> = sqlx::query_scalar(
            "UPDATE organization_deletions SET state = 'failed', resume_state = 'purging', \
             last_error_code = 'purge_precondition_failed', lease_owner = NULL, \
             lease_expires_at = NULL, next_attempt_at = NULL, state_version = state_version + 1 \
             WHERE id = $1::uuid AND state_version = $2 AND lease_owner = $3 \
             RETURNING org_id",
        )
        .bind(&lease.id)
        .bind(lease.state_version)
        .bind(&lease.worker_id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(failed_org_id) = failed.as_deref() {
            audit::record_tx(
                &mut tx,
                failed_org_id,
                &lease.worker_id,
                "org.deletion.failed",
                audit::Context {
                    target: Some(&lease.id),
                    detail: Some("purge_precondition_failed"),
                    ..Default::default()
                },
            )
            .await?;
        }
        tx.commit().await?;
        return Ok(failed.map(|org_id| DeletionView {
            id: lease.id.clone(),
            org_id,
            state: DeletionState::Failed,
        }));
    }

    // Projects own environments, grants, secrets, and machine tokens through cascading FKs.
    sqlx::query("DELETE FROM projects WHERE org_id = $1")
        .bind(&org_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM organization_memberships WHERE org_id = $1")
        .bind(&org_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE organizations SET lifecycle_state = 'deleted', deleted_at = now(), \
         enc_name = NULL, created_by = NULL, tier = 'free', trial_ends_at = NULL, \
         stripe_customer_id = NULL, stripe_subscription_id = NULL \
         WHERE id = $1 AND lifecycle_state = 'deleting'",
    )
    .bind(&org_id)
    .execute(&mut *tx)
    .await?;
    audit::record_tx(
        &mut tx,
        &org_id,
        &lease.worker_id,
        "org.deletion.completed",
        audit::Context {
            target: Some(&lease.id),
            ..Default::default()
        },
    )
    .await?;
    let completed: Option<String> = sqlx::query_scalar(
        "UPDATE organization_deletions SET state = 'completed', completed_at = now(), \
         lease_owner = NULL, lease_expires_at = NULL, state_version = state_version + 1 \
         WHERE id = $1::uuid AND state_version = $2 AND lease_owner = $3 \
         RETURNING org_id",
    )
    .bind(&lease.id)
    .bind(lease.state_version)
    .bind(&lease.worker_id)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(completed.map(|org_id| DeletionView {
        id: lease.id.clone(),
        org_id,
        state: DeletionState::Completed,
    }))
}

async fn require_owner(
    tx: &mut Transaction<'_, Postgres>,
    org_id: &str,
    actor: &str,
) -> Result<()> {
    let role: Option<String> = sqlx::query_scalar(
        "SELECT role FROM organization_memberships WHERE org_id = $1 AND user_id = $2",
    )
    .bind(org_id)
    .bind(actor)
    .fetch_optional(&mut **tx)
    .await?;
    match role {
        None => Err(Error::NotFound("organisation not found".into())),
        Some(role) if Role::from_db(&role)? == Role::Owner => Ok(()),
        Some(_) => Err(Error::Forbidden(
            "only an organisation owner may manage deletion".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        billing_transition, provider_retry_decision, retry_delay, DeletionState, RetryDecision,
    };
    use crate::billing::{ProviderErrorKind, PurgeGate};
    use crate::error::Error;
    use sqlx::postgres::PgPoolOptions;
    use std::time::Duration;

    #[test]
    fn deletion_states_round_trip_and_fail_closed() {
        for state in [
            DeletionState::Requested,
            DeletionState::CancellingBilling,
            DeletionState::Retention,
            DeletionState::Purging,
            DeletionState::Recovering,
            DeletionState::Failed,
            DeletionState::Cancelled,
            DeletionState::Completed,
        ] {
            assert_eq!(DeletionState::from_db(state.as_str()).unwrap(), state);
        }
        assert!(matches!(
            DeletionState::from_db("archived"),
            Err(Error::Internal(_))
        ));
    }

    #[tokio::test]
    async fn empty_worker_id_is_an_internal_error() {
        // Validation happens before the transaction starts, so this pool never connects. The
        // closed local port also prevents an accidental future connection from touching dev data.
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://127.0.0.1:1/sotto")
            .expect("valid lazy database URL");

        assert!(matches!(
            super::claim_due(&pool, "").await,
            Err(Error::Internal(message)) if message == "deletion worker id is empty"
        ));
    }

    #[test]
    fn cancellation_is_allowed_only_before_purge_or_terminal_state() {
        assert!(DeletionState::Requested.can_cancel());
        assert!(DeletionState::CancellingBilling.can_cancel());
        assert!(DeletionState::Retention.can_cancel());
        assert!(DeletionState::Failed.can_cancel());
        assert!(!DeletionState::Purging.can_cancel());
        assert!(DeletionState::Cancelled.is_terminal());
        assert!(DeletionState::Completed.is_terminal());
    }

    #[test]
    fn retry_schedule_is_bounded_and_ordered() {
        assert_eq!(retry_delay(1), Some(Duration::from_secs(60)));
        assert_eq!(retry_delay(2), Some(Duration::from_secs(5 * 60)));
        assert_eq!(retry_delay(6), Some(Duration::from_secs(24 * 60 * 60)));
        assert_eq!(retry_delay(7), None);
        let jittered = super::retry_delay_with_jitter("operation-a", 1).unwrap();
        assert!(jittered >= Duration::from_secs(60));
        assert!(jittered < Duration::from_secs(90));
    }

    #[test]
    fn authentication_failures_do_not_enter_the_retry_ladder() {
        assert_eq!(
            provider_retry_decision(ProviderErrorKind::Authentication, 1),
            RetryDecision::Failed
        );
        assert_eq!(
            provider_retry_decision(ProviderErrorKind::Retryable, 1),
            RetryDecision::Retry(Duration::from_secs(60))
        );
        assert_eq!(
            provider_retry_decision(ProviderErrorKind::Unknown, 7),
            RetryDecision::Failed
        );
    }

    #[test]
    fn only_terminal_or_missing_billing_observations_enter_retention() {
        assert_eq!(
            billing_transition(PurgeGate::Terminal),
            Some(DeletionState::Retention)
        );
        assert_eq!(
            billing_transition(PurgeGate::Missing),
            Some(DeletionState::Retention)
        );
        assert_eq!(billing_transition(PurgeGate::Blocking), None);
        assert_eq!(billing_transition(PurgeGate::Unknown), None);
    }
}
