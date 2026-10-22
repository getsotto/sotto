//! Durable billing correction requests and explicit early termination.
//!
//! This module is deliberately separate from coverage authority. A customer request, an operator
//! decision, and a provider refund are different facts. None of them shortens a paid term by
//! itself; early termination is applied only when the request records an explicit confirmation and
//! a successful full-refund result.

use sha2::{Digest, Sha256};
use sqlx::{Postgres, Row, Transaction};
use thiserror::Error;
use uuid::Uuid;

use crate::error::Error;

/// Stop replaying an ambiguous Stripe refund create before its idempotency record can expire.
/// Stripe retains idempotency keys for at least 24 hours; the one-hour margin is deliberate.
pub const REFUND_CREATE_REPLAY_WINDOW_SECONDS: i64 = 23 * 60 * 60;
pub const REFUND_CREATE_CLAIM_SECONDS: i64 = 2 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayerKind {
    Personal,
    Sponsor,
}

impl PayerKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Personal => "personal",
            Self::Sponsor => "sponsor",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorrectionReason {
    DuplicateCharge,
    BillingError,
    AccidentalRenewal,
    LegalRequirement,
}

impl CorrectionReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DuplicateCharge => "duplicate_charge",
            Self::BillingError => "billing_error",
            Self::AccidentalRenewal => "accidental_renewal",
            Self::LegalRequirement => "legal_requirement",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorrectionState {
    Requested,
    Approved,
    ProviderPending,
    TerminationPending,
    Refunded,
    Denied,
    Failed,
    Unknown,
}

impl CorrectionState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::Approved => "approved",
            Self::ProviderPending => "provider_pending",
            Self::TerminationPending => "termination_pending",
            Self::Refunded => "refunded",
            Self::Denied => "denied",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }

    fn parse(value: &str) -> Result<Self, BillingRefundError> {
        match value {
            "requested" => Ok(Self::Requested),
            "approved" => Ok(Self::Approved),
            "provider_pending" => Ok(Self::ProviderPending),
            "termination_pending" => Ok(Self::TerminationPending),
            "refunded" => Ok(Self::Refunded),
            "denied" => Ok(Self::Denied),
            "failed" => Ok(Self::Failed),
            "unknown" => Ok(Self::Unknown),
            _ => Err(BillingRefundError::CorruptState),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorrectionRequest {
    pub requester_user_id: String,
    pub beneficiary_id: String,
    pub organization_id: Option<String>,
    pub payer_kind: PayerKind,
    pub payment_reference: String,
    pub subscription_id: String,
    pub amount_pence: Option<i64>,
    pub reason: CorrectionReason,
    pub policy_version: String,
    pub idempotency_key: String,
    pub full_refund_requested: bool,
}

impl CorrectionRequest {
    fn validate(&self) -> Result<(), BillingRefundError> {
        for (value, field) in [
            (&self.requester_user_id, "requester_user_id"),
            (&self.beneficiary_id, "beneficiary_id"),
            (&self.payment_reference, "payment_reference"),
            (&self.subscription_id, "subscription_id"),
            (&self.policy_version, "policy_version"),
            (&self.idempotency_key, "idempotency_key"),
        ] {
            if value.trim().is_empty() {
                return Err(BillingRefundError::InvalidField(field));
            }
        }
        if !self.payment_reference.starts_with("pi_") {
            return Err(BillingRefundError::InvalidField("payment_reference"));
        }
        if (self.payer_kind == PayerKind::Personal) != self.organization_id.is_none() {
            return Err(BillingRefundError::InvalidField("payer boundary"));
        }
        if self.amount_pence.is_some_and(|amount| amount <= 0) {
            return Err(BillingRefundError::InvalidField("amount_pence"));
        }
        if self.full_refund_requested && self.amount_pence.is_some() {
            return Err(BillingRefundError::InvalidField("amount_pence"));
        }
        if !self.full_refund_requested && self.amount_pence.is_none() {
            return Err(BillingRefundError::InvalidField("amount_pence"));
        }
        Ok(())
    }

    fn request_hash(&self) -> String {
        let fields = [
            "sotto-billing-correction-v1",
            &self.requester_user_id,
            &self.beneficiary_id,
            self.organization_id.as_deref().unwrap_or(""),
            self.payer_kind.as_str(),
            &self.payment_reference,
            &self.subscription_id,
            &self.amount_pence.unwrap_or_default().to_string(),
            self.reason.as_str(),
            &self.policy_version,
            &self.idempotency_key,
            if self.full_refund_requested {
                "full"
            } else {
                "partial"
            },
        ];
        let mut input = Vec::new();
        for field in fields {
            input.extend_from_slice(&(field.len() as u64).to_be_bytes());
            input.extend_from_slice(field.as_bytes());
        }
        hex_digest(&Sha256::digest(input))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BillingRefundRequest {
    pub request_id: String,
    pub requester_user_id: String,
    pub beneficiary_id: String,
    pub organization_id: Option<String>,
    pub payer_kind: PayerKind,
    pub payment_reference: String,
    pub subscription_id: String,
    pub amount_pence: Option<i64>,
    pub reason: String,
    pub policy_version: String,
    pub idempotency_key: String,
    pub request_hash: String,
    pub full_refund_requested: bool,
    pub state: CorrectionState,
    pub preserve_paid_term: bool,
    pub early_termination_confirmed_at_epoch: Option<i64>,
    pub effective_at_epoch: Option<i64>,
    pub provider_refund_id: Option<String>,
    pub provider_refund_started_at_epoch: Option<i64>,
    pub result_code: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestDisposition {
    Created,
    AlreadyExists,
}

pub enum RefundCreateClaim {
    Claimed {
        request: BillingRefundRequest,
        token: Uuid,
    },
    InFlight(BillingRefundRequest),
    Parked(BillingRefundRequest),
}

#[derive(Debug, Error)]
pub enum BillingRefundError {
    #[error("billing correction has invalid {0}")]
    InvalidField(&'static str),
    #[error("billing correction request conflicts with stored evidence")]
    RequestConflict,
    #[error("billing correction is not ready for this step")]
    InvalidTransition,
    #[error("billing correction is not authorised")]
    Unauthorised,
    #[error("billing correction state is corrupt")]
    CorruptState,
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
}

pub async fn create_request(
    tx: &mut Transaction<'_, Postgres>,
    request: &CorrectionRequest,
) -> Result<(RequestDisposition, BillingRefundRequest), BillingRefundError> {
    request.validate()?;
    let hash = request.request_hash();
    if let Some(existing) =
        load_by_idempotency(tx, &request.requester_user_id, &request.idempotency_key).await?
    {
        if existing.request_hash != hash {
            return Err(BillingRefundError::RequestConflict);
        }
        return Ok((RequestDisposition::AlreadyExists, existing));
    }
    let request_id = format!("refund:{}", Uuid::new_v4());
    sqlx::query(
        "INSERT INTO billing_correction_requests \
         (request_id, requester_user_id, beneficiary_id, organization_id, payer_kind, \
          payment_reference, subscription_id, amount_pence, reason, policy_version, \
          idempotency_key, request_hash, full_refund_requested) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)",
    )
    .bind(&request_id)
    .bind(&request.requester_user_id)
    .bind(&request.beneficiary_id)
    .bind(request.organization_id.as_deref())
    .bind(request.payer_kind.as_str())
    .bind(&request.payment_reference)
    .bind(&request.subscription_id)
    .bind(request.amount_pence)
    .bind(request.reason.as_str())
    .bind(&request.policy_version)
    .bind(&request.idempotency_key)
    .bind(hash)
    .bind(request.full_refund_requested)
    .execute(&mut **tx)
    .await?;
    Ok((
        RequestDisposition::Created,
        load_request(tx, &request_id).await?,
    ))
}

pub async fn confirm_early_termination(
    tx: &mut Transaction<'_, Postgres>,
    request_id: &str,
    requester_user_id: &str,
    effective_at_epoch: i64,
) -> Result<BillingRefundRequest, BillingRefundError> {
    if effective_at_epoch <= 0 {
        return Err(BillingRefundError::InvalidField("effective_at_epoch"));
    }
    let current = load_request_for_update(tx, request_id).await?;
    if current.requester_user_id != requester_user_id {
        return Err(BillingRefundError::Unauthorised);
    }
    if current.payer_kind != PayerKind::Personal {
        return Err(BillingRefundError::InvalidTransition);
    }
    if !current.full_refund_requested {
        return Err(BillingRefundError::InvalidTransition);
    }
    if !matches!(
        current.state,
        CorrectionState::Requested | CorrectionState::Approved
    ) {
        if current.early_termination_confirmed_at_epoch == Some(effective_at_epoch) {
            return Ok(current);
        }
        return Err(BillingRefundError::InvalidTransition);
    }
    sqlx::query(
        "UPDATE billing_correction_requests SET preserve_paid_term = FALSE, \
         early_termination_confirmed_at_epoch = $2, effective_at_epoch = $2, updated_at = now() \
         WHERE request_id = $1",
    )
    .bind(request_id)
    .bind(effective_at_epoch)
    .execute(&mut **tx)
    .await?;
    load_request(tx, request_id).await
}

pub async fn review_request(
    tx: &mut Transaction<'_, Postgres>,
    request_id: &str,
    approve: bool,
    result_code: Option<&str>,
) -> Result<BillingRefundRequest, BillingRefundError> {
    let current = load_request_for_update(tx, request_id).await?;
    if current.state != CorrectionState::Requested {
        if (!approve && current.state == CorrectionState::Denied)
            || (approve && current.state == CorrectionState::Approved)
        {
            return Ok(current);
        }
        return Err(BillingRefundError::InvalidTransition);
    }
    if !approve && result_code.is_none_or(str::is_empty) {
        return Err(BillingRefundError::InvalidField("result_code"));
    }
    let state = if approve {
        CorrectionState::Approved
    } else {
        CorrectionState::Denied
    };
    sqlx::query(
        "UPDATE billing_correction_requests SET state = $2, result_code = $3, updated_at = now() \
         WHERE request_id = $1",
    )
    .bind(request_id)
    .bind(state.as_str())
    .bind(result_code)
    .execute(&mut **tx)
    .await?;
    load_request(tx, request_id).await
}

pub async fn begin_provider_refund(
    tx: &mut Transaction<'_, Postgres>,
    request_id: &str,
) -> Result<BillingRefundRequest, BillingRefundError> {
    let updated = sqlx::query(
        "UPDATE billing_correction_requests SET state = 'provider_pending', \
         provider_refund_started_at_epoch = COALESCE(provider_refund_started_at_epoch, \
             FLOOR(EXTRACT(EPOCH FROM now()))::BIGINT), updated_at = now() \
         WHERE request_id = $1 AND state = 'approved' RETURNING request_id",
    )
    .bind(request_id)
    .fetch_optional(&mut **tx)
    .await?;
    if updated.is_none() {
        let current = load_request(tx, request_id).await?;
        if current.state == CorrectionState::ProviderPending {
            return Ok(current);
        }
        return Err(BillingRefundError::InvalidTransition);
    }
    load_request(tx, request_id).await
}

/// Atomically claim a refund create or park it after the replay horizon. Only one request may
/// create at a time, and a live claim prevents another request from parking the row.
pub async fn claim_or_park_provider_refund_create(
    tx: &mut Transaction<'_, Postgres>,
    request_id: &str,
    claim_token: Uuid,
) -> Result<RefundCreateClaim, BillingRefundError> {
    let current = load_request_for_update(tx, request_id).await?;
    if current.state != CorrectionState::ProviderPending || current.provider_refund_id.is_some() {
        return Err(BillingRefundError::InvalidTransition);
    }
    let now_epoch: i64 = sqlx::query_scalar("SELECT FLOOR(EXTRACT(EPOCH FROM now()))::BIGINT")
        .fetch_one(&mut **tx)
        .await?;
    let claim_active: bool = sqlx::query_scalar(
        "SELECT COALESCE(provider_refund_claimed_until > now(), FALSE) \
         FROM billing_correction_requests WHERE request_id = $1",
    )
    .bind(request_id)
    .fetch_one(&mut **tx)
    .await?;
    if claim_active {
        return Ok(RefundCreateClaim::InFlight(current));
    }
    if !refund_create_replay_allowed(current.provider_refund_started_at_epoch, now_epoch) {
        sqlx::query(
            "UPDATE billing_correction_requests SET state = 'unknown', \
             result_code = 'refund_create_horizon_expired', provider_refund_claim_token = NULL, \
             provider_refund_claimed_until = NULL, updated_at = now() \
             WHERE request_id = $1 AND state = 'provider_pending' AND provider_refund_id IS NULL",
        )
        .bind(request_id)
        .execute(&mut **tx)
        .await?;
        return Ok(RefundCreateClaim::Parked(
            load_request(tx, request_id).await?,
        ));
    }
    sqlx::query(
        "UPDATE billing_correction_requests SET provider_refund_claim_token = $2::uuid, \
         provider_refund_claimed_until = now() + ($3 * interval '1 second'), updated_at = now() \
         WHERE request_id = $1 AND state = 'provider_pending' AND provider_refund_id IS NULL",
    )
    .bind(request_id)
    .bind(claim_token.to_string())
    .bind(REFUND_CREATE_CLAIM_SECONDS)
    .execute(&mut **tx)
    .await?;
    Ok(RefundCreateClaim::Claimed {
        request: load_request(tx, request_id).await?,
        token: claim_token,
    })
}

/// Confirm immediately before the provider call that this request still owns a live claim and
/// remains inside the conservative replay window, using database time for both checks.
pub async fn provider_refund_create_claim_is_current(
    tx: &mut Transaction<'_, Postgres>,
    request_id: &str,
    claim_token: Uuid,
) -> Result<bool, BillingRefundError> {
    Ok(sqlx::query_scalar(
        "SELECT COALESCE(state = 'provider_pending' AND provider_refund_id IS NULL \
         AND provider_refund_claim_token = $2::uuid AND provider_refund_claimed_until > now() \
         AND provider_refund_started_at_epoch <= FLOOR(EXTRACT(EPOCH FROM now()))::BIGINT \
         AND provider_refund_started_at_epoch > \
             FLOOR(EXTRACT(EPOCH FROM now()))::BIGINT - $3, FALSE) \
         FROM billing_correction_requests WHERE request_id = $1",
    )
    .bind(request_id)
    .bind(claim_token.to_string())
    .bind(REFUND_CREATE_REPLAY_WINDOW_SECONDS)
    .fetch_optional(&mut **tx)
    .await?
    .unwrap_or(false))
}

pub fn refund_create_replay_allowed(first_attempt_epoch: Option<i64>, now_epoch: i64) -> bool {
    first_attempt_epoch.is_some_and(|first| {
        first <= now_epoch && now_epoch.saturating_sub(first) < REFUND_CREATE_REPLAY_WINDOW_SECONDS
    })
}

/// Park an ambiguous refund create when the original provider idempotency window has elapsed.
/// A missing response is never treated as proof that no refund was created.
pub async fn park_ambiguous_refund_create(
    tx: &mut Transaction<'_, Postgres>,
    request_id: &str,
    now_epoch: i64,
) -> Result<BillingRefundRequest, BillingRefundError> {
    let current = load_request_for_update(tx, request_id).await?;
    if current.state == CorrectionState::Unknown
        && current.provider_refund_id.is_none()
        && current.result_code.as_deref() == Some("refund_create_horizon_expired")
    {
        return Ok(current);
    }
    if current.state != CorrectionState::ProviderPending || current.provider_refund_id.is_some() {
        return Err(BillingRefundError::InvalidTransition);
    }
    let claim_active: bool = sqlx::query_scalar(
        "SELECT COALESCE(provider_refund_claimed_until > now(), FALSE) \
         FROM billing_correction_requests WHERE request_id = $1",
    )
    .bind(request_id)
    .fetch_one(&mut **tx)
    .await?;
    if claim_active {
        return Err(BillingRefundError::InvalidTransition);
    }
    if refund_create_replay_allowed(current.provider_refund_started_at_epoch, now_epoch) {
        return Err(BillingRefundError::InvalidTransition);
    }
    sqlx::query(
        "UPDATE billing_correction_requests SET state = 'unknown', \
         result_code = 'refund_create_horizon_expired', provider_refund_claim_token = NULL, \
         provider_refund_claimed_until = NULL, updated_at = now() \
         WHERE request_id = $1 AND state = 'provider_pending' AND provider_refund_id IS NULL",
    )
    .bind(request_id)
    .execute(&mut **tx)
    .await?;
    load_request(tx, request_id).await
}

/// Record an unrecognised Stripe refund status without treating it as success or failure.
pub async fn record_provider_unknown(
    tx: &mut Transaction<'_, Postgres>,
    request_id: &str,
    provider_refund_id: &str,
    result_code: &str,
) -> Result<BillingRefundRequest, BillingRefundError> {
    if provider_refund_id.trim().is_empty() || result_code.trim().is_empty() {
        return Err(BillingRefundError::InvalidField("provider refund evidence"));
    }
    let current = load_request_for_update(tx, request_id).await?;
    if current.state == CorrectionState::Unknown
        && current.provider_refund_id.as_deref() == Some(provider_refund_id)
        && current.result_code.as_deref() == Some(result_code)
    {
        return Ok(current);
    }
    if !matches!(
        current.state,
        CorrectionState::ProviderPending | CorrectionState::Unknown
    ) || current
        .provider_refund_id
        .as_deref()
        .is_some_and(|existing| existing != provider_refund_id)
    {
        return Err(BillingRefundError::InvalidTransition);
    }
    sqlx::query(
        "UPDATE billing_correction_requests SET state = 'unknown', provider_refund_id = $2, \
         result_code = $3, provider_refund_claim_token = NULL, provider_refund_claimed_until = NULL, \
         updated_at = now() WHERE request_id = $1",
    )
    .bind(request_id)
    .bind(provider_refund_id)
    .bind(result_code)
    .execute(&mut **tx)
    .await?;
    load_request(tx, request_id).await
}

pub async fn record_provider_pending(
    tx: &mut Transaction<'_, Postgres>,
    request_id: &str,
    provider_refund_id: &str,
) -> Result<BillingRefundRequest, BillingRefundError> {
    if provider_refund_id.trim().is_empty() {
        return Err(BillingRefundError::InvalidField("provider_refund_id"));
    }
    let current = load_request_for_update(tx, request_id).await?;
    match current.state {
        CorrectionState::ProviderPending => {
            if current
                .provider_refund_id
                .as_deref()
                .is_some_and(|existing| existing != provider_refund_id)
            {
                return Err(BillingRefundError::RequestConflict);
            }
            if current.provider_refund_id.as_deref() == Some(provider_refund_id)
                && current.result_code.is_none()
            {
                return Ok(current);
            }
        }
        CorrectionState::Unknown => {
            if current
                .provider_refund_id
                .as_deref()
                .is_some_and(|existing| existing != provider_refund_id)
            {
                return Err(BillingRefundError::RequestConflict);
            }
            if current.provider_refund_id.is_none()
                && current.result_code.as_deref() != Some("refund_create_horizon_expired")
            {
                return Err(BillingRefundError::InvalidTransition);
            }
        }
        CorrectionState::Approved => {}
        _ => return Err(BillingRefundError::InvalidTransition),
    }
    sqlx::query(
        "UPDATE billing_correction_requests SET state = 'provider_pending', \
         provider_refund_id = $2, result_code = NULL, provider_refund_claim_token = NULL, \
         provider_refund_claimed_until = NULL, updated_at = now() WHERE request_id = $1",
    )
    .bind(request_id)
    .bind(provider_refund_id)
    .execute(&mut **tx)
    .await?;
    load_request(tx, request_id).await
}

pub async fn record_provider_refund(
    tx: &mut Transaction<'_, Postgres>,
    request_id: &str,
    provider_refund_id: &str,
    succeeded: bool,
    result_code: Option<&str>,
) -> Result<BillingRefundRequest, BillingRefundError> {
    if provider_refund_id.trim().is_empty() {
        return Err(BillingRefundError::InvalidField("provider_refund_id"));
    }
    if !succeeded && result_code.is_none_or(str::is_empty) {
        return Err(BillingRefundError::InvalidField("result_code"));
    }
    let current = load_request_for_update(tx, request_id).await?;
    if matches!(
        current.state,
        CorrectionState::ProviderPending | CorrectionState::Unknown
    ) && current
        .provider_refund_id
        .as_deref()
        .is_some_and(|existing| existing != provider_refund_id)
    {
        return Err(BillingRefundError::RequestConflict);
    }
    let early_termination =
        succeeded && current.payer_kind == PayerKind::Personal && !current.preserve_paid_term;
    let target_state = if early_termination {
        CorrectionState::TerminationPending
    } else if succeeded {
        CorrectionState::Refunded
    } else {
        CorrectionState::Failed
    };
    let stored_result_code = if succeeded {
        result_code.or(Some("refunded"))
    } else {
        result_code
    };
    if current.state == target_state {
        if current.provider_refund_id.as_deref() == Some(provider_refund_id)
            && current.result_code.as_deref() == stored_result_code
        {
            return Ok(current);
        }
        return Err(BillingRefundError::RequestConflict);
    }
    if !matches!(
        current.state,
        CorrectionState::Approved | CorrectionState::ProviderPending | CorrectionState::Unknown
    ) {
        return Err(BillingRefundError::InvalidTransition);
    }
    sqlx::query(
        "UPDATE billing_correction_requests SET state = $2, provider_refund_id = $3, \
         result_code = $4, provider_refund_claim_token = NULL, provider_refund_claimed_until = NULL, \
         updated_at = now() WHERE request_id = $1",
    )
    .bind(request_id)
    .bind(target_state.as_str())
    .bind(provider_refund_id)
    .bind(stored_result_code)
    .execute(&mut **tx)
    .await?;
    load_request(tx, request_id).await
}

/// Record the durable completion of the provider cancellation after a successful full refund.
/// Keeping this separate from `record_provider_refund` leaves the request retryable when the
/// refund succeeds but the subscription termination is temporarily unavailable.
pub async fn record_provider_termination(
    tx: &mut Transaction<'_, Postgres>,
    request_id: &str,
) -> Result<BillingRefundRequest, BillingRefundError> {
    let current = load_request_for_update(tx, request_id).await?;
    if current.state == CorrectionState::Refunded {
        return Ok(current);
    }
    if current.state != CorrectionState::TerminationPending
        || current.payer_kind != PayerKind::Personal
        || current.preserve_paid_term
    {
        return Err(BillingRefundError::InvalidTransition);
    }
    let effective_at = current
        .effective_at_epoch
        .ok_or(BillingRefundError::CorruptState)?;
    sqlx::query(
        "UPDATE billing_correction_requests SET state = 'refunded', updated_at = now() \
         WHERE request_id = $1 AND state = 'termination_pending'",
    )
    .bind(request_id)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "UPDATE billing_personal_accounts SET state = 'canceled', \
         paid_through_epoch = LEAST(COALESCE(paid_through_epoch, $2), $2), \
         paid_through_date = to_char(to_timestamp($2), 'YYYY-MM-DD'), \
         cancel_at_period_end = TRUE, updated_at = now() \
         WHERE user_id = $1 AND stripe_subscription_id = $3",
    )
    .bind(&current.beneficiary_id)
    .bind(effective_at)
    .bind(&current.subscription_id)
    .execute(&mut **tx)
    .await?;
    load_request(tx, request_id).await
}

pub async fn load_for_requester(
    tx: &mut Transaction<'_, Postgres>,
    request_id: &str,
    requester_user_id: &str,
) -> Result<Option<BillingRefundRequest>, BillingRefundError> {
    let row = sqlx::query(
        "SELECT * FROM billing_correction_requests WHERE request_id = $1 AND requester_user_id = $2",
    )
    .bind(request_id)
    .bind(requester_user_id)
    .fetch_optional(&mut **tx)
    .await?;
    row.map(|row| request_from_row(&row)).transpose()
}

pub async fn load_for_operator(
    tx: &mut Transaction<'_, Postgres>,
    request_id: &str,
) -> Result<BillingRefundRequest, BillingRefundError> {
    load_request(tx, request_id).await
}

async fn load_by_idempotency(
    tx: &mut Transaction<'_, Postgres>,
    requester_user_id: &str,
    idempotency_key: &str,
) -> Result<Option<BillingRefundRequest>, BillingRefundError> {
    let row = sqlx::query(
        "SELECT * FROM billing_correction_requests \
         WHERE requester_user_id = $1 AND idempotency_key = $2 FOR UPDATE",
    )
    .bind(requester_user_id)
    .bind(idempotency_key)
    .fetch_optional(&mut **tx)
    .await?;
    row.map(|row| request_from_row(&row)).transpose()
}

async fn load_request(
    tx: &mut Transaction<'_, Postgres>,
    request_id: &str,
) -> Result<BillingRefundRequest, BillingRefundError> {
    let row = sqlx::query("SELECT * FROM billing_correction_requests WHERE request_id = $1")
        .bind(request_id)
        .fetch_one(&mut **tx)
        .await?;
    request_from_row(&row)
}

async fn load_request_for_update(
    tx: &mut Transaction<'_, Postgres>,
    request_id: &str,
) -> Result<BillingRefundRequest, BillingRefundError> {
    let row =
        sqlx::query("SELECT * FROM billing_correction_requests WHERE request_id = $1 FOR UPDATE")
            .bind(request_id)
            .fetch_one(&mut **tx)
            .await?;
    request_from_row(&row)
}

fn request_from_row(
    row: &sqlx::postgres::PgRow,
) -> Result<BillingRefundRequest, BillingRefundError> {
    Ok(BillingRefundRequest {
        request_id: row.try_get("request_id")?,
        requester_user_id: row.try_get("requester_user_id")?,
        beneficiary_id: row.try_get("beneficiary_id")?,
        organization_id: row.try_get("organization_id")?,
        payer_kind: match row.try_get::<String, _>("payer_kind")?.as_str() {
            "personal" => PayerKind::Personal,
            "sponsor" => PayerKind::Sponsor,
            _ => return Err(BillingRefundError::CorruptState),
        },
        payment_reference: row.try_get("payment_reference")?,
        subscription_id: row.try_get("subscription_id")?,
        amount_pence: row.try_get("amount_pence")?,
        reason: row.try_get("reason")?,
        policy_version: row.try_get("policy_version")?,
        idempotency_key: row.try_get("idempotency_key")?,
        request_hash: row.try_get("request_hash")?,
        full_refund_requested: row.try_get("full_refund_requested")?,
        state: CorrectionState::parse(&row.try_get::<String, _>("state")?)?,
        preserve_paid_term: row.try_get("preserve_paid_term")?,
        early_termination_confirmed_at_epoch: row
            .try_get("early_termination_confirmed_at_epoch")?,
        effective_at_epoch: row.try_get("effective_at_epoch")?,
        provider_refund_id: row.try_get("provider_refund_id")?,
        provider_refund_started_at_epoch: row.try_get("provider_refund_started_at_epoch")?,
        result_code: row.try_get("result_code")?,
    })
}

fn hex_digest(digest: &[u8]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

impl From<BillingRefundError> for Error {
    fn from(error: BillingRefundError) -> Self {
        match error {
            BillingRefundError::InvalidField(field) => {
                Self::BadRequest(format!("invalid billing correction {field}"))
            }
            BillingRefundError::RequestConflict | BillingRefundError::InvalidTransition => {
                Self::Conflict("billing correction conflicts with stored state".into())
            }
            BillingRefundError::Unauthorised => {
                Self::Forbidden("billing correction is not authorised".into())
            }
            BillingRefundError::CorruptState => {
                Self::Internal("billing correction state is corrupt".into())
            }
            BillingRefundError::Database(error) => Self::Db(error),
        }
    }
}
