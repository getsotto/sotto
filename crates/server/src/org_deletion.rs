//! Internal organisation-deletion lifecycle and worker operations.
//!
//! The public route is deliberately absent from this module's callers for now.  This seam owns
//! the durable state machine, leases, compare-and-set transitions, provider reconciliation, and
//! final purge so a later HTTP layer cannot duplicate safety rules.

use std::time::Duration;

use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::audit;
use crate::billing::{
    ProviderErrorKind, PurgeGate, SubscriptionObservation, SubscriptionProvider, SubscriptionStatus,
};
use crate::error::{Error, Result};
use crate::org::{self, LifecycleState, Role};

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
            other => Err(Error::Config(format!(
                "unknown organisation deletion state in db: {other}"
            ))),
        }
    }

    pub fn can_cancel(self) -> bool {
        matches!(
            self,
            Self::Requested | Self::CancellingBilling | Self::Retention | Self::Failed
        )
    }

    pub fn is_terminal(self) -> bool {
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
        return Err(Error::NotFound("organisation not found".into()));
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
            return Err(Error::Config(
                "deleting organisation has no active deletion operation".into(),
            ));
        };
        let state = DeletionState::from_db(&state)?;
        if state == DeletionState::Failed {
            let resume_state = resume_state.ok_or_else(|| {
                Error::Config("failed deletion has no recorded resume state".into())
            })?;
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

/// Return an owner's current active deletion attempt without changing its state.
pub async fn status(pool: &PgPool, org_id: &str, actor: &str) -> Result<DeletionView> {
    match org::access(pool, org_id, actor).await {
        Ok(access) if access.role() == Role::Owner => {}
        Ok(_) => {
            return Err(Error::Forbidden(
                "only an organisation owner may inspect deletion".into(),
            ));
        }
        Err(Error::NotFound(_)) => {}
        Err(error) => return Err(error),
    }
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT id::text, state FROM organization_deletions \
         WHERE org_id = $1 AND state NOT IN ('cancelled', 'completed') \
         ORDER BY requested_at DESC LIMIT 1",
    )
    .bind(org_id)
    .fetch_optional(pool)
    .await?;
    let row = match row {
        Some(row) => Some(row),
        None => {
            sqlx::query_as(
                "SELECT id::text, state FROM organization_deletions \
             WHERE org_id = $1 AND requested_by = $2 AND state IN ('cancelled', 'completed') \
             ORDER BY requested_at DESC LIMIT 1",
            )
            .bind(org_id)
            .bind(actor)
            .fetch_optional(pool)
            .await?
        }
    };
    let Some((id, state)) = row else {
        return Err(Error::NotFound("deletion not found".into()));
    };
    Ok(DeletionView {
        id,
        org_id: org_id.into(),
        state: DeletionState::from_db(&state)?,
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
    let Some((id, state)) = row else {
        return Err(Error::NotFound("deletion not found".into()));
    };
    let state = DeletionState::from_db(&state)?;
    match state {
        DeletionState::Purging => {
            return Err(Error::Conflict(
                "organisation deletion cannot be cancelled after purge has started".into(),
            ));
        }
        DeletionState::Recovering | DeletionState::Cancelled | DeletionState::Completed => {
            tx.commit().await?;
            return Ok(DeletionView {
                id,
                org_id: org_id.into(),
                state,
            });
        }
        DeletionState::Requested
        | DeletionState::CancellingBilling
        | DeletionState::Retention
        | DeletionState::Failed => {}
    }
    sqlx::query(
        "UPDATE organization_deletions SET state = 'recovering', resume_state = NULL, \
         attempt_count = 0, next_attempt_at = now(), \
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
        return Err(Error::Config("deletion worker id is empty".into()));
    }
    let mut tx = pool.begin().await?;
    // Five-minute leases give another worker a bounded recovery window after a crash.
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
                AND billing_checked_at >= now() - interval '15 minutes' \
         FROM organization_deletions \
         WHERE id = $1::uuid AND state_version = $2 AND lease_owner = $3",
    )
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
                .ok_or_else(|| Error::Config("retry delay unexpectedly exhausted".into()))?;
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
                (d.billing_checked_at >= now() - interval '15 minutes' \
                 AND d.billing_observation_source IN ('provider', 'operator')), \
                d.purge_after <= now(), \
                o.lifecycle_state, o.stripe_subscription_id \
         FROM organization_deletions d JOIN organizations o ON o.id = d.org_id \
         WHERE d.id = $1::uuid AND d.state_version = $2 AND d.lease_owner = $3 \
         FOR UPDATE OF d, o",
    )
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
        assert!(DeletionState::from_db("archived").is_err());
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
