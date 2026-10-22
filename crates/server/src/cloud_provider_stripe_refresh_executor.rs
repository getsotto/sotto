//! Shared worker executor for a beneficiary whose registered history may mix personal and
//! sponsored Stripe sources.
//!
//! A ticket is prepared once for the beneficiary's complete source set. Each provider adapter
//! reads only the sources it owns, and the resulting observations are merged before one atomic
//! reconciliation completion.

use std::collections::BTreeMap;

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::cloud_coverage_reconciliation::{
    begin_collection, finish_collection_with_authority, AuthorityReceiptInput, CollectionTicket,
    SourceBinding,
};
use crate::cloud_provider::{
    collect_provider_history, prepare_verified_event, AllocationState, PayerKind, ProviderContext,
    VerifiedAllocation, VerifiedCollection,
};
use crate::cloud_provider_refresh_inputs::RefreshJobInputs;
use crate::cloud_provider_refresh_worker::{RefreshExecutionError, RefreshJobExecutor};
use crate::cloud_provider_stripe::StripeCoverageConfig;
use crate::cloud_provider_stripe_adapter::{
    prepared_source, StripeHistoryClient, StripePreparedCollection,
};
use crate::cloud_provider_stripe_http::StripeReadClient;
use crate::cloud_provider_stripe_sponsored::SponsoredStripeCoverageConfig;
use crate::cloud_provider_stripe_sponsored_refresh::{
    collect_sponsored_collection, SponsoredRefreshLimits,
};

#[derive(Clone)]
pub struct StripeRefreshExecutor {
    pool: PgPool,
    stripe: StripeReadClient,
    personal_coverage: StripeCoverageConfig,
    sponsored_coverage: SponsoredStripeCoverageConfig,
    sponsored_limits: SponsoredRefreshLimits,
    authority_max_age_seconds: Option<i64>,
}

impl StripeRefreshExecutor {
    pub fn new(
        pool: PgPool,
        stripe: StripeReadClient,
        personal_coverage: StripeCoverageConfig,
        sponsored_coverage: SponsoredStripeCoverageConfig,
        sponsored_limits: SponsoredRefreshLimits,
    ) -> Self {
        Self {
            pool,
            stripe,
            personal_coverage,
            sponsored_coverage,
            sponsored_limits,
            authority_max_age_seconds: None,
        }
    }

    pub fn with_authority_max_age(mut self, seconds: Option<i64>) -> Self {
        self.authority_max_age_seconds = seconds;
        self
    }

    pub(crate) fn authority_refresh_interval_seconds(&self) -> i64 {
        self.authority_max_age_seconds
            .unwrap_or(60 * 60)
            .saturating_mul(3)
            / 4
    }

    async fn execute_inner(&self, inputs: &RefreshJobInputs) -> Result<(), &'static str> {
        let context = &inputs.lease.context;
        if !refresh_input_matches_stripe(
            context,
            &inputs.event.event_type,
            &self.personal_coverage,
            &self.sponsored_coverage,
        ) {
            return Err("refresh_input_invalid");
        }

        let run_id = format!("stripe-refresh:{}", Uuid::new_v4());
        let mut prepare = self.pool.begin().await.map_err(|_| "refresh_database")?;
        let preparation = match prepare_verified_event(
            &mut prepare,
            context,
            &inputs.event,
            &inputs.allocation,
            &run_id,
        )
        .await
        {
            Ok(preparation) => preparation,
            Err(_) => {
                prepare.rollback().await.map_err(|_| "refresh_database")?;
                return Err("refresh_prepare");
            }
        };
        prepare.commit().await.map_err(|_| "refresh_database")?;

        // Resolve provider ownership from immutable local bindings after capturing the ticket.
        // These reads finish before any Stripe request; no transaction or connection spans I/O.
        let details = load_source_details(&self.pool, context, &preparation.ticket).await?;
        let collection = self
            .collect_mixed(context, Some(&inputs.event), &preparation.ticket, &details)
            .await?;
        let collection = apply_confirmed_early_terminations(collection, &details)?;

        let mut complete = self.pool.begin().await.map_err(|_| "refresh_database")?;
        let now_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| "refresh_clock")?
            .as_secs() as i64;
        let authority = self.authority_max_age_seconds.map(|seconds| {
            crate::cloud_coverage_reconciliation::AuthorityReceiptInput {
                provider_namespace: context.namespace.clone(),
                provider_account_id: context.account_id.clone(),
                provider_environment: context.environment.as_str().to_owned(),
                observed_at_epoch: now_epoch,
                fresh_until_epoch: now_epoch.saturating_add(seconds),
                policy_version: crate::cloud_coverage_authority::POLICY_VERSION,
                local_correction_digest: local_correction_digest(&details),
            }
        });
        let result = crate::cloud_provider::complete_verified_event_with_authority(
            &mut complete,
            context,
            &inputs.event,
            &inputs.allocation,
            &preparation,
            &collection,
            authority.as_ref(),
        )
        .await;
        match result {
            Ok(_) => complete.commit().await.map_err(|_| "refresh_database"),
            Err(_) => {
                complete.rollback().await.map_err(|_| "refresh_database")?;
                Err("refresh_complete")
            }
        }
    }

    async fn collect_mixed(
        &self,
        context: &ProviderContext,
        event: Option<&crate::cloud_provider::VerifiedProviderEvent>,
        ticket: &CollectionTicket,
        details: &[SourceDetails],
    ) -> Result<VerifiedCollection, &'static str> {
        if details.len() != ticket.source_bindings.len() || details.is_empty() {
            return Err("refresh_source_set");
        }
        let mut observations = BTreeMap::new();
        let mut evidence_references = Vec::new();

        let personal = details
            .iter()
            .filter(|detail| detail.allocation.payer_kind == PayerKind::Personal)
            .collect::<Vec<_>>();
        if !personal.is_empty() {
            let personal_ticket = filtered_ticket(
                ticket,
                personal
                    .iter()
                    .map(|detail| detail.binding.clone())
                    .collect(),
            );
            let sources = personal
                .iter()
                .map(|detail| {
                    let allocation = crate::cloud_provider_stripe::StripeAllocationBinding::new(
                        detail.allocation.external_allocation_reference.clone(),
                        detail.allocation.provider_customer_id.clone(),
                        detail.allocation.subscription_id.clone(),
                        detail.allocation.provider_item_id.clone(),
                        PayerKind::Personal,
                    )
                    .map_err(|_| "refresh_personal_binding")?;
                    Ok(prepared_source(
                        detail.binding.clone(),
                        allocation,
                        detail.source_generation,
                        detail.allocation.ownership_evidence_reference.clone(),
                    ))
                })
                .collect::<Result<Vec<_>, &'static str>>()?;
            let prepared = StripePreparedCollection::from_ticket(
                &self.personal_coverage,
                &personal_ticket,
                sources,
            )
            .map_err(|_| "refresh_personal_binding")?;
            let mut renewal_failures = Vec::new();
            for detail in &personal {
                let failures =
                    crate::cloud_provider_stripe_renewal_store::load_personal_renewal_failures(
                        &self.pool,
                        context,
                        &detail.allocation,
                        Default::default(),
                    )
                    .await
                    .map_err(|_| "refresh_personal_renewals")?;
                renewal_failures.extend(failures);
            }
            let mut client = StripeHistoryClient::new(self.stripe.clone(), prepared)
                .with_renewal_failures(renewal_failures);
            let collection = collect_provider_history(
                &mut client,
                context,
                &personal_ticket.source_bindings,
                Default::default(),
            )
            .await
            .map_err(|_| "refresh_personal_evidence")?;
            add_observations(&mut observations, &mut evidence_references, collection)?;
        }

        let mut sponsored_groups = BTreeMap::<(String, String), Vec<&SourceDetails>>::new();
        for detail in details
            .iter()
            .filter(|detail| detail.allocation.payer_kind == PayerKind::Sponsor)
        {
            sponsored_groups
                .entry((
                    detail.allocation.subscription_id.clone(),
                    detail.allocation.provider_customer_id.clone(),
                ))
                .or_default()
                .push(detail);
        }
        for group in sponsored_groups.values() {
            let sponsor_ticket = filtered_ticket(
                ticket,
                group.iter().map(|detail| detail.binding.clone()).collect(),
            );
            let anchor = &group[0].allocation;
            let mut renewal_failures = Vec::new();
            for detail in group {
                renewal_failures.extend(
                    crate::cloud_provider_stripe_renewal_store::load_renewal_failures(
                        &self.pool,
                        context,
                        &detail.allocation,
                        Default::default(),
                    )
                    .await
                    .map_err(|_| "refresh_sponsored_renewals")?,
                );
            }
            let collection = if event.is_some_and(|event| use_triggering_sponsored_invoice(
                &event.event_type,
                event.subscription_id.as_deref(),
                &anchor.subscription_id,
            )) {
                collect_sponsored_collection(
                    &self.pool,
                    &self.stripe,
                    &self.sponsored_coverage,
                    context,
                    event.expect("the triggering-invoice predicate requires an event"),
                    anchor,
                    &sponsor_ticket,
                    self.sponsored_limits,
                    &renewal_failures,
                )
                .await
            } else {
                crate::cloud_provider_stripe_sponsored_refresh::collect_sponsored_subscription_collection(
                    &self.pool,
                    &self.stripe,
                    &self.sponsored_coverage,
                    context,
                    anchor,
                    &sponsor_ticket,
                    self.sponsored_limits,
                    &renewal_failures,
                )
                .await
            }
            .map_err(|_| "refresh_sponsored_evidence")?;
            add_observations(&mut observations, &mut evidence_references, collection)?;
        }

        if observations.len() != ticket.source_bindings.len() {
            return Err("refresh_source_set");
        }
        let evidence_material = evidence_references.join("\n");
        let digest = Sha256::digest(evidence_material.as_bytes());
        let digest = digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        Ok(VerifiedCollection {
            aggregate_evidence_reference: format!("stripe-mixed-collection-v1:{digest}"),
            observations: ticket
                .source_bindings
                .iter()
                .map(|binding| {
                    observations
                        .remove(&binding.source_id)
                        .ok_or("refresh_source_set")
                })
                .collect::<Result<Vec<_>, _>>()?,
        })
    }

    pub async fn revalidate_beneficiary(&self, beneficiary_id: &str) -> Result<(), &'static str> {
        let context = self
            .personal_coverage
            .provider_context()
            .map_err(|_| "revalidation_context")?;
        let attempt_id = format!("scheduled-revalidation:{}", Uuid::new_v4());
        let mut prepare = self
            .pool
            .begin()
            .await
            .map_err(|_| "revalidation_database")?;
        let ticket = begin_collection(&mut prepare, beneficiary_id, &attempt_id)
            .await
            .map_err(|_| "revalidation_prepare")?;
        if ticket.source_bindings.is_empty() {
            prepare
                .rollback()
                .await
                .map_err(|_| "revalidation_database")?;
            return Err("revalidation_source_set");
        }
        prepare
            .commit()
            .await
            .map_err(|_| "revalidation_database")?;
        let details = load_source_details(&self.pool, &context, &ticket).await?;
        let collection = self
            .collect_mixed(&context, None, &ticket, &details)
            .await?;
        let collection = apply_confirmed_early_terminations(collection, &details)?;
        let observed_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| "revalidation_clock")?
            .as_secs() as i64;
        let fresh_until = observed_at
            .checked_add(
                self.authority_max_age_seconds
                    .ok_or("revalidation_freshness")?,
            )
            .ok_or("revalidation_freshness")?;
        let authority = AuthorityReceiptInput {
            provider_namespace: context.namespace.clone(),
            provider_account_id: context.account_id.clone(),
            provider_environment: context.environment.as_str().to_owned(),
            observed_at_epoch: observed_at,
            fresh_until_epoch: fresh_until,
            policy_version: crate::cloud_coverage_authority::POLICY_VERSION,
            local_correction_digest: local_correction_digest(&details),
        };
        let mut complete = self
            .pool
            .begin()
            .await
            .map_err(|_| "revalidation_database")?;
        let receipt = finish_collection_with_authority(
            &mut complete,
            &ticket,
            &collection.aggregate_evidence_reference,
            &collection.observations,
            Some(&authority),
        )
        .await
        .map_err(|_| "revalidation_complete")?;
        if receipt.revision <= 0 {
            complete
                .rollback()
                .await
                .map_err(|_| "revalidation_database")?;
            return Err("revalidation_complete");
        }
        complete.commit().await.map_err(|_| "revalidation_database")
    }
}

fn use_triggering_sponsored_invoice(
    event_type: &str,
    event_subscription_id: Option<&str>,
    allocation_subscription_id: &str,
) -> bool {
    event_type == "invoice.paid" && event_subscription_id == Some(allocation_subscription_id)
}

fn refresh_input_matches_stripe(
    context: &ProviderContext,
    event_type: &str,
    personal_coverage: &StripeCoverageConfig,
    sponsored_coverage: &SponsoredStripeCoverageConfig,
) -> bool {
    matches!(event_type, "invoice.paid" | "invoice.payment_failed")
        && context.namespace == crate::cloud_provider_stripe::STRIPE_NAMESPACE
        && context.account_id == personal_coverage.account_id
        && context.account_id == sponsored_coverage.account_id()
        && context.environment == personal_coverage.environment
        && context.environment == sponsored_coverage.environment()
}

#[async_trait]
impl RefreshJobExecutor for StripeRefreshExecutor {
    async fn execute(&mut self, inputs: &RefreshJobInputs) -> Result<(), RefreshExecutionError> {
        self.execute_inner(inputs).await.map_err(|code| {
            RefreshExecutionError::new(code).expect("static refresh error codes are nonempty")
        })
    }
}

pub fn spawn(
    pool: PgPool,
    stripe: StripeReadClient,
    personal_coverage: StripeCoverageConfig,
    sponsored_coverage: SponsoredStripeCoverageConfig,
    authority_max_age_seconds: Option<i64>,
) {
    tokio::spawn(async move {
        let worker_id = format!("stripe-refresh:{}", Uuid::new_v4());
        let mut executor = StripeRefreshExecutor::new(
            pool.clone(),
            stripe,
            personal_coverage,
            sponsored_coverage,
            SponsoredRefreshLimits::default(),
        )
        .with_authority_max_age(authority_max_age_seconds);
        loop {
            match crate::cloud_provider_refresh_worker::run_once(&pool, &worker_id, &mut executor)
                .await
            {
                Ok(crate::cloud_provider_refresh_worker::RefreshWorkerOutcome::Idle) => {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
                Ok(_) => {}
                Err(_) => {
                    eprintln!("warning: Stripe coverage refresh worker iteration failed");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            }
        }
    });
}

pub fn spawn_revalidation(
    pool: PgPool,
    stripe: StripeReadClient,
    personal_coverage: StripeCoverageConfig,
    sponsored_coverage: SponsoredStripeCoverageConfig,
    max_age_seconds: i64,
) {
    tokio::spawn(async move {
        let worker_id = format!("stripe-authority:{}", Uuid::new_v4());
        let mut executor = StripeRefreshExecutor::new(
            pool.clone(),
            stripe,
            personal_coverage,
            sponsored_coverage,
            SponsoredRefreshLimits::default(),
        )
        .with_authority_max_age(Some(max_age_seconds));
        loop {
            match crate::cloud_coverage_revalidation::run_once(&pool, &worker_id, &mut executor)
                .await
            {
                Ok(true) => {}
                Ok(false) => tokio::time::sleep(std::time::Duration::from_secs(2)).await,
                Err(_) => {
                    eprintln!("warning: Cloud authority revalidation iteration failed");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            }
        }
    });
}

struct SourceDetails {
    binding: SourceBinding,
    allocation: VerifiedAllocation,
    source_generation: i64,
    confirmed_early_termination_epoch: Option<i64>,
}

async fn load_source_details(
    pool: &PgPool,
    context: &ProviderContext,
    ticket: &CollectionTicket,
) -> Result<Vec<SourceDetails>, &'static str> {
    let mut tx = pool.begin().await.map_err(|_| "refresh_database")?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *tx)
        .await
        .map_err(|_| "refresh_database")?;
    let mut details = Vec::with_capacity(ticket.source_bindings.len());
    for binding in &ticket.source_bindings {
        let row = sqlx::query(
            "SELECT allocation.allocation_id, allocation.payer_id, payer.provider_customer_id, \
                    payer.payer_kind, allocation.provider_subscription_id, allocation.provider_item_id, \
                    allocation.external_allocation_reference, allocation.effective_from, \
                    allocation.effective_until, allocation.state, allocation.ownership_evidence_reference, \
                    source.registration_source_set_generation, termination.effective_at_epoch \
             FROM cloud_provider_allocations AS allocation \
             JOIN cloud_provider_payers AS payer ON payer.payer_id = allocation.payer_id \
             JOIN cloud_coverage_sources AS source ON source.source_id = allocation.coverage_source_id \
             LEFT JOIN LATERAL ( \
                 SELECT MIN(request.effective_at_epoch) AS effective_at_epoch \
                 FROM billing_correction_requests AS request \
                 WHERE request.beneficiary_id = allocation.beneficiary_id \
                   AND payer.payer_kind = 'personal' \
                   AND request.payer_kind = 'personal' \
                   AND request.subscription_id = allocation.provider_subscription_id \
                   AND request.state = 'refunded' AND request.full_refund_requested \
                   AND NOT request.preserve_paid_term \
                   AND request.early_termination_confirmed_at_epoch IS NOT NULL \
                   AND request.effective_at_epoch IS NOT NULL \
             ) AS termination ON TRUE \
             WHERE allocation.coverage_source_id = $1 AND allocation.beneficiary_id = $2 \
               AND allocation.provider_namespace = $3 AND allocation.provider_account_id = $4 \
               AND allocation.provider_environment = $5",
        )
        .bind(&binding.source_id)
        .bind(&binding.beneficiary_id)
        .bind(&context.namespace)
        .bind(&context.account_id)
        .bind(context.environment.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(|_| "refresh_database")?
        .ok_or("refresh_source_missing")?;
        let payer_kind = match row
            .try_get::<String, _>("payer_kind")
            .map_err(|_| "refresh_source_corrupt")?
            .as_str()
        {
            "personal" => PayerKind::Personal,
            "sponsor" => PayerKind::Sponsor,
            _ => return Err("refresh_source_corrupt"),
        };
        let state = match row
            .try_get::<String, _>("state")
            .map_err(|_| "refresh_source_corrupt")?
            .as_str()
        {
            "pending" => AllocationState::Pending,
            "active" => AllocationState::Active,
            "ended" => AllocationState::Ended,
            _ => return Err("refresh_source_corrupt"),
        };
        let external_reference: String = row
            .try_get("external_allocation_reference")
            .map_err(|_| "refresh_source_corrupt")?;
        if external_reference != binding.external_allocation_reference {
            return Err("refresh_source_corrupt");
        }
        let allocation = VerifiedAllocation::new(
            row.try_get::<String, _>("allocation_id")
                .map_err(|_| "refresh_source_corrupt")?,
            row.try_get::<String, _>("payer_id")
                .map_err(|_| "refresh_source_corrupt")?,
            row.try_get::<String, _>("provider_customer_id")
                .map_err(|_| "refresh_source_corrupt")?,
            payer_kind,
            ticket.beneficiary_id.clone(),
            row.try_get::<String, _>("provider_subscription_id")
                .map_err(|_| "refresh_source_corrupt")?,
            row.try_get::<String, _>("provider_item_id")
                .map_err(|_| "refresh_source_corrupt")?,
            external_reference,
            binding.source_id.clone(),
            row.try_get("effective_from")
                .map_err(|_| "refresh_source_corrupt")?,
            row.try_get("effective_until")
                .map_err(|_| "refresh_source_corrupt")?,
            state,
            row.try_get::<String, _>("ownership_evidence_reference")
                .map_err(|_| "refresh_source_corrupt")?,
        )
        .map_err(|_| "refresh_source_corrupt")?;
        details.push(SourceDetails {
            binding: binding.clone(),
            allocation,
            source_generation: row
                .try_get("registration_source_set_generation")
                .map_err(|_| "refresh_source_corrupt")?,
            confirmed_early_termination_epoch: row
                .try_get("effective_at_epoch")
                .map_err(|_| "refresh_source_corrupt")?,
        });
    }
    tx.commit().await.map_err(|_| "refresh_database")?;
    Ok(details)
}

fn apply_confirmed_early_terminations(
    mut collection: VerifiedCollection,
    details: &[SourceDetails],
) -> Result<VerifiedCollection, &'static str> {
    let cutoffs = details
        .iter()
        .filter_map(|detail| {
            detail
                .confirmed_early_termination_epoch
                .map(|epoch| (detail.binding.source_id.as_str(), epoch))
        })
        .collect::<BTreeMap<_, _>>();
    if cutoffs.is_empty() {
        return Ok(collection);
    }
    let mut material = collection.aggregate_evidence_reference.clone();
    for observation in &mut collection.observations {
        if let crate::cloud_coverage_reconciliation::SourceObservation::Complete {
            source_id,
            evidence_reference,
            paid_intervals,
        } = observation
        {
            if let Some(cutoff) = cutoffs.get(source_id.as_str()).copied() {
                material.push_str(&format!(
                    "\ntermination:{source_id}:{cutoff}:{evidence_reference}"
                ));
                for interval in paid_intervals.iter_mut() {
                    if interval.starts_at < cutoff {
                        interval.paid_until = interval.paid_until.min(cutoff);
                        interval.failed_renewal_id = None;
                    }
                }
                paid_intervals.retain(|interval| {
                    interval.starts_at < cutoff && interval.paid_until > interval.starts_at
                });
            }
        }
    }
    let digest = Sha256::digest(material.as_bytes());
    collection.aggregate_evidence_reference = format!(
        "stripe-mixed-correction-v1:{}",
        digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    Ok(collection)
}

fn local_correction_digest(details: &[SourceDetails]) -> String {
    let mut rows = details
        .iter()
        .map(|detail| {
            (
                detail.binding.source_id.as_str(),
                detail.confirmed_early_termination_epoch,
            )
        })
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| left.0.cmp(right.0));
    let mut material = String::new();
    for (source_id, effective_at) in rows {
        material.push_str(&format!("{}:{}\n", source_id.len(), source_id));
        match effective_at {
            Some(epoch) => material.push_str(&format!("{epoch}\n")),
            None => material.push_str("none\n"),
        }
    }
    let digest = Sha256::digest(material.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn filtered_ticket(
    ticket: &CollectionTicket,
    source_bindings: Vec<SourceBinding>,
) -> CollectionTicket {
    let mut filtered = ticket.clone();
    filtered.source_bindings = source_bindings;
    filtered
}

fn add_observations(
    observations: &mut BTreeMap<String, crate::cloud_coverage_reconciliation::SourceObservation>,
    evidence_references: &mut Vec<String>,
    collection: VerifiedCollection,
) -> Result<(), &'static str> {
    evidence_references.push(collection.aggregate_evidence_reference);
    for observation in collection.observations {
        let source_id = match &observation {
            crate::cloud_coverage_reconciliation::SourceObservation::Complete {
                source_id, ..
            }
            | crate::cloud_coverage_reconciliation::SourceObservation::Unavailable {
                source_id,
                ..
            } => source_id.clone(),
        };
        if observations.insert(source_id, observation).is_some() {
            return Err("refresh_duplicate_source");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud_coverage::ConfirmedPaidInterval;
    use crate::cloud_coverage_reconciliation::SourceObservation;

    fn detail(source_id: &str, termination: Option<i64>) -> SourceDetails {
        let binding = SourceBinding {
            beneficiary_id: "beneficiary".into(),
            source_id: source_id.into(),
            provider_namespace: "stripe".into(),
            external_allocation_reference: format!("external-{source_id}"),
            ownership_evidence_reference: format!("ownership-{source_id}"),
        };
        let allocation = VerifiedAllocation::new(
            format!("allocation-{source_id}"),
            format!("payer-{source_id}"),
            format!("customer-{source_id}"),
            PayerKind::Personal,
            "beneficiary",
            format!("subscription-{source_id}"),
            "price",
            &binding.external_allocation_reference,
            source_id,
            0,
            None,
            AllocationState::Active,
            &binding.ownership_evidence_reference,
        )
        .unwrap();
        SourceDetails {
            binding,
            allocation,
            source_generation: 1,
            confirmed_early_termination_epoch: termination,
        }
    }

    #[test]
    fn confirmed_full_refund_clips_only_its_personal_source() {
        let collection = VerifiedCollection {
            aggregate_evidence_reference: "stripe-mixed-collection-v1:original".into(),
            observations: vec![
                SourceObservation::Complete {
                    source_id: "personal".into(),
                    evidence_reference: "personal-evidence".into(),
                    paid_intervals: vec![
                        ConfirmedPaidInterval {
                            coverage_id: "before".into(),
                            source_id: "personal".into(),
                            starts_at: 10,
                            paid_until: 30,
                            failed_renewal_id: None,
                        },
                        ConfirmedPaidInterval {
                            coverage_id: "after".into(),
                            source_id: "personal".into(),
                            starts_at: 30,
                            paid_until: 50,
                            failed_renewal_id: None,
                        },
                    ],
                },
                SourceObservation::Complete {
                    source_id: "sponsored".into(),
                    evidence_reference: "sponsored-evidence".into(),
                    paid_intervals: vec![ConfirmedPaidInterval {
                        coverage_id: "sponsor-term".into(),
                        source_id: "sponsored".into(),
                        starts_at: 10,
                        paid_until: 50,
                        failed_renewal_id: None,
                    }],
                },
            ],
        };
        let adjusted = apply_confirmed_early_terminations(
            collection,
            &[detail("personal", Some(25)), detail("sponsored", None)],
        )
        .unwrap();
        assert!(adjusted
            .aggregate_evidence_reference
            .starts_with("stripe-mixed-correction-v1:"));
        let SourceObservation::Complete { paid_intervals, .. } = &adjusted.observations[0] else {
            panic!("personal source remains complete")
        };
        assert_eq!(paid_intervals.len(), 1);
        assert_eq!(paid_intervals[0].paid_until, 25);
        let SourceObservation::Complete { paid_intervals, .. } = &adjusted.observations[1] else {
            panic!("sponsored source remains complete")
        };
        assert_eq!(paid_intervals[0].paid_until, 50);
    }
}

#[cfg(test)]
mod termination_tests {
    use super::*;

    fn personal_config() -> StripeCoverageConfig {
        StripeCoverageConfig::new(
            "acct_test",
            crate::cloud_provider::ProviderEnvironment::Test,
            "price_month",
            "price_year",
        )
        .unwrap()
    }

    fn sponsored_config() -> SponsoredStripeCoverageConfig {
        SponsoredStripeCoverageConfig::new(
            "acct_test",
            crate::cloud_provider::ProviderEnvironment::Test,
            "price_month",
            "price_year",
            "price_found_month",
            "price_found_year",
        )
        .unwrap()
    }

    #[test]
    fn refresh_input_rejects_other_provider_namespaces_before_preparation() {
        let context = ProviderContext::new(
            "other-provider",
            "acct_test",
            crate::cloud_provider::ProviderEnvironment::Test,
        )
        .unwrap();

        assert!(!refresh_input_matches_stripe(
            &context,
            "invoice.paid",
            &personal_config(),
            &sponsored_config(),
        ));
    }

    #[test]
    fn only_paid_events_use_the_triggering_sponsored_invoice() {
        assert!(use_triggering_sponsored_invoice(
            "invoice.paid",
            Some("sub_sponsor"),
            "sub_sponsor"
        ));
        assert!(!use_triggering_sponsored_invoice(
            "invoice.payment_failed",
            Some("sub_sponsor"),
            "sub_sponsor"
        ));
        assert!(!use_triggering_sponsored_invoice(
            "invoice.paid",
            Some("sub_other"),
            "sub_sponsor"
        ));
    }
}
