//! Conservative organisation-wide retention candidate preparation.
//!
//! The policy population is the current set of owners and admins. A shared export window starts
//! no earlier than the first durable notice and lasts thirty days. Missing history,
//! unavailable authority, an orphaned organisation, an in-flight ownership change, or no verified
//! contact produces held work. This module only snapshots and dry-runs; it cannot purge data.

use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

use crate::cloud_coverage::EXPORT_WINDOW_SECONDS;
use crate::config::DeploymentMode;
use crate::error::{Error, Result};
use crate::notifications::{self, NoticeError};
use crate::retention::{
    self, EnqueueOutcome, RetentionError, RetentionIntent, ScopeItem, ScopeKind,
};
use crate::state::AppState;

const ORGANISATION_BATCH: i64 = 16;
const MAX_OWNER_ADMINS: usize = 256;
const MAX_SCOPE_ITEMS: usize = 4_096;
const SHARED_RETENTION_POLICY: &str = "shared-export-deadline-v1";

pub async fn reconcile_once(state: &AppState) -> Result<usize> {
    if !state.cloud_shared_retention_dry_run_enabled
        || state.deployment_mode != DeploymentMode::Cloud
    {
        return Ok(0);
    }
    let owner = format!("shared-retention-scan:{}", Uuid::new_v4());
    let Some(cursor) = claim_scan(&state.pool, &owner).await? else {
        return Ok(0);
    };
    let organisations = sqlx::query_scalar::<_, String>(
        "SELECT id FROM organizations WHERE lifecycle_state='active' \
         AND ($1::text IS NULL OR id COLLATE \"C\" > $1 COLLATE \"C\") \
         ORDER BY id COLLATE \"C\" LIMIT $2",
    )
    .bind(cursor.as_deref())
    .bind(ORGANISATION_BATCH)
    .fetch_all(&state.pool)
    .await?;
    for organisation_id in &organisations {
        refresh_scan(&state.pool, &owner).await?;
        if let Err(error) = reconcile_organisation(state, organisation_id).await {
            eprintln!("Cloud shared retention could not reconcile one organisation: {error}");
            if let Err(hold_error) =
                hold_existing(&state.pool, organisation_id, "reconciliation_failed").await
            {
                eprintln!(
                    "Cloud shared retention could not hold the failed organisation: {hold_error}"
                );
            }
        }
    }
    let next = (organisations.len() == ORGANISATION_BATCH as usize)
        .then(|| organisations.last().cloned())
        .flatten();
    release_scan(&state.pool, &owner, next.as_deref()).await?;
    Ok(organisations.len())
}

async fn claim_scan(pool: &PgPool, owner: &str) -> Result<Option<Option<String>>> {
    let mut tx = pool.begin().await?;
    let cursor: Option<Option<String>> = sqlx::query_scalar(
        "SELECT cursor_scope_id FROM cloud_retention_scan_state \
         WHERE scan_key='shared_organisation_retention' \
           AND (lease_expires_at IS NULL OR lease_expires_at <= now()) \
         FOR UPDATE SKIP LOCKED",
    )
    .fetch_optional(&mut *tx)
    .await?;
    let Some(cursor) = cursor else {
        tx.commit().await?;
        return Ok(None);
    };
    let affected = sqlx::query(
        "UPDATE cloud_retention_scan_state SET lease_owner=$1,lease_expires_at=now()+interval '5 minutes',updated_at=now() \
         WHERE scan_key='shared_organisation_retention' AND (lease_expires_at IS NULL OR lease_expires_at<=now())",
    )
    .bind(owner)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok((affected == 1).then_some(cursor))
}

async fn refresh_scan(pool: &PgPool, owner: &str) -> Result<()> {
    let affected = sqlx::query(
        "UPDATE cloud_retention_scan_state SET lease_expires_at=now()+interval '5 minutes',updated_at=now() \
         WHERE scan_key='shared_organisation_retention' AND lease_owner=$1 AND lease_expires_at>now()",
    )
    .bind(owner)
    .execute(pool)
    .await?
    .rows_affected();
    if affected != 1 {
        return Err(Error::Conflict(
            "Cloud shared retention scan lease was lost".into(),
        ));
    }
    Ok(())
}

async fn release_scan(pool: &PgPool, owner: &str, cursor: Option<&str>) -> Result<()> {
    let affected = sqlx::query(
        "UPDATE cloud_retention_scan_state SET cursor_scope_id=$2,lease_owner=NULL,lease_expires_at=NULL,updated_at=now() \
         WHERE scan_key='shared_organisation_retention' AND lease_owner=$1 AND lease_expires_at>now()",
    )
    .bind(owner)
    .bind(cursor)
    .execute(pool)
    .await?
    .rows_affected();
    if affected != 1 {
        return Err(Error::Conflict(
            "Cloud shared retention scan lease was lost".into(),
        ));
    }
    Ok(())
}

async fn reconcile_organisation(state: &AppState, organisation_id: &str) -> Result<()> {
    let now: i64 = sqlx::query_scalar("SELECT extract(epoch FROM now())::bigint")
        .fetch_one(&state.pool)
        .await?;
    let mut tx = state.pool.begin().await?;
    let initial_members = admin_members(&mut tx, organisation_id).await?;
    lock_users(&mut tx, &initial_members).await?;
    let organisation = sqlx::query(
        "SELECT lifecycle_state,retention_generation FROM organizations WHERE id=$1 FOR UPDATE",
    )
    .bind(organisation_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(organisation) = organisation else {
        tx.rollback().await?;
        return Ok(());
    };
    let lifecycle: String = organisation.try_get("lifecycle_state")?;
    let generation: i64 = organisation.try_get("retention_generation")?;
    if lifecycle != "active" {
        tx.rollback().await?;
        return Ok(());
    }
    let members = admin_members(&mut tx, organisation_id).await?;
    if members != initial_members {
        tx.rollback().await?;
        return Err(Error::Conflict(
            "organisation membership changed during retention snapshot".into(),
        ));
    }
    sqlx::query(
        "SELECT user_id FROM organization_memberships WHERE org_id=$1 AND role IN ('owner','admin') \
         ORDER BY user_id COLLATE \"C\" FOR UPDATE",
    )
    .bind(organisation_id)
    .fetch_all(&mut *tx)
    .await?;

    let active_deletion: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM organization_deletions WHERE org_id=$1 AND state NOT IN ('cancelled','completed'))",
    )
    .bind(organisation_id)
    .fetch_one(&mut *tx)
    .await?;
    if active_deletion {
        cancel_candidates(&mut tx, organisation_id, "organisation_deletion_active").await?;
        tx.commit().await?;
        return Ok(());
    }
    let active_transfer: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM billing_transfer_intents WHERE state IN \
          ('awaiting_consent','pending','destination_prepared','destination_paid', \
           'source_adjustment_pending','source_restoration_pending','unknown') \
         AND (source_organization_id=$1 OR destination_organization_id=$1))",
    )
    .bind(organisation_id)
    .fetch_one(&mut *tx)
    .await?;
    if active_transfer {
        create_held_episode(
            &mut tx,
            organisation_id,
            generation,
            now,
            "ownership_transition_pending",
            &members,
        )
        .await?;
        tx.commit().await?;
        return Ok(());
    }

    if members.is_empty() {
        create_orphan_hold(&mut tx, organisation_id, generation, now).await?;
        tx.commit().await?;
        return Ok(());
    }
    if members.len() > MAX_OWNER_ADMINS {
        create_held_episode(
            &mut tx,
            organisation_id,
            generation,
            now,
            "owner_admin_population_exceeds_bound",
            &members,
        )
        .await?;
        tx.commit().await?;
        return Ok(());
    }

    let mut loss_epochs = Vec::with_capacity(members.len());
    for user_id in &members {
        match retention::shared_admin_eligibility(&mut tx, user_id, now).await {
            Ok(retention::SharedAdminEligibility::StillEligible) => {
                cancel_candidates(&mut tx, organisation_id, "owner_admin_still_eligible").await?;
                tx.commit().await?;
                return Ok(());
            }
            Ok(retention::SharedAdminEligibility::Lost(epoch)) => loss_epochs.push(epoch),
            Ok(retention::SharedAdminEligibility::Unknown) => {
                create_held_episode(
                    &mut tx,
                    organisation_id,
                    generation,
                    now,
                    "owner_admin_eligibility_unproven",
                    &members,
                )
                .await?;
                tx.commit().await?;
                return Ok(());
            }
            Err(error) => return Err(map_retention_error(error)),
        }
    }
    let loss_epoch = loss_epochs.into_iter().max().unwrap_or(now);
    if loss_epoch <= 0 {
        create_held_episode(
            &mut tx,
            organisation_id,
            generation,
            now,
            "shared_deadline_invalid",
            &members,
        )
        .await?;
        tx.commit().await?;
        return Ok(());
    }
    // The scope generation fences the snapshot, but it is not a new eligibility-loss
    // episode. Keep notices and the export deadline stable while resources change.
    let event_key = shared_event_key(organisation_id, loss_epoch);
    cancel_superseded_notices(&mut tx, organisation_id, &event_key).await?;
    let contact_available: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM cloud_verified_contacts c \
         JOIN organization_memberships m ON m.user_id=c.user_id \
         WHERE m.org_id=$1 AND m.role IN ('owner','admin') AND c.channel='email' AND c.revoked_at IS NULL)",
    )
    .bind(organisation_id)
    .fetch_one(&mut *tx)
    .await?;
    let prior_notice_epoch: Option<i64> = sqlx::query_scalar(
        "SELECT ceil(extract(epoch FROM min(created_at)))::bigint FROM cloud_notice_outbox \
         WHERE scope_id=$1 AND event_key=$2 AND policy_key=$3 \
           AND kind='shared_export_deadline' AND channel='in_app' AND status<>'cancelled'",
    )
    .bind(organisation_id)
    .bind(&event_key)
    .bind(SHARED_RETENTION_POLICY)
    .fetch_one(&mut *tx)
    .await?;
    if !contact_available && prior_notice_epoch.is_none() {
        create_held_episode(
            &mut tx,
            organisation_id,
            generation,
            now,
            "no_verified_owner_contact",
            &members,
        )
        .await?;
        tx.commit().await?;
        return Ok(());
    }
    let queued_epoch = if let Some(epoch) = prior_notice_epoch {
        epoch
    } else {
        sqlx::query_scalar::<_, i64>(
            "SELECT ceil(extract(epoch FROM transaction_timestamp()))::bigint",
        )
        .fetch_one(&mut *tx)
        .await?
        .max(loss_epoch)
    };
    let Some(deadline) = queued_epoch.checked_add(EXPORT_WINDOW_SECONDS) else {
        create_held_episode(
            &mut tx,
            organisation_id,
            generation,
            now,
            "shared_deadline_invalid",
            &members,
        )
        .await?;
        tx.commit().await?;
        return Ok(());
    };
    if deadline <= 0 {
        create_held_episode(
            &mut tx,
            organisation_id,
            generation,
            now,
            "shared_deadline_invalid",
            &members,
        )
        .await?;
        tx.commit().await?;
        return Ok(());
    }
    if contact_available && now <= deadline {
        for user_id in &members {
            notifications::enqueue_shared_export_deadline_in_tx(
                &mut tx,
                user_id,
                organisation_id,
                &event_key,
                queued_epoch,
                deadline,
            )
            .await
            .map_err(map_notice_error)?;
        }
    }
    let notice_count: i64 = sqlx::query_scalar(
        "SELECT count(DISTINCT recipient_user_id) FROM cloud_notice_outbox \
         WHERE scope_id=$1 AND event_key=$2 AND policy_key=$3 AND kind='shared_export_deadline' \
           AND channel='in_app' AND status<>'cancelled'",
    )
    .bind(organisation_id)
    .bind(&event_key)
    .bind(SHARED_RETENTION_POLICY)
    .fetch_one(&mut *tx)
    .await?;
    let notices_complete = notice_count == members.len() as i64;
    let episode = format!("shared-export:{loss_epoch}");
    let stored_evidence: Option<i64> = sqlx::query_scalar(
        "SELECT extract(epoch FROM notice_evidence_at)::bigint FROM cloud_retention_jobs \
         WHERE scope_kind='shared_organisation' AND scope_key=$1 AND eligibility_episode=$2",
    )
    .bind(organisation_id)
    .bind(&episode)
    .fetch_optional(&mut *tx)
    .await?
    .flatten();
    let previous_job: Option<(Option<i64>, String, Option<String>)> = sqlx::query_as(
        "SELECT expected_scope_generation,state,hold_code FROM cloud_retention_jobs \
         WHERE scope_kind='shared_organisation' AND scope_key=$1 AND eligibility_episode=$2 \
         FOR UPDATE",
    )
    .bind(organisation_id)
    .bind(&episode)
    .fetch_optional(&mut *tx)
    .await?;
    let previous_generation = previous_job
        .as_ref()
        .and_then(|(generation, _, _)| *generation);
    let previous_state = previous_job.as_ref().map(|(_, state, _)| state.as_str());
    let preserve_operator_hold = previous_job.as_ref().is_some_and(|(_, state, hold_code)| {
        state == "held" && hold_code.as_deref() == Some("operator_hold")
    });
    let terminal_job = matches!(previous_state, Some("cancelled" | "completed" | "failed"));
    let refresh_scope = previous_generation != Some(generation) && !terminal_job;
    let evidence_epoch =
        stored_evidence.or_else(|| (notices_complete && now <= deadline).then_some(queued_epoch));
    cancel_other_episodes(&mut tx, organisation_id, &episode).await?;
    let intent = RetentionIntent {
        job_id: retention::new_job_id(),
        scope_kind: ScopeKind::SharedOrganisation,
        scope_key: organisation_id.to_owned(),
        subject_user_id: None,
        organisation_id: Some(organisation_id.to_owned()),
        eligibility_episode: episode.clone(),
        deadline_epoch: deadline,
        notice_event_key: event_key.clone(),
        notice_evidence_epoch: evidence_epoch,
        expected_coverage_revision: None,
        expected_scope_generation: Some(generation),
        eligibility_loss_epoch: Some(loss_epoch),
    };
    let outcome = retention::enqueue_in_tx(&mut tx, &intent)
        .await
        .map_err(map_retention_error)?;
    let job_id = if outcome == EnqueueOutcome::Enqueued {
        intent.job_id.clone()
    } else {
        sqlx::query_scalar::<_, String>(
            "SELECT job_id FROM cloud_retention_jobs WHERE scope_kind='shared_organisation' \
             AND scope_key=$1 AND eligibility_episode=$2 FOR UPDATE",
        )
        .bind(organisation_id)
        .bind(&episode)
        .fetch_one(&mut *tx)
        .await?
    };
    if refresh_scope {
        // Existing jobs are requeued as ready when their generation changes. Return the job to
        // planned while replacing its scope items; add_scope_item_in_tx only accepts additions
        // during that preparation state.
        sqlx::query(
            "UPDATE cloud_retention_jobs SET state='planned',hold_code=NULL,updated_at=now() \
             WHERE job_id=$1 AND dry_run=TRUE AND purge_enabled=FALSE",
        )
        .bind(&job_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM cloud_retention_scope_items WHERE job_id=$1")
            .bind(&job_id)
            .execute(&mut *tx)
            .await?;
        let scope = snapshot_scope(&mut tx, organisation_id, &job_id).await?;
        if scope.len() > MAX_SCOPE_ITEMS {
            if !preserve_operator_hold {
                hold_job(&mut tx, &job_id, "scope_exceeds_bound").await?;
            }
        } else {
            for item in scope {
                retention::add_scope_item_in_tx(&mut tx, &item, "planned", None)
                    .await
                    .map_err(map_retention_error)?;
            }
        }
    }
    if terminal_job {
        // Operator cancellation and completed work remain terminal across scope changes.
    } else if preserve_operator_hold {
        sqlx::query(
            "UPDATE cloud_retention_jobs SET state='held',hold_code='operator_hold', \
                 lease_owner=NULL,lease_expires_at=NULL,updated_at=now() \
             WHERE job_id=$1 AND dry_run=TRUE AND purge_enabled=FALSE",
        )
        .bind(&job_id)
        .execute(&mut *tx)
        .await?;
    } else if !contact_available {
        hold_job(&mut tx, &job_id, "no_verified_owner_contact").await?;
    } else if !notices_complete {
        hold_job(&mut tx, &job_id, "notice_evidence_incomplete").await?;
    } else if now > deadline && evidence_epoch.is_none() {
        hold_job(&mut tx, &job_id, "notice_window_elapsed").await?;
    } else {
        sqlx::query(
            "UPDATE cloud_retention_jobs SET state='ready',hold_code=NULL,updated_at=now() \
             WHERE job_id=$1 AND state IN ('planned','held') AND hold_code IN \
               ('no_verified_owner_contact','notice_evidence_incomplete') AND dry_run=TRUE AND purge_enabled=FALSE",
        )
        .bind(&job_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE cloud_retention_jobs SET state='ready',hold_code=NULL,updated_at=now() \
             WHERE job_id=$1 AND state='planned' AND notice_evidence_at IS NOT NULL \
               AND dry_run=TRUE AND purge_enabled=FALSE",
        )
        .bind(&job_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn admin_members(tx: &mut Transaction<'_, Postgres>, org_id: &str) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT user_id FROM organization_memberships WHERE org_id=$1 AND role IN ('owner','admin') \
         ORDER BY user_id COLLATE \"C\" LIMIT 257",
    )
    .bind(org_id)
    .fetch_all(&mut **tx)
    .await?)
}

async fn lock_users(tx: &mut Transaction<'_, Postgres>, users: &[String]) -> Result<()> {
    sqlx::query_scalar::<_, String>(
        "SELECT id FROM users WHERE id=ANY($1) ORDER BY id COLLATE \"C\" FOR UPDATE",
    )
    .bind(users)
    .fetch_all(&mut **tx)
    .await?;
    Ok(())
}

async fn snapshot_scope(
    tx: &mut Transaction<'_, Postgres>,
    org_id: &str,
    job_id: &str,
) -> Result<Vec<ScopeItem>> {
    let mut items = Vec::new();
    for row in sqlx::query(
        "SELECT id,(extract(epoch FROM created_at)*1000000)::bigint AS created_at \
         FROM projects WHERE org_id=$1 ORDER BY id COLLATE \"C\" LIMIT 4097",
    )
    .bind(org_id)
    .fetch_all(&mut **tx)
    .await?
    {
        items.push(ScopeItem {
            job_id: job_id.into(),
            resource_kind: "project".into(),
            resource_id: row.try_get("id")?,
            ownership_kind: "shared".into(),
            expected_created_at_epoch: row.try_get("created_at")?,
            expected_revision: None,
        });
    }
    if items.len() > MAX_SCOPE_ITEMS {
        return Ok(items);
    }
    for row in sqlx::query(
        "SELECT e.id,e.revision,(extract(epoch FROM e.created_at)*1000000)::bigint AS created_at \
         FROM environments e JOIN projects p ON p.id=e.project_id WHERE p.org_id=$1 \
         ORDER BY e.id COLLATE \"C\" LIMIT 4097",
    )
    .bind(org_id)
    .fetch_all(&mut **tx)
    .await?
    {
        items.push(ScopeItem {
            job_id: job_id.into(),
            resource_kind: "environment".into(),
            resource_id: row.try_get("id")?,
            ownership_kind: "shared".into(),
            expected_created_at_epoch: row.try_get("created_at")?,
            expected_revision: Some(row.try_get("revision")?),
        });
        if items.len() > MAX_SCOPE_ITEMS {
            break;
        }
    }
    if items.len() > MAX_SCOPE_ITEMS {
        return Ok(items);
    }
    for row in sqlx::query(
        "SELECT g.env_id,g.user_id,(extract(epoch FROM g.created_at)*1000000)::bigint AS created_at \
         FROM environment_grants g JOIN environments e ON e.id=g.env_id \
         JOIN projects p ON p.id=e.project_id WHERE p.org_id=$1 \
         ORDER BY g.env_id COLLATE \"C\",g.user_id COLLATE \"C\" LIMIT 4097",
    )
    .bind(org_id)
    .fetch_all(&mut **tx)
    .await?
    {
        let env_id: String = row.try_get("env_id")?;
        let user_id: String = row.try_get("user_id")?;
        items.push(ScopeItem {
            job_id: job_id.into(),
            resource_kind: "grant".into(),
            resource_id: serde_json::json!([env_id, user_id]).to_string(),
            ownership_kind: "shared".into(),
            expected_created_at_epoch: row.try_get("created_at")?,
            expected_revision: None,
        });
        if items.len() > MAX_SCOPE_ITEMS {
            return Ok(items);
        }
    }
    for row in sqlx::query(
        "SELECT m.id,m.env_id,(extract(epoch FROM m.created_at)*1000000)::bigint AS created_at \
         FROM machine_tokens m JOIN environments e ON e.id=m.env_id \
         JOIN projects p ON p.id=e.project_id WHERE p.org_id=$1 \
         ORDER BY m.id COLLATE \"C\" LIMIT 4097",
    )
    .bind(org_id)
    .fetch_all(&mut **tx)
    .await?
    {
        let token_id: String = row.try_get("id")?;
        let env_id: String = row.try_get("env_id")?;
        items.push(ScopeItem {
            job_id: job_id.into(),
            resource_kind: "machine_token".into(),
            resource_id: serde_json::json!([token_id, env_id]).to_string(),
            ownership_kind: "shared".into(),
            expected_created_at_epoch: row.try_get("created_at")?,
            expected_revision: None,
        });
        if items.len() > MAX_SCOPE_ITEMS {
            return Ok(items);
        }
    }
    Ok(items)
}

async fn create_orphan_hold(
    tx: &mut Transaction<'_, Postgres>,
    org_id: &str,
    generation: i64,
    now: i64,
) -> Result<()> {
    let episode = format!("orphan:g{generation}");
    cancel_other_episodes(tx, org_id, &episode).await?;
    let intent = held_intent(
        org_id,
        generation,
        now,
        ScopeKind::OrphanOrganisation,
        &episode,
        "orphan:no-owner-admin",
    );
    let _ = retention::enqueue_in_tx(tx, &intent)
        .await
        .map_err(map_retention_error)?;
    sqlx::query(
        "UPDATE cloud_retention_jobs SET state='held',hold_code='orphan_without_owner_admin',updated_at=now() \
         WHERE scope_kind='orphan_organisation' AND scope_key=$1 AND eligibility_episode=$2 \
           AND state IN ('planned','ready','held') AND dry_run=TRUE AND purge_enabled=FALSE",
    )
    .bind(org_id).bind(episode).execute(&mut **tx).await?;
    Ok(())
}

async fn create_held_episode(
    tx: &mut Transaction<'_, Postgres>,
    org_id: &str,
    generation: i64,
    now: i64,
    reason: &str,
    _members: &[String],
) -> Result<()> {
    let episode = format!("held:{reason}:g{generation}");
    let event = format!(
        "shared-hold:{}",
        fingerprint(&format!("{org_id}:{reason}:{generation}"))
    );
    cancel_other_episodes(tx, org_id, &episode).await?;
    let intent = held_intent(
        org_id,
        generation,
        now,
        ScopeKind::SharedOrganisation,
        &episode,
        &event,
    );
    let outcome = retention::enqueue_in_tx(tx, &intent)
        .await
        .map_err(map_retention_error)?;
    let job_id = if outcome == EnqueueOutcome::Enqueued {
        intent.job_id
    } else {
        sqlx::query_scalar("SELECT job_id FROM cloud_retention_jobs WHERE scope_kind='shared_organisation' AND scope_key=$1 AND eligibility_episode=$2")
            .bind(org_id).bind(&episode).fetch_one(&mut **tx).await?
    };
    hold_job(tx, &job_id, reason).await
}

fn held_intent(
    org_id: &str,
    generation: i64,
    _now: i64,
    kind: ScopeKind,
    episode: &str,
    event: &str,
) -> RetentionIntent {
    RetentionIntent {
        job_id: retention::new_job_id(),
        scope_kind: kind,
        scope_key: org_id.into(),
        subject_user_id: None,
        organisation_id: Some(org_id.into()),
        eligibility_episode: episode.into(),
        // Held episodes have no automatic deadline. Keep the required database placeholder
        // stable so repeated scans compare as the same idempotent episode.
        deadline_epoch: 1,
        notice_event_key: event.into(),
        notice_evidence_epoch: None,
        expected_coverage_revision: None,
        expected_scope_generation: Some(generation),
        eligibility_loss_epoch: None,
    }
}

async fn hold_job(tx: &mut Transaction<'_, Postgres>, job_id: &str, reason: &str) -> Result<()> {
    sqlx::query("UPDATE cloud_retention_jobs SET state='held',hold_code=$2,lease_owner=NULL,lease_expires_at=NULL,updated_at=now() WHERE job_id=$1 AND state IN ('planned','ready','dry_run','held','leased') AND dry_run=TRUE AND purge_enabled=FALSE")
        .bind(job_id).bind(reason).execute(&mut **tx).await?;
    Ok(())
}

async fn cancel_candidates(
    tx: &mut Transaction<'_, Postgres>,
    org_id: &str,
    reason: &str,
) -> Result<()> {
    sqlx::query("UPDATE cloud_retention_jobs SET state='cancelled',hold_code=$2,lease_owner=NULL,lease_expires_at=NULL,updated_at=now() WHERE organisation_id=$1 AND scope_kind IN ('shared_organisation','orphan_organisation') AND state IN ('planned','ready','dry_run','held','leased') AND dry_run=TRUE AND purge_enabled=FALSE")
        .bind(org_id).bind(reason).execute(&mut **tx).await?;
    sqlx::query("UPDATE cloud_notice_outbox SET status='cancelled',cancelled_at=now(),lease_owner=NULL,lease_expires_at=NULL,updated_at=now() WHERE scope_id=$1 AND kind='shared_export_deadline' AND status IN ('pending','leased')")
        .bind(org_id).execute(&mut **tx).await?;
    Ok(())
}

async fn cancel_other_episodes(
    tx: &mut Transaction<'_, Postgres>,
    org_id: &str,
    current_episode: &str,
) -> Result<()> {
    sqlx::query("UPDATE cloud_retention_jobs SET state='cancelled',hold_code='episode_superseded',lease_owner=NULL,lease_expires_at=NULL,updated_at=now() WHERE organisation_id=$1 AND scope_kind IN ('shared_organisation','orphan_organisation') AND eligibility_episode<>$2 AND state IN ('planned','ready','dry_run','held','leased') AND dry_run=TRUE AND purge_enabled=FALSE")
        .bind(org_id).bind(current_episode).execute(&mut **tx).await?;
    Ok(())
}

async fn cancel_superseded_notices(
    tx: &mut Transaction<'_, Postgres>,
    org_id: &str,
    current_event_key: &str,
) -> Result<()> {
    sqlx::query(
        "UPDATE cloud_notice_outbox SET status='cancelled',cancelled_at=now(), \
             lease_owner=NULL,lease_expires_at=NULL,updated_at=now() \
         WHERE scope_id=$1 AND policy_key=$2 AND kind='shared_export_deadline' \
           AND event_key<>$3 AND status IN ('pending','leased')",
    )
    .bind(org_id)
    .bind(SHARED_RETENTION_POLICY)
    .bind(current_event_key)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn hold_existing(pool: &PgPool, org_id: &str, reason: &str) -> Result<()> {
    sqlx::query("UPDATE cloud_retention_jobs SET state='held',hold_code=CASE WHEN state='held' AND hold_code='operator_hold' THEN hold_code ELSE $2 END,lease_owner=NULL,lease_expires_at=NULL,updated_at=now() WHERE organisation_id=$1 AND scope_kind IN ('shared_organisation','orphan_organisation') AND state IN ('planned','ready','dry_run','held','leased') AND dry_run=TRUE AND purge_enabled=FALSE")
        .bind(org_id).bind(reason).execute(pool).await?;
    Ok(())
}

fn shared_event_key(org_id: &str, loss_epoch: i64) -> String {
    format!(
        "cloud-shared-export:{}",
        fingerprint(&format!("{}:{}:{loss_epoch}", org_id.len(), org_id))
    )
}

fn fingerprint(value: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(value.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
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
        _ => Error::Internal("Cloud shared retention state is inconsistent".into()),
    }
}
