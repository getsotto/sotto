//! Durable, explicitly scoped Cloud retention jobs.
//!
//! A retention job records who or which organisation is in scope, the eligibility episode that
//! created it, the fixed deadline and the notice evidence that supports it.  Scope items are
//! stored separately so a later dry run can be reviewed before a purge switch is ever enabled.
//! This module intentionally has no startup worker or public route yet: retention policy and
//! restore evidence remain activation gates.

use std::fmt;

use sqlx::{PgPool, Postgres, Row, Transaction};
use thiserror::Error;
use uuid::Uuid;

const MAX_KEY: usize = 256;
const MAX_EPISODE: usize = 160;
const MAX_NOTICE_KEY: usize = 160;
const MAX_SCOPE_ITEMS: i64 = 4_096;

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
    let inserted = sqlx::query(
        "INSERT INTO cloud_retention_scope_items \
         (job_id, resource_kind, resource_id, ownership_kind, expected_revision) \
         VALUES ($1,$2,$3,$4,$5) ON CONFLICT (job_id, resource_kind, resource_id) DO NOTHING",
    )
    .bind(&item.job_id)
    .bind(&item.resource_kind)
    .bind(&item.resource_id)
    .bind(&item.ownership_kind)
    .bind(item.expected_revision)
    .execute(pool)
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
         WHERE job_id=$1 AND state IN ('planned','dry_run') AND notice_evidence_at IS NOT NULL \
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
        "UPDATE cloud_retention_jobs SET state='cancelled', hold_code=$2, updated_at=now() \
         WHERE job_id=$1 AND state IN ('planned','dry_run','ready','held')",
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
        "UPDATE cloud_retention_jobs SET purge_enabled=TRUE, dry_run=FALSE, updated_at=now() \
         WHERE job_id=$1 AND state='ready' AND notice_evidence_at IS NOT NULL \
           AND deadline_at <= now() AND hold_code IS NULL",
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
        "SELECT job_id, resource_kind, resource_id, ownership_kind, expected_revision \
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
                expected_revision: row.try_get("expected_revision")?,
            })
        })
        .collect::<std::result::Result<Vec<_>, sqlx::Error>>()
        .map_err(RetentionError::Database)
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
