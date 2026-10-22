//! Personal expiry candidate reconciliation and the non-destructive runtime worker.
//!
//! Candidate snapshots are created only from complete person-level eligibility. The production
//! worker has no purge mode: it can enqueue immutable personal scopes and run the dedicated
//! personal dry-run path only. Organisation-owned resources and unclassified links are recorded
//! as held scope items, never treated as personal deletion authority.

use std::time::Duration;

use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

use crate::cloud_coverage::EXPORT_WINDOW_SECONDS;
use crate::config::DeploymentMode;
use crate::error::{Error, Result};
use crate::notifications::{self, NoticeError};
use crate::person_eligibility::{self, EligibilityState, EligibilityView};
use crate::retention::{
    self, EnqueueOutcome, RetentionError, RetentionIntent, ScopeItem, ScopeKind,
};
use crate::state::AppState;

const USER_BATCH: i64 = 32;
const MAX_SCOPE_ITEMS: usize = 4_096;
const RETENTION_POLICY: &str = "cloud-export-deadline-v1";

/// One bounded pass over users, reconciling future notices and personal expiry candidates.
pub async fn reconcile_once(state: &AppState) -> Result<usize> {
    if !state.cloud_retention_dry_run_enabled || state.deployment_mode != DeploymentMode::Cloud {
        return Ok(0);
    }
    let now_epoch = epoch_now();
    let scan_owner = format!("retention-scan:{}", Uuid::new_v4());
    let Some(cursor) = claim_scan(&state.pool, &scan_owner).await? else {
        return Ok(0);
    };
    let users = sqlx::query_scalar::<_, String>(
        "SELECT id FROM users WHERE ($1::text IS NULL OR id COLLATE \"C\" > $1 COLLATE \"C\") \
         ORDER BY id COLLATE \"C\" LIMIT $2",
    )
    .bind(cursor.as_deref())
    .bind(USER_BATCH)
    .fetch_all(&state.pool)
    .await?;
    if users.is_empty() {
        release_scan(&state.pool, &scan_owner, None).await?;
        return Ok(0);
    }
    for user_id in &users {
        refresh_scan_lease(&state.pool, &scan_owner).await?;
        if let Err(error) = reconcile_user_at(state, user_id, now_epoch).await {
            eprintln!("Cloud retention scan could not reconcile one account: {error}");
            if let Err(hold_error) =
                hold_user_candidates(&state.pool, user_id, "reconciliation_failed").await
            {
                // Continue the bounded scan even if the fallback hold cannot be persisted. The
                // durable cursor will wrap around and revisit this account on a later pass.
                eprintln!(
                    "Cloud retention scan could not persist a hold for one account: {hold_error}"
                );
            }
        }
    }
    let next_cursor = (users.len() == USER_BATCH as usize)
        .then(|| users.last().cloned())
        .flatten();
    release_scan(&state.pool, &scan_owner, next_cursor.as_deref()).await?;
    let processed = users.len();
    Ok(processed)
}

async fn claim_scan(pool: &PgPool, owner: &str) -> Result<Option<Option<String>>> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO cloud_retention_scan_state(scan_key) VALUES('personal_expiry') \
         ON CONFLICT (scan_key) DO NOTHING",
    )
    .execute(&mut *tx)
    .await?;
    let cursor = sqlx::query_scalar::<_, Option<String>>(
        "SELECT cursor_user_id FROM cloud_retention_scan_state \
         WHERE scan_key='personal_expiry' AND (lease_expires_at IS NULL OR lease_expires_at <= now()) \
         FOR UPDATE SKIP LOCKED",
    )
    .fetch_optional(&mut *tx)
    .await?;
    let Some(cursor) = cursor else {
        tx.commit().await?;
        return Ok(None);
    };
    sqlx::query(
        "UPDATE cloud_retention_scan_state SET lease_owner=$1, \
             lease_expires_at=now()+interval '5 minutes', updated_at=now() \
         WHERE scan_key='personal_expiry'",
    )
    .bind(owner)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Some(cursor))
}

async fn release_scan(pool: &PgPool, owner: &str, cursor: Option<&str>) -> Result<()> {
    let affected = sqlx::query(
        "UPDATE cloud_retention_scan_state SET cursor_user_id=$2, lease_owner=NULL, \
             lease_expires_at=NULL, updated_at=now() \
         WHERE scan_key='personal_expiry' AND lease_owner=$1 AND lease_expires_at > now()",
    )
    .bind(owner)
    .bind(cursor)
    .execute(pool)
    .await?
    .rows_affected();
    if affected != 1 {
        return Err(Error::Conflict(
            "Cloud retention scan lease was lost".into(),
        ));
    }
    Ok(())
}

async fn refresh_scan_lease(pool: &PgPool, owner: &str) -> Result<()> {
    let affected = sqlx::query(
        "UPDATE cloud_retention_scan_state SET lease_expires_at=now()+interval '5 minutes', updated_at=now() \
         WHERE scan_key='personal_expiry' AND lease_owner=$1 AND lease_expires_at > now()",
    )
    .bind(owner)
    .execute(pool)
    .await?
    .rows_affected();
    if affected != 1 {
        return Err(Error::Conflict(
            "Cloud retention scan lease was lost".into(),
        ));
    }
    Ok(())
}

/// Reconcile one account's durable expiry state. This bounded seam is also used by account-level
/// repair tooling; it does not select a retention mode or expose any destructive capability.
pub async fn reconcile_user(state: &AppState, user_id: &str) -> Result<()> {
    if !state.cloud_retention_dry_run_enabled || state.deployment_mode != DeploymentMode::Cloud {
        return Ok(());
    }
    reconcile_user_at(state, user_id, epoch_now()).await
}

async fn reconcile_user_at(state: &AppState, user_id: &str, now_epoch: i64) -> Result<()> {
    let (view, revision) = match person_eligibility::load_view_with_authority_requirement(
        state, user_id, true,
    )
    .await
    {
        Ok(snapshot) => snapshot,
        // Corrupt billing or coverage evidence is not a reason to stop the bounded scan for every
        // later user. Preserve the candidate as held; database failures still propagate and retry.
        Err(Error::Internal(_)) => {
            hold_user_candidates(&state.pool, user_id, "eligibility_unavailable").await?;
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    match view.state {
        EligibilityState::Paid | EligibilityState::RenewalRecovery => {
            if let Some(revision) = revision {
                let reconciled = notifications::reconcile_export_deadline_notice(
                    &state.pool,
                    user_id,
                    &view,
                    Some(revision),
                    now_epoch,
                )
                .await
                .map_err(map_notice_error)?;
                if reconciled {
                    cancel_user_candidates(&state.pool, user_id, "coverage_resumed").await?;
                }
            }
        }
        EligibilityState::ExportOnly | EligibilityState::Expired => {
            reconcile_expiry_candidate(&state.pool, user_id, &view, revision, now_epoch).await?;
        }
        EligibilityState::PendingInitialPayment | EligibilityState::Unavailable => {
            hold_user_candidates(&state.pool, user_id, "eligibility_unavailable").await?;
        }
        EligibilityState::Free => {
            hold_user_candidates(&state.pool, user_id, "coverage_episode_unproven").await?;
        }
    }
    Ok(())
}

async fn reconcile_expiry_candidate(
    pool: &PgPool,
    user_id: &str,
    view: &EligibilityView,
    revision: Option<i64>,
    now_epoch: i64,
) -> Result<()> {
    let Some(revision) = revision else {
        hold_user_candidates(pool, user_id, "coverage_revision_missing").await?;
        return Ok(());
    };
    let Some(deadline_epoch) = view.export_until_epoch else {
        hold_user_candidates(pool, user_id, "coverage_deadline_missing").await?;
        return Ok(());
    };
    let Some(export_start_epoch) = deadline_epoch.checked_sub(EXPORT_WINDOW_SECONDS) else {
        hold_user_candidates(pool, user_id, "coverage_deadline_invalid").await?;
        return Ok(());
    };
    // The export notice starts the recovery window; it does not mean the account has expired.
    // Do not create a candidate until the full export deadline has passed.
    if deadline_epoch <= 0 || export_start_epoch <= 0 || now_epoch < deadline_epoch {
        return Ok(());
    }
    let notice_event_key = format!("cloud-export-window:{export_start_epoch}:{deadline_epoch}");
    let episode = format!("export-expiry:{export_start_epoch}:{deadline_epoch}");
    let mut tx = pool.begin().await?;
    let current_revision: Option<i64> = sqlx::query_scalar(
        "SELECT current_revision FROM cloud_coverage_heads WHERE beneficiary_id=$1 FOR SHARE",
    )
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await?
    .flatten();
    if current_revision != Some(revision) {
        tx.rollback().await?;
        return Ok(());
    }
    // The outbox row is part of the candidate's evidence. Read it under the same transaction
    // after the coverage head lock, so a concurrent notice cancellation cannot race the
    // immutable candidate snapshot.
    let notice_evidence_epoch = timely_notice_evidence(
        &mut tx,
        user_id,
        &notice_event_key,
        export_start_epoch,
        deadline_epoch,
    )
    .await?;
    // Resource rows reference their owner through foreign keys. Holding the owner row prevents a
    // concurrent project or share insert from appearing after this scope snapshot; project and
    // environment locks below fence nested environment and grant additions as well. Notice is
    // locked first to match the outbox writer's lock order and avoid a row/user lock cycle.
    let owner: Option<String> = sqlx::query_scalar("SELECT id FROM users WHERE id=$1 FOR UPDATE")
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await?;
    if owner.is_none() {
        tx.rollback().await?;
        return Ok(());
    }
    cancel_other_episodes_in_tx(&mut tx, user_id, &episode).await?;
    let intent = RetentionIntent {
        job_id: retention::new_job_id(),
        scope_kind: ScopeKind::Personal,
        scope_key: user_id.to_owned(),
        subject_user_id: Some(user_id.to_owned()),
        organisation_id: None,
        eligibility_episode: episode,
        deadline_epoch,
        notice_event_key,
        notice_evidence_epoch,
        expected_coverage_revision: Some(revision),
        expected_scope_generation: None,
        eligibility_loss_epoch: None,
    };
    if retention::enqueue_in_tx(&mut tx, &intent)
        .await
        .map_err(map_retention_error)?
        == EnqueueOutcome::AlreadyQueued
    {
        // A job first observed without its required notice is held. If the exact notice is
        // available on a later pass, enqueue_in_tx promotes the recorded evidence from absent
        // to present; release only this specific hold, keeping scope and every other hold fixed.
        if notice_evidence_epoch.is_some() {
            sqlx::query(
                "UPDATE cloud_retention_jobs SET state='ready', hold_code=NULL, updated_at=now() \
                 WHERE scope_kind='personal' AND scope_key=$1 AND eligibility_episode=$2 \
                   AND state='held' AND hold_code='notice_evidence_missing' \
                   AND notice_evidence_at=to_timestamp($3) AND dry_run=TRUE AND purge_enabled=FALSE",
            )
            .bind(user_id)
            .bind(&intent.eligibility_episode)
            .bind(export_start_epoch)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        return Ok(());
    }

    let mut scope =
        snapshot_personal_scope(&mut tx, user_id, &intent.job_id, now_epoch, deadline_epoch)
            .await?;
    let over_bound = scope.len() > MAX_SCOPE_ITEMS;
    scope.truncate(MAX_SCOPE_ITEMS);
    for (item, hold_code) in scope {
        retention::add_scope_item_in_tx(
            &mut tx,
            &item,
            if hold_code.is_some() {
                "held"
            } else {
                "planned"
            },
            hold_code,
        )
        .await
        .map_err(map_retention_error)?;
    }
    let job_hold = if over_bound {
        Some("scope_exceeds_bound")
    } else if notice_evidence_epoch.is_none() {
        Some("notice_evidence_missing")
    } else {
        None
    };
    if let Some(hold_code) = job_hold {
        sqlx::query(
            "UPDATE cloud_retention_jobs SET state='held', hold_code=$2, updated_at=now() \
             WHERE job_id=$1 AND state='planned'",
        )
        .bind(&intent.job_id)
        .bind(hold_code)
        .execute(&mut *tx)
        .await?;
    } else {
        sqlx::query(
            "UPDATE cloud_retention_jobs SET state='ready', dry_run=TRUE, updated_at=now() \
             WHERE job_id=$1 AND state='planned' AND notice_evidence_at IS NOT NULL \
               AND hold_code IS NULL",
        )
        .bind(&intent.job_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn timely_notice_evidence(
    tx: &mut Transaction<'_, Postgres>,
    user_id: &str,
    event_key: &str,
    export_start_epoch: i64,
    deadline_epoch: i64,
) -> Result<Option<i64>> {
    let found: Option<String> = sqlx::query_scalar(
        "SELECT notice_id FROM cloud_notice_outbox \
         WHERE recipient_user_id=$1 AND event_key=$2 AND policy_key=$3 \
           AND kind='export_deadline' AND channel='in_app' \
           AND (status <> 'cancelled' OR cancelled_at >= to_timestamp($4)) \
           AND due_at=to_timestamp($4) AND created_at <= to_timestamp($4) \
           AND payload->>'deadline_epoch'=$5 \
         ORDER BY created_at, notice_id LIMIT 1 FOR SHARE",
    )
    .bind(user_id)
    .bind(event_key)
    .bind(RETENTION_POLICY)
    .bind(export_start_epoch)
    .bind(deadline_epoch.to_string())
    .fetch_optional(&mut **tx)
    .await?;
    Ok(found.map(|_| export_start_epoch))
}

async fn snapshot_personal_scope(
    tx: &mut Transaction<'_, Postgres>,
    user_id: &str,
    job_id: &str,
    now_epoch: i64,
    deadline_epoch: i64,
) -> Result<Vec<(ScopeItem, Option<&'static str>)>> {
    let projects = sqlx::query(
        "SELECT p.id, (extract(epoch from p.created_at) * 1000000)::bigint AS created_epoch, \
                EXISTS (SELECT 1 FROM environments e JOIN environment_grants g ON g.env_id=e.id \
                        WHERE e.project_id=p.id AND g.user_id<>p.owner_id) AS shared \
         FROM projects p WHERE p.owner_id=$1 AND p.org_id IS NULL \
         ORDER BY p.id COLLATE \"C\" LIMIT $2 FOR UPDATE OF p",
    )
    .bind(user_id)
    .bind((MAX_SCOPE_ITEMS + 1) as i64)
    .fetch_all(&mut **tx)
    .await?;
    let environments = sqlx::query(
        "SELECT e.id, e.revision, (extract(epoch from e.created_at) * 1000000)::bigint AS created_epoch, \
                EXISTS (SELECT 1 FROM environment_grants g WHERE g.env_id=e.id AND g.user_id<>p.owner_id) AS shared \
         FROM environments e JOIN projects p ON p.id=e.project_id \
         WHERE p.owner_id=$1 AND p.org_id IS NULL \
         ORDER BY e.id COLLATE \"C\" LIMIT $2 FOR UPDATE OF e,p",
    )
    .bind(user_id)
    .bind((MAX_SCOPE_ITEMS + 1) as i64)
    .fetch_all(&mut **tx)
    .await?;
    let shares = sqlx::query(
        "SELECT id, share_class,max_views,view_count,revoked_at IS NOT NULL AS revoked, \
                extract(epoch from expires_at)::bigint AS expires_epoch, \
                (extract(epoch from created_at) * 1000000)::bigint AS created_epoch \
         FROM share_links WHERE created_by=$1 \
         ORDER BY id COLLATE \"C\" LIMIT $2 FOR UPDATE",
    )
    .bind(user_id)
    .bind((MAX_SCOPE_ITEMS + 1) as i64)
    .fetch_all(&mut **tx)
    .await?;
    let grants = sqlx::query(
        "SELECT g.env_id,g.user_id,(extract(epoch FROM g.created_at)*1000000)::bigint AS created_epoch, \
                EXISTS (SELECT 1 FROM environment_grants other \
                        WHERE other.env_id=g.env_id AND other.user_id<>p.owner_id) AS shared \
         FROM environment_grants g JOIN environments e ON e.id=g.env_id \
         JOIN projects p ON p.id=e.project_id WHERE p.owner_id=$1 AND p.org_id IS NULL \
         ORDER BY g.env_id COLLATE \"C\",g.user_id COLLATE \"C\" LIMIT $2",
    )
    .bind(user_id)
    .bind((MAX_SCOPE_ITEMS + 1) as i64)
    .fetch_all(&mut **tx)
    .await?;
    let machine_tokens = sqlx::query(
        "SELECT m.id,m.env_id,(extract(epoch FROM m.created_at)*1000000)::bigint AS created_epoch, \
                EXISTS (SELECT 1 FROM environment_grants g WHERE g.env_id=m.env_id AND g.user_id<>p.owner_id) AS shared \
         FROM machine_tokens m JOIN environments e ON e.id=m.env_id \
         JOIN projects p ON p.id=e.project_id WHERE p.owner_id=$1 AND p.org_id IS NULL \
         ORDER BY m.id COLLATE \"C\" LIMIT $2",
    )
    .bind(user_id)
    .bind((MAX_SCOPE_ITEMS + 1) as i64)
    .fetch_all(&mut **tx)
    .await?;

    let mut scope = Vec::with_capacity(
        projects.len() + environments.len() + shares.len() + grants.len() + machine_tokens.len(),
    );
    for row in projects {
        let id: String = row.try_get("id")?;
        let created: i64 = row.try_get("created_epoch")?;
        let shared: bool = row.try_get("shared")?;
        scope.push((
            ScopeItem {
                job_id: job_id.to_owned(),
                resource_kind: "project".into(),
                resource_id: id,
                ownership_kind: if shared { "shared" } else { "personal" }.into(),
                expected_created_at_epoch: created,
                expected_revision: None,
            },
            shared.then_some("shared_resource"),
        ));
    }
    for row in environments {
        let id: String = row.try_get("id")?;
        let created: i64 = row.try_get("created_epoch")?;
        let revision: i64 = row.try_get("revision")?;
        let shared: bool = row.try_get("shared")?;
        scope.push((
            ScopeItem {
                job_id: job_id.to_owned(),
                resource_kind: "environment".into(),
                resource_id: id,
                ownership_kind: if shared { "shared" } else { "personal" }.into(),
                expected_created_at_epoch: created,
                expected_revision: Some(revision),
            },
            shared.then_some("shared_resource"),
        ));
    }
    for row in shares {
        let id: String = row.try_get("id")?;
        let share_class: String = row.try_get("share_class")?;
        let expires_epoch: Option<i64> = row.try_get("expires_epoch")?;
        let max_views: i32 = row.try_get("max_views")?;
        let view_count: i32 = row.try_get("view_count")?;
        let revoked: bool = row.try_get("revoked")?;
        let created: i64 = row.try_get("created_epoch")?;
        scope.push((
            ScopeItem {
                job_id: job_id.to_owned(),
                resource_kind: "share".into(),
                resource_id: id,
                ownership_kind: "personal".into(),
                expected_created_at_epoch: created,
                expected_revision: None,
            },
            retention::share_lifetime_hold(
                &share_class,
                expires_epoch,
                max_views,
                view_count,
                revoked,
                now_epoch,
                deadline_epoch,
            ),
        ));
    }
    for row in grants {
        let env_id: String = row.try_get("env_id")?;
        let user_id: String = row.try_get("user_id")?;
        let created: i64 = row.try_get("created_epoch")?;
        let shared: bool = row.try_get("shared")?;
        scope.push((
            ScopeItem {
                job_id: job_id.to_owned(),
                resource_kind: "grant".into(),
                resource_id: serde_json::json!([env_id, user_id]).to_string(),
                ownership_kind: if shared { "shared" } else { "personal" }.into(),
                expected_created_at_epoch: created,
                expected_revision: None,
            },
            shared.then_some("shared_resource"),
        ));
    }
    for row in machine_tokens {
        let token_id: String = row.try_get("id")?;
        let env_id: String = row.try_get("env_id")?;
        let created: i64 = row.try_get("created_epoch")?;
        let shared: bool = row.try_get("shared")?;
        scope.push((
            ScopeItem {
                job_id: job_id.to_owned(),
                resource_kind: "machine_token".into(),
                resource_id: serde_json::json!([token_id, env_id]).to_string(),
                ownership_kind: if shared { "shared" } else { "personal" }.into(),
                expected_created_at_epoch: created,
                expected_revision: None,
            },
            shared.then_some("shared_resource"),
        ));
    }
    Ok(scope)
}

async fn cancel_other_episodes_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    user_id: &str,
    current_episode: &str,
) -> Result<()> {
    sqlx::query(
        "UPDATE cloud_retention_jobs SET state='cancelled', hold_code='episode_superseded', \
             lease_owner=NULL, lease_expires_at=NULL, updated_at=now() \
         WHERE scope_kind='personal' AND scope_key=$1 AND eligibility_episode<>$2 \
           AND state IN ('planned','ready','dry_run','held','leased') AND dry_run=TRUE \
           AND purge_enabled=FALSE",
    )
    .bind(user_id)
    .bind(current_episode)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn cancel_user_candidates(pool: &PgPool, user_id: &str, reason: &str) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "SELECT current_revision FROM cloud_coverage_heads WHERE beneficiary_id=$1 FOR SHARE",
    )
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE cloud_retention_jobs SET state='cancelled', hold_code=$2, lease_owner=NULL, \
             lease_expires_at=NULL, updated_at=now() \
         WHERE scope_kind='personal' AND scope_key=$1 AND state IN ('planned','ready','dry_run','held','leased') \
           AND dry_run=TRUE AND purge_enabled=FALSE",
    )
    .bind(user_id)
    .bind(reason)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn hold_user_candidates(pool: &PgPool, user_id: &str, reason: &str) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "SELECT current_revision FROM cloud_coverage_heads WHERE beneficiary_id=$1 FOR SHARE",
    )
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE cloud_retention_jobs SET state='held', hold_code=$2, lease_owner=NULL, \
             lease_expires_at=NULL, updated_at=now() \
         WHERE scope_kind='personal' AND scope_key=$1 AND state IN ('planned','ready','dry_run','held','leased') \
           AND dry_run=TRUE AND purge_enabled=FALSE",
    )
    .bind(user_id)
    .bind(reason)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

fn map_notice_error(error: NoticeError) -> Error {
    match error {
        NoticeError::Database(error) => Error::Db(error),
        other => Error::Internal(other.to_string()),
    }
}

fn map_retention_error(error: RetentionError) -> Error {
    match error {
        RetentionError::Database(error) => Error::Db(error),
        _ => Error::Internal("Cloud retention candidate state is inconsistent".into()),
    }
}

fn epoch_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}

/// Start the default-off retention workers. Dry runs are non-destructive; purge uses a separate
/// provider-independent entry point that requires a live approval and restore attestation.
pub fn spawn(state: AppState) {
    if (!state.cloud_retention_dry_run_enabled
        && !state.cloud_shared_retention_dry_run_enabled
        && !state.cloud_retention_purge_enabled)
        || state.deployment_mode != DeploymentMode::Cloud
    {
        return;
    }
    tokio::spawn(async move {
        let worker_id = format!("retention-worker:{}", Uuid::new_v4());
        loop {
            if state.cloud_retention_dry_run_enabled && reconcile_once(&state).await.is_err() {
                eprintln!("cloud retention candidate reconciliation failed");
            }
            if state.cloud_shared_retention_dry_run_enabled
                && crate::cloud_shared_retention::reconcile_once(&state)
                    .await
                    .is_err()
            {
                eprintln!("cloud shared retention candidate reconciliation failed");
            }
            if state.cloud_retention_dry_run_enabled {
                for _ in 0..128 {
                    match retention::run_dry_run_once(&state.pool, &worker_id).await {
                        Ok(Some(_)) => {}
                        Ok(None) => break,
                        Err(_) => {
                            eprintln!("cloud retention dry run failed");
                            break;
                        }
                    }
                }
            }
            if state.cloud_shared_retention_dry_run_enabled {
                for _ in 0..128 {
                    match retention::run_shared_dry_run_once(&state.pool, &worker_id).await {
                        Ok(Some(_)) => {}
                        Ok(None) => break,
                        Err(_) => {
                            eprintln!("cloud shared retention dry run failed");
                            break;
                        }
                    }
                }
            }
            if state.cloud_retention_purge_enabled {
                for _ in 0..128 {
                    let Some(release_sha) = state.cloud_retention_build_sha.as_deref() else {
                        eprintln!("cloud retention purge release identity is unavailable");
                        break;
                    };
                    match retention::run_approved_purge_once(&state.pool, &worker_id, release_sha)
                        .await
                    {
                        Ok(Some(_)) => {}
                        Ok(None) => break,
                        Err(_) => {
                            eprintln!("cloud retention purge batch failed");
                            break;
                        }
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn episode_identity_is_stable_and_bound_to_the_window() {
        let start = 1_900_000_000;
        let deadline = start + EXPORT_WINDOW_SECONDS;
        assert_eq!(
            format!("export-expiry:{start}:{deadline}"),
            format!("export-expiry:{start}:{deadline}")
        );
        assert_ne!(
            format!("export-expiry:{start}:{deadline}"),
            format!("export-expiry:{}:{deadline}", start + 1)
        );
    }
}
