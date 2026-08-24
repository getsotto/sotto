//! Protected operational controls for the staged organisation-deletion worker.
//!
//! The operator route is separate from the user-facing deletion adapter and remains unavailable
//! until a deployment sets its dedicated bearer token. It records the same audited billing
//! observation used by the worker, rather than providing a bypass around the purge gate.

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::org_deletion::{self, DeletionStatus, OperatorObservation};
use crate::org_deletion_metrics::{bearer_token, token_matches};
use crate::state::AppState;

/// Build the protected operator-observation route without registering user-facing deletion.
pub fn router() -> Router<AppState> {
    Router::new().route(
        "/ops/organisation-deletion/{org_id}/billing-observation",
        post(record_observation),
    )
}

#[derive(Deserialize)]
struct OperatorObservationRequest {
    /// Audit label supplied by the authenticated operator; the bearer token grants authority.
    operator: String,
    subscription_id: String,
    observed_status: String,
    observed_at: String,
    reason: String,
    evidence: String,
    managed_backup_expiry_by: Option<String>,
}

#[derive(Serialize)]
struct OperatorObservationResponse {
    state: &'static str,
    requested_at: String,
    recoverable_until: String,
    managed_backup_expiry_by: Option<String>,
    next_retry_at: Option<String>,
    error: Option<&'static str>,
}

impl From<DeletionStatus> for OperatorObservationResponse {
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

/// `POST /ops/organisation-deletion/{org_id}/billing-observation` - record an authenticated
/// operator's terminal or missing billing observation and return the sanitised operation status.
async fn record_observation(
    State(state): State<AppState>,
    Path(org_id): Path<String>,
    headers: HeaderMap,
    body: std::result::Result<Json<OperatorObservationRequest>, JsonRejection>,
) -> Result<Json<OperatorObservationResponse>> {
    let expected = state
        .organisation_deletion_operator_token
        .as_deref()
        .ok_or_else(|| {
            Error::NotConfigured("organisation-deletion operator controls are not enabled".into())
        })?;
    let provided = bearer_token(&headers).ok_or(Error::Unauthorized)?;
    if !token_matches(expected, provided) {
        return Err(Error::Unauthorized);
    }
    let Json(body) = body.map_err(|_| Error::BadRequest("invalid operator observation".into()))?;

    let status = org_deletion::record_operator_observation(
        &state.pool,
        &org_id,
        &body.operator,
        OperatorObservation {
            subscription_id: &body.subscription_id,
            observed_status: &body.observed_status,
            observed_at: &body.observed_at,
            reason: &body.reason,
            evidence: &body.evidence,
            managed_backup_expiry_by: body.managed_backup_expiry_by.as_deref(),
        },
    )
    .await?;
    let status = org_deletion::status_after_mutation(
        &state.pool,
        &org_deletion::DeletionView {
            id: status.id,
            org_id,
            state: status.state,
        },
    )
    .await?;
    Ok(Json(status.into()))
}
