//! Durable payer-transfer intents.
//!
//! A transfer changes who pays for one beneficiary without changing the beneficiary's account,
//! keys, membership, or founding identity. Provider execution is deliberately outside this module:
//! callbacks record each verified step here, so a timeout can be reconciled without issuing a new
//! financial identity. Both payer sides must consent: the intent records the initiating user and
//! the counterparty whose authority covers the other side. Provider execution remains behind an
//! independent default-off control until renewal-boundary behaviour is verified.

use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Row, Transaction};
use thiserror::Error;
use uuid::Uuid;

use crate::billing_catalogue::BillingOffer;
use crate::error::Error;
use crate::founding_allocator::{self, FoundingDate, FoundingOffer};
use crate::founding_schedule;
use crate::org;

/// The operator-facing lifetime of a transfer quote. A client cannot extend this by submitting
/// its own expiry when it creates the invitation.
pub const TRANSFER_QUOTE_SECONDS: i64 = 15 * 60;
/// A distinct counterparty has seven days to discover and respond to a transfer invitation.
pub const TRANSFER_CONSENT_SECONDS: i64 = 7 * 24 * 60 * 60;
pub const TRANSFER_QUOTE_VERSION: i64 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferPayer {
    Personal,
    Sponsor,
}

impl TransferPayer {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Personal => "personal",
            Self::Sponsor => "sponsor",
        }
    }

    pub fn parse(value: &str) -> Result<Self, TransferError> {
        match value {
            "personal" => Ok(Self::Personal),
            "sponsor" => Ok(Self::Sponsor),
            _ => Err(TransferError::InvalidField("payer kind")),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TransferDraft {
    pub counterparty_user_id: String,
    pub beneficiary_id: String,
    pub destination_kind: TransferPayer,
    pub destination_organization_id: Option<String>,
    pub quote_version: i64,
    pub quote_expires_at_epoch: i64,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferState {
    AwaitingConsent,
    Pending,
    DestinationPrepared,
    DestinationPaid,
    SourceAdjustmentPending,
    SourceRestorationPending,
    Completed,
    Failed,
    Unknown,
}

impl TransferState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AwaitingConsent => "awaiting_consent",
            Self::Pending => "pending",
            Self::DestinationPrepared => "destination_prepared",
            Self::DestinationPaid => "destination_paid",
            Self::SourceAdjustmentPending => "source_adjustment_pending",
            Self::SourceRestorationPending => "source_restoration_pending",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }

    fn parse(value: &str) -> Result<Self, TransferError> {
        match value {
            "awaiting_consent" => Ok(Self::AwaitingConsent),
            "pending" => Ok(Self::Pending),
            "destination_prepared" => Ok(Self::DestinationPrepared),
            "destination_paid" => Ok(Self::DestinationPaid),
            "source_adjustment_pending" => Ok(Self::SourceAdjustmentPending),
            "source_restoration_pending" => Ok(Self::SourceRestorationPending),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "unknown" => Ok(Self::Unknown),
            _ => Err(TransferError::CorruptState),
        }
    }

    #[cfg(test)]
    fn is_live(self) -> bool {
        matches!(
            self,
            Self::AwaitingConsent
                | Self::Pending
                | Self::DestinationPrepared
                | Self::DestinationPaid
                | Self::SourceAdjustmentPending
                | Self::SourceRestorationPending
                | Self::Unknown
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferRequest {
    pub actor_user_id: String,
    pub counterparty_user_id: String,
    pub beneficiary_id: String,
    pub source_kind: TransferPayer,
    pub source_organization_id: Option<String>,
    pub source_provider_subscription_id: String,
    pub destination_kind: TransferPayer,
    pub destination_organization_id: Option<String>,
    pub offer: BillingOffer,
    pub quote_version: i64,
    pub quote_expires_at_epoch: i64,
    pub effective_from: i64,
    pub effective_until: Option<i64>,
    pub idempotency_key: String,
}

impl TransferRequest {
    pub fn validate(&self, now_epoch: i64) -> Result<(), TransferError> {
        self.validate_shape()?;
        self.validate_timing(now_epoch)
    }

    fn validate_shape(&self) -> Result<(), TransferError> {
        for (value, field) in [
            (&self.actor_user_id, "actor_user_id"),
            (&self.counterparty_user_id, "counterparty_user_id"),
            (&self.beneficiary_id, "beneficiary_id"),
            (&self.source_provider_subscription_id, "source subscription"),
            (&self.idempotency_key, "idempotency_key"),
        ] {
            if value.trim().is_empty() {
                return Err(TransferError::InvalidField(field));
            }
        }
        if self.source_kind == self.destination_kind
            && self.source_organization_id == self.destination_organization_id
        {
            return Err(TransferError::InvalidField("payer boundary"));
        }
        if (self.source_kind == TransferPayer::Personal) != self.source_organization_id.is_none()
            || (self.destination_kind == TransferPayer::Personal)
                != self.destination_organization_id.is_none()
        {
            return Err(TransferError::InvalidField("payer organisation"));
        }
        if self.quote_version < 1 {
            return Err(TransferError::InvalidField("quote_version"));
        }
        if self.quote_expires_at_epoch <= 0 {
            return Err(TransferError::InvalidField("quote_expires_at_epoch"));
        }
        if self.effective_from < 0 {
            return Err(TransferError::InvalidField("effective_from"));
        }
        Ok(())
    }

    fn validate_timing(&self, now_epoch: i64) -> Result<(), TransferError> {
        if self.quote_expires_at_epoch <= now_epoch {
            return Err(TransferError::QuoteExpired);
        }
        if self.effective_from < now_epoch {
            return Err(TransferError::InvalidField("effective_from"));
        }
        if self
            .effective_until
            .is_some_and(|until| until <= self.effective_from)
        {
            return Err(TransferError::InvalidField("effective_until"));
        }
        Ok(())
    }

    fn validate_acceptance_timing(&self, now_epoch: i64) -> Result<(), TransferError> {
        if self.effective_from < now_epoch {
            return Err(TransferError::InvalidField("effective_from"));
        }
        if self
            .effective_until
            .is_some_and(|until| until <= self.effective_from)
        {
            return Err(TransferError::InvalidField("effective_until"));
        }
        Ok(())
    }

    fn request_hash(&self) -> String {
        let quote_version = self.quote_version.to_string();
        let quote_expires_at = self.quote_expires_at_epoch.to_string();
        let effective_from = self.effective_from.to_string();
        let effective_until = self.effective_until.unwrap_or_default().to_string();
        let fields = [
            "sotto-transfer-v1",
            &self.actor_user_id,
            &self.counterparty_user_id,
            &self.beneficiary_id,
            self.source_kind.as_str(),
            self.source_organization_id.as_deref().unwrap_or(""),
            &self.source_provider_subscription_id,
            self.destination_kind.as_str(),
            self.destination_organization_id.as_deref().unwrap_or(""),
            self.offer.as_str(),
            &quote_version,
            &quote_expires_at,
            &effective_from,
            &effective_until,
            &self.idempotency_key,
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
pub struct TransferIntent {
    pub transfer_id: String,
    pub actor_user_id: String,
    pub counterparty_user_id: String,
    pub beneficiary_id: String,
    pub source_kind: TransferPayer,
    pub source_organization_id: Option<String>,
    pub destination_kind: TransferPayer,
    pub destination_organization_id: Option<String>,
    pub offer: BillingOffer,
    pub quote_version: i64,
    pub quote_expires_at_epoch: i64,
    pub consent_expires_at_epoch: i64,
    pub request_hash: String,
    pub idempotency_key: String,
    pub provider_idempotency_key: String,
    pub effective_from: i64,
    pub effective_until: Option<i64>,
    pub destination_operation_id: Option<String>,
    pub source_operation_id: Option<String>,
    pub destination_provider_subscription_id: Option<String>,
    pub source_provider_subscription_id: Option<String>,
    pub destination_payment_reference: Option<String>,
    pub destination_period_start: Option<i64>,
    pub destination_period_end: Option<i64>,
    pub destination_expected_period_end: Option<i64>,
    pub source_adjustment_reference: Option<String>,
    pub source_restoration_reference: Option<String>,
    pub source_restoration_operation_id: Option<String>,
    pub source_restoration_provider_subscription_id: Option<String>,
    pub source_restoration_payment_reference: Option<String>,
    pub founding_award_id: Option<String>,
    pub execution_lease_token: Option<String>,
    pub execution_lease_until_epoch: Option<i64>,
    /// Consecutive failed execution attempts; claims and invoice waits do not increment it, and
    /// successful progress resets it.
    pub execution_attempt_count: i32,
    pub execution_manual_review_required: bool,
    /// Changes whenever new evidence parks this intent for operator review. Decisions are bound
    /// to this value so an old approval cannot release a later review.
    pub execution_review_generation: i64,
    pub destination_customer_id: Option<String>,
    pub destination_schedule_id: Option<String>,
    pub source_schedule_id: Option<String>,
    pub destination_setup_session_id: Option<String>,
    pub destination_setup_intent_id: Option<String>,
    pub last_execution_error_code: Option<String>,
    pub state: TransferState,
    pub result_code: Option<String>,
}

/// A claimed transfer row. The lease token fences an older worker after another worker
/// reclaims an expired lease; provider idempotency keys remain stable across leases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferLease {
    pub transfer: TransferIntent,
    pub lease_token: String,
    pub lease_until_epoch: i64,
}

pub const TRANSFER_EXECUTION_LEASE_SECONDS: i64 = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferReviewAction {
    Hold,
    Resume,
}

impl TransferReviewAction {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hold => "hold",
            Self::Resume => "resume",
        }
    }

    pub fn parse(value: &str) -> Result<Self, TransferError> {
        match value {
            "hold" => Ok(Self::Hold),
            "resume" => Ok(Self::Resume),
            _ => Err(TransferError::InvalidField("review action")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferReviewDecision {
    pub decision_id: i64,
    pub operator_label: String,
    pub action: TransferReviewAction,
    pub reason_code: String,
    pub review_generation: i64,
    pub idempotency_key: String,
    pub created_at_epoch: i64,
}
const MAX_TRANSFER_RETRY_DELAY_SECONDS: i64 = 60 * 60;
const MAX_AUTOMATIC_TRANSFER_ATTEMPTS: i32 = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferExecutionFailure {
    RetryableProvider,
    ProviderOutcomeUnknown,
    LocalEvidenceChanged,
    ProviderRejected,
}

/// Return the reason a recorded destination payment cannot yet justify completing a transfer.
/// The invoice period is immutable evidence, so an operator resume must not turn a known mismatch
/// into accepted payment evidence.
pub(crate) fn destination_period_mismatch_code(transfer: &TransferIntent) -> Option<&'static str> {
    transfer.destination_payment_reference.as_ref()?;
    let Some(period_start) = transfer.destination_period_start else {
        return Some("destination_period_start_missing");
    };
    if period_start != transfer.effective_from {
        return Some("destination_period_start_mismatch");
    }
    let expected_period_end = transfer.destination_expected_period_end.or_else(|| {
        (transfer.destination_kind == TransferPayer::Personal)
            .then(|| expected_transfer_period_end(transfer.offer, transfer.effective_from))
            .flatten()
    });
    if expected_period_end.is_none() {
        return Some("destination_expected_period_end_missing");
    }
    if transfer.destination_period_end != expected_period_end {
        return Some("destination_period_end_mismatch");
    }
    None
}

impl TransferExecutionFailure {
    const fn as_str(self) -> &'static str {
        match self {
            Self::RetryableProvider => "provider_retryable",
            Self::ProviderOutcomeUnknown => "provider_outcome_unknown",
            Self::LocalEvidenceChanged => "local_evidence_changed",
            Self::ProviderRejected => "provider_rejected",
        }
    }

    const fn needs_unknown_state(self) -> bool {
        matches!(self, Self::ProviderOutcomeUnknown | Self::ProviderRejected)
    }

    const fn requires_manual_review(self, attempts: i32) -> bool {
        matches!(self, Self::ProviderRejected) || attempts >= MAX_AUTOMATIC_TRANSFER_ATTEMPTS
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BeginTransfer {
    Created(TransferIntent),
    AlreadyExists(TransferIntent),
}

#[derive(Debug, Error)]
pub enum TransferError {
    #[error("transfer not found")]
    MissingTransfer,
    #[error("transfer has invalid {0}")]
    InvalidField(&'static str),
    #[error("transfer quote has expired")]
    QuoteExpired,
    #[error("transfer source is not active")]
    SourceInactive,
    #[error("source billing term changed; refresh the transfer quote")]
    SourceTermChanged,
    #[error("transfer destination already covers this beneficiary")]
    DestinationAlreadyCovers,
    #[error("destination sponsor has no active subscription")]
    DestinationSubscriptionUnavailable,
    #[error("destination sponsor renewal does not match the source paid-through boundary")]
    DestinationTermChanged,
    #[error(
        "a payer transfer invitation is pending; the person who started it or the invited payer can withdraw it"
    )]
    InvitationPending,
    #[error("transfer is already in progress for this beneficiary")]
    AlreadyInProgress,
    #[error("transfer is not authorised")]
    Unauthorised,
    #[error("transfer beneficiary does not exist")]
    BeneficiaryMissing,
    #[error("transfer state is corrupt")]
    CorruptState,
    #[error("transfer evidence conflicts with the stored result")]
    ResultConflict,
    #[error("founding award is owned by a different payer")]
    FoundingPayerConflict,
    #[error("transfer is not ready for this step")]
    InvalidTransition,
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("founding allocator error: {0}")]
    Founding(#[from] founding_allocator::FoundingAllocatorError),
}

/// Derive and validate a transfer request from local billing state. The caller supplies only the
/// destination and participants; source payer, offer and effective boundary are server-owned.
pub async fn build_transfer_request(
    tx: &mut Transaction<'_, Postgres>,
    actor_user_id: &str,
    draft: &TransferDraft,
    now_epoch: i64,
) -> Result<TransferRequest, TransferError> {
    if draft.quote_expires_at_epoch > now_epoch.saturating_add(TRANSFER_QUOTE_SECONDS) {
        return Err(TransferError::InvalidField("quote expiry"));
    }
    if draft.quote_version != TRANSFER_QUOTE_VERSION {
        return Err(TransferError::InvalidField("quote version"));
    }

    // Do a read-only authority check before taking the global billing lock. Besides preventing
    // billing-state disclosure to strangers, this keeps unauthorised quote requests from
    // serialising every checkout and invoice settlement. Authority is checked again under the
    // normal locks after source state has been derived below.
    preauthorise_transfer_draft(tx, actor_user_id, draft, now_epoch).await?;
    founding_allocator::lock_capacity(tx).await?;

    let personal = sqlx::query_as::<_, (String, Option<i64>, String)>(
        "SELECT offer, paid_through_epoch, stripe_subscription_id FROM billing_personal_accounts \
         WHERE user_id = $1 AND state = 'active' AND cancel_at_period_end = FALSE \
           AND cancellation_requested_at IS NULL AND stripe_subscription_id IS NOT NULL",
    )
    .bind(&draft.beneficiary_id)
    .fetch_optional(&mut **tx)
    .await?;
    let sponsored = sqlx::query_as::<_, (String, String, i64, String)>(
        "SELECT seat.organization_id, seat.offer, subscription.current_period_end, \
                subscription.provider_subscription_id \
         FROM billing_sponsored_seats AS seat \
         JOIN billing_sponsored_subscriptions AS subscription \
           ON subscription.organization_id = seat.organization_id \
         WHERE seat.beneficiary_id = $1 AND seat.state = 'active' \
           AND subscription.status = 'active' \
           AND subscription.provider_subscription_id IS NOT NULL \
           AND subscription.current_period_end IS NOT NULL \
           AND seat.effective_from <= $2 \
           AND (seat.effective_until IS NULL OR \
                seat.effective_until >= subscription.current_period_end) \
         ORDER BY seat.organization_id LIMIT 2",
    )
    .bind(&draft.beneficiary_id)
    .bind(now_epoch)
    .fetch_all(&mut **tx)
    .await?;

    let (source_kind, source_organization_id, source_offer, effective_from, source_subscription_id) =
        match (personal, sponsored.as_slice()) {
            (Some((offer, Some(boundary), subscription_id)), []) => (
                TransferPayer::Personal,
                None,
                offer,
                boundary,
                subscription_id,
            ),
            (None, [(organization_id, offer, boundary, subscription_id)]) => (
                TransferPayer::Sponsor,
                Some(organization_id.clone()),
                offer.clone(),
                *boundary,
                subscription_id.clone(),
            ),
            (None, []) | (Some((_, None, _)), []) => return Err(TransferError::SourceInactive),
            _ => return Err(TransferError::CorruptState),
        };

    let mut request = TransferRequest {
        actor_user_id: actor_user_id.to_owned(),
        counterparty_user_id: draft.counterparty_user_id.clone(),
        beneficiary_id: draft.beneficiary_id.clone(),
        source_kind,
        source_organization_id,
        source_provider_subscription_id: source_subscription_id,
        destination_kind: draft.destination_kind,
        destination_organization_id: draft.destination_organization_id.clone(),
        offer: parse_offer(&source_offer)?,
        quote_version: draft.quote_version,
        quote_expires_at_epoch: draft.quote_expires_at_epoch,
        effective_from,
        effective_until: None,
        idempotency_key: draft.idempotency_key.clone(),
    };
    request.validate_shape()?;
    authorise_transfer_parties(tx, &request).await?;

    let live_transfer: Option<(String, String)> = sqlx::query_as(
        "SELECT transfer_id, state FROM billing_transfer_intents WHERE beneficiary_id = $1 \
         AND (state IN ('pending','destination_prepared','destination_paid', \
                        'source_adjustment_pending','source_restoration_pending','unknown') \
              OR (state = 'awaiting_consent' AND consent_expires_at_epoch > $2 AND effective_from > $2)) \
         ORDER BY transfer_id LIMIT 1 FOR UPDATE",
    )
    .bind(&request.beneficiary_id)
    .bind(now_epoch)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some((_, state)) = live_transfer {
        return Err(if state == "awaiting_consent" {
            TransferError::InvitationPending
        } else {
            TransferError::AlreadyInProgress
        });
    }

    let source_offer = ensure_source_active(tx, &request, now_epoch).await?;
    ensure_no_pending_billing_mutation(tx, &request.beneficiary_id).await?;
    ensure_destination_free(tx, &request).await?;
    let award_id: Option<String> = sqlx::query_scalar(
        "SELECT award_id FROM billing_founding_awards WHERE beneficiary_id = $1",
    )
    .bind(&request.beneficiary_id)
    .fetch_optional(&mut **tx)
    .await?;
    let award = if let Some(award_id) = award_id.as_deref() {
        let award = founding_allocator::load_award(tx, award_id)
            .await?
            .ok_or(TransferError::CorruptState)?;
        let (payer_kind, payer_id) = payer_identity(&request, request.source_kind)?;
        if award.payer_kind != payer_kind || award.payer_id != payer_id {
            return Err(TransferError::FoundingPayerConflict);
        }
        Some(award)
    } else {
        None
    };
    request.offer =
        offer_at_transfer_boundary(&source_offer, request.effective_from, award.as_ref())?;
    request.validate(now_epoch)?;
    Ok(request)
}

async fn preauthorise_transfer_draft(
    tx: &mut Transaction<'_, Postgres>,
    actor_user_id: &str,
    draft: &TransferDraft,
    now_epoch: i64,
) -> Result<(), TransferError> {
    // An inactive/missing source is still allowed to reach SourceInactive for its beneficiary.
    // Otherwise consider the currently active source payer(s), without locking rows or exposing
    // which one exists to an unauthorised caller.
    let personal_source: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM billing_personal_accounts \
         WHERE user_id = $1 AND state = 'active' AND cancel_at_period_end = FALSE \
           AND cancellation_requested_at IS NULL AND stripe_subscription_id IS NOT NULL)",
    )
    .bind(&draft.beneficiary_id)
    .fetch_one(&mut **tx)
    .await?;
    let sponsored_sources: Vec<String> = sqlx::query_scalar(
        "SELECT seat.organization_id \
         FROM billing_sponsored_seats AS seat \
         JOIN billing_sponsored_subscriptions AS subscription \
           ON subscription.organization_id = seat.organization_id \
         WHERE seat.beneficiary_id = $1 AND seat.state = 'active' \
           AND subscription.status = 'active' \
           AND subscription.provider_subscription_id IS NOT NULL \
           AND subscription.current_period_end IS NOT NULL \
           AND seat.effective_from <= $2 \
           AND (seat.effective_until IS NULL OR \
                seat.effective_until >= subscription.current_period_end) \
         ORDER BY seat.organization_id LIMIT 2",
    )
    .bind(&draft.beneficiary_id)
    .bind(now_epoch)
    .fetch_all(&mut **tx)
    .await?;

    let mut source_candidates = Vec::new();
    if personal_source || sponsored_sources.is_empty() {
        source_candidates.push((TransferPayer::Personal, None));
    }
    source_candidates.extend(
        sponsored_sources
            .iter()
            .map(|organization_id| (TransferPayer::Sponsor, Some(organization_id.as_str()))),
    );

    let mut organization_ids = sponsored_sources
        .iter()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    if draft.destination_kind == TransferPayer::Sponsor {
        organization_ids.insert(
            draft
                .destination_organization_id
                .as_deref()
                .ok_or(TransferError::CorruptState)?,
        );
    }

    let mut organization_authority = std::collections::BTreeMap::new();
    for organization_id in organization_ids {
        for user_id in [actor_user_id, draft.counterparty_user_id.as_str()] {
            let allowed: bool = sqlx::query_scalar(
                "SELECT EXISTS( \
                   SELECT 1 FROM organizations AS o \
                   JOIN organization_memberships AS m ON m.org_id = o.id \
                   WHERE o.id = $1 AND m.user_id = $2 \
                     AND o.lifecycle_state = 'active' AND m.role IN ('owner','admin'))",
            )
            .bind(organization_id)
            .bind(user_id)
            .fetch_one(&mut **tx)
            .await?;
            organization_authority.insert((organization_id, user_id), allowed);
        }
    }

    let destination_organization_id = draft.destination_organization_id.as_deref();
    let permitted = source_candidates
        .into_iter()
        .any(|(source_kind, source_org)| {
            let actor_source = payer_authorised(
                source_kind,
                source_org,
                &draft.beneficiary_id,
                actor_user_id,
                &organization_authority,
            );
            let actor_destination = payer_authorised(
                draft.destination_kind,
                destination_organization_id,
                &draft.beneficiary_id,
                actor_user_id,
                &organization_authority,
            );
            let counterparty_source = payer_authorised(
                source_kind,
                source_org,
                &draft.beneficiary_id,
                &draft.counterparty_user_id,
                &organization_authority,
            );
            let counterparty_destination = payer_authorised(
                draft.destination_kind,
                destination_organization_id,
                &draft.beneficiary_id,
                &draft.counterparty_user_id,
                &organization_authority,
            );
            (actor_source && counterparty_destination) || (actor_destination && counterparty_source)
        });
    permitted.then_some(()).ok_or(TransferError::Unauthorised)
}

pub(crate) fn offer_at_transfer_boundary(
    source_offer: &str,
    effective_from: i64,
    award: Option<&founding_allocator::FoundingAward>,
) -> Result<BillingOffer, TransferError> {
    let source_offer = parse_offer(source_offer)?;
    let Some(award) = award else {
        if founding_allocator::FoundingOffer::from_billing_offer(source_offer).is_some() {
            return Err(TransferError::CorruptState);
        }
        return Ok(source_offer);
    };
    let founding_end = award
        .original_end_epoch
        .ok_or(TransferError::CorruptState)?;
    let founding_offer = award.offer.billing_offer();
    let expected_source = if effective_from <= founding_end {
        founding_offer
    } else {
        award.offer.standard_billing_offer()
    };
    if source_offer != expected_source {
        return Err(TransferError::SourceTermChanged);
    }
    Ok(if effective_from < founding_end {
        founding_offer
    } else {
        award.offer.standard_billing_offer()
    })
}

impl From<TransferError> for Error {
    fn from(error: TransferError) -> Self {
        match error {
            TransferError::MissingTransfer => Self::NotFound("transfer not found".into()),
            TransferError::InvalidField(field) => {
                Self::BadRequest(format!("invalid transfer {field}"))
            }
            TransferError::QuoteExpired => Self::Conflict("transfer quote expired".into()),
            TransferError::SourceInactive => Self::Conflict("transfer source is not active".into()),
            TransferError::SourceTermChanged => {
                Self::Conflict("source billing term changed; refresh the transfer quote".into())
            }
            TransferError::DestinationAlreadyCovers => {
                Self::Conflict("transfer destination already covers this beneficiary".into())
            }
            TransferError::DestinationSubscriptionUnavailable => {
                Self::Conflict("destination sponsor must have an active subscription".into())
            }
            TransferError::DestinationTermChanged => Self::Conflict(
                "destination sponsor renewal does not match the source paid-through boundary"
                    .into(),
            ),
            TransferError::InvitationPending => Self::Conflict(
                "a payer transfer invitation is pending; the person who started it or the invited payer can withdraw it"
                    .into(),
            ),
            TransferError::AlreadyInProgress => {
                Self::Conflict("a transfer is already in progress for this beneficiary".into())
            }
            TransferError::Unauthorised => Self::Forbidden("transfer is not authorised".into()),
            TransferError::BeneficiaryMissing => {
                Self::NotFound("billing beneficiary not found".into())
            }
            TransferError::CorruptState => Self::Internal("transfer state is corrupt".into()),
            TransferError::ResultConflict
            | TransferError::FoundingPayerConflict
            | TransferError::InvalidTransition => {
                Self::Conflict("transfer state conflicts with stored evidence".into())
            }
            TransferError::Database(error) => Self::Db(error),
            TransferError::Founding(error) => Self::Internal(error.to_string()),
        }
    }
}

pub async fn begin_transfer(
    tx: &mut Transaction<'_, Postgres>,
    request: &TransferRequest,
    now_epoch: i64,
) -> Result<BeginTransfer, TransferError> {
    request.validate_shape()?;
    // All payer changes and coverage mutations take the founding lock before account rows. This
    // gives transfer creation the same global lock order as checkout and invoice settlement.
    founding_allocator::lock_capacity(tx).await?;
    authorise_transfer_parties(tx, request).await?;
    sqlx::query(
        "UPDATE billing_transfer_intents SET state = 'failed', result_code = 'consent_expired', \
         updated_at = now() \
         WHERE beneficiary_id = $1 AND state = 'awaiting_consent' \
           AND (consent_expires_at_epoch <= $2 OR effective_from <= $2)",
    )
    .bind(&request.beneficiary_id)
    .bind(now_epoch)
    .execute(&mut **tx)
    .await?;
    if let Some(existing) =
        load_by_idempotency(tx, &request.actor_user_id, &request.idempotency_key).await?
    {
        if existing.request_hash != request.request_hash() {
            return Err(TransferError::ResultConflict);
        }
        return Ok(BeginTransfer::AlreadyExists(existing));
    }
    request.validate_timing(now_epoch)?;
    let live: Option<(String, String)> = sqlx::query_as(
        "SELECT transfer_id, state FROM billing_transfer_intents WHERE beneficiary_id = $1 \
         AND (state IN ('pending','destination_prepared','destination_paid','source_adjustment_pending','source_restoration_pending','unknown') \
              OR (state = 'awaiting_consent' AND consent_expires_at_epoch > $2 AND effective_from > $2)) \
         FOR UPDATE",
    )
    .bind(&request.beneficiary_id)
    .bind(now_epoch)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some((_, state)) = live {
        return Err(if state == "awaiting_consent" {
            TransferError::InvitationPending
        } else {
            TransferError::AlreadyInProgress
        });
    }
    let source_offer = ensure_source_active(tx, request, now_epoch).await?;
    ensure_no_pending_billing_mutation(tx, &request.beneficiary_id).await?;
    ensure_destination_free(tx, request).await?;
    let founding_award_id: Option<String> = sqlx::query_scalar(
        "SELECT award_id FROM billing_founding_awards WHERE beneficiary_id = $1",
    )
    .bind(&request.beneficiary_id)
    .fetch_optional(&mut **tx)
    .await?;
    let award = if let Some(award_id) = founding_award_id.as_deref() {
        let award = founding_allocator::load_award(tx, award_id)
            .await?
            .ok_or(TransferError::CorruptState)?;
        let (source_kind, source_id) = payer_identity(request, request.source_kind)?;
        if award.payer_kind != source_kind || award.payer_id != source_id {
            return Err(TransferError::FoundingPayerConflict);
        }
        Some(award)
    } else {
        None
    };
    validate_offer_at_boundary(request, &source_offer, award.as_ref())?;
    let consent_expires_at_epoch = now_epoch
        .checked_add(TRANSFER_CONSENT_SECONDS)
        .ok_or(TransferError::InvalidField("consent expiry"))?
        .min(request.effective_from);
    let transfer_id = format!("transfer:{}", Uuid::new_v4());
    let provider_key = format!(
        "sotto-transfer:{}",
        hex_digest(&Sha256::digest(transfer_id.as_bytes()))
    );
    let initial_state = if request.actor_user_id == request.counterparty_user_id {
        TransferState::Pending
    } else {
        if consent_expires_at_epoch <= now_epoch {
            return Err(TransferError::InvalidField("effective_from"));
        }
        TransferState::AwaitingConsent
    };
    sqlx::query(
        "INSERT INTO billing_transfer_intents \
         (transfer_id, actor_user_id, counterparty_user_id, beneficiary_id, source_kind, source_organization_id, \
          destination_kind, destination_organization_id, offer, quote_version, quote_expires_at_epoch, \
          consent_expires_at_epoch, \
          effective_from, effective_until, idempotency_key, request_hash, provider_idempotency_key, \
          founding_award_id, state, source_provider_subscription_id) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20)",
    )
    .bind(&transfer_id)
    .bind(&request.actor_user_id)
    .bind(&request.counterparty_user_id)
    .bind(&request.beneficiary_id)
    .bind(request.source_kind.as_str())
    .bind(request.source_organization_id.as_deref())
    .bind(request.destination_kind.as_str())
    .bind(request.destination_organization_id.as_deref())
    .bind(request.offer.as_str())
    .bind(request.quote_version)
    .bind(request.quote_expires_at_epoch)
    .bind(consent_expires_at_epoch)
    .bind(request.effective_from)
    .bind(request.effective_until)
    .bind(&request.idempotency_key)
    .bind(request.request_hash())
    .bind(provider_key)
    .bind(founding_award_id)
    .bind(initial_state.as_str())
    .bind(&request.source_provider_subscription_id)
    .execute(&mut **tx)
    .await?;
    Ok(BeginTransfer::Created(
        load_transfer(tx, &transfer_id).await?,
    ))
}

/// Accept the counterparty's side of a two-party transfer. The actor may name the
/// counterparty, but only that user can move the intent out of `awaiting_consent`.
/// The authority and coverage checks are repeated while the same user and organisation
/// rows are locked, so a stale invitation cannot authorise a transfer after either side
/// has changed.
pub async fn accept_transfer(
    tx: &mut Transaction<'_, Postgres>,
    transfer_id: &str,
    user_id: &str,
    now_epoch: i64,
) -> Result<TransferIntent, TransferError> {
    if user_id.trim().is_empty() {
        return Err(TransferError::InvalidField("user_id"));
    }
    founding_allocator::lock_capacity(tx).await?;
    let invitation = load_transfer_snapshot(tx, transfer_id).await?;
    if invitation.counterparty_user_id != user_id {
        return Err(TransferError::Unauthorised);
    }
    if invitation.state != TransferState::AwaitingConsent
        && invitation.state != TransferState::Pending
    {
        return Err(TransferError::InvalidTransition);
    }
    if invitation.state == TransferState::Pending {
        return Ok(invitation);
    }
    if invitation.consent_expires_at_epoch <= now_epoch {
        return Err(TransferError::InvalidTransition);
    }
    let request = request_from_intent(&invitation)?;
    authorise_transfer_parties(tx, &request).await?;
    let current = load_transfer(tx, transfer_id).await?;
    if current.counterparty_user_id != user_id {
        return Err(TransferError::Unauthorised);
    }
    if current.state == TransferState::Pending {
        return Ok(current);
    }
    if current.state != TransferState::AwaitingConsent {
        return Err(TransferError::InvalidTransition);
    }
    if current.consent_expires_at_epoch <= now_epoch {
        return Err(TransferError::InvalidTransition);
    }
    let request = request_from_intent(&current)?;
    request.validate_acceptance_timing(now_epoch)?;
    let source_offer = ensure_source_active(tx, &request, now_epoch).await?;
    ensure_no_pending_billing_mutation(tx, &request.beneficiary_id).await?;
    ensure_destination_free(tx, &request).await?;
    let award = if let Some(award_id) = current.founding_award_id.as_deref() {
        let award = founding_allocator::load_award(tx, award_id)
            .await?
            .ok_or(TransferError::CorruptState)?;
        let (source_kind, source_id) = payer_identity(&request, request.source_kind)?;
        if award.payer_kind != source_kind || award.payer_id != source_id {
            return Err(TransferError::FoundingPayerConflict);
        }
        Some(award)
    } else {
        None
    };
    validate_offer_at_boundary(&request, &source_offer, award.as_ref())?;
    let updated = sqlx::query(
        "UPDATE billing_transfer_intents SET state = 'pending', updated_at = now() \
         WHERE transfer_id = $1 AND state = 'awaiting_consent' RETURNING transfer_id",
    )
    .bind(transfer_id)
    .fetch_optional(&mut **tx)
    .await?;
    if updated.is_none() {
        return Err(TransferError::InvalidTransition);
    }
    load_transfer(tx, transfer_id).await
}

/// Serialise a payer/coverage mutation against transfer creation and reject changes while a
/// beneficiary has a live transfer. Invitations whose consent deadline or renewal boundary has
/// passed do not hold the beneficiary's billing state; `begin_transfer` marks them failed when
/// the next transfer is started.
pub async fn lock_and_check_no_live_transfer(
    tx: &mut Transaction<'_, Postgres>,
    beneficiary_ids: &[&str],
    now_epoch: i64,
) -> Result<(), TransferError> {
    lock_and_check_no_live_transfer_except(tx, beneficiary_ids, &[], now_epoch).await
}

/// Lock affected beneficiaries and reject every live intent except the specific transfer whose
/// verified destination invoice is being settled in this transaction.
pub async fn lock_and_check_no_live_transfer_except(
    tx: &mut Transaction<'_, Postgres>,
    beneficiary_ids: &[&str],
    allowed_transfer_ids: &[String],
    now_epoch: i64,
) -> Result<(), TransferError> {
    lock_and_check_transfer_except(tx, beneficiary_ids, allowed_transfer_ids, now_epoch, true).await
}

/// Lock beneficiaries for provider webhooks. A consent invitation has not changed provider state,
/// so it must not prevent Stripe events from updating the payer's current subscription. Accepted
/// transfers and later execution states remain fenced as before.
pub async fn lock_and_check_no_provider_transfer_except(
    tx: &mut Transaction<'_, Postgres>,
    beneficiary_ids: &[&str],
    allowed_transfer_ids: &[String],
    now_epoch: i64,
) -> Result<(), TransferError> {
    lock_and_check_transfer_except(tx, beneficiary_ids, allowed_transfer_ids, now_epoch, false)
        .await
}

async fn lock_and_check_transfer_except(
    tx: &mut Transaction<'_, Postgres>,
    beneficiary_ids: &[&str],
    allowed_transfer_ids: &[String],
    now_epoch: i64,
    include_consent_invitations: bool,
) -> Result<(), TransferError> {
    founding_allocator::lock_capacity(tx).await?;

    let user_ids = beneficiary_ids
        .iter()
        .copied()
        .filter(|id| !id.trim().is_empty())
        .collect::<std::collections::BTreeSet<_>>();
    let user_ids = user_ids.into_iter().map(str::to_owned).collect::<Vec<_>>();
    if user_ids.is_empty() {
        return Ok(());
    }
    let locked_user_ids: Vec<String> =
        sqlx::query_scalar("SELECT id FROM users WHERE id = ANY($1) ORDER BY id FOR UPDATE")
            .bind(&user_ids)
            .fetch_all(&mut **tx)
            .await?;
    if locked_user_ids.len() != user_ids.len() {
        return Err(TransferError::BeneficiaryMissing);
    }

    // Invoice settlement may be permitted to update one live transfer. Lock that row before
    // billing_personal_accounts is locked by the caller, matching the worker's
    // capacity -> beneficiary -> transfer -> billing-account order.
    if !allowed_transfer_ids.is_empty() {
        sqlx::query_scalar::<_, String>(
            "SELECT transfer_id FROM billing_transfer_intents \
             WHERE transfer_id = ANY($1) AND beneficiary_id = ANY($2) \
             ORDER BY transfer_id FOR UPDATE",
        )
        .bind(allowed_transfer_ids)
        .bind(&user_ids)
        .fetch_all(&mut **tx)
        .await?;
    }

    let active: Option<(String, String)> = sqlx::query_as(
        "SELECT transfer_id, state FROM billing_transfer_intents \
         WHERE beneficiary_id = ANY($1) AND (state IN \
           ('pending','destination_prepared','destination_paid','source_adjustment_pending','source_restoration_pending','unknown') \
           OR ($4 AND state = 'awaiting_consent' AND consent_expires_at_epoch > $2 AND effective_from > $2)) \
           AND NOT (transfer_id = ANY($3)) \
         ORDER BY beneficiary_id, transfer_id LIMIT 1 FOR UPDATE",
    )
    .bind(&user_ids)
    .bind(now_epoch)
    .bind(allowed_transfer_ids)
    .bind(include_consent_invitations)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some((_, state)) = active {
        return Err(if state == "awaiting_consent" {
            TransferError::InvitationPending
        } else {
            TransferError::AlreadyInProgress
        });
    }
    Ok(())
}

async fn ensure_no_pending_billing_mutation(
    tx: &mut Transaction<'_, Postgres>,
    beneficiary_id: &str,
) -> Result<(), TransferError> {
    let pending: bool = sqlx::query_scalar(
        "SELECT EXISTS(\
             SELECT 1 FROM billing_sponsored_operations \
             WHERE (beneficiary_id = $1 OR replacement_beneficiary_id = $1) \
               AND state IN ('pending', 'checkout_created', 'provider_pending', 'unknown')\
         ) OR EXISTS(\
             SELECT 1 FROM billing_correction_requests \
             WHERE beneficiary_id = $1 AND early_termination_confirmed_at_epoch IS NOT NULL \
               AND state IN ('requested', 'approved', 'provider_pending', 'termination_pending', 'unknown')\
         )",
    )
    .bind(beneficiary_id)
    .fetch_one(&mut **tx)
    .await?;
    if pending {
        return Err(TransferError::AlreadyInProgress);
    }
    Ok(())
}

/// Withdraw or decline an invitation before provider work starts. Either participant can clear
/// the consent slot; later provider states are deliberately irreversible here.
pub async fn withdraw_transfer(
    tx: &mut Transaction<'_, Postgres>,
    transfer_id: &str,
    user_id: &str,
    now_epoch: i64,
) -> Result<TransferIntent, TransferError> {
    if user_id.trim().is_empty() {
        return Err(TransferError::InvalidField("user_id"));
    }
    founding_allocator::lock_capacity(tx).await?;
    let current = load_transfer(tx, transfer_id).await?;
    if current.actor_user_id != user_id && current.counterparty_user_id != user_id {
        return Err(TransferError::Unauthorised);
    }
    if current.state != TransferState::AwaitingConsent {
        return Err(TransferError::InvalidTransition);
    }
    if current.consent_expires_at_epoch <= now_epoch {
        return Err(TransferError::InvalidTransition);
    }
    sqlx::query(
        "UPDATE billing_transfer_intents SET state = 'failed', result_code = 'consent_withdrawn', \
         updated_at = now() WHERE transfer_id = $1 AND state = 'awaiting_consent'",
    )
    .bind(transfer_id)
    .execute(&mut **tx)
    .await?;
    load_transfer(tx, transfer_id).await
}

async fn authorise_transfer_parties(
    tx: &mut Transaction<'_, Postgres>,
    request: &TransferRequest,
) -> Result<(), TransferError> {
    let user_ids = [
        request.actor_user_id.as_str(),
        request.counterparty_user_id.as_str(),
        request.beneficiary_id.as_str(),
    ]
    .into_iter()
    .collect::<std::collections::BTreeSet<_>>();
    for user_id in user_ids {
        let exists: Option<String> =
            sqlx::query_scalar("SELECT id FROM users WHERE id = $1 FOR UPDATE")
                .bind(user_id)
                .fetch_optional(&mut **tx)
                .await?;
        if exists.is_none() {
            return Err(TransferError::Unauthorised);
        }
    }
    let organization_ids = [
        request.source_organization_id.as_deref(),
        request.destination_organization_id.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect::<std::collections::BTreeSet<_>>();
    let mut organization_authority = std::collections::BTreeMap::new();
    for organization_id in organization_ids {
        for user_id in [
            request.actor_user_id.as_str(),
            request.counterparty_user_id.as_str(),
        ]
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>()
        {
            let allowed = org::access_for_update(tx, organization_id, user_id)
                .await
                .map(|access| access.require_write().is_ok() && access.role().can_manage_members())
                .unwrap_or(false);
            organization_authority.insert((organization_id, user_id), allowed);
        }
    }
    let actor_source = payer_authorised(
        request.source_kind,
        request.source_organization_id.as_deref(),
        &request.beneficiary_id,
        &request.actor_user_id,
        &organization_authority,
    );
    let actor_destination = payer_authorised(
        request.destination_kind,
        request.destination_organization_id.as_deref(),
        &request.beneficiary_id,
        &request.actor_user_id,
        &organization_authority,
    );
    let counterparty_source = payer_authorised(
        request.source_kind,
        request.source_organization_id.as_deref(),
        &request.beneficiary_id,
        &request.counterparty_user_id,
        &organization_authority,
    );
    let counterparty_destination = payer_authorised(
        request.destination_kind,
        request.destination_organization_id.as_deref(),
        &request.beneficiary_id,
        &request.counterparty_user_id,
        &organization_authority,
    );
    if (actor_source && counterparty_destination) || (actor_destination && counterparty_source) {
        Ok(())
    } else {
        Err(TransferError::Unauthorised)
    }
}

fn request_from_intent(intent: &TransferIntent) -> Result<TransferRequest, TransferError> {
    Ok(TransferRequest {
        actor_user_id: intent.actor_user_id.clone(),
        counterparty_user_id: intent.counterparty_user_id.clone(),
        beneficiary_id: intent.beneficiary_id.clone(),
        source_kind: intent.source_kind,
        source_organization_id: intent.source_organization_id.clone(),
        source_provider_subscription_id: intent
            .source_provider_subscription_id
            .clone()
            .ok_or(TransferError::CorruptState)?,
        destination_kind: intent.destination_kind,
        destination_organization_id: intent.destination_organization_id.clone(),
        offer: intent.offer,
        quote_version: intent.quote_version,
        quote_expires_at_epoch: intent.quote_expires_at_epoch,
        effective_from: intent.effective_from,
        effective_until: intent.effective_until,
        idempotency_key: intent.idempotency_key.clone(),
    })
}

pub async fn record_destination_prepared(
    tx: &mut Transaction<'_, Postgres>,
    transfer_id: &str,
    destination_operation_id: &str,
    provider_subscription_id: Option<&str>,
) -> Result<TransferIntent, TransferError> {
    if destination_operation_id.trim().is_empty() {
        return Err(TransferError::InvalidField("destination_operation_id"));
    }
    if provider_subscription_id.is_some_and(|value| value.trim().is_empty()) {
        return Err(TransferError::InvalidField("provider_subscription_id"));
    }
    let updated = sqlx::query(
        "UPDATE billing_transfer_intents SET state = 'destination_prepared', \
         destination_operation_id = $2, destination_provider_subscription_id = $3, updated_at = now() \
         WHERE transfer_id = $1 AND state = 'pending' RETURNING transfer_id",
    )
    .bind(transfer_id)
    .bind(destination_operation_id)
    .bind(provider_subscription_id)
    .fetch_optional(&mut **tx)
    .await?;
    if updated.is_none() {
        let current = load_transfer(tx, transfer_id).await?;
        if !matches!(
            current.state,
            TransferState::DestinationPrepared
                | TransferState::DestinationPaid
                | TransferState::SourceAdjustmentPending
                | TransferState::Completed
        ) || current.destination_operation_id.as_deref() != Some(destination_operation_id)
            || current.destination_provider_subscription_id.as_deref() != provider_subscription_id
        {
            return Err(TransferError::ResultConflict);
        }
    }
    load_transfer(tx, transfer_id).await
}

/// Attach a verified Setup Checkout completion to the pending personal destination. The caller
/// verifies the signed session and retrieves the SetupIntent from Stripe before this write.
pub async fn record_destination_setup_completion(
    tx: &mut Transaction<'_, Postgres>,
    transfer_id: &str,
    customer_id: &str,
    session_id: &str,
    setup_intent_id: &str,
) -> Result<TransferIntent, TransferError> {
    if [customer_id, session_id, setup_intent_id]
        .iter()
        .any(|value| value.trim().is_empty())
    {
        return Err(TransferError::InvalidField("setup evidence"));
    }
    let current = load_transfer(tx, transfer_id).await?;
    if current.destination_kind != TransferPayer::Personal
        || current.destination_customer_id.as_deref() != Some(customer_id)
        || current.destination_setup_session_id.as_deref() != Some(session_id)
    {
        return Err(TransferError::ResultConflict);
    }
    if let Some(existing) = current.destination_setup_intent_id.as_deref() {
        return if existing == setup_intent_id {
            Ok(current)
        } else {
            Err(TransferError::ResultConflict)
        };
    }
    if !matches!(
        current.state,
        TransferState::Pending | TransferState::DestinationPrepared
    ) {
        return Err(TransferError::InvalidTransition);
    }
    let result = sqlx::query(
        "UPDATE billing_transfer_intents SET destination_setup_intent_id = $2, updated_at = now() \
         WHERE transfer_id = $1 AND destination_kind = 'personal' \
           AND destination_customer_id = $3 AND destination_setup_session_id = $4 \
           AND destination_setup_intent_id IS NULL \
           AND state IN ('pending','destination_prepared') RETURNING transfer_id",
    )
    .bind(transfer_id)
    .bind(setup_intent_id)
    .bind(customer_id)
    .bind(session_id)
    .fetch_optional(&mut **tx)
    .await?;
    if result.is_none() {
        let current = load_transfer(tx, transfer_id).await?;
        if current.destination_setup_intent_id.as_deref() != Some(setup_intent_id) {
            return Err(TransferError::ResultConflict);
        }
    }
    load_transfer(tx, transfer_id).await
}

/// Persist a provider-read-back schedule before preparing the source side. Personal schedules
/// require the beneficiary's verified SetupIntent; sponsored schedules must bind an existing
/// subscription and do not accept personal setup evidence.
#[allow(clippy::too_many_arguments)]
pub async fn record_destination_schedule(
    tx: &mut Transaction<'_, Postgres>,
    transfer_id: &str,
    operation_id: &str,
    customer_id: &str,
    schedule_id: &str,
    subscription_id: Option<&str>,
    setup_intent_id: Option<&str>,
    expected_period_end: i64,
) -> Result<TransferIntent, TransferError> {
    if [operation_id, customer_id, schedule_id]
        .iter()
        .any(|value| value.trim().is_empty())
        || subscription_id.is_some_and(|value| value.trim().is_empty())
        || setup_intent_id.is_some_and(|value| value.trim().is_empty())
        || expected_period_end <= 0
    {
        return Err(TransferError::InvalidField("destination schedule evidence"));
    }
    let current = load_transfer(tx, transfer_id).await?;
    if expected_period_end <= current.effective_from {
        return Err(TransferError::DestinationTermChanged);
    }
    match current.destination_kind {
        TransferPayer::Personal
            if current.destination_customer_id.as_deref() != Some(customer_id)
                || current.destination_setup_intent_id.as_deref() != setup_intent_id
                || setup_intent_id.is_none() =>
        {
            return Err(TransferError::ResultConflict);
        }
        TransferPayer::Sponsor if setup_intent_id.is_some() || subscription_id.is_none() => {
            return Err(TransferError::ResultConflict);
        }
        _ => {}
    }
    if let Some(stored) = current.destination_schedule_id.as_deref() {
        if stored != schedule_id
            || current.destination_operation_id.as_deref() != Some(operation_id)
            || current.destination_customer_id.as_deref() != Some(customer_id)
            || current.destination_provider_subscription_id.as_deref() != subscription_id
            || current.destination_expected_period_end != Some(expected_period_end)
        {
            return Err(TransferError::ResultConflict);
        }
        return Ok(current);
    }
    if !matches!(
        current.state,
        TransferState::Pending | TransferState::Unknown
    ) {
        return Err(TransferError::InvalidTransition);
    }
    let updated = sqlx::query(
        "UPDATE billing_transfer_intents SET state = 'destination_prepared', \
             destination_operation_id = $2, destination_customer_id = $3, \
             destination_schedule_id = $4, destination_provider_subscription_id = $5, \
             destination_expected_period_end = $6, \
             updated_at = now() \
         WHERE transfer_id = $1 AND state IN ('pending','unknown') \
           AND (destination_customer_id IS NULL OR destination_customer_id = $3) \
           AND destination_schedule_id IS NULL \
         RETURNING transfer_id",
    )
    .bind(transfer_id)
    .bind(operation_id)
    .bind(customer_id)
    .bind(schedule_id)
    .bind(subscription_id)
    .bind(expected_period_end)
    .fetch_optional(&mut **tx)
    .await?;
    if updated.is_none() {
        let current = load_transfer(tx, transfer_id).await?;
        if current.destination_schedule_id.as_deref() != Some(schedule_id)
            || current.destination_operation_id.as_deref() != Some(operation_id)
            || current.destination_customer_id.as_deref() != Some(customer_id)
            || current.destination_provider_subscription_id.as_deref() != subscription_id
            || current.destination_expected_period_end != Some(expected_period_end)
        {
            return Err(TransferError::ResultConflict);
        }
    }
    load_transfer(tx, transfer_id).await
}

/// Link the first signed paid invoice for a future personal subscription to its already-recorded
/// schedule. The caller has already read and checked the invoice and payment with Stripe; this
/// store rechecks the transfer, customer, offer and exact renewal boundary before writing.
#[allow(clippy::too_many_arguments)]
pub async fn link_personal_destination_subscription(
    tx: &mut Transaction<'_, Postgres>,
    transfer_id: &str,
    beneficiary_id: &str,
    operation_id: &str,
    allocation_reference: &str,
    customer_id: &str,
    subscription_id: &str,
    offer: &str,
    invoice_period_start: i64,
) -> Result<TransferIntent, TransferError> {
    for value in [
        transfer_id,
        beneficiary_id,
        operation_id,
        allocation_reference,
        customer_id,
        subscription_id,
    ] {
        if value.trim().is_empty() {
            return Err(TransferError::InvalidField("destination invoice binding"));
        }
    }
    let transfer = load_transfer(tx, transfer_id).await?;
    if transfer.destination_kind != TransferPayer::Personal
        || transfer.beneficiary_id != beneficiary_id
        || transfer.destination_operation_id.as_deref() != Some(operation_id)
        || transfer.destination_customer_id.as_deref() != Some(customer_id)
        || transfer.offer.as_str() != offer
        || transfer.effective_from != invoice_period_start
        || transfer
            .destination_provider_subscription_id
            .as_deref()
            .is_some_and(|stored| stored != subscription_id)
        || !matches!(
            transfer.state,
            TransferState::DestinationPrepared
                | TransferState::DestinationPaid
                | TransferState::SourceAdjustmentPending
                | TransferState::Completed
        )
    {
        return Err(TransferError::ResultConflict);
    }
    let account = sqlx::query(
        "UPDATE billing_personal_accounts SET stripe_subscription_id = $4, updated_at = now() \
         WHERE user_id = $1 AND operation_id = $2 AND allocation_reference = $3 \
           AND stripe_customer_id = $5 AND offer = $6 \
           AND (stripe_subscription_id IS NULL OR stripe_subscription_id = $4) \
         RETURNING user_id",
    )
    .bind(beneficiary_id)
    .bind(operation_id)
    .bind(allocation_reference)
    .bind(subscription_id)
    .bind(customer_id)
    .bind(offer)
    .fetch_optional(&mut **tx)
    .await?;
    if account.is_none() {
        return Err(TransferError::ResultConflict);
    }
    let updated = sqlx::query(
        "UPDATE billing_transfer_intents SET destination_provider_subscription_id = $2, updated_at = now() \
         WHERE transfer_id = $1 AND destination_kind = 'personal' \
           AND (destination_provider_subscription_id IS NULL OR destination_provider_subscription_id = $2) \
         RETURNING transfer_id",
    )
    .bind(transfer_id)
    .bind(subscription_id)
    .fetch_optional(&mut **tx)
    .await?;
    if updated.is_none() {
        return Err(TransferError::ResultConflict);
    }
    load_transfer(tx, transfer_id).await
}

pub async fn record_destination_paid(
    tx: &mut Transaction<'_, Postgres>,
    transfer_id: &str,
    payment_reference: &str,
    invoice_period_start: i64,
    invoice_period_end: i64,
) -> Result<TransferIntent, TransferError> {
    if payment_reference.trim().is_empty() {
        return Err(TransferError::InvalidField("payment_reference"));
    }
    if invoice_period_start <= 0 || invoice_period_end <= invoice_period_start {
        return Err(TransferError::InvalidField("invoice period"));
    }
    founding_allocator::lock_capacity(tx).await?;
    let current = load_transfer(tx, transfer_id).await?;
    if current
        .destination_period_start
        .is_some_and(|recorded_start| recorded_start != invoice_period_start)
        || current
            .destination_period_end
            .is_some_and(|recorded_end| recorded_end != invoice_period_end)
    {
        return Err(TransferError::ResultConflict);
    }
    let expected_period_end = current.destination_expected_period_end.or_else(|| {
        (current.destination_kind == TransferPayer::Personal)
            .then(|| expected_transfer_period_end(current.offer, current.effective_from))
            .flatten()
    });
    let period_mismatch_code = if current.effective_from != invoice_period_start {
        Some("destination_period_start_mismatch")
    } else if expected_period_end != Some(invoice_period_end) {
        Some("destination_period_end_mismatch")
    } else {
        None
    };
    if let Some(period_mismatch_code) = period_mismatch_code {
        let updated = sqlx::query(
            "UPDATE billing_transfer_intents SET \
             destination_payment_reference = $2, destination_period_start = $3, destination_period_end = $4, \
             execution_manual_review_required = TRUE, \
             execution_review_generation = execution_review_generation + CASE \
                 WHEN execution_manual_review_required AND last_execution_error_code = $5 \
                   AND destination_payment_reference = $2 \
                   AND destination_period_start IS NOT DISTINCT FROM $3 \
                   AND destination_period_end IS NOT DISTINCT FROM $4 THEN 0 ELSE 1 END, \
             last_execution_error_code = $5, \
             execution_next_attempt_at = 'infinity'::timestamptz, \
             execution_lease_token = NULL, execution_lease_until = NULL, updated_at = now() \
             WHERE transfer_id = $1 AND state IN ('destination_prepared','source_adjustment_pending','unknown') \
               AND (destination_payment_reference IS NULL OR destination_payment_reference = $2) \
               AND (destination_period_start IS NULL OR destination_period_start = $3) \
               AND (destination_period_end IS NULL OR destination_period_end = $4) \
             RETURNING transfer_id",
        )
        .bind(transfer_id)
        .bind(payment_reference)
        .bind(invoice_period_start)
        .bind(invoice_period_end)
        .bind(period_mismatch_code)
        .fetch_optional(&mut **tx)
        .await?;
        if updated.is_none() {
            let current = load_transfer(tx, transfer_id).await?;
            if current.destination_payment_reference.as_deref() != Some(payment_reference)
                || current.destination_period_start != Some(invoice_period_start)
                || current.destination_period_end != Some(invoice_period_end)
                || current.last_execution_error_code.as_deref() != Some(period_mismatch_code)
            {
                return Err(TransferError::ResultConflict);
            }
        }
        return load_transfer(tx, transfer_id).await;
    }
    let updated = sqlx::query(
        "UPDATE billing_transfer_intents SET \
         state = CASE WHEN state IN ('source_adjustment_pending','unknown') THEN state ELSE 'destination_paid' END, \
         destination_payment_reference = $2, destination_period_start = $3, \
         destination_period_end = $4, \
         execution_manual_review_required = CASE WHEN state = 'unknown' THEN TRUE \
             ELSE execution_manual_review_required END, \
         execution_review_generation = execution_review_generation + CASE \
             WHEN state = 'unknown' AND (NOT execution_manual_review_required \
               OR last_execution_error_code IS DISTINCT FROM 'destination_invoice_received_while_unknown' \
               OR destination_payment_reference IS DISTINCT FROM $2 \
               OR destination_period_start IS DISTINCT FROM $3 \
               OR destination_period_end IS DISTINCT FROM $4) THEN 1 ELSE 0 END, \
         last_execution_error_code = CASE WHEN state = 'unknown' \
             THEN 'destination_invoice_received_while_unknown' \
             ELSE last_execution_error_code END, \
         execution_next_attempt_at = CASE WHEN state = 'unknown' THEN 'infinity'::timestamptz \
             ELSE execution_next_attempt_at END, \
         execution_lease_token = CASE WHEN state = 'unknown' THEN NULL ELSE execution_lease_token END, \
         execution_lease_until = CASE WHEN state = 'unknown' THEN NULL ELSE execution_lease_until END, \
         updated_at = now() \
         WHERE transfer_id = $1 AND state IN ('destination_prepared','source_adjustment_pending','unknown') \
           AND (destination_payment_reference IS NULL OR destination_payment_reference = $2) \
           AND (destination_period_start IS NULL OR destination_period_start = $3) \
           AND (destination_period_end IS NULL OR destination_period_end = $4) \
         RETURNING transfer_id",
    )
    .bind(transfer_id)
    .bind(payment_reference)
    .bind(invoice_period_start)
    .bind(invoice_period_end)
    .fetch_optional(&mut **tx)
    .await?;
    if updated.is_none() {
        let current = load_transfer(tx, transfer_id).await?;
        if matches!(
            current.state,
            TransferState::DestinationPaid
                | TransferState::SourceAdjustmentPending
                | TransferState::Completed
        ) && current.destination_payment_reference.as_deref() == Some(payment_reference)
            && current.destination_period_start == Some(invoice_period_start)
            && current.destination_period_end == Some(invoice_period_end)
        {
            return Ok(current);
        }
        return Err(TransferError::ResultConflict);
    }
    load_transfer(tx, transfer_id).await
}

fn expected_transfer_period_end(offer: BillingOffer, period_start: i64) -> Option<i64> {
    let interval = match offer {
        BillingOffer::StandardMonthly | BillingOffer::FoundingMonthly => FoundingOffer::Monthly,
        BillingOffer::StandardAnnual | BillingOffer::FoundingAnnual => FoundingOffer::Annual,
    };
    FoundingDate::anchored_term_end_epoch(period_start, interval, 1).ok()
}

/// Begin the source adjustment after the destination has been prepared. It can precede the
/// destination's first paid invoice so the source is ready to end at the renewal boundary. If
/// that payment then fails, the source must be restored before this transfer can become failed.
/// The provider call itself is outside this transaction; the operation id makes a retry safe.
pub async fn begin_source_adjustment(
    tx: &mut Transaction<'_, Postgres>,
    transfer_id: &str,
    source_operation_id: &str,
) -> Result<TransferIntent, TransferError> {
    if source_operation_id.trim().is_empty() {
        return Err(TransferError::InvalidField("source_operation_id"));
    }
    let updated = sqlx::query(
        "UPDATE billing_transfer_intents SET state = 'source_adjustment_pending', \
         source_operation_id = $2, updated_at = now() \
         WHERE transfer_id = $1 AND state IN ('destination_prepared','destination_paid') \
         RETURNING transfer_id",
    )
    .bind(transfer_id)
    .bind(source_operation_id)
    .fetch_optional(&mut **tx)
    .await?;
    if updated.is_none() {
        let current = load_transfer(tx, transfer_id).await?;
        if current.state == TransferState::SourceAdjustmentPending
            && current.source_operation_id.as_deref() == Some(source_operation_id)
        {
            return Ok(current);
        }
        return Err(TransferError::ResultConflict);
    }
    load_transfer(tx, transfer_id).await
}

pub async fn complete_source_adjustment(
    tx: &mut Transaction<'_, Postgres>,
    transfer_id: &str,
    source_adjustment_reference: &str,
) -> Result<TransferIntent, TransferError> {
    if source_adjustment_reference.trim().is_empty() {
        return Err(TransferError::InvalidField("source_adjustment_reference"));
    }
    founding_allocator::lock_capacity(tx).await?;
    let transfer = load_transfer(tx, transfer_id).await?;
    if transfer.state == TransferState::Completed {
        if transfer.source_adjustment_reference.as_deref() == Some(source_adjustment_reference) {
            return Ok(transfer);
        }
        return Err(TransferError::ResultConflict);
    }
    if transfer.state != TransferState::SourceAdjustmentPending
        || transfer.execution_manual_review_required
    {
        return Err(TransferError::InvalidTransition);
    }
    if transfer.destination_payment_reference.is_some() && transfer.destination_period_end.is_none()
    {
        return Err(TransferError::CorruptState);
    }
    if transfer
        .source_adjustment_reference
        .as_deref()
        .is_some_and(|stored| stored != source_adjustment_reference)
    {
        return Err(TransferError::ResultConflict);
    }
    if transfer.destination_payment_reference.is_none() {
        sqlx::query(
            "UPDATE billing_transfer_intents SET source_adjustment_reference = $2, \
             updated_at = now() WHERE transfer_id = $1 AND state = 'source_adjustment_pending'",
        )
        .bind(transfer_id)
        .bind(source_adjustment_reference)
        .execute(&mut **tx)
        .await?;
        return load_transfer(tx, transfer_id).await;
    }
    if let Some(award_id) = transfer.founding_award_id.as_deref() {
        let destination_payer = match transfer.destination_kind {
            TransferPayer::Personal => transfer.beneficiary_id.as_str(),
            TransferPayer::Sponsor => transfer
                .destination_organization_id
                .as_deref()
                .ok_or(TransferError::CorruptState)?,
        };
        let award = founding_allocator::load_award(tx, award_id)
            .await?
            .ok_or(TransferError::CorruptState)?;
        if award.beneficiary_id != transfer.beneficiary_id {
            return Err(TransferError::CorruptState);
        }
        let (source_kind, source_payer) = payer_identity_for_transfer(
            transfer.source_kind,
            &transfer.beneficiary_id,
            transfer.source_organization_id.as_deref(),
        )?;
        let destination_kind = match transfer.destination_kind {
            TransferPayer::Personal => founding_allocator::FoundingPayerKind::Personal,
            TransferPayer::Sponsor => founding_allocator::FoundingPayerKind::Sponsor,
        };
        if (award.payer_kind != source_kind || award.payer_id != source_payer)
            && (award.payer_kind != destination_kind || award.payer_id != destination_payer)
        {
            return Err(TransferError::FoundingPayerConflict);
        }
        let destination_subscription_id = transfer
            .destination_provider_subscription_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or(TransferError::CorruptState)?;
        let award = founding_allocator::transfer_payer_with_kind(
            tx,
            award_id,
            source_kind,
            source_payer,
            destination_kind,
            destination_payer,
        )
        .await?;
        founding_schedule::enqueue(tx, &award, destination_subscription_id).await?;
    }
    sqlx::query(
        "UPDATE billing_transfer_intents SET state = 'completed', \
         source_adjustment_reference = $2, result_code = 'completed', updated_at = now() \
         WHERE transfer_id = $1 AND state = 'source_adjustment_pending' \
           AND NOT execution_manual_review_required",
    )
    .bind(transfer_id)
    .bind(source_adjustment_reference)
    .execute(&mut **tx)
    .await?;
    let mut notice_organizations = Vec::new();
    notice_organizations.extend(transfer.source_organization_id.clone());
    notice_organizations.extend(transfer.destination_organization_id.clone());
    crate::notifications::enqueue_payer_transfer_outcome_in_tx(
        tx,
        transfer_id,
        &transfer.beneficiary_id,
        &notice_organizations,
        transfer.effective_from,
    )
    .await
    .map_err(|error| match error {
        crate::notifications::NoticeError::Database(error) => TransferError::Database(error),
        _ => TransferError::InvalidField("transfer notice"),
    })?;
    load_transfer(tx, transfer_id).await
}

/// Store the provider schedule identity before recording the source adjustment as complete.
/// Retries may confirm the same schedule but cannot silently substitute another one.
pub async fn record_source_schedule(
    tx: &mut Transaction<'_, Postgres>,
    transfer_id: &str,
    schedule_id: &str,
) -> Result<TransferIntent, TransferError> {
    if schedule_id.trim().is_empty() {
        return Err(TransferError::InvalidField("source_schedule_id"));
    }
    let updated = sqlx::query(
        "UPDATE billing_transfer_intents SET source_schedule_id = $2, updated_at = now() \
         WHERE transfer_id = $1 AND state = 'source_adjustment_pending' \
           AND (source_schedule_id IS NULL OR source_schedule_id = $2) \
         RETURNING transfer_id",
    )
    .bind(transfer_id)
    .bind(schedule_id)
    .fetch_optional(&mut **tx)
    .await?;
    if updated.is_none() {
        let current = load_transfer(tx, transfer_id).await?;
        if !matches!(
            current.state,
            TransferState::SourceAdjustmentPending | TransferState::Completed
        ) || current.source_schedule_id.as_deref() != Some(schedule_id)
        {
            return Err(TransferError::ResultConflict);
        }
        return Ok(current);
    }
    load_transfer(tx, transfer_id).await
}

/// Start compensation after the destination's first payment definitively failed, but the source
/// schedule had already been adjusted. The transfer remains live until the caller has restored
/// the source subscription and records that provider-verified result with
/// `complete_source_restoration`.
pub async fn request_source_restoration(
    tx: &mut Transaction<'_, Postgres>,
    transfer_id: &str,
    failure_code: &str,
) -> Result<TransferIntent, TransferError> {
    if failure_code.trim().is_empty() {
        return Err(TransferError::InvalidField("failure_code"));
    }
    let transfer = load_transfer(tx, transfer_id).await?;
    if transfer.state == TransferState::SourceRestorationPending
        && transfer.result_code.as_deref() == Some(failure_code)
    {
        return Ok(transfer);
    }
    if transfer.state == TransferState::Failed
        && transfer.result_code.as_deref() == Some(failure_code)
        && transfer.source_restoration_reference.is_some()
    {
        return Ok(transfer);
    }
    if transfer.state != TransferState::SourceAdjustmentPending
        || transfer.source_adjustment_reference.is_none()
        || transfer.destination_payment_reference.is_some()
    {
        return Err(TransferError::InvalidTransition);
    }
    sqlx::query(
        "UPDATE billing_transfer_intents SET state = 'source_restoration_pending', \
         result_code = $2, updated_at = now() \
         WHERE transfer_id = $1 AND state = 'source_adjustment_pending'",
    )
    .bind(transfer_id)
    .bind(failure_code)
    .execute(&mut **tx)
    .await?;
    load_transfer(tx, transfer_id).await
}

/// Record provider-read-back evidence that the source subscription no longer ends at the
/// transfer boundary. Only then does the transfer become failed and release its live-beneficiary
/// fence. A provider object id or stable operation receipt may be used as the reference.
pub async fn complete_source_restoration(
    tx: &mut Transaction<'_, Postgres>,
    transfer_id: &str,
    restoration_reference: &str,
) -> Result<TransferIntent, TransferError> {
    if restoration_reference.trim().is_empty() {
        return Err(TransferError::InvalidField("source_restoration_reference"));
    }
    founding_allocator::lock_capacity(tx).await?;
    let transfer = load_transfer(tx, transfer_id).await?;
    if transfer.state == TransferState::Failed && transfer.source_restoration_reference.is_some() {
        return if transfer.source_restoration_reference.as_deref() == Some(restoration_reference) {
            Ok(transfer)
        } else {
            Err(TransferError::ResultConflict)
        };
    }
    if transfer.state != TransferState::SourceRestorationPending
        || transfer.source_adjustment_reference.is_none()
        || transfer.destination_payment_reference.is_some()
        || transfer.source_restoration_operation_id.is_none()
        || transfer
            .source_restoration_provider_subscription_id
            .is_none()
        || transfer.source_restoration_payment_reference.is_none()
    {
        return Err(TransferError::InvalidTransition);
    }
    if transfer
        .source_restoration_reference
        .as_deref()
        .is_some_and(|stored| stored != restoration_reference)
    {
        return Err(TransferError::ResultConflict);
    }
    sqlx::query(
        "UPDATE billing_transfer_intents SET state = 'failed', \
         source_restoration_reference = $2, updated_at = now() \
         WHERE transfer_id = $1 AND state = 'source_restoration_pending'",
    )
    .bind(transfer_id)
    .bind(restoration_reference)
    .execute(&mut **tx)
    .await?;
    load_transfer(tx, transfer_id).await
}

/// Persist the replacement subscription returned for an original personal payer. The stable
/// operation id is written before provider I/O; the subscription binding is added after Stripe
/// accepts the idempotent create request.
pub async fn record_source_restoration_subscription(
    tx: &mut Transaction<'_, Postgres>,
    transfer_id: &str,
    operation_id: &str,
    subscription_id: &str,
) -> Result<TransferIntent, TransferError> {
    if operation_id.trim().is_empty() || subscription_id.trim().is_empty() {
        return Err(TransferError::InvalidField(
            "source_restoration_subscription",
        ));
    }
    let transfer = load_transfer(tx, transfer_id).await?;
    if transfer.state != TransferState::SourceRestorationPending {
        return Err(TransferError::InvalidTransition);
    }
    if transfer
        .source_restoration_operation_id
        .as_deref()
        .is_some_and(|stored| stored != operation_id)
        || transfer
            .source_restoration_provider_subscription_id
            .as_deref()
            .is_some_and(|stored| stored != subscription_id)
    {
        return Err(TransferError::ResultConflict);
    }
    sqlx::query(
        "UPDATE billing_transfer_intents SET source_restoration_operation_id = $2, \
         source_restoration_provider_subscription_id = $3, updated_at = now() \
         WHERE transfer_id = $1 AND state = 'source_restoration_pending'",
    )
    .bind(transfer_id)
    .bind(operation_id)
    .bind(subscription_id)
    .execute(&mut **tx)
    .await?;
    load_transfer(tx, transfer_id).await
}

/// Persist the idempotent recovery operation before creating a replacement subscription.
pub async fn begin_source_restoration_payment(
    tx: &mut Transaction<'_, Postgres>,
    transfer_id: &str,
    operation_id: &str,
) -> Result<TransferIntent, TransferError> {
    if operation_id.trim().is_empty() {
        return Err(TransferError::InvalidField(
            "source_restoration_operation_id",
        ));
    }
    let transfer = load_transfer(tx, transfer_id).await?;
    if transfer.state != TransferState::SourceRestorationPending {
        return Err(TransferError::InvalidTransition);
    }
    if transfer
        .source_restoration_operation_id
        .as_deref()
        .is_some_and(|stored| stored != operation_id)
    {
        return Err(TransferError::ResultConflict);
    }
    sqlx::query(
        "UPDATE billing_transfer_intents SET source_restoration_operation_id = $2, \
         updated_at = now() WHERE transfer_id = $1 AND state = 'source_restoration_pending'",
    )
    .bind(transfer_id)
    .bind(operation_id)
    .execute(&mut **tx)
    .await?;
    load_transfer(tx, transfer_id).await
}

/// Record an authenticated, paid recovery invoice. Merely creating or activating a subscription
/// never releases the transfer fence; the worker must still read the replacement back from Stripe.
pub async fn record_source_restoration_payment(
    tx: &mut Transaction<'_, Postgres>,
    transfer_id: &str,
    operation_id: &str,
    subscription_id: &str,
    payment_reference: &str,
) -> Result<TransferIntent, TransferError> {
    if operation_id.trim().is_empty()
        || subscription_id.trim().is_empty()
        || payment_reference.trim().is_empty()
    {
        return Err(TransferError::InvalidField("source_restoration_payment"));
    }
    let transfer = load_transfer(tx, transfer_id).await?;
    if transfer.state != TransferState::SourceRestorationPending
        || transfer.source_restoration_operation_id.as_deref() != Some(operation_id)
        || transfer
            .source_restoration_provider_subscription_id
            .as_deref()
            != Some(subscription_id)
    {
        return Err(TransferError::ResultConflict);
    }
    if transfer
        .source_restoration_payment_reference
        .as_deref()
        .is_some_and(|stored| stored != payment_reference)
    {
        return Err(TransferError::ResultConflict);
    }
    sqlx::query(
        "UPDATE billing_transfer_intents SET source_restoration_payment_reference = $2, \
         updated_at = now() WHERE transfer_id = $1 AND state = 'source_restoration_pending'",
    )
    .bind(transfer_id)
    .bind(payment_reference)
    .execute(&mut **tx)
    .await?;
    load_transfer(tx, transfer_id).await
}

pub async fn mark_failed(
    tx: &mut Transaction<'_, Postgres>,
    transfer_id: &str,
    result_code: &str,
) -> Result<TransferIntent, TransferError> {
    if result_code.trim().is_empty() {
        return Err(TransferError::InvalidField("result_code"));
    }
    founding_allocator::lock_capacity(tx).await?;
    let updated = sqlx::query(
        "UPDATE billing_transfer_intents SET state = 'failed', result_code = $2, \
         execution_manual_review_required = FALSE, execution_lease_token = NULL, \
         execution_lease_until = NULL, execution_next_attempt_at = 'infinity'::timestamptz, \
         updated_at = now() \
         WHERE transfer_id = $1 AND state IN ('awaiting_consent','pending','destination_prepared','unknown')",
    )
    .bind(transfer_id)
    .bind(result_code)
    .execute(&mut **tx)
    .await?;
    let current = load_transfer(tx, transfer_id).await?;
    if updated.rows_affected() == 0 {
        if current.state == TransferState::Failed
            && current.result_code.as_deref() == Some(result_code)
        {
            return Ok(current);
        }
        return Err(TransferError::InvalidTransition);
    }
    Ok(current)
}

/// Claim one accepted transfer for provider reconciliation. The query takes a bounded number
/// of rows (one here), skips work owned by another live lease, and returns expired leases to
/// the same stable provider operation keys after a process restart.
pub async fn claim_due(pool: &PgPool) -> Result<Option<TransferLease>, TransferError> {
    claim_due_matching(pool, None).await
}

/// Claim a known transfer after an operator or callback has requested immediate reconciliation.
/// It obeys the same lease and due-time checks as the ordinary worker poll.
pub async fn claim_transfer_for_execution(
    pool: &PgPool,
    transfer_id: &str,
) -> Result<Option<TransferLease>, TransferError> {
    if transfer_id.trim().is_empty() {
        return Err(TransferError::InvalidField("transfer_id"));
    }
    claim_due_matching(pool, Some(transfer_id)).await
}

async fn claim_due_matching(
    pool: &PgPool,
    only_transfer_id: Option<&str>,
) -> Result<Option<TransferLease>, TransferError> {
    let lease_token = Uuid::new_v4().to_string();
    let mut tx = pool.begin().await?;
    let row = sqlx::query(
        "WITH candidate AS (\
             SELECT transfer.transfer_id FROM billing_transfer_intents AS transfer \
             WHERE transfer.state IN ('pending','destination_prepared','destination_paid', \
                             'source_adjustment_pending','source_restoration_pending','unknown') \
               AND (transfer.destination_kind <> 'personal' \
                    OR transfer.destination_setup_intent_id IS NOT NULL) \
               AND transfer.execution_next_attempt_at <= now() \
               AND NOT transfer.execution_manual_review_required \
               AND (transfer.execution_lease_until IS NULL OR transfer.execution_lease_until <= now()) \
               AND ($3::TEXT IS NULL OR transfer.transfer_id = $3) \
             ORDER BY transfer.execution_next_attempt_at, transfer.created_at, transfer.transfer_id \
             LIMIT 1 FOR UPDATE OF transfer SKIP LOCKED \
         ) \
         UPDATE billing_transfer_intents AS transfer SET \
             execution_lease_token = $2, \
             execution_lease_until = now() + ($1 * interval '1 second'), \
             updated_at = now() \
         FROM candidate WHERE transfer.transfer_id = candidate.transfer_id \
         RETURNING transfer.*, EXTRACT(EPOCH FROM transfer.execution_lease_until)::BIGINT \
             AS execution_lease_until_epoch",
    )
    .bind(TRANSFER_EXECUTION_LEASE_SECONDS)
    .bind(&lease_token)
    .bind(only_transfer_id)
    .fetch_optional(&mut *tx)
    .await?;
    let lease = row
        .map(|row| {
            Ok::<TransferLease, TransferError>(TransferLease {
                transfer: transfer_from_row(&row)?,
                lease_token,
                lease_until_epoch: row.try_get("execution_lease_until_epoch")?,
            })
        })
        .transpose()?;
    tx.commit().await?;
    Ok(lease)
}

/// Return a bounded, stable page of transfers awaiting an operator decision. The cursor is the
/// last transfer id, not a client-selected sort expression.
pub async fn list_manual_review_transfers(
    pool: &PgPool,
    after_transfer_id: Option<&str>,
    limit: i64,
) -> Result<Vec<TransferIntent>, TransferError> {
    if !(1..=51).contains(&limit)
        || after_transfer_id.is_some_and(|cursor| cursor.trim().is_empty() || cursor.len() > 256)
    {
        return Err(TransferError::InvalidField("manual review page"));
    }
    let rows = sqlx::query(
        "SELECT *, EXTRACT(EPOCH FROM execution_lease_until)::BIGINT AS execution_lease_until_epoch \
         FROM billing_transfer_intents \
         WHERE execution_manual_review_required \
           AND state IN ('destination_prepared','source_adjustment_pending','unknown') \
           AND ($1::TEXT IS NULL OR transfer_id > $1) \
         ORDER BY transfer_id ASC LIMIT $2",
    )
    .bind(after_transfer_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    rows.iter().map(transfer_from_row).collect()
}

/// Load one parked transfer and its bounded audit history for a protected operator view.
pub async fn load_manual_review_transfer(
    pool: &PgPool,
    transfer_id: &str,
) -> Result<Option<(TransferIntent, Vec<TransferReviewDecision>)>, TransferError> {
    let row = sqlx::query(
        "SELECT *, EXTRACT(EPOCH FROM execution_lease_until)::BIGINT AS execution_lease_until_epoch \
         FROM billing_transfer_intents WHERE transfer_id = $1 \
           AND execution_manual_review_required \
           AND state IN ('destination_prepared','source_adjustment_pending','unknown')",
    )
    .bind(transfer_id)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let transfer = transfer_from_row(&row)?;
    if !transfer.execution_manual_review_required {
        return Ok(None);
    }
    let rows = sqlx::query(
        "SELECT decision_id, operator_label, action, reason_code, review_generation, \
                idempotency_key, EXTRACT(EPOCH FROM created_at)::BIGINT AS created_at_epoch \
         FROM billing_transfer_review_decisions WHERE transfer_id = $1 \
         ORDER BY decision_id DESC LIMIT 50",
    )
    .bind(transfer_id)
    .fetch_all(pool)
    .await?;
    let decisions = rows
        .iter()
        .map(transfer_review_decision_from_row)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some((transfer, decisions)))
}

/// Persist a protected operator hold or resume decision. Resume only makes the intent eligible for
/// the ordinary worker; it does not record provider success or perform a provider operation.
pub async fn record_operator_review_decision(
    pool: &PgPool,
    transfer_id: &str,
    operator_label: &str,
    action: TransferReviewAction,
    reason_code: &str,
    review_generation: i64,
    idempotency_key: &str,
) -> Result<(TransferIntent, TransferReviewDecision), TransferError> {
    if transfer_id.trim().is_empty() || transfer_id.len() > 256 {
        return Err(TransferError::InvalidField("transfer_id"));
    }
    if !valid_operator_label(operator_label) {
        return Err(TransferError::InvalidField("operator"));
    }
    if !valid_review_reason(reason_code) {
        return Err(TransferError::InvalidField("review reason"));
    }
    if review_generation < 0 {
        return Err(TransferError::InvalidField("review generation"));
    }
    if idempotency_key.trim().is_empty()
        || idempotency_key.len() > 128
        || idempotency_key.chars().any(char::is_control)
    {
        return Err(TransferError::InvalidField("review idempotency key"));
    }

    let mut tx = pool.begin().await?;
    founding_allocator::lock_capacity(&mut tx).await?;
    let row = sqlx::query(
        "SELECT *, EXTRACT(EPOCH FROM execution_lease_until)::BIGINT AS execution_lease_until_epoch, \
                (execution_lease_until IS NOT NULL AND execution_lease_until > now()) AS active_lease \
         FROM billing_transfer_intents WHERE transfer_id = $1 FOR UPDATE",
    )
    .bind(transfer_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(TransferError::MissingTransfer)?;
    let current = transfer_from_row(&row)?;

    if let Some(existing) = sqlx::query(
        "SELECT decision_id, operator_label, action, reason_code, review_generation, \
                idempotency_key, EXTRACT(EPOCH FROM created_at)::BIGINT AS created_at_epoch \
         FROM billing_transfer_review_decisions \
         WHERE transfer_id = $1 AND idempotency_key = $2",
    )
    .bind(transfer_id)
    .bind(idempotency_key)
    .fetch_optional(&mut *tx)
    .await?
    {
        let decision = transfer_review_decision_from_row(&existing)?;
        if decision.operator_label != operator_label.trim()
            || decision.action != action
            || decision.reason_code != reason_code
            || decision.review_generation != review_generation
        {
            return Err(TransferError::ResultConflict);
        }
        tx.commit().await?;
        return Ok((current, decision));
    }

    let active_lease: bool = row.try_get("active_lease")?;
    if !current.execution_manual_review_required
        || !matches!(
            current.state,
            TransferState::DestinationPrepared
                | TransferState::SourceAdjustmentPending
                | TransferState::Unknown
        )
        || current.execution_review_generation != review_generation
        || active_lease
    {
        return Err(TransferError::InvalidTransition);
    }

    let decision_row = sqlx::query(
        "INSERT INTO billing_transfer_review_decisions \
         (transfer_id, operator_label, action, reason_code, review_generation, idempotency_key) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         RETURNING decision_id, operator_label, action, reason_code, review_generation, \
                   idempotency_key, EXTRACT(EPOCH FROM created_at)::BIGINT AS created_at_epoch",
    )
    .bind(transfer_id)
    .bind(operator_label.trim())
    .bind(action.as_str())
    .bind(reason_code)
    .bind(review_generation)
    .bind(idempotency_key)
    .fetch_one(&mut *tx)
    .await?;
    let decision = transfer_review_decision_from_row(&decision_row)?;

    if action == TransferReviewAction::Resume {
        let updated = sqlx::query(
            "UPDATE billing_transfer_intents SET \
                 execution_manual_review_required = FALSE, execution_next_attempt_at = now(), \
                 execution_attempt_count = 0, result_code = NULL, updated_at = now() \
             WHERE transfer_id = $1 AND execution_manual_review_required \
               AND execution_review_generation = $2 \
               AND state IN ('destination_prepared','source_adjustment_pending','unknown') \
               AND (execution_lease_until IS NULL OR execution_lease_until <= now()) \
             RETURNING transfer_id",
        )
        .bind(transfer_id)
        .bind(review_generation)
        .fetch_optional(&mut *tx)
        .await?;
        if updated.is_none() {
            return Err(TransferError::InvalidTransition);
        }
    }

    let transfer = load_transfer(&mut tx, transfer_id).await?;
    tx.commit().await?;
    Ok((transfer, decision))
}

fn valid_review_reason(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.as_bytes()[0].is_ascii_lowercase()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn valid_operator_label(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 128 && !value.chars().any(char::is_control)
}

fn transfer_review_decision_from_row(
    row: &sqlx::postgres::PgRow,
) -> Result<TransferReviewDecision, TransferError> {
    Ok(TransferReviewDecision {
        decision_id: row.try_get("decision_id")?,
        operator_label: row.try_get("operator_label")?,
        action: TransferReviewAction::parse(&row.try_get::<String, _>("action")?)?,
        reason_code: row.try_get("reason_code")?,
        review_generation: row.try_get("review_generation")?,
        idempotency_key: row.try_get("idempotency_key")?,
        created_at_epoch: row.try_get("created_at_epoch")?,
    })
}

/// Release a worker lease after a retryable or ambiguous result. A provider timeout is recorded
/// as unknown but retains the same intent and stable idempotency identity for the next retrieval.
pub async fn release_execution_lease(
    pool: &PgPool,
    transfer_id: &str,
    lease_token: &str,
    failure: TransferExecutionFailure,
) -> Result<bool, TransferError> {
    if transfer_id.trim().is_empty() || lease_token.trim().is_empty() {
        return Err(TransferError::InvalidField("execution lease"));
    }
    let mut tx = pool.begin().await?;
    let prior = sqlx::query_as::<_, (i32, bool, Option<String>, Option<String>)>(
        "SELECT execution_attempt_count, execution_manual_review_required, \
                last_execution_error_code, result_code FROM billing_transfer_intents \
         WHERE transfer_id = $1 AND execution_lease_token = $2 FOR UPDATE",
    )
    .bind(transfer_id)
    .bind(lease_token)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((previous_failures, already_requires_review, previous_error, previous_result)) = prior
    else {
        tx.rollback().await?;
        return Ok(false);
    };
    let failure_count = previous_failures.saturating_add(1);
    let manual_review_required =
        already_requires_review || failure.requires_manual_review(failure_count);
    let unknown_state_required = failure.needs_unknown_state() || manual_review_required;
    let error_code = if already_requires_review {
        previous_error.unwrap_or_else(|| failure.as_str().to_owned())
    } else {
        failure.as_str().to_owned()
    };
    let result_code: String = if already_requires_review {
        previous_result.unwrap_or_else(|| "manual_review_required".to_owned())
    } else if manual_review_required && failure != TransferExecutionFailure::ProviderRejected {
        "automatic_retry_limit_reached".to_owned()
    } else {
        failure.as_str().to_owned()
    };
    let row = sqlx::query(
        "UPDATE billing_transfer_intents SET \
             state = CASE WHEN $3 THEN 'unknown' ELSE state END, \
             last_execution_error_code = $4, \
             result_code = CASE WHEN $3 THEN $5 ELSE result_code END, \
             execution_attempt_count = $8, \
             execution_manual_review_required = $6, \
             execution_review_generation = execution_review_generation + \
                 CASE WHEN $6 AND NOT execution_manual_review_required THEN 1 ELSE 0 END, \
             execution_lease_token = NULL, execution_lease_until = NULL, \
             execution_next_attempt_at = CASE WHEN $6 THEN 'infinity'::timestamptz \
                 ELSE now() + ($7 * interval '1 second') END, updated_at = now() \
         WHERE transfer_id = $1 AND execution_lease_token = $2 \
         RETURNING transfer_id",
    )
    .bind(transfer_id)
    .bind(lease_token)
    .bind(unknown_state_required)
    .bind(error_code)
    .bind(result_code)
    .bind(manual_review_required)
    .bind(transfer_retry_delay_seconds(failure, failure_count))
    .bind(failure_count)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(row.is_some())
}

/// Restore the review fence when a resumed transfer still carries a mismatched destination invoice.
/// This rechecks the row under lock and consumes only the current worker lease.
pub async fn repark_mismatched_destination_invoice(
    pool: &PgPool,
    transfer_id: &str,
    lease_token: &str,
) -> Result<bool, TransferError> {
    if transfer_id.trim().is_empty() || lease_token.trim().is_empty() {
        return Err(TransferError::InvalidField("execution lease"));
    }
    let mut tx = pool.begin().await?;
    founding_allocator::lock_capacity(&mut tx).await?;
    let row = sqlx::query(
        "SELECT *, EXTRACT(EPOCH FROM execution_lease_until)::BIGINT AS execution_lease_until_epoch \
         FROM billing_transfer_intents WHERE transfer_id = $1 FOR UPDATE",
    )
    .bind(transfer_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        tx.rollback().await?;
        return Ok(false);
    };
    let current = transfer_from_row(&row)?;
    if current.execution_lease_token.as_deref() != Some(lease_token)
        || current.execution_manual_review_required
    {
        tx.rollback().await?;
        return Ok(false);
    }
    let Some(reason) = destination_period_mismatch_code(&current) else {
        tx.rollback().await?;
        return Ok(false);
    };
    let rows = sqlx::query(
        "UPDATE billing_transfer_intents SET \
             execution_manual_review_required = TRUE, \
             execution_review_generation = execution_review_generation + 1, \
             last_execution_error_code = $3, result_code = 'manual_review_required', \
             execution_next_attempt_at = 'infinity'::timestamptz, execution_attempt_count = 0, \
             execution_lease_token = NULL, execution_lease_until = NULL, updated_at = now() \
         WHERE transfer_id = $1 AND execution_lease_token = $2 \
           AND NOT execution_manual_review_required",
    )
    .bind(transfer_id)
    .bind(lease_token)
    .bind(reason)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok(rows == 1)
}

fn transfer_retry_delay_seconds(failure: TransferExecutionFailure, attempts: i32) -> i64 {
    if matches!(failure, TransferExecutionFailure::LocalEvidenceChanged) {
        return MAX_TRANSFER_RETRY_DELAY_SECONDS;
    }
    let exponent = attempts.clamp(0, 12) as u32;
    15_i64
        .saturating_mul(1_i64 << exponent)
        .min(MAX_TRANSFER_RETRY_DELAY_SECONDS)
}

/// Remove a lease after the worker has durably advanced the transfer. The token check prevents
/// an expired worker from clearing a newer worker's lease.
pub async fn finish_execution_lease(
    pool: &PgPool,
    transfer_id: &str,
    lease_token: &str,
) -> Result<bool, TransferError> {
    let rows = sqlx::query(
        "UPDATE billing_transfer_intents SET execution_lease_token = NULL, \
             execution_lease_until = NULL, \
             execution_next_attempt_at = CASE WHEN execution_manual_review_required \
                 THEN 'infinity'::timestamptz ELSE now() END, \
             execution_attempt_count = CASE WHEN execution_manual_review_required \
                 THEN execution_attempt_count ELSE 0 END, \
             result_code = CASE WHEN execution_manual_review_required \
                 THEN COALESCE(result_code, 'manual_review_required') ELSE NULL END, \
             last_execution_error_code = CASE WHEN execution_manual_review_required \
                 THEN last_execution_error_code ELSE NULL END, updated_at = now() \
         WHERE transfer_id = $1 AND execution_lease_token = $2",
    )
    .bind(transfer_id)
    .bind(lease_token)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(rows == 1)
}

/// Yield a lease while a valid provider transition is waiting for an external event, such as the
/// destination's first paid invoice. The transfer state and its recovery evidence remain intact.
pub async fn defer_execution_lease(
    pool: &PgPool,
    transfer_id: &str,
    lease_token: &str,
    delay_seconds: i64,
) -> Result<bool, TransferError> {
    if transfer_id.trim().is_empty() || lease_token.trim().is_empty() || delay_seconds <= 0 {
        return Err(TransferError::InvalidField("execution lease deferral"));
    }
    let rows = sqlx::query(
        "UPDATE billing_transfer_intents SET execution_lease_token = NULL, \
             execution_lease_until = NULL, \
             execution_attempt_count = CASE WHEN execution_manual_review_required \
                 THEN execution_attempt_count ELSE 0 END, \
             execution_next_attempt_at = CASE WHEN execution_manual_review_required \
                 THEN 'infinity'::timestamptz \
                 ELSE now() + ($3 * interval '1 second') END, updated_at = now() \
         WHERE transfer_id = $1 AND execution_lease_token = $2",
    )
    .bind(transfer_id)
    .bind(lease_token)
    .bind(delay_seconds)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(rows == 1)
}

pub async fn load_for_actor(
    pool: &PgPool,
    transfer_id: &str,
    actor_user_id: &str,
) -> Result<Option<TransferIntent>, TransferError> {
    let row = sqlx::query(
        "SELECT *, EXTRACT(EPOCH FROM execution_lease_until)::BIGINT AS execution_lease_until_epoch \
         FROM billing_transfer_intents WHERE transfer_id = $1 AND actor_user_id = $2",
    )
    .bind(transfer_id)
    .bind(actor_user_id)
    .fetch_optional(pool)
    .await?;
    row.map(|row| transfer_from_row(&row)).transpose()
}

pub async fn load_for_participant(
    pool: &PgPool,
    transfer_id: &str,
    user_id: &str,
) -> Result<Option<TransferIntent>, TransferError> {
    let row = sqlx::query(
        "SELECT *, EXTRACT(EPOCH FROM execution_lease_until)::BIGINT AS execution_lease_until_epoch \
         FROM billing_transfer_intents WHERE transfer_id = $1 \
         AND (actor_user_id = $2 OR counterparty_user_id = $2)",
    )
    .bind(transfer_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    row.map(|row| transfer_from_row(&row)).transpose()
}

pub async fn find_by_idempotency(
    tx: &mut Transaction<'_, Postgres>,
    actor_user_id: &str,
    idempotency_key: &str,
) -> Result<Option<TransferIntent>, TransferError> {
    load_by_idempotency(tx, actor_user_id, idempotency_key).await
}

pub async fn list_for_participant(
    pool: &PgPool,
    user_id: &str,
    before_transfer_id: Option<&str>,
    limit: i64,
) -> Result<Vec<TransferIntent>, TransferError> {
    let rows = sqlx::query(
        "SELECT *, EXTRACT(EPOCH FROM execution_lease_until)::BIGINT AS execution_lease_until_epoch \
         FROM billing_transfer_intents \
         WHERE (actor_user_id = $1 OR counterparty_user_id = $1) \
           AND ($2::text IS NULL OR transfer_id < $2) \
         ORDER BY transfer_id DESC LIMIT $3",
    )
    .bind(user_id)
    .bind(before_transfer_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    rows.iter().map(transfer_from_row).collect()
}

async fn ensure_source_active(
    tx: &mut Transaction<'_, Postgres>,
    request: &TransferRequest,
    now_epoch: i64,
) -> Result<String, TransferError> {
    let source = match request.source_kind {
        TransferPayer::Personal => {
            sqlx::query_as::<_, (String, Option<i64>, String)>(
                "SELECT offer, paid_through_epoch, stripe_subscription_id \
                 FROM billing_personal_accounts \
                 WHERE user_id = $1 AND state = 'active' AND cancel_at_period_end = FALSE \
                   AND cancellation_requested_at IS NULL AND stripe_subscription_id IS NOT NULL \
                 FOR UPDATE",
            )
            .bind(&request.beneficiary_id)
            .fetch_optional(&mut **tx)
            .await?
        }
        TransferPayer::Sponsor => {
            let organization_id = request
                .source_organization_id
                .as_deref()
                .ok_or(TransferError::CorruptState)?;
            sqlx::query_as::<_, (String, Option<i64>, String)>(
                "SELECT seat.offer, subscription.current_period_end, \
                        subscription.provider_subscription_id \
                 FROM billing_sponsored_seats AS seat \
                 JOIN billing_sponsored_subscriptions AS subscription \
                   ON subscription.organization_id = seat.organization_id \
                 WHERE seat.organization_id = $1 AND seat.beneficiary_id = $2 \
                   AND seat.state = 'active' AND subscription.status = 'active' \
                   AND subscription.provider_subscription_id IS NOT NULL \
                   AND subscription.current_period_end IS NOT NULL \
                   AND seat.effective_from <= $3 \
                   AND (seat.effective_until IS NULL OR \
                        seat.effective_until >= subscription.current_period_end) \
                 FOR UPDATE OF seat, subscription",
            )
            .bind(organization_id)
            .bind(&request.beneficiary_id)
            .bind(now_epoch)
            .fetch_optional(&mut **tx)
            .await?
        }
    };
    validate_source_term(source, request, now_epoch)
}

fn validate_source_term(
    source: Option<(String, Option<i64>, String)>,
    request: &TransferRequest,
    now_epoch: i64,
) -> Result<String, TransferError> {
    let Some((offer, paid_through, subscription_id)) = source else {
        return Err(TransferError::SourceInactive);
    };
    if subscription_id != request.source_provider_subscription_id {
        return Err(TransferError::SourceTermChanged);
    }
    if paid_through
        .is_none_or(|boundary| boundary <= now_epoch || boundary != request.effective_from)
    {
        return Err(TransferError::SourceTermChanged);
    }
    Ok(offer)
}

fn validate_offer_at_boundary(
    request: &TransferRequest,
    source_offer: &str,
    award: Option<&founding_allocator::FoundingAward>,
) -> Result<(), TransferError> {
    let expected = match award {
        Some(award) => {
            let founding_end = award
                .original_end_epoch
                .ok_or(TransferError::CorruptState)?;
            let founding_offer = award.offer.billing_offer();
            let expected_source = if request.effective_from <= founding_end {
                founding_offer
            } else {
                award.offer.standard_billing_offer()
            };
            if parse_offer(source_offer)? != expected_source {
                return Err(TransferError::SourceTermChanged);
            }
            if request.effective_from < founding_end {
                founding_offer
            } else {
                award.offer.standard_billing_offer()
            }
        }
        None => parse_offer(source_offer)?,
    };
    if expected != request.offer {
        return Err(TransferError::SourceTermChanged);
    }
    Ok(())
}

async fn ensure_destination_free(
    tx: &mut Transaction<'_, Postgres>,
    request: &TransferRequest,
) -> Result<(), TransferError> {
    let covered = match request.destination_kind {
        TransferPayer::Personal => {
            sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM billing_personal_accounts \
             WHERE user_id = $1 AND state IN ('pending','active','past_due','refund_required'))",
            )
            .bind(&request.beneficiary_id)
            .fetch_one(&mut **tx)
            .await?
        }
        TransferPayer::Sponsor => {
            let organization_id = request
                .destination_organization_id
                .as_deref()
                .ok_or(TransferError::CorruptState)?;
            let covered = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM billing_sponsored_seats \
                 WHERE organization_id = $1 AND beneficiary_id = $2 \
                   AND state IN ('pending','active','scheduled_removal'))",
            )
            .bind(organization_id)
            .bind(&request.beneficiary_id)
            .fetch_one(&mut **tx)
            .await?;
            if covered {
                return Err(TransferError::DestinationAlreadyCovers);
            }
            let destination_boundary: Option<i64> = sqlx::query_scalar(
                "SELECT current_period_end FROM billing_sponsored_subscriptions \
                 WHERE organization_id = $1 AND status = 'active' \
                   AND provider_subscription_id IS NOT NULL AND current_period_end IS NOT NULL \
                 FOR UPDATE",
            )
            .bind(organization_id)
            .fetch_optional(&mut **tx)
            .await?
            .flatten();
            let Some(destination_boundary) = destination_boundary else {
                return Err(TransferError::DestinationSubscriptionUnavailable);
            };
            if destination_boundary != request.effective_from {
                return Err(TransferError::DestinationTermChanged);
            }
            false
        }
    };
    (!covered)
        .then_some(())
        .ok_or(TransferError::DestinationAlreadyCovers)
}

async fn load_by_idempotency(
    tx: &mut Transaction<'_, Postgres>,
    actor_user_id: &str,
    idempotency_key: &str,
) -> Result<Option<TransferIntent>, TransferError> {
    let row = sqlx::query(
        "SELECT *, EXTRACT(EPOCH FROM execution_lease_until)::BIGINT AS execution_lease_until_epoch \
         FROM billing_transfer_intents WHERE actor_user_id = $1 AND idempotency_key = $2 FOR UPDATE",
    )
    .bind(actor_user_id)
    .bind(idempotency_key)
    .fetch_optional(&mut **tx)
    .await?;
    row.map(|row| transfer_from_row(&row)).transpose()
}

async fn load_transfer(
    tx: &mut Transaction<'_, Postgres>,
    transfer_id: &str,
) -> Result<TransferIntent, TransferError> {
    let row =
        sqlx::query(
            "SELECT *, EXTRACT(EPOCH FROM execution_lease_until)::BIGINT AS execution_lease_until_epoch \
             FROM billing_transfer_intents WHERE transfer_id = $1 FOR UPDATE",
        )
            .bind(transfer_id)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(TransferError::MissingTransfer)?;
    transfer_from_row(&row)
}

async fn load_transfer_snapshot(
    tx: &mut Transaction<'_, Postgres>,
    transfer_id: &str,
) -> Result<TransferIntent, TransferError> {
    let row = sqlx::query(
        "SELECT *, EXTRACT(EPOCH FROM execution_lease_until)::BIGINT AS execution_lease_until_epoch \
         FROM billing_transfer_intents WHERE transfer_id = $1",
    )
        .bind(transfer_id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(TransferError::MissingTransfer)?;
    transfer_from_row(&row)
}

pub(crate) fn transfer_from_row(
    row: &sqlx::postgres::PgRow,
) -> Result<TransferIntent, TransferError> {
    Ok(TransferIntent {
        transfer_id: row.try_get("transfer_id")?,
        actor_user_id: row.try_get("actor_user_id")?,
        counterparty_user_id: row.try_get("counterparty_user_id")?,
        beneficiary_id: row.try_get("beneficiary_id")?,
        source_kind: parse_payer(&row.try_get::<String, _>("source_kind")?)?,
        source_organization_id: row.try_get("source_organization_id")?,
        destination_kind: parse_payer(&row.try_get::<String, _>("destination_kind")?)?,
        destination_organization_id: row.try_get("destination_organization_id")?,
        offer: parse_offer(&row.try_get::<String, _>("offer")?)?,
        quote_version: row.try_get("quote_version")?,
        quote_expires_at_epoch: row.try_get("quote_expires_at_epoch")?,
        consent_expires_at_epoch: row.try_get("consent_expires_at_epoch")?,
        request_hash: row.try_get("request_hash")?,
        idempotency_key: row.try_get("idempotency_key")?,
        provider_idempotency_key: row.try_get("provider_idempotency_key")?,
        effective_from: row.try_get("effective_from")?,
        effective_until: row.try_get("effective_until")?,
        destination_operation_id: row.try_get("destination_operation_id")?,
        source_operation_id: row.try_get("source_operation_id")?,
        destination_provider_subscription_id: row
            .try_get("destination_provider_subscription_id")?,
        source_provider_subscription_id: row.try_get("source_provider_subscription_id")?,
        destination_payment_reference: row.try_get("destination_payment_reference")?,
        destination_period_start: row.try_get("destination_period_start")?,
        destination_period_end: row.try_get("destination_period_end")?,
        destination_expected_period_end: row.try_get("destination_expected_period_end")?,
        source_adjustment_reference: row.try_get("source_adjustment_reference")?,
        source_restoration_reference: row.try_get("source_restoration_reference")?,
        source_restoration_operation_id: row.try_get("source_restoration_operation_id")?,
        source_restoration_provider_subscription_id: row
            .try_get("source_restoration_provider_subscription_id")?,
        source_restoration_payment_reference: row
            .try_get("source_restoration_payment_reference")?,
        founding_award_id: row.try_get("founding_award_id")?,
        execution_lease_token: row.try_get("execution_lease_token")?,
        execution_lease_until_epoch: row
            .try_get::<Option<i64>, _>("execution_lease_until_epoch")?,
        execution_attempt_count: row.try_get("execution_attempt_count")?,
        execution_manual_review_required: row.try_get("execution_manual_review_required")?,
        execution_review_generation: row.try_get("execution_review_generation")?,
        destination_customer_id: row.try_get("destination_customer_id")?,
        destination_schedule_id: row.try_get("destination_schedule_id")?,
        source_schedule_id: row.try_get("source_schedule_id")?,
        destination_setup_session_id: row.try_get("destination_setup_session_id")?,
        destination_setup_intent_id: row.try_get("destination_setup_intent_id")?,
        last_execution_error_code: row.try_get("last_execution_error_code")?,
        state: TransferState::parse(&row.try_get::<String, _>("state")?)?,
        result_code: row.try_get("result_code")?,
    })
}

fn parse_payer(value: &str) -> Result<TransferPayer, TransferError> {
    match value {
        "personal" => Ok(TransferPayer::Personal),
        "sponsor" => Ok(TransferPayer::Sponsor),
        _ => Err(TransferError::CorruptState),
    }
}

fn parse_offer(value: &str) -> Result<BillingOffer, TransferError> {
    BillingOffer::ALL
        .into_iter()
        .find(|offer| offer.as_str() == value)
        .ok_or(TransferError::CorruptState)
}

fn payer_authorised(
    payer: TransferPayer,
    organization_id: Option<&str>,
    beneficiary_id: &str,
    user_id: &str,
    organization_authority: &std::collections::BTreeMap<(&str, &str), bool>,
) -> bool {
    match payer {
        TransferPayer::Personal => user_id == beneficiary_id,
        TransferPayer::Sponsor => organization_id
            .and_then(|organization_id| organization_authority.get(&(organization_id, user_id)))
            .copied()
            .unwrap_or(false),
    }
}

fn payer_identity(
    request: &TransferRequest,
    payer: TransferPayer,
) -> Result<(founding_allocator::FoundingPayerKind, &str), TransferError> {
    match payer {
        TransferPayer::Personal => Ok((
            founding_allocator::FoundingPayerKind::Personal,
            &request.beneficiary_id,
        )),
        TransferPayer::Sponsor => Ok((
            founding_allocator::FoundingPayerKind::Sponsor,
            request
                .source_organization_id
                .as_deref()
                .ok_or(TransferError::CorruptState)?,
        )),
    }
}

fn payer_identity_for_transfer<'a>(
    payer: TransferPayer,
    beneficiary_id: &'a str,
    organization_id: Option<&'a str>,
) -> Result<(founding_allocator::FoundingPayerKind, &'a str), TransferError> {
    match payer {
        TransferPayer::Personal => Ok((
            founding_allocator::FoundingPayerKind::Personal,
            beneficiary_id,
        )),
        TransferPayer::Sponsor => Ok((
            founding_allocator::FoundingPayerKind::Sponsor,
            organization_id.ok_or(TransferError::CorruptState)?,
        )),
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::{
        expected_transfer_period_end, offer_at_transfer_boundary, transfer_retry_delay_seconds,
        validate_offer_at_boundary, validate_source_term, TransferError, TransferExecutionFailure,
        TransferPayer, TransferRequest, TransferState, MAX_AUTOMATIC_TRANSFER_ATTEMPTS,
        MAX_TRANSFER_RETRY_DELAY_SECONDS,
    };
    use crate::billing_catalogue::BillingOffer;
    use crate::founding_allocator::FoundingDate;

    fn request() -> TransferRequest {
        TransferRequest {
            actor_user_id: "user".into(),
            counterparty_user_id: "user".into(),
            beneficiary_id: "user".into(),
            source_kind: TransferPayer::Personal,
            source_organization_id: None,
            source_provider_subscription_id: "sub_source".into(),
            destination_kind: TransferPayer::Sponsor,
            destination_organization_id: Some("org".into()),
            offer: BillingOffer::StandardMonthly,
            quote_version: 1,
            quote_expires_at_epoch: 2_000,
            effective_from: 1_500,
            effective_until: None,
            idempotency_key: "key".into(),
        }
    }

    #[test]
    fn transfer_requires_a_distinct_well_shaped_payer_boundary() {
        assert!(request().validate(1_000).is_ok());
        let mut invalid = request();
        invalid.destination_organization_id = None;
        assert!(invalid.validate(1_000).is_err());
        let mut same = request();
        same.destination_kind = TransferPayer::Personal;
        assert!(same.validate(1_000).is_err());
    }

    #[test]
    fn quote_must_be_live_but_renewal_boundary_can_be_after_quote_expiry() {
        let mut invalid = request();
        invalid.quote_expires_at_epoch = 1_000;
        assert!(invalid.validate(1_000).is_err());
        let mut invalid = request();
        invalid.effective_from = 900;
        assert!(invalid.validate(1_000).is_err());
        let mut next_renewal = request();
        next_renewal.effective_from = 3_000;
        assert!(next_renewal.validate(1_000).is_ok());
    }

    #[test]
    fn destination_invoice_end_must_match_the_quoted_billing_interval() {
        let monthly_start = FoundingDate::new(2027, 2, 14).unwrap().to_unix_seconds() + 8 * 3_600;
        let monthly_end = FoundingDate::new(2027, 3, 14).unwrap().to_unix_seconds() + 8 * 3_600;
        assert_eq!(
            expected_transfer_period_end(BillingOffer::StandardMonthly, monthly_start),
            Some(monthly_end)
        );
        assert_eq!(
            expected_transfer_period_end(BillingOffer::FoundingMonthly, monthly_start),
            Some(monthly_end)
        );

        let annual_start = FoundingDate::new(2024, 2, 29).unwrap().to_unix_seconds() + 123;
        let annual_end = FoundingDate::new(2025, 2, 28).unwrap().to_unix_seconds() + 123;
        assert_eq!(
            expected_transfer_period_end(BillingOffer::StandardAnnual, annual_start),
            Some(annual_end)
        );
        assert_eq!(
            expected_transfer_period_end(BillingOffer::FoundingAnnual, annual_start),
            Some(annual_end)
        );
    }

    #[test]
    fn source_terms_must_match_the_quoted_offer_and_paid_through_boundary() {
        let mut transfer = request();
        transfer.effective_from = 3_000;
        assert_eq!(
            validate_source_term(
                Some(("standard_monthly".into(), Some(3_000), "sub_source".into(),)),
                &transfer,
                1_000,
            )
            .unwrap(),
            "standard_monthly"
        );
        assert!(matches!(
            validate_source_term(
                Some(("standard_monthly".into(), Some(3_001), "sub_source".into(),)),
                &transfer,
                1_000,
            ),
            Err(TransferError::SourceTermChanged)
        ));
        assert!(matches!(
            validate_source_term(
                Some((
                    "standard_monthly".into(),
                    Some(3_000),
                    "sub_replaced".into(),
                )),
                &transfer,
                1_000,
            ),
            Err(TransferError::SourceTermChanged)
        ));
        assert!(matches!(
            validate_offer_at_boundary(&transfer, "standard_annual", None),
            Err(TransferError::SourceTermChanged)
        ));
    }

    #[test]
    fn founding_offer_changes_to_standard_at_the_original_end_boundary() {
        let award = crate::founding_allocator::FoundingAward {
            award_id: "award".into(),
            beneficiary_id: "user".into(),
            payer_kind: crate::founding_allocator::FoundingPayerKind::Personal,
            payer_id: "user".into(),
            offer: crate::founding_allocator::FoundingOffer::Monthly,
            cohort_ordinal: 1,
            original_start: crate::founding_allocator::FoundingDate::new(2026, 1, 1).unwrap(),
            original_end: crate::founding_allocator::FoundingDate::new(2027, 1, 1).unwrap(),
            original_start_epoch: Some(1_000),
            original_end_epoch: Some(2_000),
        };
        let mut transfer = request();
        transfer.effective_from = 1_999;
        transfer.offer = BillingOffer::FoundingMonthly;
        assert!(validate_offer_at_boundary(&transfer, "founding_monthly", Some(&award)).is_ok());
        assert!(matches!(
            validate_offer_at_boundary(&transfer, "standard_monthly", Some(&award)),
            Err(TransferError::SourceTermChanged)
        ));

        transfer.effective_from = 2_000;
        transfer.offer = BillingOffer::StandardMonthly;
        assert!(validate_offer_at_boundary(&transfer, "founding_monthly", Some(&award)).is_ok());

        transfer.offer = BillingOffer::FoundingMonthly;
        assert!(matches!(
            validate_offer_at_boundary(&transfer, "founding_monthly", Some(&award)),
            Err(TransferError::SourceTermChanged)
        ));
    }

    #[test]
    fn server_derived_offer_keeps_founding_price_only_before_its_original_end() {
        let award = crate::founding_allocator::FoundingAward {
            award_id: "award".into(),
            beneficiary_id: "user".into(),
            payer_kind: crate::founding_allocator::FoundingPayerKind::Personal,
            payer_id: "user".into(),
            offer: crate::founding_allocator::FoundingOffer::Annual,
            cohort_ordinal: 1,
            original_start: crate::founding_allocator::FoundingDate::new(2026, 1, 1).unwrap(),
            original_end: crate::founding_allocator::FoundingDate::new(2027, 1, 1).unwrap(),
            original_start_epoch: Some(1_000),
            original_end_epoch: Some(2_000),
        };
        assert_eq!(
            offer_at_transfer_boundary("founding_annual", 1_999, Some(&award)).unwrap(),
            BillingOffer::FoundingAnnual
        );
        assert_eq!(
            offer_at_transfer_boundary("founding_annual", 2_000, Some(&award)).unwrap(),
            BillingOffer::StandardAnnual
        );
        assert!(matches!(
            offer_at_transfer_boundary("founding_annual", 2_001, None),
            Err(TransferError::CorruptState)
        ));
    }

    #[test]
    fn only_live_transfer_states_block_a_second_transfer() {
        assert!(TransferState::Pending.is_live());
        assert!(TransferState::AwaitingConsent.is_live());
        assert!(TransferState::SourceAdjustmentPending.is_live());
        assert!(TransferState::SourceRestorationPending.is_live());
        assert!(!TransferState::Completed.is_live());
        assert!(!TransferState::Failed.is_live());
    }

    #[test]
    fn transfer_retry_delay_is_bounded_and_local_drift_waits_longer() {
        assert_eq!(
            transfer_retry_delay_seconds(TransferExecutionFailure::RetryableProvider, 0),
            15
        );
        assert_eq!(
            transfer_retry_delay_seconds(TransferExecutionFailure::RetryableProvider, 4),
            240
        );
        assert_eq!(
            transfer_retry_delay_seconds(TransferExecutionFailure::RetryableProvider, 100),
            MAX_TRANSFER_RETRY_DELAY_SECONDS
        );
        assert_eq!(
            transfer_retry_delay_seconds(TransferExecutionFailure::LocalEvidenceChanged, 0),
            MAX_TRANSFER_RETRY_DELAY_SECONDS
        );
    }

    #[test]
    fn rejected_or_exhausted_provider_work_requires_manual_review() {
        assert!(TransferExecutionFailure::ProviderOutcomeUnknown.needs_unknown_state());
        assert!(!TransferExecutionFailure::RetryableProvider.needs_unknown_state());
        assert!(!TransferExecutionFailure::LocalEvidenceChanged.needs_unknown_state());
        assert!(TransferExecutionFailure::ProviderRejected.needs_unknown_state());
        assert!(TransferExecutionFailure::ProviderRejected.requires_manual_review(1));
        assert!(TransferExecutionFailure::RetryableProvider
            .requires_manual_review(MAX_AUTOMATIC_TRANSFER_ATTEMPTS));
        assert!(!TransferExecutionFailure::RetryableProvider
            .requires_manual_review(MAX_AUTOMATIC_TRANSFER_ATTEMPTS - 1));
    }
}
