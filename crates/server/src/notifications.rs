//! Durable, redacted lifecycle notices.
//!
//! Notice identity is separate from delivery.  A caller records one `(recipient, event, policy,
//! channel)` intent and may safely retry that write; the unique identity prevents a provider
//! callback or worker restart from resetting the lifecycle dates or creating a duplicate message.
//! In-app notices are available without unlocking the vault. Email delivery is deliberately
//! adapter-driven and can only use a contact that was verified separately.

use std::time::Duration;

use async_trait::async_trait;
use axum::extract::{Path, Query, State};
use axum::http::header::CACHE_CONTROL;
use axum::http::{HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Row, Transaction};
use thiserror::Error;
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::cloud_coverage::EXPORT_WINDOW_SECONDS;
use crate::error::{Error, Result};
use crate::person_eligibility::{EligibilityState, EligibilityView};
use crate::state::AppState;

const MAX_EVENT_KEY: usize = 160;
const MAX_TEXT: usize = 240;
const MAX_DETAIL: usize = 2_000;
const MAX_NOTICES: i64 = 100;
const MAX_NOTICES_PER_WORKER_TICK: usize = 20;
pub const STANDARD_MONTHLY_PRICE_NOTICE_LEAD_SECONDS: i64 = 60 * 24 * 60 * 60;
pub const RECOVERY_END_NOTICE_LEAD_SECONDS: i64 = 3 * 24 * 60 * 60;
pub(crate) const EXPORT_DEADLINE_POLICY_KEY: &str = "cloud-export-deadline-v1";
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
    SharedExportDeadline,
    ImpendingPurge,
    SponsorshipChange,
    PayerTransferInvitation,
    PayerTransferOutcome,
    FoundingExpiry,
    PriceChange,
    RecoveryResolved,
    SponsoredRenewalFailure,
    SponsoredRenewalAction,
}

impl NoticeKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CheckoutOutcome => "checkout_outcome",
            Self::FailedRenewal => "failed_renewal",
            Self::RecoveryEnd => "recovery_end",
            Self::ExportDeadline => "export_deadline",
            Self::SharedExportDeadline => "shared_export_deadline",
            Self::ImpendingPurge => "impending_purge",
            Self::SponsorshipChange => "sponsorship_change",
            Self::PayerTransferInvitation => "payer_transfer_invitation",
            Self::PayerTransferOutcome => "payer_transfer_outcome",
            Self::FoundingExpiry => "founding_expiry",
            Self::PriceChange => "price_change",
            Self::RecoveryResolved => "recovery_resolved",
            Self::SponsoredRenewalFailure => "sponsored_renewal_failure",
            Self::SponsoredRenewalAction => "sponsored_renewal_action",
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_interval: Option<String>,
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
        if self
            .price_interval
            .as_deref()
            .is_some_and(|interval| !matches!(interval, "month" | "year"))
        {
            return Err(NoticeError::InvalidContent(
                "price_interval must be month or year",
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
    /// Internal scope used to re-check recipient authority before external delivery.
    pub scope_id: Option<String>,
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
        if self.scope_id.as_deref().is_some_and(|scope| {
            scope.trim().is_empty()
                || scope.len() > MAX_EVENT_KEY
                || scope.chars().any(char::is_control)
        }) {
            return Err(NoticeError::InvalidKey("scope_id"));
        }
        self.content.validate()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueOutcome {
    Enqueued,
    AlreadyQueued,
}

#[derive(Debug, Clone, Copy)]
pub enum CheckoutOutcome {
    Completed,
    RefundRequired,
    Expired,
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
    #[error("notification provider signature is invalid")]
    InvalidProviderSignature,
    #[error("notification provider event is invalid")]
    InvalidProviderEvent,
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
    let outcome = insert_intent_in_tx(tx, intent).await?;
    if intent.channel == NoticeChannel::InApp && intent.kind != NoticeKind::ImpendingPurge {
        let contact_id: Option<String> = sqlx::query_scalar(
            "SELECT contact_id FROM cloud_verified_contacts \
             WHERE user_id=$1 AND channel='email' AND revoked_at IS NULL",
        )
        .bind(&intent.recipient_user_id)
        .fetch_optional(&mut **tx)
        .await?;
        if let Some(contact_id) = contact_id {
            let mut email_intent = intent.clone();
            email_intent.channel = NoticeChannel::Email;
            email_intent.contact_id = Some(contact_id);
            insert_intent_in_tx(tx, &email_intent).await?;
        }
    }
    Ok(outcome)
}

/// Record a durable checkout result using the existing notice identity/outbox. No provider
/// identifiers or checkout tokens enter the customer-visible payload.
pub async fn enqueue_checkout_outcome_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    recipient_user_id: &str,
    operation_id: &str,
    outcome: CheckoutOutcome,
) -> NoticeResult<()> {
    use sha2::{Digest, Sha256};
    bounded_key(recipient_user_id, "recipient_user_id")?;
    bounded_key(operation_id, "operation_id")?;
    let digest = Sha256::digest(operation_id.as_bytes());
    let event_key = format!(
        "checkout-outcome-v1:{}",
        digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    let due_at_epoch: i64 = sqlx::query_scalar("SELECT floor(extract(epoch FROM now()))::bigint")
        .fetch_one(&mut **tx)
        .await?;
    let (title, detail) = match outcome {
        CheckoutOutcome::Completed => (
            "Your Cloud checkout is complete",
            "Payment was confirmed and your Cloud billing status has been updated.",
        ),
        CheckoutOutcome::RefundRequired => (
            "Your Cloud checkout needs attention",
            "Payment was received, but this checkout could not be activated. A refund is being arranged.",
        ),
        CheckoutOutcome::Expired => (
            "Your Cloud checkout expired",
            "The checkout session expired before completing. You can start again from your Cloud account.",
        ),
    };
    enqueue_in_tx(
        tx,
        &NoticeIntent {
            recipient_user_id: recipient_user_id.to_owned(),
            event_key,
            policy_key: "cloud-checkout-outcome-v1".into(),
            kind: NoticeKind::CheckoutOutcome,
            channel: NoticeChannel::InApp,
            contact_id: None,
            scope_id: None,
            due_at_epoch,
            content: NoticeContent {
                title: title.into(),
                detail: detail.into(),
                effective_at_epoch: None,
                deadline_epoch: None,
                amount_pence: None,
                price_interval: None,
            },
        },
    )
    .await?;
    Ok(())
}

/// Queue the shared-organisation export notice for one current owner or admin. The same event key
/// is used for every recipient; the organisation id remains internal in `scope_id`.
pub(crate) async fn enqueue_shared_export_deadline_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    recipient_user_id: &str,
    organisation_id: &str,
    event_key: &str,
    effective_at_epoch: i64,
    deadline_epoch: i64,
) -> NoticeResult<()> {
    let intent = NoticeIntent {
        recipient_user_id: recipient_user_id.to_owned(),
        event_key: event_key.to_owned(),
        policy_key: "shared-export-deadline-v1".to_owned(),
        kind: NoticeKind::SharedExportDeadline,
        channel: NoticeChannel::InApp,
        contact_id: None,
        scope_id: Some(organisation_id.to_owned()),
        due_at_epoch: effective_at_epoch,
        content: NoticeContent {
            title: "Shared organisation data is in its export period".into(),
            detail: "Download an encrypted export of shared organisation data before its export deadline.".into(),
            effective_at_epoch: Some(effective_at_epoch),
            deadline_epoch: Some(deadline_epoch),
            amount_pence: None,
            price_interval: None,
        },
    };
    enqueue_in_tx(tx, &intent).await?;
    Ok(())
}

/// Notify the affected seat holders and current billing administrators that a provider schedule
/// accepted a named sponsorship change. The notice is due at its effective boundary and the
/// dispatcher rechecks the live operation and recipients before sending.
pub async fn enqueue_sponsorship_change_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    operation_id: &str,
) -> NoticeResult<()> {
    bounded_key(operation_id, "operation_id")?;
    let operation = sqlx::query(
        "SELECT operation.organization_id,operation.action,operation.beneficiary_id, \
                operation.replacement_beneficiary_id,operation.effective_from,operation.state, \
                organization.lifecycle_state \
         FROM billing_sponsored_operations operation \
         JOIN organizations organization ON organization.id=operation.organization_id \
         WHERE operation.operation_id=$1",
    )
    .bind(operation_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(operation) = operation else {
        return Ok(());
    };
    let organization_id: String = operation.try_get("organization_id")?;
    let action: String = operation.try_get("action")?;
    let beneficiary_id: String = operation.try_get("beneficiary_id")?;
    let replacement_id: Option<String> = operation.try_get("replacement_beneficiary_id")?;
    let effective_from: i64 = operation.try_get("effective_from")?;
    let state: String = operation.try_get("state")?;
    let lifecycle_state: String = operation.try_get("lifecycle_state")?;
    if lifecycle_state != "active" || !matches!(state.as_str(), "provider_pending" | "active") {
        return Ok(());
    }
    let mut recipients = std::collections::BTreeSet::from([beneficiary_id]);
    recipients.extend(replacement_id);
    let administrators: Vec<String> = sqlx::query_scalar(
        "SELECT membership.user_id FROM organization_memberships membership \
         JOIN organizations organization ON organization.id=membership.org_id \
         WHERE membership.org_id=$1 AND membership.role IN ('owner','admin') \
           AND organization.lifecycle_state='active' \
         ORDER BY membership.user_id COLLATE \"C\" LIMIT 128",
    )
    .bind(&organization_id)
    .fetch_all(&mut **tx)
    .await?;
    recipients.extend(administrators);
    let (title, detail) = match action.as_str() {
        "add" => (
            "Sponsored Cloud access is scheduled",
            "Your organisation has scheduled sponsored Cloud access to begin on the displayed date.",
        ),
        "remove" => (
            "Sponsored Cloud access is scheduled to end",
            "Your organisation has scheduled sponsored Cloud access to end on the displayed date. Access remains available through the paid term.",
        ),
        "replace" => (
            "A sponsored Cloud seat change is scheduled",
            "Your organisation has scheduled a change to a named Cloud seat for the displayed date.",
        ),
        _ => return Ok(()),
    };
    for recipient_user_id in recipients {
        enqueue_in_tx(
            tx,
            &NoticeIntent {
                recipient_user_id,
                event_key: operation_id.to_owned(),
                policy_key: "sponsorship-change-scheduled-v1".into(),
                kind: NoticeKind::SponsorshipChange,
                channel: NoticeChannel::InApp,
                contact_id: None,
                scope_id: Some(organization_id.clone()),
                due_at_epoch: effective_from,
                content: NoticeContent {
                    title: title.into(),
                    detail: detail.into(),
                    effective_at_epoch: Some(effective_from),
                    deadline_epoch: None,
                    amount_pence: None,
                    price_interval: None,
                },
            },
        )
        .await?;
    }
    Ok(())
}

pub async fn enqueue_payer_transfer_outcome_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    transfer_id: &str,
    beneficiary_id: &str,
    organization_ids: &[String],
    effective_at_epoch: i64,
) -> NoticeResult<()> {
    bounded_key(transfer_id, "transfer_id")?;
    bounded_key(beneficiary_id, "beneficiary_id")?;
    let mut recipients = std::collections::BTreeSet::from([beneficiary_id.to_owned()]);
    for organization_id in organization_ids {
        let administrators: Vec<String> = sqlx::query_scalar(
            "SELECT membership.user_id FROM organization_memberships membership \
             JOIN organizations organization ON organization.id=membership.org_id \
             WHERE membership.org_id=$1 AND membership.role IN ('owner','admin') \
               AND organization.lifecycle_state='active' \
             ORDER BY membership.user_id COLLATE \"C\" LIMIT 129",
        )
        .bind(organization_id)
        .fetch_all(&mut **tx)
        .await?;
        if administrators.len() > 128 {
            return Err(NoticeError::InvalidContent(
                "transfer notice scope exceeded its bound",
            ));
        }
        recipients.extend(administrators);
    }
    let due_at_epoch: i64 = sqlx::query_scalar("SELECT floor(extract(epoch FROM now()))::bigint")
        .fetch_one(&mut **tx)
        .await?;
    for recipient_user_id in recipients {
        enqueue_in_tx(
            tx,
            &NoticeIntent {
                recipient_user_id,
                event_key: transfer_id.to_owned(),
                policy_key: "payer-transfer-outcome-v1".into(),
                kind: NoticeKind::PayerTransferOutcome,
                channel: NoticeChannel::InApp,
                contact_id: None,
                scope_id: None,
                due_at_epoch,
                content: NoticeContent {
                    title: "Your Cloud payer transfer is complete".into(),
                    detail: "The new payer is responsible from the displayed renewal date. Your existing paid term remains in effect, with no mid-term credit or refund.".into(),
                    effective_at_epoch: Some(effective_at_epoch),
                    deadline_epoch: None,
                    amount_pence: None,
                    price_interval: None,
                },
            },
        )
        .await?;
    }
    Ok(())
}

async fn insert_intent_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    intent: &NoticeIntent,
) -> NoticeResult<EnqueueOutcome> {
    let payload = payload_json(&intent.content)?;
    let notice_id = format!("notice:{}", Uuid::new_v4());
    let inserted = sqlx::query(
        "INSERT INTO cloud_notice_outbox \
         (notice_id, recipient_user_id, event_key, policy_key, kind, channel, contact_id, scope_id, payload, due_at, available_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9::jsonb,to_timestamp($10),to_timestamp($10)) \
         ON CONFLICT (recipient_user_id, event_key, policy_key, channel) DO NOTHING",
    )
    .bind(notice_id)
    .bind(&intent.recipient_user_id)
    .bind(&intent.event_key)
    .bind(&intent.policy_key)
    .bind(intent.kind.as_str())
    .bind(intent.channel.as_str())
    .bind(&intent.contact_id)
    .bind(&intent.scope_id)
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

/// Queue a bounded catch-up for notices that are scheduled for the future or were created within
/// the fixed export-window horizon. At most `MAX_NOTICES` are copied, with the earliest due items
/// first, so a large backlog cannot block contact verification. Every message keeps the original
/// event identity and due date; the dispatcher rechecks current authority before sending.
pub async fn enqueue_current_for_verified_contact_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    recipient_user_id: &str,
    contact_id: &str,
) -> NoticeResult<usize> {
    let contact_is_current: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM cloud_verified_contacts WHERE contact_id=$1 \
         AND user_id=$2 AND channel='email' AND revoked_at IS NULL)",
    )
    .bind(contact_id)
    .bind(recipient_user_id)
    .fetch_one(&mut **tx)
    .await?;
    if !contact_is_current {
        return Err(NoticeError::ContactUnavailable);
    }
    let rows = sqlx::query(
        "SELECT notice.event_key, notice.policy_key, notice.kind, notice.scope_id, \
             extract(epoch FROM notice.due_at)::bigint AS due_at, notice.payload \
         FROM cloud_notice_outbox notice \
         WHERE notice.recipient_user_id=$1 AND notice.channel='in_app' \
           AND notice.status <> 'cancelled' AND notice.kind <> 'impending_purge' \
           AND (notice.due_at > now() OR notice.created_at >= now()-interval '30 days') \
           AND NOT EXISTS (SELECT 1 FROM cloud_notice_outbox email \
             WHERE email.recipient_user_id=notice.recipient_user_id \
               AND email.event_key=notice.event_key AND email.policy_key=notice.policy_key \
               AND email.channel='email' \
               AND email.status NOT IN ('cancelled','suppressed')) \
         ORDER BY notice.due_at, notice.created_at, notice.notice_id LIMIT $2",
    )
    .bind(recipient_user_id)
    .bind(MAX_NOTICES)
    .fetch_all(&mut **tx)
    .await?;
    let mut queued = 0;
    for row in rows {
        let kind = notice_kind_from_str(&row.try_get::<String, _>("kind")?)
            .ok_or(NoticeError::InvalidContent("notice kind is invalid"))?;
        let content: NoticeContent = serde_json::from_value(row.try_get("payload")?)
            .map_err(|_| NoticeError::InvalidContent("notice payload is invalid"))?;
        let intent = NoticeIntent {
            recipient_user_id: recipient_user_id.to_owned(),
            event_key: row.try_get("event_key")?,
            policy_key: row.try_get("policy_key")?,
            kind,
            channel: NoticeChannel::Email,
            contact_id: Some(contact_id.to_owned()),
            scope_id: row.try_get("scope_id")?,
            due_at_epoch: row.try_get("due_at")?,
            content,
        };
        if upsert_catch_up_email_in_tx(tx, &intent).await? {
            queued += 1;
        }
    }
    Ok(queued)
}

async fn upsert_catch_up_email_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    intent: &NoticeIntent,
) -> NoticeResult<bool> {
    intent.validate()?;
    let contact_id = intent
        .contact_id
        .as_deref()
        .ok_or(NoticeError::ContactUnavailable)?;
    let payload = payload_json(&intent.content)?;
    let notice_id = format!("notice:{}", Uuid::new_v4());
    let affected = sqlx::query(
        "INSERT INTO cloud_notice_outbox \
         (notice_id,recipient_user_id,event_key,policy_key,kind,channel,contact_id,scope_id,payload,due_at,available_at) \
         VALUES($1,$2,$3,$4,$5,'email',$6,$7,$8::jsonb,to_timestamp($9),to_timestamp($9)) \
         ON CONFLICT(recipient_user_id,event_key,policy_key,channel) DO UPDATE SET \
           status='pending',contact_id=EXCLUDED.contact_id,scope_id=EXCLUDED.scope_id,cancelled_at=NULL, \
           available_at=EXCLUDED.available_at,lease_owner=NULL,lease_expires_at=NULL, \
           last_error_code=NULL,updated_at=now() \
         WHERE cloud_notice_outbox.status IN ('cancelled','suppressed') \
           AND cloud_notice_outbox.delivered_at IS NULL \
           AND cloud_notice_outbox.due_at=EXCLUDED.due_at \
           AND cloud_notice_outbox.payload=EXCLUDED.payload",
    )
    .bind(notice_id)
    .bind(&intent.recipient_user_id)
    .bind(&intent.event_key)
    .bind(&intent.policy_key)
    .bind(intent.kind.as_str())
    .bind(contact_id)
    .bind(&intent.scope_id)
    .bind(payload)
    .bind(intent.due_at_epoch)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    Ok(affected == 1)
}

fn notice_kind_from_str(value: &str) -> Option<NoticeKind> {
    Some(match value {
        "checkout_outcome" => NoticeKind::CheckoutOutcome,
        "failed_renewal" => NoticeKind::FailedRenewal,
        "recovery_end" => NoticeKind::RecoveryEnd,
        "export_deadline" => NoticeKind::ExportDeadline,
        "shared_export_deadline" => NoticeKind::SharedExportDeadline,
        "impending_purge" => NoticeKind::ImpendingPurge,
        "sponsorship_change" => NoticeKind::SponsorshipChange,
        "payer_transfer_invitation" => NoticeKind::PayerTransferInvitation,
        "payer_transfer_outcome" => NoticeKind::PayerTransferOutcome,
        "founding_expiry" => NoticeKind::FoundingExpiry,
        "price_change" => NoticeKind::PriceChange,
        "recovery_resolved" => NoticeKind::RecoveryResolved,
        "sponsored_renewal_failure" => NoticeKind::SponsoredRenewalFailure,
        "sponsored_renewal_action" => NoticeKind::SponsoredRenewalAction,
        _ => return None,
    })
}

/// Reconcile the in-app export notice against one complete eligibility snapshot.
///
/// The account endpoint supplies the coverage revision it evaluated. The shared projection head
/// is locked while the outbox is changed, so an older eligibility read cannot restore a notice
/// after a newer source set has already committed.
pub async fn reconcile_export_deadline_notice(
    pool: &PgPool,
    recipient_user_id: &str,
    view: &EligibilityView,
    coverage_revision: Option<i64>,
    now_epoch: i64,
) -> NoticeResult<bool> {
    bounded_key(recipient_user_id, "recipient_user_id")?;
    if now_epoch <= 0 {
        return Err(NoticeError::InvalidContent("now_epoch must be positive"));
    }

    // Unavailable means the source set cannot be trusted. Keep the last durable result intact;
    // an unavailable snapshot cannot prove that an earlier export notice is stale.
    if view.state == EligibilityState::Unavailable {
        return Ok(true);
    }
    if coverage_revision.is_none() {
        // A missing projection cannot show that an earlier export episode was superseded. This
        // is the normal first-read shape for a free account, so preserve existing notice state
        // without treating the absence as an error or as proof that coverage ended.
        if view.state == EligibilityState::Free {
            return Ok(true);
        }
        return Err(NoticeError::InvalidContent(
            "this eligibility state requires a complete coverage revision",
        ));
    }

    let mut tx = pool.begin().await?;
    let lock_key = format!("cloud-export-notice:{recipient_user_id}");
    sqlx::query_scalar::<_, ()>("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(lock_key)
        .fetch_one(&mut *tx)
        .await?;

    let current_head: Option<Option<i64>> = sqlx::query_scalar(
        "SELECT current_revision FROM cloud_coverage_heads WHERE beneficiary_id=$1 FOR SHARE",
    )
    .bind(recipient_user_id)
    .fetch_optional(&mut *tx)
    .await?;
    let current_revision = match current_head {
        None => None,
        Some(Some(revision)) => Some(revision),
        Some(None) => return Ok(false),
    };
    if current_revision != coverage_revision {
        tx.commit().await?;
        return Ok(false);
    }

    // Eligibility can cross the export deadline between loading an ExportOnly view and writing
    // its notice. Preserve the existing row for the retention reconciler rather than cancelling
    // it based on a snapshot that is stale only because the clock crossed the boundary.
    if view.state == EligibilityState::ExportOnly
        && view
            .export_until_epoch
            .is_some_and(|deadline| deadline <= now_epoch)
    {
        tx.commit().await?;
        return Ok(false);
    }

    // A pending personal checkout takes precedence in the public eligibility state, but its
    // coverage decision can still carry the beneficiary's current export deadline. Keep the
    // notice tied to that complete projection until its deadline passes; pending billing alone
    // does not prove that the export episode has ended.
    let notice_window = export_deadline_notice_window(view, now_epoch)?;
    let intent = if let Some((effective_at, deadline)) = notice_window {
        Some(NoticeIntent {
            recipient_user_id: recipient_user_id.to_owned(),
            event_key: format!("cloud-export-window:{effective_at}:{deadline}"),
            policy_key: EXPORT_DEADLINE_POLICY_KEY.to_owned(),
            kind: NoticeKind::ExportDeadline,
            channel: NoticeChannel::InApp,
            contact_id: None,
            scope_id: None,
            due_at_epoch: effective_at,
            content: NoticeContent {
                title: "Your Cloud account is in its export period".into(),
                detail: "Download an encrypted export before your export deadline.".into(),
                effective_at_epoch: Some(effective_at),
                deadline_epoch: Some(deadline),
                amount_pence: None,
                price_interval: None,
            },
        })
    } else {
        None
    };
    let recovery_end_intent = if view.state == EligibilityState::RenewalRecovery {
        view.recovery_until_epoch
            .map(|recovery_until| {
                let due_at = recovery_end_notice_due_at(recovery_until)?;
                Ok::<NoticeIntent, NoticeError>(NoticeIntent {
                    recipient_user_id: recipient_user_id.to_owned(),
                    event_key: format!("cloud-recovery-end:{recovery_until}"),
                    policy_key: "cloud-recovery-end-v1".to_owned(),
                    kind: NoticeKind::RecoveryEnd,
                    channel: NoticeChannel::InApp,
                    contact_id: None,
                    scope_id: None,
                    due_at_epoch: due_at,
                    content: NoticeContent {
                        title: "Your Cloud recovery period is ending soon".into(),
                        detail: "Your renewal is still unresolved. Resolve billing by the displayed date to keep Cloud coverage active.".into(),
                        effective_at_epoch: None,
                        deadline_epoch: Some(recovery_until),
                        amount_pence: None,
                        price_interval: None,
                    },
                })
            })
            .transpose()?
    } else {
        None
    };

    let desired_event_key = intent.as_ref().map(|notice| notice.event_key.as_str());
    sqlx::query(
        "UPDATE cloud_notice_outbox SET status='cancelled', cancelled_at=now(), updated_at=now() \
         WHERE recipient_user_id=$1 AND kind='export_deadline' AND policy_key=$2 \
           AND channel IN ('in_app','email') AND status IN ('pending','leased') \
           AND ($3::text IS NULL OR event_key <> $3)",
    )
    .bind(recipient_user_id)
    .bind(EXPORT_DEADLINE_POLICY_KEY)
    .bind(desired_event_key)
    .execute(&mut *tx)
    .await?;
    let desired_recovery_end = recovery_end_intent
        .as_ref()
        .map(|notice| notice.event_key.as_str());
    sqlx::query(
        "UPDATE cloud_notice_outbox SET status='cancelled',cancelled_at=now(), \
             lease_owner=NULL,lease_expires_at=NULL,updated_at=now() \
         WHERE recipient_user_id=$1 AND kind='recovery_end' AND policy_key='cloud-recovery-end-v1' \
           AND channel IN ('in_app','email') AND status IN ('pending','leased') \
           AND ($2::text IS NULL OR event_key <> $2)",
    )
    .bind(recipient_user_id)
    .bind(desired_recovery_end)
    .execute(&mut *tx)
    .await?;

    if let Some(intent) = intent {
        let payload = payload_json(&intent.content)?;
        sqlx::query(
            "UPDATE cloud_notice_outbox SET status='pending', cancelled_at=NULL, \
                 available_at=due_at, updated_at=now() \
             WHERE recipient_user_id=$1 AND event_key=$2 AND policy_key=$3 AND channel IN ('in_app','email') \
               AND status='cancelled' AND delivered_at IS NULL AND due_at=to_timestamp($4) \
               AND payload=$5::jsonb",
        )
        .bind(recipient_user_id)
        .bind(&intent.event_key)
        .bind(EXPORT_DEADLINE_POLICY_KEY)
        .bind(intent.due_at_epoch)
        .bind(payload)
        .execute(&mut *tx)
        .await?;
        enqueue_in_tx(&mut tx, &intent).await?;
    }
    if let Some(intent) = recovery_end_intent {
        enqueue_in_tx(&mut tx, &intent).await?;
    }

    tx.commit().await?;
    Ok(true)
}

/// Return the fixed export-window start and deadline that should identify the durable notice.
///
/// Queueing the notice while the person is still paid makes it available in the account surface
/// exactly when the export window opens, even if they do not sign in at that boundary. The
/// eligibility projection remains authoritative: a later paid-through change produces a new
/// event key and cancels the superseded pending notice.
fn export_deadline_notice_window(
    view: &EligibilityView,
    now_epoch: i64,
) -> NoticeResult<Option<(i64, i64)>> {
    let window =
        match view.state {
            EligibilityState::Paid | EligibilityState::RenewalRecovery => {
                let effective_at = match view.state {
                    EligibilityState::Paid => view.paid_through_epoch,
                    EligibilityState::RenewalRecovery => {
                        view.recovery_until_epoch.or(view.paid_through_epoch)
                    }
                    _ => unreachable!("matched paid or recovery state"),
                };
                let Some(effective_at) = effective_at else {
                    return Ok(None);
                };
                if effective_at <= now_epoch {
                    return Ok(None);
                }
                let deadline = effective_at.checked_add(EXPORT_WINDOW_SECONDS).ok_or(
                    NoticeError::InvalidContent("export deadline is out of range"),
                )?;
                Some((effective_at, deadline))
            }
            EligibilityState::ExportOnly => {
                let deadline = view
                    .export_until_epoch
                    .ok_or(NoticeError::InvalidContent("export deadline is missing"))?;
                let effective_at = deadline.checked_sub(EXPORT_WINDOW_SECONDS).ok_or(
                    NoticeError::InvalidContent("export deadline is out of range"),
                )?;
                if deadline <= now_epoch {
                    return Ok(None);
                }
                Some((effective_at, deadline))
            }
            EligibilityState::PendingInitialPayment
                if view
                    .export_until_epoch
                    .is_some_and(|deadline| deadline > now_epoch) =>
            {
                let deadline = view.export_until_epoch.expect("guarded above");
                Some((
                    deadline.checked_sub(EXPORT_WINDOW_SECONDS).ok_or(
                        NoticeError::InvalidContent("export deadline is out of range"),
                    )?,
                    deadline,
                ))
            }
            _ => None,
        };
    if window.is_some_and(|(effective_at, deadline)| effective_at <= 0 || deadline <= effective_at)
    {
        return Err(NoticeError::InvalidContent(
            "export-only deadline is outside its expected window",
        ));
    }
    Ok(window)
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
         WHERE recipient_user_id=$1 AND channel='in_app' AND status <> 'cancelled' AND due_at <= now() \
           AND (kind <> 'price_change' OR EXISTS ( \
             SELECT 1 FROM billing_founding_price_notice_state AS notice_state \
             JOIN billing_founding_awards AS award ON award.award_id = notice_state.award_id \
             JOIN billing_founding_schedule_jobs AS schedule ON schedule.award_id = award.award_id \
             WHERE notice_state.outcome='queued' \
               AND notice_state.schedule_version=schedule.version \
               AND schedule.state='scheduled' \
               AND notice_state.payer_kind=award.payer_kind \
               AND notice_state.payer_id=award.payer_id \
               AND cloud_notice_outbox.event_key = \
                 'founding-price-change:' || award.award_id || ':' || notice_state.transition_epoch::text \
               AND ((award.payer_kind='personal' AND award.payer_id=$1 AND EXISTS ( \
                    SELECT 1 FROM billing_personal_accounts account \
                    WHERE account.user_id=$1 AND account.state IN ('active','past_due') \
                      AND NOT account.cancel_at_period_end AND account.cancellation_requested_at IS NULL \
                      AND account.stripe_subscription_id=schedule.provider_subscription_id)) \
                 OR (award.payer_kind='sponsor' AND EXISTS ( \
                    SELECT 1 FROM organization_memberships membership \
                    JOIN organizations org ON org.id=membership.org_id \
                    JOIN billing_sponsored_subscriptions sub ON sub.organization_id=org.id \
                    JOIN billing_sponsored_seats seat ON seat.organization_id=org.id \
                      AND seat.beneficiary_id=award.beneficiary_id \
                    WHERE membership.user_id=$1 AND membership.org_id=award.payer_id \
                      AND membership.role IN ('owner','admin') AND org.lifecycle_state='active' \
                      AND sub.status IN ('active','past_due') \
                      AND sub.provider_subscription_id=schedule.provider_subscription_id \
                      AND seat.state IN ('active','scheduled_removal') \
                      AND seat.effective_from <= notice_state.transition_epoch \
                      AND (seat.effective_until IS NULL OR seat.effective_until > notice_state.transition_epoch)))) \
           )) \
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
    Router::new()
        .route("/account/notices", get(get_notices))
        .route("/webhooks/resend", axum::routing::post(resend_webhook))
}

pub fn operator_router() -> Router<AppState> {
    Router::new()
        .route("/ops/cloud/notices", get(list_notice_operations))
        .route(
            "/ops/cloud/notices/{fingerprint}",
            post(apply_notice_operation),
        )
}

#[derive(Debug, Deserialize)]
struct NoticeOperationQuery {
    after: Option<String>,
    limit: Option<i64>,
}

#[derive(Debug, Serialize)]
struct NoticeOperationSummary {
    fingerprint: String,
    kind: &'static str,
    status: &'static str,
    due_at_epoch: i64,
    attempt_count: i32,
    operator_retry_count: i32,
    last_error_code: Option<&'static str>,
    updated_at_epoch: i64,
}

#[derive(Debug, Serialize)]
struct NoticeOperationsResponse {
    states: Vec<(String, i64)>,
    notices: Vec<NoticeOperationSummary>,
    next_cursor: Option<String>,
}

async fn list_notice_operations(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<NoticeOperationQuery>,
) -> Result<Response> {
    require_notice_operator(&state, &headers)?;
    let limit = query.limit.unwrap_or(50);
    if !(1..=100).contains(&limit)
        || query
            .after
            .as_deref()
            .is_some_and(|cursor| !is_notice_fingerprint(cursor))
    {
        return Err(Error::BadRequest("notice query is invalid".into()));
    }
    let states: Vec<(String, i64)> = sqlx::query_as(
        "SELECT status,count(*) FROM cloud_notice_outbox GROUP BY status ORDER BY status",
    )
    .fetch_all(&state.pool)
    .await?;
    let rows = sqlx::query(
        "SELECT operator_fingerprint,kind,status,extract(epoch FROM due_at)::bigint AS due_at, \
                attempt_count,operator_retry_count,last_error_code, \
                extract(epoch FROM updated_at)::bigint AS updated_at \
         FROM cloud_notice_outbox WHERE ($1::text IS NULL OR operator_fingerprint COLLATE \"C\" > $1 COLLATE \"C\") \
         ORDER BY operator_fingerprint COLLATE \"C\" LIMIT $2",
    )
    .bind(query.after.as_deref())
    .bind(limit + 1)
    .fetch_all(&state.pool)
    .await?;
    let has_more = rows.len() as i64 > limit;
    let rows = rows.into_iter().take(limit as usize).collect::<Vec<_>>();
    let mut notices = Vec::with_capacity(rows.len());
    for row in rows {
        let kind = safe_notice_kind(&row.try_get::<String, _>("kind")?);
        notices.push(NoticeOperationSummary {
            fingerprint: row.try_get("operator_fingerprint")?,
            kind,
            status: safe_notice_status(&row.try_get::<String, _>("status")?),
            due_at_epoch: row.try_get("due_at")?,
            attempt_count: row.try_get("attempt_count")?,
            operator_retry_count: row.try_get("operator_retry_count")?,
            last_error_code: row
                .try_get::<Option<String>, _>("last_error_code")?
                .as_deref()
                .map(safe_delivery_error_code),
            updated_at_epoch: row.try_get("updated_at")?,
        });
    }
    let next_cursor = has_more
        .then(|| notices.last().map(|notice| notice.fingerprint.clone()))
        .flatten();
    no_store(Json(NoticeOperationsResponse {
        states,
        notices,
        next_cursor,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NoticeOperationRequest {
    operator: String,
    action: NoticeOperationAction,
}

#[derive(Debug, Deserialize, Copy, Clone)]
#[serde(rename_all = "snake_case")]
enum NoticeOperationAction {
    Retry,
    Suppress,
}

#[derive(Debug, Serialize)]
struct NoticeOperationResult {
    accepted: bool,
}

async fn apply_notice_operation(
    State(state): State<AppState>,
    Path(fingerprint): Path<String>,
    headers: HeaderMap,
    Json(request): Json<NoticeOperationRequest>,
) -> Result<Response> {
    require_notice_operator(&state, &headers)?;
    if !is_notice_fingerprint(&fingerprint)
        || request.operator.trim().is_empty()
        || request.operator.len() > 80
        || request.operator.chars().any(char::is_control)
    {
        return Err(Error::BadRequest("notice operation is invalid".into()));
    }
    let action_name = match request.action {
        NoticeOperationAction::Retry => "retry",
        NoticeOperationAction::Suppress => "suppress",
    };
    let mut tx = state.pool.begin().await?;
    let notice_id: Option<String> = sqlx::query_scalar(
        "SELECT notice_id FROM cloud_notice_outbox WHERE operator_fingerprint=$1 FOR UPDATE",
    )
    .bind(&fingerprint)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(notice_id) = notice_id else {
        return Err(Error::NotFound("notice not found".into()));
    };
    let applied = match request.action {
        NoticeOperationAction::Retry => sqlx::query(
            "UPDATE cloud_notice_outbox SET status='pending',attempt_count=0, \
                 operator_retry_count=operator_retry_count+1,available_at=now(), \
                 provider_first_attempt_at=NULL,last_error_code=NULL,updated_at=now() \
             WHERE notice_id=$1 AND status='failed' \
               AND last_error_code IN ('provider_rejected_request','authority_unavailable') \
               AND provider_message_id IS NULL AND operator_retry_count<2 \
               AND (last_error_code='authority_unavailable' OR provider_first_attempt_at IS NULL \
                    OR provider_first_attempt_at>now()-interval '23 hours') \
               AND EXISTS (SELECT 1 FROM cloud_verified_contacts contact \
                 WHERE contact.contact_id=cloud_notice_outbox.contact_id \
                   AND contact.user_id=cloud_notice_outbox.recipient_user_id \
                   AND contact.channel='email' AND contact.revoked_at IS NULL)",
        )
        .bind(&notice_id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1,
        NoticeOperationAction::Suppress => sqlx::query(
            "UPDATE cloud_notice_outbox SET status='suppressed',last_error_code='operator_suppressed', \
                 lease_owner=NULL,lease_expires_at=NULL,updated_at=now() \
             WHERE notice_id=$1 AND status IN ('pending','failed')",
        )
        .bind(&notice_id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1,
    };
    sqlx::query(
        "INSERT INTO cloud_notice_operator_actions(notice_fingerprint,operator_label,action,applied) \
         VALUES($1,$2,$3,$4)",
    )
    .bind(&fingerprint)
    .bind(request.operator.trim())
    .bind(action_name)
    .bind(applied)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    no_store(Json(NoticeOperationResult { accepted: applied }))
}

fn require_notice_operator(state: &AppState, headers: &HeaderMap) -> Result<()> {
    let expected = state
        .cloud_retention_operator_token
        .as_deref()
        .ok_or_else(|| {
            Error::NotConfigured("Cloud notice operator controls are disabled".into())
        })?;
    let provided = crate::org_deletion_metrics::bearer_token(headers).ok_or(Error::Unauthorized)?;
    if !crate::org_deletion_metrics::token_matches(expected, provided) {
        return Err(Error::Unauthorized);
    }
    Ok(())
}

fn no_store(body: impl IntoResponse) -> Result<Response> {
    let mut response = body.into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

fn is_notice_fingerprint(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn safe_notice_kind(value: &str) -> &'static str {
    match value {
        "checkout_outcome" => "checkout_outcome",
        "failed_renewal" => "failed_renewal",
        "recovery_end" => "recovery_end",
        "export_deadline" => "export_deadline",
        "shared_export_deadline" => "shared_export_deadline",
        "impending_purge" => "impending_purge",
        "sponsorship_change" => "sponsorship_change",
        "payer_transfer_invitation" => "payer_transfer_invitation",
        "payer_transfer_outcome" => "payer_transfer_outcome",
        "price_change" => "price_change",
        "recovery_resolved" => "recovery_resolved",
        "sponsored_renewal_failure" => "sponsored_renewal_failure",
        "sponsored_renewal_action" => "sponsored_renewal_action",
        _ => "unknown",
    }
}

fn safe_notice_status(value: &str) -> &'static str {
    match value {
        "pending" => "pending",
        "leased" => "leased",
        "accepted" => "accepted",
        "delivery_unknown" => "delivery_unknown",
        "delivered" => "delivered",
        "cancelled" => "cancelled",
        "failed" => "failed",
        "bounced" => "bounced",
        "suppressed" => "suppressed",
        _ => "unknown",
    }
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

#[derive(Debug, Deserialize)]
struct ResendWebhookEvent {
    #[serde(rename = "type")]
    event_type: String,
    data: ResendWebhookData,
}

#[derive(Debug, Deserialize)]
struct ResendWebhookData {
    email_id: String,
}

async fn resend_webhook(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Result<axum::http::StatusCode> {
    if body.len() > 64 * 1024 {
        return Err(Error::BadRequest("provider event is too large".into()));
    }
    let secret = state
        .resend_webhook_secret
        .as_deref()
        .ok_or_else(|| Error::NotFound("provider webhook is unavailable".into()))?;
    let header = |name: &'static str| -> Result<&str> {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .ok_or(Error::Unauthorized)
    };
    verify_resend_signature(
        secret,
        header("svix-id")?,
        header("svix-timestamp")?,
        header("svix-signature")?,
        &body,
        epoch_now(),
    )
    .map_err(|_| Error::Unauthorized)?;
    let event: ResendWebhookEvent = serde_json::from_slice(&body)
        .map_err(|_| Error::BadRequest("invalid provider event".into()))?;
    let outcome = match event.event_type.as_str() {
        "email.sent" => "sent",
        "email.delivered" => "delivered",
        "email.bounced" => "bounced",
        "email.failed" => "failed",
        "email.complained" => "complained",
        // Delays are not terminal delivery evidence. Persist no contact or provider payload data.
        "email.delivery_delayed" => return Ok(axum::http::StatusCode::OK),
        _ => return Ok(axum::http::StatusCode::OK),
    };
    record_provider_event(
        &state.pool,
        header("svix-id")?,
        &event.data.email_id,
        outcome,
    )
    .await
    .map_err(|error| match error {
        NoticeError::Database(error) => Error::Db(error),
        other => Error::Internal(other.to_string()),
    })?;
    Ok(axum::http::StatusCode::OK)
}

fn verify_resend_signature(
    secret: &str,
    event_id: &str,
    timestamp: &str,
    signature_header: &str,
    body: &[u8],
    now_epoch: i64,
) -> NoticeResult<()> {
    use base64::Engine;
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    let secret = secret
        .strip_prefix("whsec_")
        .ok_or(NoticeError::InvalidProviderSignature)?;
    let secret = base64::engine::general_purpose::STANDARD
        .decode(secret)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(secret))
        .map_err(|_| NoticeError::InvalidProviderSignature)?;
    if event_id.is_empty() || event_id.len() > 256 || now_epoch <= 0 {
        return Err(NoticeError::InvalidProviderSignature);
    }
    let timestamp_epoch = timestamp
        .parse::<i64>()
        .map_err(|_| NoticeError::InvalidProviderSignature)?;
    if timestamp_epoch.abs_diff(now_epoch) > 300 {
        return Err(NoticeError::InvalidProviderSignature);
    }
    let mut signed = Vec::with_capacity(event_id.len() + timestamp.len() + body.len() + 2);
    signed.extend_from_slice(event_id.as_bytes());
    signed.push(b'.');
    signed.extend_from_slice(timestamp.as_bytes());
    signed.push(b'.');
    signed.extend_from_slice(body);
    let mut valid = false;
    for candidate in signature_header.split_ascii_whitespace() {
        let Some(signature) = candidate.strip_prefix("v1,") else {
            continue;
        };
        let Ok(signature) = base64::engine::general_purpose::STANDARD.decode(signature) else {
            continue;
        };
        let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(&secret) else {
            return Err(NoticeError::InvalidProviderSignature);
        };
        mac.update(&signed);
        valid |= mac.verify_slice(&signature).is_ok();
    }
    if valid {
        Ok(())
    } else {
        Err(NoticeError::InvalidProviderSignature)
    }
}

pub async fn record_provider_event(
    pool: &PgPool,
    provider_event_id: &str,
    provider_message_id: &str,
    outcome: &str,
) -> NoticeResult<bool> {
    if provider_event_id.trim().is_empty()
        || provider_event_id.len() > 256
        || provider_message_id.trim().is_empty()
        || provider_message_id.len() > 256
        || !matches!(
            outcome,
            "sent" | "delivered" | "bounced" | "failed" | "complained"
        )
    {
        return Err(NoticeError::InvalidProviderEvent);
    }
    let mut tx = pool.begin().await?;
    let inserted = sqlx::query(
        "INSERT INTO cloud_notice_provider_events(provider_event_id,provider_message_id,outcome) \
         VALUES($1,$2,$3) ON CONFLICT(provider_event_id) DO NOTHING",
    )
    .bind(provider_event_id)
    .bind(provider_message_id)
    .bind(outcome)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if inserted == 1 {
        apply_provider_events_in_tx(&mut tx, provider_message_id).await?;
    }
    sqlx::query(
        "DELETE FROM cloud_notice_provider_events WHERE received_at < now()-interval '90 days' \
         AND provider_event_id IN (SELECT provider_event_id FROM cloud_notice_provider_events \
           WHERE received_at < now()-interval '90 days' ORDER BY received_at LIMIT 500)",
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(inserted == 1)
}

async fn apply_provider_events_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    provider_message_id: &str,
) -> NoticeResult<()> {
    let result = sqlx::query(
        "UPDATE cloud_notice_outbox AS notice SET \
           status=CASE \
             WHEN EXISTS (SELECT 1 FROM cloud_notice_provider_events event WHERE event.provider_message_id=$1 AND event.outcome IN ('bounced','complained')) THEN 'bounced' \
             WHEN EXISTS (SELECT 1 FROM cloud_notice_provider_events event WHERE event.provider_message_id=$1 AND event.outcome='delivered') THEN 'delivered' \
             WHEN EXISTS (SELECT 1 FROM cloud_notice_provider_events event WHERE event.provider_message_id=$1 AND event.outcome='failed') THEN 'failed' \
             ELSE 'accepted' END, \
           provider_delivered_at=CASE WHEN EXISTS (SELECT 1 FROM cloud_notice_provider_events event WHERE event.provider_message_id=$1 AND event.outcome='delivered') THEN COALESCE(provider_delivered_at,now()) ELSE provider_delivered_at END, \
           delivered_at=CASE WHEN EXISTS (SELECT 1 FROM cloud_notice_provider_events event WHERE event.provider_message_id=$1 AND event.outcome='delivered') THEN COALESCE(delivered_at,now()) ELSE delivered_at END, \
           last_error_code=CASE \
             WHEN EXISTS (SELECT 1 FROM cloud_notice_provider_events event WHERE event.provider_message_id=$1 AND event.outcome IN ('bounced','complained')) THEN 'provider_bounced' \
             WHEN EXISTS (SELECT 1 FROM cloud_notice_provider_events event WHERE event.provider_message_id=$1 AND event.outcome='failed') THEN 'provider_failed' \
             ELSE NULL END, lease_owner=NULL,lease_expires_at=NULL,updated_at=now() \
         WHERE notice.provider_message_id=$1 AND notice.channel='email'",
    )
    .bind(provider_message_id)
    .execute(&mut **tx)
    .await?;
    if result.rows_affected() == 1 {
        sqlx::query(
            "UPDATE cloud_verified_contacts contact SET revoked_at=COALESCE(revoked_at,now()) \
             FROM cloud_notice_outbox notice WHERE notice.provider_message_id=$1 \
               AND notice.contact_id=contact.contact_id AND contact.revoked_at IS NULL \
               AND EXISTS (SELECT 1 FROM cloud_notice_provider_events event \
                 WHERE event.provider_message_id=$1 AND event.outcome IN ('bounced','complained'))",
        )
        .bind(provider_message_id)
        .execute(&mut **tx)
        .await?;
        sqlx::query(
            "UPDATE cloud_notice_outbox SET status='suppressed', last_error_code='contact_bounced', \
                 lease_owner=NULL,lease_expires_at=NULL,updated_at=now() \
             WHERE channel='email' AND status IN ('pending','leased') \
               AND contact_id=(SELECT contact_id FROM cloud_notice_outbox WHERE provider_message_id=$1)",
        )
        .bind(provider_message_id)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoticeLease {
    pub notice_id: String,
    pub recipient_user_id: String,
    pub kind: String,
    pub channel: String,
    pub contact_id: Option<String>,
    pub scope_id: Option<String>,
    pub content: NoticeContent,
    pub worker_id: String,
    pub attempt_count: i32,
    pub provider_message_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoticeDelivery {
    /// Stable across retries. The configured relay must receive this as its idempotency key.
    pub message_id: String,
    pub destination: String,
    pub kind: String,
    pub content: NoticeContent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryOutcome {
    Accepted { provider_message_id: String },
    Unknown { code: String },
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
    // Resend retains idempotency keys for 24 hours. Stop automatic sends an hour early so a
    // delayed worker can never accidentally turn an unknown outcome into a duplicate email.
    sqlx::query(
        "UPDATE cloud_notice_outbox SET status='delivery_unknown', lease_owner=NULL, \
             lease_expires_at=NULL, last_error_code='provider_idempotency_window_expired', updated_at=now() \
         WHERE channel='email' AND provider_first_attempt_at <= now()-interval '23 hours' \
           AND (status='pending' OR (status='leased' AND lease_expires_at <= now()))",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE cloud_notice_outbox SET status='suppressed', last_error_code='notice_expired', \
             lease_owner=NULL, lease_expires_at=NULL, updated_at=now() \
         WHERE channel='email' AND kind <> 'price_change' AND status='pending' \
           AND due_at<=now() AND created_at < now()-interval '30 days'",
    )
    .execute(&mut *tx)
    .await?;
    let row = sqlx::query(
        "SELECT notice_id FROM cloud_notice_outbox \
         WHERE channel='email' AND due_at <= now() AND available_at <= now() \
           AND (status='pending' OR (status='leased' AND lease_expires_at <= now())) \
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
             lease_expires_at=now()+interval '5 minutes', attempt_count=attempt_count+1, \
             updated_at=now() \
         WHERE notice_id=$2 AND (status='pending' OR (status='leased' AND lease_expires_at <= now())) \
         RETURNING recipient_user_id, kind, channel, contact_id, scope_id, payload, attempt_count, provider_message_id",
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
        scope_id: row.try_get("scope_id")?,
        content,
        worker_id: worker_id.to_owned(),
        attempt_count: row.try_get("attempt_count")?,
        provider_message_id: row.try_get("provider_message_id")?,
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
    debug_assert_eq!(lease.channel, NoticeChannel::Email.as_str());
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
    match notice_is_current(pool, &lease).await? {
        Some(true) => {}
        Some(false) => {
            suppress(pool, &lease, "notice_no_longer_current").await?;
            return Ok(true);
        }
        None => {
            retry(pool, &lease, "authority_unavailable").await?;
            return Ok(true);
        }
    }
    record_first_provider_attempt(pool, &lease).await?;
    let outcome = sender
        .send(NoticeDelivery {
            message_id: lease.notice_id.clone(),
            destination,
            kind: lease.kind.clone(),
            content: lease.content.clone(),
        })
        .await;
    match outcome {
        DeliveryOutcome::Accepted {
            provider_message_id,
        } => record_acceptance(pool, &lease, &provider_message_id).await?,
        DeliveryOutcome::Unknown { code } => retry(pool, &lease, &code).await?,
        DeliveryOutcome::Retry { code } => retry(pool, &lease, &code).await?,
        DeliveryOutcome::PermanentFailure { code } => permanently_fail(pool, &lease, &code).await?,
    }
    Ok(true)
}

async fn record_first_provider_attempt(pool: &PgPool, lease: &NoticeLease) -> NoticeResult<()> {
    let affected = sqlx::query(
        "UPDATE cloud_notice_outbox SET provider_first_attempt_at=COALESCE(provider_first_attempt_at,now()), \
             updated_at=now() WHERE notice_id=$1 AND status='leased' AND lease_owner=$2 \
               AND lease_expires_at>now()",
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

async fn notice_is_current(pool: &PgPool, lease: &NoticeLease) -> NoticeResult<Option<bool>> {
    if lease.kind == "shared_export_deadline" {
        let current: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM cloud_notice_outbox notice \
             JOIN organizations org ON org.id=notice.scope_id AND org.lifecycle_state='active' \
             JOIN organization_memberships membership ON membership.org_id=org.id \
               AND membership.user_id=notice.recipient_user_id AND membership.role IN ('owner','admin') \
             JOIN cloud_retention_jobs job ON job.organisation_id=org.id \
               AND job.scope_kind='shared_organisation' AND job.notice_event_key=notice.event_key \
             WHERE notice.notice_id=$1 AND notice.channel='email' AND notice.status='leased' \
               AND notice.lease_owner=$2 AND notice.lease_expires_at>now() \
               AND notice.policy_key='shared-export-deadline-v1' \
               AND job.state IN ('ready','dry_run') AND job.dry_run=TRUE AND job.purge_enabled=FALSE \
               AND job.deadline_at>now() AND notice.due_at<=now())",
        )
        .bind(&lease.notice_id)
        .bind(&lease.worker_id)
        .fetch_one(pool)
        .await?;
        return Ok(Some(current));
    }
    let current: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM cloud_notice_outbox notice \
         JOIN cloud_verified_contacts contact ON contact.contact_id=notice.contact_id \
         WHERE notice.notice_id=$1 AND notice.recipient_user_id=$2 AND notice.channel='email' \
           AND notice.status='leased' AND notice.lease_owner=$3 AND notice.lease_expires_at>now() \
           AND notice.due_at<=now() AND contact.user_id=notice.recipient_user_id \
           AND contact.channel='email' AND contact.revoked_at IS NULL \
           AND ($4 <> 'failed_renewal' OR notice.policy_key <> 'personal-renewal-failure-v1' OR EXISTS ( \
             SELECT 1 FROM billing_personal_accounts account \
             WHERE account.user_id=notice.recipient_user_id AND account.state='past_due' \
               AND account.paid_through_epoch=$6)) \
           AND ($4 <> 'sponsored_renewal_failure' OR (notice.scope_id=$5 AND EXISTS ( \
             SELECT 1 FROM organizations org \
             JOIN billing_sponsored_subscriptions sub ON sub.organization_id=org.id \
             JOIN billing_sponsored_seats seat ON seat.organization_id=org.id \
             WHERE org.id=notice.scope_id AND org.lifecycle_state='active' \
               AND sub.status='past_due' AND seat.beneficiary_id=notice.recipient_user_id \
               AND seat.state IN ('active','scheduled_removal') \
               AND seat.effective_from <= $6 \
               AND (seat.effective_until IS NULL OR seat.effective_until >= $6)))) \
           AND ($4 <> 'sponsored_renewal_action' OR (notice.scope_id=$5 AND EXISTS ( \
             SELECT 1 FROM organization_memberships membership \
             JOIN organizations org ON org.id=membership.org_id \
             JOIN billing_sponsored_subscriptions sub ON sub.organization_id=org.id \
             WHERE membership.org_id=notice.scope_id AND membership.user_id=notice.recipient_user_id \
               AND membership.role IN ('owner','admin') AND org.lifecycle_state='active' \
               AND sub.status='past_due'))) \
           AND ($4 <> 'payer_transfer_invitation' OR EXISTS ( \
             SELECT 1 FROM billing_transfer_intents transfer \
             WHERE transfer.transfer_id=notice.event_key \
               AND transfer.counterparty_user_id=notice.recipient_user_id \
               AND transfer.state='awaiting_consent' \
               AND transfer.consent_expires_at_epoch > extract(epoch FROM now())::bigint)) \
           AND ($4 <> 'price_change' OR EXISTS ( \
             SELECT 1 FROM billing_founding_price_notice_state notice_state \
             JOIN billing_founding_awards award ON award.award_id=notice_state.award_id \
             JOIN billing_founding_schedule_jobs schedule ON schedule.award_id=award.award_id \
             WHERE notice_state.outcome='queued' AND notice_state.schedule_version=schedule.version \
               AND schedule.state='scheduled' AND notice_state.payer_kind=award.payer_kind \
               AND notice_state.payer_id=award.payer_id \
               AND notice.event_key='founding-price-change:' || award.award_id || ':' || notice_state.transition_epoch::text \
               AND ((award.payer_kind='personal' AND award.payer_id=notice.recipient_user_id AND EXISTS ( \
                 SELECT 1 FROM billing_personal_accounts account WHERE account.user_id=notice.recipient_user_id \
                   AND account.state IN ('active','past_due') AND NOT account.cancel_at_period_end \
                   AND account.cancellation_requested_at IS NULL \
                   AND account.stripe_subscription_id=schedule.provider_subscription_id)) \
                 OR (award.payer_kind='sponsor' AND EXISTS ( \
                   SELECT 1 FROM organization_memberships membership JOIN organizations org ON org.id=membership.org_id \
                   JOIN billing_sponsored_subscriptions sub ON sub.organization_id=org.id \
                   JOIN billing_sponsored_seats seat ON seat.organization_id=org.id AND seat.beneficiary_id=award.beneficiary_id \
                   WHERE membership.user_id=notice.recipient_user_id AND membership.org_id=award.payer_id \
                     AND membership.role IN ('owner','admin') AND org.lifecycle_state='active' \
                     AND sub.status IN ('active','past_due') AND sub.provider_subscription_id=schedule.provider_subscription_id \
                     AND seat.state IN ('active','scheduled_removal') \
                     AND seat.effective_from<=notice_state.transition_epoch \
                     AND (seat.effective_until IS NULL OR seat.effective_until>notice_state.transition_epoch)))))))",
    )
    .bind(&lease.notice_id)
    .bind(&lease.recipient_user_id)
    .bind(&lease.worker_id)
    .bind(&lease.kind)
    .bind(lease.scope_id.as_deref())
    .bind(lease.content.effective_at_epoch)
    .fetch_one(pool)
    .await?;
    if !current {
        return Ok(Some(false));
    }
    if lease.kind == "payer_transfer_invitation" {
        let current: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM cloud_notice_outbox notice \
             JOIN billing_transfer_intents transfer ON transfer.transfer_id=notice.event_key \
             WHERE notice.notice_id=$1 AND notice.recipient_user_id=$2 \
               AND transfer.counterparty_user_id=notice.recipient_user_id \
               AND transfer.state='awaiting_consent' \
               AND transfer.consent_expires_at_epoch>extract(epoch FROM now())::bigint \
               AND ( \
                 ((transfer.source_kind='personal' AND transfer.actor_user_id=transfer.beneficiary_id) \
                   OR (transfer.source_kind='sponsor' AND EXISTS ( \
                     SELECT 1 FROM organization_memberships membership \
                     JOIN organizations organization ON organization.id=membership.org_id \
                     WHERE membership.org_id=transfer.source_organization_id \
                       AND membership.user_id=transfer.actor_user_id \
                       AND membership.role IN ('owner','admin') \
                       AND organization.lifecycle_state='active'))) \
                  AND ((transfer.destination_kind='personal' AND transfer.counterparty_user_id=transfer.beneficiary_id) \
                    OR (transfer.destination_kind='sponsor' AND EXISTS ( \
                      SELECT 1 FROM organization_memberships membership \
                      JOIN organizations organization ON organization.id=membership.org_id \
                      WHERE membership.org_id=transfer.destination_organization_id \
                        AND membership.user_id=transfer.counterparty_user_id \
                        AND membership.role IN ('owner','admin') \
                        AND organization.lifecycle_state='active'))) \
                  OR \
                 ((transfer.destination_kind='personal' AND transfer.actor_user_id=transfer.beneficiary_id) \
                   OR (transfer.destination_kind='sponsor' AND EXISTS ( \
                     SELECT 1 FROM organization_memberships membership \
                     JOIN organizations organization ON organization.id=membership.org_id \
                     WHERE membership.org_id=transfer.destination_organization_id \
                       AND membership.user_id=transfer.actor_user_id \
                       AND membership.role IN ('owner','admin') \
                       AND organization.lifecycle_state='active'))) \
                  AND ((transfer.source_kind='personal' AND transfer.counterparty_user_id=transfer.beneficiary_id) \
                    OR (transfer.source_kind='sponsor' AND EXISTS ( \
                      SELECT 1 FROM organization_memberships membership \
                      JOIN organizations organization ON organization.id=membership.org_id \
                      WHERE membership.org_id=transfer.source_organization_id \
                        AND membership.user_id=transfer.counterparty_user_id \
                        AND membership.role IN ('owner','admin') \
                        AND organization.lifecycle_state='active')))) \
               AND ( \
                 (transfer.source_kind='personal' AND EXISTS ( \
                   SELECT 1 FROM billing_personal_accounts account \
                   WHERE account.user_id=transfer.beneficiary_id AND account.state='active' \
                     AND NOT account.cancel_at_period_end AND account.cancellation_requested_at IS NULL \
                     AND account.paid_through_epoch=transfer.effective_from \
                     AND account.stripe_subscription_id=transfer.source_provider_subscription_id)) \
                 OR (transfer.source_kind='sponsor' AND EXISTS ( \
                   SELECT 1 FROM billing_sponsored_seats seat \
                   JOIN billing_sponsored_subscriptions subscription \
                     ON subscription.organization_id=seat.organization_id \
                   JOIN organizations organization ON organization.id=seat.organization_id \
                   WHERE seat.organization_id=transfer.source_organization_id \
                     AND seat.beneficiary_id=transfer.beneficiary_id AND seat.state='active' \
                     AND seat.effective_from<=extract(epoch FROM now())::bigint \
                     AND (seat.effective_until IS NULL OR seat.effective_until>=transfer.effective_from) \
                     AND subscription.status='active' \
                     AND subscription.current_period_end=transfer.effective_from \
                     AND subscription.provider_subscription_id=transfer.source_provider_subscription_id \
                     AND organization.lifecycle_state='active'))) \
               AND ( \
                 (transfer.destination_kind='personal' AND NOT EXISTS ( \
                   SELECT 1 FROM billing_personal_accounts account \
                   WHERE account.user_id=transfer.beneficiary_id \
                     AND account.state IN ('pending','active','past_due','refund_required'))) \
                 OR (transfer.destination_kind='sponsor' AND NOT EXISTS ( \
                   SELECT 1 FROM billing_sponsored_seats seat \
                   WHERE seat.organization_id=transfer.destination_organization_id \
                     AND seat.beneficiary_id=transfer.beneficiary_id \
                     AND seat.state IN ('pending','active','scheduled_removal')))) \
               AND (transfer.destination_kind='personal' OR EXISTS ( \
                 SELECT 1 FROM billing_sponsored_subscriptions destination_subscription \
                 JOIN organizations destination_organization \
                   ON destination_organization.id=destination_subscription.organization_id \
                 WHERE destination_subscription.organization_id=transfer.destination_organization_id \
                   AND destination_subscription.status='active' \
                   AND destination_subscription.provider_subscription_id IS NOT NULL \
                   AND destination_subscription.current_period_end=transfer.effective_from \
                   AND destination_organization.lifecycle_state='active')))",
        )
        .bind(&lease.notice_id)
        .bind(&lease.recipient_user_id)
        .fetch_one(pool)
        .await?;
        if !current {
            return Ok(Some(false));
        }
    }
    if lease.kind == "sponsorship_change" {
        let current: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM cloud_notice_outbox notice \
             JOIN billing_sponsored_operations operation ON operation.operation_id=notice.event_key \
             JOIN organizations organization ON organization.id=operation.organization_id \
             JOIN billing_sponsored_subscriptions subscription \
               ON subscription.organization_id=operation.organization_id \
              AND subscription.provider_subscription_id=operation.provider_subscription_id \
             WHERE notice.notice_id=$1 AND operation.organization_id=notice.scope_id \
               AND operation.state IN ('provider_pending','active') \
               AND organization.lifecycle_state='active' AND subscription.status IN ('active','past_due') \
               AND operation.effective_from=$3 \
               AND ( \
                 (operation.beneficiary_id=$2 AND ( \
                   (operation.action='add' AND EXISTS (SELECT 1 FROM billing_sponsored_seats seat \
                     WHERE seat.operation_id=operation.operation_id AND seat.beneficiary_id=$2 \
                       AND seat.state IN ('pending','active') AND seat.effective_from=operation.effective_from)) \
                   OR (operation.action IN ('remove','replace') AND EXISTS (SELECT 1 FROM billing_sponsored_seats seat \
                     WHERE seat.organization_id=operation.organization_id AND seat.beneficiary_id=$2 \
                       AND seat.state='scheduled_removal' AND seat.effective_until IS NOT NULL)))) \
                 OR (operation.replacement_beneficiary_id=$2 AND EXISTS (SELECT 1 FROM billing_sponsored_seats seat \
                   WHERE seat.operation_id=operation.operation_id AND seat.beneficiary_id=$2 \
                     AND seat.state IN ('pending','active') AND seat.effective_from=operation.effective_from)) \
                 OR EXISTS (SELECT 1 FROM organization_memberships membership \
                   WHERE membership.org_id=operation.organization_id AND membership.user_id=$2 \
                     AND membership.role IN ('owner','admin'))))",
        )
        .bind(&lease.notice_id)
        .bind(&lease.recipient_user_id)
        .bind(lease.content.effective_at_epoch.unwrap_or_default())
        .fetch_one(pool)
        .await?;
        if !current {
            return Ok(Some(false));
        }
    }
    if lease.kind == "payer_transfer_outcome" {
        let current: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM cloud_notice_outbox notice \
             JOIN billing_transfer_intents transfer ON transfer.transfer_id=notice.event_key \
             WHERE notice.notice_id=$1 AND transfer.state='completed' AND ( \
               transfer.beneficiary_id=$2 OR EXISTS (SELECT 1 FROM organization_memberships membership \
                 JOIN organizations organization ON organization.id=membership.org_id \
                 WHERE membership.user_id=$2 AND membership.role IN ('owner','admin') \
                   AND organization.lifecycle_state='active' \
                   AND organization.id IN (transfer.source_organization_id,transfer.destination_organization_id))))",
        )
        .bind(&lease.notice_id)
        .bind(&lease.recipient_user_id)
        .fetch_one(pool)
        .await?;
        if !current {
            return Ok(Some(false));
        }
    }
    if matches!(lease.kind.as_str(), "recovery_end" | "export_deadline") {
        let now = epoch_now();
        let authority =
            crate::cloud_coverage_authority::load_current(pool, &lease.recipient_user_id, now)
                .await;
        let Ok(crate::cloud_coverage_authority::AuthorityState::Current { loaded, .. }) = authority
        else {
            // Missing or stale authority cannot prove that a recovery or export boundary still
            // applies. Keep the message retryable instead of treating uncertainty as expiry.
            return Ok(None);
        };
        let Ok(decision) = crate::cloud_coverage::evaluate(&loaded.coverage, now) else {
            return Ok(None);
        };
        let current = match lease.kind.as_str() {
            "recovery_end" => {
                recovery_end_notice_is_current(decision, lease.content.deadline_epoch)
            }
            "export_deadline" => {
                decision.state == crate::cloud_coverage::CoverageState::ExportOnly
                    && decision.export_until == lease.content.deadline_epoch
            }
            _ => unreachable!(),
        };
        return Ok(Some(current));
    }
    Ok(Some(true))
}

fn recovery_end_notice_is_current(
    decision: crate::cloud_coverage::CoverageDecision,
    expected_recovery_until: Option<i64>,
) -> bool {
    expected_recovery_until.is_some_and(|expected| {
        decision.state == crate::cloud_coverage::CoverageState::RenewalRecovery
            && decision.recovery_until == Some(expected)
    })
}

pub fn recovery_end_notice_due_at(recovery_until_epoch: i64) -> NoticeResult<i64> {
    recovery_until_epoch
        .checked_sub(RECOVERY_END_NOTICE_LEAD_SECONDS)
        .filter(|due_at| *due_at > 0)
        .ok_or(NoticeError::InvalidContent(
            "recovery warning date is out of range",
        ))
}

async fn record_acceptance(
    pool: &PgPool,
    lease: &NoticeLease,
    provider_message_id: &str,
) -> NoticeResult<()> {
    if provider_message_id.trim().is_empty() || provider_message_id.len() > 256 {
        return permanently_fail(pool, lease, "provider_response_invalid").await;
    }
    let mut tx = pool.begin().await?;
    let affected = sqlx::query(
        "UPDATE cloud_notice_outbox SET status='accepted', provider_message_id=$1, \
             provider_accepted_at=now(), lease_owner=NULL, lease_expires_at=NULL, \
             last_error_code=NULL, updated_at=now() \
         WHERE notice_id=$2 AND channel='email' AND status='leased' \
           AND lease_owner=$3 AND lease_expires_at>now()",
    )
    .bind(provider_message_id)
    .bind(&lease.notice_id)
    .bind(&lease.worker_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if affected != 1 {
        return Err(NoticeError::LeaseLost);
    }
    apply_provider_events_in_tx(&mut tx, provider_message_id).await?;
    tx.commit().await?;
    Ok(())
}

async fn retry(pool: &PgPool, lease: &NoticeLease, code: &str) -> NoticeResult<()> {
    let code = safe_delivery_error_code(code);
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
    .bind(safe_delivery_error_code(code))
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

async fn suppress(pool: &PgPool, lease: &NoticeLease, code: &str) -> NoticeResult<()> {
    let affected = sqlx::query(
        "UPDATE cloud_notice_outbox SET status='suppressed', lease_owner=NULL, lease_expires_at=NULL, \
             last_error_code=$1, updated_at=now() \
         WHERE notice_id=$2 AND status='leased' AND lease_owner=$3 AND lease_expires_at>now()",
    )
    .bind(safe_delivery_error_code(code))
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

fn safe_delivery_error_code(code: &str) -> &'static str {
    match code {
        "contact_missing" => "contact_missing",
        "contact_unavailable" => "contact_unavailable",
        "provider_outcome_unknown" => "provider_outcome_unknown",
        "provider_response_invalid" => "provider_response_invalid",
        "provider_temporarily_unavailable" => "provider_temporarily_unavailable",
        "provider_rejected_request" => "provider_rejected_request",
        "provider_idempotency_window_expired" => "provider_idempotency_window_expired",
        "message_identity_invalid" => "message_identity_invalid",
        "notice_kind_unsupported" => "notice_kind_unsupported",
        "notice_no_longer_current" => "notice_no_longer_current",
        "notice_expired" => "notice_expired",
        "contact_bounced" => "contact_bounced",
        "authority_unavailable" => "authority_unavailable",
        "operator_suppressed" => "operator_suppressed",
        _ => "provider_error",
    }
}

pub fn spawn<S>(pool: PgPool, mut sender: S)
where
    S: NoticeSender + 'static,
{
    tokio::spawn(async move {
        let worker_id = format!("cloud-notices:{}", Uuid::new_v4());
        loop {
            let mut processed = 0;
            while processed < MAX_NOTICES_PER_WORKER_TICK {
                match run_once(&pool, &worker_id, &mut sender).await {
                    Ok(true) => processed += 1,
                    Ok(false) => break,
                    Err(error) => {
                        eprintln!("Cloud notice delivery worker failed: {error}");
                        break;
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    });
}

fn epoch_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
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
            price_interval: None,
        }
    }

    #[test]
    fn resend_webhook_signature_is_bound_to_body_and_fresh_timestamp() {
        use base64::Engine;
        use hmac::{Hmac, Mac};
        use sha2::Sha256;

        let key = b"test webhook signing key";
        let secret = format!(
            "whsec_{}",
            base64::engine::general_purpose::STANDARD.encode(key)
        );
        let event_id = "evt_test";
        let timestamp = "1900000000";
        let body = br#"{"type":"email.delivered","data":{"email_id":"msg_test"}}"#;
        let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("hmac key");
        mac.update(event_id.as_bytes());
        mac.update(b".");
        mac.update(timestamp.as_bytes());
        mac.update(b".");
        mac.update(body);
        let signature =
            base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
        let header = format!("v1,{signature}");

        assert!(verify_resend_signature(
            &secret,
            event_id,
            timestamp,
            &header,
            body,
            1_900_000_000
        )
        .is_ok());
        assert!(matches!(
            verify_resend_signature(
                &secret,
                event_id,
                timestamp,
                &header,
                b"different body",
                1_900_000_000
            ),
            Err(NoticeError::InvalidProviderSignature)
        ));
        assert!(matches!(
            verify_resend_signature(&secret, event_id, timestamp, &header, body, 1_900_000_301),
            Err(NoticeError::InvalidProviderSignature)
        ));
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
    fn absent_price_interval_keeps_legacy_notice_payload_shape() {
        let value = serde_json::to_value(content()).expect("serialise legacy notice content");
        assert!(value.get("price_interval").is_none());

        let mut price_change = content();
        price_change.price_interval = Some("year".into());
        let value = serde_json::to_value(price_change).expect("serialise price change content");
        assert_eq!(value["price_interval"], "year");
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
            scope_id: None,
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
        assert_eq!(
            NoticeKind::SponsoredRenewalFailure.as_str(),
            "sponsored_renewal_failure"
        );
        assert_eq!(
            NoticeKind::SponsoredRenewalAction.as_str(),
            "sponsored_renewal_action"
        );
        assert_eq!(
            NoticeKind::PayerTransferInvitation.as_str(),
            "payer_transfer_invitation"
        );
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

    #[test]
    fn recovery_warning_is_due_three_days_before_its_deadline() {
        let recovery_until = 2_000_000_000;
        assert_eq!(
            recovery_end_notice_due_at(recovery_until).unwrap(),
            recovery_until - RECOVERY_END_NOTICE_LEAD_SECONDS
        );
        assert!(matches!(
            recovery_end_notice_due_at(RECOVERY_END_NOTICE_LEAD_SECONDS),
            Err(NoticeError::InvalidContent(
                "recovery warning date is out of range"
            ))
        ));
    }

    #[test]
    fn recovery_warning_is_current_only_during_the_matching_recovery_period() {
        let recovery_until = 2_000_000_000;
        let recovering = crate::cloud_coverage::CoverageDecision {
            state: crate::cloud_coverage::CoverageState::RenewalRecovery,
            active_until: Some(recovery_until),
            recovery_until: Some(recovery_until),
            export_until: None,
        };
        assert!(recovery_end_notice_is_current(
            recovering,
            Some(recovery_until)
        ));
        assert!(!recovery_end_notice_is_current(
            recovering,
            Some(recovery_until + 1)
        ));

        let export_only = crate::cloud_coverage::CoverageDecision {
            state: crate::cloud_coverage::CoverageState::ExportOnly,
            active_until: None,
            recovery_until: None,
            export_until: Some(recovery_until + EXPORT_WINDOW_SECONDS),
        };
        assert!(!recovery_end_notice_is_current(
            export_only,
            Some(recovery_until)
        ));
    }

    #[test]
    fn export_notice_is_queued_while_paid_for_the_future_window_boundary() {
        let now = 1_900_000_000;
        let view = EligibilityView {
            model: "person_eligibility_v1",
            deployment_mode: "cloud",
            state: EligibilityState::Paid,
            account_initialized: true,
            billing_available: true,
            paid_through_epoch: Some(now + 60),
            recovery_until_epoch: None,
            export_until_epoch: None,
            actions: crate::person_eligibility::EligibilityActions {
                setup: false,
                billing: false,
                export: false,
                revoke: true,
            },
            next_actions: vec!["revoke"],
            payer: Some("personal"),
        };

        assert_eq!(
            export_deadline_notice_window(&view, now).expect("notice window"),
            Some((now + 60, now + 60 + EXPORT_WINDOW_SECONDS))
        );
    }

    #[test]
    fn export_notice_uses_recovery_end_as_the_start_of_the_export_window() {
        let now = 1_900_000_000;
        let recovery_end = now + 14 * 24 * 60 * 60;
        let view = EligibilityView {
            model: "person_eligibility_v1",
            deployment_mode: "cloud",
            state: EligibilityState::RenewalRecovery,
            account_initialized: true,
            billing_available: true,
            paid_through_epoch: Some(recovery_end),
            recovery_until_epoch: Some(recovery_end),
            export_until_epoch: None,
            actions: crate::person_eligibility::EligibilityActions {
                setup: false,
                billing: false,
                export: false,
                revoke: true,
            },
            next_actions: vec!["revoke"],
            payer: Some("personal"),
        };

        assert_eq!(
            export_deadline_notice_window(&view, now).expect("notice window"),
            Some((recovery_end, recovery_end + EXPORT_WINDOW_SECONDS))
        );
    }

    #[test]
    fn export_notice_uses_recovery_boundary_when_it_differs_from_paid_boundary() {
        let now = 1_900_000_000;
        let paid_boundary = now + 2 * 24 * 60 * 60;
        let recovery_end = now + 9 * 24 * 60 * 60;
        let view = EligibilityView {
            model: "person_eligibility_v1",
            deployment_mode: "cloud",
            state: EligibilityState::RenewalRecovery,
            account_initialized: true,
            billing_available: true,
            paid_through_epoch: Some(paid_boundary),
            recovery_until_epoch: Some(recovery_end),
            export_until_epoch: None,
            actions: crate::person_eligibility::EligibilityActions {
                setup: false,
                billing: false,
                export: false,
                revoke: true,
            },
            next_actions: vec!["revoke"],
            payer: Some("personal"),
        };

        assert_eq!(
            export_deadline_notice_window(&view, now).expect("notice window"),
            Some((recovery_end, recovery_end + EXPORT_WINDOW_SECONDS))
        );
    }
}
