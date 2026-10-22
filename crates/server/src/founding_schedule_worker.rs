//! Opt-in worker that reconciles Stripe schedules for paid founding awards.

use std::{sync::Arc, time::Duration};

use sqlx::PgPool;
use sqlx::Row;

use crate::{
    billing::{
        founding_price_boundary, sponsored_provider_phases, ProviderError,
        SponsoredSubscriptionItemRequest, SponsoredSubscriptionPhase, SubscriptionProvider,
    },
    billing_catalogue::BillingPriceIds,
    founding_allocator::{FoundingOffer, FoundingPayerKind},
    founding_schedule::{self, FoundingScheduleJob},
};

const POLL_INTERVAL: Duration = Duration::from_secs(5);

pub fn spawn(pool: PgPool, provider: Arc<dyn SubscriptionProvider>, catalogue: BillingPriceIds) {
    tokio::spawn(run(pool, provider, catalogue));
}

pub async fn run_once(
    pool: &PgPool,
    provider: &dyn SubscriptionProvider,
    catalogue: &BillingPriceIds,
) -> Result<bool, sqlx::Error> {
    let Some(job) = founding_schedule::claim_due(pool).await? else {
        return Ok(false);
    };
    let result = process_job(pool, provider, catalogue, &job).await;
    match result {
        Ok(Some(schedule_id)) => {
            let marked = founding_schedule::mark_scheduled(pool, &job, &schedule_id).await?;
            if !marked && job.payer_kind == FoundingPayerKind::Personal {
                let cancellation_requested = personal_cancellation_requested(pool, &job).await?;
                if cancellation_requested {
                    match apply_personal_cancellation(pool, provider, &job).await {
                        Ok(()) => {
                            founding_schedule::mark_not_required(pool, &job).await?;
                        }
                        Err(error) => {
                            let _ = founding_schedule::mark_unresolved(pool, &job, &error).await?;
                            eprintln!(
                                "founding schedule worker: cancellation remains unresolved attempt={}",
                                job.attempt_count,
                            );
                        }
                    }
                }
            }
        }
        Ok(None) => {
            founding_schedule::mark_not_required(pool, &job).await?;
        }
        Err(error) => {
            let _ = founding_schedule::mark_unresolved(pool, &job, &error).await?;
            eprintln!(
                "founding schedule worker: schedule remains unresolved attempt={}",
                job.attempt_count,
            );
        }
    }
    Ok(true)
}

async fn process_job(
    pool: &PgPool,
    provider: &dyn SubscriptionProvider,
    catalogue: &BillingPriceIds,
    job: &FoundingScheduleJob,
) -> Result<Option<String>, ProviderError> {
    match job.payer_kind {
        FoundingPayerKind::Personal => {
            let row = sqlx::query(
                "SELECT account.stripe_subscription_id, account.state, account.cancel_at_period_end, \
                        account.cancellation_requested_at IS NOT NULL AS cancellation_requested, \
                        award.offer, award.original_end_epoch \
                 FROM billing_personal_accounts AS account \
                 JOIN billing_founding_awards AS award ON award.beneficiary_id = account.user_id \
                 WHERE account.user_id = $1 AND award.award_id = $2 \
                   AND award.payer_kind = 'personal' AND award.payer_id = $1",
            )
            .bind(&job.beneficiary_id)
            .bind(&job.award_id)
            .fetch_optional(pool)
            .await
            .map_err(|_| local_error("local_evidence_invalid"))?
            .ok_or_else(|| local_error("local_binding_missing"))?;
            let subscription_id: Option<String> = row
                .try_get("stripe_subscription_id")
                .map_err(|_| local_error("local_evidence_invalid"))?;
            let state: String = row
                .try_get("state")
                .map_err(|_| local_error("local_evidence_invalid"))?;
            let cancel_at_period_end: bool = row
                .try_get("cancel_at_period_end")
                .map_err(|_| local_error("local_evidence_invalid"))?;
            let cancellation_requested: bool = row
                .try_get("cancellation_requested")
                .map_err(|_| local_error("local_evidence_invalid"))?;
            if state == "canceled" {
                return Ok(None);
            }
            if subscription_id.as_deref() != Some(job.subscription_id.as_str()) {
                return Err(local_error("local_binding_missing"));
            }
            if cancel_at_period_end || cancellation_requested {
                provider
                    .cancel_personal_subscription(
                        &job.subscription_id,
                        &personal_cancellation_idempotency_key(
                            &job.subscription_id,
                            &job.lease_token,
                        ),
                        &job.beneficiary_id,
                    )
                    .await?;
                founding_schedule::record_personal_cancellation(
                    pool,
                    &job.beneficiary_id,
                    &job.subscription_id,
                )
                .await
                .map_err(|_| local_error("local_evidence_invalid"))?;
                return Ok(None);
            }
            if !matches!(state.as_str(), "active" | "past_due") {
                return Err(local_error("local_evidence_invalid"));
            }
            let offer_text: String = row
                .try_get("offer")
                .map_err(|_| local_error("local_evidence_invalid"))?;
            let award_offer = parse_founding_offer(&offer_text)?;
            let end_epoch: Option<i64> = row
                .try_get("original_end_epoch")
                .map_err(|_| local_error("local_evidence_invalid"))?;
            let end_epoch = founding_price_boundary(
                end_epoch.ok_or_else(|| local_error("local_evidence_invalid"))?,
            )
            .map_err(|_| local_error("local_evidence_invalid"))?;
            let current = provider
                .get_sponsored_subscription(&job.subscription_id)
                .await?;
            if current.subscription_id != job.subscription_id {
                return Err(local_error("local_binding_missing"));
            }
            if current.cancel_at_period_end || current.cancel_at.is_some() {
                founding_schedule::record_personal_cancellation(
                    pool,
                    &job.beneficiary_id,
                    &job.subscription_id,
                )
                .await
                .map_err(|_| local_error("local_evidence_invalid"))?;
                return Ok(None);
            }
            if current.items.len() != 1 || current.items[0].quantity != 1 {
                return Err(local_error("local_evidence_invalid"));
            }
            let current_end = current
                .current_period_end
                .ok_or_else(|| local_error("local_evidence_invalid"))?;
            let current_start = current
                .current_period_start
                .filter(|start| *start < current_end)
                .ok_or_else(|| local_error("local_evidence_invalid"))?;
            if !personal_current_price_matches(
                &current,
                award_offer,
                current_start,
                end_epoch,
                catalogue,
            ) {
                return Err(local_error("local_evidence_invalid"));
            }
            let phases = personal_transition_phases(award_offer, current_end, end_epoch, catalogue);
            let updated = provider
                .update_personal_subscription(&job.subscription_id, &phases, &job.idempotency_key)
                .await?;
            let cancellation_requested: bool = sqlx::query_scalar(
                "SELECT cancellation_requested_at IS NOT NULL OR cancel_at_period_end \
                 FROM billing_personal_accounts WHERE user_id = $1 \
                   AND stripe_subscription_id = $2",
            )
            .bind(&job.beneficiary_id)
            .bind(&job.subscription_id)
            .fetch_optional(pool)
            .await
            .map_err(|_| local_error("local_evidence_invalid"))?
            .ok_or_else(|| local_error("local_binding_missing"))?;
            if cancellation_requested {
                provider
                    .cancel_personal_subscription(
                        &job.subscription_id,
                        &personal_cancellation_idempotency_key(
                            &job.subscription_id,
                            &job.lease_token,
                        ),
                        &job.beneficiary_id,
                    )
                    .await?;
                founding_schedule::record_personal_cancellation(
                    pool,
                    &job.beneficiary_id,
                    &job.subscription_id,
                )
                .await
                .map_err(|_| local_error("local_evidence_invalid"))?;
                return Ok(None);
            }
            updated
                .schedule_id
                .ok_or_else(|| local_error("local_evidence_invalid"))
                .map(Some)
        }
        FoundingPayerKind::Sponsor => {
            let subscription: Option<String> = sqlx::query_scalar(
                "SELECT subscription.provider_subscription_id \
                 FROM billing_sponsored_subscriptions AS subscription \
                 JOIN billing_founding_awards AS award ON award.beneficiary_id = $3 \
                 WHERE subscription.organization_id = $1 AND subscription.provider_subscription_id = $2 \
                   AND subscription.status IN ('pending', 'active', 'past_due') \
                   AND award.award_id = $4 AND award.payer_kind = 'sponsor' AND award.payer_id = $1",
            )
            .bind(&job.payer_id)
            .bind(&job.subscription_id)
            .bind(&job.beneficiary_id)
            .bind(&job.award_id)
            .fetch_optional(pool)
            .await
            .map_err(|_| local_error("local_evidence_invalid"))?;
            if subscription.as_deref() != Some(job.subscription_id.as_str()) {
                return Err(local_error("local_binding_missing"));
            }
            let current = provider
                .get_sponsored_subscription(&job.subscription_id)
                .await?;
            if current.subscription_id != job.subscription_id {
                return Err(local_error("local_binding_missing"));
            }
            let current_period_end = current
                .current_period_end
                .ok_or_else(|| local_error("local_evidence_invalid"))?;
            // A retry may run after the wall clock has crossed the old billing boundary. Stripe
            // still needs the first proposed phase to cover its current provider boundary.
            let phases =
                sponsored_provider_phases(pool, &job.payer_id, catalogue, current_period_end)
                    .await
                    .map_err(|_| local_error("local_evidence_invalid"))?;
            let updated = crate::billing::with_sponsored_subscription_write_lock(
                pool,
                &job.subscription_id,
                async {
                    Ok::<_, crate::error::Error>(
                        provider
                            .update_sponsored_subscription(
                                &job.payer_id,
                                &job.subscription_id,
                                &phases,
                                &job.idempotency_key,
                            )
                            .await,
                    )
                },
            )
            .await
            .map_err(|_| local_error("sponsored_schedule_write_unavailable"))??;
            updated
                .schedule_id
                .ok_or_else(|| local_error("local_evidence_invalid"))
                .map(Some)
        }
    }
}

async fn personal_cancellation_requested(
    pool: &PgPool,
    job: &FoundingScheduleJob,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT cancel_at_period_end OR cancellation_requested_at IS NOT NULL \
         FROM billing_personal_accounts WHERE user_id = $1 AND stripe_subscription_id = $2",
    )
    .bind(&job.beneficiary_id)
    .bind(&job.subscription_id)
    .fetch_optional(pool)
    .await
    .map(|value| value.unwrap_or(false))
}

async fn apply_personal_cancellation(
    pool: &PgPool,
    provider: &dyn SubscriptionProvider,
    job: &FoundingScheduleJob,
) -> Result<(), ProviderError> {
    provider
        .cancel_personal_subscription(
            &job.subscription_id,
            &personal_cancellation_idempotency_key(&job.subscription_id, &job.lease_token),
            &job.beneficiary_id,
        )
        .await?;
    founding_schedule::record_personal_cancellation(
        pool,
        &job.beneficiary_id,
        &job.subscription_id,
    )
    .await
    .map_err(|_| local_error("local_evidence_invalid"))?;
    Ok(())
}

fn personal_cancellation_idempotency_key(subscription_id: &str, lease_token: &str) -> String {
    format!("sotto-personal-cancel:{subscription_id}:worker:{lease_token}")
}

fn parse_founding_offer(value: &str) -> Result<FoundingOffer, ProviderError> {
    match value {
        "founding_monthly" => Ok(FoundingOffer::Monthly),
        "founding_annual" => Ok(FoundingOffer::Annual),
        _ => Err(local_error("local_evidence_invalid")),
    }
}

fn personal_transition_phases(
    offer: FoundingOffer,
    current_period_end: i64,
    founding_end: i64,
    catalogue: &BillingPriceIds,
) -> Vec<SponsoredSubscriptionPhase> {
    let current_offer = if founding_end <= current_period_end {
        offer.standard_billing_offer()
    } else {
        offer.billing_offer()
    };
    let mut phases = vec![SponsoredSubscriptionPhase {
        start_at_epoch: current_period_end,
        end_at_epoch: (founding_end > current_period_end).then_some(founding_end),
        items: vec![SponsoredSubscriptionItemRequest {
            provider_price_id: catalogue.id_for(current_offer).to_string(),
            quantity: 1,
        }],
    }];
    if founding_end > current_period_end {
        phases.push(SponsoredSubscriptionPhase {
            start_at_epoch: founding_end,
            end_at_epoch: None,
            items: vec![SponsoredSubscriptionItemRequest {
                provider_price_id: catalogue.id_for(offer.standard_billing_offer()).to_string(),
                quantity: 1,
            }],
        });
    }
    phases
}

fn personal_current_price_matches(
    current: &crate::billing::SponsoredSubscriptionSnapshot,
    founding_offer: FoundingOffer,
    current_period_start: i64,
    founding_end: i64,
    catalogue: &BillingPriceIds,
) -> bool {
    // The current item covers through its period end, so the founding price remains valid when
    // that end equals the boundary. The standard price is current only once a period starts at it.
    let expected_offer = if founding_end <= current_period_start {
        founding_offer.standard_billing_offer()
    } else {
        founding_offer.billing_offer()
    };
    current.items.len() == 1
        && current.items[0].provider_price_id == catalogue.id_for(expected_offer)
}

fn local_error(code: &'static str) -> ProviderError {
    ProviderError {
        status: None,
        code: Some(code.into()),
        kind: crate::billing::ProviderErrorKind::Unknown,
    }
}

async fn run(pool: PgPool, provider: Arc<dyn SubscriptionProvider>, catalogue: BillingPriceIds) {
    loop {
        match run_once(&pool, provider.as_ref(), &catalogue).await {
            Ok(true) => tokio::time::sleep(POLL_INTERVAL).await,
            Ok(false) => tokio::time::sleep(POLL_INTERVAL).await,
            Err(_) => {
                eprintln!(
                    "founding schedule worker: queue operation failed; lease retained for retry"
                );
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalogue() -> BillingPriceIds {
        BillingPriceIds {
            standard_monthly: "price_standard_monthly".into(),
            standard_annual: "price_standard_annual".into(),
            founding_monthly: "price_founding_monthly".into(),
            founding_annual: "price_founding_annual".into(),
        }
    }

    #[test]
    fn monthly_founder_keeps_twelve_periods_then_uses_standard_price() {
        let phases = personal_transition_phases(FoundingOffer::Monthly, 100, 1_000, &catalogue());
        assert_eq!(phases.len(), 2);
        assert_eq!(phases[0].start_at_epoch, 100);
        assert_eq!(phases[0].end_at_epoch, Some(1_000));
        assert_eq!(
            phases[0].items[0].provider_price_id,
            "price_founding_monthly"
        );
        assert_eq!(phases[1].start_at_epoch, 1_000);
        assert_eq!(
            phases[1].items[0].provider_price_id,
            "price_standard_monthly"
        );
    }

    #[test]
    fn annual_founder_already_at_boundary_starts_the_standard_phase() {
        let phases = personal_transition_phases(FoundingOffer::Annual, 1_000, 900, &catalogue());
        assert_eq!(phases.len(), 1);
        assert_eq!(phases[0].start_at_epoch, 1_000);
        assert_eq!(phases[0].end_at_epoch, None);
        assert_eq!(
            phases[0].items[0].provider_price_id,
            "price_standard_annual"
        );
    }

    #[test]
    fn personal_worker_requires_the_provider_price_for_the_current_founding_phase() {
        let catalogue = catalogue();
        let snapshot =
            |price: &str, start: i64, end: i64| crate::billing::SponsoredSubscriptionSnapshot {
                customer_id: Some("cus".into()),
                default_payment_method_id: None,
                subscription_id: "sub".into(),
                status: crate::billing::SubscriptionStatus::Active,
                cancel_at_period_end: false,
                cancel_at: None,
                current_period_start: Some(start),
                current_period_end: Some(end),
                schedule_id: None,
                items: vec![crate::billing::SponsoredSubscriptionItem {
                    provider_item_id: "si".into(),
                    provider_price_id: price.into(),
                    quantity: 1,
                }],
            };

        assert!(personal_current_price_matches(
            &snapshot("price_founding_monthly", 0, 99),
            FoundingOffer::Monthly,
            0,
            200,
            &catalogue,
        ));
        assert!(personal_current_price_matches(
            &snapshot("price_founding_monthly", 0, 100),
            FoundingOffer::Monthly,
            0,
            100,
            &catalogue,
        ));
        assert!(personal_current_price_matches(
            &snapshot("price_standard_monthly", 100, 200),
            FoundingOffer::Monthly,
            100,
            100,
            &catalogue,
        ));
        assert!(personal_current_price_matches(
            &snapshot("price_founding_monthly", 99, 100),
            FoundingOffer::Monthly,
            99,
            100,
            &catalogue,
        ));
        assert!(!personal_current_price_matches(
            &snapshot("price_standard_monthly", 99, 100),
            FoundingOffer::Monthly,
            99,
            100,
            &catalogue,
        ));

        let annual_at_its_first_boundary = snapshot("price_founding_annual", 1_000, 2_000);
        assert!(personal_current_price_matches(
            &annual_at_its_first_boundary,
            FoundingOffer::Annual,
            1_000,
            2_000,
            &catalogue,
        ));
    }
}
