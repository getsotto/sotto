//! Durable, explicitly scoped Cloud retention jobs.
//!
//! A retention job records who or which organisation is in scope, the eligibility episode that
//! created it, the fixed deadline and the notice evidence that supports it.  Scope items are
//! stored separately so a later dry run can be reviewed before any purge switch is enabled. The
//! production lifecycle calls the personal and shared dry-run entry points by default only when
//! their switches are set. A separately configured purge worker still requires signed restore
//! evidence and a fresh exact-scope approval before each destructive batch.
//!
//! Scope item identifiers are canonical database identifiers. Share-link tokens supplied by a
//! producer are resolved to their internal `share_links.id` while the scope is inserted.

use std::collections::HashSet;
use std::fmt;

use crate::cloud_coverage::{self, CoverageState, EXPORT_WINDOW_SECONDS};
use crate::cloud_coverage_store::StoreError;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Row, Transaction};
use thiserror::Error;
use uuid::Uuid;

const MAX_KEY: usize = 256;
const MAX_EPISODE: usize = 160;
const MAX_NOTICE_KEY: usize = 160;
const MAX_SCOPE_ITEMS: i64 = 4_096;
const BATCH_SIZE: i64 = 64;
const RETENTION_POLICY_VERSION: i32 = 1;
const RETENTION_PURGE_LOCK_NAMESPACE: i32 = 731_044;
const RETENTION_PURGE_LOCK_KEY: i32 = 1;

type RetentionResult<T> = std::result::Result<T, RetentionError>;
type RestoreEvidenceIdentityRow = (String, String, String, i64, i64, i64, i64, bool, String);

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

/// Signed summary emitted only after a real scratch restore and two tombstone replays.
/// Fingerprints are safe to retain; paths, database URLs and customer identifiers are excluded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreEvidenceAttestation {
    pub evidence_id: String,
    pub release_sha: String,
    pub backup_fingerprint: String,
    pub sidecar_sha256: String,
    pub journal_entry_count: i64,
    pub replay_entry_count: i64,
    pub first_replay_deleted_count: i64,
    pub second_replay_deleted_count: i64,
    pub incarnation_guard_verified: bool,
    pub signature: String,
}

impl RestoreEvidenceAttestation {
    pub fn validate_and_verify(
        &self,
        expected_release_sha: &str,
        hmac_key: &[u8],
    ) -> RetentionResult<()> {
        bounded(&self.evidence_id, 128, "restore evidence id")?;
        if !self.evidence_id.starts_with("restore-evidence:")
            || self.release_sha != expected_release_sha
            || !is_lower_hex(&self.release_sha, &[40, 64])
            || !is_lower_hex(&self.backup_fingerprint, &[64])
            || !is_lower_hex(&self.sidecar_sha256, &[64])
            || !is_lower_hex(&self.signature, &[64])
            || self.journal_entry_count < 0
            || self.replay_entry_count != self.journal_entry_count
            || self.first_replay_deleted_count < 0
            || self.first_replay_deleted_count > self.replay_entry_count
            || self.second_replay_deleted_count != 0
            || !self.incarnation_guard_verified
            || hmac_key.len() < 32
        {
            return Err(RetentionError::Held(
                "restore evidence is incomplete or stale",
            ));
        }
        let mut mac = Hmac::<Sha256>::new_from_slice(hmac_key)
            .map_err(|_| RetentionError::InvalidInput("restore evidence key"))?;
        mac.update(restore_evidence_payload(self).as_bytes());
        let signature = decode_lower_hex_32(&self.signature).ok_or(RetentionError::Held(
            "restore evidence signature is invalid",
        ))?;
        mac.verify_slice(&signature)
            .map_err(|_| RetentionError::Held("restore evidence signature is invalid"))
    }
}

fn restore_evidence_payload(evidence: &RestoreEvidenceAttestation) -> String {
    let fields = [
        "sotto-cloud-retention-restore-v1".to_owned(),
        evidence.evidence_id.clone(),
        evidence.release_sha.clone(),
        evidence.backup_fingerprint.clone(),
        evidence.sidecar_sha256.clone(),
        evidence.journal_entry_count.to_string(),
        evidence.replay_entry_count.to_string(),
        evidence.first_replay_deleted_count.to_string(),
        evidence.second_replay_deleted_count.to_string(),
        evidence.incarnation_guard_verified.to_string(),
    ];
    fields
        .iter()
        .map(|field| format!("{}:{field}", field.len()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Test and offline signer seam. Production only accepts signatures made with the separate
/// restore-evidence key; it never manufactures attestations itself.
pub fn sign_restore_evidence(evidence: &RestoreEvidenceAttestation, key: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts arbitrary key sizes");
    mac.update(restore_evidence_payload(evidence).as_bytes());
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn is_lower_hex(value: &str, lengths: &[usize]) -> bool {
    lengths.contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn decode_lower_hex_32(value: &str) -> Option<[u8; 32]> {
    if !is_lower_hex(value, &[64]) {
        return None;
    }
    let mut output = [0_u8; 32];
    for (index, output_byte) in output.iter_mut().enumerate() {
        let chunk = &value.as_bytes()[index * 2..index * 2 + 2];
        let high = (chunk[0] as char).to_digit(16)? as u8;
        let low = (chunk[1] as char).to_digit(16)? as u8;
        *output_byte = (high << 4) | low;
    }
    Some(output)
}

#[derive(Debug, Clone)]
struct RetentionLease {
    job_id: String,
    scope_kind: String,
    subject_user_id: Option<String>,
    expected_coverage_revision: Option<i64>,
    expected_scope_generation: Option<i64>,
    execution_scope_generation: Option<i64>,
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
                   AND ((dry_run=TRUE AND purge_enabled=FALSE) OR (dry_run=FALSE AND purge_enabled=TRUE))",
        )
        .bind(job_id)
        .execute(&mut *tx)
        .await?
        .rows_affected(),
        OperatorAction::Cancel => sqlx::query(
            "UPDATE cloud_retention_jobs SET state='cancelled', hold_code='operator_cancelled', \
                     lease_owner=NULL, lease_expires_at=NULL, updated_at=now() \
                 WHERE job_id=$1 AND state IN ('planned','ready','dry_run','held','leased') \
                   AND ((dry_run=TRUE AND purge_enabled=FALSE) OR (dry_run=FALSE AND purge_enabled=TRUE))",
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
    if matches!(action, OperatorAction::Hold | OperatorAction::Cancel) {
        sqlx::query(
            "UPDATE cloud_retention_purge_approvals SET state='revoked',invalidated_at=now() \
             WHERE job_id=$1 AND state='active'",
        )
        .bind(job_id)
        .execute(&mut *tx)
        .await?;
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

/// Record an HMAC-authenticated report from the private current-candidate restore rehearsal.
/// The supplied count must match the live append-only journal at registration time.
pub async fn register_restore_evidence(
    pool: &PgPool,
    evidence: &RestoreEvidenceAttestation,
    signing_key: &[u8],
    expected_release_sha: &str,
    operator: &str,
) -> RetentionResult<bool> {
    bounded(operator, 80, "operator")?;
    evidence.validate_and_verify(expected_release_sha, signing_key)?;
    let mut tx = pool.begin().await?;
    let journal_entries: i64 =
        sqlx::query_scalar("SELECT count(*) FROM cloud_retention_tombstone_journal")
            .fetch_one(&mut *tx)
            .await?;
    if journal_entries != evidence.journal_entry_count {
        return Err(RetentionError::Held("restore evidence journal is stale"));
    }
    let inserted = sqlx::query(
        "INSERT INTO cloud_retention_restore_evidence \
         (evidence_id,release_sha,backup_fingerprint,sidecar_sha256,journal_entry_count, \
          replay_entry_count,first_replay_deleted_count,second_replay_deleted_count, \
          incarnation_guard_verified,signature,recorded_by) \
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11) \
         ON CONFLICT(evidence_id) DO NOTHING",
    )
    .bind(&evidence.evidence_id)
    .bind(&evidence.release_sha)
    .bind(&evidence.backup_fingerprint)
    .bind(&evidence.sidecar_sha256)
    .bind(evidence.journal_entry_count)
    .bind(evidence.replay_entry_count)
    .bind(evidence.first_replay_deleted_count)
    .bind(evidence.second_replay_deleted_count)
    .bind(evidence.incarnation_guard_verified)
    .bind(&evidence.signature)
    .bind(operator)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if inserted == 0 {
        let stored: Option<RestoreEvidenceIdentityRow> = sqlx::query_as(
            "SELECT release_sha,backup_fingerprint,sidecar_sha256,journal_entry_count, \
                 replay_entry_count,first_replay_deleted_count,second_replay_deleted_count, \
                 incarnation_guard_verified,signature \
                 FROM cloud_retention_restore_evidence WHERE evidence_id=$1 FOR UPDATE",
        )
        .bind(&evidence.evidence_id)
        .fetch_optional(&mut *tx)
        .await?;
        let expected = (
            evidence.release_sha.clone(),
            evidence.backup_fingerprint.clone(),
            evidence.sidecar_sha256.clone(),
            evidence.journal_entry_count,
            evidence.replay_entry_count,
            evidence.first_replay_deleted_count,
            evidence.second_replay_deleted_count,
            evidence.incarnation_guard_verified,
            evidence.signature.clone(),
        );
        if stored != Some(expected) {
            return Err(RetentionError::IdentityConflict);
        }
    }
    tx.commit().await?;
    Ok(inserted == 1)
}

/// Return a stable, redacted digest of the job contract and every resource identity. Item state
/// is intentionally excluded so the same approval remains bound to the exact scope as batches
/// progress; any identity, owner, deadline, notice or generation change alters the digest.
pub async fn review_scope_fingerprint(pool: &PgPool, job_id: &str) -> RetentionResult<String> {
    bounded(job_id, MAX_KEY, "job_id")?;
    let mut tx = pool.begin().await?;
    let fingerprint = review_scope_fingerprint_in_tx(&mut tx, job_id).await?;
    tx.commit().await?;
    Ok(fingerprint)
}

async fn review_scope_fingerprint_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    job_id: &str,
) -> RetentionResult<String> {
    let job = sqlx::query(
        "SELECT job_id,scope_kind,scope_key,subject_user_id,organisation_id,eligibility_episode, \
                extract(epoch FROM deadline_at)::bigint AS deadline_epoch,notice_event_key, \
                extract(epoch FROM notice_evidence_at)::bigint AS notice_evidence_epoch, \
                expected_coverage_revision,expected_scope_generation, \
                extract(epoch FROM eligibility_loss_at)::bigint AS eligibility_loss_epoch \
         FROM cloud_retention_jobs WHERE job_id=$1",
    )
    .bind(job_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(RetentionError::NotFound)?;
    let mut hasher = Sha256::new();
    fingerprint_field(&mut hasher, &RETENTION_POLICY_VERSION.to_string());
    for name in [
        "job_id",
        "scope_kind",
        "scope_key",
        "subject_user_id",
        "organisation_id",
        "eligibility_episode",
        "deadline_epoch",
        "notice_event_key",
        "notice_evidence_epoch",
        "expected_coverage_revision",
        "expected_scope_generation",
        "eligibility_loss_epoch",
    ] {
        fingerprint_field(&mut hasher, name);
        let value: Option<String> = match name {
            "job_id" => Some(job.try_get("job_id")?),
            "scope_kind" => Some(job.try_get("scope_kind")?),
            "scope_key" => Some(job.try_get("scope_key")?),
            "subject_user_id" => job.try_get("subject_user_id")?,
            "organisation_id" => job.try_get("organisation_id")?,
            "eligibility_episode" => Some(job.try_get("eligibility_episode")?),
            "deadline_epoch" => Some(job.try_get::<i64, _>("deadline_epoch")?.to_string()),
            "notice_event_key" => Some(job.try_get("notice_event_key")?),
            "notice_evidence_epoch" => job
                .try_get::<Option<i64>, _>("notice_evidence_epoch")?
                .map(|value| value.to_string()),
            "expected_coverage_revision" => job
                .try_get::<Option<i64>, _>("expected_coverage_revision")?
                .map(|value| value.to_string()),
            "expected_scope_generation" => job
                .try_get::<Option<i64>, _>("expected_scope_generation")?
                .map(|value| value.to_string()),
            "eligibility_loss_epoch" => job
                .try_get::<Option<i64>, _>("eligibility_loss_epoch")?
                .map(|value| value.to_string()),
            _ => unreachable!(),
        };
        fingerprint_field(&mut hasher, value.as_deref().unwrap_or("<null>"));
    }
    let items = sqlx::query(
        "SELECT resource_kind,resource_id,ownership_kind,expected_created_at,expected_revision \
         FROM cloud_retention_scope_items WHERE job_id=$1 \
         ORDER BY resource_kind COLLATE \"C\",resource_id COLLATE \"C\" LIMIT $2",
    )
    .bind(job_id)
    .bind(MAX_SCOPE_ITEMS + 1)
    .fetch_all(&mut **tx)
    .await?;
    if items.is_empty() || items.len() as i64 > MAX_SCOPE_ITEMS {
        return Err(RetentionError::Held(
            "retention scope is empty or exceeds its bound",
        ));
    }
    fingerprint_field(&mut hasher, &items.len().to_string());
    for item in items {
        fingerprint_field(&mut hasher, &item.try_get::<String, _>("resource_kind")?);
        fingerprint_field(&mut hasher, &item.try_get::<String, _>("resource_id")?);
        fingerprint_field(&mut hasher, &item.try_get::<String, _>("ownership_kind")?);
        fingerprint_field(
            &mut hasher,
            &item.try_get::<i64, _>("expected_created_at")?.to_string(),
        );
        let revision: Option<i64> = item.try_get("expected_revision")?;
        fingerprint_field(
            &mut hasher,
            &revision.map_or_else(|| "<null>".into(), |v| v.to_string()),
        );
    }
    Ok(hex_digest(&hasher.finalize()))
}

fn fingerprint_field(hasher: &mut Sha256, value: &str) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value.as_bytes());
}

fn hex_digest(value: &[u8]) -> String {
    value.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Approve one exact completed dry-run after checking current authority, notices, inventory and
/// restore evidence. The separate protected route supplies the release identity and HMAC key.
pub async fn approve_purge(
    pool: &PgPool,
    job_id: &str,
    requested_scope_fingerprint: &str,
    evidence_id: &str,
    operator: &str,
    expected_release_sha: &str,
) -> RetentionResult<String> {
    bounded(job_id, MAX_KEY, "job_id")?;
    bounded(evidence_id, 128, "restore evidence id")?;
    bounded(operator, 80, "operator")?;
    if !is_lower_hex(requested_scope_fingerprint, &[64]) {
        return Err(RetentionError::InvalidInput("scope fingerprint"));
    }
    let mut tx = pool.begin().await?;
    let scope_kind: Option<String> =
        sqlx::query_scalar("SELECT scope_kind FROM cloud_retention_jobs WHERE job_id=$1")
            .bind(job_id)
            .fetch_optional(&mut *tx)
            .await?;
    match scope_kind.as_deref() {
        Some("personal") => {
            let subject: Option<String> = sqlx::query_scalar(
                "SELECT subject_user_id FROM cloud_retention_jobs WHERE job_id=$1",
            )
            .bind(job_id)
            .fetch_one(&mut *tx)
            .await?;
            if let Some(subject) = subject.as_deref() {
                sqlx::query_scalar::<_, String>(
                    "SELECT beneficiary_id FROM cloud_coverage_coordinators WHERE beneficiary_id=$1 FOR SHARE",
                )
                .bind(subject)
                .fetch_optional(&mut *tx)
                .await?;
                sqlx::query_scalar::<_, Option<i64>>(
                    "SELECT current_revision FROM cloud_coverage_heads WHERE beneficiary_id=$1 FOR SHARE",
                )
                .bind(subject)
                .fetch_optional(&mut *tx)
                .await?;
                let preview = load_retention_lease(&mut tx, job_id, "purge-review").await?;
                lock_matching_notice(&mut tx, &preview, subject).await?;
                sqlx::query_scalar::<_, String>("SELECT id FROM users WHERE id=$1 FOR UPDATE")
                    .bind(subject)
                    .fetch_optional(&mut *tx)
                    .await?;
            }
        }
        Some("shared_organisation") => {
            let preview = load_retention_lease(&mut tx, job_id, "purge-review").await?;
            lock_shared_scope_rows(&mut tx, &preview).await?;
        }
        _ => {
            return Err(RetentionError::Held(
                "this retention scope cannot be purged",
            ))
        }
    }
    let lease = load_retention_lease(&mut tx, job_id, "purge-review").await?;
    let job = sqlx::query(
        "SELECT state,dry_run,purge_enabled,hold_code, \
                dry_run_completed_at IS NOT NULL AS dry_run_complete,deadline_at <= now() AS deadline_passed, \
                notice_evidence_at IS NOT NULL AS notice_evidence_present \
         FROM cloud_retention_jobs WHERE job_id=$1 FOR UPDATE",
    )
    .bind(job_id)
    .fetch_one(&mut *tx)
    .await?;
    let state: String = job.try_get("state")?;
    let dry_run: bool = job.try_get("dry_run")?;
    let purge_enabled: bool = job.try_get("purge_enabled")?;
    let hold_code: Option<String> = job.try_get("hold_code")?;
    let dry_run_complete: bool = job.try_get("dry_run_complete")?;
    let deadline_passed: bool = job.try_get("deadline_passed")?;
    let notice_evidence_present: bool = job.try_get("notice_evidence_present")?;
    if !((state == "dry_run" && dry_run && !purge_enabled && hold_code.is_none())
        || (state == "held"
            && !dry_run
            && purge_enabled
            && matches!(
                hold_code.as_deref(),
                Some("restore_evidence_stale" | "operator_hold")
            )))
        || !dry_run_complete
        || !deadline_passed
        || !notice_evidence_present
    {
        return Err(RetentionError::Held("purge prerequisites are not met"));
    }
    let evidence = sqlx::query(
        "SELECT release_sha,journal_entry_count FROM cloud_retention_restore_evidence \
         WHERE evidence_id=$1 FOR SHARE",
    )
    .bind(evidence_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(RetentionError::Held("restore evidence is missing"))?;
    let release_sha: String = evidence.try_get("release_sha")?;
    let evidence_journal_count: i64 = evidence.try_get("journal_entry_count")?;
    let current_journal_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM cloud_retention_tombstone_journal")
            .fetch_one(&mut *tx)
            .await?;
    if release_sha != expected_release_sha || evidence_journal_count != current_journal_count {
        return Err(RetentionError::Held("restore evidence is stale"));
    }
    let item_counts: (i64, i64) = sqlx::query_as(
        "SELECT count(*),count(*) FILTER (WHERE state IN ('dry_run','deleted')) \
         FROM cloud_retention_scope_items WHERE job_id=$1",
    )
    .bind(job_id)
    .fetch_one(&mut *tx)
    .await?;
    if item_counts.0 <= 0 || item_counts.0 > MAX_SCOPE_ITEMS || item_counts.0 != item_counts.1 {
        return Err(RetentionError::Held("dry-run scope is incomplete"));
    }
    if lease.scope_kind == ScopeKind::Personal.as_str() {
        let subject = lease
            .subject_user_id
            .as_deref()
            .ok_or(RetentionError::Held("retention owner is missing"))?;
        let now: i64 = sqlx::query_scalar("SELECT extract(epoch FROM now())::bigint")
            .fetch_one(&mut *tx)
            .await?;
        let authority = match crate::cloud_coverage_authority::load_current_in_transaction(
            &mut tx, subject, now,
        )
        .await
        {
            Ok(state) => state,
            Err(crate::cloud_coverage_authority::AuthorityError::Database(error))
            | Err(crate::cloud_coverage_authority::AuthorityError::Store(StoreError::Database(
                error,
            ))) => {
                return Err(RetentionError::Database(error));
            }
            Err(_) => crate::cloud_coverage_authority::AuthorityState::Stale,
        };
        let coverage_revision: Option<i64> = sqlx::query_scalar(
            "SELECT current_revision FROM cloud_coverage_heads WHERE beneficiary_id=$1",
        )
        .bind(subject)
        .fetch_optional(&mut *tx)
        .await?
        .flatten();
        let owner_exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE id=$1)")
                .bind(subject)
                .fetch_one(&mut *tx)
                .await?;
        let notice_available = lock_matching_notice(&mut tx, &lease, subject).await?;
        if let Some(reason) = validate_dry_run_evidence(
            &mut tx,
            &lease,
            Some(authority),
            coverage_revision,
            notice_available,
            owner_exists,
        )
        .await?
        {
            return Err(RetentionError::Held(reason));
        }
        if let Some(reason) = validate_scope_inventory(&mut tx, &lease).await? {
            return Err(RetentionError::Held(reason));
        }
    } else {
        if let Some(reason) =
            validate_shared_dry_run(&mut tx, &lease, lease.execution_scope_generation).await?
        {
            return Err(RetentionError::Held(reason));
        }
    }
    let scope_fingerprint = review_scope_fingerprint_in_tx(&mut tx, job_id).await?;
    if scope_fingerprint != requested_scope_fingerprint {
        return Err(RetentionError::Held("reviewed scope changed"));
    }
    sqlx::query(
        "UPDATE cloud_retention_purge_approvals SET state='stale',invalidated_at=now() \
         WHERE job_id=$1 AND state='active'",
    )
    .bind(job_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO cloud_retention_purge_approvals \
         (approval_id,job_id,scope_fingerprint,policy_version,evidence_id,journal_entry_count,actor_label) \
         VALUES($1,$2,$3,$4,$5,$6,$7)",
    )
    .bind(format!("retention-approval:{}", Uuid::new_v4()))
    .bind(job_id)
    .bind(&scope_fingerprint)
    .bind(RETENTION_POLICY_VERSION)
    .bind(evidence_id)
    .bind(evidence_journal_count)
    .bind(operator)
    .execute(&mut *tx)
    .await?;
    let execution_generation = if lease.scope_kind == ScopeKind::SharedOrganisation.as_str() {
        let org_id = lease
            .organisation_id
            .as_deref()
            .ok_or(RetentionError::Held("organisation scope is missing"))?;
        Some(
            sqlx::query_scalar::<_, i64>(
                "SELECT retention_generation FROM organizations WHERE id=$1",
            )
            .bind(org_id)
            .fetch_one(&mut *tx)
            .await?,
        )
    } else {
        None
    };
    sqlx::query(
        "UPDATE cloud_retention_jobs SET state='ready',dry_run=FALSE,purge_enabled=TRUE, \
                hold_code=NULL,execution_scope_generation=COALESCE($2,expected_scope_generation), \
                updated_at=now() WHERE job_id=$1",
    )
    .bind(job_id)
    .bind(execution_generation)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(scope_fingerprint)
}

async fn load_retention_lease(
    tx: &mut Transaction<'_, Postgres>,
    job_id: &str,
    worker_id: &str,
) -> RetentionResult<RetentionLease> {
    let row = sqlx::query(
        "SELECT scope_kind,subject_user_id,expected_coverage_revision,expected_scope_generation,execution_scope_generation, \
                extract(epoch FROM eligibility_loss_at)::bigint AS eligibility_loss_epoch, \
                organisation_id,dry_run,extract(epoch FROM deadline_at)::bigint AS deadline_epoch, \
                notice_event_key,extract(epoch FROM notice_evidence_at)::bigint AS notice_evidence_epoch \
         FROM cloud_retention_jobs WHERE job_id=$1",
    )
    .bind(job_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(RetentionError::NotFound)?;
    Ok(RetentionLease {
        job_id: job_id.to_owned(),
        scope_kind: row.try_get("scope_kind")?,
        subject_user_id: row.try_get("subject_user_id")?,
        expected_coverage_revision: row.try_get("expected_coverage_revision")?,
        expected_scope_generation: row.try_get("expected_scope_generation")?,
        execution_scope_generation: row.try_get("execution_scope_generation")?,
        eligibility_loss_epoch: row.try_get("eligibility_loss_epoch")?,
        organisation_id: row.try_get("organisation_id")?,
        dry_run: row.try_get("dry_run")?,
        deadline_epoch: row.try_get("deadline_epoch")?,
        notice_event_key: row.try_get("notice_event_key")?,
        notice_evidence_epoch: row.try_get("notice_evidence_epoch")?,
        worker_id: worker_id.to_owned(),
    })
}

async fn validate_purge_approval(
    tx: &mut Transaction<'_, Postgres>,
    lease: &RetentionLease,
    expected_release_sha: &str,
) -> RetentionResult<Option<&'static str>> {
    if !matches!(
        lease.scope_kind.as_str(),
        "personal" | "shared_organisation"
    ) {
        return Ok(Some("unsupported_scope"));
    }
    let approval = sqlx::query(
        "SELECT approval.approval_id,approval.scope_fingerprint,approval.policy_version, \
                approval.evidence_id,approval.journal_entry_count,evidence.release_sha, \
                evidence.journal_entry_count AS evidence_journal_count \
         FROM cloud_retention_purge_approvals approval \
         JOIN cloud_retention_restore_evidence evidence USING(evidence_id) \
         WHERE approval.job_id=$1 AND approval.state='active' FOR UPDATE OF approval",
    )
    .bind(&lease.job_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(approval) = approval else {
        return Ok(Some("purge_approval_missing"));
    };
    let approval_id: String = approval.try_get("approval_id")?;
    let scope_fingerprint: String = approval.try_get("scope_fingerprint")?;
    let policy_version: i32 = approval.try_get("policy_version")?;
    let evidence_release_sha: String = approval.try_get("release_sha")?;
    let approval_journal_count: i64 = approval.try_get("journal_entry_count")?;
    let evidence_journal_count: i64 = approval.try_get("evidence_journal_count")?;
    let current_journal_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM cloud_retention_tombstone_journal")
            .fetch_one(&mut **tx)
            .await?;
    let current_scope_fingerprint = review_scope_fingerprint_in_tx(tx, &lease.job_id).await?;
    let item_counts: (i64, i64) = sqlx::query_as(
        "SELECT count(*),count(*) FILTER (WHERE state IN ('dry_run','deleted')) \
         FROM cloud_retention_scope_items WHERE job_id=$1",
    )
    .bind(&lease.job_id)
    .fetch_one(&mut **tx)
    .await?;
    let reason = if policy_version != RETENTION_POLICY_VERSION
        || scope_fingerprint != current_scope_fingerprint
        || item_counts.0 <= 0
        || item_counts.0 != item_counts.1
    {
        Some("scope_changed")
    } else if evidence_release_sha != expected_release_sha
        || approval_journal_count != evidence_journal_count
        || approval_journal_count != current_journal_count
    {
        Some("restore_evidence_stale")
    } else {
        None
    };
    if let Some(reason) = reason {
        sqlx::query(
            "UPDATE cloud_retention_purge_approvals SET state='stale',invalidated_at=now() \
             WHERE approval_id=$1 AND state='active'",
        )
        .bind(approval_id)
        .execute(&mut **tx)
        .await?;
        return Ok(Some(reason));
    }
    Ok(None)
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
           AND ($3=FALSE OR scope_kind='shared_organisation') \
           AND ($1 <> 'purge' OR (scope_kind IN ('personal','shared_organisation') \
                AND EXISTS (SELECT 1 FROM cloud_retention_purge_approvals approval \
                    WHERE approval.job_id=cloud_retention_jobs.job_id AND approval.state='active'))) AND \
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
           AND ($5=FALSE OR scope_kind='shared_organisation') \
           AND ($3 <> 'purge' OR (scope_kind IN ('personal','shared_organisation') \
                AND EXISTS (SELECT 1 FROM cloud_retention_purge_approvals approval \
                    WHERE approval.job_id=cloud_retention_jobs.job_id AND approval.state='active'))) AND \
           (($3 = 'dry_run' AND state='ready' AND dry_run=TRUE) OR \
            ($3 = 'purge' AND state='ready' AND dry_run=FALSE AND purge_enabled=TRUE) OR \
            (state='leased' AND lease_expires_at <= now() AND \
             (($3 = 'dry_run' AND dry_run=TRUE) OR \
              ($3 = 'purge' AND dry_run=FALSE AND purge_enabled=TRUE)))) \
         RETURNING scope_kind, subject_user_id, organisation_id, expected_coverage_revision, expected_scope_generation, execution_scope_generation, \
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
        execution_scope_generation: row.try_get("execution_scope_generation")?,
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

/// Run the bounded non-destructive mode. Purge is deliberately rejected here so ordinary worker
/// call sites cannot turn a dry run into deletion by changing only a mode argument.
pub async fn run_once(
    pool: &PgPool,
    worker_id: &str,
    mode: RetentionMode,
) -> RetentionResult<Option<RetentionRun>> {
    if mode == RetentionMode::Purge {
        return Err(RetentionError::Held(
            "purge execution requires the configured approved-retention worker",
        ));
    }
    run_once_with_scope(pool, worker_id, mode, false, false, None).await
}

/// Production purge seam. It is called only by the separately configured worker, and each batch
/// still requires a live approval plus matching restore evidence in the database.
pub async fn run_approved_purge_once(
    pool: &PgPool,
    worker_id: &str,
    expected_release_sha: &str,
) -> RetentionResult<Option<RetentionRun>> {
    if !is_lower_hex(expected_release_sha, &[40, 64]) {
        return Err(RetentionError::InvalidInput("release sha"));
    }
    run_once_with_scope(
        pool,
        worker_id,
        RetentionMode::Purge,
        false,
        false,
        Some(expected_release_sha),
    )
    .await
}

/// Production entry point: it can only claim personal dry-run jobs. The mode and scope are fixed
/// here so runtime wiring cannot accidentally select the dormant purge branch.
pub async fn run_dry_run_once(
    pool: &PgPool,
    worker_id: &str,
) -> RetentionResult<Option<RetentionRun>> {
    run_once_with_scope(pool, worker_id, RetentionMode::DryRun, true, false, None).await
}

/// Run one non-destructive dry-run batch for a qualified shared organisation scope.
/// Orphaned organisations remain held and cannot be claimed by this entry point.
pub async fn run_shared_dry_run_once(
    pool: &PgPool,
    worker_id: &str,
) -> RetentionResult<Option<RetentionRun>> {
    run_once_with_scope(pool, worker_id, RetentionMode::DryRun, false, true, None).await
}

async fn run_once_with_scope(
    pool: &PgPool,
    worker_id: &str,
    mode: RetentionMode,
    personal_only: bool,
    shared_only: bool,
    expected_release_sha: Option<&str>,
) -> RetentionResult<Option<RetentionRun>> {
    let Some(lease) = claim_due(pool, worker_id, mode, personal_only, shared_only).await? else {
        return Ok(None);
    };
    let mut tx = pool.begin().await?;
    if mode == RetentionMode::Purge {
        // Restore evidence binds the global tombstone journal count. Serialize destructive
        // batches across jobs so two workers cannot both validate the same journal generation
        // and then append tombstones concurrently under that one attestation.
        sqlx::query("SELECT pg_advisory_xact_lock($1, $2)")
            .bind(RETENTION_PURGE_LOCK_NAMESPACE)
            .bind(RETENTION_PURGE_LOCK_KEY)
            .execute(&mut *tx)
            .await?;
    }
    // Collection completion locks the coordinator before publishing the coverage head. Keep
    // that order here, then retain the existing head-before-job order used by candidate
    // reconciliation.
    if matches!(mode, RetentionMode::DryRun | RetentionMode::Purge)
        && lease.scope_kind == ScopeKind::Personal.as_str()
    {
        if let Some(subject) = lease.subject_user_id.as_deref() {
            sqlx::query_scalar::<_, String>(
                "SELECT beneficiary_id FROM cloud_coverage_coordinators WHERE beneficiary_id=$1 FOR SHARE",
            )
            .bind(subject)
            .fetch_optional(&mut *tx)
            .await?;
        }
    }
    let locked_coverage_revision = if matches!(mode, RetentionMode::DryRun | RetentionMode::Purge)
        && lease.scope_kind == ScopeKind::Personal.as_str()
    {
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
    if matches!(mode, RetentionMode::DryRun | RetentionMode::Purge)
        && lease.scope_kind == ScopeKind::SharedOrganisation.as_str()
    {
        lock_shared_scope_rows(&mut tx, &lease).await?;
    }
    // Keep the candidate writer's lock order: coverage head, exact notice, owner, then job. The
    // owner lock prevents new personal resources appearing between the inventory check and the
    // dry-run result, while the notice lock prevents cancellation after evidence was checked.
    let (notice_available, owner_exists) =
        if matches!(mode, RetentionMode::DryRun | RetentionMode::Purge)
            && lease.scope_kind == ScopeKind::Personal.as_str()
        {
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
    if mode == RetentionMode::Purge {
        let Some(release_sha) = expected_release_sha else {
            return Err(RetentionError::Held("purge release identity is missing"));
        };
        if let Some(reason) = validate_purge_approval(&mut tx, &lease, release_sha).await? {
            set_job_hold(&mut tx, &lease, reason).await?;
            tx.commit().await?;
            return Ok(Some(RetentionRun {
                job_id: lease.job_id,
                state: RetentionState::Held,
                dry_run: false,
                processed_items: 0,
                held_items: 0,
            }));
        }
    }
    if matches!(mode, RetentionMode::DryRun | RetentionMode::Purge)
        && lease.scope_kind == ScopeKind::Personal.as_str()
    {
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
                dry_run: lease.dry_run,
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
                dry_run: lease.dry_run,
                processed_items: 0,
                held_items: 0,
            }));
        }
    } else if mode == RetentionMode::DryRun {
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
    }

    if mode == RetentionMode::Purge && lease.scope_kind == ScopeKind::SharedOrganisation.as_str() {
        if let Some(reason) =
            validate_shared_dry_run(&mut tx, &lease, lease.execution_scope_generation).await?
        {
            sqlx::query(
                "UPDATE cloud_retention_purge_approvals SET state='stale',invalidated_at=now() \
                 WHERE job_id=$1 AND state='active'",
            )
            .bind(&lease.job_id)
            .execute(&mut *tx)
            .await?;
            set_job_hold(&mut tx, &lease, reason).await?;
            tx.commit().await?;
            return Ok(Some(RetentionRun {
                job_id: lease.job_id,
                state: RetentionState::Held,
                dry_run: false,
                processed_items: 0,
                held_items: 0,
            }));
        }
    }

    // Validate and lock the complete remaining scope before deleting any child. Parent rows
    // are checked first, so a changed environment or project cannot be discovered only after
    // its wrapped key or token has already been removed.
    if mode == RetentionMode::Purge {
        let remaining_items = sqlx::query(
            "SELECT resource_kind,resource_id,ownership_kind,expected_created_at,expected_revision \
             FROM cloud_retention_scope_items WHERE job_id=$1 AND state='dry_run' \
             ORDER BY CASE resource_kind WHEN 'project' THEN 0 WHEN 'environment' THEN 1 \
                      WHEN 'grant' THEN 2 WHEN 'machine_token' THEN 3 ELSE 4 END, \
                      resource_kind COLLATE \"C\",resource_id COLLATE \"C\" FOR UPDATE",
        )
        .bind(&lease.job_id)
        .fetch_all(&mut *tx)
        .await?;
        for item in remaining_items {
            let kind: String = item.try_get("resource_kind")?;
            let id: String = item.try_get("resource_id")?;
            let ownership: String = item.try_get("ownership_kind")?;
            let created: i64 = item.try_get("expected_created_at")?;
            let revision: Option<i64> = item.try_get("expected_revision")?;
            let reason = if lease.scope_kind == ScopeKind::Personal.as_str() {
                validate_dry_run_resource(
                    &mut tx, &lease, &kind, &id, &ownership, created, revision,
                )
                .await?
            } else {
                validate_shared_resource(&mut tx, &lease, &kind, &id, created, revision).await?
            };
            if let Some(reason) = reason.filter(|reason| *reason != "resource_missing") {
                sqlx::query(
                    "UPDATE cloud_retention_scope_items SET state='held',hold_code=$4,updated_at=now() \
                     WHERE job_id=$1 AND resource_kind=$2 AND resource_id=$3 AND state='dry_run'",
                )
                .bind(&lease.job_id)
                .bind(&kind)
                .bind(&id)
                .bind(reason)
                .execute(&mut *tx)
                .await?;
                sqlx::query(
                    "UPDATE cloud_retention_purge_approvals SET state='stale',invalidated_at=now() \
                     WHERE job_id=$1 AND state='active'",
                )
                .bind(&lease.job_id)
                .execute(&mut *tx)
                .await?;
                set_job_hold(&mut tx, &lease, reason).await?;
                tx.commit().await?;
                return Ok(Some(RetentionRun {
                    job_id: lease.job_id,
                    state: RetentionState::Held,
                    dry_run: false,
                    processed_items: 0,
                    held_items: 1,
                }));
            }
        }
    }

    let item_state = if mode == RetentionMode::DryRun {
        "planned"
    } else {
        "dry_run"
    };
    // A purge batch is bounded by independent deletion units, not rows. An environment and
    // every reviewed grant/token beneath it must commit together: if the job stops between
    // batches, no surviving environment may have lost its decryption key. A large unit may
    // therefore exceed BATCH_SIZE rows; the total scope is still capped by MAX_SCOPE_ITEMS.
    let batch_query = if mode == RetentionMode::Purge {
        "WITH units AS ( \
             SELECT CASE \
                      WHEN resource_kind='environment' THEN 'environment:' || resource_id \
                      WHEN resource_kind='grant' THEN 'environment:' || (resource_id::jsonb->>0) \
                      WHEN resource_kind='machine_token' THEN 'environment:' || (resource_id::jsonb->>1) \
                      ELSE resource_kind || ':' || resource_id \
                    END AS unit_key, \
                    min(CASE \
                      WHEN resource_kind IN ('environment','grant','machine_token') THEN 0 \
                      WHEN resource_kind='share' THEN 1 ELSE 2 END) AS unit_order \
             FROM cloud_retention_scope_items WHERE job_id=$1 AND state=$2 \
             GROUP BY 1 \
         ), selected_units AS ( \
             SELECT unit_key FROM units ORDER BY unit_order,unit_key COLLATE \"C\" LIMIT $3 \
         ) \
         SELECT item.resource_kind,item.resource_id,item.ownership_kind, \
                item.expected_created_at,item.expected_revision \
         FROM cloud_retention_scope_items item \
         JOIN selected_units unit ON unit.unit_key = CASE \
              WHEN item.resource_kind='environment' THEN 'environment:' || item.resource_id \
              WHEN item.resource_kind='grant' THEN 'environment:' || (item.resource_id::jsonb->>0) \
              WHEN item.resource_kind='machine_token' THEN 'environment:' || (item.resource_id::jsonb->>1) \
              ELSE item.resource_kind || ':' || item.resource_id END \
         WHERE item.job_id=$1 AND item.state=$2 \
         ORDER BY CASE item.resource_kind WHEN 'grant' THEN 0 WHEN 'machine_token' THEN 1 \
                  WHEN 'environment' THEN 2 WHEN 'share' THEN 3 WHEN 'project' THEN 4 ELSE 5 END, \
                  item.resource_kind COLLATE \"C\",item.resource_id COLLATE \"C\" \
         FOR UPDATE OF item"
    } else {
        "SELECT resource_kind, resource_id, ownership_kind, expected_created_at, expected_revision \
         FROM cloud_retention_scope_items WHERE job_id=$1 AND state=$2 \
         ORDER BY CASE resource_kind WHEN 'grant' THEN 0 WHEN 'machine_token' THEN 1 \
                  WHEN 'environment' THEN 2 WHEN 'share' THEN 3 WHEN 'project' THEN 4 ELSE 5 END, \
                  resource_kind COLLATE \"C\",resource_id COLLATE \"C\" LIMIT $3 FOR UPDATE"
    };
    let items = sqlx::query(batch_query)
        .bind(&lease.job_id)
        .bind(item_state)
        .bind(BATCH_SIZE)
        .fetch_all(&mut *tx)
        .await?;
    if mode == RetentionMode::DryRun && lease.scope_kind == ScopeKind::SharedOrganisation.as_str() {
        if let Some(reason) = validate_shared_dry_run(&mut tx, &lease, None).await? {
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

    let owner_id = match lease.scope_kind.as_str() {
        "personal" => lease.subject_user_id.as_deref(),
        "shared_organisation" => lease.organisation_id.as_deref(),
        _ => None,
    }
    .ok_or(RetentionError::Held("retention scope owner is missing"))?;
    let expected_ownership = if lease.scope_kind == ScopeKind::Personal.as_str() {
        "personal"
    } else {
        "shared"
    };
    let mut processed = 0;
    let mut held = 0;
    let mut hold_reason = None;
    for item in &items {
        let resource_kind: String = item.try_get("resource_kind")?;
        let resource_id: String = item.try_get("resource_id")?;
        let ownership_kind: String = item.try_get("ownership_kind")?;
        let expected_created_at_epoch: i64 = item.try_get("expected_created_at")?;
        let expected_revision: Option<i64> = item.try_get("expected_revision")?;
        if ownership_kind != expected_ownership {
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
        let deleted = purge_scoped_resource(
            &mut tx,
            &lease,
            &resource_kind,
            &resource_id,
            owner_id,
            expected_created_at_epoch,
            expected_revision,
        )
        .await?;
        let Some(deleted) = deleted else {
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
        };
        let tombstone = serde_json::json!({
            "resource_kind": resource_kind,
            "resource_id": resource_id,
            "ownership_kind": ownership_kind,
            "expected_owner_id": owner_id,
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
        .bind(owner_id)
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
    if lease.scope_kind == ScopeKind::SharedOrganisation.as_str() && processed > 0 {
        let org_id = lease
            .organisation_id
            .as_deref()
            .ok_or(RetentionError::Held("organisation scope is missing"))?;
        sqlx::query(
            "UPDATE cloud_retention_jobs SET execution_scope_generation=( \
                 SELECT retention_generation FROM organizations WHERE id=$2) \
             WHERE job_id=$1 AND state='leased' AND lease_owner=$3",
        )
        .bind(&lease.job_id)
        .bind(org_id)
        .bind(&lease.worker_id)
        .execute(&mut *tx)
        .await?;
    }
    let next_state = if held > 0 {
        RetentionState::Held
    } else {
        let remaining: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM cloud_retention_scope_items WHERE job_id=$1 AND state='dry_run'",
        )
        .bind(&lease.job_id)
        .fetch_one(&mut *tx)
        .await?;
        if remaining == 0 {
            RetentionState::Completed
        } else if processed > 0 {
            sqlx::query(
                "UPDATE cloud_retention_purge_approvals SET state='stale',invalidated_at=now() \
                 WHERE job_id=$1 AND state='active'",
            )
            .bind(&lease.job_id)
            .execute(&mut *tx)
            .await?;
            set_job_hold(&mut tx, &lease, "restore_evidence_stale").await?;
            RetentionState::Held
        } else {
            RetentionState::Ready
        }
    };
    if held > 0 {
        if processed > 0 {
            sqlx::query(
                "UPDATE cloud_retention_purge_approvals SET state='stale',invalidated_at=now() \
                 WHERE job_id=$1 AND state='active'",
            )
            .bind(&lease.job_id)
            .execute(&mut *tx)
            .await?;
        }
        set_job_hold(
            &mut tx,
            &lease,
            hold_reason.unwrap_or("retention_scope_held"),
        )
        .await?;
    } else if next_state == RetentionState::Completed {
        sqlx::query(
            "UPDATE cloud_retention_purge_approvals SET state='completed',invalidated_at=now() \
             WHERE job_id=$1 AND state='active'",
        )
        .bind(&lease.job_id)
        .execute(&mut *tx)
        .await?;
        clear_lease(&mut tx, &lease, next_state).await?;
    } else if next_state == RetentionState::Held {
        // The hold was set above because every successful batch changes the journal. A new
        // independent restore attestation and approval are required before another batch.
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

/// Delete one previously reviewed resource with its full incarnation and owner predicate. A
/// parent is only deleted after all of its scoped children have been deleted in an earlier
/// step of the same ordered batch. Returning `None` means an existing row no longer matches
/// the reviewed identity, so the caller parks the job instead of journalling a deletion.
async fn purge_scoped_resource(
    tx: &mut Transaction<'_, Postgres>,
    lease: &RetentionLease,
    kind: &str,
    id: &str,
    owner_id: &str,
    created_at: i64,
    revision: Option<i64>,
) -> RetentionResult<Option<u64>> {
    let shared = lease.scope_kind == ScopeKind::SharedOrganisation.as_str();
    let affected = match kind {
        "project" if !shared => sqlx::query(
            "DELETE FROM projects p WHERE p.id=$1 AND p.owner_id=$2 AND p.org_id IS NULL \
             AND (extract(epoch FROM p.created_at)*1000000)::bigint=$3 \
             AND NOT EXISTS (SELECT 1 FROM environments e WHERE e.project_id=p.id)",
        )
        .bind(id)
        .bind(owner_id)
        .bind(created_at)
        .execute(&mut **tx)
        .await?
        .rows_affected(),
        "project" => sqlx::query(
            "DELETE FROM projects p WHERE p.id=$1 AND p.org_id=$2 \
             AND (extract(epoch FROM p.created_at)*1000000)::bigint=$3 \
             AND NOT EXISTS (SELECT 1 FROM environments e WHERE e.project_id=p.id)",
        )
        .bind(id)
        .bind(owner_id)
        .bind(created_at)
        .execute(&mut **tx)
        .await?
        .rows_affected(),
        "environment" if !shared => sqlx::query(
            "DELETE FROM environments e USING projects p WHERE e.id=$1 AND e.project_id=p.id \
             AND p.owner_id=$2 AND p.org_id IS NULL AND e.revision=$3 \
             AND (extract(epoch FROM e.created_at)*1000000)::bigint=$4 \
             AND NOT EXISTS (SELECT 1 FROM environment_grants g WHERE g.env_id=e.id) \
             AND NOT EXISTS (SELECT 1 FROM machine_tokens m WHERE m.env_id=e.id)",
        )
        .bind(id)
        .bind(owner_id)
        .bind(revision)
        .bind(created_at)
        .execute(&mut **tx)
        .await?
        .rows_affected(),
        "environment" => sqlx::query(
            "DELETE FROM environments e USING projects p WHERE e.id=$1 AND e.project_id=p.id \
             AND p.org_id=$2 AND e.revision=$3 \
             AND (extract(epoch FROM e.created_at)*1000000)::bigint=$4 \
             AND NOT EXISTS (SELECT 1 FROM environment_grants g WHERE g.env_id=e.id) \
             AND NOT EXISTS (SELECT 1 FROM machine_tokens m WHERE m.env_id=e.id)",
        )
        .bind(id)
        .bind(owner_id)
        .bind(revision)
        .bind(created_at)
        .execute(&mut **tx)
        .await?
        .rows_affected(),
        "share" if !shared => sqlx::query(
            "DELETE FROM share_links WHERE id=$1 AND created_by=$2 \
             AND (extract(epoch FROM created_at)*1000000)::bigint=$3",
        )
        .bind(id)
        .bind(owner_id)
        .bind(created_at)
        .execute(&mut **tx)
        .await?
        .rows_affected(),
        "grant" => {
            let parts = parse_composite_resource_id(id)?;
            let env_id = &parts[0];
            let user_id = &parts[1];
            let owner_predicate = if shared {
                "p.org_id=$3"
            } else {
                "p.owner_id=$3 AND p.org_id IS NULL"
            };
            let sql = format!(
                "DELETE FROM environment_grants g USING environments e, projects p \
                 WHERE g.env_id=$1 AND g.user_id=$2 AND e.id=g.env_id AND p.id=e.project_id \
                   AND {owner_predicate} AND (extract(epoch FROM g.created_at)*1000000)::bigint=$4"
            );
            sqlx::query(&sql)
                .bind(env_id)
                .bind(user_id)
                .bind(owner_id)
                .bind(created_at)
                .execute(&mut **tx)
                .await?
                .rows_affected()
        }
        "machine_token" => {
            let parts = parse_composite_resource_id(id)?;
            let token_id = &parts[0];
            let env_id = &parts[1];
            let owner_predicate = if shared {
                "p.org_id=$3"
            } else {
                "p.owner_id=$3 AND p.org_id IS NULL"
            };
            let sql = format!(
                "DELETE FROM machine_tokens m USING environments e, projects p \
                 WHERE m.id=$1 AND m.env_id=$2 AND e.id=m.env_id AND p.id=e.project_id \
                   AND {owner_predicate} AND (extract(epoch FROM m.created_at)*1000000)::bigint=$4"
            );
            sqlx::query(&sql)
                .bind(token_id)
                .bind(env_id)
                .bind(owner_id)
                .bind(created_at)
                .execute(&mut **tx)
                .await?
                .rows_affected()
        }
        _ => return Err(RetentionError::InvalidInput("resource_kind")),
    };
    if affected > 0 {
        return Ok(Some(affected));
    }
    let still_exists: bool = match kind {
        "project" => {
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM projects WHERE id=$1)")
                .bind(id)
                .fetch_one(&mut **tx)
                .await?
        }
        "environment" => {
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM environments WHERE id=$1)")
                .bind(id)
                .fetch_one(&mut **tx)
                .await?
        }
        "share" => {
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM share_links WHERE id=$1)")
                .bind(id)
                .fetch_one(&mut **tx)
                .await?
        }
        "grant" => {
            let parts = parse_composite_resource_id(id)?;
            sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM environment_grants WHERE env_id=$1 AND user_id=$2)",
            )
            .bind(&parts[0])
            .bind(&parts[1])
            .fetch_one(&mut **tx)
            .await?
        }
        "machine_token" => {
            let parts = parse_composite_resource_id(id)?;
            sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM machine_tokens WHERE id=$1 AND env_id=$2)",
            )
            .bind(&parts[0])
            .bind(&parts[1])
            .fetch_one(&mut **tx)
            .await?
        }
        _ => return Err(RetentionError::InvalidInput("resource_kind")),
    };
    Ok((!still_exists).then_some(0))
}

fn parse_composite_resource_id(id: &str) -> RetentionResult<[String; 2]> {
    let parts: Vec<String> = serde_json::from_str(id)
        .map_err(|_| RetentionError::InvalidInput("composite resource id"))?;
    if parts.len() != 2 || parts.iter().any(|part| part.trim().is_empty()) {
        return Err(RetentionError::InvalidInput("composite resource id"));
    }
    let mut parts = parts.into_iter();
    Ok([
        parts.next().expect("validated length"),
        parts.next().expect("validated length"),
    ])
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
    let grants = sqlx::query(
        "SELECT g.env_id,g.user_id FROM environment_grants g \
         JOIN environments e ON e.id=g.env_id JOIN projects p ON p.id=e.project_id \
         WHERE p.owner_id=$1 AND p.org_id IS NULL ORDER BY g.env_id,g.user_id LIMIT $2",
    )
    .bind(subject)
    .bind(MAX_SCOPE_ITEMS + 1)
    .fetch_all(&mut **tx)
    .await?;
    let machine_tokens = sqlx::query(
        "SELECT m.id,m.env_id FROM machine_tokens m \
         JOIN environments e ON e.id=m.env_id JOIN projects p ON p.id=e.project_id \
         WHERE p.owner_id=$1 AND p.org_id IS NULL ORDER BY m.id LIMIT $2",
    )
    .bind(subject)
    .bind(MAX_SCOPE_ITEMS + 1)
    .fetch_all(&mut **tx)
    .await?;
    let max_scope_items = MAX_SCOPE_ITEMS as usize;
    if projects.len() + environments.len() + shares.len() + grants.len() + machine_tokens.len()
        > max_scope_items
        || projects.len() > max_scope_items
        || environments.len() > max_scope_items
        || shares.len() > max_scope_items
        || grants.len() > max_scope_items
        || machine_tokens.len() > max_scope_items
    {
        return Ok(Some("scope_exceeds_bound"));
    }

    let mut current = HashSet::with_capacity(
        projects.len() + environments.len() + shares.len() + grants.len() + machine_tokens.len(),
    );
    current.extend(projects.into_iter().map(|id| ("project".to_owned(), id)));
    current.extend(
        environments
            .into_iter()
            .map(|id| ("environment".to_owned(), id)),
    );
    current.extend(shares.into_iter().map(|id| ("share".to_owned(), id)));
    for row in grants {
        current.insert((
            "grant".to_owned(),
            serde_json::json!([
                row.try_get::<String, _>("env_id")?,
                row.try_get::<String, _>("user_id")?
            ])
            .to_string(),
        ));
    }
    for row in machine_tokens {
        current.insert((
            "machine_token".to_owned(),
            serde_json::json!([
                row.try_get::<String, _>("id")?,
                row.try_get::<String, _>("env_id")?
            ])
            .to_string(),
        ));
    }
    let rows = sqlx::query(
        "SELECT resource_kind, resource_id FROM cloud_retention_scope_items \
         WHERE job_id=$1 AND state<>'deleted' ORDER BY resource_kind, resource_id LIMIT $2",
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
    execution_generation: Option<i64>,
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
    if generation != execution_generation.unwrap_or(expected_generation) {
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
         FROM cloud_retention_scope_items WHERE job_id=$1 AND state<>'deleted' \
         ORDER BY resource_kind,resource_id LIMIT 4097",
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
    let found = match kind {
        "project" => {
            let row = sqlx::query(
                "SELECT (extract(epoch FROM created_at)*1000000)::bigint AS created_epoch \
                 FROM projects WHERE id=$1 AND org_id=$2 FOR UPDATE",
            )
            .bind(id)
            .bind(org_id)
            .fetch_optional(&mut **tx)
            .await?;
            row.map(|row| row.try_get::<i64, _>("created_epoch"))
                .transpose()?
                == Some(created)
        }
        "environment" => {
            let row = sqlx::query(
                "SELECT e.revision, \
                            (extract(epoch FROM e.created_at)*1000000)::bigint AS created_epoch \
                     FROM environments e JOIN projects p ON p.id=e.project_id \
                     WHERE e.id=$1 AND p.org_id=$2 FOR UPDATE OF e,p",
            )
            .bind(id)
            .bind(org_id)
            .fetch_optional(&mut **tx)
            .await?;
            match row {
                None => false,
                Some(row) => {
                    let actual_revision: i64 = row.try_get("revision")?;
                    let actual_created: i64 = row.try_get("created_epoch")?;
                    Some(actual_revision) == revision && actual_created == created
                }
            }
        }
        "grant" => {
            let parts = parse_composite_resource_id(id)?;
            let row = sqlx::query(
                "SELECT (extract(epoch FROM g.created_at)*1000000)::bigint AS created_epoch \
                 FROM environment_grants g JOIN environments e ON e.id=g.env_id \
                 JOIN projects p ON p.id=e.project_id \
                 WHERE g.env_id=$1 AND g.user_id=$2 AND p.org_id=$3 \
                 FOR UPDATE OF g,e,p",
            )
            .bind(&parts[0])
            .bind(&parts[1])
            .bind(org_id)
            .fetch_optional(&mut **tx)
            .await?;
            row.map(|row| row.try_get::<i64, _>("created_epoch"))
                .transpose()?
                == Some(created)
        }
        "machine_token" => {
            let parts = parse_composite_resource_id(id)?;
            let row = sqlx::query(
                "SELECT (extract(epoch FROM m.created_at)*1000000)::bigint AS created_epoch \
                 FROM machine_tokens m JOIN environments e ON e.id=m.env_id \
                 JOIN projects p ON p.id=e.project_id \
                 WHERE m.id=$1 AND m.env_id=$2 AND p.org_id=$3 \
                 FOR UPDATE OF m,e,p",
            )
            .bind(&parts[0])
            .bind(&parts[1])
            .bind(org_id)
            .fetch_optional(&mut **tx)
            .await?;
            row.map(|row| row.try_get::<i64, _>("created_epoch"))
                .transpose()?
                == Some(created)
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
        "grant" => {
            let parts = parse_composite_resource_id(resource_id)?;
            let row = sqlx::query(
                "SELECT g.user_id=p.owner_id AS owner_grant, \
                        p.owner_id, p.org_id, \
                        (extract(epoch FROM g.created_at)*1000000)::bigint AS created_epoch, \
                        EXISTS (SELECT 1 FROM environment_grants other \
                                WHERE other.env_id=e.id AND other.user_id<>p.owner_id) AS shared \
                 FROM environment_grants g JOIN environments e ON e.id=g.env_id \
                 JOIN projects p ON p.id=e.project_id \
                 WHERE g.env_id=$1 AND g.user_id=$2 FOR UPDATE OF g,e,p",
            )
            .bind(&parts[0])
            .bind(&parts[1])
            .fetch_optional(&mut **tx)
            .await?;
            match row {
                None => Some("resource_missing"),
                Some(row) => {
                    let owner_grant: bool = row.try_get("owner_grant")?;
                    let owner: String = row.try_get("owner_id")?;
                    let organisation: Option<String> = row.try_get("org_id")?;
                    let created: i64 = row.try_get("created_epoch")?;
                    let shared: bool = row.try_get("shared")?;
                    if owner != subject || organisation.is_some() || shared || !owner_grant {
                        Some("shared_resource")
                    } else if created != expected_created_at {
                        Some("resource_identity_changed")
                    } else {
                        None
                    }
                }
            }
        }
        "machine_token" => {
            let parts = parse_composite_resource_id(resource_id)?;
            let row = sqlx::query(
                "SELECT p.owner_id, p.org_id, \
                        (extract(epoch FROM m.created_at)*1000000)::bigint AS created_epoch, \
                        EXISTS (SELECT 1 FROM environment_grants g \
                                WHERE g.env_id=e.id AND g.user_id<>p.owner_id) AS shared \
                 FROM machine_tokens m JOIN environments e ON e.id=m.env_id \
                 JOIN projects p ON p.id=e.project_id \
                 WHERE m.id=$1 AND m.env_id=$2 FOR UPDATE OF m,e,p",
            )
            .bind(&parts[0])
            .bind(&parts[1])
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
    fn restore_evidence_accepts_an_empty_journal_but_rejects_tampering() {
        let key = b"a separate restore evidence HMAC key with enough entropy";
        let mut evidence = RestoreEvidenceAttestation {
            evidence_id: "restore-evidence:test-run-1".into(),
            release_sha: "a".repeat(40),
            backup_fingerprint: "b".repeat(64),
            sidecar_sha256: "c".repeat(64),
            journal_entry_count: 0,
            replay_entry_count: 0,
            first_replay_deleted_count: 0,
            second_replay_deleted_count: 0,
            incarnation_guard_verified: true,
            signature: String::new(),
        };
        evidence.signature = sign_restore_evidence(&evidence, key);
        assert!(evidence
            .validate_and_verify(&evidence.release_sha, key)
            .is_ok());

        evidence.backup_fingerprint.replace_range(0..1, "c");
        assert!(matches!(
            evidence.validate_and_verify(&evidence.release_sha, key),
            Err(RetentionError::Held(
                "restore evidence signature is invalid"
            ))
        ));
    }

    #[test]
    fn restore_evidence_allows_already_absent_rows_but_bounds_replay_deletions() {
        let key = b"a separate restore evidence HMAC key with enough entropy";
        let mut evidence = RestoreEvidenceAttestation {
            evidence_id: "restore-evidence:already-absent-rows".into(),
            release_sha: "a".repeat(40),
            backup_fingerprint: "b".repeat(64),
            sidecar_sha256: "c".repeat(64),
            journal_entry_count: 3,
            replay_entry_count: 3,
            // The backup may already omit rows purged before the dump was taken.
            first_replay_deleted_count: 0,
            second_replay_deleted_count: 0,
            incarnation_guard_verified: true,
            signature: String::new(),
        };
        evidence.signature = sign_restore_evidence(&evidence, key);
        assert!(evidence
            .validate_and_verify(&evidence.release_sha, key)
            .is_ok());

        evidence.first_replay_deleted_count = 4;
        evidence.signature = sign_restore_evidence(&evidence, key);
        assert!(matches!(
            evidence.validate_and_verify(&evidence.release_sha, key),
            Err(RetentionError::Held(
                "restore evidence is incomplete or stale"
            ))
        ));
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
