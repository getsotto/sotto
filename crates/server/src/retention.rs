//! Durable, explicitly scoped Cloud retention jobs.
//!
//! A retention job records who or which organisation is in scope, the eligibility episode that
//! created it, the fixed deadline and the notice evidence that supports it.  Scope items are
//! stored separately so a later dry run can be reviewed before any purge switch is enabled. The
//! production lifecycle calls only the personal dry-run entry point; purge remains separate and
//! dormant pending its own activation gates.
//!
//! Scope item identifiers are canonical database identifiers. Share-link tokens supplied by a
//! producer are resolved to their internal `share_links.id` while the scope is inserted.

use std::collections::HashSet;
use std::fmt;

use crate::cloud_coverage::{self, CoverageState, EXPORT_WINDOW_SECONDS};
use crate::cloud_coverage_store::StoreError;
use sqlx::{PgPool, Postgres, Row, Transaction};
use thiserror::Error;
use uuid::Uuid;

const MAX_KEY: usize = 256;
const MAX_EPISODE: usize = 160;
const MAX_NOTICE_KEY: usize = 160;
const MAX_SCOPE_ITEMS: i64 = 4_096;
const BATCH_SIZE: i64 = 64;

type RetentionResult<T> = std::result::Result<T, RetentionError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeKind {
    Personal,
    SharedOrganisation,
    OrphanOrganisation,
}

impl ScopeKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Personal => "personal",
            Self::SharedOrganisation => "shared_organisation",
            Self::OrphanOrganisation => "orphan_organisation",
        }
    }
}

impl fmt::Display for ScopeKind {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        output.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetentionState {
    Planned,
    DryRun,
    Ready,
    Leased,
    Held,
    Cancelled,
    Completed,
    Failed,
}

impl RetentionState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::DryRun => "dry_run",
            Self::Ready => "ready",
            Self::Leased => "leased",
            Self::Held => "held",
            Self::Cancelled => "cancelled",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetentionMode {
    DryRun,
    Purge,
}

impl RetentionMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DryRun => "dry_run",
            Self::Purge => "purge",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionIntent {
    pub job_id: String,
    pub scope_kind: ScopeKind,
    pub scope_key: String,
    pub subject_user_id: Option<String>,
    pub organisation_id: Option<String>,
    pub eligibility_episode: String,
    pub deadline_epoch: i64,
    pub notice_event_key: String,
    pub notice_evidence_epoch: Option<i64>,
    pub expected_coverage_revision: Option<i64>,
    pub expected_scope_generation: Option<i64>,
    pub eligibility_loss_epoch: Option<i64>,
}

impl RetentionIntent {
    pub fn validate(&self) -> RetentionResult<()> {
        bounded(&self.job_id, MAX_KEY, "job_id")?;
        bounded(&self.scope_key, MAX_KEY, "scope_key")?;
        bounded(
            &self.eligibility_episode,
            MAX_EPISODE,
            "eligibility_episode",
        )?;
        bounded(&self.notice_event_key, MAX_NOTICE_KEY, "notice_event_key")?;
        if self.deadline_epoch <= 0 {
            return Err(RetentionError::InvalidInput("deadline_epoch"));
        }
        if self
            .notice_evidence_epoch
            .is_some_and(|evidence| evidence <= 0 || evidence > self.deadline_epoch)
        {
            return Err(RetentionError::InvalidInput("notice_evidence_epoch"));
        }
        match self.scope_kind {
            ScopeKind::Personal
                if self
                    .subject_user_id
                    .as_deref()
                    .is_some_and(|id| !id.trim().is_empty())
                    && self.organisation_id.is_none()
                    && self.expected_scope_generation.is_none()
                    && self.eligibility_loss_epoch.is_none() => {}
            ScopeKind::SharedOrganisation
                if self.subject_user_id.is_none()
                    && self
                        .organisation_id
                        .as_deref()
                        .is_some_and(|id| !id.trim().is_empty())
                    && self
                        .expected_scope_generation
                        .is_some_and(|generation| generation >= 0)
                    && (self.eligibility_loss_epoch.is_some_and(|epoch| epoch >= 0)
                        || (!self.eligibility_episode.starts_with("shared-export:")
                            && self.eligibility_loss_epoch.is_none())) => {}
            ScopeKind::OrphanOrganisation
                if self.subject_user_id.is_none()
                    && self
                        .organisation_id
                        .as_deref()
                        .is_some_and(|id| !id.trim().is_empty())
                    && self
                        .expected_scope_generation
                        .is_some_and(|generation| generation >= 0)
                    && self.eligibility_loss_epoch.is_none() => {}
            _ => return Err(RetentionError::InvalidInput("scope ownership")),
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeItem {
    pub job_id: String,
    pub resource_kind: String,
    pub resource_id: String,
    pub ownership_kind: String,
    pub expected_created_at_epoch: i64,
    pub expected_revision: Option<i64>,
}

impl ScopeItem {
    pub fn validate(&self) -> RetentionResult<()> {
        bounded(&self.job_id, MAX_KEY, "job_id")?;
        bounded(&self.resource_kind, 32, "resource_kind")?;
        bounded(&self.resource_id, MAX_KEY, "resource_id")?;
        if !matches!(self.ownership_kind.as_str(), "personal" | "shared") {
            return Err(RetentionError::InvalidInput("ownership_kind"));
        }
        if self.expected_created_at_epoch <= 0 {
            return Err(RetentionError::InvalidInput("expected_created_at_epoch"));
        }
        if !matches!(
            self.resource_kind.as_str(),
            "project" | "environment" | "grant" | "machine_token" | "share"
        ) {
            return Err(RetentionError::InvalidInput("resource_kind"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueOutcome {
    Enqueued,
    AlreadyQueued,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperatorAction {
    Hold,
    Cancel,
    RetryDryRun,
}

impl OperatorAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Hold => "held",
            Self::Cancel => "cancelled",
            Self::RetryDryRun => "retried",
        }
    }
}

#[derive(Debug, Error)]
pub enum RetentionError {
    #[error("retention database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("retention input is invalid: {0}")]
    InvalidInput(&'static str),
    #[error("retention job was not found")]
    NotFound,
    #[error("retention episode identity conflicts with an existing job")]
    IdentityConflict,
    #[error("retention lease was lost")]
    LeaseLost,
    #[error("retention job is held: {0}")]
    Held(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionRun {
    pub job_id: String,
    pub state: RetentionState,
    pub dry_run: bool,
    pub processed_items: usize,
    pub held_items: usize,
}

#[derive(Debug, Clone)]
struct RetentionLease {
    job_id: String,
    scope_kind: String,
    subject_user_id: Option<String>,
    expected_coverage_revision: Option<i64>,
    expected_scope_generation: Option<i64>,
    eligibility_loss_epoch: Option<i64>,
    organisation_id: Option<String>,
    dry_run: bool,
    deadline_epoch: i64,
    notice_event_key: String,
    notice_evidence_epoch: Option<i64>,
    worker_id: String,
}

fn bounded(value: &str, max: usize, field: &'static str) -> RetentionResult<()> {
    if value.trim().is_empty() || value.len() > max || value.chars().any(char::is_control) {
        return Err(RetentionError::InvalidInput(field));
    }
    Ok(())
}

pub(crate) fn share_lifetime_hold(
    share_class: &str,
    expires_at: Option<i64>,
    max_views: i32,
    view_count: i32,
    revoked: bool,
    now: i64,
    creator_deadline: i64,
) -> Option<&'static str> {
    if !matches!(share_class, "free" | "paid" | "legacy") {
        return Some("share_lifetime_unproven");
    }
    let inactive =
        revoked || view_count >= max_views || expires_at.is_some_and(|expiry| expiry <= now);
    if share_class == "free"
        && !inactive
        && expires_at.is_none_or(|expiry| expiry > creator_deadline)
    {
        Some("free_share_promise_after_deadline")
    } else {
        None
    }
}

pub async fn enqueue(pool: &PgPool, intent: &RetentionIntent) -> RetentionResult<EnqueueOutcome> {
    let mut tx = pool.begin().await?;
    let result = enqueue_in_tx(&mut tx, intent).await?;
    tx.commit().await?;
    Ok(result)
}

pub async fn enqueue_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    intent: &RetentionIntent,
) -> RetentionResult<EnqueueOutcome> {
    intent.validate()?;
    let inserted = sqlx::query(
        "INSERT INTO cloud_retention_jobs \
         (job_id, scope_kind, scope_key, subject_user_id, organisation_id, eligibility_episode, \
          deadline_at, notice_event_key, notice_evidence_at, expected_coverage_revision, expected_scope_generation, eligibility_loss_at) \
         VALUES ($1,$2,$3,$4,$5,$6,to_timestamp($7),$8,\
                 CASE WHEN $9::bigint IS NULL THEN NULL ELSE to_timestamp($9) END,$10,$11, \
                 CASE WHEN $12::bigint IS NULL THEN NULL ELSE to_timestamp($12) END) \
         ON CONFLICT (scope_kind, scope_key, eligibility_episode) DO NOTHING",
    )
    .bind(&intent.job_id)
    .bind(intent.scope_kind.as_str())
    .bind(&intent.scope_key)
    .bind(&intent.subject_user_id)
    .bind(&intent.organisation_id)
    .bind(&intent.eligibility_episode)
    .bind(intent.deadline_epoch)
    .bind(&intent.notice_event_key)
    .bind(intent.notice_evidence_epoch)
    .bind(intent.expected_coverage_revision)
    .bind(intent.expected_scope_generation)
    .bind(intent.eligibility_loss_epoch)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    if inserted == 1 {
        return Ok(EnqueueOutcome::Enqueued);
    }
    let existing = sqlx::query(
        "SELECT subject_user_id, organisation_id, extract(epoch from deadline_at)::bigint AS deadline_epoch, notice_event_key, \
                extract(epoch from notice_evidence_at)::bigint AS notice_evidence_epoch, expected_coverage_revision, expected_scope_generation, \
                extract(epoch from eligibility_loss_at)::bigint AS eligibility_loss_epoch, \
                state, dry_run, purge_enabled, job_id \
         FROM cloud_retention_jobs WHERE scope_kind=$1 AND scope_key=$2 \
           AND eligibility_episode=$3 FOR UPDATE",
    )
    .bind(intent.scope_kind.as_str())
    .bind(&intent.scope_key)
    .bind(&intent.eligibility_episode)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(RetentionError::IdentityConflict)?;
    let subject_user_id: Option<String> = existing.try_get("subject_user_id")?;
    let organisation_id: Option<String> = existing.try_get("organisation_id")?;
    let deadline_epoch: i64 = existing.try_get("deadline_epoch")?;
    let notice_event_key: String = existing.try_get("notice_event_key")?;
    let notice_evidence_epoch: Option<i64> = existing.try_get("notice_evidence_epoch")?;
    let expected_coverage_revision: Option<i64> = existing.try_get("expected_coverage_revision")?;
    let expected_scope_generation: Option<i64> = existing.try_get("expected_scope_generation")?;
    let eligibility_loss_epoch: Option<i64> = existing.try_get("eligibility_loss_epoch")?;
    let state: String = existing.try_get("state")?;
    let dry_run: bool = existing.try_get("dry_run")?;
    let purge_enabled: bool = existing.try_get("purge_enabled")?;
    let job_id: String = existing.try_get("job_id")?;
    if subject_user_id != intent.subject_user_id
        || organisation_id != intent.organisation_id
        || deadline_epoch != intent.deadline_epoch
        || notice_event_key != intent.notice_event_key
    {
        return Err(RetentionError::IdentityConflict);
    }
    if expected_coverage_revision != intent.expected_coverage_revision
        && !matches!(state.as_str(), "cancelled" | "completed" | "failed")
    {
        // A coverage revision is an observation for an unfinished personal dry-run episode, not
        // part of that episode's immutable identity. Requeue the recorded scope so every item is
        // checked again against the new authority before the dry-run can complete.
        if intent.scope_kind != ScopeKind::Personal
            || !dry_run
            || purge_enabled
            || (expected_coverage_revision.is_some() && intent.expected_coverage_revision.is_none())
        {
            return Err(RetentionError::IdentityConflict);
        }
        let affected = sqlx::query(
            "UPDATE cloud_retention_jobs SET expected_coverage_revision=$2, \
                 state=CASE WHEN state='dry_run' OR (state='held' AND hold_code='coverage_revision_changed') \
                            THEN 'ready' ELSE state END, \
                 hold_code=CASE WHEN state='held' AND hold_code='coverage_revision_changed' \
                                THEN NULL ELSE hold_code END, \
                 dry_run_completed_at=CASE WHEN state='dry_run' \
                       OR (state='held' AND hold_code='coverage_revision_changed') \
                       THEN NULL ELSE dry_run_completed_at END, updated_at=now() \
             WHERE job_id=$1 AND dry_run=TRUE AND purge_enabled=FALSE",
        )
        .bind(&job_id)
        .bind(intent.expected_coverage_revision)
        .execute(&mut **tx)
        .await?
        .rows_affected();
        if affected != 1 {
            return Err(RetentionError::IdentityConflict);
        }
        sqlx::query(
            "UPDATE cloud_retention_scope_items SET state='planned', updated_at=now() \
             WHERE job_id=$1 AND state='dry_run'",
        )
        .bind(&job_id)
        .execute(&mut **tx)
        .await?;
    }
    if expected_scope_generation != intent.expected_scope_generation
        && !matches!(state.as_str(), "cancelled" | "completed" | "failed")
    {
        if intent.scope_kind != ScopeKind::SharedOrganisation || !dry_run || purge_enabled {
            return Err(RetentionError::IdentityConflict);
        }
        let affected = sqlx::query(
            "UPDATE cloud_retention_jobs SET expected_scope_generation=$2, \
                    state=CASE WHEN state='held' AND hold_code='operator_hold' \
                               THEN state ELSE 'ready' END, \
                    hold_code=CASE WHEN state='held' AND hold_code='operator_hold' \
                                   THEN hold_code ELSE NULL END, \
                    dry_run_completed_at=NULL, lease_owner=NULL, \
                    lease_expires_at=NULL, updated_at=now() \
             WHERE job_id=$1 AND dry_run=TRUE AND purge_enabled=FALSE",
        )
        .bind(&job_id)
        .bind(intent.expected_scope_generation)
        .execute(&mut **tx)
        .await?
        .rows_affected();
        if affected != 1 {
            return Err(RetentionError::IdentityConflict);
        }
        sqlx::query(
            "UPDATE cloud_retention_scope_items SET state='planned', hold_code=NULL, updated_at=now() \
             WHERE job_id=$1 AND state IN ('dry_run','held')",
        )
        .bind(&job_id)
        .execute(&mut **tx)
        .await?;
    }
    if eligibility_loss_epoch != intent.eligibility_loss_epoch {
        return Err(RetentionError::IdentityConflict);
    }
    match (notice_evidence_epoch, intent.notice_evidence_epoch) {
        (Some(stored), Some(incoming)) if stored == incoming => {}
        (None, Some(incoming)) => {
            // Evidence may be attached once, after the exact notice becomes available. It can
            // never be removed or changed, and this update is protected by the job row lock.
            let affected = sqlx::query(
                "UPDATE cloud_retention_jobs SET notice_evidence_at=to_timestamp($2), updated_at=now() \
                 WHERE scope_kind=$1 AND scope_key=$3 AND eligibility_episode=$4 \
                   AND notice_evidence_at IS NULL",
            )
            .bind(intent.scope_kind.as_str())
            .bind(incoming)
            .bind(&intent.scope_key)
            .bind(&intent.eligibility_episode)
            .execute(&mut **tx)
            .await?
            .rows_affected();
            if affected != 1 {
                return Err(RetentionError::IdentityConflict);
            }
        }
        (None, None) => {}
        _ => return Err(RetentionError::IdentityConflict),
    }
    Ok(EnqueueOutcome::AlreadyQueued)
}

pub async fn add_scope_item(pool: &PgPool, item: &ScopeItem) -> RetentionResult<bool> {
    let mut tx = pool.begin().await?;
    let inserted = add_scope_item_in_tx(&mut tx, item, "planned", None).await?;
    tx.commit().await?;
    Ok(inserted)
}

pub(crate) async fn add_scope_item_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    item: &ScopeItem,
    state: &str,
    hold_code: Option<&str>,
) -> RetentionResult<bool> {
    item.validate()?;
    if !matches!(state, "planned" | "held") || (state == "held") != hold_code.is_some() {
        return Err(RetentionError::InvalidInput("scope item state"));
    }
    let job_state: Option<String> =
        sqlx::query_scalar("SELECT state FROM cloud_retention_jobs WHERE job_id=$1 FOR UPDATE")
            .bind(&item.job_id)
            .fetch_optional(&mut **tx)
            .await?;
    let Some(job_state) = job_state else {
        return Err(RetentionError::NotFound);
    };
    if job_state != RetentionState::Planned.as_str() {
        return Err(RetentionError::Held("retention scope is immutable"));
    }
    let resource_id = if item.resource_kind == "share" {
        sqlx::query_scalar::<_, String>(
            "SELECT id FROM share_links WHERE id=$1 OR token=$1 \
             ORDER BY (id=$1) DESC LIMIT 1",
        )
        .bind(&item.resource_id)
        .fetch_optional(&mut **tx)
        .await?
        .unwrap_or_else(|| item.resource_id.clone())
    } else {
        item.resource_id.clone()
    };
    let inserted = sqlx::query(
        "INSERT INTO cloud_retention_scope_items \
         (job_id, resource_kind, resource_id, ownership_kind, expected_created_at, expected_revision, state, hold_code) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT (job_id, resource_kind, resource_id) DO NOTHING",
    )
    .bind(&item.job_id)
    .bind(&item.resource_kind)
    .bind(&resource_id)
    .bind(&item.ownership_kind)
    .bind(item.expected_created_at_epoch)
    .bind(item.expected_revision)
    .bind(state)
    .bind(hold_code)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    Ok(inserted == 1)
}

pub async fn record_notice_evidence(
    pool: &PgPool,
    job_id: &str,
    notice_evidence_epoch: i64,
) -> RetentionResult<()> {
    bounded(job_id, MAX_KEY, "job_id")?;
    if notice_evidence_epoch <= 0 {
        return Err(RetentionError::InvalidInput("notice_evidence_epoch"));
    }
    let affected = sqlx::query(
        "UPDATE cloud_retention_jobs SET notice_evidence_at=to_timestamp($2), updated_at=now() \
         WHERE job_id=$1 AND notice_evidence_at IS NULL AND state IN ('planned','dry_run','held')",
    )
    .bind(job_id)
    .bind(notice_evidence_epoch)
    .execute(pool)
    .await?
    .rows_affected();
    if affected != 1 {
        return Err(RetentionError::NotFound);
    }
    Ok(())
}

pub async fn mark_ready(pool: &PgPool, job_id: &str) -> RetentionResult<()> {
    bounded(job_id, MAX_KEY, "job_id")?;
    let affected = sqlx::query(
        "UPDATE cloud_retention_jobs SET state='ready', dry_run=TRUE, updated_at=now() \
         WHERE job_id=$1 AND state='planned' AND notice_evidence_at IS NOT NULL \
           AND hold_code IS NULL",
    )
    .bind(job_id)
    .execute(pool)
    .await?
    .rows_affected();
    if affected != 1 {
        return Err(RetentionError::NotFound);
    }
    Ok(())
}

pub async fn cancel(pool: &PgPool, job_id: &str, reason: &str) -> RetentionResult<bool> {
    bounded(job_id, MAX_KEY, "job_id")?;
    bounded(reason, 160, "reason")?;
    let affected = sqlx::query(
        "UPDATE cloud_retention_jobs SET state='cancelled', hold_code=$2, lease_owner=NULL, \
             lease_expires_at=NULL, updated_at=now() \
         WHERE job_id=$1 AND state IN ('planned','dry_run','ready','held','leased')",
    )
    .bind(job_id)
    .bind(reason)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(affected == 1)
}

/// Apply a protected operator action and append its audit record in the same transaction.
/// Retry only requeues a dry-run job; it cannot change `purge_enabled` or `dry_run`.
pub async fn apply_operator_action(
    pool: &PgPool,
    job_id: &str,
    operator: &str,
    action: OperatorAction,
) -> RetentionResult<()> {
    bounded(job_id, MAX_KEY, "job_id")?;
    bounded(operator, 80, "operator")?;
    let mut tx = pool.begin().await?;
    let affected = match action {
        OperatorAction::Hold => sqlx::query(
            "UPDATE cloud_retention_jobs SET state='held', hold_code='operator_hold', \
                     lease_owner=NULL, lease_expires_at=NULL, updated_at=now() \
                 WHERE job_id=$1 AND state IN ('planned','ready','dry_run','held','leased') \
                   AND dry_run=TRUE AND purge_enabled=FALSE",
        )
        .bind(job_id)
        .execute(&mut *tx)
        .await?
        .rows_affected(),
        OperatorAction::Cancel => sqlx::query(
            "UPDATE cloud_retention_jobs SET state='cancelled', hold_code='operator_cancelled', \
                     lease_owner=NULL, lease_expires_at=NULL, updated_at=now() \
                 WHERE job_id=$1 AND state IN ('planned','ready','dry_run','held','leased') \
                   AND dry_run=TRUE AND purge_enabled=FALSE",
        )
        .bind(job_id)
        .execute(&mut *tx)
        .await?
        .rows_affected(),
        OperatorAction::RetryDryRun => {
            let affected = sqlx::query(
                "UPDATE cloud_retention_jobs SET state='ready', hold_code=NULL, \
                     dry_run_completed_at=NULL, lease_owner=NULL, lease_expires_at=NULL, updated_at=now() \
                 WHERE job_id=$1 AND state='held' AND dry_run=TRUE AND purge_enabled=FALSE \
                   AND notice_evidence_at IS NOT NULL AND deadline_at <= now() \
                   AND hold_code IS DISTINCT FROM 'scope_exceeds_bound' \
                   AND hold_code NOT IN ('no_verified_owner_contact','notice_window_elapsed')",
            )
            .bind(job_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            if affected == 1 {
                // Held items retain their expected identity, but a retry must validate that
                // identity again. Still-shared or invalid items will be held by the worker.
                sqlx::query(
                    "UPDATE cloud_retention_scope_items SET state='planned', hold_code=NULL, updated_at=now() \
                     WHERE job_id=$1 AND state='held'",
                )
                .bind(job_id)
                .execute(&mut *tx)
                .await?;
            }
            affected
        }
    };
    if affected != 1 {
        return Err(RetentionError::NotFound);
    }
    sqlx::query(
        "INSERT INTO cloud_retention_operator_actions(action_id, job_id, action, actor_label) \
         VALUES ($1,$2,$3,$4)",
    )
    .bind(format!("retention-action:{}", Uuid::new_v4()))
    .bind(job_id)
    .bind(action.as_str())
    .bind(operator)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn enable_purge(pool: &PgPool, job_id: &str) -> RetentionResult<()> {
    bounded(job_id, MAX_KEY, "job_id")?;
    let mut tx = pool.begin().await?;
    let affected = sqlx::query(
        "UPDATE cloud_retention_jobs SET state='ready', purge_enabled=TRUE, dry_run=FALSE, updated_at=now() \
         WHERE job_id=$1 AND scope_kind='personal' AND state='dry_run' AND dry_run_completed_at IS NOT NULL \
           AND notice_evidence_at IS NOT NULL AND deadline_at <= now() AND hold_code IS NULL",
    )
    .bind(job_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if affected != 1 {
        return Err(RetentionError::Held("purge prerequisites are not met"));
    }
    sqlx::query(
        "UPDATE cloud_retention_scope_items SET state='planned',updated_at=now() \
         WHERE job_id=$1 AND state='dry_run'",
    )
    .bind(job_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn list_scope(pool: &PgPool, job_id: &str) -> RetentionResult<Vec<ScopeItem>> {
    bounded(job_id, MAX_KEY, "job_id")?;
    let rows = sqlx::query(
        "SELECT job_id, resource_kind, resource_id, ownership_kind, expected_created_at, expected_revision \
         FROM cloud_retention_scope_items WHERE job_id=$1 ORDER BY resource_kind, resource_id \
         LIMIT $2",
    )
    .bind(job_id)
    .bind(MAX_SCOPE_ITEMS + 1)
    .fetch_all(pool)
    .await?;
    if rows.len() as i64 > MAX_SCOPE_ITEMS {
        return Err(RetentionError::Held(
            "retention scope exceeds the item bound",
        ));
    }
    rows.into_iter()
        .map(|row| {
            Ok(ScopeItem {
                job_id: row.try_get("job_id")?,
                resource_kind: row.try_get("resource_kind")?,
                resource_id: row.try_get("resource_id")?,
                ownership_kind: row.try_get("ownership_kind")?,
                expected_created_at_epoch: row.try_get("expected_created_at")?,
                expected_revision: row.try_get("expected_revision")?,
            })
        })
        .collect::<std::result::Result<Vec<_>, sqlx::Error>>()
        .map_err(RetentionError::Database)
}

/// Claim one due job.  A lease expiry makes a worker crash recoverable without allowing two
/// workers to process the same job at once.
async fn claim_due(
    pool: &PgPool,
    worker_id: &str,
    mode: RetentionMode,
    personal_only: bool,
    shared_only: bool,
) -> RetentionResult<Option<RetentionLease>> {
    bounded(worker_id, MAX_KEY, "worker_id")?;
    let mut tx = pool.begin().await?;
    let row = sqlx::query(
        "SELECT job_id FROM cloud_retention_jobs \
         WHERE deadline_at <= now() AND ($2=FALSE OR scope_kind='personal') \
           AND ($3=FALSE OR scope_kind='shared_organisation') AND \
           (($1 = 'dry_run' AND state='ready' AND dry_run=TRUE) OR \
            (state = 'ready' AND dry_run = FALSE AND purge_enabled = TRUE AND $1 = 'purge') OR \
            (state = 'leased' AND lease_expires_at <= now() AND \
             (($1 = 'dry_run' AND dry_run = TRUE) OR \
              ($1 = 'purge' AND dry_run = FALSE AND purge_enabled = TRUE)))) \
         ORDER BY deadline_at, created_at, job_id LIMIT 1 FOR UPDATE SKIP LOCKED",
    )
    .bind(mode.as_str())
    .bind(personal_only)
    .bind(shared_only)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        tx.commit().await?;
        return Ok(None);
    };
    let job_id: String = row.try_get("job_id")?;
    let row = sqlx::query(
        "UPDATE cloud_retention_jobs SET state='leased', lease_owner=$1, \
             lease_expires_at=now()+interval '5 minutes', attempt_count=attempt_count+1, updated_at=now() \
        WHERE job_id=$2 AND ($4=FALSE OR scope_kind='personal') \
           AND ($5=FALSE OR scope_kind='shared_organisation') AND \
           (($3 = 'dry_run' AND state='ready' AND dry_run=TRUE) OR \
            ($3 = 'purge' AND state='ready' AND dry_run=FALSE AND purge_enabled=TRUE) OR \
            (state='leased' AND lease_expires_at <= now() AND \
             (($3 = 'dry_run' AND dry_run=TRUE) OR \
              ($3 = 'purge' AND dry_run=FALSE AND purge_enabled=TRUE)))) \
         RETURNING scope_kind, subject_user_id, organisation_id, expected_coverage_revision, expected_scope_generation, \
                   extract(epoch FROM eligibility_loss_at)::bigint AS eligibility_loss_epoch, dry_run, \
                   extract(epoch from deadline_at)::bigint AS deadline_epoch, \
                   notice_event_key, extract(epoch from notice_evidence_at)::bigint AS notice_evidence_epoch",
    )
    .bind(worker_id)
    .bind(&job_id)
    .bind(mode.as_str())
    .bind(personal_only)
    .bind(shared_only)
    .fetch_one(&mut *tx)
    .await?;
    let lease = RetentionLease {
        job_id,
        scope_kind: row.try_get("scope_kind")?,
        subject_user_id: row.try_get("subject_user_id")?,
        organisation_id: row.try_get("organisation_id")?,
        expected_coverage_revision: row.try_get("expected_coverage_revision")?,
        expected_scope_generation: row.try_get("expected_scope_generation")?,
        eligibility_loss_epoch: row.try_get("eligibility_loss_epoch")?,
        dry_run: row.try_get("dry_run")?,
        deadline_epoch: row.try_get("deadline_epoch")?,
        notice_event_key: row.try_get("notice_event_key")?,
        notice_evidence_epoch: row.try_get("notice_evidence_epoch")?,
        worker_id: worker_id.to_owned(),
    };
    tx.commit().await?;
    Ok(Some(lease))
}

/// Run one bounded dry-run or purge batch.  The purge mode is unreachable until the durable job
/// has an explicit switch, a notice timestamp and a passed deadline; a caller cannot turn a dry
/// run into deletion by changing only the process argument.
pub async fn run_once(
    pool: &PgPool,
    worker_id: &str,
    mode: RetentionMode,
) -> RetentionResult<Option<RetentionRun>> {
    run_once_with_scope(pool, worker_id, mode, false, false).await
}

/// Production entry point: it can only claim personal dry-run jobs. The mode and scope are fixed
/// here so runtime wiring cannot accidentally select the dormant purge branch.
pub async fn run_dry_run_once(
    pool: &PgPool,
    worker_id: &str,
) -> RetentionResult<Option<RetentionRun>> {
    run_once_with_scope(pool, worker_id, RetentionMode::DryRun, true, false).await
}

/// Run one non-destructive dry-run batch for a qualified shared organisation scope.
/// Orphaned organisations remain held and cannot be claimed by this entry point.
pub async fn run_shared_dry_run_once(
    pool: &PgPool,
    worker_id: &str,
) -> RetentionResult<Option<RetentionRun>> {
    run_once_with_scope(pool, worker_id, RetentionMode::DryRun, false, true).await
}

async fn run_once_with_scope(
    pool: &PgPool,
    worker_id: &str,
    mode: RetentionMode,
    personal_only: bool,
    shared_only: bool,
) -> RetentionResult<Option<RetentionRun>> {
    let Some(lease) = claim_due(pool, worker_id, mode, personal_only, shared_only).await? else {
        return Ok(None);
    };
    let mut tx = pool.begin().await?;
    // Collection completion locks the coordinator before publishing the coverage head. Keep
    // that order here, then retain the existing head-before-job order used by candidate
    // reconciliation.
    if mode == RetentionMode::DryRun && personal_only {
        if let Some(subject) = lease.subject_user_id.as_deref() {
            sqlx::query_scalar::<_, String>(
                "SELECT beneficiary_id FROM cloud_coverage_coordinators WHERE beneficiary_id=$1 FOR SHARE",
            )
            .bind(subject)
            .fetch_optional(&mut *tx)
            .await?;
        }
    }
    let locked_coverage_revision = if mode == RetentionMode::DryRun && personal_only {
        match lease.subject_user_id.as_deref() {
            Some(subject) => sqlx::query_scalar::<_, Option<i64>>(
                "SELECT current_revision FROM cloud_coverage_heads WHERE beneficiary_id=$1 FOR SHARE",
            )
            .bind(subject)
            .fetch_optional(&mut *tx)
            .await?
            .flatten(),
            None => None,
        }
    } else {
        None
    };
    if mode == RetentionMode::DryRun && lease.scope_kind == ScopeKind::SharedOrganisation.as_str() {
        lock_shared_scope_rows(&mut tx, &lease).await?;
    }
    // Keep the candidate writer's lock order: coverage head, exact notice, owner, then job. The
    // owner lock prevents new personal resources appearing between the inventory check and the
    // dry-run result, while the notice lock prevents cancellation after evidence was checked.
    let (notice_available, owner_exists) = if mode == RetentionMode::DryRun && personal_only {
        let notice_available = match lease.subject_user_id.as_deref() {
            Some(subject) => lock_matching_notice(&mut tx, &lease, subject).await?,
            None => false,
        };
        let owner_exists = match lease.subject_user_id.as_deref() {
            Some(subject) => {
                sqlx::query_scalar::<_, String>("SELECT id FROM users WHERE id=$1 FOR UPDATE")
                    .bind(subject)
                    .fetch_optional(&mut *tx)
                    .await?
                    .is_some()
            }
            None => false,
        };
        (notice_available, owner_exists)
    } else {
        (false, false)
    };
    let lease_state = sqlx::query(
        "SELECT state, lease_owner FROM cloud_retention_jobs WHERE job_id=$1 FOR UPDATE",
    )
    .bind(&lease.job_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(lease_state) = lease_state else {
        return Err(RetentionError::LeaseLost);
    };
    let state: String = lease_state.try_get("state")?;
    let owner: Option<String> = lease_state.try_get("lease_owner")?;
    if state == RetentionState::Cancelled.as_str() {
        tx.commit().await?;
        return Ok(Some(RetentionRun {
            job_id: lease.job_id,
            state: RetentionState::Cancelled,
            dry_run: lease.dry_run,
            processed_items: 0,
            held_items: 0,
        }));
    }
    if state != RetentionState::Leased.as_str() || owner.as_deref() != Some(&lease.worker_id) {
        return Err(RetentionError::LeaseLost);
    }
    if mode == RetentionMode::DryRun && personal_only {
        let authority_snapshot = match lease.subject_user_id.as_deref() {
            Some(subject) => {
                let now_epoch: i64 = sqlx::query_scalar("SELECT extract(epoch FROM now())::bigint")
                    .fetch_one(&mut *tx)
                    .await?;
                Some(
                    match crate::cloud_coverage_authority::load_current_in_transaction(
                        &mut tx, subject, now_epoch,
                    )
                    .await
                    {
                        Ok(state) => state,
                        Err(crate::cloud_coverage_authority::AuthorityError::Database(error))
                        | Err(crate::cloud_coverage_authority::AuthorityError::Store(
                            StoreError::Database(error),
                        )) => return Err(RetentionError::Database(error)),
                        Err(_) => crate::cloud_coverage_authority::AuthorityState::Stale,
                    },
                )
            }
            None => None,
        };
        if let Some(reason) = validate_dry_run_evidence(
            &mut tx,
            &lease,
            authority_snapshot,
            locked_coverage_revision,
            notice_available,
            owner_exists,
        )
        .await?
        {
            set_job_hold(&mut tx, &lease, reason).await?;
            tx.commit().await?;
            return Ok(Some(RetentionRun {
                job_id: lease.job_id,
                state: RetentionState::Held,
                dry_run: true,
                processed_items: 0,
                held_items: 0,
            }));
        }
        if let Some(reason) = validate_scope_inventory(&mut tx, &lease).await? {
            set_job_hold(&mut tx, &lease, reason).await?;
            tx.commit().await?;
            return Ok(Some(RetentionRun {
                job_id: lease.job_id,
                state: RetentionState::Held,
                dry_run: true,
                processed_items: 0,
                held_items: 0,
            }));
        }
    } else if let Some(expected) = lease.expected_coverage_revision {
        let current: Option<i64> = sqlx::query_scalar(
            "SELECT current_revision FROM cloud_coverage_heads WHERE beneficiary_id=$1",
        )
        .bind(lease.subject_user_id.as_deref())
        .fetch_optional(&mut *tx)
        .await?;
        if current != Some(expected) {
            set_job_hold(&mut tx, &lease, "coverage_revision_changed").await?;
            tx.commit().await?;
            return Ok(Some(RetentionRun {
                job_id: lease.job_id,
                state: RetentionState::Held,
                dry_run: lease.dry_run,
                processed_items: 0,
                held_items: 0,
            }));
        }
    }

    let items = sqlx::query(
        "SELECT resource_kind, resource_id, ownership_kind, expected_created_at, expected_revision \
         FROM cloud_retention_scope_items WHERE job_id=$1 AND state='planned' \
         ORDER BY resource_kind, resource_id LIMIT $2 FOR UPDATE",
    )
    .bind(&lease.job_id)
    .bind(BATCH_SIZE)
    .fetch_all(&mut *tx)
    .await?;
    if mode == RetentionMode::DryRun && lease.scope_kind == ScopeKind::SharedOrganisation.as_str() {
        if let Some(reason) = validate_shared_dry_run(&mut tx, &lease).await? {
            set_job_hold(&mut tx, &lease, reason).await?;
            sqlx::query(
                "UPDATE cloud_retention_scope_items SET state='held',hold_code=$2,updated_at=now() \
                 WHERE job_id=$1 AND state='planned'",
            )
            .bind(&lease.job_id)
            .bind(reason)
            .execute(&mut *tx)
            .await?;
            let held: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM cloud_retention_scope_items WHERE job_id=$1 AND state='held'",
            )
            .bind(&lease.job_id)
            .fetch_one(&mut *tx)
            .await?;
            tx.commit().await?;
            return Ok(Some(RetentionRun {
                job_id: lease.job_id,
                state: RetentionState::Held,
                dry_run: true,
                processed_items: 0,
                held_items: held as usize,
            }));
        }
        for item in &items {
            let kind: String = item.try_get("resource_kind")?;
            let id: String = item.try_get("resource_id")?;
            let created: i64 = item.try_get("expected_created_at")?;
            let revision: Option<i64> = item.try_get("expected_revision")?;
            let reason =
                validate_shared_resource(&mut tx, &lease, &kind, &id, created, revision).await?;
            if let Some(reason) = reason {
                sqlx::query(
                    "UPDATE cloud_retention_scope_items SET state='held',hold_code=$4,updated_at=now() \
                     WHERE job_id=$1 AND resource_kind=$2 AND resource_id=$3 AND state='planned'",
                )
                .bind(&lease.job_id)
                .bind(&kind)
                .bind(&id)
                .bind(reason)
                .execute(&mut *tx)
                .await?;
                set_job_hold(&mut tx, &lease, reason).await?;
                tx.commit().await?;
                return Ok(Some(RetentionRun {
                    job_id: lease.job_id,
                    state: RetentionState::Held,
                    dry_run: true,
                    processed_items: 0,
                    held_items: 1,
                }));
            }
            sqlx::query(
                "UPDATE cloud_retention_scope_items SET state='dry_run',updated_at=now() \
                 WHERE job_id=$1 AND resource_kind=$2 AND resource_id=$3 AND state='planned'",
            )
            .bind(&lease.job_id)
            .bind(&kind)
            .bind(&id)
            .execute(&mut *tx)
            .await?;
        }
        let remaining: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM cloud_retention_scope_items WHERE job_id=$1 AND state='planned'",
        )
        .bind(&lease.job_id)
        .fetch_one(&mut *tx)
        .await?;
        if remaining > 0 {
            clear_lease(&mut tx, &lease, RetentionState::Ready).await?;
        } else {
            clear_lease(&mut tx, &lease, RetentionState::DryRun).await?;
        }
        tx.commit().await?;
        return Ok(Some(RetentionRun {
            job_id: lease.job_id,
            state: if remaining > 0 {
                RetentionState::Ready
            } else {
                RetentionState::DryRun
            },
            dry_run: true,
            processed_items: items.len(),
            held_items: 0,
        }));
    }
    if mode == RetentionMode::DryRun {
        let mut processed = 0;
        let mut hold_reason = None;
        for item in &items {
            let resource_kind: String = item.try_get("resource_kind")?;
            let resource_id: String = item.try_get("resource_id")?;
            let ownership_kind: String = item.try_get("ownership_kind")?;
            let expected_created_at: i64 = item.try_get("expected_created_at")?;
            let expected_revision: Option<i64> = item.try_get("expected_revision")?;
            let reason = validate_dry_run_resource(
                &mut tx,
                &lease,
                &resource_kind,
                &resource_id,
                &ownership_kind,
                expected_created_at,
                expected_revision,
            )
            .await?;
            if let Some(reason) = reason {
                sqlx::query(
                    "UPDATE cloud_retention_scope_items SET state='held', hold_code=$4, updated_at=now() \
                     WHERE job_id=$1 AND resource_kind=$2 AND resource_id=$3 AND state='planned'",
                )
                .bind(&lease.job_id)
                .bind(&resource_kind)
                .bind(&resource_id)
                .bind(reason)
                .execute(&mut *tx)
                .await?;
                hold_reason.get_or_insert(reason);
            } else {
                sqlx::query(
                    "UPDATE cloud_retention_scope_items SET state='dry_run', updated_at=now() \
                     WHERE job_id=$1 AND resource_kind=$2 AND resource_id=$3 AND state='planned'",
                )
                .bind(&lease.job_id)
                .bind(&resource_kind)
                .bind(&resource_id)
                .execute(&mut *tx)
                .await?;
                processed += 1;
            }
        }
        let total_held: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM cloud_retention_scope_items WHERE job_id=$1 AND state='held'",
        )
        .bind(&lease.job_id)
        .fetch_one(&mut *tx)
        .await?;
        let remaining: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM cloud_retention_scope_items WHERE job_id=$1 AND state='planned'",
        )
        .bind(&lease.job_id)
        .fetch_one(&mut *tx)
        .await?;
        let next_state = if let Some(reason) = hold_reason {
            set_job_hold(&mut tx, &lease, reason).await?;
            RetentionState::Held
        } else {
            let state = if remaining > 0 {
                RetentionState::Ready
            } else {
                RetentionState::DryRun
            };
            clear_lease(&mut tx, &lease, state).await?;
            state
        };
        tx.commit().await?;
        return Ok(Some(RetentionRun {
            job_id: lease.job_id,
            state: next_state,
            dry_run: true,
            processed_items: processed,
            held_items: total_held as usize,
        }));
    }

    let subject = lease
        .subject_user_id
        .as_deref()
        .ok_or(RetentionError::Held("shared scope is not deletable"))?;
    let mut processed = 0;
    let mut held = 0;
    let mut hold_reason = None;
    for item in &items {
        let resource_kind: String = item.try_get("resource_kind")?;
        let resource_id: String = item.try_get("resource_id")?;
        let ownership_kind: String = item.try_get("ownership_kind")?;
        let expected_created_at_epoch: i64 = item.try_get("expected_created_at")?;
        let expected_revision: Option<i64> = item.try_get("expected_revision")?;
        if lease.scope_kind != ScopeKind::Personal.as_str() || ownership_kind != "personal" {
            sqlx::query(
                "UPDATE cloud_retention_scope_items SET state='held', hold_code='shared_resource', updated_at=now() \
                 WHERE job_id=$1 AND resource_kind=$2 AND resource_id=$3",
            )
            .bind(&lease.job_id)
            .bind(&resource_kind)
            .bind(&resource_id)
            .execute(&mut *tx)
            .await?;
            held += 1;
            hold_reason = Some("shared_resource");
            continue;
        }
        let deleted = match resource_kind.as_str() {
            "project" => {
                let current = sqlx::query(
                    "SELECT owner_id, org_id, (extract(epoch from created_at) * 1000000)::bigint AS created_epoch \
                     FROM projects WHERE id=$1 FOR UPDATE",
                )
                    .bind(&resource_id)
                    .fetch_optional(&mut *tx)
                    .await?;
                match current {
                    None => 0,
                    Some(current) => {
                        let owner_id: String = current.try_get("owner_id")?;
                        let org_id: Option<String> = current.try_get("org_id")?;
                        let created_epoch: i64 = current.try_get("created_epoch")?;
                        if owner_id != subject
                            || org_id.is_some()
                            || created_epoch != expected_created_at_epoch
                        {
                            sqlx::query(
                            "UPDATE cloud_retention_scope_items SET state='held', hold_code='resource_identity_changed', updated_at=now() \
                             WHERE job_id=$1 AND resource_kind=$2 AND resource_id=$3",
                        )
                        .bind(&lease.job_id)
                        .bind(&resource_kind)
                        .bind(&resource_id)
                        .execute(&mut *tx)
                        .await?;
                            held += 1;
                            hold_reason = Some("resource_identity_changed");
                            continue;
                        }
                        sqlx::query("DELETE FROM projects WHERE id=$1")
                            .bind(&resource_id)
                            .execute(&mut *tx)
                            .await?
                            .rows_affected()
                    }
                }
            }
            "environment" => {
                let current = sqlx::query(
                    "SELECT e.revision, p.owner_id, p.org_id, \
                            (extract(epoch from e.created_at) * 1000000)::bigint AS created_epoch \
                     FROM environments e JOIN projects p ON p.id=e.project_id \
                     WHERE e.id=$1 FOR UPDATE",
                )
                .bind(&resource_id)
                .fetch_optional(&mut *tx)
                .await?;
                match current {
                    None => 0,
                    Some(current) => {
                        let revision: i64 = current.try_get("revision")?;
                        let owner_id: String = current.try_get("owner_id")?;
                        let org_id: Option<String> = current.try_get("org_id")?;
                        let created_epoch: i64 = current.try_get("created_epoch")?;
                        if owner_id != subject
                            || org_id.is_some()
                            || expected_revision.is_some_and(|expected| expected != revision)
                            || created_epoch != expected_created_at_epoch
                        {
                            sqlx::query(
                            "UPDATE cloud_retention_scope_items SET state='held', hold_code='resource_identity_changed', updated_at=now() \
                             WHERE job_id=$1 AND resource_kind=$2 AND resource_id=$3",
                        )
                        .bind(&lease.job_id)
                        .bind(&resource_kind)
                        .bind(&resource_id)
                        .execute(&mut *tx)
                            .await?;
                            held += 1;
                            hold_reason = Some("resource_identity_changed");
                            continue;
                        }
                        sqlx::query("DELETE FROM environments WHERE id=$1")
                            .bind(&resource_id)
                            .execute(&mut *tx)
                            .await?
                            .rows_affected()
                    }
                }
            }
            "share" => {
                let current = sqlx::query(
                    "SELECT created_by, (extract(epoch from created_at) * 1000000)::bigint AS created_epoch \
                     FROM share_links WHERE id=$1 FOR UPDATE",
                )
                .bind(&resource_id)
                .fetch_optional(&mut *tx)
                .await?;
                match current {
                    None => 0,
                    Some(current) => {
                        let created_by: String = current.try_get("created_by")?;
                        let created_epoch: i64 = current.try_get("created_epoch")?;
                        if created_by != subject || created_epoch != expected_created_at_epoch {
                            sqlx::query(
                            "UPDATE cloud_retention_scope_items SET state='held', hold_code='resource_identity_changed', updated_at=now() \
                             WHERE job_id=$1 AND resource_kind=$2 AND resource_id=$3",
                        )
                        .bind(&lease.job_id)
                        .bind(&resource_kind)
                        .bind(&resource_id)
                        .execute(&mut *tx)
                        .await?;
                            held += 1;
                            hold_reason = Some("resource_identity_changed");
                            continue;
                        }
                        sqlx::query("DELETE FROM share_links WHERE id=$1")
                            .bind(&resource_id)
                            .execute(&mut *tx)
                            .await?
                            .rows_affected()
                    }
                }
            }
            _ => return Err(RetentionError::InvalidInput("resource_kind")),
        };
        let tombstone = serde_json::json!({
            "resource_kind": resource_kind,
            "resource_id": resource_id,
            "ownership_kind": ownership_kind,
            "expected_owner_id": subject,
            "expected_created_at": expected_created_at_epoch,
            "expected_revision": expected_revision,
            "already_absent": deleted == 0,
        });
        let receipt_id = format!("receipt:{}", Uuid::new_v4());
        sqlx::query(
            "INSERT INTO cloud_retention_receipts \
             (receipt_id, job_id, resource_kind, resource_id, action, tombstone) \
             VALUES ($1,$2,$3,$4,'deleted',$5) ON CONFLICT (job_id, resource_kind, resource_id, action) DO NOTHING",
        )
        .bind(&receipt_id)
        .bind(&lease.job_id)
        .bind(&resource_kind)
        .bind(&resource_id)
        .bind(&tombstone)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO cloud_retention_tombstone_journal \
             (journal_id, job_id, resource_kind, resource_id, ownership_kind, expected_owner_id, \
              expected_created_at, expected_revision, action, tombstone) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,'deleted',$9) \
             ON CONFLICT (job_id, resource_kind, resource_id, action) DO NOTHING",
        )
        .bind(format!("tombstone:{}", Uuid::new_v4()))
        .bind(&lease.job_id)
        .bind(&resource_kind)
        .bind(&resource_id)
        .bind(&ownership_kind)
        .bind(subject)
        .bind(expected_created_at_epoch)
        .bind(expected_revision)
        .bind(&tombstone)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE cloud_retention_scope_items SET state='deleted', updated_at=now() \
             WHERE job_id=$1 AND resource_kind=$2 AND resource_id=$3",
        )
        .bind(&lease.job_id)
        .bind(&resource_kind)
        .bind(&resource_id)
        .execute(&mut *tx)
        .await?;
        processed += 1;
    }
    let next_state = if held > 0 {
        RetentionState::Held
    } else {
        let remaining: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM cloud_retention_scope_items WHERE job_id=$1 AND state='planned'",
        )
        .bind(&lease.job_id)
        .fetch_one(&mut *tx)
        .await?;
        if remaining == 0 {
            RetentionState::Completed
        } else {
            RetentionState::Ready
        }
    };
    if held > 0 {
        set_job_hold(
            &mut tx,
            &lease,
            hold_reason.unwrap_or("retention_scope_held"),
        )
        .await?;
    } else {
        clear_lease(&mut tx, &lease, next_state).await?;
    }
    tx.commit().await?;
    Ok(Some(RetentionRun {
        job_id: lease.job_id,
        state: next_state,
        dry_run: false,
        processed_items: processed,
        held_items: held,
    }))
}

async fn lock_matching_notice(
    tx: &mut Transaction<'_, Postgres>,
    lease: &RetentionLease,
    subject: &str,
) -> RetentionResult<bool> {
    let Some(export_start) = lease.deadline_epoch.checked_sub(EXPORT_WINDOW_SECONDS) else {
        return Ok(false);
    };
    if lease.notice_evidence_epoch != Some(export_start) {
        return Ok(false);
    }
    let notice = sqlx::query_scalar::<_, String>(
        "SELECT notice_id FROM cloud_notice_outbox \
         WHERE recipient_user_id=$1 AND event_key=$2 AND policy_key=$3 \
           AND kind='export_deadline' AND channel='in_app' \
           AND (status <> 'cancelled' OR cancelled_at >= to_timestamp($4)) \
           AND due_at=to_timestamp($4) AND created_at <= to_timestamp($4) \
           AND payload->>'deadline_epoch'=$5 \
         FOR SHARE",
    )
    .bind(subject)
    .bind(&lease.notice_event_key)
    .bind(crate::notifications::EXPORT_DEADLINE_POLICY_KEY)
    .bind(export_start)
    .bind(lease.deadline_epoch.to_string())
    .fetch_optional(&mut **tx)
    .await?;
    Ok(notice.is_some())
}

async fn validate_scope_inventory(
    tx: &mut Transaction<'_, Postgres>,
    lease: &RetentionLease,
) -> RetentionResult<Option<&'static str>> {
    let Some(subject) = lease.subject_user_id.as_deref() else {
        return Ok(Some("scope_owner_missing"));
    };

    let projects = sqlx::query_scalar::<_, String>(
        "SELECT id FROM projects WHERE owner_id=$1 AND org_id IS NULL \
         ORDER BY id COLLATE \"C\" LIMIT $2 FOR UPDATE",
    )
    .bind(subject)
    .bind(MAX_SCOPE_ITEMS + 1)
    .fetch_all(&mut **tx)
    .await?;
    let environments = sqlx::query_scalar::<_, String>(
        "SELECT e.id FROM environments e JOIN projects p ON p.id=e.project_id \
         WHERE p.owner_id=$1 AND p.org_id IS NULL \
         ORDER BY e.id COLLATE \"C\" LIMIT $2 FOR UPDATE OF e,p",
    )
    .bind(subject)
    .bind(MAX_SCOPE_ITEMS + 1)
    .fetch_all(&mut **tx)
    .await?;
    let shares = sqlx::query_scalar::<_, String>(
        "SELECT id FROM share_links WHERE created_by=$1 \
         ORDER BY id COLLATE \"C\" LIMIT $2 FOR UPDATE",
    )
    .bind(subject)
    .bind(MAX_SCOPE_ITEMS + 1)
    .fetch_all(&mut **tx)
    .await?;
    let max_scope_items = MAX_SCOPE_ITEMS as usize;
    if projects.len() + environments.len() + shares.len() > max_scope_items
        || projects.len() > max_scope_items
        || environments.len() > max_scope_items
        || shares.len() > max_scope_items
    {
        return Ok(Some("scope_exceeds_bound"));
    }

    let mut current = HashSet::with_capacity(projects.len() + environments.len() + shares.len());
    current.extend(projects.into_iter().map(|id| ("project".to_owned(), id)));
    current.extend(
        environments
            .into_iter()
            .map(|id| ("environment".to_owned(), id)),
    );
    current.extend(shares.into_iter().map(|id| ("share".to_owned(), id)));
    let rows = sqlx::query(
        "SELECT resource_kind, resource_id FROM cloud_retention_scope_items \
         WHERE job_id=$1 ORDER BY resource_kind, resource_id LIMIT $2",
    )
    .bind(&lease.job_id)
    .bind(MAX_SCOPE_ITEMS + 1)
    .fetch_all(&mut **tx)
    .await?;
    if rows.len() as i64 > MAX_SCOPE_ITEMS {
        return Ok(Some("scope_exceeds_bound"));
    }
    let mut recorded = HashSet::with_capacity(rows.len());
    for row in rows {
        recorded.insert((row.try_get("resource_kind")?, row.try_get("resource_id")?));
    }
    if current != recorded {
        return Ok(Some("scope_changed"));
    }
    Ok(None)
}

async fn validate_dry_run_evidence(
    tx: &mut Transaction<'_, Postgres>,
    lease: &RetentionLease,
    authority_snapshot: Option<crate::cloud_coverage_authority::AuthorityState>,
    locked_coverage_revision: Option<i64>,
    notice_available: bool,
    owner_exists: bool,
) -> RetentionResult<Option<&'static str>> {
    if lease.scope_kind != ScopeKind::Personal.as_str() {
        return Ok(Some("unsupported_scope"));
    }
    let Some(subject) = lease.subject_user_id.as_deref() else {
        return Ok(Some("scope_owner_missing"));
    };
    if !owner_exists {
        return Ok(Some("scope_owner_missing"));
    }
    let Some(expected_revision) = lease.expected_coverage_revision else {
        return Ok(Some("coverage_revision_missing"));
    };
    let Some(authority_snapshot) = authority_snapshot else {
        return Ok(Some("coverage_unavailable"));
    };
    let coverage = match authority_snapshot {
        crate::cloud_coverage_authority::AuthorityState::Current { loaded, .. } => loaded,
        crate::cloud_coverage_authority::AuthorityState::Missing
        | crate::cloud_coverage_authority::AuthorityState::Stale => {
            return Ok(Some("coverage_authority_unavailable"))
        }
    };
    if locked_coverage_revision != Some(expected_revision) || coverage.revision != expected_revision
    {
        return Ok(Some("coverage_revision_changed"));
    }
    let now_epoch: i64 = sqlx::query_scalar("SELECT extract(epoch from now())::bigint")
        .fetch_one(&mut **tx)
        .await?;
    let decision = match cloud_coverage::evaluate(&coverage.coverage, now_epoch) {
        Ok(decision) => decision,
        Err(_) => return Ok(Some("coverage_unavailable")),
    };
    if decision.state != CoverageState::Expired
        || decision.export_until != Some(lease.deadline_epoch)
    {
        return Ok(Some("coverage_episode_changed"));
    }
    let personal_state: Option<String> =
        sqlx::query_scalar("SELECT state FROM billing_personal_accounts WHERE user_id=$1")
            .bind(subject)
            .fetch_optional(&mut **tx)
            .await?;
    if personal_state.as_deref() == Some("refund_required") {
        return Ok(Some("eligibility_unavailable"));
    }
    if personal_state
        .as_deref()
        .is_some_and(|state| matches!(state, "pending" | "active" | "past_due"))
    {
        return Ok(Some("billing_state_active"));
    }
    let export_start = lease
        .deadline_epoch
        .checked_sub(EXPORT_WINDOW_SECONDS)
        .ok_or(RetentionError::InvalidInput("deadline_epoch"))?;
    if lease.notice_evidence_epoch != Some(export_start) {
        return Ok(Some("notice_evidence_missing"));
    }
    if !notice_available {
        return Ok(Some("notice_evidence_missing"));
    }
    Ok(None)
}

/// Lock the shared scope in the same order as candidate creation: participants, organisation,
/// memberships, then the retention job. The organisation lock serialises membership writers and
/// the generation triggers fence resource/grant changes during later validation.
async fn lock_shared_scope_rows(
    tx: &mut Transaction<'_, Postgres>,
    lease: &RetentionLease,
) -> RetentionResult<()> {
    let org_id = lease
        .organisation_id
        .as_deref()
        .ok_or(RetentionError::Held("organisation scope is missing"))?;
    let users: Vec<String> = sqlx::query_scalar(
        "SELECT user_id FROM organization_memberships WHERE org_id=$1 AND role IN ('owner','admin') \
         ORDER BY user_id COLLATE \"C\" LIMIT 257",
    )
    .bind(org_id)
    .fetch_all(&mut **tx)
    .await?;
    if users.len() > 256 {
        return Ok(());
    }
    let locked_users: Vec<String> = sqlx::query_scalar(
        "SELECT id FROM users WHERE id=ANY($1) ORDER BY id COLLATE \"C\" FOR UPDATE",
    )
    .bind(&users)
    .fetch_all(&mut **tx)
    .await?;
    if locked_users != users {
        return Err(RetentionError::Held("organisation membership changed"));
    }
    sqlx::query("SELECT id FROM organizations WHERE id=$1 FOR UPDATE")
        .bind(org_id)
        .fetch_optional(&mut **tx)
        .await?;
    sqlx::query(
        "SELECT user_id FROM organization_memberships WHERE org_id=$1 AND role IN ('owner','admin') \
         ORDER BY user_id COLLATE \"C\" FOR UPDATE",
    )
    .bind(org_id)
    .fetch_all(&mut **tx)
    .await?;
    Ok(())
}

async fn validate_shared_dry_run(
    tx: &mut Transaction<'_, Postgres>,
    lease: &RetentionLease,
) -> RetentionResult<Option<&'static str>> {
    if lease.scope_kind != ScopeKind::SharedOrganisation.as_str() {
        return Ok(Some("unsupported_scope"));
    }
    let Some(org_id) = lease.organisation_id.as_deref() else {
        return Ok(Some("organisation_scope_missing"));
    };
    let Some(expected_generation) = lease.expected_scope_generation else {
        return Ok(Some("scope_generation_missing"));
    };
    let org =
        sqlx::query("SELECT lifecycle_state, retention_generation FROM organizations WHERE id=$1")
            .bind(org_id)
            .fetch_optional(&mut **tx)
            .await?;
    let Some(org) = org else {
        return Ok(Some("organisation_missing"));
    };
    let lifecycle: String = org.try_get("lifecycle_state")?;
    let generation: i64 = org.try_get("retention_generation")?;
    if lifecycle != "active" {
        return Ok(Some("organisation_not_active"));
    }
    if generation != expected_generation {
        return Ok(Some("scope_generation_changed"));
    }
    let deletion_pending: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM organization_deletions WHERE org_id=$1 \
         AND state NOT IN ('cancelled','completed'))",
    )
    .bind(org_id)
    .fetch_one(&mut **tx)
    .await?;
    if deletion_pending {
        return Ok(Some("organisation_deletion_active"));
    }
    let transfer_pending: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM billing_transfer_intents WHERE state IN \
           ('awaiting_consent','pending','destination_prepared','destination_paid', \
            'source_adjustment_pending','source_restoration_pending','unknown') \
         AND (source_organization_id=$1 OR destination_organization_id=$1))",
    )
    .bind(org_id)
    .fetch_one(&mut **tx)
    .await?;
    if transfer_pending {
        return Ok(Some("ownership_transition_pending"));
    }
    let Some(expected_notice_epoch) = lease.deadline_epoch.checked_sub(EXPORT_WINDOW_SECONDS)
    else {
        return Ok(Some("notice_deadline_invalid"));
    };
    let Some(expected_loss) = lease.eligibility_loss_epoch else {
        return Ok(Some("eligibility_boundary_missing"));
    };
    if expected_notice_epoch < expected_loss
        || lease.notice_evidence_epoch != Some(expected_notice_epoch)
    {
        return Ok(Some("notice_evidence_missing"));
    }
    let members = sqlx::query(
        "SELECT user_id, role FROM organization_memberships WHERE org_id=$1 \
         AND role IN ('owner','admin') ORDER BY user_id COLLATE \"C\" LIMIT 257",
    )
    .bind(org_id)
    .fetch_all(&mut **tx)
    .await?;
    if members.is_empty() {
        return Ok(Some("owner_admin_missing"));
    }
    if members.len() > 256 {
        return Ok(Some("owner_admin_population_exceeds_bound"));
    }
    let notice_count: i64 = sqlx::query_scalar(
        "SELECT count(DISTINCT notice.recipient_user_id) FROM cloud_notice_outbox notice \
         JOIN organization_memberships membership ON membership.org_id=notice.scope_id \
           AND membership.user_id=notice.recipient_user_id AND membership.role IN ('owner','admin') \
         WHERE notice.scope_id=$1 AND notice.event_key=$2 AND notice.policy_key='shared-export-deadline-v1' \
           AND notice.kind='shared_export_deadline' AND notice.channel='in_app' \
           AND notice.due_at=to_timestamp($3) AND notice.created_at<=to_timestamp($3) \
           AND notice.status<>'cancelled'",
    )
    .bind(org_id)
    .bind(&lease.notice_event_key)
    .bind(expected_notice_epoch)
    .fetch_one(&mut **tx)
    .await?;
    if notice_count != members.len() as i64 {
        return Ok(Some("notice_evidence_incomplete"));
    }
    let reachable: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM cloud_verified_contacts contact \
         JOIN organization_memberships membership ON membership.user_id=contact.user_id \
         WHERE membership.org_id=$1 AND membership.role IN ('owner','admin') \
           AND contact.channel='email' AND contact.revoked_at IS NULL)",
    )
    .bind(org_id)
    .fetch_one(&mut **tx)
    .await?;
    if !reachable {
        return Ok(Some("no_verified_owner_contact"));
    }
    let now: i64 = sqlx::query_scalar("SELECT extract(epoch FROM now())::bigint")
        .fetch_one(&mut **tx)
        .await?;
    let mut latest_loss = None;
    for member in members {
        let user_id: String = member.try_get("user_id")?;
        let Some(loss_epoch) = shared_admin_loss_epoch(tx, &user_id, now).await? else {
            return Ok(Some("owner_admin_eligibility_unproven"));
        };
        latest_loss = Some(latest_loss.map_or(loss_epoch, |latest: i64| latest.max(loss_epoch)));
    }
    if latest_loss != Some(expected_loss) {
        return Ok(Some("eligibility_boundary_changed"));
    }
    validate_shared_scope_inventory(tx, lease).await
}

pub(crate) async fn shared_admin_loss_epoch(
    tx: &mut Transaction<'_, Postgres>,
    user_id: &str,
    now: i64,
) -> RetentionResult<Option<i64>> {
    Ok(match shared_admin_eligibility(tx, user_id, now).await? {
        SharedAdminEligibility::Lost(epoch) => Some(epoch),
        SharedAdminEligibility::StillEligible | SharedAdminEligibility::Unknown => None,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SharedAdminEligibility {
    StillEligible,
    Lost(i64),
    Unknown,
}

pub(crate) async fn shared_admin_eligibility(
    tx: &mut Transaction<'_, Postgres>,
    user_id: &str,
    now: i64,
) -> RetentionResult<SharedAdminEligibility> {
    let account = crate::personal_billing::load_account(tx, user_id)
        .await
        .map_err(|error| match error {
            crate::personal_billing::PersonalBillingError::Database(error) => {
                RetentionError::Database(error)
            }
            _ => RetentionError::Held("owner eligibility is corrupt"),
        })?;
    let coordinator: Option<String> = sqlx::query_scalar(
        "SELECT beneficiary_id FROM cloud_coverage_coordinators WHERE beneficiary_id=$1 FOR SHARE",
    )
    .bind(user_id)
    .fetch_optional(&mut **tx)
    .await?;
    let coverage = match crate::cloud_coverage_authority::load_current_in_transaction(
        tx, user_id, now,
    )
    .await
    {
        Ok(crate::cloud_coverage_authority::AuthorityState::Current { loaded, .. }) => {
            match crate::cloud_coverage::evaluate(&loaded.coverage, now) {
                Ok(decision) => crate::person_eligibility::CoverageInput::Decision(decision),
                Err(_) => crate::person_eligibility::CoverageInput::Unavailable,
            }
        }
        Ok(crate::cloud_coverage_authority::AuthorityState::Missing) => {
            crate::person_eligibility::CoverageInput::Missing
        }
        Ok(crate::cloud_coverage_authority::AuthorityState::Stale) => {
            crate::person_eligibility::CoverageInput::Unavailable
        }
        Err(crate::cloud_coverage_authority::AuthorityError::Database(error))
        | Err(crate::cloud_coverage_authority::AuthorityError::Store(StoreError::Database(
            error,
        ))) => {
            return Err(RetentionError::Database(error));
        }
        Err(_) => crate::person_eligibility::CoverageInput::Unavailable,
    };
    let _ = coordinator;
    let view =
        crate::person_eligibility::evaluate_input(&crate::person_eligibility::EligibilityInput {
            deployment_mode: crate::config::DeploymentMode::Cloud,
            billing_available: true,
            account_initialized: true,
            personal_billing_state: account.map(|account| account.state),
            coverage,
        });
    Ok(match view.state {
        crate::person_eligibility::EligibilityState::Paid
        | crate::person_eligibility::EligibilityState::RenewalRecovery => {
            SharedAdminEligibility::StillEligible
        }
        crate::person_eligibility::EligibilityState::ExportOnly
        | crate::person_eligibility::EligibilityState::Expired => view
            .export_until_epoch
            .and_then(|deadline| deadline.checked_sub(EXPORT_WINDOW_SECONDS))
            .map_or(
                SharedAdminEligibility::Unknown,
                SharedAdminEligibility::Lost,
            ),
        _ => SharedAdminEligibility::Unknown,
    })
}

async fn validate_shared_scope_inventory(
    tx: &mut Transaction<'_, Postgres>,
    lease: &RetentionLease,
) -> RetentionResult<Option<&'static str>> {
    let org_id = lease
        .organisation_id
        .as_deref()
        .ok_or(RetentionError::Held("organisation scope is missing"))?;
    let mut expected = HashSet::new();
    let projects = sqlx::query(
        "SELECT id, (extract(epoch FROM created_at)*1000000)::bigint AS created_at \
         FROM projects WHERE org_id=$1 ORDER BY id COLLATE \"C\" LIMIT 4097",
    )
    .bind(org_id)
    .fetch_all(&mut **tx)
    .await?;
    if projects.len() > 4096 {
        return Ok(Some("scope_exceeds_bound"));
    }
    for row in projects {
        expected.insert((
            "project".to_owned(),
            row.try_get::<String, _>("id")?,
            row.try_get::<i64, _>("created_at")?,
            None,
        ));
    }
    let environments = sqlx::query(
        "SELECT e.id,e.revision,(extract(epoch FROM e.created_at)*1000000)::bigint AS created_at \
         FROM environments e JOIN projects p ON p.id=e.project_id WHERE p.org_id=$1 \
         ORDER BY e.id COLLATE \"C\" LIMIT 4097",
    )
    .bind(org_id)
    .fetch_all(&mut **tx)
    .await?;
    if expected.len() + environments.len() > 4096 {
        return Ok(Some("scope_exceeds_bound"));
    }
    for row in environments {
        expected.insert((
            "environment".to_owned(),
            row.try_get::<String, _>("id")?,
            row.try_get::<i64, _>("created_at")?,
            Some(row.try_get::<i64, _>("revision")?),
        ));
    }
    let grants = sqlx::query(
        "SELECT g.env_id,g.user_id,(extract(epoch FROM g.created_at)*1000000)::bigint AS created_at \
         FROM environment_grants g JOIN environments e ON e.id=g.env_id \
         JOIN projects p ON p.id=e.project_id WHERE p.org_id=$1 \
         ORDER BY g.env_id COLLATE \"C\",g.user_id COLLATE \"C\" LIMIT 4097",
    )
    .bind(org_id)
    .fetch_all(&mut **tx)
    .await?;
    if expected.len() + grants.len() > 4096 {
        return Ok(Some("scope_exceeds_bound"));
    }
    for row in grants {
        let env_id: String = row.try_get("env_id")?;
        let user_id: String = row.try_get("user_id")?;
        expected.insert((
            "grant".to_owned(),
            serde_json::json!([env_id, user_id]).to_string(),
            row.try_get::<i64, _>("created_at")?,
            None,
        ));
    }
    let tokens = sqlx::query(
        "SELECT m.id,m.env_id,(extract(epoch FROM m.created_at)*1000000)::bigint AS created_at \
         FROM machine_tokens m JOIN environments e ON e.id=m.env_id \
         JOIN projects p ON p.id=e.project_id WHERE p.org_id=$1 \
         ORDER BY m.id COLLATE \"C\" LIMIT 4097",
    )
    .bind(org_id)
    .fetch_all(&mut **tx)
    .await?;
    if expected.len() + tokens.len() > 4096 {
        return Ok(Some("scope_exceeds_bound"));
    }
    for row in tokens {
        let token_id: String = row.try_get("id")?;
        let env_id: String = row.try_get("env_id")?;
        expected.insert((
            "machine_token".to_owned(),
            serde_json::json!([token_id, env_id]).to_string(),
            row.try_get::<i64, _>("created_at")?,
            None,
        ));
    }
    let rows = sqlx::query(
        "SELECT resource_kind,resource_id,expected_created_at,expected_revision \
         FROM cloud_retention_scope_items WHERE job_id=$1 ORDER BY resource_kind,resource_id LIMIT 4097",
    )
    .bind(&lease.job_id)
    .fetch_all(&mut **tx)
    .await?;
    if rows.len() > 4096 {
        return Ok(Some("scope_exceeds_bound"));
    }
    let mut actual = HashSet::new();
    for row in rows {
        actual.insert((
            row.try_get::<String, _>("resource_kind")?,
            row.try_get::<String, _>("resource_id")?,
            row.try_get::<i64, _>("expected_created_at")?,
            row.try_get::<Option<i64>, _>("expected_revision")?,
        ));
    }
    if expected != actual {
        return Ok(Some("scope_changed"));
    }
    Ok(None)
}

async fn validate_shared_resource(
    tx: &mut Transaction<'_, Postgres>,
    lease: &RetentionLease,
    kind: &str,
    id: &str,
    created: i64,
    revision: Option<i64>,
) -> RetentionResult<Option<&'static str>> {
    let org_id = lease
        .organisation_id
        .as_deref()
        .ok_or(RetentionError::Held("organisation scope is missing"))?;
    let found =
        match kind {
            "project" => {
                sqlx::query_scalar::<_, bool>(
                    "SELECT EXISTS(SELECT 1 FROM projects WHERE id=$1 AND org_id=$2 \
             AND (extract(epoch FROM created_at)*1000000)::bigint=$3)",
                )
                .bind(id)
                .bind(org_id)
                .bind(created)
                .fetch_one(&mut **tx)
                .await?
            }
            "environment" => sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM environments e JOIN projects p ON p.id=e.project_id \
             WHERE e.id=$1 AND p.org_id=$2 AND e.revision=$3 \
               AND (extract(epoch FROM e.created_at)*1000000)::bigint=$4)",
            )
            .bind(id)
            .bind(org_id)
            .bind(revision)
            .bind(created)
            .fetch_one(&mut **tx)
            .await?,
            "grant" => {
                sqlx::query_scalar::<_, bool>(
                    "SELECT EXISTS(SELECT 1 FROM environment_grants g \
                 JOIN environments e ON e.id=g.env_id JOIN projects p ON p.id=e.project_id \
                 WHERE p.org_id=$2 AND jsonb_build_array(g.env_id,g.user_id)=$1::jsonb \
                   AND (extract(epoch FROM g.created_at)*1000000)::bigint=$3)",
                )
                .bind(id)
                .bind(org_id)
                .bind(created)
                .fetch_one(&mut **tx)
                .await?
            }
            "machine_token" => {
                sqlx::query_scalar::<_, bool>(
                    "SELECT EXISTS(SELECT 1 FROM machine_tokens m \
                 JOIN environments e ON e.id=m.env_id JOIN projects p ON p.id=e.project_id \
                 WHERE p.org_id=$2 AND jsonb_build_array(m.id,m.env_id)=$1::jsonb \
                   AND (extract(epoch FROM m.created_at)*1000000)::bigint=$3)",
                )
                .bind(id)
                .bind(org_id)
                .bind(created)
                .fetch_one(&mut **tx)
                .await?
            }
            _ => return Ok(Some("unsupported_resource_kind")),
        };
    Ok((!found).then_some("resource_identity_changed"))
}

async fn validate_dry_run_resource(
    tx: &mut Transaction<'_, Postgres>,
    lease: &RetentionLease,
    resource_kind: &str,
    resource_id: &str,
    ownership_kind: &str,
    expected_created_at: i64,
    expected_revision: Option<i64>,
) -> RetentionResult<Option<&'static str>> {
    if ownership_kind != "personal" {
        return Ok(Some("shared_resource"));
    }
    let Some(subject) = lease.subject_user_id.as_deref() else {
        return Ok(Some("scope_owner_missing"));
    };
    let reason = match resource_kind {
        "project" => {
            let row = sqlx::query(
                "SELECT p.owner_id, p.org_id, \
                        (extract(epoch from p.created_at) * 1000000)::bigint AS created_epoch, \
                        EXISTS (SELECT 1 FROM environments e JOIN environment_grants g ON g.env_id=e.id \
                                WHERE e.project_id=p.id AND g.user_id<>p.owner_id) AS shared \
                 FROM projects p WHERE p.id=$1 FOR UPDATE OF p",
            )
            .bind(resource_id)
            .fetch_optional(&mut **tx)
            .await?;
            match row {
                None => Some("resource_missing"),
                Some(row) => {
                    let owner: String = row.try_get("owner_id")?;
                    let organisation: Option<String> = row.try_get("org_id")?;
                    let created: i64 = row.try_get("created_epoch")?;
                    let shared: bool = row.try_get("shared")?;
                    if owner != subject || organisation.is_some() || shared {
                        Some("shared_resource")
                    } else if created != expected_created_at {
                        Some("resource_identity_changed")
                    } else {
                        None
                    }
                }
            }
        }
        "environment" => {
            let row = sqlx::query(
                "SELECT e.revision, p.owner_id, p.org_id, \
                        (extract(epoch from e.created_at) * 1000000)::bigint AS created_epoch, \
                        EXISTS (SELECT 1 FROM environment_grants g WHERE g.env_id=e.id AND g.user_id<>p.owner_id) AS shared \
                 FROM environments e JOIN projects p ON p.id=e.project_id \
                 WHERE e.id=$1 FOR UPDATE OF e,p",
            )
            .bind(resource_id)
            .fetch_optional(&mut **tx)
            .await?;
            match row {
                None => Some("resource_missing"),
                Some(row) => {
                    let owner: String = row.try_get("owner_id")?;
                    let organisation: Option<String> = row.try_get("org_id")?;
                    let created: i64 = row.try_get("created_epoch")?;
                    let revision: i64 = row.try_get("revision")?;
                    let shared: bool = row.try_get("shared")?;
                    if owner != subject || organisation.is_some() || shared {
                        Some("shared_resource")
                    } else if created != expected_created_at
                        || expected_revision.is_some_and(|expected| expected != revision)
                    {
                        Some("resource_identity_changed")
                    } else {
                        None
                    }
                }
            }
        }
        "share" => {
            let row = sqlx::query(
                "SELECT created_by, share_class, max_views, view_count, revoked_at IS NOT NULL AS revoked, \
                        extract(epoch from expires_at)::bigint AS expires_epoch, \
                        extract(epoch from now())::bigint AS now_epoch, \
                        (extract(epoch from created_at) * 1000000)::bigint AS created_epoch \
                 FROM share_links WHERE id=$1 FOR UPDATE",
            )
            .bind(resource_id)
            .fetch_optional(&mut **tx)
            .await?;
            match row {
                None => Some("resource_missing"),
                Some(row) => {
                    let owner: String = row.try_get("created_by")?;
                    let share_class: String = row.try_get("share_class")?;
                    let max_views: i32 = row.try_get("max_views")?;
                    let view_count: i32 = row.try_get("view_count")?;
                    let revoked: bool = row.try_get("revoked")?;
                    let expires_at: Option<i64> = row.try_get("expires_epoch")?;
                    let now: i64 = row.try_get("now_epoch")?;
                    let created: i64 = row.try_get("created_epoch")?;
                    if owner != subject {
                        Some("resource_identity_changed")
                    } else if let Some(reason) = share_lifetime_hold(
                        &share_class,
                        expires_at,
                        max_views,
                        view_count,
                        revoked,
                        now,
                        lease.deadline_epoch,
                    ) {
                        Some(reason)
                    } else if created != expected_created_at {
                        Some("resource_identity_changed")
                    } else {
                        None
                    }
                }
            }
        }
        _ => return Err(RetentionError::InvalidInput("resource_kind")),
    };
    Ok(reason)
}

async fn set_job_hold(
    tx: &mut Transaction<'_, Postgres>,
    lease: &RetentionLease,
    reason: &str,
) -> RetentionResult<()> {
    let affected = sqlx::query(
        "UPDATE cloud_retention_jobs SET state='held', hold_code=$1, lease_owner=NULL, \
             lease_expires_at=NULL, updated_at=now() \
         WHERE job_id=$2 AND state='leased' AND lease_owner=$3",
    )
    .bind(reason)
    .bind(&lease.job_id)
    .bind(&lease.worker_id)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    if affected != 1 {
        return Err(RetentionError::LeaseLost);
    }
    Ok(())
}

async fn clear_lease(
    tx: &mut Transaction<'_, Postgres>,
    lease: &RetentionLease,
    state: RetentionState,
) -> RetentionResult<()> {
    let affected = sqlx::query(
        "UPDATE cloud_retention_jobs SET state=$1, lease_owner=NULL, lease_expires_at=NULL, \
             dry_run_completed_at=CASE WHEN $1='dry_run' THEN now() ELSE dry_run_completed_at END, \
             updated_at=now() \
         WHERE job_id=$2 AND state='leased' AND lease_owner=$3",
    )
    .bind(state.as_str())
    .bind(&lease.job_id)
    .bind(&lease.worker_id)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    if affected != 1 {
        return Err(RetentionError::LeaseLost);
    }
    Ok(())
}

pub fn new_job_id() -> String {
    format!("retention:{}", Uuid::new_v4())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn personal() -> RetentionIntent {
        RetentionIntent {
            job_id: "retention:job-1".into(),
            scope_kind: ScopeKind::Personal,
            scope_key: "user-1".into(),
            subject_user_id: Some("user-1".into()),
            organisation_id: None,
            eligibility_episode: "export-expiry:1".into(),
            deadline_epoch: 1_900_000_000,
            notice_event_key: "notice:export-expiry:user-1".into(),
            notice_evidence_epoch: Some(1_899_000_000),
            expected_coverage_revision: Some(4),
            expected_scope_generation: None,
            eligibility_loss_epoch: None,
        }
    }

    #[test]
    fn scope_requires_explicit_single_owner() {
        assert!(personal().validate().is_ok());
        let mut invalid = personal();
        invalid.organisation_id = Some("org-1".into());
        assert!(matches!(
            invalid.validate(),
            Err(RetentionError::InvalidInput("scope ownership"))
        ));
    }

    #[test]
    fn notice_evidence_cannot_follow_the_deadline() {
        let mut invalid = personal();
        invalid.notice_evidence_epoch = Some(invalid.deadline_epoch + 1);
        assert!(matches!(
            invalid.validate(),
            Err(RetentionError::InvalidInput("notice_evidence_epoch"))
        ));
    }

    #[test]
    fn purge_requires_a_non_dry_run_state() {
        assert_eq!(RetentionMode::DryRun.as_str(), "dry_run");
        assert_eq!(RetentionMode::Purge.as_str(), "purge");
        assert_eq!(RetentionState::Held.as_str(), "held");
    }

    #[test]
    fn share_lifetimes_keep_free_promises_and_end_paid_legacy_at_creator_deadline() {
        let deadline = 1_900_000_000;
        let now = deadline - 86_400;
        assert_eq!(
            share_lifetime_hold("free", Some(deadline - 1), 1, 0, false, now, deadline),
            None
        );
        assert_eq!(
            share_lifetime_hold("free", Some(deadline + 1), 1, 0, false, now, deadline),
            Some("free_share_promise_after_deadline")
        );
        for class in ["paid", "legacy"] {
            assert_eq!(
                share_lifetime_hold(class, Some(deadline + 1), 1, 0, false, now, deadline),
                None
            );
            assert_eq!(
                share_lifetime_hold(class, None, 1, 0, false, now, deadline),
                None
            );
        }
        for (class, expiry, views, revoked) in [
            ("free", Some(deadline + 1), 1, false),
            ("paid", Some(deadline + 1), 0, true),
            ("free", Some(now - 1), 0, false),
        ] {
            assert_eq!(
                share_lifetime_hold(class, expiry, 1, views, revoked, now, deadline),
                None
            );
        }
        assert_eq!(
            share_lifetime_hold("unknown", None, 1, 0, false, now, deadline),
            Some("share_lifetime_unproven")
        );
    }
}
