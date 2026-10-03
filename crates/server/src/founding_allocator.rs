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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundingAward {
    pub award_id: String,
    pub beneficiary_id: String,
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
    pub payer_id: String,
    pub offer: FoundingOffer,
    pub quote_version: i64,
    pub quote_expires_at_epoch: i64,
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
            && existing.payer_id == payer_id
            && existing.offer == offer
            && existing.quote_version == quote_version
            && existing.quote_expires_at_epoch == quote_expires_at_epoch
        {
            return Ok(ReservationOutcome::AlreadyExists(existing));
        }
        return Err(FoundingAllocatorError::ReservationConflict);
    }
    if quote_expires_at_epoch <= now_epoch {
        return Err(FoundingAllocatorError::QuoteExpired);
    }
    let latest_allowed_expiry = now_epoch
        .checked_add(RESERVATION_SECONDS)
        .ok_or(FoundingAllocatorError::InvalidField("now_epoch"))?;
    if quote_expires_at_epoch > latest_allowed_expiry {
        return Err(FoundingAllocatorError::InvalidField("quote_expiry"));
    }
    if load_award_by_beneficiary(tx, beneficiary_id)
        .await?
        .is_some()
    {
        return Err(FoundingAllocatorError::ReservationConflict);
    }
    let active_reservation: Option<String> = sqlx::query_scalar(
        "SELECT reservation_id FROM billing_founding_reservations \
         WHERE beneficiary_id = $1 AND status = 'reserved' AND quote_expires_at_epoch > $2 \
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
         (reservation_id, operation_id, beneficiary_id, payer_id, offer, quote_version, \
          quote_expires_at_epoch) VALUES ($1,$2,$3,$4,$5,$6,$7)",
    )
    .bind(reservation_id)
    .bind(operation_id)
    .bind(beneficiary_id)
    .bind(payer_id)
    .bind(offer.as_str())
    .bind(quote_version)
    .bind(quote_expires_at_epoch)
    .execute(&mut **tx)
    .await?;
    Ok(ReservationOutcome::Created(FoundingReservation {
        reservation_id: reservation_id.into(),
        operation_id: operation_id.into(),
        beneficiary_id: beneficiary_id.into(),
        payer_id: payer_id.into(),
        offer,
        quote_version,
        quote_expires_at_epoch,
    }))
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
        "SELECT reservation_id, operation_id, beneficiary_id, payer_id, offer, quote_version, \
         quote_expires_at_epoch, status, award_id, payment_reference \
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
    let reservation_expires_at_epoch: i64 = row.try_get("quote_expires_at_epoch")?;
    let remaining = available_places(tx, now_epoch).await?;
    let reservation_is_live = reservation_expires_at_epoch > now_epoch;
    // An expired reservation is no longer included in available_places. If all places have
    // since been awarded, or the last place is held by another live reservation, a late payment
    // must be refunded rather than taking a place that was already promised to someone else.
    // A live reservation being confirmed itself is included in `remaining`, so zero is valid for
    // that path.
    if awarded >= FOUNDING_CAPACITY || remaining < 0 || (!reservation_is_live && remaining == 0) {
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
         (award_id, beneficiary_id, payer_id, offer, cohort_ordinal, original_start_date, original_end_date) \
         VALUES ($1,$2,$3,$4,$5,$6,$7)",
    )
    .bind(&award_id)
    .bind(&beneficiary_id)
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
    validate_identity(award_id, "award_id")?;
    validate_identity(new_payer_id, "payer_id")?;
    sqlx::query("UPDATE billing_founding_awards SET payer_id = $2 WHERE award_id = $1")
        .bind(award_id)
        .bind(new_payer_id)
        .execute(&mut **tx)
        .await?;
    load_award(tx, award_id)
        .await?
        .ok_or(FoundingAllocatorError::ReservationMissing)
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
         WHERE status = 'reserved' AND quote_expires_at_epoch > $1",
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

pub async fn load_award(
    tx: &mut Transaction<'_, Postgres>,
    award_id: &str,
) -> Result<Option<FoundingAward>, FoundingAllocatorError> {
    let row = sqlx::query(
        "SELECT award_id, beneficiary_id, payer_id, offer, cohort_ordinal, original_start_date, \
         original_end_date FROM billing_founding_awards WHERE award_id = $1",
    )
    .bind(award_id)
    .fetch_optional(&mut **tx)
    .await?;
    row.map(award_from_row).transpose()
}

async fn lock_capacity(tx: &mut Transaction<'_, Postgres>) -> Result<(), FoundingAllocatorError> {
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
         WHERE status = 'reserved' AND quote_expires_at_epoch > $1",
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
        "SELECT reservation_id, operation_id, beneficiary_id, payer_id, offer, quote_version, \
         quote_expires_at_epoch FROM billing_founding_reservations WHERE operation_id = $1 FOR UPDATE",
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
        "SELECT award_id, beneficiary_id, payer_id, offer, cohort_ordinal, original_start_date, \
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
        payer_id: row.try_get("payer_id")?,
        offer: parse_offer(&row.try_get::<String, _>("offer")?)?,
        quote_version: row.try_get("quote_version")?,
        quote_expires_at_epoch: row.try_get("quote_expires_at_epoch")?,
    })
}

fn award_from_row(row: sqlx::postgres::PgRow) -> Result<FoundingAward, FoundingAllocatorError> {
    Ok(FoundingAward {
        award_id: row.try_get("award_id")?,
        beneficiary_id: row.try_get("beneficiary_id")?,
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
