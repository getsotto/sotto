//! Durable state for a person's hosted Cloud subscription.
//!
//! This table is deliberately separate from organisation tiers. A personal checkout can be
//! pending while Stripe is still collecting payment, and a paid term remains readable after a
//! cancellation request. The module owns only database state; provider verification stays in the
//! billing webhook path.

use sqlx::{Postgres, Row, Transaction};
use thiserror::Error;

use crate::billing_catalogue::BillingOffer;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersonalBillingState {
    Pending,
    Active,
    PastDue,
    Unpaid,
    Canceled,
    RefundRequired,
}

impl PersonalBillingState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Active => "active",
            Self::PastDue => "past_due",
            Self::Unpaid => "unpaid",
            Self::Canceled => "canceled",
            Self::RefundRequired => "refund_required",
        }
    }

    fn parse(value: &str) -> Result<Self, PersonalBillingError> {
        match value {
            "pending" => Ok(Self::Pending),
            "active" => Ok(Self::Active),
            "past_due" => Ok(Self::PastDue),
            "unpaid" => Ok(Self::Unpaid),
            "canceled" => Ok(Self::Canceled),
            "refund_required" => Ok(Self::RefundRequired),
            _ => Err(PersonalBillingError::CorruptState),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersonalBillingAccount {
    pub user_id: String,
    pub operation_id: String,
    pub offer: BillingOffer,
    pub stripe_customer_id: Option<String>,
    pub stripe_subscription_id: Option<String>,
    pub state: PersonalBillingState,
    pub pending_expires_at_epoch: i64,
    pub paid_through_epoch: Option<i64>,
    pub paid_through_date: Option<String>,
    pub payment_reference: Option<String>,
    pub cancel_at_period_end: bool,
}

#[derive(Debug, Error)]
pub enum PersonalBillingError {
    #[error("personal billing account already exists")]
    AccountExists,
    #[error("personal billing account is corrupt")]
    CorruptState,
    #[error("personal settlement conflicts with stored evidence")]
    SettlementConflict,
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettlementDisposition {
    Applied,
    AlreadyApplied,
}

/// Reserve the one personal billing account row for an operation. Replaying the same operation is
/// idempotent; a different operation cannot create a second personal subscription.
pub async fn begin_account(
    tx: &mut Transaction<'_, Postgres>,
    user_id: &str,
    operation_id: &str,
    offer: BillingOffer,
    pending_expires_at_epoch: i64,
    now_epoch: i64,
) -> Result<(), PersonalBillingError> {
    let existing = sqlx::query(
        "SELECT operation_id, offer, state, pending_expires_at_epoch \
         FROM billing_personal_accounts WHERE user_id = $1 FOR UPDATE",
    )
    .bind(user_id)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some(row) = existing {
        let existing_operation: String = row.try_get("operation_id")?;
        let existing_offer: String = row.try_get("offer")?;
        let state: String = row.try_get("state")?;
        let same_pending = existing_operation == operation_id
            && existing_offer == offer.as_str()
            && state == PersonalBillingState::Pending.as_str();
        if same_pending {
            return Ok(());
        }
        let existing_pending_expires: i64 = row.try_get("pending_expires_at_epoch")?;
        let reusable = state == PersonalBillingState::Canceled.as_str()
            || (state == PersonalBillingState::Pending.as_str()
                && existing_pending_expires <= now_epoch);
        if reusable {
            sqlx::query(
                "UPDATE billing_personal_accounts SET operation_id = $2, offer = $3, \
                 stripe_customer_id = NULL, stripe_subscription_id = NULL, state = 'pending', \
                 pending_expires_at_epoch = $4, paid_through_epoch = NULL, \
                 paid_through_date = NULL, payment_reference = NULL, cancel_at_period_end = FALSE, \
                 cancellation_requested_at = NULL, updated_at = now() WHERE user_id = $1",
            )
            .bind(user_id)
            .bind(operation_id)
            .bind(offer.as_str())
            .bind(pending_expires_at_epoch)
            .execute(&mut **tx)
            .await?;
            return Ok(());
        }
        return Err(PersonalBillingError::AccountExists);
    }
    sqlx::query(
        "INSERT INTO billing_personal_accounts \
         (user_id, operation_id, offer, pending_expires_at_epoch) VALUES ($1, $2, $3, $4)",
    )
    .bind(user_id)
    .bind(operation_id)
    .bind(offer.as_str())
    .bind(pending_expires_at_epoch)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub async fn load_account(
    tx: &mut Transaction<'_, Postgres>,
    user_id: &str,
) -> Result<Option<PersonalBillingAccount>, PersonalBillingError> {
    let row = sqlx::query(
        "SELECT user_id, operation_id, offer, stripe_customer_id, stripe_subscription_id, \
                state, pending_expires_at_epoch, paid_through_epoch, paid_through_date, payment_reference, \
                cancel_at_period_end \
         FROM billing_personal_accounts WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_optional(&mut **tx)
    .await?;
    row.map(account_from_row).transpose()
}

pub async fn record_checkout_url(
    tx: &mut Transaction<'_, Postgres>,
    operation_id: &str,
    checkout_url: &str,
) -> Result<(), PersonalBillingError> {
    sqlx::query(
        "UPDATE billing_operations SET provider_checkout_url = $2, updated_at = now() \
         WHERE operation_id = $1",
    )
    .bind(operation_id)
    .bind(checkout_url)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub async fn record_event(
    tx: &mut Transaction<'_, Postgres>,
    user_id: &str,
    operation_id: &str,
    action: &str,
    detail: Option<&str>,
) -> Result<(), PersonalBillingError> {
    sqlx::query(
        "INSERT INTO billing_personal_events (user_id, operation_id, action, detail) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(user_id)
    .bind(operation_id)
    .bind(action)
    .bind(detail)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub async fn record_paid_settlement(
    tx: &mut Transaction<'_, Postgres>,
    operation_id: &str,
    customer_id: &str,
    subscription_id: &str,
    payment_reference: &str,
    paid_through_epoch: i64,
    paid_through_date: &str,
) -> Result<SettlementDisposition, PersonalBillingError> {
    let updated = sqlx::query(
        "UPDATE billing_personal_accounts SET state = 'active', stripe_customer_id = $2, \
         stripe_subscription_id = $3, payment_reference = $4, paid_through_epoch = $5, \
         paid_through_date = $6, cancel_at_period_end = FALSE, updated_at = now() \
         WHERE operation_id = $1 AND state = 'pending' RETURNING user_id",
    )
    .bind(operation_id)
    .bind(customer_id)
    .bind(subscription_id)
    .bind(payment_reference)
    .bind(paid_through_epoch)
    .bind(paid_through_date)
    .fetch_optional(&mut **tx)
    .await?;
    if updated.is_some() {
        return Ok(SettlementDisposition::Applied);
    }
    let existing = sqlx::query(
        "SELECT stripe_customer_id, stripe_subscription_id, payment_reference, \
                paid_through_epoch, paid_through_date, state \
         FROM billing_personal_accounts WHERE operation_id = $1",
    )
    .bind(operation_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(PersonalBillingError::CorruptState)?;
    let same = existing.try_get::<String, _>("state")? == "active"
        && existing
            .try_get::<Option<String>, _>("stripe_customer_id")?
            .as_deref()
            == Some(customer_id)
        && existing
            .try_get::<Option<String>, _>("stripe_subscription_id")?
            .as_deref()
            == Some(subscription_id)
        && existing
            .try_get::<Option<String>, _>("payment_reference")?
            .as_deref()
            == Some(payment_reference)
        && existing.try_get::<Option<i64>, _>("paid_through_epoch")? == Some(paid_through_epoch)
        && existing
            .try_get::<Option<String>, _>("paid_through_date")?
            .as_deref()
            == Some(paid_through_date);
    if same {
        Ok(SettlementDisposition::AlreadyApplied)
    } else {
        Err(PersonalBillingError::SettlementConflict)
    }
}

pub async fn record_refund_required(
    tx: &mut Transaction<'_, Postgres>,
    operation_id: &str,
    customer_id: &str,
    subscription_id: &str,
    payment_reference: &str,
) -> Result<SettlementDisposition, PersonalBillingError> {
    let updated = sqlx::query(
        "UPDATE billing_personal_accounts SET state = 'refund_required', \
         stripe_customer_id = $2, stripe_subscription_id = $3, payment_reference = $4, \
         updated_at = now() WHERE operation_id = $1 AND state = 'pending' RETURNING user_id",
    )
    .bind(operation_id)
    .bind(customer_id)
    .bind(subscription_id)
    .bind(payment_reference)
    .fetch_optional(&mut **tx)
    .await?;
    if updated.is_some() {
        return Ok(SettlementDisposition::Applied);
    }
    let existing = sqlx::query(
        "SELECT state, stripe_customer_id, stripe_subscription_id, payment_reference \
         FROM billing_personal_accounts WHERE operation_id = $1",
    )
    .bind(operation_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(PersonalBillingError::CorruptState)?;
    let same = existing.try_get::<String, _>("state")? == "refund_required"
        && existing
            .try_get::<Option<String>, _>("stripe_customer_id")?
            .as_deref()
            == Some(customer_id)
        && existing
            .try_get::<Option<String>, _>("stripe_subscription_id")?
            .as_deref()
            == Some(subscription_id)
        && existing
            .try_get::<Option<String>, _>("payment_reference")?
            .as_deref()
            == Some(payment_reference);
    if same {
        Ok(SettlementDisposition::AlreadyApplied)
    } else {
        Err(PersonalBillingError::SettlementConflict)
    }
}

pub async fn advance_paid_through(
    tx: &mut Transaction<'_, Postgres>,
    user_id: &str,
    subscription_id: &str,
    paid_through_epoch: i64,
    paid_through_date: &str,
) -> Result<(), PersonalBillingError> {
    sqlx::query(
        "UPDATE billing_personal_accounts SET paid_through_epoch = $3, paid_through_date = $4, \
         updated_at = now() WHERE user_id = $1 AND stripe_subscription_id = $2 \
         AND state <> 'pending' AND (paid_through_epoch IS NULL OR paid_through_epoch < $3)",
    )
    .bind(user_id)
    .bind(subscription_id)
    .bind(paid_through_epoch)
    .bind(paid_through_date)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub async fn record_invoice_paid(
    tx: &mut Transaction<'_, Postgres>,
    subscription_id: &str,
    payment_reference: &str,
    paid_through_epoch: i64,
    paid_through_date: &str,
) -> Result<(), PersonalBillingError> {
    sqlx::query(
        "UPDATE billing_personal_accounts SET state = CASE WHEN state = 'past_due' OR state = 'unpaid' \
             THEN 'active' ELSE state END, payment_reference = $2, \
             paid_through_epoch = GREATEST(COALESCE(paid_through_epoch, 0), $3), \
             paid_through_date = CASE WHEN COALESCE(paid_through_epoch, 0) < $3 \
                 THEN $4 ELSE paid_through_date END, updated_at = now() \
         WHERE stripe_subscription_id = $1 AND state <> 'pending'",
    )
    .bind(subscription_id)
    .bind(payment_reference)
    .bind(paid_through_epoch)
    .bind(paid_through_date)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn account_from_row(
    row: sqlx::postgres::PgRow,
) -> Result<PersonalBillingAccount, PersonalBillingError> {
    let offer = match row.try_get::<String, _>("offer")?.as_str() {
        "standard_monthly" => BillingOffer::StandardMonthly,
        "standard_annual" => BillingOffer::StandardAnnual,
        "founding_monthly" => BillingOffer::FoundingMonthly,
        "founding_annual" => BillingOffer::FoundingAnnual,
        _ => return Err(PersonalBillingError::CorruptState),
    };
    Ok(PersonalBillingAccount {
        user_id: row.try_get("user_id")?,
        operation_id: row.try_get("operation_id")?,
        offer,
        stripe_customer_id: row.try_get("stripe_customer_id")?,
        stripe_subscription_id: row.try_get("stripe_subscription_id")?,
        state: PersonalBillingState::parse(&row.try_get::<String, _>("state")?)?,
        pending_expires_at_epoch: row.try_get("pending_expires_at_epoch")?,
        paid_through_epoch: row.try_get("paid_through_epoch")?,
        paid_through_date: row.try_get("paid_through_date")?,
        payment_reference: row.try_get("payment_reference")?,
        cancel_at_period_end: row.try_get("cancel_at_period_end")?,
    })
}
