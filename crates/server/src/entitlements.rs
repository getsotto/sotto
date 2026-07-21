//! Org entitlements: tiers, the 14-day Team trial, and server-enforced quotas.
//!
//! The org is the paid unit. The *effective* tier is `Team` while `tier = 'team'` or the trial is
//! still running; otherwise the free limits apply. Enforcement happens at creation points (adding
//! members, creating org projects) and feature gates (the audit log) — never on reads of data a
//! team already has, so an expired trial degrades but never locks anyone out of their secrets.
//!
//! Tier assignment: [`crate::billing`] flips `tier` from verified Stripe webhooks when billing is
//! configured; an operator `UPDATE organizations SET tier = 'team'` remains the manual fallback.

use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use sqlx::{PgPool, Postgres, Transaction};

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

/// Resolve the effective tier inside `tx`, taking a `FOR UPDATE` lock on the org row so a quota
/// check and the write it guards are atomic against concurrent quota-affecting writes on the org.
async fn effective_tier_locked(tx: &mut Transaction<'_, Postgres>, org_id: &str) -> Result<Tier> {
    let row: Option<(String, bool)> = sqlx::query_as(
        "SELECT tier, (trial_ends_at IS NOT NULL AND trial_ends_at > now()) \
         FROM organizations WHERE id = $1 FOR UPDATE",
    )
    .bind(org_id)
    .fetch_optional(&mut **tx)
    .await?;
    let (tier, trial_active) =
        row.ok_or_else(|| Error::NotFound("organization not found".into()))?;
    if tier == "team" || trial_active {
        Ok(Tier::Team)
    } else {
        Ok(Tier::Free)
    }
}

/// Quota gate for adding a member (free tier: at most [`FREE_MAX_MEMBERS`]). Runs inside `tx` and
/// locks the org row, so the count and the caller's insert commit atomically — two concurrent adds
/// can't both pass the check and overshoot the limit.
pub async fn check_can_add_member(tx: &mut Transaction<'_, Postgres>, org_id: &str) -> Result<()> {
    if effective_tier_locked(tx, org_id).await? == Tier::Team {
        return Ok(());
    }
    let members: i64 =
        sqlx::query_scalar("SELECT count(*) FROM organization_memberships WHERE org_id = $1")
            .bind(org_id)
            .fetch_one(&mut **tx)
            .await?;
    if members >= FREE_MAX_MEMBERS {
        return Err(Error::Quota(format!(
            "the free tier allows {FREE_MAX_MEMBERS} members per organization; upgrade to add more"
        )));
    }
    Ok(())
}

/// Quota gate for creating an org project (free tier: at most [`FREE_MAX_ORG_PROJECTS`]). Runs
/// inside `tx` and locks the org row, so the count and the caller's insert commit atomically — two
/// concurrent creates can't both pass the check and overshoot the limit. The quota applies only to a
/// genuinely new `project_id`: an idempotent re-create of one that already exists always passes (so
/// re-sent `push` creates keep working for an org already at the limit), and that check runs under
/// the same lock so a concurrent re-create of a just-created id isn't mistaken for a new one.
pub async fn check_can_create_org_project(
    tx: &mut Transaction<'_, Postgres>,
    org_id: &str,
    project_id: &str,
) -> Result<()> {
    if effective_tier_locked(tx, org_id).await? == Tier::Team {
        return Ok(());
    }
    let exists: Option<i32> = sqlx::query_scalar("SELECT 1 FROM projects WHERE id = $1")
        .bind(project_id)
        .fetch_optional(&mut **tx)
        .await?;
    if exists.is_some() {
        return Ok(());
    }
    let projects: i64 = sqlx::query_scalar("SELECT count(*) FROM projects WHERE org_id = $1")
        .bind(org_id)
        .fetch_one(&mut **tx)
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
    /// Whether this instance can take payment (the `STRIPE_*` variables are set). Clients hide
    /// upgrade affordances when `false` — a self-hosted instance has no checkout to offer.
    billing_enabled: bool,
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
        // `AT TIME ZONE 'UTC'` normalizes the timestamptz to UTC before formatting, so the trailing
        // `Z` is truthful regardless of the DB session's time zone.
        "SELECT tier, to_char(trial_ends_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') \
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
        billing_enabled: state.billing.is_some(),
    }))
}
