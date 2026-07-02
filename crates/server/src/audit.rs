//! The org audit log: append-only recording of team state changes, and its read endpoint.
//!
//! Handlers call [`record`] (or [`record_tx`] inside a transaction, so the event commits or rolls
//! back with the change it describes). An audit insert failure fails the operation — a log with
//! silent gaps is worse than a failed request. Reads are admin+-only and newest-first.

use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Transaction};

use crate::auth::AuthUser;
use crate::error::{Error, Result};
use crate::org;
use crate::state::AppState;

/// Default page size for the audit listing.
const DEFAULT_LIMIT: i64 = 100;
/// Hard cap on one page.
const MAX_LIMIT: i64 = 500;

pub fn router() -> Router<AppState> {
    Router::new().route("/orgs/{org_id}/audit", get(list_events))
}

/// The optional pieces of an audit event, so call sites stay readable.
#[derive(Default)]
pub struct Context<'a> {
    /// The acted-on user or token id.
    pub target: Option<&'a str>,
    /// The affected environment.
    pub env_id: Option<&'a str>,
    /// Small human-readable context (a role, a change count) — metadata only.
    pub detail: Option<&'a str>,
}

/// Record one event (auto-commit; use [`record_tx`] from inside a transaction).
pub async fn record(
    pool: &PgPool,
    org_id: &str,
    actor: &str,
    action: &str,
    ctx: Context<'_>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO audit_events (org_id, actor, action, target, env_id, detail) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(org_id)
    .bind(actor)
    .bind(action)
    .bind(ctx.target)
    .bind(ctx.env_id)
    .bind(ctx.detail)
    .execute(pool)
    .await?;
    Ok(())
}

/// Record one event inside `tx`, so it commits (or rolls back) with the change it describes.
pub async fn record_tx(
    tx: &mut Transaction<'_, Postgres>,
    org_id: &str,
    actor: &str,
    action: &str,
    ctx: Context<'_>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO audit_events (org_id, actor, action, target, env_id, detail) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(org_id)
    .bind(actor)
    .bind(action)
    .bind(ctx.target)
    .bind(ctx.env_id)
    .bind(ctx.detail)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

#[derive(Deserialize)]
struct ListParams {
    #[serde(default)]
    limit: Option<i64>,
}

#[derive(Serialize)]
struct EventView {
    id: i64,
    actor: String,
    action: String,
    target: Option<String>,
    env_id: Option<String>,
    detail: Option<String>,
    /// RFC 3339 timestamp.
    at: String,
}

/// Listing row: `(id, actor, action, target, env_id, detail, at)`.
type EventRow = (
    i64,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
);

/// `GET /orgs/{org_id}/audit?limit=N` — the org's events, newest first (admin+).
async fn list_events(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<String>,
    Query(params): Query<ListParams>,
) -> Result<Json<Vec<EventView>>> {
    match org::role_of(&state.pool, &org_id, &user.user_id).await? {
        Some(role) if role.can_manage_members() => {}
        Some(_) => {
            return Err(Error::Forbidden(
                "must be an admin or owner to read the audit log".into(),
            ))
        }
        None => return Err(Error::NotFound("organization not found".into())),
    }

    let limit = params.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let rows: Vec<EventRow> = sqlx::query_as(
        "SELECT id, actor, action, target, env_id, detail, \
                to_char(created_at, 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') \
         FROM audit_events WHERE org_id = $1 ORDER BY id DESC LIMIT $2",
    )
    .bind(&org_id)
    .bind(limit)
    .fetch_all(&state.pool)
    .await?;

    Ok(Json(
        rows.into_iter()
            .map(
                |(id, actor, action, target, env_id, detail, at)| EventView {
                    id,
                    actor,
                    action,
                    target,
                    env_id,
                    detail,
                    at,
                },
            )
            .collect(),
    ))
}
