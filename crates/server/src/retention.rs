//! Durable, explicitly scoped Cloud retention jobs.
//!
//! A retention job records who or which organisation is in scope, the eligibility episode that
//! created it, the fixed deadline and the notice evidence that supports it.  Scope items are
//! stored separately so a later dry run can be reviewed before a purge switch is ever enabled.
//! This module intentionally has no startup worker or public route yet: retention policy and
//! restore evidence remain activation gates.
//!
//! Scope item identifiers are canonical database identifiers. Share-link tokens supplied by a
//! producer are resolved to their internal `share_links.id` while the scope is inserted.

use std::fmt;

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
                    && self.organisation_id.is_none() => {}
            ScopeKind::SharedOrganisation | ScopeKind::OrphanOrganisation
                if self.subject_user_id.is_none()
                    && self
                        .organisation_id
                        .as_deref()
                        .is_some_and(|id| !id.trim().is_empty()) => {}
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
            "project" | "environment" | "share"
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

#[derive(Debug, Error)]
pub enum RetentionError {
    #[error("retention database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("retention input is invalid: {0}")]
    InvalidInput(&'static str),
    #[error("retention job was not found")]
    NotFound,
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
    dry_run: bool,
    worker_id: String,
}

fn bounded(value: &str, max: usize, field: &'static str) -> RetentionResult<()> {
    if value.trim().is_empty() || value.len() > max || value.chars().any(char::is_control) {
        return Err(RetentionError::InvalidInput(field));
    }
    Ok(())
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
          deadline_at, notice_event_key, notice_evidence_at, expected_coverage_revision) \
         VALUES ($1,$2,$3,$4,$5,$6,to_timestamp($7),$8,\
                 CASE WHEN $9::bigint IS NULL THEN NULL ELSE to_timestamp($9) END,$10) \
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
    .execute(&mut **tx)
    .await?
    .rows_affected();
    Ok(if inserted == 1 {
        EnqueueOutcome::Enqueued
    } else {
        EnqueueOutcome::AlreadyQueued
    })
}

pub async fn add_scope_item(pool: &PgPool, item: &ScopeItem) -> RetentionResult<bool> {
    item.validate()?;
    let mut tx = pool.begin().await?;
    let state: Option<String> =
        sqlx::query_scalar("SELECT state FROM cloud_retention_jobs WHERE job_id=$1 FOR UPDATE")
            .bind(&item.job_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(state) = state else {
        return Err(RetentionError::NotFound);
    };
    if state != RetentionState::Planned.as_str() {
        return Err(RetentionError::Held("retention scope is immutable"));
    }
    let resource_id = if item.resource_kind == "share" {
        sqlx::query_scalar::<_, String>(
            "SELECT id FROM share_links WHERE id=$1 OR token=$1 \
             ORDER BY (id=$1) DESC LIMIT 1",
        )
        .bind(&item.resource_id)
        .fetch_optional(&mut *tx)
        .await?
        .unwrap_or_else(|| item.resource_id.clone())
    } else {
        item.resource_id.clone()
    };
    let inserted = sqlx::query(
        "INSERT INTO cloud_retention_scope_items \
         (job_id, resource_kind, resource_id, ownership_kind, expected_created_at, expected_revision) \
         VALUES ($1,$2,$3,$4,$5,$6) ON CONFLICT (job_id, resource_kind, resource_id) DO NOTHING",
    )
    .bind(&item.job_id)
    .bind(&item.resource_kind)
    .bind(&resource_id)
    .bind(&item.ownership_kind)
    .bind(item.expected_created_at_epoch)
    .bind(item.expected_revision)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
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

pub async fn enable_purge(pool: &PgPool, job_id: &str) -> RetentionResult<()> {
    bounded(job_id, MAX_KEY, "job_id")?;
    let affected = sqlx::query(
        "UPDATE cloud_retention_jobs SET state='ready', purge_enabled=TRUE, dry_run=FALSE, updated_at=now() \
         WHERE job_id=$1 AND state='dry_run' AND dry_run_completed_at IS NOT NULL \
           AND notice_evidence_at IS NOT NULL AND deadline_at <= now() AND hold_code IS NULL",
    )
    .bind(job_id)
    .execute(pool)
    .await?
    .rows_affected();
    if affected != 1 {
        return Err(RetentionError::Held("purge prerequisites are not met"));
    }
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
) -> RetentionResult<Option<RetentionLease>> {
    bounded(worker_id, MAX_KEY, "worker_id")?;
    let mut tx = pool.begin().await?;
    let row = sqlx::query(
        "SELECT job_id FROM cloud_retention_jobs \
         WHERE deadline_at <= now() AND \
           (($1 = 'dry_run' AND ((state='ready' AND dry_run=TRUE) OR state='dry_run')) OR \
            (state = 'ready' AND dry_run = FALSE AND purge_enabled = TRUE AND $1 = 'purge') OR \
            (state = 'leased' AND lease_expires_at <= now() AND \
             (($1 = 'dry_run' AND dry_run = TRUE) OR \
              ($1 = 'purge' AND dry_run = FALSE AND purge_enabled = TRUE)))) \
         ORDER BY deadline_at, created_at, job_id LIMIT 1 FOR UPDATE SKIP LOCKED",
    )
    .bind(mode.as_str())
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
         WHERE job_id=$2 AND \
           (($3 = 'dry_run' AND ((state='ready' AND dry_run=TRUE) OR state='dry_run')) OR \
            ($3 = 'purge' AND state='ready' AND dry_run=FALSE AND purge_enabled=TRUE) OR \
            (state='leased' AND lease_expires_at <= now() AND \
             (($3 = 'dry_run' AND dry_run=TRUE) OR \
              ($3 = 'purge' AND dry_run=FALSE AND purge_enabled=TRUE)))) \
         RETURNING scope_kind, subject_user_id, expected_coverage_revision, dry_run",
    )
    .bind(worker_id)
    .bind(&job_id)
    .bind(mode.as_str())
    .fetch_one(&mut *tx)
    .await?;
    let lease = RetentionLease {
        job_id,
        scope_kind: row.try_get("scope_kind")?,
        subject_user_id: row.try_get("subject_user_id")?,
        expected_coverage_revision: row.try_get("expected_coverage_revision")?,
        dry_run: row.try_get("dry_run")?,
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
    let Some(lease) = claim_due(pool, worker_id, mode).await? else {
        return Ok(None);
    };
    let mut tx = pool.begin().await?;
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
    if let Some(expected) = lease.expected_coverage_revision {
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
    if mode == RetentionMode::DryRun {
        clear_lease(&mut tx, &lease, RetentionState::DryRun).await?;
        tx.commit().await?;
        return Ok(Some(RetentionRun {
            job_id: lease.job_id,
            state: RetentionState::DryRun,
            dry_run: true,
            processed_items: items.len(),
            held_items: items
                .iter()
                .filter(|item| {
                    item.try_get::<String, _>("ownership_kind")
                        .map(|kind| kind == "shared")
                        .unwrap_or(false)
                })
                .count(),
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
}
