//! Default-off worker that discovers recent missing Stripe invoice webhook events.

use std::time::Duration;

use crate::billing::BillingState;
use crate::cloud_provider_stripe::StripeCoverageConfig;
use crate::cloud_provider_stripe_event_repair::{self, StripeEventRepairRunOutcome};
use crate::cloud_provider_stripe_http::StripeReadClient;
use sqlx::PgPool;

pub fn spawn(
    pool: PgPool,
    stripe: StripeReadClient,
    coverage: StripeCoverageConfig,
    billing: BillingState,
) {
    tokio::spawn(async move {
        loop {
            let pool_for_apply = pool.clone();
            let billing_for_apply = billing.clone();
            match cloud_provider_stripe_event_repair::run_once(
                &pool,
                &stripe,
                &coverage.account_id,
                coverage.environment,
                epoch_now(),
                move |event, now| {
                    let pool = pool_for_apply.clone();
                    let billing = billing_for_apply.clone();
                    async move {
                        crate::billing::repair_authenticated_stripe_event(
                            &pool, &billing, &event, now,
                        )
                        .await
                        .map_err(|_| {
                            cloud_provider_stripe_event_repair::StripeEventRepairRunError::Processing(
                                "event_application_retryable",
                            )
                        })
                    }
                },
            )
            .await
            {
                Ok(StripeEventRepairRunOutcome::Idle) => {
                    tokio::time::sleep(Duration::from_secs(10)).await;
                }
                Ok(StripeEventRepairRunOutcome::PageCompleted {
                    events,
                    applied,
                    already_applied,
                    unrelated,
                    rejected,
                    unresolved,
                    window_completed,
                }) => {
                    let status = cloud_provider_stripe_event_repair::status_snapshot(
                        &pool,
                        &coverage.account_id,
                        coverage.environment,
                    )
                    .await
                    .ok()
                    .flatten();
                    eprintln!(
                        "stripe event repair: page completed events={events} applied={applied} \
                         already_applied={already_applied} unrelated={unrelated} rejected={rejected} \
                         unresolved={unresolved} window_completed={window_completed} \
                         scan_pending={} completed_through={} last_outcome={} last_error_code={} \
                         history_gap={} pending_scopes={} \
                         unresolved_total={} rejected_total={} unsupported_total={} \
                         oldest_unresolved_present={} oldest_unresolved_age_seconds={}",
                        status
                            .as_ref()
                            .is_some_and(|status| status.scan_pending),
                        status
                            .as_ref()
                            .and_then(|status| status.completed_through)
                            .unwrap_or(0),
                        status
                            .as_ref()
                            .and_then(|status| status.last_outcome.as_deref())
                            .unwrap_or("unknown"),
                        status
                            .as_ref()
                            .and_then(|status| status.last_error_code.as_deref())
                            .unwrap_or("none"),
                        status.as_ref().is_some_and(|status| status.history_gap.is_some()),
                        status.as_ref().map_or(0, |status| status.pending_scope_count),
                        status.as_ref().map_or(0, |status| status.unresolved_count),
                        status.as_ref().map_or(0, |status| status.rejected_count),
                        status.as_ref().map_or(0, |status| status.unsupported_count),
                        status
                            .as_ref()
                            .and_then(|status| status.oldest_unresolved_created)
                            .is_some(),
                        status
                            .as_ref()
                            .and_then(|status| status.oldest_unresolved_created)
                            .map_or(0, |created| epoch_now().saturating_sub(created).max(0))
                    );
                    if events == 0 || window_completed {
                        tokio::time::sleep(Duration::from_secs(10)).await;
                    }
                }
                Err(_) => {
                    eprintln!(
                        "warning: Stripe event repair page failed; cursor retained for retry"
                    );
                    tokio::time::sleep(Duration::from_secs(10)).await;
                }
            }
        }
    });
}

fn epoch_now() -> i64 {
    crate::clock::business_epoch_seconds()
}
