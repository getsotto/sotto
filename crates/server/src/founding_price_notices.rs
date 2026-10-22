//! Durable in-app notices for verified founding-to-standard price transitions.
//!
//! This worker consumes only founding schedule rows already marked `scheduled` by the provider
//! read-back path. It never calls Stripe and never changes billing. The local schedule version
//! fences payer changes and cancellation while a notice is being reconciled.

use std::time::Duration;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use sqlx::{PgPool, Postgres, Row, Transaction};

use crate::error::{Error, Result};
use crate::founding_allocator::{FoundingOffer, FoundingPayerKind};
use crate::notifications::{self, NoticeChannel, NoticeContent, NoticeIntent, NoticeKind};
use crate::org_deletion_metrics::{bearer_token, token_matches};
use crate::state::AppState;

const POLICY_KEY: &str = "founding-standard-price-v1";
const MAX_RECIPIENTS: i64 = 128;
const POLL_INTERVAL: Duration = Duration::from_secs(30);
const BATCH_SIZE: i64 = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Queued,
    LateUnresolved,
    Retired,
    ScheduleUnresolved,
}

impl Outcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::LateUnresolved => "late_unresolved",
            Self::Retired => "retired",
            Self::ScheduleUnresolved => "schedule_unresolved",
        }
    }
}

pub fn spawn(pool: PgPool) {
    tokio::spawn(async move {
        loop {
            match run_once(&pool, epoch_now()).await {
                Ok(_) => {}
                Err(error) => eprintln!("founding price notice worker: {error}"),
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    });
}

/// Reconcile one bounded batch of verified founding schedules. `now_epoch` is explicit so the
/// 60-day boundary and delayed-worker behaviour are deterministic in database tests.
pub async fn run_once(pool: &PgPool, now_epoch: i64) -> Result<bool> {
    if now_epoch <= 0 {
        return Err(Error::Internal(
            "founding price notice clock is invalid".into(),
        ));
    }
    let candidates = sqlx::query(
        "SELECT job.award_id FROM billing_founding_schedule_jobs AS job \
         LEFT JOIN billing_founding_price_notice_state AS notice \
           ON notice.award_id = job.award_id \
         WHERE job.price_notice_eligible \
           AND (notice.award_id IS NULL \
             OR notice.schedule_version <> job.version \
             OR notice.payer_kind <> job.payer_kind \
             OR notice.payer_id <> job.payer_id \
             OR (notice.outcome = 'queued' AND job.state <> 'scheduled') \
             OR (notice.outcome = 'schedule_unresolved' AND job.state = 'scheduled') \
             OR (notice.outcome = 'queued' AND notice.next_reconcile_at <= now()) \
             OR (notice.outcome = 'late_unresolved' \
                 AND notice.reason_code IN ('recipient_bound_exceeded','recipient_unavailable') \
                 AND notice.next_reconcile_at <= now())) \
         ORDER BY COALESCE(notice.next_reconcile_at, job.updated_at), job.award_id LIMIT $1",
    )
    .bind(BATCH_SIZE)
    .fetch_all(pool)
    .await
    .map_err(Error::Db)?;
    let mut processed = false;
    let mut first_error = None;
    for row in candidates {
        let award_id: String = row.try_get("award_id").map_err(Error::Db)?;
        match reconcile_award(pool, &award_id, now_epoch).await {
            Ok(()) => processed = true,
            Err(error) => {
                // One corrupt or temporarily failing award must not prevent the rest of this
                // bounded batch from progressing. Return the first failure after trying peers.
                first_error.get_or_insert(error);
            }
        }
    }
    if let Some(error) = first_error {
        Err(error)
    } else {
        Ok(processed)
    }
}

async fn reconcile_award(pool: &PgPool, award_id: &str, now_epoch: i64) -> Result<()> {
    let mut tx = pool.begin().await.map_err(Error::Db)?;
    sqlx::query_scalar::<_, ()>("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("founding-price-notice:{award_id}"))
        .fetch_one(&mut *tx)
        .await
        .map_err(Error::Db)?;

    let row = sqlx::query(
        "SELECT job.version AS schedule_version, job.state AS schedule_state, job.provider_subscription_id, \
                job.payer_kind, job.payer_id, award.offer, award.original_end_epoch \
         FROM billing_founding_schedule_jobs AS job \
         JOIN billing_founding_awards AS award ON award.award_id = job.award_id \
         WHERE job.award_id = $1 AND job.price_notice_eligible FOR UPDATE OF job, award",
    )
    .bind(award_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(Error::Db)?;
    let Some(row) = row else {
        tx.commit().await.map_err(Error::Db)?;
        return Ok(());
    };
    let version: i64 = row.try_get("schedule_version").map_err(Error::Db)?;
    let payer_kind_text: String = row.try_get("payer_kind").map_err(Error::Db)?;
    let payer_id: String = row.try_get("payer_id").map_err(Error::Db)?;
    let identity = NoticeIdentity {
        award_id,
        version,
        payer_kind: &payer_kind_text,
        payer_id: &payer_id,
    };
    let schedule_state: String = row.try_get("schedule_state").map_err(Error::Db)?;
    let subscription_id: String = row.try_get("provider_subscription_id").map_err(Error::Db)?;
    let transition_epoch: Option<i64> = row.try_get("original_end_epoch").map_err(Error::Db)?;
    let Some(transition_epoch) = transition_epoch.filter(|epoch| *epoch > 0) else {
        retire_or_unresolve(
            &mut tx,
            identity,
            None,
            Outcome::ScheduleUnresolved,
            "schedule_not_verified",
        )
        .await?;
        tx.commit().await.map_err(Error::Db)?;
        return Ok(());
    };
    let payer_kind = match payer_kind_text.as_str() {
        "personal" => FoundingPayerKind::Personal,
        "sponsor" => FoundingPayerKind::Sponsor,
        _ => {
            return Err(Error::Internal(
                "founding notice payer kind is corrupt".into(),
            ));
        }
    };
    if schedule_state != "scheduled" {
        let outcome = if schedule_state == "not_required" {
            Outcome::Retired
        } else {
            Outcome::ScheduleUnresolved
        };
        let reason = if outcome == Outcome::Retired {
            "subscription_ending"
        } else {
            "schedule_not_verified"
        };
        retire_or_unresolve(&mut tx, identity, Some(transition_epoch), outcome, reason).await?;
        tx.commit().await.map_err(Error::Db)?;
        return Ok(());
    }

    // Schedule verification is persisted before this worker can see the row. The current local
    // payer and billable state are checked again here, after taking the award-scoped lock.
    let recipients = current_recipients(
        &mut tx,
        award_id,
        payer_kind,
        &payer_id,
        &subscription_id,
        transition_epoch,
    )
    .await?;
    let Some(recipients) = recipients else {
        retire_or_unresolve(
            &mut tx,
            identity,
            Some(transition_epoch),
            Outcome::Retired,
            "subscription_ending",
        )
        .await?;
        tx.commit().await.map_err(Error::Db)?;
        return Ok(());
    };
    if recipients.is_empty() || recipients.len() > MAX_RECIPIENTS as usize {
        retire_or_unresolve(
            &mut tx,
            identity,
            Some(transition_epoch),
            Outcome::LateUnresolved,
            if recipients.len() > MAX_RECIPIENTS as usize {
                "recipient_bound_exceeded"
            } else {
                "recipient_unavailable"
            },
        )
        .await?;
        tx.commit().await.map_err(Error::Db)?;
        return Ok(());
    }

    let due_at = notifications::price_change_due_at(transition_epoch)
        .map_err(|error| Error::Internal(error.to_string()))?;
    let already_queued: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM billing_founding_price_notice_state \
         WHERE award_id=$1 AND transition_epoch=$2 AND schedule_version=$3 \
           AND payer_kind=$4 AND payer_id=$5 AND outcome='queued')",
    )
    .bind(award_id)
    .bind(transition_epoch)
    .bind(version)
    .bind(&payer_kind_text)
    .bind(&payer_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(Error::Db)?;
    // A notice discovered after its send window is a review item. Once the exact transition
    // has been queued on time, however, later reconciliation must preserve it after due_at;
    // otherwise the 30-second poll would cancel it just as it becomes visible.
    if due_at < now_epoch && !already_queued {
        retire_or_unresolve(
            &mut tx,
            identity,
            Some(transition_epoch),
            Outcome::LateUnresolved,
            "notice_window_elapsed",
        )
        .await?;
        tx.commit().await.map_err(Error::Db)?;
        return Ok(());
    }
    let offer_text: String = row.try_get("offer").map_err(Error::Db)?;
    let offer = match offer_text.as_str() {
        "founding_monthly" => FoundingOffer::Monthly,
        "founding_annual" => FoundingOffer::Annual,
        _ => {
            retire_or_unresolve(
                &mut tx,
                identity,
                Some(transition_epoch),
                Outcome::ScheduleUnresolved,
                "schedule_not_verified",
            )
            .await?;
            tx.commit().await.map_err(Error::Db)?;
            return Ok(());
        }
    };
    let standard_offer = offer.standard_billing_offer();
    let amount_pence = standard_offer.expected_amount_pence();
    let interval = match offer {
        FoundingOffer::Monthly => "month",
        FoundingOffer::Annual => "year",
    };
    let interval_label = match offer {
        FoundingOffer::Monthly => "monthly",
        FoundingOffer::Annual => "annual",
    };
    let event_key = event_key(award_id, transition_epoch);
    // A payer transfer supersedes the previous recipient. Cancel old rows before inserting the
    // current payer's copy; the unique outbox key preserves replay idempotency.
    cancel_event_in_tx(&mut tx, &event_key).await?;
    let payload = NoticeContent {
        title: "Your founding price is changing".into(),
        detail: format!("Your founding price ends on the date shown. Your next renewal will use the standard {interval_label} price shown."),
        effective_at_epoch: Some(transition_epoch),
        deadline_epoch: None,
        amount_pence: Some(amount_pence),
        price_interval: Some(interval.into()),
    };
    for recipient in &recipients {
        // Re-enable only the exact same notice. A changed boundary or amount remains a conflict
        // and cannot silently rewrite the customer's original notice.
        let serialized = serde_json::to_value(&payload)
            .map_err(|_| Error::Internal("founding price notice is invalid".into()))?;
        sqlx::query(
            "UPDATE cloud_notice_outbox SET status='pending', cancelled_at=NULL, \
                 available_at=due_at, updated_at=now() \
             WHERE recipient_user_id=$1 AND event_key=$2 AND policy_key=$3 AND channel IN ('in_app','email') \
               AND status='cancelled' AND delivered_at IS NULL AND due_at=to_timestamp($4) \
               AND payload=$5::jsonb",
        )
        .bind(recipient)
        .bind(&event_key)
        .bind(POLICY_KEY)
        .bind(due_at)
        .bind(&serialized)
        .execute(&mut *tx)
        .await
        .map_err(Error::Db)?;
        notifications::enqueue_in_tx(
            &mut tx,
            &NoticeIntent {
                recipient_user_id: recipient.clone(),
                event_key: event_key.clone(),
                policy_key: POLICY_KEY.into(),
                kind: NoticeKind::PriceChange,
                channel: NoticeChannel::InApp,
                contact_id: None,
                scope_id: None,
                due_at_epoch: due_at,
                content: payload.clone(),
            },
        )
        .await
        .map_err(|error| Error::Internal(error.to_string()))?;
    }
    save_outcome(
        &mut tx,
        identity,
        Some(transition_epoch),
        Outcome::Queued,
        None,
    )
    .await?;
    tx.commit().await.map_err(Error::Db)?;
    Ok(())
}

async fn current_recipients(
    tx: &mut Transaction<'_, Postgres>,
    award_id: &str,
    payer_kind: FoundingPayerKind,
    payer_id: &str,
    subscription_id: &str,
    transition_epoch: i64,
) -> Result<Option<Vec<String>>> {
    match payer_kind {
        FoundingPayerKind::Personal => {
            let row = sqlx::query(
                "SELECT account.user_id FROM billing_founding_awards award \
                 JOIN billing_personal_accounts account ON account.user_id=award.beneficiary_id \
                 WHERE award.award_id=$1 AND award.payer_kind='personal' AND award.payer_id=$2 \
                   AND account.state IN ('active','past_due') \
                   AND NOT account.cancel_at_period_end AND account.cancellation_requested_at IS NULL \
                   AND account.stripe_subscription_id=$3",
            )
            .bind(award_id)
            .bind(payer_id)
            .bind(subscription_id)
            .fetch_optional(&mut **tx)
            .await
            .map_err(Error::Db)?;
            Ok(row.map(|row| vec![row.get("user_id")]))
        }
        FoundingPayerKind::Sponsor => {
            let live_seat = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM billing_founding_awards award \
                 JOIN billing_sponsored_seats seat ON seat.beneficiary_id=award.beneficiary_id \
                   AND seat.organization_id=award.payer_id \
                 JOIN organizations org ON org.id=seat.organization_id \
                 JOIN billing_sponsored_subscriptions sub ON sub.organization_id=org.id \
                 WHERE award.award_id=$1 AND award.payer_kind='sponsor' AND award.payer_id=$2 \
                   AND org.lifecycle_state='active' AND sub.provider_subscription_id=$3 \
                   AND sub.status IN ('active','past_due') \
                   AND seat.state IN ('active','scheduled_removal') \
                   AND seat.effective_from <= $4 \
                   AND (seat.effective_until IS NULL OR seat.effective_until > $4))",
            )
            .bind(award_id)
            .bind(payer_id)
            .bind(subscription_id)
            .bind(transition_epoch)
            .fetch_one(&mut **tx)
            .await
            .map_err(Error::Db)?;
            if !live_seat {
                return Ok(None);
            }
            let rows = sqlx::query(
                "SELECT membership.user_id FROM organization_memberships membership \
                 JOIN organizations org ON org.id=membership.org_id \
                 WHERE membership.org_id=$1 AND membership.role IN ('owner','admin') \
                   AND org.lifecycle_state='active' \
                 ORDER BY membership.user_id LIMIT $2",
            )
            .bind(payer_id)
            .bind(MAX_RECIPIENTS + 1)
            .fetch_all(&mut **tx)
            .await
            .map_err(Error::Db)?;
            Ok(Some(
                rows.into_iter().map(|row| row.get("user_id")).collect(),
            ))
        }
    }
}

#[derive(Clone, Copy)]
struct NoticeIdentity<'a> {
    award_id: &'a str,
    version: i64,
    payer_kind: &'a str,
    payer_id: &'a str,
}

async fn retire_or_unresolve(
    tx: &mut Transaction<'_, Postgres>,
    identity: NoticeIdentity<'_>,
    transition_epoch: Option<i64>,
    outcome: Outcome,
    reason: &'static str,
) -> Result<()> {
    if let Some(transition_epoch) = transition_epoch {
        cancel_event_in_tx(tx, &event_key(identity.award_id, transition_epoch)).await?;
    } else {
        let previous_transition_epoch: Option<i64> = sqlx::query_scalar(
            "SELECT transition_epoch FROM billing_founding_price_notice_state WHERE award_id=$1",
        )
        .bind(identity.award_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(Error::Db)?
        .flatten();
        if let Some(previous_transition_epoch) = previous_transition_epoch {
            cancel_event_in_tx(tx, &event_key(identity.award_id, previous_transition_epoch))
                .await?;
        }
    }
    save_outcome(tx, identity, transition_epoch, outcome, Some(reason)).await?;
    if transition_epoch.is_none() {
        // A missing boundary cannot produce a notice. Disable this corrupted candidate after
        // recording it so it does not occupy a batch slot on every poll.
        sqlx::query(
            "UPDATE billing_founding_schedule_jobs SET price_notice_eligible=FALSE, updated_at=now() \
             WHERE award_id=$1 AND version=$2",
        )
        .bind(identity.award_id)
        .bind(identity.version)
        .execute(&mut **tx)
        .await
        .map_err(Error::Db)?;
    }
    Ok(())
}

async fn save_outcome(
    tx: &mut Transaction<'_, Postgres>,
    identity: NoticeIdentity<'_>,
    transition_epoch: Option<i64>,
    outcome: Outcome,
    reason: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO billing_founding_price_notice_state \
         (award_id, transition_epoch, schedule_version, payer_kind, payer_id, outcome, reason_code, next_reconcile_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,CASE WHEN $6='queued' THEN now()+interval '30 seconds' \
           WHEN $7 IN ('recipient_bound_exceeded','recipient_unavailable') THEN now()+interval '1 hour' \
           ELSE now()+interval '1 day' END) \
         ON CONFLICT (award_id) DO UPDATE SET transition_epoch=EXCLUDED.transition_epoch, \
           schedule_version=EXCLUDED.schedule_version, payer_kind=EXCLUDED.payer_kind, \
           payer_id=EXCLUDED.payer_id, outcome=EXCLUDED.outcome, reason_code=EXCLUDED.reason_code, \
           next_reconcile_at=EXCLUDED.next_reconcile_at, updated_at=now()",
    )
    .bind(identity.award_id).bind(transition_epoch).bind(identity.version)
    .bind(identity.payer_kind).bind(identity.payer_id)
    .bind(outcome.as_str()).bind(reason)
    .execute(&mut **tx).await.map_err(Error::Db)?;
    Ok(())
}

async fn cancel_event_in_tx(tx: &mut Transaction<'_, Postgres>, event_key: &str) -> Result<()> {
    sqlx::query(
        "UPDATE cloud_notice_outbox SET status='cancelled', cancelled_at=now(), updated_at=now() \
         WHERE event_key=$1 AND policy_key=$2 AND kind='price_change' AND status IN ('pending','leased')",
    )
    .bind(event_key).bind(POLICY_KEY).execute(&mut **tx).await.map_err(Error::Db)?;
    Ok(())
}

pub async fn retire_award_notice(
    tx: &mut Transaction<'_, Postgres>,
    award_id: &str,
    reason: &'static str,
) -> std::result::Result<(), sqlx::Error> {
    sqlx::query(
        "WITH retired AS ( \
             UPDATE billing_founding_price_notice_state SET outcome='retired', reason_code=$2, updated_at=now() \
             WHERE award_id=$1 AND outcome <> 'retired' \
             RETURNING award_id, transition_epoch \
         ) \
         UPDATE cloud_notice_outbox AS outbox SET status='cancelled', cancelled_at=now(), updated_at=now() \
         FROM retired WHERE outbox.event_key = \
           'founding-price-change:' || retired.award_id || ':' || retired.transition_epoch::text \
           AND outbox.kind='price_change' AND outbox.policy_key=$3 \
           AND outbox.status IN ('pending','leased')",
    )
    .bind(award_id)
    .bind(reason)
    .bind(POLICY_KEY)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub async fn mark_award_schedule_unresolved(
    tx: &mut Transaction<'_, Postgres>,
    award_id: &str,
) -> std::result::Result<(), sqlx::Error> {
    sqlx::query(
        "WITH unresolved AS ( \
             UPDATE billing_founding_price_notice_state SET outcome='schedule_unresolved', \
               reason_code='schedule_not_verified', updated_at=now() \
             WHERE award_id=$1 AND outcome='queued' \
             RETURNING award_id, transition_epoch \
         ) \
         UPDATE cloud_notice_outbox AS outbox SET status='cancelled', cancelled_at=now(), updated_at=now() \
         FROM unresolved WHERE outbox.event_key = \
           'founding-price-change:' || unresolved.award_id || ':' || unresolved.transition_epoch::text \
           AND outbox.kind='price_change' AND outbox.policy_key=$2 \
           AND outbox.status IN ('pending','leased')",
    )
    .bind(award_id)
    .bind(POLICY_KEY)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn event_key(award_id: &str, transition_epoch: i64) -> String {
    format!("founding-price-change:{award_id}:{transition_epoch}")
}

fn epoch_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or(1)
}

#[derive(Debug, Serialize)]
struct ReviewSummary {
    queued: i64,
    late_unresolved: i64,
    schedule_unresolved: i64,
    retired: i64,
    late_reasons: Vec<ReviewReasonCount>,
}

#[derive(Debug, Serialize)]
struct ReviewReasonCount {
    reason_code: String,
    count: i64,
}

pub fn operator_router() -> Router<AppState> {
    Router::new().route("/ops/billing/founding-price-notices", get(review_summary))
}

async fn review_summary(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<ReviewSummary>> {
    let expected = state
        .billing_transfer_operator_token
        .as_deref()
        .ok_or(Error::Unauthorized)?;
    let provided = bearer_token(&headers).ok_or(Error::Unauthorized)?;
    if !token_matches(expected, provided) {
        return Err(Error::Unauthorized);
    }
    let rows = sqlx::query(
        "SELECT outcome, reason_code, count(*)::bigint AS count \
        FROM billing_founding_price_notice_state GROUP BY outcome, reason_code",
    )
    .fetch_all(&state.pool)
    .await
    .map_err(Error::Db)?;
    let mut summary = ReviewSummary {
        queued: 0,
        late_unresolved: 0,
        schedule_unresolved: 0,
        retired: 0,
        late_reasons: Vec::new(),
    };
    for row in rows {
        let outcome: String = row.try_get("outcome").map_err(Error::Db)?;
        let count: i64 = row.try_get("count").map_err(Error::Db)?;
        let reason: Option<String> = row.try_get("reason_code").map_err(Error::Db)?;
        match outcome.as_str() {
            "queued" => summary.queued = count,
            "late_unresolved" => summary.late_unresolved += count,
            "schedule_unresolved" => summary.schedule_unresolved += count,
            "retired" => summary.retired = count,
            _ => {
                return Err(Error::Internal(
                    "founding notice review state is corrupt".into(),
                ))
            }
        }
        if outcome == "late_unresolved" {
            if let Some(reason_code) = reason {
                summary
                    .late_reasons
                    .push(ReviewReasonCount { reason_code, count });
            }
        }
    }
    summary
        .late_reasons
        .sort_by(|left, right| left.reason_code.cmp(&right.reason_code));
    Ok(Json(summary))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::billing_catalogue::BillingOffer;
    use crate::notifications;
    use crate::notifications::STANDARD_MONTHLY_PRICE_NOTICE_LEAD_SECONDS;

    #[test]
    fn price_notice_due_at_is_fixed_to_the_original_transition() {
        let boundary = 1_900_000_000;
        assert_eq!(
            notifications::price_change_due_at(boundary).unwrap(),
            boundary - STANDARD_MONTHLY_PRICE_NOTICE_LEAD_SECONDS,
        );
        assert!(notifications::price_change_due_at(1).is_err());
    }

    #[test]
    fn founding_offer_selects_matching_standard_interval_and_amount() {
        let monthly = FoundingOffer::Monthly.standard_billing_offer();
        let annual = FoundingOffer::Annual.standard_billing_offer();
        assert_eq!(monthly, BillingOffer::StandardMonthly);
        assert_eq!(annual, BillingOffer::StandardAnnual);
        assert_eq!(FoundingOffer::Monthly.interval().as_str(), "month");
        assert_eq!(FoundingOffer::Annual.interval().as_str(), "year");
        assert_eq!(BillingOffer::StandardMonthly.expected_amount_pence(), 299);
        assert_eq!(BillingOffer::StandardAnnual.expected_amount_pence(), 2_999);
    }
}
