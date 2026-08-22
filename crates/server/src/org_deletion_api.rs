//! Staged HTTP adapter for organisation deletion.
//!
//! [`router`] is intentionally absent from [`crate::app`]. Tests exercise the complete wire
//! contract now, while the production route remains unavailable until the later enablement PR.

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::auth::AuthUser;
use crate::error::{Error, Result};
use crate::org_deletion::{self, DeletionStatus};
use crate::state::AppState;

/// Build the deletion routes for the test-only router. Production enablement must explicitly merge
/// this router into [`crate::app`] after the remaining client, operations, and safety gates exist.
pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/orgs/{org_id}/deletion",
            get(get_deletion).post(request_deletion),
        )
        .route("/orgs/{org_id}/deletion/cancel", post(cancel_deletion))
}

#[derive(Deserialize)]
struct RequestDeletion {
    confirm_org_id: Option<String>,
    acknowledge_subscription_cancellation: Option<bool>,
}

#[derive(Serialize)]
struct DeletionResponse {
    /// Current lifecycle phase from the closed [`org_deletion::DeletionState`] vocabulary.
    state: &'static str,
    /// Time the first accepted request fixed the operation's recovery window.
    requested_at: String,
    /// Final recovery deadline, backed by the operation's immutable `purge_after` value.
    recoverable_until: String,
    /// Latest managed-backup expiry when the operations policy has supplied one.
    managed_backup_expiry_by: Option<String>,
    /// Next scheduled worker attempt, or `None` when no retry or transition is queued.
    next_retry_at: Option<String>,
    /// Sanitised owner-visible failure code; provider messages and identifiers never appear here.
    error: Option<&'static str>,
}

impl From<DeletionStatus> for DeletionResponse {
    fn from(status: DeletionStatus) -> Self {
        let error = status.public_error();
        Self {
            state: status.state.as_str(),
            requested_at: status.requested_at,
            recoverable_until: status.recoverable_until,
            managed_backup_expiry_by: status.managed_backup_expiry_by,
            next_retry_at: status.next_retry_at,
            error,
        }
    }
}

/// `POST /orgs/{org_id}/deletion` - accept an owner's exact, explicit confirmation. The first and
/// repeated valid requests return `202`; malformed confirmations return `400` without mutation.
async fn request_deletion(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<String>,
    body: std::result::Result<Json<RequestDeletion>, JsonRejection>,
) -> Result<(StatusCode, Json<DeletionResponse>)> {
    // Axum distinguishes syntax and data errors with different default statuses. Discard the
    // parser detail rather than logging or exposing client input, and present one stable 400
    // contract for every malformed deletion confirmation instead.
    let Json(body) = body.map_err(|_| Error::BadRequest("invalid deletion request".into()))?;
    let confirmation = body
        .confirm_org_id
        .as_deref()
        .ok_or_else(|| Error::BadRequest("deletion confirmation is required".into()))?;
    if body.acknowledge_subscription_cancellation != Some(true) {
        return Err(Error::BadRequest(
            "subscription cancellation acknowledgement is required".into(),
        ));
    }
    let operation = org_deletion::request_with_retention(
        &state.pool,
        &org_id,
        &user.user_id,
        confirmation,
        state.organisation_deletion_retention_days,
    )
    .await?;
    // Refetch the authorised operation by id so first and repeated requests share one projection;
    // a concurrent transition may update its state without changing terminal visibility here.
    let status = org_deletion::status_after_mutation(&state.pool, &operation).await?;
    Ok((StatusCode::ACCEPTED, Json(status.into())))
}

/// `GET /orgs/{org_id}/deletion` - return an owner-visible active or permitted terminal status.
async fn get_deletion(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<String>,
) -> Result<Json<DeletionResponse>> {
    let status = org_deletion::status(&state.pool, &org_id, &user.user_id).await?;
    Ok(Json(status.into()))
}

/// `POST /orgs/{org_id}/deletion/cancel` - begin idempotent owner recovery with `202`, or return
/// `409` once purge has started and recovery is no longer safe.
async fn cancel_deletion(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<String>,
) -> Result<(StatusCode, Json<DeletionResponse>)> {
    let operation = org_deletion::cancel(&state.pool, &org_id, &user.user_id).await?;
    // Recovery may complete after cancellation commits. Refetching the authorised operation by id
    // reports that newer state without turning a successful second-owner request into a 404.
    let status = org_deletion::status_after_mutation(&state.pool, &operation).await?;
    Ok((StatusCode::ACCEPTED, Json(status.into())))
}
