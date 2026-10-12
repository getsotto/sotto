//! Durable named sponsor seat quotes and operations.
//!
//! Every operation names an existing beneficiary. Organisation authority is checked while the
//! organisation row is locked, and provider calls happen only after that transaction commits.

use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Row, Transaction};
use thiserror::Error;
use uuid::Uuid;

use crate::billing_catalogue::BillingOffer;
use crate::error::Error;
use crate::org;

pub const QUOTE_SECONDS: i64 = 15 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SponsoredSeatAction {
    Add,
    Remove,
    Replace,
}

impl SponsoredSeatAction {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Remove => "remove",
            Self::Replace => "replace",
        }
    }

    pub fn parse(value: &str) -> Result<Self, SponsoredBillingError> {
        match value {
            "add" => Ok(Self::Add),
            "remove" => Ok(Self::Remove),
            "replace" => Ok(Self::Replace),
            _ => Err(SponsoredBillingError::InvalidField("action")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SponsoredSeatState {
    Pending,
    Active,
    ScheduledRemoval,
    Replaced,
    Canceled,
}

impl SponsoredSeatState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Active => "active",
            Self::ScheduledRemoval => "scheduled_removal",
            Self::Replaced => "replaced",
            Self::Canceled => "canceled",
        }
    }

    fn parse(value: &str) -> Result<Self, SponsoredBillingError> {
        match value {
            "pending" => Ok(Self::Pending),
            "active" => Ok(Self::Active),
            "scheduled_removal" => Ok(Self::ScheduledRemoval),
            "replaced" => Ok(Self::Replaced),
            "canceled" => Ok(Self::Canceled),
            _ => Err(SponsoredBillingError::CorruptState),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SponsoredSeat {
    pub seat_id: String,
    pub organization_id: String,
    pub beneficiary_id: String,
    pub offer: BillingOffer,
    pub effective_from: i64,
    pub effective_until: Option<i64>,
    pub state: SponsoredSeatState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SponsoredSeatRequest {
    pub action: SponsoredSeatAction,
    pub beneficiary_id: String,
    pub replacement_beneficiary_id: Option<String>,
    pub offer: BillingOffer,
    pub quote_version: i64,
    pub quote_expires_at_epoch: i64,
    pub effective_from: i64,
    pub effective_until: Option<i64>,
    pub idempotency_key: String,
}

impl SponsoredSeatRequest {
    pub fn validate(&self, now_epoch: i64) -> Result<(), SponsoredBillingError> {
        self.validate_identity()?;
        if self.effective_from < now_epoch {
            return Err(SponsoredBillingError::InvalidField("effective interval"));
        }
        if self.quote_expires_at_epoch <= now_epoch {
            return Err(SponsoredBillingError::QuoteExpired);
        }
        Ok(())
    }

    fn validate_identity(&self) -> Result<(), SponsoredBillingError> {
        if self.beneficiary_id.trim().is_empty() {
            return Err(SponsoredBillingError::InvalidField("beneficiary_id"));
        }
        if self.idempotency_key.trim().is_empty() {
            return Err(SponsoredBillingError::InvalidField("idempotency_key"));
        }
        if self.quote_version < 1 {
            return Err(SponsoredBillingError::InvalidField("quote_version"));
        }
        if self.effective_from > self.quote_expires_at_epoch {
            return Err(SponsoredBillingError::InvalidField("effective_from"));
        }
        if self
            .effective_until
            .is_some_and(|until| until <= self.effective_from)
        {
            return Err(SponsoredBillingError::InvalidField("effective interval"));
        }
        if matches!(
            self.action,
            SponsoredSeatAction::Remove | SponsoredSeatAction::Replace
        ) && self.effective_until.is_none()
        {
            return Err(SponsoredBillingError::InvalidField("effective_until"));
        }
        match self.action {
            SponsoredSeatAction::Replace if self.replacement_beneficiary_id.is_none() => Err(
                SponsoredBillingError::InvalidField("replacement_beneficiary_id"),
            ),
            SponsoredSeatAction::Replace
                if self.replacement_beneficiary_id.as_deref()
                    == Some(self.beneficiary_id.as_str()) =>
            {
                Err(SponsoredBillingError::InvalidField(
                    "replacement_beneficiary_id",
                ))
            }
            SponsoredSeatAction::Add | SponsoredSeatAction::Remove
                if self.replacement_beneficiary_id.is_some() =>
            {
                Err(SponsoredBillingError::InvalidField(
                    "replacement_beneficiary_id",
                ))
            }
            _ => Ok(()),
        }
    }

    fn ensure_unexpired(&self, now_epoch: i64) -> Result<(), SponsoredBillingError> {
        if self.quote_expires_at_epoch <= now_epoch {
            return Err(SponsoredBillingError::QuoteExpired);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SponsoredQuote {
    pub action: SponsoredSeatAction,
    pub seat_count: i64,
    pub amount_pence: i64,
    pub currency: &'static str,
    pub interval: &'static str,
    pub quote_version: i64,
    pub quote_expires_at_epoch: i64,
}

pub fn quote(
    action: SponsoredSeatAction,
    offer: BillingOffer,
    seat_count: usize,
    now_epoch: i64,
) -> Result<SponsoredQuote, SponsoredBillingError> {
    if seat_count == 0 || seat_count > 1000 {
        return Err(SponsoredBillingError::InvalidField("seat_count"));
    }
    let seat_count =
        i64::try_from(seat_count).map_err(|_| SponsoredBillingError::InvalidField("seat_count"))?;
    let amount_pence = if action == SponsoredSeatAction::Remove {
        0
    } else {
        offer
            .expected_amount_pence()
            .checked_mul(seat_count)
            .ok_or(SponsoredBillingError::InvalidField("seat_count"))?
    };
    let quote_expires_at_epoch = now_epoch
        .checked_add(QUOTE_SECONDS)
        .ok_or(SponsoredBillingError::InvalidField("quote expiry"))?;
    Ok(SponsoredQuote {
        action,
        seat_count,
        amount_pence,
        currency: "gbp",
        interval: offer.expected_interval().as_str(),
        quote_version: 1,
        quote_expires_at_epoch,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SponsoredOperation {
    pub operation_id: String,
    pub organization_id: String,
    pub actor_user_id: String,
    pub beneficiary_id: String,
    pub replacement_beneficiary_id: Option<String>,
    pub action: SponsoredSeatAction,
    pub offer: BillingOffer,
    pub quote_version: i64,
    pub quote_expires_at_epoch: i64,
    pub effective_from: i64,
    pub effective_until: Option<i64>,
    pub request_hash: String,
    pub provider_idempotency_key: String,
    pub provider_customer_id: Option<String>,
    pub provider_subscription_id: Option<String>,
    pub provider_checkout_session_id: Option<String>,
    pub provider_payment_reference: Option<String>,
    pub provider_item_id: Option<String>,
    pub provider_checkout_url: Option<String>,
    pub provider_operation_id: Option<String>,
    pub state: String,
    pub result_code: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SponsoredProviderEvidence {
    pub customer_id: Option<String>,
    pub subscription_id: String,
    pub schedule_id: Option<String>,
    pub checkout_session_id: Option<String>,
    pub payment_reference: String,
    pub provider_item_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SponsoredSubscription {
    pub organization_id: String,
    pub provider_customer_id: Option<String>,
    pub provider_subscription_id: Option<String>,
    pub provider_schedule_id: Option<String>,
    pub status: String,
}

#[derive(Debug, Error)]
pub enum SponsoredBillingError {
    #[error("sponsored billing operation has invalid {0}")]
    InvalidField(&'static str),
    #[error("sponsored billing quote has expired")]
    QuoteExpired,
    #[error("sponsored billing operation is not authorised")]
    Unauthorised,
    #[error("sponsored organisation is not active")]
    OrganisationNotActive,
    #[error("sponsored beneficiary must be an existing account")]
    BeneficiaryMissing,
    #[error("sponsored beneficiary already has a live seat")]
    BeneficiaryAlreadyCovered,
    #[error("sponsored seat is missing or not active")]
    SeatMissing,
    #[error("sponsored operation conflicts with an existing request")]
    IdempotencyConflict,
    #[error("sponsored operation result conflicts with stored evidence")]
    ResultConflict,
    #[error("sponsored billing state is corrupt")]
    CorruptState,
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
}

impl From<SponsoredBillingError> for Error {
    fn from(error: SponsoredBillingError) -> Self {
        match error {
            SponsoredBillingError::InvalidField(field) => {
                Self::BadRequest(format!("invalid sponsored billing {field}"))
            }
            SponsoredBillingError::QuoteExpired => Self::Conflict("sponsored quote expired".into()),
            SponsoredBillingError::Unauthorised => {
                Self::Forbidden("sponsored billing is not authorised".into())
            }
            SponsoredBillingError::OrganisationNotActive => {
                Self::Conflict("organisation deletion is in progress".into())
            }
            SponsoredBillingError::BeneficiaryMissing => {
                Self::NotFound("sponsored beneficiary not found".into())
            }
            SponsoredBillingError::BeneficiaryAlreadyCovered => {
                Self::Conflict("beneficiary already has sponsored coverage".into())
            }
            SponsoredBillingError::SeatMissing => Self::NotFound("sponsored seat not found".into()),
            SponsoredBillingError::IdempotencyConflict => Self::Conflict(
                "sponsored idempotency key conflicts with a different request".into(),
            ),
            SponsoredBillingError::ResultConflict => {
                Self::Conflict("sponsored provider result conflicts with stored evidence".into())
            }
            SponsoredBillingError::CorruptState => {
                Self::Internal("sponsored billing state is corrupt".into())
            }
            SponsoredBillingError::Database(error) => Self::Db(error),
        }
    }
}

pub fn current_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_secs() as i64
}

pub fn request_hash(
    organization_id: &str,
    actor_user_id: &str,
    request: &SponsoredSeatRequest,
) -> String {
    let mut preimage = Vec::new();
    append_hash_field(&mut preimage, "sponsored-v2");
    append_hash_field(&mut preimage, organization_id);
    append_hash_field(&mut preimage, actor_user_id);
    append_hash_field(&mut preimage, request.action.as_str());
    append_hash_field(&mut preimage, &request.beneficiary_id);
    append_optional_hash_field(&mut preimage, request.replacement_beneficiary_id.as_deref());
    append_hash_field(&mut preimage, request.offer.as_str());
    append_hash_field(&mut preimage, &request.quote_version.to_string());
    append_hash_field(&mut preimage, &request.quote_expires_at_epoch.to_string());
    append_hash_field(&mut preimage, &request.effective_from.to_string());
    append_optional_hash_field(
        &mut preimage,
        request
            .effective_until
            .map(|value| value.to_string())
            .as_deref(),
    );
    append_hash_field(&mut preimage, &request.idempotency_key);
    digest_hex(&Sha256::digest(preimage))
}

fn append_hash_field(preimage: &mut Vec<u8>, value: &str) {
    let bytes = value.as_bytes();
    preimage.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
    preimage.extend_from_slice(bytes);
}

fn append_optional_hash_field(preimage: &mut Vec<u8>, value: Option<&str>) {
    match value {
        Some(value) => {
            preimage.push(1);
            append_hash_field(preimage, value);
        }
        None => preimage.push(0),
    }
}

pub async fn list_seats(
    pool: &PgPool,
    organization_id: &str,
    actor_user_id: &str,
) -> Result<Vec<SponsoredSeat>, SponsoredBillingError> {
    let access = org::access(pool, organization_id, actor_user_id)
        .await
        .map_err(|_| SponsoredBillingError::Unauthorised)?;
    if !access.role().can_manage_members() {
        return Err(SponsoredBillingError::Unauthorised);
    }
    let rows = sqlx::query(
        "SELECT seat_id, organization_id, beneficiary_id, offer, effective_from, effective_until, state \
         FROM billing_sponsored_seats WHERE organization_id = $1 ORDER BY effective_from, seat_id",
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await?;
    rows.iter().map(seat_from_row).collect()
}

pub async fn begin_operation(
    tx: &mut Transaction<'_, Postgres>,
    organization_id: &str,
    actor_user_id: &str,
    request: &SponsoredSeatRequest,
    now_epoch: i64,
) -> Result<SponsoredOperation, SponsoredBillingError> {
    request.validate_identity()?;
    let access = org::access_for_update(tx, organization_id, actor_user_id)
        .await
        .map_err(|_| SponsoredBillingError::Unauthorised)?;
    access
        .require_write()
        .map_err(|_| SponsoredBillingError::OrganisationNotActive)?;
    if !access.role().can_manage_members() {
        return Err(SponsoredBillingError::Unauthorised);
    }
    for beneficiary_id in
        std::iter::once(&request.beneficiary_id).chain(request.replacement_beneficiary_id.iter())
    {
        let exists: Option<String> = sqlx::query_scalar("SELECT id FROM users WHERE id = $1")
            .bind(beneficiary_id)
            .fetch_optional(&mut **tx)
            .await?;
        if exists.is_none() {
            return Err(SponsoredBillingError::BeneficiaryMissing);
        }
    }
    let hash = request_hash(organization_id, actor_user_id, request);
    if let Some(existing) =
        load_by_idempotency(tx, organization_id, actor_user_id, &request.idempotency_key).await?
    {
        if existing.request_hash != hash {
            return Err(SponsoredBillingError::IdempotencyConflict);
        }
        return Ok(existing);
    }
    if request.effective_from < now_epoch {
        return Err(SponsoredBillingError::InvalidField("effective interval"));
    }
    request.ensure_unexpired(now_epoch)?;
    let existing_live: Option<String> = sqlx::query_scalar(
        "SELECT state FROM billing_sponsored_seats WHERE organization_id = $1 \
         AND beneficiary_id = $2 AND state IN ('pending','active','scheduled_removal') FOR UPDATE",
    )
    .bind(organization_id)
    .bind(&request.beneficiary_id)
    .fetch_optional(&mut **tx)
    .await?;
    match request.action {
        SponsoredSeatAction::Add if existing_live.is_some() => {
            return Err(SponsoredBillingError::BeneficiaryAlreadyCovered)
        }
        SponsoredSeatAction::Remove | SponsoredSeatAction::Replace
            if existing_live.as_deref() != Some("active") =>
        {
            return Err(SponsoredBillingError::SeatMissing)
        }
        _ => {}
    }
    if let Some(replacement) = request.replacement_beneficiary_id.as_deref() {
        let occupied: Option<String> = sqlx::query_scalar(
            "SELECT beneficiary_id FROM billing_sponsored_seats WHERE organization_id = $1 \
             AND beneficiary_id = $2 AND state IN ('pending','active','scheduled_removal') FOR UPDATE",
        )
        .bind(organization_id)
        .bind(replacement)
        .fetch_optional(&mut **tx)
        .await?;
        if occupied.is_some() {
            return Err(SponsoredBillingError::BeneficiaryAlreadyCovered);
        }
    }
    let operation_id = format!("sponsored:{}", Uuid::new_v4());
    let provider_idempotency_key = format!(
        "sotto-sponsored:{}",
        digest_hex(&Sha256::digest(operation_id.as_bytes()))
    );
    sqlx::query(
        "INSERT INTO billing_sponsored_operations \
         (operation_id, organization_id, actor_user_id, idempotency_key, request_hash, action, beneficiary_id, \
          replacement_beneficiary_id, offer, quote_version, quote_expires_at_epoch, effective_from, effective_until, provider_idempotency_key) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)",
    )
    .bind(&operation_id)
    .bind(organization_id)
    .bind(actor_user_id)
    .bind(&request.idempotency_key)
    .bind(&hash)
    .bind(request.action.as_str())
    .bind(&request.beneficiary_id)
    .bind(request.replacement_beneficiary_id.as_deref())
    .bind(request.offer.as_str())
    .bind(request.quote_version)
    .bind(request.quote_expires_at_epoch)
    .bind(request.effective_from)
    .bind(request.effective_until)
    .bind(&provider_idempotency_key)
    .execute(&mut **tx)
    .await?;
    if matches!(
        request.action,
        SponsoredSeatAction::Add | SponsoredSeatAction::Replace
    ) {
        let beneficiary = request
            .replacement_beneficiary_id
            .as_deref()
            .unwrap_or(&request.beneficiary_id);
        let seat_effective_from = match request.action {
            SponsoredSeatAction::Replace => request
                .effective_until
                .expect("replacement validation requires an end boundary"),
            SponsoredSeatAction::Add => request.effective_from,
            SponsoredSeatAction::Remove => unreachable!("remove does not create a seat"),
        };
        let seat_effective_until = match request.action {
            SponsoredSeatAction::Replace => None,
            SponsoredSeatAction::Add => request.effective_until,
            SponsoredSeatAction::Remove => unreachable!("remove does not create a seat"),
        };
        sqlx::query(
            "INSERT INTO billing_sponsored_seats \
             (seat_id, organization_id, beneficiary_id, offer, effective_from, effective_until, state, operation_id) \
             VALUES ($1,$2,$3,$4,$5,$6,'pending',$7)",
        )
        .bind(format!("seat:{}", Uuid::new_v4()))
        .bind(organization_id)
        .bind(beneficiary)
        .bind(request.offer.as_str())
        .bind(seat_effective_from)
        .bind(seat_effective_until)
        .bind(&operation_id)
        .execute(&mut **tx)
        .await?;
    }
    load_operation(tx, &operation_id).await
}

pub async fn record_checkout(
    tx: &mut Transaction<'_, Postgres>,
    operation_id: &str,
    checkout_url: &str,
    checkout_session_id: &str,
    customer_id: Option<&str>,
    subscription_id: Option<&str>,
) -> Result<SponsoredOperation, SponsoredBillingError> {
    let updated = sqlx::query(
        "UPDATE billing_sponsored_operations SET state = 'checkout_created', provider_checkout_url = $2, \
         provider_checkout_session_id = $3, provider_customer_id = $4, provider_subscription_id = $5, updated_at = now() \
         WHERE operation_id = $1 AND state = 'pending' RETURNING operation_id",
    )
    .bind(operation_id)
    .bind(checkout_url)
    .bind(checkout_session_id)
    .bind(customer_id)
    .bind(subscription_id)
    .fetch_optional(&mut **tx)
    .await?;
    if updated.is_none() {
        let current = load_operation(tx, operation_id).await?;
        if current.state != "checkout_created"
            || current.provider_checkout_url.as_deref() != Some(checkout_url)
            || current.provider_checkout_session_id.as_deref() != Some(checkout_session_id)
        {
            return Err(SponsoredBillingError::ResultConflict);
        }
    }
    sqlx::query(
        "UPDATE billing_sponsored_seats SET state = 'scheduled_removal', effective_until = COALESCE(operation.effective_until, operation.effective_from), updated_at = now() \
         FROM billing_sponsored_operations AS operation \
         WHERE operation.operation_id = $1 AND operation.action IN ('remove', 'replace') \
           AND billing_sponsored_seats.organization_id = operation.organization_id \
           AND billing_sponsored_seats.beneficiary_id = operation.beneficiary_id \
           AND billing_sponsored_seats.state = 'active'",
    )
    .bind(operation_id)
    .execute(&mut **tx)
    .await?;
    load_operation(tx, operation_id).await
}

pub async fn record_provider_update(
    tx: &mut Transaction<'_, Postgres>,
    operation_id: &str,
    customer_id: Option<&str>,
    subscription_id: &str,
    schedule_id: Option<&str>,
    current_period_end: Option<i64>,
    provider_item_id: Option<&str>,
) -> Result<SponsoredOperation, SponsoredBillingError> {
    let updated = sqlx::query(
        "UPDATE billing_sponsored_operations SET state = 'provider_pending', \
         provider_customer_id = $2, provider_subscription_id = $3, provider_item_id = $4, updated_at = now() \
         WHERE operation_id = $1 AND state = 'pending' RETURNING operation_id",
    )
    .bind(operation_id)
    .bind(customer_id)
    .bind(subscription_id)
    .bind(provider_item_id)
    .fetch_optional(&mut **tx)
    .await?;
    if updated.is_none() {
        let current = load_operation(tx, operation_id).await?;
        if current.state != "provider_pending"
            || current.provider_subscription_id.as_deref() != Some(subscription_id)
        {
            return Err(SponsoredBillingError::ResultConflict);
        }
    }
    let organization_id: String = sqlx::query_scalar(
        "SELECT organization_id FROM billing_sponsored_operations WHERE operation_id = $1",
    )
    .bind(operation_id)
    .fetch_one(&mut **tx)
    .await?;
    sqlx::query(
        "INSERT INTO billing_sponsored_subscriptions \
         (organization_id, provider_customer_id, provider_subscription_id, provider_schedule_id, status, updated_at) \
         VALUES ($1, $2, $3, $4, 'active', now()) \
         ON CONFLICT (organization_id) DO UPDATE SET \
           provider_customer_id = COALESCE(EXCLUDED.provider_customer_id, billing_sponsored_subscriptions.provider_customer_id), \
           provider_subscription_id = EXCLUDED.provider_subscription_id, \
           provider_schedule_id = COALESCE(EXCLUDED.provider_schedule_id, billing_sponsored_subscriptions.provider_schedule_id), \
           status = 'active', updated_at = now()",
    )
    .bind(&organization_id)
    .bind(customer_id)
    .bind(subscription_id)
    .bind(schedule_id)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "UPDATE billing_sponsored_seats SET state = 'scheduled_removal', \
         effective_until = GREATEST(\
             COALESCE(operation.effective_until, operation.effective_from),\
             COALESCE($2, 0)\
         ), updated_at = now() \
         FROM billing_sponsored_operations AS operation \
         WHERE operation.operation_id = $1 AND operation.action IN ('remove', 'replace') \
           AND billing_sponsored_seats.organization_id = operation.organization_id \
           AND billing_sponsored_seats.beneficiary_id = operation.beneficiary_id \
           AND billing_sponsored_seats.state = 'active'",
    )
    .bind(operation_id)
    .bind(current_period_end)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "UPDATE billing_sponsored_seats SET effective_from = GREATEST(\
             effective_from, COALESCE($2, 0)\
         ), updated_at = now() \
         WHERE operation_id = $1 AND state = 'pending'",
    )
    .bind(operation_id)
    .bind(current_period_end)
    .execute(&mut **tx)
    .await?;
    load_operation(tx, operation_id).await
}

pub async fn record_provider_item(
    tx: &mut Transaction<'_, Postgres>,
    organization_id: &str,
    provider_item_id: &str,
    provider_price_id: &str,
    offer: BillingOffer,
    quantity: i64,
) -> Result<(), SponsoredBillingError> {
    if provider_item_id.trim().is_empty() || provider_price_id.trim().is_empty() || quantity < 0 {
        return Err(SponsoredBillingError::CorruptState);
    }
    sqlx::query(
        "INSERT INTO billing_sponsored_subscription_items \
         (subscription_item_id, organization_id, provider_item_id, provider_price_id, offer, quantity, updated_at) \
         VALUES ($1, $2, $3, $4, $5, $6, now()) \
         ON CONFLICT (organization_id, offer) DO UPDATE SET provider_item_id = EXCLUDED.provider_item_id, \
           provider_price_id = EXCLUDED.provider_price_id, quantity = EXCLUDED.quantity, updated_at = now()",
    )
    .bind(format!("sponsored-item:{}", Uuid::new_v4()))
    .bind(organization_id)
    .bind(provider_item_id)
    .bind(provider_price_id)
    .bind(offer.as_str())
    .bind(quantity)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub async fn complete_paid_checkout(
    tx: &mut Transaction<'_, Postgres>,
    operation_id: &str,
    evidence: &SponsoredProviderEvidence,
) -> Result<SponsoredOperation, SponsoredBillingError> {
    let organization_id: String = sqlx::query_scalar(
        "SELECT organization_id FROM billing_sponsored_operations WHERE operation_id = $1",
    )
    .bind(operation_id)
    .fetch_one(&mut **tx)
    .await?;
    let lifecycle_state: Option<String> =
        sqlx::query_scalar("SELECT lifecycle_state FROM organizations WHERE id = $1 FOR UPDATE")
            .bind(&organization_id)
            .fetch_optional(&mut **tx)
            .await?;
    if lifecycle_state.as_deref() != Some("active") {
        return Err(SponsoredBillingError::OrganisationNotActive);
    }
    let updated = sqlx::query(
        "UPDATE billing_sponsored_operations SET state = 'active', provider_operation_id = $2, \
         provider_payment_reference = $2, provider_customer_id = COALESCE($3, provider_customer_id), \
         provider_subscription_id = $4, provider_checkout_session_id = COALESCE($5, provider_checkout_session_id), \
         provider_item_id = COALESCE($6, provider_item_id), result_code = 'paid', updated_at = now() \
         WHERE operation_id = $1 AND state IN ('checkout_created','provider_pending') RETURNING operation_id",
    )
    .bind(operation_id)
    .bind(&evidence.payment_reference)
    .bind(evidence.customer_id.as_deref())
    .bind(&evidence.subscription_id)
    .bind(evidence.checkout_session_id.as_deref())
    .bind(evidence.provider_item_id.as_deref())
    .fetch_optional(&mut **tx)
    .await?;
    if updated.is_none() {
        let current = load_operation(tx, operation_id).await?;
        if current.state != "active"
            || current.provider_payment_reference.as_deref()
                != Some(evidence.payment_reference.as_str())
        {
            return Err(SponsoredBillingError::ResultConflict);
        }
        return Ok(current);
    }
    sqlx::query(
        "UPDATE billing_sponsored_seats SET state = 'active', provider_item_id = COALESCE($2, provider_item_id), updated_at = now() \
         WHERE operation_id = $1 AND state = 'pending'",
    )
    .bind(operation_id)
    .bind(evidence.provider_item_id.as_deref())
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "INSERT INTO billing_sponsored_subscriptions \
         (organization_id, provider_customer_id, provider_subscription_id, provider_schedule_id, status, updated_at) \
         VALUES ($1, $2, $3, $4, 'active', now()) \
         ON CONFLICT (organization_id) DO UPDATE SET provider_customer_id = COALESCE(EXCLUDED.provider_customer_id, billing_sponsored_subscriptions.provider_customer_id), \
           provider_subscription_id = EXCLUDED.provider_subscription_id, \
           provider_schedule_id = COALESCE(EXCLUDED.provider_schedule_id, billing_sponsored_subscriptions.provider_schedule_id), \
           status = 'active', updated_at = now()",
    )
    .bind(&organization_id)
    .bind(evidence.customer_id.as_deref())
    .bind(&evidence.subscription_id)
    .bind(evidence.schedule_id.as_deref())
    .execute(&mut **tx)
    .await?;
    load_operation(tx, operation_id).await
}

pub async fn complete_local_removal(
    tx: &mut Transaction<'_, Postgres>,
    operation_id: &str,
) -> Result<SponsoredOperation, SponsoredBillingError> {
    let updated = sqlx::query(
        "UPDATE billing_sponsored_operations SET state = 'active', result_code = 'scheduled_removal', \
         updated_at = now() WHERE operation_id = $1 AND action = 'remove' AND state = 'pending' \
         RETURNING operation_id",
    )
    .bind(operation_id)
    .fetch_optional(&mut **tx)
    .await?;
    if updated.is_none() {
        let current = load_operation(tx, operation_id).await?;
        if current.state != "active" || current.result_code.as_deref() != Some("scheduled_removal")
        {
            return Err(SponsoredBillingError::ResultConflict);
        }
        return Ok(current);
    }
    load_operation(tx, operation_id).await
}

pub async fn cancel_failed_operation(
    tx: &mut Transaction<'_, Postgres>,
    operation_id: &str,
    result_code: &str,
) -> Result<SponsoredOperation, SponsoredBillingError> {
    let updated = sqlx::query(
        "UPDATE billing_sponsored_operations SET state = 'failed', result_code = $2, updated_at = now() \
         WHERE operation_id = $1 AND state IN ('pending','checkout_created','provider_pending','unknown') RETURNING operation_id",
    )
    .bind(operation_id)
    .bind(result_code)
    .fetch_optional(&mut **tx)
    .await?;
    if updated.is_none() {
        let current = load_operation(tx, operation_id).await?;
        if current.state != "failed" || current.result_code.as_deref() != Some(result_code) {
            return Err(SponsoredBillingError::ResultConflict);
        }
        return Ok(current);
    }
    sqlx::query(
        "UPDATE billing_sponsored_seats SET state = 'canceled', updated_at = now() \
         WHERE operation_id = $1 AND state = 'pending'",
    )
    .bind(operation_id)
    .execute(&mut **tx)
    .await?;
    load_operation(tx, operation_id).await
}

pub async fn load_operation_for_actor(
    pool: &PgPool,
    operation_id: &str,
    actor_user_id: &str,
) -> Result<Option<SponsoredOperation>, SponsoredBillingError> {
    let row = sqlx::query(
        "SELECT * FROM billing_sponsored_operations WHERE operation_id = $1 AND actor_user_id = $2",
    )
    .bind(operation_id)
    .bind(actor_user_id)
    .fetch_optional(pool)
    .await?;
    row.map(|row| operation_from_row(&row)).transpose()
}

async fn load_operation(
    tx: &mut Transaction<'_, Postgres>,
    operation_id: &str,
) -> Result<SponsoredOperation, SponsoredBillingError> {
    let row = sqlx::query(
        "SELECT * FROM billing_sponsored_operations WHERE operation_id = $1 FOR UPDATE",
    )
    .bind(operation_id)
    .fetch_one(&mut **tx)
    .await?;
    operation_from_row(&row)
}

async fn load_by_idempotency(
    tx: &mut Transaction<'_, Postgres>,
    organization_id: &str,
    actor_user_id: &str,
    idempotency_key: &str,
) -> Result<Option<SponsoredOperation>, SponsoredBillingError> {
    let row = sqlx::query(
        "SELECT * FROM billing_sponsored_operations \
         WHERE organization_id = $1 AND actor_user_id = $2 AND idempotency_key = $3 FOR UPDATE",
    )
    .bind(organization_id)
    .bind(actor_user_id)
    .bind(idempotency_key)
    .fetch_optional(&mut **tx)
    .await?;
    row.map(|row| operation_from_row(&row)).transpose()
}

fn operation_from_row(
    row: &sqlx::postgres::PgRow,
) -> Result<SponsoredOperation, SponsoredBillingError> {
    Ok(SponsoredOperation {
        operation_id: row.try_get("operation_id")?,
        organization_id: row.try_get("organization_id")?,
        actor_user_id: row.try_get("actor_user_id")?,
        beneficiary_id: row.try_get("beneficiary_id")?,
        replacement_beneficiary_id: row.try_get("replacement_beneficiary_id")?,
        action: SponsoredSeatAction::parse(&row.try_get::<String, _>("action")?)?,
        offer: parse_offer(&row.try_get::<String, _>("offer")?)?,
        quote_version: row.try_get("quote_version")?,
        quote_expires_at_epoch: row.try_get("quote_expires_at_epoch")?,
        effective_from: row.try_get("effective_from")?,
        effective_until: row.try_get("effective_until")?,
        request_hash: row.try_get("request_hash")?,
        provider_idempotency_key: row.try_get("provider_idempotency_key")?,
        provider_customer_id: row.try_get("provider_customer_id")?,
        provider_subscription_id: row.try_get("provider_subscription_id")?,
        provider_checkout_session_id: row.try_get("provider_checkout_session_id")?,
        provider_payment_reference: row.try_get("provider_payment_reference")?,
        provider_item_id: row.try_get("provider_item_id")?,
        provider_checkout_url: row.try_get("provider_checkout_url")?,
        provider_operation_id: row.try_get("provider_operation_id")?,
        state: row.try_get("state")?,
        result_code: row.try_get("result_code")?,
    })
}

fn seat_from_row(row: &sqlx::postgres::PgRow) -> Result<SponsoredSeat, SponsoredBillingError> {
    Ok(SponsoredSeat {
        seat_id: row.try_get("seat_id")?,
        organization_id: row.try_get("organization_id")?,
        beneficiary_id: row.try_get("beneficiary_id")?,
        offer: parse_offer(&row.try_get::<String, _>("offer")?)?,
        effective_from: row.try_get("effective_from")?,
        effective_until: row.try_get("effective_until")?,
        state: SponsoredSeatState::parse(&row.try_get::<String, _>("state")?)?,
    })
}

fn parse_offer(value: &str) -> Result<BillingOffer, SponsoredBillingError> {
    BillingOffer::ALL
        .into_iter()
        .find(|offer| offer.as_str() == value)
        .ok_or(SponsoredBillingError::CorruptState)
}

fn digest_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_named_seats_with_a_bounded_expiry() {
        let monthly = quote(
            SponsoredSeatAction::Add,
            BillingOffer::StandardMonthly,
            3,
            100,
        )
        .unwrap();
        assert_eq!(monthly.amount_pence, 897);
        assert_eq!(monthly.quote_expires_at_epoch, 100 + QUOTE_SECONDS);
        assert_eq!(monthly.interval, "month");
        let removal = quote(
            SponsoredSeatAction::Remove,
            BillingOffer::StandardMonthly,
            1,
            100,
        )
        .unwrap();
        assert_eq!(removal.amount_pence, 0);
    }

    #[test]
    fn rejects_zero_and_oversized_seat_quotes() {
        assert!(quote(
            SponsoredSeatAction::Add,
            BillingOffer::StandardMonthly,
            0,
            100
        )
        .is_err());
        assert!(quote(
            SponsoredSeatAction::Add,
            BillingOffer::StandardMonthly,
            1001,
            100
        )
        .is_err());
    }

    #[test]
    fn replacement_requires_a_different_identity() {
        let request = SponsoredSeatRequest {
            action: SponsoredSeatAction::Replace,
            beneficiary_id: "a".into(),
            replacement_beneficiary_id: None,
            offer: BillingOffer::StandardMonthly,
            quote_version: 1,
            quote_expires_at_epoch: 200,
            effective_from: 100,
            effective_until: Some(200),
            idempotency_key: "request".into(),
        };
        assert!(request.validate(100).is_err());
        let mut same = request.clone();
        same.replacement_beneficiary_id = Some("a".into());
        assert!(same.validate(100).is_err());
    }

    #[test]
    fn request_hash_distinguishes_delimited_identifiers() {
        let first = SponsoredSeatRequest {
            action: SponsoredSeatAction::Replace,
            beneficiary_id: "a|b".into(),
            replacement_beneficiary_id: Some("c".into()),
            offer: BillingOffer::StandardMonthly,
            quote_version: 1,
            quote_expires_at_epoch: 200,
            effective_from: 100,
            effective_until: Some(200),
            idempotency_key: "request".into(),
        };
        let second = SponsoredSeatRequest {
            beneficiary_id: "a".into(),
            replacement_beneficiary_id: Some("b|c".into()),
            ..first.clone()
        };
        assert_ne!(
            request_hash("org", "actor", &first),
            request_hash("org", "actor", &second)
        );
    }
}
