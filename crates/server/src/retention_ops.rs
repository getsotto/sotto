//! Protected, redacted inspection, operator review, and separately gated retention controls.

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::header::CACHE_CONTROL;
use axum::http::{HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::error::{Error, Result};
use crate::org_deletion_metrics::{bearer_token, token_matches};
use crate::retention::{self, OperatorAction, RetentionError};
use crate::state::AppState;

const MAX_PAGE_SIZE: i64 = 100;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/ops/cloud/retention", get(list_jobs))
        .route("/ops/cloud/retention/inventory", get(list_inventory))
        .route("/ops/cloud/retention/{fingerprint}", post(apply_action))
}

/// Separate destructive credentials protect restore attestations and purge approval. These
/// endpoints are mounted only when the purge switch and all required secrets are configured.
pub fn purge_router() -> Router<AppState> {
    Router::new()
        .route(
            "/ops/cloud/retention/purge/restore-evidence",
            post(register_restore_evidence),
        )
        .route(
            "/ops/cloud/retention/purge/{fingerprint}/review",
            get(review_purge_scope),
        )
        .route(
            "/ops/cloud/retention/purge/{fingerprint}/approve",
            post(approve_purge),
        )
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RestoreEvidenceRequest {
    operator: String,
    evidence: retention::RestoreEvidenceAttestation,
}

async fn register_restore_evidence(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: std::result::Result<Json<RestoreEvidenceRequest>, JsonRejection>,
) -> Result<Response> {
    require_destructive_operator(&state, &headers)?;
    let Json(body) = body.map_err(|_| Error::BadRequest("invalid restore evidence".into()))?;
    validate_operator(&body.operator)?;
    let key = state
        .cloud_retention_evidence_hmac_key
        .as_deref()
        .ok_or_else(|| {
            Error::NotConfigured("Cloud retention restore evidence is disabled".into())
        })?;
    let release_sha = state.cloud_retention_build_sha.as_deref().ok_or_else(|| {
        Error::NotConfigured("Cloud retention release identity is unavailable".into())
    })?;
    let inserted = retention::register_restore_evidence(
        &state.pool,
        &body.evidence,
        key.as_bytes(),
        release_sha,
        &body.operator,
    )
    .await
    .map_err(map_retention_error)?;
    no_store(Json(serde_json::json!({ "recorded": inserted })))
}

#[derive(Debug, Serialize)]
struct PurgeReviewResponse {
    scope_fingerprint: String,
    item_count: i64,
    state: &'static str,
    deadline_epoch: i64,
}

async fn review_purge_scope(
    State(state): State<AppState>,
    Path(fingerprint): Path<String>,
    headers: HeaderMap,
) -> Result<Response> {
    require_destructive_operator(&state, &headers)?;
    if !is_fingerprint(&fingerprint) {
        return Err(Error::BadRequest(
            "retention job reference is invalid".into(),
        ));
    }
    let row = sqlx::query(
        "SELECT job.job_id,job.state,extract(epoch FROM job.deadline_at)::bigint AS deadline_epoch, \
                count(item.resource_id) AS item_count \
         FROM cloud_retention_jobs job LEFT JOIN cloud_retention_scope_items item USING(job_id) \
         WHERE job.operator_fingerprint=$1 AND job.scope_kind IN ('personal','shared_organisation') \
         GROUP BY job.job_id",
    )
    .bind(&fingerprint)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| Error::NotFound("retention job not found".into()))?;
    let job_id: String = row.try_get("job_id")?;
    let scope_fingerprint = retention::review_scope_fingerprint(&state.pool, &job_id)
        .await
        .map_err(map_retention_error)?;
    let response = PurgeReviewResponse {
        scope_fingerprint,
        item_count: row.try_get("item_count")?,
        state: safe_state(&row.try_get::<String, _>("state")?),
        deadline_epoch: row.try_get("deadline_epoch")?,
    };
    no_store(Json(response))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ApprovePurgeRequest {
    operator: String,
    scope_fingerprint: String,
    evidence_id: String,
}

async fn approve_purge(
    State(state): State<AppState>,
    Path(fingerprint): Path<String>,
    headers: HeaderMap,
    body: std::result::Result<Json<ApprovePurgeRequest>, JsonRejection>,
) -> Result<Response> {
    require_destructive_operator(&state, &headers)?;
    if !is_fingerprint(&fingerprint) {
        return Err(Error::BadRequest(
            "retention job reference is invalid".into(),
        ));
    }
    let Json(body) = body.map_err(|_| Error::BadRequest("invalid purge approval".into()))?;
    validate_operator(&body.operator)?;
    let job_id: Option<String> = sqlx::query_scalar(
        "SELECT job_id FROM cloud_retention_jobs WHERE operator_fingerprint=$1 \
         AND scope_kind IN ('personal','shared_organisation')",
    )
    .bind(&fingerprint)
    .fetch_optional(&state.pool)
    .await?;
    let Some(job_id) = job_id else {
        return Err(Error::NotFound("retention job not found".into()));
    };
    let release_sha = state.cloud_retention_build_sha.as_deref().ok_or_else(|| {
        Error::NotConfigured("Cloud retention release identity is unavailable".into())
    })?;
    let stored_fingerprint = retention::approve_purge(
        &state.pool,
        &job_id,
        &body.scope_fingerprint,
        &body.evidence_id,
        &body.operator,
        release_sha,
    )
    .await
    .map_err(map_retention_error)?;
    no_store(Json(
        serde_json::json!({ "approved": true, "scope_fingerprint": stored_fingerprint }),
    ))
}

fn require_destructive_operator(state: &AppState, headers: &HeaderMap) -> Result<()> {
    if !state.cloud_retention_purge_enabled {
        return Err(Error::NotConfigured(
            "Cloud retention purge is disabled".into(),
        ));
    }
    let expected = state
        .cloud_retention_destructive_token
        .as_deref()
        .ok_or_else(|| {
            Error::NotConfigured("Cloud retention destructive controls are disabled".into())
        })?;
    let provided = bearer_token(headers).ok_or(Error::Unauthorized)?;
    if !token_matches(expected, provided) {
        return Err(Error::Unauthorized);
    }
    Ok(())
}

fn validate_operator(operator: &str) -> Result<()> {
    if operator.trim().is_empty() || operator.len() > 80 || operator.chars().any(char::is_control) {
        return Err(Error::BadRequest("operator label is invalid".into()));
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct OrganisationInventoryBucket {
    lifecycle: &'static str,
    authority: &'static str,
    project_band: &'static str,
    organisations: i64,
    projects: i64,
    environments: i64,
    grants: i64,
}

#[derive(Debug, Serialize)]
struct ShareInventoryBucket {
    share_class: &'static str,
    lifetime: &'static str,
    view_limit: &'static str,
    state: &'static str,
    count: i64,
}

#[derive(Debug, Serialize)]
struct InventoryResponse {
    organisations: Vec<OrganisationInventoryBucket>,
    shares: Vec<ShareInventoryBucket>,
}

/// Return bounded aggregate facts for the policy-gated shared and link retention review.
///
/// This endpoint deliberately exposes no resource identifiers and creates no retention jobs.
/// The query runs against one read-only snapshot and is capped so a large deployment fails
/// visibly rather than returning a partial inventory.
async fn list_inventory(State(state): State<AppState>, headers: HeaderMap) -> Result<Response> {
    require_operator(&state, &headers)?;

    let mut tx = state.pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL statement_timeout = '3s'")
        .execute(&mut *tx)
        .await?;

    let organisation_rows = sqlx::query(
        "WITH member_counts AS ( \
             SELECT org_id, count(*) AS members, \
                    bool_or(role IN ('owner','admin')) AS has_admin \
             FROM organization_memberships GROUP BY org_id \
         ), resource_counts AS ( \
             SELECT p.org_id, count(DISTINCT p.id) AS projects, \
                    count(DISTINCT e.id) AS environments, count(g.env_id) AS grants \
             FROM projects p \
             LEFT JOIN environments e ON e.project_id=p.id \
             LEFT JOIN environment_grants g ON g.env_id=e.id \
             WHERE p.org_id IS NOT NULL GROUP BY p.org_id \
         ), per_organisation AS ( \
             SELECT o.lifecycle_state, \
                    CASE WHEN coalesce(m.has_admin,FALSE) THEN 'owner_or_admin' \
                         WHEN coalesce(m.members,0)>0 THEN 'members_only' \
                         ELSE 'no_members' END AS authority, \
                    CASE WHEN coalesce(r.projects,0)=0 THEN 'none' \
                         WHEN r.projects=1 THEN 'one' ELSE 'multiple' END AS project_band, \
                    coalesce(r.projects,0) AS projects, \
                    coalesce(r.environments,0) AS environments, \
                    coalesce(r.grants,0) AS grants \
             FROM organizations o \
             LEFT JOIN member_counts m ON m.org_id=o.id \
             LEFT JOIN resource_counts r ON r.org_id=o.id \
         ) \
         SELECT lifecycle_state, authority, project_band, count(*) AS organisations, \
                sum(projects)::bigint AS projects, \
                sum(environments)::bigint AS environments, \
                sum(grants)::bigint AS grants \
         FROM per_organisation \
         GROUP BY lifecycle_state, authority, project_band \
         ORDER BY lifecycle_state, authority, project_band",
    )
    .fetch_all(&mut *tx)
    .await?;
    let organisations = organisation_rows
        .into_iter()
        .map(|row| {
            Ok(OrganisationInventoryBucket {
                lifecycle: safe_lifecycle(&row.try_get::<String, _>("lifecycle_state")?),
                authority: safe_authority(&row.try_get::<String, _>("authority")?),
                project_band: safe_project_band(&row.try_get::<String, _>("project_band")?),
                organisations: row.try_get("organisations")?,
                projects: row.try_get("projects")?,
                environments: row.try_get("environments")?,
                grants: row.try_get("grants")?,
            })
        })
        .collect::<std::result::Result<Vec<_>, sqlx::Error>>()?;

    let share_rows = sqlx::query(
        "SELECT share_class, \
                CASE WHEN expires_at IS NULL THEN 'no_expiry' ELSE 'finite' END AS lifetime, \
                CASE WHEN max_views = 1 THEN 'single_use' ELSE 'multi_use' END AS view_limit, \
                CASE WHEN revoked_at IS NOT NULL THEN 'revoked' \
                     WHEN view_count >= max_views THEN 'consumed' \
                     WHEN expires_at IS NOT NULL AND expires_at <= now() THEN 'expired' \
                     ELSE 'active' END AS state, \
                count(*) AS count \
         FROM share_links \
         GROUP BY share_class, lifetime, view_limit, state \
         ORDER BY share_class, lifetime, view_limit, state",
    )
    .fetch_all(&mut *tx)
    .await?;
    let shares = share_rows
        .into_iter()
        .map(|row| {
            Ok(ShareInventoryBucket {
                share_class: safe_share_class(&row.try_get::<String, _>("share_class")?),
                lifetime: safe_lifetime(&row.try_get::<String, _>("lifetime")?),
                view_limit: safe_view_limit(&row.try_get::<String, _>("view_limit")?),
                state: safe_share_state(&row.try_get::<String, _>("state")?),
                count: row.try_get("count")?,
            })
        })
        .collect::<std::result::Result<Vec<_>, sqlx::Error>>()?;

    tx.commit().await?;
    no_store(Json(InventoryResponse {
        organisations,
        shares,
    }))
}

#[derive(Debug, Deserialize)]
struct ListQuery {
    after: Option<String>,
    limit: Option<i64>,
}

#[derive(Debug, Serialize)]
struct StateCount {
    state: &'static str,
    count: i64,
}

#[derive(Debug, Serialize)]
struct HoldCount {
    reason: &'static str,
    count: i64,
}

#[derive(Debug, Serialize)]
struct JobSummary {
    fingerprint: String,
    state: &'static str,
    deadline_epoch: i64,
    dry_run_completed_at_epoch: Option<i64>,
    planned_items: i64,
    dry_run_items: i64,
    deleted_items: i64,
    held_items: i64,
    purge_enabled: bool,
    purge_approval: &'static str,
    hold_reason: Option<&'static str>,
}

#[derive(Debug, Serialize)]
struct ListResponse {
    states: Vec<StateCount>,
    holds: Vec<HoldCount>,
    jobs: Vec<JobSummary>,
    next_cursor: Option<String>,
}

async fn list_jobs(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Result<Response> {
    require_operator(&state, &headers)?;
    let limit = query.limit.unwrap_or(50);
    if !(1..=MAX_PAGE_SIZE).contains(&limit) {
        return Err(Error::BadRequest("retention page limit is invalid".into()));
    }
    if query
        .after
        .as_deref()
        .is_some_and(|cursor| !is_fingerprint(cursor))
    {
        return Err(Error::BadRequest("retention cursor is invalid".into()));
    }
    let state_rows = sqlx::query(
        "SELECT state, count(*) AS count FROM cloud_retention_jobs \
         WHERE scope_kind IN ('personal','shared_organisation','orphan_organisation') GROUP BY state ORDER BY state",
    )
    .fetch_all(&state.pool)
    .await?;
    let states = state_rows
        .into_iter()
        .filter_map(|row| {
            let state: String = row.try_get("state").ok()?;
            Some(StateCount {
                state: safe_state(&state),
                count: row.try_get("count").ok()?,
            })
        })
        .collect();
    let hold_rows = sqlx::query(
        "SELECT reason, sum(count) AS count FROM ( \
           SELECT hold_code AS reason, count(*) AS count FROM cloud_retention_jobs \
             WHERE scope_kind IN ('personal','shared_organisation','orphan_organisation') AND hold_code IS NOT NULL GROUP BY hold_code \
           UNION ALL \
           SELECT item.hold_code AS reason, count(*) AS count \
             FROM cloud_retention_scope_items item JOIN cloud_retention_jobs job USING(job_id) \
             WHERE job.scope_kind IN ('personal','shared_organisation','orphan_organisation') AND item.state='held' AND item.hold_code IS NOT NULL \
             GROUP BY item.hold_code \
         ) holds GROUP BY reason ORDER BY reason",
    )
    .fetch_all(&state.pool)
    .await?;
    let holds = hold_rows
        .into_iter()
        .filter_map(|row| {
            let reason: Option<String> = row.try_get("reason").ok()?;
            Some(HoldCount {
                reason: safe_reason(reason.as_deref()?),
                count: row.try_get("count").ok()?,
            })
        })
        .collect();
    let mut rows = sqlx::query(
        "SELECT j.operator_fingerprint, j.state,j.purge_enabled, \
                (SELECT approval.state FROM cloud_retention_purge_approvals approval \
                 WHERE approval.job_id=j.job_id ORDER BY approval.approved_at DESC LIMIT 1) AS purge_approval, \
                extract(epoch from j.deadline_at)::bigint AS deadline_epoch, \
                extract(epoch from j.dry_run_completed_at)::bigint AS dry_run_completed_at_epoch, \
                j.hold_code, \
                count(*) FILTER (WHERE i.state='planned') AS planned_items, \
                count(*) FILTER (WHERE i.state='dry_run') AS dry_run_items, \
                count(*) FILTER (WHERE i.state='deleted') AS deleted_items, \
                count(*) FILTER (WHERE i.state='held') AS held_items \
         FROM cloud_retention_jobs j LEFT JOIN cloud_retention_scope_items i USING(job_id) \
         WHERE j.scope_kind IN ('personal','shared_organisation','orphan_organisation') AND ($1::text IS NULL OR j.operator_fingerprint COLLATE \"C\" > $1 COLLATE \"C\") \
         GROUP BY j.job_id ORDER BY j.operator_fingerprint COLLATE \"C\" LIMIT $2",
    )
    .bind(query.after.as_deref())
    .bind(limit + 1)
    .fetch_all(&state.pool)
    .await?;
    let has_more = rows.len() as i64 > limit;
    if has_more {
        rows.truncate(limit as usize);
    }
    let jobs = rows
        .into_iter()
        .map(|row| {
            Ok(JobSummary {
                fingerprint: row.try_get("operator_fingerprint")?,
                state: safe_state(&row.try_get::<String, _>("state")?),
                deadline_epoch: row.try_get("deadline_epoch")?,
                dry_run_completed_at_epoch: row.try_get("dry_run_completed_at_epoch")?,
                planned_items: row.try_get("planned_items")?,
                dry_run_items: row.try_get("dry_run_items")?,
                deleted_items: row.try_get("deleted_items")?,
                held_items: row.try_get("held_items")?,
                purge_enabled: row.try_get("purge_enabled")?,
                purge_approval: safe_approval(&row.try_get::<Option<String>, _>("purge_approval")?),
                hold_reason: row
                    .try_get::<Option<String>, _>("hold_code")?
                    .as_deref()
                    .map(safe_reason),
            })
        })
        .collect::<std::result::Result<Vec<_>, sqlx::Error>>()?;
    let next_cursor = if has_more {
        jobs.last().map(|job| job.fingerprint.clone())
    } else {
        None
    };
    no_store(Json(ListResponse {
        states,
        holds,
        jobs,
        next_cursor,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActionRequest {
    operator: String,
    action: ActionKind,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ActionKind {
    Hold,
    Cancel,
    RetryDryRun,
}

async fn apply_action(
    State(state): State<AppState>,
    Path(fingerprint): Path<String>,
    headers: HeaderMap,
    body: std::result::Result<Json<ActionRequest>, JsonRejection>,
) -> Result<Response> {
    require_operator(&state, &headers)?;
    if !is_fingerprint(&fingerprint) {
        return Err(Error::BadRequest(
            "retention job reference is invalid".into(),
        ));
    }
    let Json(body) = body.map_err(|_| Error::BadRequest("invalid retention action".into()))?;
    if body.operator.trim().is_empty()
        || body.operator.len() > 80
        || body.operator.chars().any(char::is_control)
    {
        return Err(Error::BadRequest("operator label is invalid".into()));
    }
    let action = match body.action {
        ActionKind::Hold => OperatorAction::Hold,
        ActionKind::Cancel => OperatorAction::Cancel,
        ActionKind::RetryDryRun => OperatorAction::RetryDryRun,
    };
    let job_id: Option<String> = sqlx::query_scalar(
        "SELECT job_id FROM cloud_retention_jobs \
         WHERE operator_fingerprint=$1 AND scope_kind IN ('personal','shared_organisation','orphan_organisation')",
    )
    .bind(&fingerprint)
    .fetch_optional(&state.pool)
    .await?;
    let Some(job_id) = job_id else {
        return Err(Error::NotFound("retention job not found".into()));
    };
    retention::apply_operator_action(&state.pool, &job_id, &body.operator, action)
        .await
        .map_err(map_retention_error)?;
    no_store(Json(serde_json::json!({ "accepted": true })))
}

fn require_operator(state: &AppState, headers: &HeaderMap) -> Result<()> {
    let expected = state
        .cloud_retention_operator_token
        .as_deref()
        .ok_or_else(|| {
            Error::NotConfigured("Cloud retention operator controls are disabled".into())
        })?;
    let provided = bearer_token(headers).ok_or(Error::Unauthorized)?;
    if !token_matches(expected, provided) {
        return Err(Error::Unauthorized);
    }
    Ok(())
}

fn no_store(body: impl IntoResponse) -> Result<Response> {
    let mut response = body.into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

fn is_fingerprint(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn safe_state(value: &str) -> &'static str {
    match value {
        "planned" => "planned",
        "dry_run" => "dry_run",
        "ready" => "ready",
        "leased" => "leased",
        "held" => "held",
        "cancelled" => "cancelled",
        "completed" => "completed",
        "failed" => "failed",
        _ => "unknown",
    }
}

fn safe_reason(value: &str) -> &'static str {
    match value {
        "operator_hold" => "operator_hold",
        "operator_cancelled" => "operator_cancelled",
        "eligibility_unavailable" => "eligibility_unavailable",
        "coverage_resumed" => "coverage_resumed",
        "reconciliation_failed" => "reconciliation_failed",
        "coverage_revision_changed" => "coverage_revision_changed",
        "coverage_revision_missing" => "coverage_revision_missing",
        "coverage_unavailable" => "coverage_unavailable",
        "coverage_episode_changed" => "coverage_episode_changed",
        "coverage_episode_unproven" => "coverage_episode_unproven",
        "coverage_deadline_missing" => "coverage_deadline_missing",
        "coverage_deadline_invalid" => "coverage_deadline_invalid",
        "billing_state_active" => "billing_state_active",
        "notice_evidence_missing" => "notice_evidence_missing",
        "scope_exceeds_bound" => "scope_exceeds_bound",
        "scope_changed" => "scope_changed",
        "share_lifetime_unproven" => "share_lifetime_unproven",
        "share_promise_exceeds_deadline" => "share_promise_exceeds_deadline",
        "shared_resource" => "shared_resource",
        "resource_missing" => "resource_missing",
        "resource_identity_changed" => "resource_identity_changed",
        "unsupported_scope" => "unsupported_scope",
        "owner_admin_still_eligible" => "owner_admin_still_eligible",
        "owner_admin_eligibility_unproven" => "owner_admin_eligibility_unproven",
        "owner_admin_population_exceeds_bound" => "owner_admin_population_exceeds_bound",
        "owner_admin_missing" => "owner_admin_missing",
        "orphan_without_owner_admin" => "orphan_without_owner_admin",
        "no_verified_owner_contact" => "no_verified_owner_contact",
        "notice_evidence_incomplete" => "notice_evidence_incomplete",
        "notice_window_elapsed" => "notice_window_elapsed",
        "organisation_deletion_active" => "organisation_deletion_active",
        "ownership_transition_pending" => "ownership_transition_pending",
        "organisation_missing" => "organisation_missing",
        "organisation_not_active" => "organisation_not_active",
        "scope_generation_missing" => "scope_generation_missing",
        "scope_generation_changed" => "scope_generation_changed",
        "eligibility_boundary_changed" => "eligibility_boundary_changed",
        "unsupported_resource_kind" => "unsupported_resource_kind",
        "scope_owner_missing" => "scope_owner_missing",
        "episode_superseded" => "episode_superseded",
        "restore_evidence_stale" => "restore_evidence_stale",
        "purge_approval_missing" => "purge_approval_missing",
        "purge_release_identity_changed" => "purge_release_identity_changed",
        "purge_journal_changed" => "purge_journal_changed",
        "purge_policy_changed" => "purge_policy_changed",
        "retention_scope_owner_missing" => "retention_scope_owner_missing",
        "free_share_promise_after_deadline" => "free_share_promise_after_deadline",
        _ => "other",
    }
}

fn safe_approval(value: &Option<String>) -> &'static str {
    match value.as_deref() {
        Some("active") => "active",
        Some("stale") => "stale",
        Some("revoked") => "revoked",
        Some("completed") => "completed",
        None => "none",
        _ => "unknown",
    }
}

fn safe_lifecycle(value: &str) -> &'static str {
    match value {
        "active" => "active",
        "deleting" => "deleting",
        "deleted" => "deleted",
        _ => "unknown",
    }
}

fn safe_authority(value: &str) -> &'static str {
    match value {
        "owner_or_admin" => "owner_or_admin",
        "members_only" => "members_only",
        "no_members" => "no_members",
        _ => "unknown",
    }
}

fn safe_project_band(value: &str) -> &'static str {
    match value {
        "none" => "none",
        "one" => "one",
        "multiple" => "multiple",
        _ => "unknown",
    }
}

fn safe_share_class(value: &str) -> &'static str {
    match value {
        "free" => "free",
        "paid" => "paid",
        "legacy" => "legacy",
        _ => "unknown",
    }
}

fn safe_lifetime(value: &str) -> &'static str {
    match value {
        "finite" => "finite",
        "no_expiry" => "no_expiry",
        _ => "unknown",
    }
}

fn safe_view_limit(value: &str) -> &'static str {
    match value {
        "single_use" => "single_use",
        "multi_use" => "multi_use",
        _ => "unknown",
    }
}

fn safe_share_state(value: &str) -> &'static str {
    match value {
        "active" => "active",
        "revoked" => "revoked",
        "consumed" => "consumed",
        "expired" => "expired",
        _ => "unknown",
    }
}

fn map_retention_error(error: RetentionError) -> Error {
    match error {
        RetentionError::Database(error) => Error::Db(error),
        RetentionError::NotFound => {
            Error::Conflict("retention action is not valid for this job".into())
        }
        RetentionError::LeaseLost => Error::Conflict("retention job is currently changing".into()),
        _ => Error::BadRequest("retention action is invalid".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operator_output_labels_are_fixed_and_fingerprints_are_bounded() {
        assert_eq!(safe_reason("secret provider payload"), "other");
        assert_eq!(safe_state("secret state"), "unknown");
        assert!(is_fingerprint("0123456789abcdef0123456789abcdef"));
        assert!(!is_fingerprint("user-secret"));
        assert!(!is_fingerprint("0123456789ABCDEF0123456789ABCDEF"));
    }
}
