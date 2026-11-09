//! Protected operator controls for parked payer transfers.
//!
//! A resume decision only releases the database hold and makes a transfer eligible for the normal
//! worker. It does not declare payment successful or perform a Stripe operation.

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::header::CACHE_CONTROL;
use axum::http::{HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::billing_transfers::{
    self, TransferIntent, TransferReviewAction, TransferReviewDecision,
};
use crate::error::{Error, Result};
use crate::org_deletion_metrics::{bearer_token, token_matches};
use crate::state::AppState;

const MAX_REVIEW_PAGE: i64 = 50;

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/ops/billing/transfers/manual-review",
            get(list_manual_review),
        )
        .route(
            "/ops/billing/transfers/{transfer_id}/manual-review",
            get(get_manual_review),
        )
        .route(
            "/ops/billing/transfers/{transfer_id}/manual-review/decision",
            post(record_review_decision),
        )
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewQueueQuery {
    limit: Option<i64>,
    after: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewDecisionRequest {
    operator: String,
    action: String,
    reason_code: String,
    review_generation: i64,
    idempotency_key: String,
}

#[derive(Debug, Serialize)]
struct ReviewQueueResponse {
    transfers: Vec<TransferReviewView>,
    next_cursor: Option<String>,
}

#[derive(Debug, Serialize)]
struct ReviewDetailResponse {
    transfer: TransferReviewView,
    decisions: Vec<ReviewDecisionView>,
}

#[derive(Debug, Serialize)]
struct ReviewDecisionResponse {
    transfer: TransferReviewView,
    decision: ReviewDecisionView,
}

#[derive(Debug, Serialize)]
struct TransferReviewView {
    transfer_id: String,
    state: &'static str,
    beneficiary_id: String,
    source_kind: &'static str,
    source_organization_id: Option<String>,
    source_provider_subscription_id: Option<String>,
    destination_kind: &'static str,
    destination_organization_id: Option<String>,
    destination_operation_id: Option<String>,
    source_operation_id: Option<String>,
    destination_provider_subscription_id: Option<String>,
    destination_customer_id: Option<String>,
    destination_schedule_id: Option<String>,
    source_schedule_id: Option<String>,
    destination_setup_session_id: Option<String>,
    destination_setup_intent_id: Option<String>,
    destination_payment_reference: Option<String>,
    destination_period_start: Option<i64>,
    destination_period_end: Option<i64>,
    destination_expected_period_end: Option<i64>,
    source_adjustment_reference: Option<String>,
    source_restoration_reference: Option<String>,
    source_restoration_provider_subscription_id: Option<String>,
    source_restoration_payment_reference: Option<String>,
    execution_review_generation: i64,
    execution_manual_review_required: bool,
    execution_attempt_count: i32,
    last_execution_error_code: Option<String>,
    result_code: Option<String>,
}

#[derive(Debug, Serialize)]
struct ReviewDecisionView {
    decision_id: i64,
    operator: String,
    action: &'static str,
    reason_code: String,
    review_generation: i64,
    idempotency_key: String,
    created_at_epoch: i64,
}

impl From<TransferIntent> for TransferReviewView {
    fn from(transfer: TransferIntent) -> Self {
        Self {
            transfer_id: transfer.transfer_id,
            state: transfer.state.as_str(),
            beneficiary_id: transfer.beneficiary_id,
            source_kind: transfer.source_kind.as_str(),
            source_organization_id: transfer.source_organization_id,
            source_provider_subscription_id: transfer.source_provider_subscription_id,
            destination_kind: transfer.destination_kind.as_str(),
            destination_organization_id: transfer.destination_organization_id,
            destination_operation_id: transfer.destination_operation_id,
            source_operation_id: transfer.source_operation_id,
            destination_provider_subscription_id: transfer.destination_provider_subscription_id,
            destination_customer_id: transfer.destination_customer_id,
            destination_schedule_id: transfer.destination_schedule_id,
            source_schedule_id: transfer.source_schedule_id,
            destination_setup_session_id: transfer.destination_setup_session_id,
            destination_setup_intent_id: transfer.destination_setup_intent_id,
            destination_payment_reference: transfer.destination_payment_reference,
            destination_period_start: transfer.destination_period_start,
            destination_period_end: transfer.destination_period_end,
            destination_expected_period_end: transfer.destination_expected_period_end,
            source_adjustment_reference: transfer.source_adjustment_reference,
            source_restoration_reference: transfer.source_restoration_reference,
            source_restoration_provider_subscription_id: transfer
                .source_restoration_provider_subscription_id,
            source_restoration_payment_reference: transfer.source_restoration_payment_reference,
            execution_review_generation: transfer.execution_review_generation,
            execution_manual_review_required: transfer.execution_manual_review_required,
            execution_attempt_count: transfer.execution_attempt_count,
            last_execution_error_code: transfer.last_execution_error_code,
            result_code: transfer.result_code,
        }
    }
}

impl From<TransferReviewDecision> for ReviewDecisionView {
    fn from(decision: TransferReviewDecision) -> Self {
        Self {
            decision_id: decision.decision_id,
            operator: decision.operator_label,
            action: decision.action.as_str(),
            reason_code: decision.reason_code,
            review_generation: decision.review_generation,
            idempotency_key: decision.idempotency_key,
            created_at_epoch: decision.created_at_epoch,
        }
    }
}

async fn list_manual_review(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: std::result::Result<Query<ReviewQueueQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Response> {
    require_operator(&state, &headers)?;
    let Query(query) = query.map_err(|_| Error::BadRequest("invalid review queue query".into()))?;
    let limit = query.limit.unwrap_or(MAX_REVIEW_PAGE);
    if !(1..=MAX_REVIEW_PAGE).contains(&limit) {
        return Err(Error::BadRequest(
            "review queue limit must be between 1 and 50".into(),
        ));
    }
    let mut transfers = billing_transfers::list_manual_review_transfers(
        &state.pool,
        query.after.as_deref(),
        limit + 1,
    )
    .await?;
    let next_cursor = (transfers.len() as i64 > limit)
        .then(|| {
            transfers
                .get(limit as usize - 1)
                .map(|item| item.transfer_id.clone())
        })
        .flatten();
    transfers.truncate(limit as usize);
    json_no_store(ReviewQueueResponse {
        transfers: transfers.into_iter().map(Into::into).collect(),
        next_cursor,
    })
}

async fn get_manual_review(
    State(state): State<AppState>,
    Path(transfer_id): Path<String>,
    headers: HeaderMap,
) -> Result<Response> {
    require_operator(&state, &headers)?;
    let Some((transfer, decisions)) =
        billing_transfers::load_manual_review_transfer(&state.pool, &transfer_id).await?
    else {
        return Err(Error::NotFound("manual review transfer not found".into()));
    };
    json_no_store(ReviewDetailResponse {
        transfer: transfer.into(),
        decisions: decisions.into_iter().map(Into::into).collect(),
    })
}

async fn record_review_decision(
    State(state): State<AppState>,
    Path(transfer_id): Path<String>,
    headers: HeaderMap,
    body: std::result::Result<Json<ReviewDecisionRequest>, JsonRejection>,
) -> Result<Response> {
    require_operator(&state, &headers)?;
    let Json(body) =
        body.map_err(|_| Error::BadRequest("invalid transfer review decision".into()))?;
    let action = TransferReviewAction::parse(&body.action).map_err(Error::from)?;
    let (transfer, decision) = billing_transfers::record_operator_review_decision(
        &state.pool,
        &transfer_id,
        &body.operator,
        action,
        &body.reason_code,
        body.review_generation,
        &body.idempotency_key,
    )
    .await?;
    json_no_store(ReviewDecisionResponse {
        transfer: transfer.into(),
        decision: decision.into(),
    })
}

fn require_operator(state: &AppState, headers: &HeaderMap) -> Result<()> {
    let expected = state
        .billing_transfer_operator_token
        .as_deref()
        .ok_or_else(|| {
            Error::NotConfigured("payer transfer operator controls are not enabled".into())
        })?;
    let provided = bearer_token(headers).ok_or(Error::Unauthorized)?;
    if !token_matches(expected, provided) {
        return Err(Error::Unauthorized);
    }
    Ok(())
}

fn json_no_store<T: Serialize>(body: T) -> Result<Response> {
    let mut response = Json(body).into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::billing_transfers::TransferState;

    #[test]
    fn response_state_is_fixed_vocabulary() {
        assert_eq!(TransferState::Unknown.as_str(), "unknown");
        assert!(matches!(
            TransferReviewAction::parse("resume"),
            Ok(TransferReviewAction::Resume)
        ));
        assert!(TransferReviewAction::parse("complete").is_err());
    }
}
