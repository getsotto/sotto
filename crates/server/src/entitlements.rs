//! Org entitlements: tiers, the 14-day Team trial, and server-enforced quotas.
//!
//! The org is the paid unit. The *effective* tier is `Team` while `tier = 'team'` or the trial is
//! still running; otherwise the free limits apply. Enforcement happens at creation points (adding
//! members, creating org projects) and feature gates (the audit log) — never on reads of data a
//! team already has, so an expired trial degrades but never locks anyone out of their secrets.
//!
//! Tier assignment is manual for now (an operator `UPDATE organizations SET tier = 'team'`);
//! Stripe checkout becomes a thin later PR on top of this machinery.

use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use sqlx::PgPool;

use crate::auth::AuthUser;
use crate::error::{Error, Result};
use crate::org;
use crate::state::AppState;

/// Free-tier quota: members per organization (including the owner).
pub const FREE_MAX_MEMBERS: i64 = 3;
/// Free-tier quota: projects per organization.
pub const FREE_MAX_ORG_PROJECTS: i64 = 1;

pub fn router() -> Router<AppState> {
    Router::new().route("/orgs/{org_id}/entitlements", get(get_entitlements))
}

/// An org's effective tier, after accounting for a running trial.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Free,
    Team,
}

/// Resolve the effective tier: `Team` while assigned as such or while the trial runs.
pub async fn effective_tier(pool: &PgPool, org_id: &str) -> Result<Tier> {
    let row: Option<(String, bool)> = sqlx::query_as(
        "SELECT tier, (trial_ends_at IS NOT NULL AND trial_ends_at > now()) \
         FROM organizations WHERE id = $1",
    )
    .bind(org_id)
    .fetch_optional(pool)
    .await?;
    let (tier, trial_active) =
        row.ok_or_else(|| Error::NotFound("organization not found".into()))?;
    if tier == "team" || trial_active {
        Ok(Tier::Team)
    } else {
        Ok(Tier::Free)
    }
}

/// Gate a Team-only feature: `402` with an actionable message on the free tier.
pub async fn require_team(pool: &PgPool, org_id: &str, feature: &str) -> Result<()> {
    match effective_tier(pool, org_id).await? {
        Tier::Team => Ok(()),
        Tier::Free => Err(Error::Quota(format!(
            "{feature} is a Team feature; upgrade the organization to keep using it"
        ))),
    }
}

/// Quota gate for adding a member (free tier: at most [`FREE_MAX_MEMBERS`]).
pub async fn check_can_add_member(pool: &PgPool, org_id: &str) -> Result<()> {
    if effective_tier(pool, org_id).await? == Tier::Team {
        return Ok(());
    }
    let members: i64 =
        sqlx::query_scalar("SELECT count(*) FROM organization_memberships WHERE org_id = $1")
            .bind(org_id)
            .fetch_one(pool)
            .await?;
    if members >= FREE_MAX_MEMBERS {
        return Err(Error::Quota(format!(
            "the free tier allows {FREE_MAX_MEMBERS} members per organization; upgrade to add more"
        )));
    }
    Ok(())
}

/// Quota gate for creating a NEW org project (free tier: at most [`FREE_MAX_ORG_PROJECTS`]).
/// Callers must apply this only to genuinely new projects — idempotent re-creates (every `push`
/// re-sends the create) must keep working for orgs already at the limit.
pub async fn check_can_create_org_project(pool: &PgPool, org_id: &str) -> Result<()> {
    if effective_tier(pool, org_id).await? == Tier::Team {
        return Ok(());
    }
    let projects: i64 = sqlx::query_scalar("SELECT count(*) FROM projects WHERE org_id = $1")
        .bind(org_id)
        .fetch_one(pool)
        .await?;
    if projects >= FREE_MAX_ORG_PROJECTS {
        return Err(Error::Quota(format!(
            "the free tier allows {FREE_MAX_ORG_PROJECTS} project per organization; upgrade to add more"
        )));
    }
    Ok(())
}

#[derive(Serialize)]
struct Limits {
    max_members: i64,
    max_org_projects: i64,
}

#[derive(Serialize)]
struct EntitlementsView {
    /// The assigned tier (`free` or `team`).
    tier: String,
    /// The tier currently in effect (`team` during a trial).
    effective_tier: String,
    /// RFC 3339 end of the trial, if one was ever started.
    trial_ends_at: Option<String>,
    /// The numeric limits in effect; `null` on the Team tier (unlimited).
    limits: Option<Limits>,
}

/// `GET /orgs/{org_id}/entitlements` — the org's plan, visible to any member.
async fn get_entitlements(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<String>,
) -> Result<Json<EntitlementsView>> {
    if org::role_of(&state.pool, &org_id, &user.user_id)
        .await?
        .is_none()
    {
        return Err(Error::NotFound("organization not found".into()));
    }

    let (tier, trial_ends_at): (String, Option<String>) = sqlx::query_as(
        "SELECT tier, to_char(trial_ends_at, 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') \
         FROM organizations WHERE id = $1",
    )
    .bind(&org_id)
    .fetch_one(&state.pool)
    .await?;
    let effective = effective_tier(&state.pool, &org_id).await?;

    Ok(Json(EntitlementsView {
        tier,
        effective_tier: match effective {
            Tier::Team => "team".into(),
            Tier::Free => "free".into(),
        },
        trial_ends_at,
        limits: match effective {
            Tier::Team => None,
            Tier::Free => Some(Limits {
                max_members: FREE_MAX_MEMBERS,
                max_org_projects: FREE_MAX_ORG_PROJECTS,
            }),
        },
    }))
}
