//! Default-off executor for consented payer transfers.
//!
//! The worker claims one durable transfer lease at a time and performs each provider request
//! without holding a database transaction. Database records fence every provider step and make
//! retries converge on the same subscription or schedule identity.

use std::{sync::Arc, time::Duration};

use sqlx::{PgPool, Postgres, Row, Transaction};

use crate::{
    billing::{
        sponsored_provider_phases, ProviderError, ProviderErrorKind, SubscriptionProvider,
        SubscriptionStatus,
    },
    billing_catalogue::{BillingOffer, BillingPriceIds},
    billing_transfers::{
        self, TransferExecutionFailure, TransferIntent, TransferLease, TransferPayer, TransferState,
    },
    founding_allocator,
};

const POLL_INTERVAL: Duration = Duration::from_secs(5);
const INVOICE_WAIT_SECONDS: i64 = 300;
const STRIPE_IDEMPOTENCY_SAFE_RETRY_SECONDS: i64 = 23 * 60 * 60;

pub fn spawn(pool: PgPool, provider: Arc<dyn SubscriptionProvider>, catalogue: BillingPriceIds) {
    tokio::spawn(run(pool, provider, catalogue));
}

pub async fn run_once(
    pool: &PgPool,
    provider: &dyn SubscriptionProvider,
    catalogue: &BillingPriceIds,
) -> Result<bool, sqlx::Error> {
    let Some(lease) = billing_transfers::claim_due(pool)
        .await
        .map_err(|error| sqlx::Error::Protocol(error.to_string()))?
    else {
        return Ok(false);
    };
    match advance(pool, provider, catalogue, &lease).await {
        Ok(Advance::WaitingForInvoice) => {
            billing_transfers::defer_execution_lease(
                pool,
                &lease.transfer.transfer_id,
                &lease.lease_token,
                INVOICE_WAIT_SECONDS,
            )
            .await
            .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
        }
        Ok(Advance::ParkedForReview) => {}
        Ok(Advance::Advanced) => {
            billing_transfers::finish_execution_lease(
                pool,
                &lease.transfer.transfer_id,
                &lease.lease_token,
            )
            .await
            .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
        }
        Err(ExecutionError::Provider(error)) => {
            let failure = classify_provider_failure(&error);
            let _ = billing_transfers::release_execution_lease(
                pool,
                &lease.transfer.transfer_id,
                &lease.lease_token,
                failure,
            )
            .await;
            eprintln!(
                "payer transfer execution deferred transfer={} code={}",
                opaque_transfer_id(&lease.transfer.transfer_id),
                error.code.as_deref().unwrap_or("provider_error")
            );
        }
        Err(ExecutionError::Local(code)) => {
            let _ = billing_transfers::release_execution_lease(
                pool,
                &lease.transfer.transfer_id,
                &lease.lease_token,
                TransferExecutionFailure::LocalEvidenceChanged,
            )
            .await;
            eprintln!(
                "payer transfer execution deferred transfer={} code={code}",
                opaque_transfer_id(&lease.transfer.transfer_id)
            );
        }
    }
    Ok(true)
}

async fn run(pool: PgPool, provider: Arc<dyn SubscriptionProvider>, catalogue: BillingPriceIds) {
    loop {
        match run_once(&pool, provider.as_ref(), &catalogue).await {
            Ok(true) => {}
            Ok(false) => tokio::time::sleep(POLL_INTERVAL).await,
            Err(error) => {
                eprintln!("payer transfer worker: queue operation failed: {error}");
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        }
    }
}

enum Advance {
    Advanced,
    WaitingForInvoice,
    ParkedForReview,
}

#[derive(Debug)]
enum ExecutionError {
    Provider(ProviderError),
    Local(&'static str),
}

impl From<ProviderError> for ExecutionError {
    fn from(value: ProviderError) -> Self {
        Self::Provider(value)
    }
}

async fn advance(
    pool: &PgPool,
    provider: &dyn SubscriptionProvider,
    catalogue: &BillingPriceIds,
    lease: &TransferLease,
) -> Result<Advance, ExecutionError> {
    let transfer = &lease.transfer;
    if transfer.execution_manual_review_required {
        return Err(ExecutionError::Local("operator_review_required"));
    }
    if billing_transfers::destination_period_mismatch_code(transfer).is_some() {
        billing_transfers::repark_mismatched_destination_invoice(
            pool,
            &transfer.transfer_id,
            &lease.lease_token,
        )
        .await
        .map_err(|_| ExecutionError::Local("destination_period_mismatch_review_failed"))?;
        return Ok(Advance::ParkedForReview);
    }
    if transfer.state == TransferState::SourceRestorationPending {
        return restore_source(pool, provider, catalogue, lease).await;
    }
    if transfer.destination_schedule_id.is_none() {
        prepare_destination(pool, provider, catalogue, lease).await?;
        // Minimise the interval where the destination is scheduled but the source is not yet
        // fenced for the same renewal. The source is preflighted before destination writes and
        // re-read immediately before its own provider update.
        adjust_source(pool, provider, catalogue, lease).await?;
        return Ok(Advance::Advanced);
    }
    if transfer.source_adjustment_reference.is_none() {
        adjust_source(pool, provider, catalogue, lease).await?;
        return Ok(Advance::Advanced);
    }
    if transfer.destination_payment_reference.is_some() {
        let source_end_reference = verify_source_end(pool, provider, catalogue, transfer).await?;
        let mut tx = pool
            .begin()
            .await
            .map_err(|_| ExecutionError::Local("transfer_transaction_unavailable"))?;
        let current = lock_transfer_context(&mut tx, lease, true).await?;
        let adjustment_reference = current
            .source_adjustment_reference
            .as_deref()
            .ok_or(ExecutionError::Local("source_adjustment_evidence_missing"))?;
        if current.destination_payment_reference.is_none()
            || current.source_provider_subscription_id != transfer.source_provider_subscription_id
        {
            return Err(ExecutionError::Local("transfer_settlement_changed"));
        }
        retire_transfer_source(&mut tx, &current, &source_end_reference).await?;
        billing_transfers::complete_source_adjustment(
            &mut tx,
            &current.transfer_id,
            adjustment_reference,
        )
        .await
        .map_err(|_| ExecutionError::Local("transfer_completion_rejected"))?;
        tx.commit()
            .await
            .map_err(|_| ExecutionError::Local("transfer_commit_failed"))?;
        return Ok(Advance::Advanced);
    }
    Ok(Advance::WaitingForInvoice)
}

async fn prepare_destination(
    pool: &PgPool,
    provider: &dyn SubscriptionProvider,
    catalogue: &BillingPriceIds,
    lease: &TransferLease,
) -> Result<(), ExecutionError> {
    if epoch_now() >= lease.transfer.effective_from {
        return Err(ExecutionError::Local("transfer_boundary_already_reached"));
    }
    preflight_source_adjustment(provider, &lease.transfer).await?;
    let mut tx = pool
        .begin()
        .await
        .map_err(|_| ExecutionError::Local("transfer_transaction_unavailable"))?;
    let transfer = lock_transfer_context(&mut tx, lease, false).await?;
    tx.commit()
        .await
        .map_err(|_| ExecutionError::Local("transfer_commit_failed"))?;
    match transfer.destination_kind {
        TransferPayer::Personal => {
            prepare_personal_destination(pool, provider, catalogue, &transfer).await
        }
        TransferPayer::Sponsor => {
            prepare_sponsored_destination(pool, provider, catalogue, &transfer).await
        }
    }
}

async fn prepare_personal_destination(
    pool: &PgPool,
    provider: &dyn SubscriptionProvider,
    catalogue: &BillingPriceIds,
    transfer: &TransferIntent,
) -> Result<(), ExecutionError> {
    let customer_id = transfer
        .destination_customer_id
        .as_deref()
        .ok_or(ExecutionError::Local("destination_setup_missing"))?;
    let setup_intent_id = transfer
        .destination_setup_intent_id
        .as_deref()
        .ok_or(ExecutionError::Local("destination_setup_missing"))?;
    let setup = provider.get_transfer_setup_intent(setup_intent_id).await?;
    if setup.transfer_id != transfer.transfer_id
        || setup.customer_id != customer_id
        || setup.setup_intent_id != setup_intent_id
        || setup.status != "succeeded"
        || !setup.off_session
    {
        return Err(ExecutionError::Local("destination_setup_changed"));
    }
    let allocation_reference = transfer_allocation_reference(&transfer.transfer_id);
    let operation_id = transfer_personal_operation_id(&transfer.transfer_id);
    seed_personal_destination(
        pool,
        transfer,
        customer_id,
        &allocation_reference,
        &operation_id,
    )
    .await?;
    let schedule = provider
        .create_personal_transfer_schedule(
            &transfer.transfer_id,
            &transfer.beneficiary_id,
            customer_id,
            &setup.payment_method_id,
            transfer.offer,
            catalogue.id_for(transfer.offer),
            &allocation_reference,
            transfer.effective_from,
            &format!("{}:destination", transfer.provider_idempotency_key),
        )
        .await?;
    if schedule.customer_id != customer_id
        || schedule.start_at_epoch != transfer.effective_from
        || !matches!(schedule.status.as_str(), "not_started" | "active")
    {
        return Err(ExecutionError::Local("destination_schedule_mismatch"));
    }
    let mut tx = pool
        .begin()
        .await
        .map_err(|_| ExecutionError::Local("transfer_transaction_unavailable"))?;
    billing_transfers::record_destination_schedule(
        &mut tx,
        &transfer.transfer_id,
        &operation_id,
        customer_id,
        &schedule.schedule_id,
        schedule.subscription_id.as_deref(),
        Some(setup_intent_id),
        schedule.expected_period_end_epoch,
    )
    .await
    .map_err(|_| ExecutionError::Local("destination_schedule_record_failed"))?;
    tx.commit()
        .await
        .map_err(|_| ExecutionError::Local("transfer_commit_failed"))
}

async fn prepare_sponsored_destination(
    pool: &PgPool,
    provider: &dyn SubscriptionProvider,
    catalogue: &BillingPriceIds,
    transfer: &TransferIntent,
) -> Result<(), ExecutionError> {
    let organization_id = transfer
        .destination_organization_id
        .as_deref()
        .ok_or(ExecutionError::Local("destination_organisation_missing"))?;
    let snapshot = provider
        .get_sponsored_subscription(
            transfer
                .destination_provider_subscription_id
                .as_deref()
                .ok_or(ExecutionError::Local("destination_subscription_missing"))?,
        )
        .await?;
    validate_sponsor_boundary(
        &snapshot,
        transfer,
        organization_id,
        transfer
            .destination_provider_subscription_id
            .as_deref()
            .ok_or(ExecutionError::Local("destination_subscription_missing"))?,
    )?;
    seed_sponsored_destination(pool, transfer, organization_id, &snapshot, catalogue).await?;
    let phases = sponsored_provider_phases(pool, organization_id, catalogue, epoch_now())
        .await
        .map_err(|_| ExecutionError::Local("destination_schedule_evidence_invalid"))?;
    let updated = crate::billing::with_sponsored_subscription_write_lock(
        pool,
        &snapshot.subscription_id,
        async {
            Ok::<_, crate::error::Error>(
                provider
                    .update_sponsored_subscription(
                        organization_id,
                        &snapshot.subscription_id,
                        &phases,
                        &format!("{}:destination", transfer.provider_idempotency_key),
                    )
                    .await,
            )
        },
    )
    .await
    .map_err(|_| ExecutionError::Local("sponsored_schedule_write_unavailable"))??;
    validate_sponsor_boundary(
        &updated,
        transfer,
        organization_id,
        transfer
            .destination_provider_subscription_id
            .as_deref()
            .ok_or(ExecutionError::Local("destination_subscription_missing"))?,
    )?;
    let expected_period_end = provider
        .preview_transfer_period_end(
            &updated.subscription_id,
            catalogue.id_for(transfer.offer),
            transfer.effective_from,
        )
        .await?;
    if expected_period_end <= transfer.effective_from {
        return Err(ExecutionError::Local("destination_period_preview_invalid"));
    }
    let schedule_id = updated
        .schedule_id
        .as_deref()
        .ok_or(ExecutionError::Local("destination_schedule_missing"))?;
    let mut tx = pool
        .begin()
        .await
        .map_err(|_| ExecutionError::Local("transfer_transaction_unavailable"))?;
    billing_transfers::record_destination_schedule(
        &mut tx,
        &transfer.transfer_id,
        &transfer_sponsored_operation_id(&transfer.transfer_id),
        updated
            .customer_id
            .as_deref()
            .ok_or(ExecutionError::Local("destination_customer_missing"))?,
        schedule_id,
        Some(&updated.subscription_id),
        None,
        expected_period_end,
    )
    .await
    .map_err(|_| ExecutionError::Local("destination_schedule_record_failed"))?;
    tx.commit()
        .await
        .map_err(|_| ExecutionError::Local("transfer_commit_failed"))
}

async fn adjust_source(
    pool: &PgPool,
    provider: &dyn SubscriptionProvider,
    catalogue: &BillingPriceIds,
    lease: &TransferLease,
) -> Result<(), ExecutionError> {
    let transfer = &lease.transfer;
    if transfer.source_kind == TransferPayer::Personal {
        let source_subscription = transfer
            .source_provider_subscription_id
            .as_deref()
            .ok_or(ExecutionError::Local("source_subscription_missing"))?;
        let source = provider
            .get_sponsored_subscription(source_subscription)
            .await?;
        if !personal_source_adjustment_preflight_matches(
            &source,
            source_subscription,
            transfer.effective_from,
        ) {
            return Err(ExecutionError::Local("source_period_changed"));
        }
        if !personal_source_adjustment_retry_allowed(
            epoch_now(),
            transfer.effective_from,
            personal_source_adjustment_is_applied(&source, transfer.effective_from),
        ) {
            return Err(ExecutionError::Local("source_boundary_already_reached"));
        }
    } else if !sponsor_source_adjustment_is_before_boundary(epoch_now(), transfer.effective_from) {
        return Err(ExecutionError::Local("source_boundary_already_reached"));
    }
    let mut tx = pool
        .begin()
        .await
        .map_err(|_| ExecutionError::Local("transfer_transaction_unavailable"))?;
    let current = lock_transfer_context(&mut tx, lease, false).await?;
    billing_transfers::begin_source_adjustment(
        &mut tx,
        &current.transfer_id,
        &format!("{}:source", current.provider_idempotency_key),
    )
    .await
    .map_err(|_| ExecutionError::Local("source_adjustment_transition_rejected"))?;
    tx.commit()
        .await
        .map_err(|_| ExecutionError::Local("transfer_commit_failed"))?;

    let source_subscription = transfer
        .source_provider_subscription_id
        .as_deref()
        .ok_or(ExecutionError::Local("source_subscription_missing"))?;
    let (reference, schedule_id) = match transfer.source_kind {
        TransferPayer::Personal => {
            let snapshot = provider
                .cancel_personal_subscription_at_boundary(
                    source_subscription,
                    transfer.effective_from,
                    &format!("{}:source", transfer.provider_idempotency_key),
                    &transfer.beneficiary_id,
                )
                .await?;
            if snapshot.subscription_id != source_subscription
                || snapshot.current_period_end != Some(transfer.effective_from)
                || (!snapshot.cancel_at_period_end
                    && snapshot.cancel_at != Some(transfer.effective_from)
                    && snapshot.status != SubscriptionStatus::Canceled)
            {
                return Err(ExecutionError::Local("source_subscription_changed"));
            }
            let reference = snapshot
                .schedule_id
                .clone()
                .unwrap_or_else(|| source_subscription.to_string());
            (reference, snapshot.schedule_id)
        }
        TransferPayer::Sponsor => {
            let organization_id = transfer
                .source_organization_id
                .as_deref()
                .ok_or(ExecutionError::Local("source_organisation_missing"))?;
            let current = provider
                .get_sponsored_subscription(source_subscription)
                .await?;
            validate_sponsor_boundary(&current, transfer, organization_id, source_subscription)?;
            prepare_sponsored_source_removal(pool, transfer, organization_id, catalogue).await?;
            let phases = sponsored_provider_phases(pool, organization_id, catalogue, epoch_now())
                .await
                .map_err(|_| ExecutionError::Local("source_schedule_evidence_invalid"))?;
            let updated = crate::billing::with_sponsored_subscription_write_lock(
                pool,
                source_subscription,
                async {
                    Ok::<_, crate::error::Error>(
                        provider
                            .update_sponsored_subscription(
                                organization_id,
                                source_subscription,
                                &phases,
                                &format!("{}:source", transfer.provider_idempotency_key),
                            )
                            .await,
                    )
                },
            )
            .await
            .map_err(|_| ExecutionError::Local("sponsored_schedule_write_unavailable"))??;
            validate_sponsor_boundary(&updated, transfer, organization_id, source_subscription)?;
            let schedule_id = updated
                .schedule_id
                .ok_or(ExecutionError::Local("source_schedule_missing"))?;
            (schedule_id.clone(), Some(schedule_id))
        }
    };
    let mut tx = pool
        .begin()
        .await
        .map_err(|_| ExecutionError::Local("transfer_transaction_unavailable"))?;
    let current = lock_transfer_context(&mut tx, lease, true).await?;
    if let Some(schedule_id) = schedule_id.as_deref() {
        billing_transfers::record_source_schedule(&mut tx, &current.transfer_id, schedule_id)
            .await
            .map_err(|_| ExecutionError::Local("source_schedule_record_failed"))?;
    }
    billing_transfers::complete_source_adjustment(&mut tx, &current.transfer_id, &reference)
        .await
        .map_err(|_| ExecutionError::Local("source_adjustment_record_failed"))?;
    tx.commit()
        .await
        .map_err(|_| ExecutionError::Local("transfer_commit_failed"))
}

async fn preflight_source_adjustment(
    provider: &dyn SubscriptionProvider,
    transfer: &TransferIntent,
) -> Result<(), ExecutionError> {
    let subscription_id = transfer
        .source_provider_subscription_id
        .as_deref()
        .ok_or(ExecutionError::Local("source_subscription_missing"))?;
    let snapshot = provider.get_sponsored_subscription(subscription_id).await?;
    match transfer.source_kind {
        TransferPayer::Personal => {
            if !personal_source_adjustment_preflight_matches(
                &snapshot,
                subscription_id,
                transfer.effective_from,
            ) {
                return Err(ExecutionError::Local("source_period_changed"));
            }
        }
        TransferPayer::Sponsor => {
            let organization_id = transfer
                .source_organization_id
                .as_deref()
                .ok_or(ExecutionError::Local("source_organisation_missing"))?;
            validate_sponsor_boundary(&snapshot, transfer, organization_id, subscription_id)?;
        }
    }
    Ok(())
}

async fn retire_transfer_source(
    tx: &mut Transaction<'_, Postgres>,
    transfer: &TransferIntent,
    provider_end_reference: &str,
) -> Result<(), ExecutionError> {
    if transfer.effective_from > epoch_now() {
        return Err(ExecutionError::Local("source_paid_term_not_ended"));
    }
    let source_subscription = transfer
        .source_provider_subscription_id
        .as_deref()
        .ok_or(ExecutionError::Local("source_subscription_missing"))?;
    let row = match transfer.source_kind {
        TransferPayer::Personal => {
            sqlx::query(
                "SELECT allocation.allocation_id, allocation.coverage_source_id, \
                        allocation.effective_from, allocation.effective_until, allocation.state \
                 FROM cloud_provider_allocations AS allocation \
                 WHERE allocation.beneficiary_id = $1 AND allocation.provider_namespace = 'stripe' \
                   AND allocation.provider_subscription_id = $2 \
                 ORDER BY allocation.allocation_id COLLATE \"C\" FOR UPDATE",
            )
            .bind(&transfer.beneficiary_id)
            .bind(source_subscription)
            .fetch_all(&mut **tx)
            .await
        }
        TransferPayer::Sponsor => {
            let organization_id = transfer
                .source_organization_id
                .as_deref()
                .ok_or(ExecutionError::Local("source_organisation_missing"))?;
            sqlx::query(
                "SELECT allocation.allocation_id, allocation.coverage_source_id, \
                        allocation.effective_from, allocation.effective_until, allocation.state \
                 FROM cloud_provider_allocations AS allocation \
                 JOIN billing_sponsored_seats AS seat ON seat.allocation_id = allocation.allocation_id \
                 WHERE seat.organization_id = $1 AND seat.beneficiary_id = $2 \
                   AND allocation.beneficiary_id = $2 AND allocation.provider_namespace = 'stripe' \
                   AND allocation.provider_subscription_id = $3 \
                 ORDER BY allocation.allocation_id COLLATE \"C\" FOR UPDATE OF allocation, seat",
            )
            .bind(organization_id)
            .bind(&transfer.beneficiary_id)
            .bind(source_subscription)
            .fetch_all(&mut **tx)
            .await
        }
    }
    .map_err(|_| ExecutionError::Local("source_allocation_lookup_failed"))?;
    if row.len() != 1 {
        return Err(ExecutionError::Local("source_allocation_ambiguous"));
    }
    let allocation = &row[0];
    let allocation_id: String = allocation
        .try_get("allocation_id")
        .map_err(|_| ExecutionError::Local("source_allocation_corrupt"))?;
    let source_id: String = allocation
        .try_get("coverage_source_id")
        .map_err(|_| ExecutionError::Local("source_allocation_corrupt"))?;
    let effective_from: i64 = allocation
        .try_get("effective_from")
        .map_err(|_| ExecutionError::Local("source_allocation_corrupt"))?;
    let effective_until: Option<i64> = allocation
        .try_get("effective_until")
        .map_err(|_| ExecutionError::Local("source_allocation_corrupt"))?;
    let state: String = allocation
        .try_get("state")
        .map_err(|_| ExecutionError::Local("source_allocation_corrupt"))?;
    if effective_from >= transfer.effective_from
        || effective_until.is_some_and(|until| until != transfer.effective_from)
        || !matches!(state.as_str(), "active" | "ended")
    {
        return Err(ExecutionError::Local("source_allocation_changed"));
    }
    let retired = sqlx::query(
        "UPDATE cloud_provider_allocations SET state = 'ended', effective_until = $2, updated_at = now() \
         WHERE allocation_id = $1 AND state IN ('active','ended') \
           AND (effective_until IS NULL OR effective_until = $2)",
    )
    .bind(&allocation_id)
    .bind(transfer.effective_from)
    .execute(&mut **tx)
    .await
    .map_err(|_| ExecutionError::Local("source_allocation_retire_failed"))?;
    if retired.rows_affected() != 1 {
        return Err(ExecutionError::Local("source_allocation_changed"));
    }
    crate::cloud_coverage_reconciliation::retire_source(
        tx,
        &format!("transfer:{}:source-retirement", transfer.transfer_id),
        &transfer.beneficiary_id,
        &source_id,
        provider_end_reference,
        transfer.effective_from,
    )
    .await
    .map_err(|_| ExecutionError::Local("source_coverage_retire_failed"))?;
    Ok(())
}

async fn verify_source_end(
    pool: &PgPool,
    provider: &dyn SubscriptionProvider,
    catalogue: &BillingPriceIds,
    transfer: &TransferIntent,
) -> Result<String, ExecutionError> {
    if transfer.effective_from > epoch_now() {
        return Err(ExecutionError::Local("source_paid_term_not_ended"));
    }
    let subscription_id = transfer
        .source_provider_subscription_id
        .as_deref()
        .ok_or(ExecutionError::Local("source_subscription_missing"))?;
    match provider.get_subscription(subscription_id).await? {
        crate::billing::SubscriptionObservation::Missing => {
            Err(ExecutionError::Local("source_end_evidence_missing"))
        }
        crate::billing::SubscriptionObservation::Current(base) => {
            if base.id != subscription_id {
                return Err(ExecutionError::Local("source_subscription_changed"));
            }
            let current = provider.get_sponsored_subscription(subscription_id).await?;
            if current.subscription_id != subscription_id {
                return Err(ExecutionError::Local("source_subscription_changed"));
            }
            match transfer.source_kind {
                TransferPayer::Personal => {
                    if !personal_source_end_matches(&current, transfer.effective_from) {
                        return Err(ExecutionError::Local("source_end_not_confirmed"));
                    }
                }
                TransferPayer::Sponsor => {
                    let ended_with_subscription = current.status == SubscriptionStatus::Canceled
                        && current.current_period_end == Some(transfer.effective_from);
                    let ended_as_seat = if matches!(
                        current.status,
                        SubscriptionStatus::Active | SubscriptionStatus::PastDue
                    ) {
                        let period_start = current
                            .current_period_start
                            .filter(|start| *start >= transfer.effective_from)
                            .ok_or(ExecutionError::Local("source_period_not_advanced"))?;
                        let organization_id = transfer
                            .source_organization_id
                            .as_deref()
                            .ok_or(ExecutionError::Local("source_organisation_missing"))?;
                        let phases = sponsored_provider_phases(
                            pool,
                            organization_id,
                            catalogue,
                            transfer.effective_from,
                        )
                        .await
                        .map_err(|_| ExecutionError::Local("source_schedule_evidence_invalid"))?;
                        phases
                            .iter()
                            .find(|phase| {
                                phase.start_at_epoch <= period_start
                                    && phase.end_at_epoch.is_none_or(|end| period_start < end)
                            })
                            .is_some_and(|phase| sponsored_phase_matches_snapshot(phase, &current))
                    } else {
                        false
                    };
                    if !ended_with_subscription && !ended_as_seat {
                        return Err(ExecutionError::Local("source_end_not_confirmed"));
                    }
                }
            }
            Ok(format!(
                "stripe-source-end:{}:{}:{}",
                opaque_transfer_id(&transfer.transfer_id),
                transfer.effective_from,
                current.current_period_start.unwrap_or_default()
            ))
        }
    }
}

fn personal_source_end_matches(
    snapshot: &crate::billing::SponsoredSubscriptionSnapshot,
    boundary_epoch: i64,
) -> bool {
    snapshot.status == SubscriptionStatus::Canceled
        && snapshot.current_period_end == Some(boundary_epoch)
}

fn personal_source_adjustment_preflight_matches(
    snapshot: &crate::billing::SponsoredSubscriptionSnapshot,
    expected_subscription_id: &str,
    boundary_epoch: i64,
) -> bool {
    if snapshot.subscription_id != expected_subscription_id
        || snapshot.current_period_end != Some(boundary_epoch)
    {
        return false;
    }
    match snapshot.status {
        SubscriptionStatus::Canceled | SubscriptionStatus::Active => snapshot
            .cancel_at
            .is_none_or(|cancel_at| cancel_at == boundary_epoch),
        _ => false,
    }
}

fn personal_source_adjustment_is_applied(
    snapshot: &crate::billing::SponsoredSubscriptionSnapshot,
    boundary_epoch: i64,
) -> bool {
    snapshot.status == SubscriptionStatus::Canceled
        || snapshot.cancel_at_period_end
        || snapshot.cancel_at == Some(boundary_epoch)
}

fn personal_source_adjustment_retry_allowed(
    now_epoch: i64,
    boundary_epoch: i64,
    provider_adjustment_is_applied: bool,
) -> bool {
    now_epoch < boundary_epoch || provider_adjustment_is_applied
}

fn sponsored_phase_matches_snapshot(
    expected: &crate::billing::SponsoredSubscriptionPhase,
    actual: &crate::billing::SponsoredSubscriptionSnapshot,
) -> bool {
    let expected = expected
        .items
        .iter()
        .map(|item| (item.provider_price_id.as_str(), item.quantity))
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut actual_quantities = std::collections::BTreeMap::<&str, i64>::new();
    for item in &actual.items {
        if item.quantity > 0 {
            *actual_quantities
                .entry(item.provider_price_id.as_str())
                .or_default() += item.quantity;
        }
    }
    expected == actual_quantities
}

async fn restore_source(
    pool: &PgPool,
    provider: &dyn SubscriptionProvider,
    catalogue: &BillingPriceIds,
    lease: &TransferLease,
) -> Result<Advance, ExecutionError> {
    let transfer = &lease.transfer;
    if transfer.source_restoration_payment_reference.is_some() {
        let subscription_id = transfer
            .source_restoration_provider_subscription_id
            .as_deref()
            .ok_or(ExecutionError::Local(
                "source_restoration_subscription_missing",
            ))?;
        let snapshot = provider.get_sponsored_subscription(subscription_id).await?;
        if snapshot.subscription_id != subscription_id
            || snapshot.status != SubscriptionStatus::Active
            || snapshot.cancel_at_period_end
            || snapshot.cancel_at.is_some()
            || snapshot.items.len() != 1
            || snapshot.items[0].provider_price_id != catalogue.id_for(transfer.offer)
            || snapshot.items[0].quantity != 1
            || snapshot
                .current_period_start
                .is_none_or(|start| start > epoch_now())
            || snapshot
                .current_period_end
                .is_none_or(|end| end <= epoch_now())
        {
            return Err(ExecutionError::Local("source_restoration_not_active"));
        }
        let payment_reference = transfer
            .source_restoration_payment_reference
            .as_deref()
            .ok_or(ExecutionError::Local("source_restoration_payment_missing"))?;
        let reference = format!(
            "stripe-source-restored:{}:{}",
            opaque_transfer_id(&transfer.transfer_id),
            opaque_transfer_id(payment_reference)
        );
        let mut tx = pool
            .begin()
            .await
            .map_err(|_| ExecutionError::Local("transfer_transaction_unavailable"))?;
        lock_transfer_for_lease_context(&mut tx, lease).await?;
        let account = sqlx::query(
            "SELECT state, stripe_customer_id, stripe_subscription_id, offer, payment_reference, \
                    paid_through_epoch \
             FROM billing_personal_accounts \
             WHERE user_id = $1 AND operation_id = $2 FOR UPDATE",
        )
        .bind(&transfer.beneficiary_id)
        .bind(
            transfer
                .source_restoration_operation_id
                .as_deref()
                .unwrap_or_default(),
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(|_| ExecutionError::Local("source_restoration_account_lookup_failed"))?
        .ok_or(ExecutionError::Local("source_restoration_account_missing"))?;
        let account_state: String = account
            .try_get("state")
            .map_err(|_| ExecutionError::Local("source_restoration_account_corrupt"))?;
        let account_customer: Option<String> = account
            .try_get("stripe_customer_id")
            .map_err(|_| ExecutionError::Local("source_restoration_account_corrupt"))?;
        let account_subscription: Option<String> = account
            .try_get("stripe_subscription_id")
            .map_err(|_| ExecutionError::Local("source_restoration_account_corrupt"))?;
        let account_offer: String = account
            .try_get("offer")
            .map_err(|_| ExecutionError::Local("source_restoration_account_corrupt"))?;
        let account_payment: Option<String> = account
            .try_get("payment_reference")
            .map_err(|_| ExecutionError::Local("source_restoration_account_corrupt"))?;
        let paid_through: Option<i64> = account
            .try_get("paid_through_epoch")
            .map_err(|_| ExecutionError::Local("source_restoration_account_corrupt"))?;
        if account_state != "active"
            || account_customer.as_deref() != snapshot.customer_id.as_deref()
            || account_subscription.as_deref() != Some(subscription_id)
            || account_offer != transfer.offer.as_str()
            || account_payment.as_deref() != Some(payment_reference)
            || paid_through.is_none_or(|end| end <= transfer.effective_from)
        {
            return Err(ExecutionError::Local("source_restoration_account_unpaid"));
        }
        billing_transfers::complete_source_restoration(&mut tx, &transfer.transfer_id, &reference)
            .await
            .map_err(|_| ExecutionError::Local("source_restoration_completion_rejected"))?;
        tx.commit()
            .await
            .map_err(|_| ExecutionError::Local("transfer_commit_failed"))?;
        return Ok(Advance::Advanced);
    }

    match transfer.source_kind {
        TransferPayer::Personal => {
            restore_personal_source(pool, provider, catalogue, lease).await?;
            Ok(Advance::WaitingForInvoice)
        }
        // A sponsor payer uses a shared subscription. Until that rollback can be expressed and
        // read back without creating a second subscription for the organisation, keep it fenced
        // for operator recovery instead of claiming that the payer has been restored.
        TransferPayer::Sponsor => Err(ExecutionError::Local(
            "sponsored_source_restoration_requires_review",
        )),
    }
}

async fn restore_personal_source(
    pool: &PgPool,
    provider: &dyn SubscriptionProvider,
    catalogue: &BillingPriceIds,
    lease: &TransferLease,
) -> Result<(), ExecutionError> {
    let transfer = &lease.transfer;
    if transfer.source_kind != TransferPayer::Personal {
        return Err(ExecutionError::Local("source_binding_changed"));
    }
    let old_subscription_id = transfer
        .source_provider_subscription_id
        .as_deref()
        .ok_or(ExecutionError::Local("source_subscription_missing"))?;
    let original = provider
        .get_sponsored_subscription(old_subscription_id)
        .await?;
    if original.subscription_id != old_subscription_id
        || original.status != SubscriptionStatus::Canceled
        || original.cancel_at_period_end
        || original.cancel_at.is_some()
    {
        return Err(ExecutionError::Local("source_subscription_not_ended"));
    }
    let customer_id = original
        .customer_id
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or(ExecutionError::Local("source_customer_missing"))?;
    let payment_method_id = original
        .default_payment_method_id
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or(ExecutionError::Local("source_payment_method_missing"))?;
    let operation_id = transfer_personal_restoration_operation_id(&transfer.transfer_id);

    let mut tx = pool
        .begin()
        .await
        .map_err(|_| ExecutionError::Local("transfer_transaction_unavailable"))?;
    founding_allocator::lock_capacity(&mut tx)
        .await
        .map_err(|_| ExecutionError::Local("transfer_capacity_lock_failed"))?;
    sqlx::query("SELECT id FROM users WHERE id = $1 FOR UPDATE")
        .bind(&transfer.beneficiary_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|_| ExecutionError::Local("transfer_beneficiary_missing"))?
        .ok_or(ExecutionError::Local("transfer_beneficiary_missing"))?;
    lock_transfer_for_lease(
        &mut tx,
        &TransferLease {
            transfer: transfer.clone(),
            lease_token: lease.lease_token.clone(),
            lease_until_epoch: lease.lease_until_epoch,
        },
    )
    .await?;

    let existing_account: Option<(String, Option<String>, String, Option<String>)> =
        sqlx::query_as(
            "SELECT operation_id, stripe_subscription_id, state, stripe_customer_id \
         FROM billing_personal_accounts \
         WHERE user_id = $1 FOR UPDATE",
        )
        .bind(&transfer.beneficiary_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|_| ExecutionError::Local("source_account_lookup_failed"))?;
    let is_original_account = existing_account
        .as_ref()
        .is_some_and(|(_, subscription, _, _)| {
            subscription.as_deref() == Some(old_subscription_id)
        });
    let is_resumable_restoration = existing_account.as_ref().is_some_and(
        |(account_operation, subscription, state, account_customer)| {
            personal_restoration_account_matches(
                account_operation,
                subscription.as_deref(),
                state,
                account_customer.as_deref(),
                &operation_id,
                customer_id,
                transfer
                    .source_restoration_provider_subscription_id
                    .as_deref(),
            )
        },
    );
    if !is_original_account && !is_resumable_restoration {
        return Err(ExecutionError::Local("source_account_binding_changed"));
    }
    let allocation_reference = if is_resumable_restoration {
        sqlx::query_scalar::<_, String>(
            "SELECT allocation_reference FROM billing_personal_accounts WHERE user_id = $1",
        )
        .bind(&transfer.beneficiary_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|_| ExecutionError::Local("source_account_corrupt"))?
    } else {
        sqlx::query(
            "UPDATE billing_personal_accounts SET state = 'canceled', updated_at = now() \
             WHERE user_id = $1 AND stripe_subscription_id = $2 \
               AND state IN ('active','past_due','unpaid')",
        )
        .bind(&transfer.beneficiary_id)
        .bind(old_subscription_id)
        .execute(&mut *tx)
        .await
        .map_err(|_| ExecutionError::Local("source_account_state_update_failed"))?;
        let request_key = format!("{}:source-restoration", transfer.transfer_id);
        let provider_key = format!("{}:source-restoration", transfer.provider_idempotency_key);
        sqlx::query(
            "INSERT INTO billing_operations \
             (operation_id, idempotency_key, request_hash, actor_user_id, payer_id, beneficiary_id, \
              offer, quote_version, quote_expires_at_epoch, provider_idempotency_key, state) \
             VALUES ($1,$2,$3,$4,$4,$4,$5,$6,$7,$8,'pending') ON CONFLICT (operation_id) DO NOTHING",
        )
        .bind(&operation_id)
        .bind(&request_key)
        .bind(&transfer.request_hash)
        .bind(&transfer.beneficiary_id)
        .bind(transfer.offer.as_str())
        .bind(transfer.quote_version)
        .bind(epoch_now().saturating_add(900))
        .bind(&provider_key)
        .execute(&mut *tx)
        .await
        .map_err(|_| ExecutionError::Local("source_operation_insert_failed"))?;
        let allocation = crate::personal_billing::begin_account(
            &mut tx,
            &transfer.beneficiary_id,
            &operation_id,
            transfer.offer,
            epoch_now().saturating_add(86_400),
            epoch_now(),
        )
        .await
        .map_err(|_| ExecutionError::Local("source_account_reservation_failed"))?;
        sqlx::query(
            "UPDATE billing_personal_accounts SET stripe_customer_id = $2, updated_at = now() \
             WHERE user_id = $1 AND operation_id = $3 AND state = 'pending'",
        )
        .bind(&transfer.beneficiary_id)
        .bind(customer_id)
        .bind(&operation_id)
        .execute(&mut *tx)
        .await
        .map_err(|_| ExecutionError::Local("source_customer_binding_failed"))?;
        allocation
    };
    billing_transfers::begin_source_restoration_payment(
        &mut tx,
        &transfer.transfer_id,
        &operation_id,
    )
    .await
    .map_err(|_| ExecutionError::Local("source_restoration_binding_failed"))?;
    tx.commit()
        .await
        .map_err(|_| ExecutionError::Local("transfer_commit_failed"))?;

    if transfer
        .source_restoration_provider_subscription_id
        .is_none()
    {
        let created_at = sqlx::query_scalar::<_, i64>(
            "SELECT EXTRACT(EPOCH FROM created_at)::BIGINT FROM billing_operations \
             WHERE operation_id = $1",
        )
        .bind(&operation_id)
        .fetch_optional(pool)
        .await
        .map_err(|_| ExecutionError::Local("source_operation_lookup_failed"))?
        .ok_or(ExecutionError::Local("source_operation_missing"))?;
        let restored = resolve_personal_source_restoration_subscription(
            provider,
            transfer,
            customer_id,
            payment_method_id,
            catalogue.id_for(transfer.offer),
            &allocation_reference,
            &operation_id,
            created_at,
            epoch_now(),
        )
        .await?;
        let mut tx = pool
            .begin()
            .await
            .map_err(|_| ExecutionError::Local("transfer_transaction_unavailable"))?;
        founding_allocator::lock_capacity(&mut tx)
            .await
            .map_err(|_| ExecutionError::Local("transfer_capacity_lock_failed"))?;
        lock_transfer_for_lease(
            &mut tx,
            &TransferLease {
                transfer: transfer.clone(),
                lease_token: lease.lease_token.clone(),
                lease_until_epoch: lease.lease_until_epoch,
            },
        )
        .await?;
        sqlx::query(
            "UPDATE billing_personal_accounts SET stripe_customer_id = $2, stripe_subscription_id = $3, updated_at = now() \
             WHERE user_id = $1 AND operation_id = $4 AND state = 'pending'",
        )
        .bind(&transfer.beneficiary_id)
        .bind(customer_id)
        .bind(&restored.subscription_id)
        .bind(&operation_id)
        .execute(&mut *tx)
        .await
        .map_err(|_| ExecutionError::Local("source_subscription_binding_failed"))?;
        billing_transfers::record_source_restoration_subscription(
            &mut tx,
            &transfer.transfer_id,
            &operation_id,
            &restored.subscription_id,
        )
        .await
        .map_err(|_| ExecutionError::Local("source_restoration_binding_failed"))?;
        tx.commit()
            .await
            .map_err(|_| ExecutionError::Local("transfer_commit_failed"))?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn resolve_personal_source_restoration_subscription(
    provider: &dyn SubscriptionProvider,
    transfer: &TransferIntent,
    customer_id: &str,
    payment_method_id: &str,
    price_id: &str,
    allocation_reference: &str,
    operation_id: &str,
    operation_created_at_epoch: i64,
    now_epoch: i64,
) -> Result<crate::billing::SponsoredSubscriptionSnapshot, ExecutionError> {
    let restored = if let Some(existing) = provider
        .find_personal_source_restoration_subscription(
            operation_id,
            &transfer.transfer_id,
            &transfer.beneficiary_id,
            customer_id,
            payment_method_id,
            price_id,
            allocation_reference,
        )
        .await?
    {
        existing
    } else {
        if !restoration_create_retry_is_safe(now_epoch, operation_created_at_epoch) {
            return Err(ExecutionError::Local(
                "source_restoration_idempotency_window_elapsed",
            ));
        }
        provider
            .create_personal_source_restoration_subscription(
                &transfer.transfer_id,
                &transfer.beneficiary_id,
                customer_id,
                payment_method_id,
                transfer.offer,
                price_id,
                allocation_reference,
                operation_id,
                &format!("{}:source-restoration", transfer.provider_idempotency_key),
            )
            .await?
    };
    if restored.customer_id.as_deref() != Some(customer_id)
        || restored.default_payment_method_id.as_deref() != Some(payment_method_id)
        || !matches!(
            restored.status,
            SubscriptionStatus::Active | SubscriptionStatus::Incomplete
        )
        || restored.items.len() != 1
        || restored.items[0].provider_price_id != price_id
        || restored.items[0].quantity != 1
    {
        return Err(ExecutionError::Local(
            "source_restoration_provider_binding_invalid",
        ));
    }
    Ok(restored)
}

async fn seed_personal_destination(
    pool: &PgPool,
    transfer: &TransferIntent,
    customer_id: &str,
    allocation_reference: &str,
    operation_id: &str,
) -> Result<(), ExecutionError> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|_| ExecutionError::Local("transfer_transaction_unavailable"))?;
    founding_allocator::lock_capacity(&mut tx)
        .await
        .map_err(|_| ExecutionError::Local("transfer_capacity_lock_failed"))?;
    sqlx::query("SELECT id FROM users WHERE id = $1 FOR UPDATE")
        .bind(&transfer.beneficiary_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|_| ExecutionError::Local("transfer_beneficiary_missing"))?
        .ok_or(ExecutionError::Local("transfer_beneficiary_missing"))?;
    let locked = lock_transfer_for_lease(
        &mut tx,
        &TransferLease {
            transfer: transfer.clone(),
            lease_token: transfer.execution_lease_token.clone().unwrap_or_default(),
            lease_until_epoch: transfer.execution_lease_until_epoch.unwrap_or_default(),
        },
    )
    .await?;
    if locked.destination_kind != TransferPayer::Personal
        || locked.destination_customer_id.as_deref() != Some(customer_id)
    {
        return Err(ExecutionError::Local("destination_binding_changed"));
    }
    sqlx::query(
        "INSERT INTO billing_operations \
         (operation_id, idempotency_key, request_hash, actor_user_id, payer_id, beneficiary_id, offer, \
          quote_version, quote_expires_at_epoch, provider_idempotency_key, state, result_code) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,'pending',NULL) \
         ON CONFLICT (operation_id) DO NOTHING",
    )
    .bind(operation_id)
    .bind(&transfer.idempotency_key)
    .bind(&transfer.request_hash)
    .bind(&transfer.beneficiary_id)
    .bind(&transfer.beneficiary_id)
    .bind(&transfer.beneficiary_id)
    .bind(transfer.offer.as_str())
    .bind(transfer.quote_version)
    .bind(transfer.quote_expires_at_epoch)
    .bind(format!("{}:destination", transfer.provider_idempotency_key))
    .execute(&mut *tx)
    .await
    .map_err(|_| ExecutionError::Local("personal_transfer_operation_conflict"))?;
    let seeded_account = sqlx::query(
        "INSERT INTO billing_personal_accounts \
         (user_id, operation_id, offer, stripe_customer_id, state, pending_expires_at_epoch, \
          stripe_coverage_binding_required, allocation_reference) \
         VALUES ($1,$2,$3,$4,'pending',$5,TRUE,$6) \
         ON CONFLICT (user_id) DO UPDATE SET operation_id = EXCLUDED.operation_id, \
           offer = EXCLUDED.offer, stripe_customer_id = EXCLUDED.stripe_customer_id, \
           stripe_subscription_id = NULL, state = 'pending', pending_expires_at_epoch = EXCLUDED.pending_expires_at_epoch, \
           paid_through_epoch = NULL, cancel_at_period_end = FALSE, cancellation_requested_at = NULL, \
           stripe_coverage_binding_required = TRUE, allocation_reference = EXCLUDED.allocation_reference, updated_at = now() \
         WHERE billing_personal_accounts.state = 'canceled' \
            OR (billing_personal_accounts.operation_id = EXCLUDED.operation_id \
                AND billing_personal_accounts.state = 'pending')",
    )
    .bind(&transfer.beneficiary_id)
    .bind(operation_id)
    .bind(transfer.offer.as_str())
    .bind(customer_id)
    .bind(transfer.effective_from.saturating_add(86_400))
    .bind(allocation_reference)
    .execute(&mut *tx)
    .await
    .map_err(|_| ExecutionError::Local("personal_destination_already_bound"))?;
    if seeded_account.rows_affected() != 1 {
        return Err(ExecutionError::Local("personal_destination_already_bound"));
    }
    tx.commit()
        .await
        .map_err(|_| ExecutionError::Local("transfer_commit_failed"))
}

async fn seed_sponsored_destination(
    pool: &PgPool,
    transfer: &TransferIntent,
    organization_id: &str,
    snapshot: &crate::billing::SponsoredSubscriptionSnapshot,
    catalogue: &BillingPriceIds,
) -> Result<(), ExecutionError> {
    let operation_id = transfer_sponsored_operation_id(&transfer.transfer_id);
    let mut tx = pool
        .begin()
        .await
        .map_err(|_| ExecutionError::Local("transfer_transaction_unavailable"))?;
    founding_allocator::lock_capacity(&mut tx)
        .await
        .map_err(|_| ExecutionError::Local("transfer_capacity_lock_failed"))?;
    sqlx::query("SELECT id FROM users WHERE id = $1 FOR UPDATE")
        .bind(&transfer.beneficiary_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|_| ExecutionError::Local("transfer_beneficiary_missing"))?
        .ok_or(ExecutionError::Local("transfer_beneficiary_missing"))?;
    sqlx::query("SELECT id FROM organizations WHERE id = $1 FOR UPDATE")
        .bind(organization_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|_| ExecutionError::Local("destination_organisation_missing"))?
        .ok_or(ExecutionError::Local("destination_organisation_missing"))?;
    let lease = TransferLease {
        transfer: transfer.clone(),
        lease_token: transfer.execution_lease_token.clone().unwrap_or_default(),
        lease_until_epoch: transfer.execution_lease_until_epoch.unwrap_or_default(),
    };
    let locked = lock_transfer_for_lease(&mut tx, &lease).await?;
    if locked.destination_kind != TransferPayer::Sponsor
        || locked.destination_organization_id.as_deref() != Some(organization_id)
    {
        return Err(ExecutionError::Local("destination_binding_changed"));
    }
    let existing: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM billing_sponsored_operations WHERE operation_id = $1)",
    )
    .bind(&operation_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|_| ExecutionError::Local("destination_operation_lookup_failed"))?;
    if !existing {
        sqlx::query(
            "INSERT INTO billing_sponsored_operations \
             (operation_id, organization_id, actor_user_id, idempotency_key, request_hash, action, \
              beneficiary_id, offer, quote_version, quote_expires_at_epoch, effective_from, \
              provider_idempotency_key, provider_customer_id, provider_subscription_id, state) \
             VALUES ($1,$2,$3,$4,$5,'add',$6,$7,$8,$9,$10,$11,$12,$13,'provider_pending')",
        )
        .bind(&operation_id)
        .bind(organization_id)
        .bind(&transfer.actor_user_id)
        .bind(format!("transfer:{}", transfer.transfer_id))
        .bind(&transfer.request_hash)
        .bind(&transfer.beneficiary_id)
        .bind(transfer.offer.as_str())
        .bind(transfer.quote_version)
        .bind(transfer.quote_expires_at_epoch)
        .bind(transfer.effective_from)
        .bind(format!("{}:destination", transfer.provider_idempotency_key))
        .bind(snapshot.customer_id.as_deref())
        .bind(&snapshot.subscription_id)
        .execute(&mut *tx)
        .await
        .map_err(|_| ExecutionError::Local("destination_operation_insert_failed"))?;
        let item_id = snapshot
            .items
            .iter()
            .find(|item| item.provider_price_id == catalogue.id_for(transfer.offer))
            .map(|item| item.provider_item_id.as_str());
        sqlx::query(
            "INSERT INTO billing_sponsored_seats \
             (seat_id, organization_id, beneficiary_id, offer, effective_from, state, operation_id, provider_item_id) \
             VALUES ($1,$2,$3,$4,$5,'pending',$6,$7)",
        )
        .bind(format!("seat:{operation_id}"))
        .bind(organization_id)
        .bind(&transfer.beneficiary_id)
        .bind(transfer.offer.as_str())
        .bind(transfer.effective_from)
        .bind(&operation_id)
        .bind(item_id)
        .execute(&mut *tx)
        .await
        .map_err(|_| ExecutionError::Local("destination_seat_insert_failed"))?;
    }
    tx.commit()
        .await
        .map_err(|_| ExecutionError::Local("transfer_commit_failed"))
}

async fn prepare_sponsored_source_removal(
    pool: &PgPool,
    transfer: &TransferIntent,
    organization_id: &str,
    _catalogue: &BillingPriceIds,
) -> Result<(), ExecutionError> {
    let operation_id = format!(
        "{}:source",
        transfer_sponsored_operation_id(&transfer.transfer_id)
    );
    let mut tx = pool
        .begin()
        .await
        .map_err(|_| ExecutionError::Local("transfer_transaction_unavailable"))?;
    founding_allocator::lock_capacity(&mut tx)
        .await
        .map_err(|_| ExecutionError::Local("transfer_capacity_lock_failed"))?;
    sqlx::query("SELECT id FROM users WHERE id = $1 FOR UPDATE")
        .bind(&transfer.beneficiary_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|_| ExecutionError::Local("transfer_beneficiary_missing"))?
        .ok_or(ExecutionError::Local("transfer_beneficiary_missing"))?;
    sqlx::query("SELECT id FROM organizations WHERE id = $1 FOR UPDATE")
        .bind(organization_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|_| ExecutionError::Local("source_organisation_missing"))?
        .ok_or(ExecutionError::Local("source_organisation_missing"))?;
    let lease = TransferLease {
        transfer: transfer.clone(),
        lease_token: transfer.execution_lease_token.clone().unwrap_or_default(),
        lease_until_epoch: transfer.execution_lease_until_epoch.unwrap_or_default(),
    };
    let current = lock_transfer_context(&mut tx, &lease, false).await?;
    if current.source_kind != TransferPayer::Sponsor
        || current.source_organization_id.as_deref() != Some(organization_id)
    {
        return Err(ExecutionError::Local("source_binding_changed"));
    }
    let seat = sqlx::query(
        "SELECT seat_id, offer, effective_from, effective_until, state \
         FROM billing_sponsored_seats WHERE organization_id = $1 AND beneficiary_id = $2 FOR UPDATE",
    )
    .bind(organization_id)
    .bind(&transfer.beneficiary_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|_| ExecutionError::Local("source_seat_lookup_failed"))?
    .ok_or(ExecutionError::Local("source_seat_missing"))?;
    let seat_id: String = seat
        .try_get("seat_id")
        .map_err(|_| ExecutionError::Local("source_seat_corrupt"))?;
    let offer: String = seat
        .try_get("offer")
        .map_err(|_| ExecutionError::Local("source_seat_corrupt"))?;
    let effective_from: i64 = seat
        .try_get("effective_from")
        .map_err(|_| ExecutionError::Local("source_seat_corrupt"))?;
    let until: Option<i64> = seat
        .try_get("effective_until")
        .map_err(|_| ExecutionError::Local("source_seat_corrupt"))?;
    let seat_state: String = seat
        .try_get("state")
        .map_err(|_| ExecutionError::Local("source_seat_corrupt"))?;
    if offer != transfer.offer.as_str()
        || !matches!(seat_state.as_str(), "active" | "scheduled_removal")
        || until.is_some_and(|value| value != transfer.effective_from)
    {
        return Err(ExecutionError::Local("source_seat_changed"));
    }
    sqlx::query(
        "INSERT INTO billing_sponsored_operations \
         (operation_id, organization_id, actor_user_id, idempotency_key, request_hash, action, \
          beneficiary_id, offer, quote_version, quote_expires_at_epoch, effective_from, effective_until, \
          provider_idempotency_key, provider_customer_id, provider_subscription_id, state) \
         VALUES ($1,$2,$3,$4,$5,'remove',$6,$7,$8,$9,$10,$11,$12, \
           (SELECT provider_customer_id FROM billing_sponsored_subscriptions WHERE organization_id = $2), \
           $13,'provider_pending') ON CONFLICT (operation_id) DO NOTHING",
    )
    .bind(&operation_id)
    .bind(organization_id)
    .bind(&transfer.actor_user_id)
    .bind(format!("transfer:{}:source", transfer.transfer_id))
    .bind(&transfer.request_hash)
    .bind(&transfer.beneficiary_id)
    .bind(offer)
    .bind(transfer.quote_version)
    .bind(transfer.quote_expires_at_epoch)
    .bind(effective_from)
    .bind(transfer.effective_from)
    .bind(format!("{}:source", transfer.provider_idempotency_key))
    .bind(&transfer.source_provider_subscription_id)
    .execute(&mut *tx)
    .await
    .map_err(|_| ExecutionError::Local("source_operation_insert_failed"))?;
    sqlx::query(
        "UPDATE billing_sponsored_seats SET state = 'scheduled_removal', effective_until = $2, updated_at = now() \
         WHERE seat_id = $1 AND state IN ('active','scheduled_removal') \
           AND (effective_until IS NULL OR effective_until = $2)",
    )
    .bind(seat_id)
    .bind(transfer.effective_from)
    .execute(&mut *tx)
    .await
    .map_err(|_| ExecutionError::Local("source_seat_update_failed"))?;
    tx.commit()
        .await
        .map_err(|_| ExecutionError::Local("transfer_commit_failed"))
}

async fn lock_transfer_context(
    tx: &mut Transaction<'_, Postgres>,
    lease: &TransferLease,
    allow_source_adjustment: bool,
) -> Result<TransferIntent, ExecutionError> {
    let transfer = lock_transfer_for_lease_context(tx, lease).await?;
    validate_local_bindings(tx, &transfer, allow_source_adjustment).await?;
    Ok(transfer)
}

async fn lock_transfer_for_lease_context(
    tx: &mut Transaction<'_, Postgres>,
    lease: &TransferLease,
) -> Result<TransferIntent, ExecutionError> {
    founding_allocator::lock_capacity(tx)
        .await
        .map_err(|_| ExecutionError::Local("transfer_capacity_lock_failed"))?;
    let mut users = vec![
        lease.transfer.actor_user_id.clone(),
        lease.transfer.counterparty_user_id.clone(),
        lease.transfer.beneficiary_id.clone(),
    ];
    users.sort();
    users.dedup();
    for user_id in users {
        sqlx::query("SELECT id FROM users WHERE id = $1 FOR UPDATE")
            .bind(user_id)
            .fetch_optional(&mut **tx)
            .await
            .map_err(|_| ExecutionError::Local("transfer_user_lock_failed"))?
            .ok_or(ExecutionError::Local("transfer_user_missing"))?;
    }
    let mut organizations = lease
        .transfer
        .source_organization_id
        .iter()
        .chain(lease.transfer.destination_organization_id.iter())
        .cloned()
        .collect::<Vec<_>>();
    organizations.sort();
    organizations.dedup();
    for organization_id in organizations {
        sqlx::query("SELECT id FROM organizations WHERE id = $1 FOR UPDATE")
            .bind(organization_id)
            .fetch_optional(&mut **tx)
            .await
            .map_err(|_| ExecutionError::Local("transfer_organisation_lock_failed"))?
            .ok_or(ExecutionError::Local("transfer_organisation_missing"))?;
    }
    lock_transfer_for_lease(tx, lease).await
}

async fn validate_local_bindings(
    tx: &mut Transaction<'_, Postgres>,
    transfer: &TransferIntent,
    allow_source_adjustment: bool,
) -> Result<(), ExecutionError> {
    let (source_offer, source_period_end, source_subscription_id, source_status, source_cancelled) =
        match transfer.source_kind {
            TransferPayer::Personal => {
                sqlx::query_as::<_, (String, Option<i64>, String, String, bool)>(
                    "SELECT offer, paid_through_epoch, stripe_subscription_id, state, \
                        (cancel_at_period_end OR cancellation_requested_at IS NOT NULL) \
                 FROM billing_personal_accounts WHERE user_id = $1 FOR UPDATE",
                )
                .bind(&transfer.beneficiary_id)
                .fetch_optional(&mut **tx)
                .await
                .map_err(|_| ExecutionError::Local("source_billing_lookup_failed"))?
                .ok_or(ExecutionError::Local("source_billing_missing"))?
            }
            TransferPayer::Sponsor => {
                let organization_id = transfer
                    .source_organization_id
                    .as_deref()
                    .ok_or(ExecutionError::Local("source_organisation_missing"))?;
                sqlx::query_as::<_, (String, Option<i64>, String, String, bool)>(
                    "SELECT seat.offer, subscription.current_period_end, \
                        subscription.provider_subscription_id, subscription.status, \
                        (subscription.cancel_at_period_end OR subscription.cancel_at IS NOT NULL) \
                 FROM billing_sponsored_seats AS seat \
                 JOIN billing_sponsored_subscriptions AS subscription \
                   ON subscription.organization_id = seat.organization_id \
                 WHERE seat.organization_id = $1 AND seat.beneficiary_id = $2 \
                   AND seat.state IN ('active','scheduled_removal') \
                 FOR UPDATE OF seat, subscription",
                )
                .bind(organization_id)
                .bind(&transfer.beneficiary_id)
                .fetch_optional(&mut **tx)
                .await
                .map_err(|_| ExecutionError::Local("source_billing_lookup_failed"))?
                .ok_or(ExecutionError::Local("source_billing_missing"))?
            }
        };
    let source_period_matches = if allow_source_adjustment {
        source_period_end.is_some_and(|end| end >= transfer.effective_from)
    } else {
        source_period_end == Some(transfer.effective_from)
    };
    if (source_status != "active" && !allow_source_adjustment)
        || (source_cancelled && !allow_source_adjustment)
        || source_subscription_id
            != transfer
                .source_provider_subscription_id
                .as_deref()
                .unwrap_or_default()
        || !source_period_matches
    {
        return Err(ExecutionError::Local("source_binding_changed"));
    }
    let award = if let Some(award_id) = transfer.founding_award_id.as_deref() {
        Some(
            founding_allocator::load_award(tx, award_id)
                .await
                .map_err(|_| ExecutionError::Local("founding_award_lookup_failed"))?
                .ok_or(ExecutionError::Local("founding_award_missing"))?,
        )
    } else {
        None
    };
    let parsed_source_offer = BillingOffer::ALL
        .into_iter()
        .find(|offer| offer.as_str() == source_offer)
        .ok_or(ExecutionError::Local("source_offer_invalid"))?;
    let expected_offer = billing_transfers::offer_at_transfer_boundary(
        &source_offer,
        transfer.effective_from,
        award.as_ref(),
    )
    .map_err(|_| ExecutionError::Local("founding_offer_changed"))?;
    let _source_offer = parsed_source_offer;
    if expected_offer != transfer.offer {
        return Err(ExecutionError::Local("source_offer_changed"));
    }
    if transfer.destination_kind == TransferPayer::Sponsor {
        let organization_id = transfer
            .destination_organization_id
            .as_deref()
            .ok_or(ExecutionError::Local("destination_organisation_missing"))?;
        let destination = sqlx::query_as::<_, (String, Option<i64>, String)>(
            "SELECT provider_subscription_id, current_period_end, status \
             FROM billing_sponsored_subscriptions WHERE organization_id = $1 FOR UPDATE",
        )
        .bind(organization_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(|_| ExecutionError::Local("destination_subscription_lookup_failed"))?
        .ok_or(ExecutionError::Local("destination_subscription_missing"))?;
        let destination_period_matches = if transfer.destination_payment_reference.is_some() {
            destination
                .1
                .is_some_and(|end| end >= transfer.effective_from)
        } else {
            destination.1 == Some(transfer.effective_from)
        };
        if destination.0
            != transfer
                .destination_provider_subscription_id
                .as_deref()
                .unwrap_or_default()
            || !destination_period_matches
            || destination.2 != "active"
        {
            return Err(ExecutionError::Local("destination_subscription_changed"));
        }
        let owned_operation = transfer_sponsored_operation_id(&transfer.transfer_id);
        let conflicting_seat: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM billing_sponsored_seats \
             WHERE organization_id = $1 AND beneficiary_id = $2 \
               AND state IN ('pending','active','scheduled_removal') \
               AND operation_id <> $3)",
        )
        .bind(organization_id)
        .bind(&transfer.beneficiary_id)
        .bind(owned_operation)
        .fetch_one(&mut **tx)
        .await
        .map_err(|_| ExecutionError::Local("destination_seat_lookup_failed"))?;
        if conflicting_seat {
            return Err(ExecutionError::Local("destination_already_covers"));
        }
    } else if transfer.destination_operation_id.is_some() {
        let account = sqlx::query(
            "SELECT operation_id, offer, stripe_customer_id, stripe_subscription_id, \
                    allocation_reference, state FROM billing_personal_accounts \
             WHERE user_id = $1 FOR UPDATE",
        )
        .bind(&transfer.beneficiary_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(|_| ExecutionError::Local("destination_personal_lookup_failed"))?
        .ok_or(ExecutionError::Local("destination_personal_missing"))?;
        let expected_operation = transfer_personal_operation_id(&transfer.transfer_id);
        let expected_allocation = transfer_allocation_reference(&transfer.transfer_id);
        let operation: String = account
            .try_get("operation_id")
            .map_err(|_| ExecutionError::Local("destination_personal_corrupt"))?;
        let offer: String = account
            .try_get("offer")
            .map_err(|_| ExecutionError::Local("destination_personal_corrupt"))?;
        let customer: Option<String> = account
            .try_get("stripe_customer_id")
            .map_err(|_| ExecutionError::Local("destination_personal_corrupt"))?;
        let subscription: Option<String> = account
            .try_get("stripe_subscription_id")
            .map_err(|_| ExecutionError::Local("destination_personal_corrupt"))?;
        let allocation: Option<String> = account
            .try_get("allocation_reference")
            .map_err(|_| ExecutionError::Local("destination_personal_corrupt"))?;
        let state: String = account
            .try_get("state")
            .map_err(|_| ExecutionError::Local("destination_personal_corrupt"))?;
        if operation != expected_operation
            || offer != transfer.offer.as_str()
            || customer.as_deref() != transfer.destination_customer_id.as_deref()
            || allocation.as_deref() != Some(expected_allocation.as_str())
            || subscription.as_deref().is_some_and(|value| {
                transfer.destination_provider_subscription_id.as_deref() != Some(value)
            })
            || (transfer.destination_payment_reference.is_some()
                && (subscription.as_deref()
                    != transfer.destination_provider_subscription_id.as_deref()
                    || !matches!(state.as_str(), "active" | "past_due")))
        {
            return Err(ExecutionError::Local("destination_personal_changed"));
        }
    }
    Ok(())
}

async fn lock_transfer_for_lease(
    tx: &mut Transaction<'_, Postgres>,
    lease: &TransferLease,
) -> Result<TransferIntent, ExecutionError> {
    let row = sqlx::query(
        "SELECT *, EXTRACT(EPOCH FROM execution_lease_until)::BIGINT AS execution_lease_until_epoch \
         FROM billing_transfer_intents WHERE transfer_id = $1 AND execution_lease_token = $2 \
           AND execution_lease_until > now() FOR UPDATE",
    )
    .bind(&lease.transfer.transfer_id)
    .bind(&lease.lease_token)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|_| ExecutionError::Local("transfer_lease_lookup_failed"))?
    .ok_or(ExecutionError::Local("transfer_lease_expired"))?;
    let transfer = billing_transfers::transfer_from_row(&row)
        .map_err(|_| ExecutionError::Local("transfer_row_corrupt"))?;
    if transfer.execution_manual_review_required {
        return Err(ExecutionError::Local("operator_review_required"));
    }
    Ok(transfer)
}

fn validate_sponsor_boundary(
    snapshot: &crate::billing::SponsoredSubscriptionSnapshot,
    transfer: &TransferIntent,
    _organization_id: &str,
    expected_subscription_id: &str,
) -> Result<(), ExecutionError> {
    if snapshot.subscription_id != expected_subscription_id {
        return Err(ExecutionError::Local("subscription_binding_changed"));
    }
    if snapshot.status != SubscriptionStatus::Active
        || snapshot.cancel_at_period_end
        || snapshot.cancel_at.is_some()
        || snapshot.current_period_end != Some(transfer.effective_from)
        || snapshot.customer_id.as_deref().is_none_or(str::is_empty)
    {
        return Err(ExecutionError::Local("subscription_term_changed"));
    }
    Ok(())
}

fn transfer_personal_operation_id(transfer_id: &str) -> String {
    format!("{transfer_id}:destination")
}

fn transfer_sponsored_operation_id(transfer_id: &str) -> String {
    format!("{transfer_id}:destination")
}

fn transfer_personal_restoration_operation_id(transfer_id: &str) -> String {
    format!("{transfer_id}:source-restoration")
}

fn personal_restoration_account_matches(
    account_operation: &str,
    account_subscription: Option<&str>,
    account_state: &str,
    account_customer: Option<&str>,
    operation_id: &str,
    customer_id: &str,
    expected_subscription_id: Option<&str>,
) -> bool {
    account_operation == operation_id
        && account_state == "pending"
        && account_customer == Some(customer_id)
        && match expected_subscription_id {
            Some(expected) => account_subscription == Some(expected),
            None => account_subscription.is_none(),
        }
}

fn restoration_create_retry_is_safe(now_epoch: i64, operation_created_at_epoch: i64) -> bool {
    now_epoch.saturating_sub(operation_created_at_epoch) < STRIPE_IDEMPOTENCY_SAFE_RETRY_SECONDS
}

fn transfer_allocation_reference(transfer_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(transfer_id.as_bytes());
    format!(
        "alloc_transfer_{}",
        digest[..16]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

fn classify_provider_failure(error: &ProviderError) -> TransferExecutionFailure {
    match error.kind {
        ProviderErrorKind::Retryable if error.status.is_none() => {
            TransferExecutionFailure::ProviderOutcomeUnknown
        }
        ProviderErrorKind::Retryable => TransferExecutionFailure::RetryableProvider,
        ProviderErrorKind::Authentication
        | ProviderErrorKind::ResourceMissing
        | ProviderErrorKind::Unknown => TransferExecutionFailure::ProviderRejected,
    }
}

fn epoch_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs() as i64)
        .unwrap_or(0)
}

fn sponsor_source_adjustment_is_before_boundary(now: i64, boundary: i64) -> bool {
    now < boundary
}

fn opaque_transfer_id(transfer_id: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(transfer_id.as_bytes())[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::billing::{SponsoredSubscriptionItem, SponsoredSubscriptionSnapshot};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn sponsor_source_adjustment_stops_at_the_boundary() {
        assert!(sponsor_source_adjustment_is_before_boundary(499, 500));
        assert!(!sponsor_source_adjustment_is_before_boundary(500, 500));
        assert!(!sponsor_source_adjustment_is_before_boundary(501, 500));
    }

    #[test]
    fn a_resumed_transfer_does_not_accept_a_mismatched_destination_invoice() {
        let mut transfer = transfer();
        transfer.destination_payment_reference = Some("pi_paid".into());
        transfer.destination_period_start = Some(transfer.effective_from + 1);
        transfer.destination_period_end = Some(transfer.effective_from + 31 * 86_400);
        transfer.destination_expected_period_end = Some(transfer.effective_from + 30 * 86_400);
        assert_eq!(
            billing_transfers::destination_period_mismatch_code(&transfer),
            Some("destination_period_start_mismatch")
        );

        transfer.destination_period_start = Some(transfer.effective_from);
        assert_eq!(
            billing_transfers::destination_period_mismatch_code(&transfer),
            Some("destination_period_end_mismatch")
        );

        transfer.destination_period_end = transfer.destination_expected_period_end;
        assert_eq!(
            billing_transfers::destination_period_mismatch_code(&transfer),
            None
        );
    }

    struct TimeoutThenFoundProvider {
        searches: AtomicUsize,
        creates: AtomicUsize,
        subscription: SponsoredSubscriptionSnapshot,
    }

    #[async_trait::async_trait]
    impl SubscriptionProvider for TimeoutThenFoundProvider {
        async fn create_checkout(
            &self,
            _org_id: &str,
            _customer: Option<&str>,
            _success_url: &str,
            _cancel_url: &str,
        ) -> crate::billing::ProviderResult<String> {
            unreachable!("transfer restoration does not create Checkout")
        }

        async fn create_portal(
            &self,
            _customer: &str,
            _return_url: &str,
            _configuration_id: Option<&str>,
        ) -> crate::billing::ProviderResult<String> {
            unreachable!("transfer restoration does not create a portal session")
        }

        async fn get_subscription(
            &self,
            _subscription_id: &str,
        ) -> crate::billing::ProviderResult<crate::billing::SubscriptionObservation> {
            unreachable!("transfer restoration uses the sponsored subscription reader")
        }

        async fn cancel_subscription(
            &self,
            _subscription_id: &str,
            _idempotency_key: &str,
            _org_id: &str,
        ) -> crate::billing::ProviderResult<crate::billing::SubscriptionObservation> {
            unreachable!("transfer restoration does not cancel a subscription")
        }

        async fn find_personal_source_restoration_subscription(
            &self,
            _operation_id: &str,
            _transfer_id: &str,
            _beneficiary_id: &str,
            _customer_id: &str,
            _payment_method_id: &str,
            _price_id: &str,
            _allocation_reference: &str,
        ) -> crate::billing::ProviderResult<Option<SponsoredSubscriptionSnapshot>> {
            if self.searches.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(None)
            } else {
                Ok(Some(self.subscription.clone()))
            }
        }

        async fn create_personal_source_restoration_subscription(
            &self,
            _transfer_id: &str,
            _beneficiary_id: &str,
            _customer_id: &str,
            _payment_method_id: &str,
            _offer: BillingOffer,
            _price_id: &str,
            _allocation_reference: &str,
            _operation_id: &str,
            _idempotency_key: &str,
        ) -> crate::billing::ProviderResult<SponsoredSubscriptionSnapshot> {
            self.creates.fetch_add(1, Ordering::SeqCst);
            Err(crate::billing::ProviderError {
                status: None,
                code: Some("transport_error".into()),
                kind: ProviderErrorKind::Retryable,
            })
        }
    }

    fn transfer() -> TransferIntent {
        TransferIntent {
            transfer_id: "transfer:test".into(),
            actor_user_id: "actor".into(),
            counterparty_user_id: "counterparty".into(),
            beneficiary_id: "beneficiary".into(),
            source_kind: TransferPayer::Sponsor,
            source_organization_id: Some("source-org".into()),
            destination_kind: TransferPayer::Sponsor,
            destination_organization_id: Some("destination-org".into()),
            offer: BillingOffer::StandardMonthly,
            quote_version: 1,
            quote_expires_at_epoch: 1_000,
            consent_expires_at_epoch: 1_000 + crate::billing_transfers::TRANSFER_CONSENT_SECONDS,
            request_hash: "hash".into(),
            idempotency_key: "key".into(),
            provider_idempotency_key: "provider-key".into(),
            effective_from: 500,
            effective_until: None,
            destination_operation_id: None,
            source_operation_id: None,
            destination_provider_subscription_id: Some("sub_destination".into()),
            source_provider_subscription_id: Some("sub_source".into()),
            destination_payment_reference: None,
            destination_period_start: None,
            destination_period_end: None,
            destination_expected_period_end: None,
            source_adjustment_reference: None,
            source_restoration_reference: None,
            source_restoration_operation_id: None,
            source_restoration_provider_subscription_id: None,
            source_restoration_payment_reference: None,
            founding_award_id: None,
            execution_lease_token: Some("lease".into()),
            execution_lease_until_epoch: Some(900),
            execution_attempt_count: 1,
            execution_manual_review_required: false,
            execution_review_generation: 0,
            destination_customer_id: None,
            destination_schedule_id: None,
            source_schedule_id: None,
            destination_setup_session_id: None,
            destination_setup_intent_id: None,
            last_execution_error_code: None,
            state: TransferState::Pending,
            result_code: None,
        }
    }

    fn snapshot(subscription_id: &str) -> SponsoredSubscriptionSnapshot {
        SponsoredSubscriptionSnapshot {
            customer_id: Some("cus_test".into()),
            default_payment_method_id: None,
            subscription_id: subscription_id.into(),
            status: SubscriptionStatus::Active,
            cancel_at_period_end: false,
            cancel_at: None,
            current_period_start: Some(200),
            current_period_end: Some(500),
            schedule_id: Some("sched_test".into()),
            items: vec![SponsoredSubscriptionItem {
                provider_item_id: "si_test".into(),
                provider_price_id: "price_test".into(),
                quantity: 1,
            }],
        }
    }

    #[test]
    fn sponsor_snapshot_must_match_the_requested_side_subscription() {
        let transfer = transfer();
        assert!(validate_sponsor_boundary(
            &snapshot("sub_destination"),
            &transfer,
            "destination-org",
            "sub_destination",
        )
        .is_ok());
        assert!(validate_sponsor_boundary(
            &snapshot("sub_source"),
            &transfer,
            "destination-org",
            "sub_destination",
        )
        .is_err());
    }

    #[test]
    fn sponsor_snapshot_rejects_changed_period_and_cancellation() {
        let transfer = transfer();
        let mut changed_period = snapshot("sub_destination");
        changed_period.current_period_end = Some(501);
        assert!(validate_sponsor_boundary(
            &changed_period,
            &transfer,
            "destination-org",
            "sub_destination",
        )
        .is_err());

        let mut cancellation = snapshot("sub_destination");
        cancellation.cancel_at_period_end = true;
        assert!(validate_sponsor_boundary(
            &cancellation,
            &transfer,
            "destination-org",
            "sub_destination",
        )
        .is_err());
    }

    #[test]
    fn sponsor_source_end_requires_the_exact_provider_phase_quantities() {
        let phase = crate::billing::SponsoredSubscriptionPhase {
            start_at_epoch: 500,
            end_at_epoch: None,
            items: vec![
                crate::billing::SponsoredSubscriptionItemRequest {
                    provider_price_id: "price_monthly".into(),
                    quantity: 2,
                },
                crate::billing::SponsoredSubscriptionItemRequest {
                    provider_price_id: "price_annual".into(),
                    quantity: 1,
                },
            ],
        };
        let mut actual = snapshot("sub_source");
        actual.current_period_start = Some(500);
        actual.items = vec![
            SponsoredSubscriptionItem {
                provider_item_id: "si_monthly_a".into(),
                provider_price_id: "price_monthly".into(),
                quantity: 1,
            },
            SponsoredSubscriptionItem {
                provider_item_id: "si_monthly_b".into(),
                provider_price_id: "price_monthly".into(),
                quantity: 1,
            },
            SponsoredSubscriptionItem {
                provider_item_id: "si_annual".into(),
                provider_price_id: "price_annual".into(),
                quantity: 1,
            },
        ];
        assert!(sponsored_phase_matches_snapshot(&phase, &actual));

        actual.items[0].quantity = 2;
        assert!(
            !sponsored_phase_matches_snapshot(&phase, &actual),
            "a provider quantity mismatch must keep source retirement fenced"
        );
        actual.items[0].quantity = 1;
        actual.items.push(SponsoredSubscriptionItem {
            provider_item_id: "si_unexpected".into(),
            provider_price_id: "price_unexpected".into(),
            quantity: 1,
        });
        assert!(!sponsored_phase_matches_snapshot(&phase, &actual));
    }

    #[test]
    fn personal_source_retirement_requires_cancellation_at_the_quoted_boundary() {
        let mut current = snapshot("sub_source");
        current.status = SubscriptionStatus::Canceled;
        assert!(personal_source_end_matches(&current, 500));
        assert!(!personal_source_end_matches(&current, 501));
        current.status = SubscriptionStatus::Active;
        assert!(!personal_source_end_matches(&current, 500));
    }

    #[test]
    fn personal_source_adjustment_retry_accepts_the_recorded_boundary_cancellation() {
        let mut current = snapshot("sub_source");
        assert!(personal_source_adjustment_preflight_matches(
            &current,
            "sub_source",
            500
        ));
        assert!(!personal_source_adjustment_is_applied(&current, 500));

        current.cancel_at_period_end = true;
        assert!(personal_source_adjustment_is_applied(&current, 500));
        assert!(personal_source_adjustment_preflight_matches(
            &current,
            "sub_source",
            500
        ));
        current.cancel_at_period_end = false;
        current.cancel_at = Some(500);
        assert!(personal_source_adjustment_is_applied(&current, 500));
        assert!(personal_source_adjustment_preflight_matches(
            &current,
            "sub_source",
            500
        ));

        current.status = SubscriptionStatus::Canceled;
        assert!(personal_source_adjustment_is_applied(&current, 500));
        assert!(personal_source_adjustment_retry_allowed(501, 500, true));
        assert!(personal_source_adjustment_preflight_matches(
            &current,
            "sub_source",
            500
        ));
        assert!(!personal_source_adjustment_preflight_matches(
            &current,
            "sub_source",
            501
        ));
        assert!(personal_source_adjustment_retry_allowed(499, 500, false));
        assert!(!personal_source_adjustment_retry_allowed(500, 500, false));
        assert!(!personal_source_adjustment_preflight_matches(
            &current,
            "other_subscription",
            500
        ));
        current.cancel_at = Some(499);
        assert!(!personal_source_adjustment_preflight_matches(
            &current,
            "sub_source",
            500
        ));
    }

    #[test]
    fn personal_restoration_retries_only_the_same_pending_operation_binding() {
        assert!(personal_restoration_account_matches(
            "transfer:one:source-restoration",
            None,
            "pending",
            Some("cus_original"),
            "transfer:one:source-restoration",
            "cus_original",
            None,
        ));
        assert!(personal_restoration_account_matches(
            "transfer:one:source-restoration",
            Some("sub_restored"),
            "pending",
            Some("cus_original"),
            "transfer:one:source-restoration",
            "cus_original",
            Some("sub_restored"),
        ));
        assert!(!personal_restoration_account_matches(
            "transfer:other:source-restoration",
            None,
            "pending",
            Some("cus_original"),
            "transfer:one:source-restoration",
            "cus_original",
            None,
        ));
        assert!(!personal_restoration_account_matches(
            "transfer:one:source-restoration",
            None,
            "pending",
            Some("cus_other"),
            "transfer:one:source-restoration",
            "cus_original",
            None,
        ));
        assert!(!personal_restoration_account_matches(
            "transfer:one:source-restoration",
            None,
            "active",
            Some("cus_original"),
            "transfer:one:source-restoration",
            "cus_original",
            None,
        ));
    }

    #[test]
    fn restoration_replays_stable_create_only_inside_stripes_idempotency_window() {
        assert!(restoration_create_retry_is_safe(1_000, 1_000));
        assert!(restoration_create_retry_is_safe(1_000 + 82_799, 1_000));
        assert!(!restoration_create_retry_is_safe(1_000 + 82_800, 1_000));
        assert!(restoration_create_retry_is_safe(900, 1_000));
    }

    #[tokio::test]
    async fn restoration_timeout_is_recovered_by_metadata_lookup_without_second_create() {
        let mut restored = snapshot("sub_restored");
        restored.customer_id = Some("cus_original".into());
        restored.default_payment_method_id = Some("pm_saved".into());
        restored.status = SubscriptionStatus::Incomplete;
        restored.items[0].provider_price_id = "price_standard_monthly".into();
        let provider = TimeoutThenFoundProvider {
            searches: AtomicUsize::new(0),
            creates: AtomicUsize::new(0),
            subscription: restored,
        };
        let transfer = transfer();
        let first = resolve_personal_source_restoration_subscription(
            &provider,
            &transfer,
            "cus_original",
            "pm_saved",
            "price_standard_monthly",
            "alloc_restore",
            "transfer:test:source-restoration",
            1_000,
            1_000,
        )
        .await;
        assert!(matches!(first, Err(ExecutionError::Provider(_))));

        let retried = resolve_personal_source_restoration_subscription(
            &provider,
            &transfer,
            "cus_original",
            "pm_saved",
            "price_standard_monthly",
            "alloc_restore",
            "transfer:test:source-restoration",
            1_000,
            1_001,
        )
        .await
        .expect("retry locates the subscription created before the timeout");
        assert_eq!(retried.subscription_id, "sub_restored");
        assert_eq!(provider.searches.load(Ordering::SeqCst), 2);
        assert_eq!(provider.creates.load(Ordering::SeqCst), 1);
    }
}
