//! Staged HTTP adapter for organisation deletion.
//!
//! [`router`] is intentionally absent from [`crate::app`]. Tests exercise the complete wire
//! contract now, while the production route remains unavailable until the later enablement PR.

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
    state: &'static str,
    requested_at: String,
    recoverable_until: String,
    managed_backup_expiry_by: Option<String>,
    next_retry_at: Option<String>,
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

async fn request_deletion(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<String>,
    Json(body): Json<RequestDeletion>,
) -> Result<(StatusCode, Json<DeletionResponse>)> {
    let confirmation = body
        .confirm_org_id
        .as_deref()
        .ok_or_else(|| Error::BadRequest("deletion confirmation is required".into()))?;
    if body.acknowledge_subscription_cancellation != Some(true) {
        return Err(Error::BadRequest(
            "subscription cancellation acknowledgement is required".into(),
        ));
    }
    org_deletion::request(&state.pool, &org_id, &user.user_id, confirmation).await?;
    let status = org_deletion::status(&state.pool, &org_id, &user.user_id).await?;
    Ok((StatusCode::ACCEPTED, Json(status.into())))
}

async fn get_deletion(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<String>,
) -> Result<Json<DeletionResponse>> {
    let status = org_deletion::status(&state.pool, &org_id, &user.user_id).await?;
    Ok(Json(status.into()))
}

async fn cancel_deletion(
    State(state): State<AppState>,
    user: AuthUser,
    Path(org_id): Path<String>,
) -> Result<(StatusCode, Json<DeletionResponse>)> {
    org_deletion::cancel(&state.pool, &org_id, &user.user_id).await?;
    let status = org_deletion::status(&state.pool, &org_id, &user.user_id).await?;
    Ok((StatusCode::ACCEPTED, Json(status.into())))
}
