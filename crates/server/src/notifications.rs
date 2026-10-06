//! Durable, redacted lifecycle notices.
//!
//! Notice identity is separate from delivery.  A caller records one `(recipient, event, policy,
//! channel)` intent and may safely retry that write; the unique identity prevents a provider
//! callback or worker restart from resetting the lifecycle dates or creating a duplicate message.
//! In-app notices are available without unlocking the vault. Email delivery is deliberately
//! adapter-driven and can only use a contact that was verified separately.

use std::time::Duration;

use async_trait::async_trait;
use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Row, Transaction};
use thiserror::Error;
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::error::{Error, Result};
use crate::state::AppState;

const MAX_EVENT_KEY: usize = 160;
const MAX_TEXT: usize = 240;
const MAX_DETAIL: usize = 2_000;
const MAX_NOTICES: i64 = 100;
pub const STANDARD_MONTHLY_PRICE_NOTICE_LEAD_SECONDS: i64 = 60 * 24 * 60 * 60;
type NoticeResult<T> = std::result::Result<T, NoticeError>;

const RETRY_DELAYS: [Duration; 5] = [
    Duration::from_secs(60),
    Duration::from_secs(5 * 60),
    Duration::from_secs(30 * 60),
    Duration::from_secs(2 * 60 * 60),
    Duration::from_secs(24 * 60 * 60),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeKind {
    CheckoutOutcome,
    FailedRenewal,
    RecoveryEnd,
    ExportDeadline,
    ImpendingPurge,
    SponsorshipChange,
    FoundingExpiry,
    PriceChange,
}

impl NoticeKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CheckoutOutcome => "checkout_outcome",
            Self::FailedRenewal => "failed_renewal",
            Self::RecoveryEnd => "recovery_end",
            Self::ExportDeadline => "export_deadline",
            Self::ImpendingPurge => "impending_purge",
            Self::SponsorshipChange => "sponsorship_change",
            Self::FoundingExpiry => "founding_expiry",
            Self::PriceChange => "price_change",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeChannel {
    InApp,
    Email,
}

impl NoticeChannel {
    const fn as_str(self) -> &'static str {
        match self {
            Self::InApp => "in_app",
            Self::Email => "email",
        }
    }
}

/// Safe, bounded content for a notice.  There is intentionally no free-form metadata map: secret
/// names, ciphertext, provider payloads and high-cardinality labels cannot enter the outbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoticeContent {
    pub title: String,
    pub detail: String,
    pub effective_at_epoch: Option<i64>,
    pub deadline_epoch: Option<i64>,
    pub amount_pence: Option<i64>,
}

impl NoticeContent {
    pub fn validate(&self) -> NoticeResult<()> {
        bounded_text(&self.title, MAX_TEXT, "title")?;
        bounded_text(&self.detail, MAX_DETAIL, "detail")?;
        if self.amount_pence.is_some_and(|amount| amount < 0) {
            return Err(NoticeError::InvalidContent(
                "amount_pence must not be negative",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoticeIntent {
    pub recipient_user_id: String,
    pub event_key: String,
    pub policy_key: String,
    pub kind: NoticeKind,
    pub channel: NoticeChannel,
    pub contact_id: Option<String>,
    pub due_at_epoch: i64,
    pub content: NoticeContent,
}

impl NoticeIntent {
    pub fn validate(&self) -> NoticeResult<()> {
        bounded_key(&self.recipient_user_id, "recipient_user_id")?;
        bounded_key(&self.event_key, "event_key")?;
        bounded_key(&self.policy_key, "policy_key")?;
        if self.due_at_epoch <= 0 {
            return Err(NoticeError::InvalidContent("due_at_epoch must be positive"));
        }
        if self.channel == NoticeChannel::InApp && self.contact_id.is_some() {
            return Err(NoticeError::InvalidContent(
                "in-app notices must not carry a contact",
            ));
        }
        if self.channel == NoticeChannel::Email && self.contact_id.is_none() {
            return Err(NoticeError::InvalidContent(
                "email notices require a verified contact",
            ));
        }
        self.content.validate()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueOutcome {
    Enqueued,
    AlreadyQueued,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NoticeView {
    pub notice_id: String,
    pub kind: String,
    pub channel: String,
    pub content: NoticeContent,
    pub due_at_epoch: i64,
    pub status: String,
    pub last_error_code: Option<String>,
    pub delivered_at_epoch: Option<i64>,
    pub created_at_epoch: i64,
}

#[derive(Debug, Error)]
pub enum NoticeError {
    #[error("notification database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("notification content is invalid: {0}")]
    InvalidContent(&'static str),
    #[error("notification key is invalid: {0}")]
    InvalidKey(&'static str),
    #[error("notification worker id is empty")]
    EmptyWorkerId,
    #[error("notification lease was lost")]
    LeaseLost,
    #[error("notification contact is unavailable")]
    ContactUnavailable,
}

fn bounded_key(value: &str, field: &'static str) -> NoticeResult<()> {
    if value.trim().is_empty() || value.len() > MAX_EVENT_KEY || value.chars().any(char::is_control)
    {
        return Err(NoticeError::InvalidKey(field));
    }
    Ok(())
}

fn bounded_text(value: &str, max: usize, field: &'static str) -> NoticeResult<()> {
    if value.trim().is_empty() || value.len() > max || value.chars().any(char::is_control) {
        return Err(NoticeError::InvalidContent(field));
    }
    Ok(())
}

fn payload_json(content: &NoticeContent) -> NoticeResult<String> {
    content.validate()?;
    serde_json::to_string(content).map_err(|_| NoticeError::InvalidContent("payload"))
}

/// Calculate the due time for a standard monthly price-change notice from its confirmed effective
/// date. Retries and restarts therefore cannot move a customer's notice window.
pub fn price_change_due_at(effective_at_epoch: i64) -> NoticeResult<i64> {
    let due_at = effective_at_epoch
        .checked_sub(STANDARD_MONTHLY_PRICE_NOTICE_LEAD_SECONDS)
        .ok_or(NoticeError::InvalidContent(
            "price change date is out of range",
        ))?;
    if due_at <= 0 {
        return Err(NoticeError::InvalidContent(
            "price change notice would be due before the epoch",
        ));
    }
    Ok(due_at)
}

/// Queue a notice idempotently. A conflict returns `AlreadyQueued` without changing due dates or
/// the existing delivery state.
pub async fn enqueue(pool: &PgPool, intent: &NoticeIntent) -> NoticeResult<EnqueueOutcome> {
    let mut tx = pool.begin().await?;
    let result = enqueue_in_tx(&mut tx, intent).await?;
    tx.commit().await?;
    Ok(result)
}

pub async fn enqueue_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    intent: &NoticeIntent,
) -> NoticeResult<EnqueueOutcome> {
    intent.validate()?;
    if intent.channel == NoticeChannel::Email {
        let contact_id = intent
            .contact_id
            .as_deref()
            .ok_or(NoticeError::ContactUnavailable)?;
        let contact_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM cloud_verified_contacts \
             WHERE contact_id=$1 AND user_id=$2 AND channel='email' AND revoked_at IS NULL)",
        )
        .bind(contact_id)
        .bind(&intent.recipient_user_id)
        .fetch_one(&mut **tx)
        .await?;
        if !contact_exists {
            return Err(NoticeError::ContactUnavailable);
        }
    }
    let payload = payload_json(&intent.content)?;
    let notice_id = format!("notice:{}", Uuid::new_v4());
    let inserted = sqlx::query(
        "INSERT INTO cloud_notice_outbox \
         (notice_id, recipient_user_id, event_key, policy_key, kind, channel, contact_id, payload, due_at, available_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8::jsonb,to_timestamp($9),to_timestamp($9)) \
         ON CONFLICT (recipient_user_id, event_key, policy_key, channel) DO NOTHING",
    )
    .bind(notice_id)
    .bind(&intent.recipient_user_id)
    .bind(&intent.event_key)
    .bind(&intent.policy_key)
    .bind(intent.kind.as_str())
    .bind(intent.channel.as_str())
    .bind(&intent.contact_id)
    .bind(payload)
    .bind(intent.due_at_epoch)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    Ok(if inserted == 1 {
        EnqueueOutcome::Enqueued
    } else {
        EnqueueOutcome::AlreadyQueued
    })
}

/// Record a verified email destination. Verification is intentionally an internal boundary until
/// the approved provider and challenge flow are configured; a GitHub login alone never creates it.
pub async fn record_verified_email(
    pool: &PgPool,
    contact_id: &str,
    user_id: &str,
    destination: &str,
    verified_at_epoch: i64,
) -> NoticeResult<()> {
    if contact_id.trim().is_empty() || user_id.trim().is_empty() {
        return Err(NoticeError::InvalidKey("contact identity"));
    }
    if verified_at_epoch <= 0 || destination.len() > 320 || destination.contains(['\r', '\n']) {
        return Err(NoticeError::InvalidContent("verified email"));
    }
    let affected = sqlx::query(
        "INSERT INTO cloud_verified_contacts \
         (contact_id, user_id, channel, destination, verified_at) \
         VALUES ($1,$2,'email',$3,to_timestamp($4)) \
         ON CONFLICT (contact_id) DO UPDATE SET destination=$3, verified_at=to_timestamp($4), revoked_at=NULL \
         WHERE cloud_verified_contacts.user_id = $2",
    )
    .bind(contact_id)
    .bind(user_id)
    .bind(destination)
    .bind(verified_at_epoch)
    .execute(pool)
    .await?
    .rows_affected();
    if affected != 1 {
        return Err(NoticeError::InvalidKey("contact belongs to another user"));
    }
    Ok(())
}

pub async fn revoke_contact(pool: &PgPool, user_id: &str, contact_id: &str) -> NoticeResult<()> {
    sqlx::query(
        "UPDATE cloud_verified_contacts SET revoked_at=now() \
         WHERE contact_id=$1 AND user_id=$2 AND revoked_at IS NULL",
    )
    .bind(contact_id)
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn cancel(
    pool: &PgPool,
    recipient_user_id: &str,
    event_key: &str,
    policy_key: &str,
) -> NoticeResult<u64> {
    let count = sqlx::query(
        "UPDATE cloud_notice_outbox SET status='cancelled', cancelled_at=now(), updated_at=now() \
         WHERE recipient_user_id=$1 AND event_key=$2 AND policy_key=$3 \
           AND status IN ('pending','leased')",
    )
    .bind(recipient_user_id)
    .bind(event_key)
    .bind(policy_key)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(count)
}

pub async fn list_for_user(pool: &PgPool, user_id: &str) -> NoticeResult<Vec<NoticeView>> {
    let rows = sqlx::query(
        "SELECT notice_id, kind, channel, payload, extract(epoch from due_at)::bigint AS due_at, \
                status, last_error_code, extract(epoch from delivered_at)::bigint AS delivered_at, \
                extract(epoch from created_at)::bigint AS created_at \
         FROM cloud_notice_outbox \
         WHERE recipient_user_id=$1 AND status <> 'cancelled' \
         ORDER BY created_at DESC, notice_id DESC LIMIT $2",
    )
    .bind(user_id)
    .bind(MAX_NOTICES)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| {
            let payload: NoticeContent = serde_json::from_value(row.try_get("payload")?)
                .map_err(|error| sqlx::Error::Decode(Box::new(error)))?;
            Ok(NoticeView {
                notice_id: row.try_get("notice_id")?,
                kind: row.try_get("kind")?,
                channel: row.try_get("channel")?,
                content: payload,
                due_at_epoch: row.try_get("due_at")?,
                status: row.try_get("status")?,
                last_error_code: row.try_get("last_error_code")?,
                delivered_at_epoch: row.try_get("delivered_at")?,
                created_at_epoch: row.try_get("created_at")?,
            })
        })
        .collect::<std::result::Result<Vec<_>, sqlx::Error>>()
        .map_err(NoticeError::Database)
}

/// Authenticated, in-app notices remain available without vault unlock. Email contacts are never
/// returned by this endpoint.
pub fn router() -> Router<AppState> {
    Router::new().route("/account/notices", get(get_notices))
}

async fn get_notices(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<Vec<NoticeView>>> {
    let notices = list_for_user(&state.pool, &user.user_id)
        .await
        .map_err(|error| match error {
            NoticeError::Database(error) => Error::Db(error),
            other => Error::Internal(other.to_string()),
        })?;
    Ok(Json(notices))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoticeLease {
    pub notice_id: String,
    pub recipient_user_id: String,
    pub kind: String,
    pub channel: String,
    pub contact_id: Option<String>,
    pub content: NoticeContent,
    pub worker_id: String,
    pub attempt_count: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoticeDelivery {
    pub destination: String,
    pub kind: String,
    pub content: NoticeContent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryOutcome {
    Delivered,
    Retry { code: String },
    PermanentFailure { code: String },
}

#[async_trait]
pub trait NoticeSender: Send {
    async fn send(&mut self, delivery: NoticeDelivery) -> DeliveryOutcome;
}

pub async fn claim_due(pool: &PgPool, worker_id: &str) -> NoticeResult<Option<NoticeLease>> {
    if worker_id.trim().is_empty() {
        return Err(NoticeError::EmptyWorkerId);
    }
    let mut tx = pool.begin().await?;
    let row = sqlx::query(
        "SELECT notice_id FROM cloud_notice_outbox \
         WHERE available_at <= now() AND (status='pending' OR (status='leased' AND lease_expires_at <= now())) \
         ORDER BY available_at, created_at, notice_id LIMIT 1 FOR UPDATE SKIP LOCKED",
    )
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        tx.commit().await?;
        return Ok(None);
    };
    let notice_id: String = row.try_get("notice_id")?;
    let row = sqlx::query(
        "UPDATE cloud_notice_outbox SET status='leased', lease_owner=$1, \
             lease_expires_at=now()+interval '5 minutes', attempt_count=attempt_count+1, updated_at=now() \
         WHERE notice_id=$2 AND (status='pending' OR (status='leased' AND lease_expires_at <= now())) \
         RETURNING recipient_user_id, kind, channel, contact_id, payload, attempt_count",
    )
    .bind(worker_id)
    .bind(&notice_id)
    .fetch_one(&mut *tx)
    .await?;
    let content: NoticeContent = serde_json::from_value(row.try_get("payload")?)
        .map_err(|error| sqlx::Error::Decode(Box::new(error)))?;
    let lease = NoticeLease {
        notice_id,
        recipient_user_id: row.try_get("recipient_user_id")?,
        kind: row.try_get("kind")?,
        channel: row.try_get("channel")?,
        contact_id: row.try_get("contact_id")?,
        content,
        worker_id: worker_id.to_owned(),
        attempt_count: row.try_get("attempt_count")?,
    };
    tx.commit().await?;
    Ok(Some(lease))
}

pub async fn run_once<S: NoticeSender + ?Sized>(
    pool: &PgPool,
    worker_id: &str,
    sender: &mut S,
) -> NoticeResult<bool> {
    let Some(lease) = claim_due(pool, worker_id).await? else {
        return Ok(false);
    };
    if lease.channel == NoticeChannel::InApp.as_str() {
        complete(pool, &lease).await?;
        return Ok(true);
    }
    let Some(contact_id) = lease.contact_id.as_deref() else {
        permanently_fail(pool, &lease, "contact_missing").await?;
        return Ok(true);
    };
    let destination: Option<String> = sqlx::query_scalar(
        "SELECT destination FROM cloud_verified_contacts \
         WHERE contact_id=$1 AND user_id=$2 AND channel='email' AND revoked_at IS NULL",
    )
    .bind(contact_id)
    .bind(&lease.recipient_user_id)
    .fetch_optional(pool)
    .await?;
    let Some(destination) = destination else {
        permanently_fail(pool, &lease, "contact_unavailable").await?;
        return Ok(true);
    };
    let outcome = sender
        .send(NoticeDelivery {
            destination,
            kind: lease.kind.clone(),
            content: lease.content.clone(),
        })
        .await;
    match outcome {
        DeliveryOutcome::Delivered => complete(pool, &lease).await?,
        DeliveryOutcome::Retry { code } => retry(pool, &lease, &code).await?,
        DeliveryOutcome::PermanentFailure { code } => permanently_fail(pool, &lease, &code).await?,
    }
    Ok(true)
}

async fn complete(pool: &PgPool, lease: &NoticeLease) -> NoticeResult<()> {
    let affected = sqlx::query(
        "UPDATE cloud_notice_outbox SET status='delivered', lease_owner=NULL, lease_expires_at=NULL, \
             delivered_at=now(), updated_at=now() \
         WHERE notice_id=$1 AND status='leased' AND lease_owner=$2 AND lease_expires_at > now()",
    )
    .bind(&lease.notice_id)
    .bind(&lease.worker_id)
    .execute(pool)
    .await?
    .rows_affected();
    if affected != 1 {
        return Err(NoticeError::LeaseLost);
    }
    Ok(())
}

async fn retry(pool: &PgPool, lease: &NoticeLease, code: &str) -> NoticeResult<()> {
    let delay = RETRY_DELAYS
        .get(lease.attempt_count.saturating_sub(1) as usize)
        .copied();
    if let Some(delay) = delay {
        let affected = sqlx::query(
            "UPDATE cloud_notice_outbox SET status='pending', lease_owner=NULL, lease_expires_at=NULL, \
                 available_at=now()+$1::interval, last_error_code=$2, updated_at=now() \
             WHERE notice_id=$3 AND status='leased' AND lease_owner=$4 AND lease_expires_at > now()",
        )
        .bind(format!("{} seconds", delay.as_secs()))
        .bind(code)
        .bind(&lease.notice_id)
        .bind(&lease.worker_id)
        .execute(pool)
        .await?
        .rows_affected();
        if affected != 1 {
            return Err(NoticeError::LeaseLost);
        }
    } else {
        permanently_fail(pool, lease, code).await?;
    }
    Ok(())
}

async fn permanently_fail(pool: &PgPool, lease: &NoticeLease, code: &str) -> NoticeResult<()> {
    let affected = sqlx::query(
        "UPDATE cloud_notice_outbox SET status='failed', lease_owner=NULL, lease_expires_at=NULL, \
             last_error_code=$1, updated_at=now() \
         WHERE notice_id=$2 AND status='leased' AND lease_owner=$3 AND lease_expires_at > now()",
    )
    .bind(code)
    .bind(&lease.notice_id)
    .bind(&lease.worker_id)
    .execute(pool)
    .await?
    .rows_affected();
    if affected != 1 {
        return Err(NoticeError::LeaseLost);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn content() -> NoticeContent {
        NoticeContent {
            title: "Renewal failed".into(),
            detail: "Recovery is available until the displayed deadline.".into(),
            effective_at_epoch: Some(1_800_000_000),
            deadline_epoch: Some(1_800_100_000),
            amount_pence: Some(299),
        }
    }

    #[test]
    fn content_rejects_control_bytes_and_negative_amounts() {
        let mut invalid = content();
        invalid.title = "bad\nsubject".into();
        assert!(matches!(
            invalid.validate(),
            Err(NoticeError::InvalidContent("title"))
        ));
        let mut invalid = content();
        invalid.amount_pence = Some(-1);
        assert!(matches!(
            invalid.validate(),
            Err(NoticeError::InvalidContent(
                "amount_pence must not be negative"
            ))
        ));
    }

    #[test]
    fn intent_requires_a_verified_contact_for_email_only() {
        let mut intent = NoticeIntent {
            recipient_user_id: "user".into(),
            event_key: "renewal:event".into(),
            policy_key: "recovery:v1".into(),
            kind: NoticeKind::FailedRenewal,
            channel: NoticeChannel::Email,
            contact_id: None,
            due_at_epoch: 1_800_000_000,
            content: content(),
        };
        assert!(matches!(
            intent.validate(),
            Err(NoticeError::InvalidContent(
                "email notices require a verified contact"
            ))
        ));
        intent.channel = NoticeChannel::InApp;
        assert!(intent.validate().is_ok());
    }

    #[test]
    fn kind_and_channel_names_are_stable() {
        assert_eq!(NoticeKind::PriceChange.as_str(), "price_change");
        assert_eq!(NoticeChannel::InApp.as_str(), "in_app");
        assert_eq!(NoticeChannel::Email.as_str(), "email");
    }

    #[test]
    fn monthly_price_notice_uses_the_confirmed_sixty_day_boundary() {
        let effective_at = 2_000_000_000;
        assert_eq!(
            price_change_due_at(effective_at).unwrap(),
            effective_at - STANDARD_MONTHLY_PRICE_NOTICE_LEAD_SECONDS
        );
        assert!(matches!(
            price_change_due_at(STANDARD_MONTHLY_PRICE_NOTICE_LEAD_SECONDS),
            Err(NoticeError::InvalidContent(
                "price change notice would be due before the epoch"
            ))
        ));
    }
}
