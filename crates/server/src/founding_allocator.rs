//! Durable allocation of the first 100 founding-price places.
//!
//! Reservations are capacity claims, not awards. A place is consumed only when a verified
//! payment is confirmed. The capacity row serialises personal and sponsored reservations so two
//! concurrent buyers cannot create cohort ordinal 101.

use std::fmt;

use sqlx::{Postgres, Row, Transaction};
use thiserror::Error;
use uuid::Uuid;

use crate::billing_catalogue::{BillingInterval, BillingOffer};

pub const FOUNDING_CAPACITY: i64 = 100;
pub const RESERVATION_SECONDS: i64 = 30 * 60;
/// Keep a timely Checkout or scheduled sponsored place through the accepted 14-day settlement
/// recovery window before treating a late payment as refundable.
pub const SETTLEMENT_RECOVERY_SECONDS: i64 = 14 * 24 * 60 * 60;
/// A Checkout Session is created after the quote, so its expiry may be one quote window beyond
/// the reservation window while still satisfying Stripe's minimum lifetime.
pub const MAX_RESERVATION_SECONDS: i64 = RESERVATION_SECONDS * 2;
/// Sponsored scheduled additions can be charged at the next annual renewal plus the recovery
/// window (at most one leap-year annual term plus 14 days).
pub const MAX_CAPACITY_HOLD_SECONDS: i64 = 400 * 24 * 60 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FoundingCapacityStatus {
    pub confirmed_awards: i64,
    pub active_reservations: i64,
    pub remaining_places: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FoundingQuoteStatus {
    pub offer: FoundingOffer,
    pub remaining_places: i64,
    pub founding_amount_pence: i64,
    pub standard_amount_pence: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FoundingOffer {
    Monthly,
    Annual,
}

impl FoundingOffer {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Monthly => "founding_monthly",
            Self::Annual => "founding_annual",
        }
    }

    pub const fn billing_offer(self) -> BillingOffer {
        match self {
            Self::Monthly => BillingOffer::FoundingMonthly,
            Self::Annual => BillingOffer::FoundingAnnual,
        }
    }

    pub const fn standard_billing_offer(self) -> BillingOffer {
        match self {
            Self::Monthly => BillingOffer::StandardMonthly,
            Self::Annual => BillingOffer::StandardAnnual,
        }
    }

    pub const fn interval(self) -> BillingInterval {
        match self {
            Self::Monthly => BillingInterval::Month,
            Self::Annual => BillingInterval::Year,
        }
    }

    pub const fn amount_pence(self) -> i64 {
        self.billing_offer().expected_amount_pence()
    }

    pub const fn from_billing_offer(offer: BillingOffer) -> Option<Self> {
        match offer {
            BillingOffer::FoundingMonthly => Some(Self::Monthly),
            BillingOffer::FoundingAnnual => Some(Self::Annual),
            BillingOffer::StandardMonthly | BillingOffer::StandardAnnual => None,
        }
    }

    pub const fn for_billing_offer_interval(offer: BillingOffer) -> Self {
        match offer {
            BillingOffer::StandardMonthly | BillingOffer::FoundingMonthly => Self::Monthly,
            BillingOffer::StandardAnnual | BillingOffer::FoundingAnnual => Self::Annual,
        }
    }
}

impl fmt::Display for FoundingOffer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct FoundingDate {
    pub year: i32,
    pub month: u8,
    pub day: u8,
}

impl FoundingDate {
    pub const fn new(year: i32, month: u8, day: u8) -> Result<Self, CalendarError> {
        if month == 0 || month > 12 || day == 0 || day > days_in_month(year, month) {
            return Err(CalendarError::InvalidDate);
        }
        Ok(Self { year, month, day })
    }

    pub fn parse(value: &str) -> Result<Self, CalendarError> {
        let mut parts = value.split('-');
        let year = parts
            .next()
            .ok_or(CalendarError::InvalidDate)?
            .parse()
            .map_err(|_| CalendarError::InvalidDate)?;
        let month = parts
            .next()
            .ok_or(CalendarError::InvalidDate)?
            .parse()
            .map_err(|_| CalendarError::InvalidDate)?;
        let day = parts
            .next()
            .ok_or(CalendarError::InvalidDate)?
            .parse()
            .map_err(|_| CalendarError::InvalidDate)?;
        if parts.next().is_some() {
            return Err(CalendarError::InvalidDate);
        }
        Self::new(year, month, day)
    }

    /// Convert a non-negative Unix timestamp to the UTC calendar date used by billing anchors.
    pub fn from_unix_seconds(seconds: i64) -> Result<Self, CalendarError> {
        if seconds < 0 {
            return Err(CalendarError::InvalidDate);
        }
        let days = seconds / 86_400;
        let shifted = days + 719_468;
        let era = if shifted >= 0 {
            shifted / 146_097
        } else {
            (shifted - 146_096) / 146_097
        };
        let day_of_era = shifted - era * 146_097;
        let year_of_era =
            (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
        let year = year_of_era + era * 400;
        let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
        let month_part = (5 * day_of_year + 2) / 153;
        let day = day_of_year - (153 * month_part + 2) / 5 + 1;
        let month = month_part + if month_part < 10 { 3 } else { -9 };
        let year = year + i64::from(month <= 2);
        Self::new(year as i32, month as u8, day as u8)
    }

    pub fn to_unix_seconds(self) -> i64 {
        let year = i64::from(self.year) - i64::from(self.month <= 2);
        let era = year.div_euclid(400);
        let year_of_era = year - era * 400;
        let month = i64::from(self.month);
        let month_from_march = month + if month > 2 { -3 } else { 9 };
        let day_of_year = (153 * month_from_march + 2) / 5 + i64::from(self.day) - 1;
        let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
        (era * 146_097 + day_of_era - 719_468) * 86_400
    }

    pub fn add_term(self, offer: FoundingOffer) -> Self {
        self.add_terms(offer, 1)
    }

    pub fn add_terms(self, offer: FoundingOffer, terms: i32) -> Self {
        match offer.interval() {
            BillingInterval::Month => {
                let absolute = self.year * 12 + i32::from(self.month) - 1 + terms;
                let year = absolute.div_euclid(12);
                let month = (absolute.rem_euclid(12) + 1) as u8;
                Self {
                    year,
                    month,
                    day: self.day.min(days_in_month(year, month)),
                }
            }
            BillingInterval::Year => {
                let year = self.year + terms;
                Self {
                    year,
                    month: self.month,
                    day: self.day.min(days_in_month(year, self.month)),
                }
            }
        }
    }

    pub fn anchored_term_end(self, offer: FoundingOffer, terms: i32) -> Self {
        let target = self.add_terms(offer, terms);
        Self {
            day: self.day.min(days_in_month(target.year, target.month)),
            ..target
        }
    }
}

impl fmt::Display for FoundingDate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{:04}-{:02}-{:02}",
            self.year, self.month, self.day
        )
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CalendarError {
    #[error("founding date is invalid")]
    InvalidDate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoundingPayerKind {
    Personal,
    Sponsor,
}

impl FoundingPayerKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Personal => "personal",
            Self::Sponsor => "sponsor",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundingAward {
    pub award_id: String,
    pub beneficiary_id: String,
    pub payer_kind: FoundingPayerKind,
    pub payer_id: String,
    pub offer: FoundingOffer,
    pub cohort_ordinal: i64,
    pub original_start: FoundingDate,
    pub original_end: FoundingDate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundingReservation {
    pub reservation_id: String,
    pub operation_id: String,
    pub beneficiary_id: String,
    pub payer_kind: FoundingPayerKind,
    pub payer_id: String,
    pub offer: FoundingOffer,
    pub quote_version: i64,
    pub quote_expires_at_epoch: i64,
    pub reservation_expires_at_epoch: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReservationOutcome {
    Created(FoundingReservation),
    AlreadyExists(FoundingReservation),
    Full,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmationOutcome {
    Awarded(FoundingAward),
    AlreadyAwarded(FoundingAward),
    RefundRequired,
}

#[derive(Debug, Error)]
pub enum FoundingAllocatorError {
    #[error("founding reservation has invalid {0}")]
    InvalidField(&'static str),
    #[error("founding offer is not a founding price")]
    NotFoundingOffer,
    #[error("founding reservation quote is expired")]
    QuoteExpired,
    #[error("founding capacity is full")]
    Full,
    #[error("founding reservation conflicts with an existing request")]
    ReservationConflict,
    #[error("founding payment reference conflicts with an existing payment")]
    PaymentConflict,
    #[error("founding reservation is not available")]
    ReservationMissing,
    #[error("founding reservation cannot be confirmed twice with different evidence")]
    ConfirmationConflict,
    #[error("founding award payer conflicts with stored identity")]
    PayerConflict,
    #[error("founding allocator state is corrupt")]
    CorruptState,
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
}

#[allow(clippy::too_many_arguments)]
pub async fn reserve(
    tx: &mut Transaction<'_, Postgres>,
    reservation_id: &str,
    operation_id: &str,
    beneficiary_id: &str,
    payer_id: &str,
    offer: FoundingOffer,
    quote_version: i64,
    quote_expires_at_epoch: i64,
    now_epoch: i64,
) -> Result<ReservationOutcome, FoundingAllocatorError> {
    reserve_with_payer_kind(
        tx,
        reservation_id,
        operation_id,
        beneficiary_id,
        FoundingPayerKind::Personal,
        payer_id,
        offer,
        quote_version,
        quote_expires_at_epoch,
        now_epoch,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn reserve_with_payer_kind(
    tx: &mut Transaction<'_, Postgres>,
    reservation_id: &str,
    operation_id: &str,
    beneficiary_id: &str,
    payer_kind: FoundingPayerKind,
    payer_id: &str,
    offer: FoundingOffer,
    quote_version: i64,
    quote_expires_at_epoch: i64,
    now_epoch: i64,
) -> Result<ReservationOutcome, FoundingAllocatorError> {
    reserve_with_payer_kind_until(
        tx,
        reservation_id,
        operation_id,
        beneficiary_id,
        payer_kind,
        payer_id,
        offer,
        quote_version,
        quote_expires_at_epoch,
        quote_expires_at_epoch,
        now_epoch,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn reserve_with_payer_kind_until(
    tx: &mut Transaction<'_, Postgres>,
    reservation_id: &str,
    operation_id: &str,
    beneficiary_id: &str,
    payer_kind: FoundingPayerKind,
    payer_id: &str,
    offer: FoundingOffer,
    quote_version: i64,
    quote_expires_at_epoch: i64,
    reservation_expires_at_epoch: i64,
    now_epoch: i64,
) -> Result<ReservationOutcome, FoundingAllocatorError> {
    validate_identity(reservation_id, "reservation_id")?;
    validate_identity(operation_id, "operation_id")?;
    validate_identity(beneficiary_id, "beneficiary_id")?;
    validate_identity(payer_id, "payer_id")?;
    if quote_version < 1 {
        return Err(FoundingAllocatorError::InvalidField("quote_version"));
    }

    lock_capacity(tx).await?;
    if let Some(existing) = load_reservation_by_operation(tx, operation_id).await? {
        if existing.beneficiary_id == beneficiary_id
            && existing.payer_kind == payer_kind
            && existing.payer_id == payer_id
            && existing.offer == offer
            && existing.quote_version == quote_version
            && existing.quote_expires_at_epoch == quote_expires_at_epoch
            && existing.reservation_expires_at_epoch == reservation_expires_at_epoch
        {
            return Ok(ReservationOutcome::AlreadyExists(existing));
        }
        return Err(FoundingAllocatorError::ReservationConflict);
    }
    if quote_expires_at_epoch <= now_epoch {
        return Err(FoundingAllocatorError::QuoteExpired);
    }
    let latest_allowed_expiry = now_epoch
        .checked_add(MAX_RESERVATION_SECONDS)
        .ok_or(FoundingAllocatorError::InvalidField("now_epoch"))?;
    if quote_expires_at_epoch > latest_allowed_expiry {
        return Err(FoundingAllocatorError::InvalidField("quote_expiry"));
    }
    let latest_capacity_hold = now_epoch
        .checked_add(MAX_CAPACITY_HOLD_SECONDS)
        .ok_or(FoundingAllocatorError::InvalidField("now_epoch"))?;
    if reservation_expires_at_epoch < quote_expires_at_epoch
        || reservation_expires_at_epoch > latest_capacity_hold
    {
        return Err(FoundingAllocatorError::InvalidField("reservation_expiry"));
    }
    if load_award_by_beneficiary(tx, beneficiary_id)
        .await?
        .is_some()
    {
        return Err(FoundingAllocatorError::ReservationConflict);
    }
    let active_reservation: Option<String> = sqlx::query_scalar(
        "SELECT reservation_id FROM billing_founding_reservations \
         WHERE beneficiary_id = $1 AND status = 'reserved' AND reservation_expires_at_epoch > $2 \
         FOR UPDATE",
    )
    .bind(beneficiary_id)
    .bind(now_epoch)
    .fetch_optional(&mut **tx)
    .await?;
    if active_reservation.is_some() {
        return Err(FoundingAllocatorError::ReservationConflict);
    }
    // A new reservation does not occupy a place until it is inserted, so zero means all places
    // are already claimed by awards or live reservations.
    if available_places(tx, now_epoch).await? <= 0 {
        return Ok(ReservationOutcome::Full);
    }
    sqlx::query(
        "INSERT INTO billing_founding_reservations \
         (reservation_id, operation_id, beneficiary_id, payer_kind, payer_id, offer, quote_version, \
          quote_expires_at_epoch, reservation_expires_at_epoch) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)",
    )
    .bind(reservation_id)
    .bind(operation_id)
    .bind(beneficiary_id)
    .bind(payer_kind.as_str())
    .bind(payer_id)
    .bind(offer.as_str())
    .bind(quote_version)
    .bind(quote_expires_at_epoch)
    .bind(reservation_expires_at_epoch)
    .execute(&mut **tx)
    .await?;
    Ok(ReservationOutcome::Created(FoundingReservation {
        reservation_id: reservation_id.into(),
        operation_id: operation_id.into(),
        beneficiary_id: beneficiary_id.into(),
        payer_kind,
        payer_id: payer_id.into(),
        offer,
        quote_version,
        quote_expires_at_epoch,
        reservation_expires_at_epoch,
    }))
}

/// Keep a founding place reserved when a verified Checkout Session completed before its
/// reservation deadline but Stripe is still processing a delayed payment method. The original
/// deadline remains the eligibility boundary; only the resulting payment-processing window is
/// extended, and it is bounded by the existing settlement-recovery period.
pub async fn record_pending_checkout_completion(
    tx: &mut Transaction<'_, Postgres>,
    operation_id: &str,
    checkout_completed_by_epoch: i64,
    now_epoch: i64,
) -> Result<bool, FoundingAllocatorError> {
    validate_identity(operation_id, "operation_id")?;
    if checkout_completed_by_epoch <= 0 {
        return Err(FoundingAllocatorError::InvalidField(
            "checkout_completed_by_epoch",
        ));
    }
    if now_epoch <= 0 {
        return Err(FoundingAllocatorError::InvalidField("now_epoch"));
    }

    lock_capacity(tx).await?;
    let row = sqlx::query(
        "SELECT status, reservation_expires_at_epoch, checkout_completed_by_epoch \
         FROM billing_founding_reservations WHERE operation_id = $1 FOR UPDATE",
    )
    .bind(operation_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row else {
        // Standard-price checkouts do not have a founding reservation.
        return Ok(false);
    };

    let status: String = row.try_get("status")?;
    let completed_at: Option<i64> = row.try_get("checkout_completed_by_epoch")?;
    if let Some(completed_at) = completed_at {
        // An async-payment event can arrive first and only proves completion happened before
        // the Checkout Session expired. The later completed event carries the actual timestamp,
        // which may be earlier than that stored upper bound.
        if checkout_completed_by_epoch > completed_at {
            return Err(FoundingAllocatorError::ConfirmationConflict);
        }
        return Ok(true);
    }
    if status != "reserved" {
        return Ok(false);
    }

    let reservation_expires_at_epoch: i64 = row.try_get("reservation_expires_at_epoch")?;
    if checkout_completed_by_epoch >= reservation_expires_at_epoch {
        return Ok(false);
    }
    if reservation_expires_at_epoch <= now_epoch && available_places(tx, now_epoch).await? <= 0 {
        // A delayed webhook must not reclaim a place another checkout reserved after this one
        // expired. Capacity is serialised by the shared ledger lock held above.
        return Ok(false);
    }
    let settlement_deadline = checkout_completed_by_epoch
        .checked_add(SETTLEMENT_RECOVERY_SECONDS)
        .ok_or(FoundingAllocatorError::InvalidField(
            "checkout_completed_by_epoch",
        ))?;
    sqlx::query(
        "UPDATE billing_founding_reservations \
         SET checkout_completed_by_epoch = $2, \
             reservation_expires_at_epoch = GREATEST(reservation_expires_at_epoch, $3), \
             updated_at = now() \
         WHERE operation_id = $1 AND status = 'reserved' \
           AND checkout_completed_by_epoch IS NULL",
    )
    .bind(operation_id)
    .bind(checkout_completed_by_epoch)
    .bind(settlement_deadline)
    .execute(&mut **tx)
    .await?;
    Ok(true)
}

/// Apply the same bounded hold when Stripe delivers the successful delayed-payment event before
/// the separate Checkout-completed event. A completed Session cannot be paid after its own expiry,
/// so its provider-reported expiry must match the reservation's original checkout deadline.
pub async fn record_async_payment_checkout_completion(
    tx: &mut Transaction<'_, Postgres>,
    operation_id: &str,
    session_expires_at_epoch: i64,
    session_is_complete: bool,
    now_epoch: i64,
) -> Result<bool, FoundingAllocatorError> {
    validate_identity(operation_id, "operation_id")?;
    if session_expires_at_epoch <= 1 {
        return Err(FoundingAllocatorError::InvalidField(
            "session_expires_at_epoch",
        ));
    }
    if !session_is_complete {
        return Ok(false);
    }
    if now_epoch <= 0 {
        return Err(FoundingAllocatorError::InvalidField("now_epoch"));
    }

    lock_capacity(tx).await?;
    let row = sqlx::query(
        "SELECT status, quote_expires_at_epoch, reservation_expires_at_epoch, checkout_completed_by_epoch \
         FROM billing_founding_reservations WHERE operation_id = $1 FOR UPDATE",
    )
    .bind(operation_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row else {
        return Ok(false);
    };
    let status: String = row.try_get("status")?;
    let quote_expires_at_epoch: i64 = row.try_get("quote_expires_at_epoch")?;
    let expected_session_expiry = quote_expires_at_epoch
        .checked_add(RESERVATION_SECONDS)
        .ok_or(FoundingAllocatorError::InvalidField(
            "session_expires_at_epoch",
        ))?;
    if expected_session_expiry != session_expires_at_epoch {
        return Ok(false);
    }
    if row
        .try_get::<Option<i64>, _>("checkout_completed_by_epoch")?
        .is_some()
    {
        return Ok(true);
    }
    let reservation_expires_at_epoch: i64 = row.try_get("reservation_expires_at_epoch")?;
    if status != "reserved" || reservation_expires_at_epoch != session_expires_at_epoch {
        return Ok(false);
    }
    if reservation_expires_at_epoch <= now_epoch && available_places(tx, now_epoch).await? <= 0 {
        // This event can arrive after the Checkout deadline. Recover its timely completion only
        // while a founding place remains unclaimed at delivery time.
        return Ok(false);
    }

    let completion_upper_bound = session_expires_at_epoch - 1;
    let settlement_deadline = completion_upper_bound
        .checked_add(SETTLEMENT_RECOVERY_SECONDS)
        .ok_or(FoundingAllocatorError::InvalidField(
            "session_expires_at_epoch",
        ))?;
    sqlx::query(
        "UPDATE billing_founding_reservations \
         SET checkout_completed_by_epoch = $2, \
             reservation_expires_at_epoch = GREATEST(reservation_expires_at_epoch, $3), \
             updated_at = now() \
         WHERE operation_id = $1 AND status = 'reserved' \
           AND checkout_completed_by_epoch IS NULL \
           AND reservation_expires_at_epoch = $4",
    )
    .bind(operation_id)
    .bind(completion_upper_bound)
    .bind(settlement_deadline)
    .bind(session_expires_at_epoch)
    .execute(&mut **tx)
    .await?;
    Ok(true)
}

pub async fn confirm_payment(
    tx: &mut Transaction<'_, Postgres>,
    reservation_id: &str,
    payment_reference: &str,
    paid_on: FoundingDate,
    now_epoch: i64,
) -> Result<ConfirmationOutcome, FoundingAllocatorError> {
    validate_identity(reservation_id, "reservation_id")?;
    validate_identity(payment_reference, "payment_reference")?;
    lock_capacity(tx).await?;
    let row = sqlx::query(
        "SELECT reservation_id, operation_id, beneficiary_id, payer_kind, payer_id, offer, quote_version, \
         quote_expires_at_epoch, reservation_expires_at_epoch, status, award_id, payment_reference \
         FROM billing_founding_reservations WHERE reservation_id = $1 FOR UPDATE",
    )
    .bind(reservation_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(FoundingAllocatorError::ReservationMissing)?;
    let status: String = row.try_get("status")?;
    if status == "confirmed" {
        let stored_payment: Option<String> = row.try_get("payment_reference")?;
        if stored_payment.as_deref() != Some(payment_reference) {
            return Err(FoundingAllocatorError::ConfirmationConflict);
        }
        let award_id: String = row
            .try_get::<Option<String>, _>("award_id")?
            .ok_or(FoundingAllocatorError::CorruptState)?;
        let award = load_award(tx, &award_id)
            .await?
            .ok_or(FoundingAllocatorError::CorruptState)?;
        return Ok(ConfirmationOutcome::AlreadyAwarded(award));
    }
    if status == "refund_required" {
        let stored_payment: String = row
            .try_get::<Option<String>, _>("payment_reference")?
            .ok_or(FoundingAllocatorError::CorruptState)?;
        if stored_payment != payment_reference {
            return Err(FoundingAllocatorError::ConfirmationConflict);
        }
        return Ok(ConfirmationOutcome::RefundRequired);
    }
    if status == "released" {
        let duplicate_payment: Option<String> = sqlx::query_scalar(
            "SELECT reservation_id FROM billing_founding_reservations WHERE payment_reference = $1",
        )
        .bind(payment_reference)
        .fetch_optional(&mut **tx)
        .await?;
        if duplicate_payment.is_some() {
            return Err(FoundingAllocatorError::PaymentConflict);
        }
        sqlx::query(
            "UPDATE billing_founding_reservations SET status = 'refund_required', \
             payment_reference = $2, updated_at = now() WHERE reservation_id = $1 AND status = 'released'",
        )
        .bind(reservation_id)
        .bind(payment_reference)
        .execute(&mut **tx)
        .await?;
        return Ok(ConfirmationOutcome::RefundRequired);
    }
    if status != "reserved" {
        return Err(FoundingAllocatorError::CorruptState);
    }
    let duplicate_payment: Option<String> = sqlx::query_scalar(
        "SELECT reservation_id FROM billing_founding_reservations WHERE payment_reference = $1",
    )
    .bind(payment_reference)
    .fetch_optional(&mut **tx)
    .await?;
    if duplicate_payment.is_some() {
        return Err(FoundingAllocatorError::PaymentConflict);
    }
    let beneficiary_id: String = row.try_get("beneficiary_id")?;
    let payer_kind = parse_payer_kind(&row.try_get::<String, _>("payer_kind")?)?;
    let payer_id: String = row.try_get("payer_id")?;
    if load_award_by_beneficiary(tx, &beneficiary_id)
        .await?
        .is_some()
    {
        sqlx::query(
            "UPDATE billing_founding_reservations SET status = 'refund_required', \
             payment_reference = $2, updated_at = now() \
             WHERE reservation_id = $1",
        )
        .bind(reservation_id)
        .bind(payment_reference)
        .execute(&mut **tx)
        .await?;
        return Ok(ConfirmationOutcome::RefundRequired);
    }
    let awarded: i64 = sqlx::query_scalar("SELECT count(*) FROM billing_founding_awards")
        .fetch_one(&mut **tx)
        .await?;
    let reservation_expires_at_epoch: i64 = row.try_get("reservation_expires_at_epoch")?;
    let remaining = available_places(tx, now_epoch).await?;
    let reservation_is_live = reservation_expires_at_epoch > now_epoch;
    // Once a reservation expires, its founding offer is no longer honoured. Even if capacity
    // happens to remain, accepting the late payment would make a short-lived quote reusable.
    // A live reservation being confirmed itself is included in `remaining`, so zero is valid for
    // that path.
    if awarded >= FOUNDING_CAPACITY || remaining < 0 || !reservation_is_live {
        sqlx::query(
            "UPDATE billing_founding_reservations SET status = 'refund_required', \
             payment_reference = $2, updated_at = now() \
             WHERE reservation_id = $1",
        )
        .bind(reservation_id)
        .bind(payment_reference)
        .execute(&mut **tx)
        .await?;
        return Ok(ConfirmationOutcome::RefundRequired);
    }
    let offer = parse_offer(&row.try_get::<String, _>("offer")?)?;
    let ordinal: i64 = sqlx::query_scalar(
        "SELECT COALESCE(max(cohort_ordinal), 0) + 1 FROM billing_founding_awards",
    )
    .fetch_one(&mut **tx)
    .await?;
    if ordinal > FOUNDING_CAPACITY {
        return Err(FoundingAllocatorError::CorruptState);
    }
    let award_id = format!("founding:{}", Uuid::new_v4());
    let end = paid_on.anchored_term_end(offer, 1);
    sqlx::query(
        "INSERT INTO billing_founding_awards \
         (award_id, beneficiary_id, payer_kind, payer_id, offer, cohort_ordinal, original_start_date, original_end_date) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8)",
    )
    .bind(&award_id)
    .bind(&beneficiary_id)
    .bind(payer_kind.as_str())
    .bind(&payer_id)
    .bind(offer.as_str())
    .bind(ordinal)
    .bind(paid_on.to_string())
    .bind(end.to_string())
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "UPDATE billing_founding_reservations SET status = 'confirmed', award_id = $2, \
         payment_reference = $3, updated_at = now() WHERE reservation_id = $1",
    )
    .bind(reservation_id)
    .bind(&award_id)
    .bind(payment_reference)
    .execute(&mut **tx)
    .await?;
    let award = FoundingAward {
        award_id,
        beneficiary_id,
        payer_kind,
        payer_id,
        offer,
        cohort_ordinal: ordinal,
        original_start: paid_on,
        original_end: end,
    };
    Ok(ConfirmationOutcome::Awarded(award))
}

pub async fn transfer_payer(
    tx: &mut Transaction<'_, Postgres>,
    award_id: &str,
    new_payer_id: &str,
) -> Result<FoundingAward, FoundingAllocatorError> {
    let current = load_award(tx, award_id)
        .await?
        .ok_or(FoundingAllocatorError::ReservationMissing)?;
    transfer_payer_with_kind(
        tx,
        award_id,
        current.payer_kind,
        &current.payer_id,
        FoundingPayerKind::Personal,
        new_payer_id,
    )
    .await
}

pub async fn transfer_payer_with_kind(
    tx: &mut Transaction<'_, Postgres>,
    award_id: &str,
    expected_payer_kind: FoundingPayerKind,
    expected_payer_id: &str,
    new_payer_kind: FoundingPayerKind,
    new_payer_id: &str,
) -> Result<FoundingAward, FoundingAllocatorError> {
    validate_identity(award_id, "award_id")?;
    validate_identity(expected_payer_id, "payer_id")?;
    validate_identity(new_payer_id, "payer_id")?;
    let updated = sqlx::query(
        "UPDATE billing_founding_awards SET payer_kind = $4, payer_id = $5 \
         WHERE award_id = $1 AND payer_kind = $2 AND payer_id = $3",
    )
    .bind(award_id)
    .bind(expected_payer_kind.as_str())
    .bind(expected_payer_id)
    .bind(new_payer_kind.as_str())
    .bind(new_payer_id)
    .execute(&mut **tx)
    .await?;
    let award = load_award(tx, award_id)
        .await?
        .ok_or(FoundingAllocatorError::ReservationMissing)?;
    if updated.rows_affected() == 0
        && (award.payer_kind != new_payer_kind || award.payer_id != new_payer_id)
    {
        return Err(FoundingAllocatorError::PayerConflict);
    }
    Ok(award)
}

/// Return the public capacity and standard-price context used by a founding quote.
///
/// The caller owns the transaction so a quote can read this status and reserve a place in the
/// same transaction when it needs a stable view.
pub async fn quote_status(
    tx: &mut Transaction<'_, Postgres>,
    offer: FoundingOffer,
    now_epoch: i64,
) -> Result<FoundingQuoteStatus, FoundingAllocatorError> {
    let capacity = load_capacity_status(tx, now_epoch).await?;
    let standard_amount_pence = match offer {
        FoundingOffer::Monthly => BillingOffer::StandardMonthly.expected_amount_pence(),
        FoundingOffer::Annual => BillingOffer::StandardAnnual.expected_amount_pence(),
    };
    Ok(FoundingQuoteStatus {
        offer,
        remaining_places: capacity.remaining_places,
        founding_amount_pence: offer.amount_pence(),
        standard_amount_pence,
    })
}

/// Select the canonical price for one named person from the shared founding capacity.
///
/// An unawarded person receives the founding price while a place is available. A permanent award
/// prevents a later checkout from claiming a second founding term. Live reservations are not
/// duplicated by a second quote for the same beneficiary.
pub async fn select_offer(
    tx: &mut Transaction<'_, Postgres>,
    beneficiary_id: &str,
    requested_interval: FoundingOffer,
    operation_id: Option<&str>,
    now_epoch: i64,
) -> Result<BillingOffer, FoundingAllocatorError> {
    validate_identity(beneficiary_id, "beneficiary_id")?;
    lock_capacity(tx).await?;

    if load_award_by_beneficiary(tx, beneficiary_id)
        .await?
        .is_some()
    {
        return Ok(requested_interval.standard_billing_offer());
    }

    let active_reservation: Option<(String, String)> = sqlx::query_as(
        "SELECT operation_id, offer FROM billing_founding_reservations \
         WHERE beneficiary_id = $1 AND status = 'reserved' \
           AND reservation_expires_at_epoch > $2 FOR UPDATE",
    )
    .bind(beneficiary_id)
    .bind(now_epoch)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some((reserved_operation_id, reserved_offer)) = active_reservation {
        if operation_id == Some(reserved_operation_id.as_str()) {
            let offer = parse_offer(&reserved_offer)?;
            if offer != requested_interval {
                return Err(FoundingAllocatorError::ReservationConflict);
            }
            return Ok(offer.billing_offer());
        }
        return Ok(requested_interval.standard_billing_offer());
    }

    if available_places(tx, now_epoch).await? > 0 {
        Ok(requested_interval.billing_offer())
    } else {
        Ok(requested_interval.standard_billing_offer())
    }
}

pub async fn load_capacity_status(
    tx: &mut Transaction<'_, Postgres>,
    now_epoch: i64,
) -> Result<FoundingCapacityStatus, FoundingAllocatorError> {
    lock_capacity(tx).await?;
    let confirmed_awards: i64 = sqlx::query_scalar("SELECT count(*) FROM billing_founding_awards")
        .fetch_one(&mut **tx)
        .await?;
    let active_reservations: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM billing_founding_reservations \
         WHERE status = 'reserved' AND reservation_expires_at_epoch > $1",
    )
    .bind(now_epoch)
    .fetch_one(&mut **tx)
    .await?;
    Ok(FoundingCapacityStatus {
        confirmed_awards,
        active_reservations,
        remaining_places: FOUNDING_CAPACITY - confirmed_awards - active_reservations,
    })
}

/// Release an unused reservation after a definitive checkout failure or expiry.
///
/// Confirmed and refund-required records are immutable. Releasing an absent or already-released
/// reservation is harmless so provider failure cleanup can be retried transactionally.
pub async fn release_reservation(
    tx: &mut Transaction<'_, Postgres>,
    operation_id: &str,
) -> Result<(), FoundingAllocatorError> {
    validate_identity(operation_id, "operation_id")?;
    lock_capacity(tx).await?;
    sqlx::query(
        "UPDATE billing_founding_reservations SET status = 'released', updated_at = now() \
         WHERE operation_id = $1 AND status = 'reserved'",
    )
    .bind(operation_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub async fn load_award(
    tx: &mut Transaction<'_, Postgres>,
    award_id: &str,
) -> Result<Option<FoundingAward>, FoundingAllocatorError> {
    let row = sqlx::query(
        "SELECT award_id, beneficiary_id, payer_kind, payer_id, offer, cohort_ordinal, original_start_date, \
         original_end_date FROM billing_founding_awards WHERE award_id = $1",
    )
    .bind(award_id)
    .fetch_optional(&mut **tx)
    .await?;
    row.map(award_from_row).transpose()
}

/// Acquire the shared founding ledger lock before touching personal or sponsored billing rows.
/// Checkout and webhook settlement paths take this first so they cannot deadlock while acquiring
/// the ledger alongside account, operation, or subscription locks.
pub async fn lock_capacity(
    tx: &mut Transaction<'_, Postgres>,
) -> Result<(), FoundingAllocatorError> {
    sqlx::query(
        "SELECT singleton FROM billing_founding_capacity WHERE singleton = TRUE FOR UPDATE",
    )
    .fetch_one(&mut **tx)
    .await?;
    Ok(())
}

async fn available_places(
    tx: &mut Transaction<'_, Postgres>,
    now_epoch: i64,
) -> Result<i64, FoundingAllocatorError> {
    let awarded: i64 = sqlx::query_scalar("SELECT count(*) FROM billing_founding_awards")
        .fetch_one(&mut **tx)
        .await?;
    let reserved: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM billing_founding_reservations \
         WHERE status = 'reserved' AND reservation_expires_at_epoch > $1",
    )
    .bind(now_epoch)
    .fetch_one(&mut **tx)
    .await?;
    Ok(FOUNDING_CAPACITY - awarded - reserved)
}

async fn load_reservation_by_operation(
    tx: &mut Transaction<'_, Postgres>,
    operation_id: &str,
) -> Result<Option<FoundingReservation>, FoundingAllocatorError> {
    let row = sqlx::query(
        "SELECT reservation_id, operation_id, beneficiary_id, payer_kind, payer_id, offer, quote_version, \
         quote_expires_at_epoch, reservation_expires_at_epoch \
         FROM billing_founding_reservations WHERE operation_id = $1 FOR UPDATE",
    )
    .bind(operation_id)
    .fetch_optional(&mut **tx)
    .await?;
    row.map(reservation_from_row).transpose()
}

async fn load_award_by_beneficiary(
    tx: &mut Transaction<'_, Postgres>,
    beneficiary_id: &str,
) -> Result<Option<FoundingAward>, FoundingAllocatorError> {
    let row = sqlx::query(
        "SELECT award_id, beneficiary_id, payer_kind, payer_id, offer, cohort_ordinal, original_start_date, \
         original_end_date FROM billing_founding_awards WHERE beneficiary_id = $1",
    )
    .bind(beneficiary_id)
    .fetch_optional(&mut **tx)
    .await?;
    row.map(award_from_row).transpose()
}

fn reservation_from_row(
    row: sqlx::postgres::PgRow,
) -> Result<FoundingReservation, FoundingAllocatorError> {
    Ok(FoundingReservation {
        reservation_id: row.try_get("reservation_id")?,
        operation_id: row.try_get("operation_id")?,
        beneficiary_id: row.try_get("beneficiary_id")?,
        payer_kind: parse_payer_kind(&row.try_get::<String, _>("payer_kind")?)?,
        payer_id: row.try_get("payer_id")?,
        offer: parse_offer(&row.try_get::<String, _>("offer")?)?,
        quote_version: row.try_get("quote_version")?,
        quote_expires_at_epoch: row.try_get("quote_expires_at_epoch")?,
        reservation_expires_at_epoch: row.try_get("reservation_expires_at_epoch")?,
    })
}

fn award_from_row(row: sqlx::postgres::PgRow) -> Result<FoundingAward, FoundingAllocatorError> {
    Ok(FoundingAward {
        award_id: row.try_get("award_id")?,
        beneficiary_id: row.try_get("beneficiary_id")?,
        payer_kind: parse_payer_kind(&row.try_get::<String, _>("payer_kind")?)?,
        payer_id: row.try_get("payer_id")?,
        offer: parse_offer(&row.try_get::<String, _>("offer")?)?,
        cohort_ordinal: row.try_get("cohort_ordinal")?,
        original_start: FoundingDate::parse(&row.try_get::<String, _>("original_start_date")?)
            .map_err(|_| FoundingAllocatorError::CorruptState)?,
        original_end: FoundingDate::parse(&row.try_get::<String, _>("original_end_date")?)
            .map_err(|_| FoundingAllocatorError::CorruptState)?,
    })
}

fn parse_offer(value: &str) -> Result<FoundingOffer, FoundingAllocatorError> {
    match value {
        "founding_monthly" => Ok(FoundingOffer::Monthly),
        "founding_annual" => Ok(FoundingOffer::Annual),
        _ => Err(FoundingAllocatorError::CorruptState),
    }
}

fn parse_payer_kind(value: &str) -> Result<FoundingPayerKind, FoundingAllocatorError> {
    match value {
        "personal" => Ok(FoundingPayerKind::Personal),
        "sponsor" => Ok(FoundingPayerKind::Sponsor),
        _ => Err(FoundingAllocatorError::CorruptState),
    }
}

fn validate_identity(value: &str, field: &'static str) -> Result<(), FoundingAllocatorError> {
    if value.trim().is_empty() {
        return Err(FoundingAllocatorError::InvalidField(field));
    }
    Ok(())
}

const fn is_leap_year(year: i32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

const fn days_in_month(year: i32, month: u8) -> u8 {
    match month {
        2 if is_leap_year(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn month_end_and_leap_day_keep_the_original_anchor() {
        let january = FoundingDate::new(2026, 1, 31).unwrap();
        assert_eq!(
            january.add_term(FoundingOffer::Monthly).to_string(),
            "2026-02-28"
        );
        assert_eq!(
            january
                .anchored_term_end(FoundingOffer::Monthly, 2)
                .to_string(),
            "2026-03-31"
        );
        let leap = FoundingDate::new(2024, 2, 29).unwrap();
        assert_eq!(
            leap.add_term(FoundingOffer::Annual).to_string(),
            "2025-02-28"
        );
        assert_eq!(
            leap.add_terms(FoundingOffer::Annual, 4).to_string(),
            "2028-02-29"
        );
        assert_eq!(
            FoundingDate::from_unix_seconds(0).unwrap().to_string(),
            "1970-01-01"
        );
        assert_eq!(
            FoundingDate::from_unix_seconds(leap.to_unix_seconds()).unwrap(),
            leap
        );
    }

    #[test]
    fn only_founding_offers_can_enter_the_allocator() {
        assert_eq!(
            FoundingOffer::from_billing_offer(BillingOffer::FoundingMonthly),
            Some(FoundingOffer::Monthly)
        );
        assert_eq!(
            FoundingOffer::from_billing_offer(BillingOffer::StandardMonthly),
            None
        );
    }
}
