//! Sponsored Stripe refresh execution for durable provider jobs.
//!
//! This module is the provider-specific executor behind the generic refresh queue. It deliberately
//! remains dormant until the runtime wires it into the worker: the implementation is complete and
//! testable, but enabling it is a separate rollout decision. A sponsored beneficiary can have
//! sources represented by several invoices, so collection reads a bounded recent subscription
//! prefix, selects the newest valid invoice candidate for each source, and only then crosses the
//! reconciliation adapter.

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use sqlx::PgPool;
use thiserror::Error;
use uuid::Uuid;

use crate::cloud_provider::{
    complete_verified_event, prepare_verified_event, ApplyReceipt, ProviderAdapterError,
    ProviderContext, VerifiedAllocation, VerifiedCollection, VerifiedProviderEvent,
};
use crate::cloud_provider_refresh_inputs::RefreshJobInputs;
use crate::cloud_provider_refresh_worker::{RefreshExecutionError, RefreshJobExecutor};
use crate::cloud_provider_stripe_http::{StripeReadClient, StripeReadError};
use crate::cloud_provider_stripe_sponsored::{
    compose_sponsored_coverage, SponsoredCoverageError, SponsoredCoverageResult,
    SponsoredNeedsEvidence, SponsoredStripeCoverageConfig,
};
use crate::cloud_provider_stripe_sponsored_adapter::collection_for_ticket_by_source;
use crate::cloud_provider_stripe_sponsored_store::{
    load_sponsored_allocation_manifest, SponsoredManifestLoadError, SponsoredManifestLoadLimits,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SponsoredRefreshLimits {
    /// Maximum number of paid invoices considered for one subscription refresh.
    pub max_invoices: usize,
    pub manifest: SponsoredManifestLoadLimits,
}

impl Default for SponsoredRefreshLimits {
    fn default() -> Self {
        Self {
            max_invoices: 64,
            manifest: SponsoredManifestLoadLimits::default(),
        }
    }
}

impl SponsoredRefreshLimits {
    fn validate(self) -> Result<Self, SponsoredRefreshError> {
        if self.max_invoices == 0 {
            return Err(SponsoredRefreshError::InvalidConfig(
                "maximum invoice count must be nonzero",
            ));
        }
        Ok(self)
    }
}

#[derive(Debug, Error)]
pub enum SponsoredRefreshError {
    #[error("sponsored refresh inputs are invalid: {0}")]
    InvalidInput(&'static str),
    #[error("sponsored refresh configuration is invalid: {0}")]
    InvalidConfig(&'static str),
    #[error("sponsored refresh invoice history did not contain the triggering invoice")]
    TriggerInvoiceMissing,
    #[error("sponsored Stripe read failed: {0}")]
    Stripe(#[source] StripeReadError),
    #[error("sponsored allocation manifest load failed: {0}")]
    Manifest(#[source] SponsoredManifestLoadError),
    #[error("sponsored invoice composition needs more evidence: {0:?}")]
    NeedsEvidence(SponsoredNeedsEvidence),
    #[error("sponsored invoice composition failed: {0}")]
    Composition(#[source] SponsoredCoverageError),
    #[error("sponsored collection adapter failed: {0}")]
    Adapter(String),
    #[error("sponsored refresh preparation failed: {0}")]
    Preparation(#[source] ProviderAdapterError),
    #[error("sponsored refresh preparation rollback failed after {error}: {rollback}")]
    PreparationRollback {
        error: ProviderAdapterError,
        rollback: sqlx::Error,
    },
    #[error("sponsored refresh preparation commit failed: {0}")]
    PreparationCommit(#[source] sqlx::Error),
    #[error("sponsored refresh completion failed: {0}")]
    Completion(#[source] ProviderAdapterError),
    #[error("sponsored refresh completion rollback failed after {error}: {rollback}")]
    CompletionRollback {
        error: ProviderAdapterError,
        rollback: sqlx::Error,
    },
    #[error("sponsored refresh completion commit failed: {0}")]
    CompletionCommit(#[source] sqlx::Error),
    #[error("sponsored refresh database transaction could not start: {0}")]
    Transaction(#[source] sqlx::Error),
}

/// Executor configuration owned by the future refresh worker wiring.
#[derive(Clone)]
pub struct SponsoredRefreshExecutor {
    pool: PgPool,
    stripe: StripeReadClient,
    coverage: SponsoredStripeCoverageConfig,
    limits: SponsoredRefreshLimits,
}

impl SponsoredRefreshExecutor {
    pub fn new(
        pool: PgPool,
        stripe: StripeReadClient,
        coverage: SponsoredStripeCoverageConfig,
        limits: SponsoredRefreshLimits,
    ) -> Result<Self, SponsoredRefreshError> {
        let limits = limits.validate()?;
        Ok(Self {
            pool,
            stripe,
            coverage,
            limits,
        })
    }

    async fn execute_inner(
        &self,
        inputs: &RefreshJobInputs,
    ) -> Result<ApplyReceipt, SponsoredRefreshError> {
        validate_inputs(inputs, &self.coverage)?;
        let run_id = format!("sponsored-refresh:{}", Uuid::new_v4());
        let mut preparation_tx = self
            .pool
            .begin()
            .await
            .map_err(SponsoredRefreshError::Transaction)?;
        let preparation = match prepare_verified_event(
            &mut preparation_tx,
            &inputs.lease.context,
            &inputs.event,
            &inputs.allocation,
            &run_id,
        )
        .await
        {
            Ok(preparation) => preparation,
            Err(error) => {
                return match preparation_tx.rollback().await {
                    Ok(()) => Err(SponsoredRefreshError::Preparation(error)),
                    Err(rollback) => {
                        Err(SponsoredRefreshError::PreparationRollback { error, rollback })
                    }
                };
            }
        };
        preparation_tx
            .commit()
            .await
            .map_err(SponsoredRefreshError::PreparationCommit)?;

        let collection = collect_sponsored_collection(
            &self.pool,
            &self.stripe,
            &self.coverage,
            &inputs.lease.context,
            &inputs.event,
            &inputs.allocation,
            &preparation.ticket,
            self.limits,
        )
        .await?;

        let mut completion_tx = self
            .pool
            .begin()
            .await
            .map_err(SponsoredRefreshError::Transaction)?;
        let receipt = match complete_verified_event(
            &mut completion_tx,
            &inputs.lease.context,
            &inputs.event,
            &inputs.allocation,
            &preparation,
            &collection,
        )
        .await
        {
            Ok(receipt) => receipt,
            Err(error) => {
                return match completion_tx.rollback().await {
                    Ok(()) => Err(SponsoredRefreshError::Completion(error)),
                    Err(rollback) => {
                        Err(SponsoredRefreshError::CompletionRollback { error, rollback })
                    }
                };
            }
        };
        completion_tx
            .commit()
            .await
            .map_err(SponsoredRefreshError::CompletionCommit)?;
        Ok(receipt)
    }
}

#[async_trait]
impl RefreshJobExecutor for SponsoredRefreshExecutor {
    async fn execute(&mut self, inputs: &RefreshJobInputs) -> Result<(), RefreshExecutionError> {
        self.execute_inner(inputs)
            .await
            .map(|_| ())
            .map_err(|error| {
                // Queue rows intentionally retain a stable, non-sensitive category rather than a
                // provider identifier or database error string.
                let code = match error {
                    SponsoredRefreshError::InvalidInput(_)
                    | SponsoredRefreshError::InvalidConfig(_)
                    | SponsoredRefreshError::TriggerInvoiceMissing => "sponsored_refresh_invalid",
                    SponsoredRefreshError::Stripe(_) => "sponsored_refresh_stripe",
                    SponsoredRefreshError::Manifest(_) => "sponsored_refresh_manifest",
                    SponsoredRefreshError::NeedsEvidence(_)
                    | SponsoredRefreshError::Composition(_) => "sponsored_refresh_evidence",
                    SponsoredRefreshError::Adapter(_) => "sponsored_refresh_adapter",
                    SponsoredRefreshError::Preparation(_)
                    | SponsoredRefreshError::PreparationRollback { .. }
                    | SponsoredRefreshError::PreparationCommit(_)
                    | SponsoredRefreshError::Completion(_)
                    | SponsoredRefreshError::CompletionRollback { .. }
                    | SponsoredRefreshError::CompletionCommit(_)
                    | SponsoredRefreshError::Transaction(_) => "sponsored_refresh_database",
                };
                RefreshExecutionError::new(code).expect("static refresh error codes are nonempty")
            })
    }
}

fn validate_inputs(
    inputs: &RefreshJobInputs,
    coverage: &SponsoredStripeCoverageConfig,
) -> Result<(), SponsoredRefreshError> {
    if inputs.event.event_type != "invoice.paid" {
        return Err(SponsoredRefreshError::InvalidInput(
            "sponsored refresh requires invoice.paid",
        ));
    }
    if inputs
        .event
        .provider_object_id
        .as_deref()
        .is_none_or(str::is_empty)
    {
        return Err(SponsoredRefreshError::InvalidInput(
            "sponsored refresh requires the triggering invoice id",
        ));
    }
    if inputs.allocation.payer_kind != crate::cloud_provider::PayerKind::Sponsor {
        return Err(SponsoredRefreshError::InvalidInput(
            "sponsored refresh requires a sponsor allocation",
        ));
    }
    if inputs.lease.context.namespace != crate::cloud_provider_stripe::STRIPE_NAMESPACE
        || inputs.lease.context.account_id != coverage.account_id()
        || inputs.lease.context.environment != coverage.environment()
    {
        return Err(SponsoredRefreshError::InvalidInput(
            "sponsored refresh context does not match Stripe coverage",
        ));
    }
    Ok(())
}

/// Collect one provider-neutral collection from every relevant paid invoice in the subscription.
#[allow(clippy::too_many_arguments)]
///
/// A manifest is loaded for each invoice period, so ended and replacement seats are evaluated at
/// the dates they were actually paid. Invoices that have no registered source in their period are
/// ignored; an invoice that could explain a registered source but cannot be composed fails closed.
pub async fn collect_sponsored_collection(
    pool: &PgPool,
    stripe: &StripeReadClient,
    coverage: &SponsoredStripeCoverageConfig,
    context: &ProviderContext,
    event: &VerifiedProviderEvent,
    allocation: &VerifiedAllocation,
    ticket: &crate::cloud_coverage_reconciliation::CollectionTicket,
    limits: SponsoredRefreshLimits,
) -> Result<VerifiedCollection, SponsoredRefreshError> {
    let limits = limits.validate()?;
    let invoice_id =
        event
            .provider_object_id
            .as_deref()
            .ok_or(SponsoredRefreshError::InvalidInput(
                "triggering invoice id is required",
            ))?;
    let mut session = stripe.session();
    let mut invoices = stripe
        .subscription_invoices_bounded(
            &mut session,
            &allocation.subscription_id,
            Some(&allocation.provider_customer_id),
            limits.max_invoices,
        )
        .await
        .map_err(SponsoredRefreshError::Stripe)?;
    if !invoices.iter().any(|invoice| invoice.id == invoice_id) {
        invoices.push(
            stripe
                .invoice(&mut session, invoice_id)
                .await
                .map_err(SponsoredRefreshError::Stripe)?,
        );
    }
    let Some(trigger) = invoices.iter().find(|invoice| invoice.id == invoice_id) else {
        return Err(SponsoredRefreshError::TriggerInvoiceMissing);
    };
    if trigger.status.as_deref() != Some("paid") {
        return Err(SponsoredRefreshError::InvalidInput(
            "triggering invoice is not paid",
        ));
    }

    let expected_sources = ticket
        .source_bindings
        .iter()
        .filter(|binding| binding.beneficiary_id == ticket.beneficiary_id)
        .map(|binding| binding.source_id.as_str())
        .collect::<BTreeSet<_>>();
    #[derive(Debug)]
    enum SourceSelection {
        Candidate {
            period_end: i64,
            invoice_id: String,
            candidate_index: usize,
        },
        NeedsEvidence {
            period_end: i64,
            invoice_id: String,
            reason: SponsoredNeedsEvidence,
        },
    }

    fn is_newer(
        period_end: i64,
        invoice_id: &str,
        previous_period_end: i64,
        previous_invoice_id: &str,
    ) -> bool {
        (period_end, invoice_id) > (previous_period_end, previous_invoice_id)
    }

    let mut candidates = Vec::new();
    let mut selections = BTreeMap::<String, SourceSelection>::new();
    for invoice in invoices {
        if invoice.status.as_deref() != Some("paid") {
            continue;
        }
        let settlement = stripe
            .sponsored_invoice_settlement(
                &mut session,
                &invoice.id,
                &allocation.provider_customer_id,
                &allocation.subscription_id,
                coverage,
            )
            .await
            .map_err(SponsoredRefreshError::Stripe)?;
        let manifest = load_sponsored_allocation_manifest(
            pool,
            context,
            &allocation.provider_customer_id,
            &allocation.subscription_id,
            settlement.period_start(),
            settlement.period_end(),
            limits.manifest,
        )
        .await
        .map_err(SponsoredRefreshError::Manifest)?;
        let relevant_sources = manifest
            .allocations()
            .iter()
            .filter(|interval| expected_sources.contains(interval.source_id()))
            .map(|interval| interval.source_id().to_owned())
            .collect::<BTreeSet<_>>();
        if relevant_sources.is_empty() {
            continue;
        }
        match compose_sponsored_coverage(coverage, &manifest, &settlement)
            .map_err(SponsoredRefreshError::Composition)?
        {
            SponsoredCoverageResult::Candidate(candidate) => {
                let candidate_index = candidates.len();
                let source_ids = candidate
                    .source_observations()
                    .into_iter()
                    .map(|observation| match observation {
                        crate::cloud_coverage_reconciliation::SourceObservation::Complete {
                            source_id,
                            ..
                        }
                        | crate::cloud_coverage_reconciliation::SourceObservation::Unavailable {
                            source_id,
                            ..
                        } => source_id,
                    })
                    .filter(|source_id| expected_sources.contains(source_id.as_str()))
                    .collect::<BTreeSet<_>>();
                candidates.push(candidate);
                for source_id in source_ids {
                    let replace = match selections.get(&source_id) {
                        Some(SourceSelection::Candidate {
                            period_end,
                            invoice_id: previous_invoice_id,
                            ..
                        })
                        | Some(SourceSelection::NeedsEvidence {
                            period_end,
                            invoice_id: previous_invoice_id,
                            ..
                        }) => is_newer(
                            settlement.period_end(),
                            &invoice.id,
                            *period_end,
                            previous_invoice_id,
                        ),
                        None => true,
                    };
                    if replace {
                        selections.insert(
                            source_id,
                            SourceSelection::Candidate {
                                period_end: settlement.period_end(),
                                invoice_id: invoice.id.clone(),
                                candidate_index,
                            },
                        );
                    }
                }
            }
            SponsoredCoverageResult::NeedsEvidence(reason) => {
                for source_id in relevant_sources {
                    let replace = match selections.get(&source_id) {
                        Some(SourceSelection::Candidate {
                            period_end,
                            invoice_id: previous_invoice_id,
                            ..
                        })
                        | Some(SourceSelection::NeedsEvidence {
                            period_end,
                            invoice_id: previous_invoice_id,
                            ..
                        }) => is_newer(
                            settlement.period_end(),
                            &invoice.id,
                            *period_end,
                            previous_invoice_id,
                        ),
                        None => true,
                    };
                    if replace {
                        selections.insert(
                            source_id,
                            SourceSelection::NeedsEvidence {
                                period_end: settlement.period_end(),
                                invoice_id: invoice.id.clone(),
                                reason: reason.clone(),
                            },
                        );
                    }
                }
            }
        }
    }

    let mut selected_candidates = BTreeMap::new();
    for source_id in expected_sources {
        match selections.get(source_id) {
            Some(SourceSelection::Candidate {
                candidate_index, ..
            }) => {
                selected_candidates.insert(source_id.to_owned(), &candidates[*candidate_index]);
            }
            Some(SourceSelection::NeedsEvidence { reason, .. }) => {
                return Err(SponsoredRefreshError::NeedsEvidence(reason.clone()));
            }
            None => {}
        }
    }
    collection_for_ticket_by_source(context, ticket, &selected_candidates)
        .map_err(|error| SponsoredRefreshError::Adapter(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::{validate_inputs, SponsoredRefreshLimits};
    use crate::cloud_provider::{
        AllocationState, PayerKind, ProviderContext, ProviderEnvironment, VerifiedAllocation,
        VerifiedProviderEvent,
    };
    use crate::cloud_provider_refresh_inputs::RefreshJobInputs;
    use crate::cloud_provider_refresh_jobs::RefreshJobLease;
    use crate::cloud_provider_stripe_sponsored::SponsoredStripeCoverageConfig;

    fn inputs(event_type: &str, object_id: Option<&str>) -> RefreshJobInputs {
        let context =
            ProviderContext::new("stripe", "acct_test", ProviderEnvironment::Test).unwrap();
        let event = VerifiedProviderEvent::from_stored(
            "evt_1".into(),
            event_type.to_owned(),
            1,
            Some("sub_1".into()),
            None,
            object_id.map(str::to_owned),
            "0000000000000000000000000000000000000000000000000000000000000000".into(),
        )
        .unwrap();
        let allocation = VerifiedAllocation::new(
            "allocation_1",
            "payer_1",
            "cus_1",
            PayerKind::Sponsor,
            "user_1",
            "sub_1",
            "item_1",
            "reference_1",
            "source_1",
            1,
            None,
            AllocationState::Active,
            "ownership",
        )
        .unwrap();
        RefreshJobInputs {
            lease: RefreshJobLease {
                job_id: "job".into(),
                context,
                event_id: "evt_1".into(),
                beneficiary_id: "user_1".into(),
                allocation_id: "allocation_1".into(),
                source_id: "source_1".into(),
                worker_id: "worker".into(),
                attempt_count: 1,
            },
            event,
            allocation,
            receipt_status: "pending".into(),
        }
    }

    fn coverage() -> SponsoredStripeCoverageConfig {
        SponsoredStripeCoverageConfig::new(
            "acct_test",
            ProviderEnvironment::Test,
            "price_standard_month",
            "price_standard_year",
            "price_founding_month",
            "price_founding_year",
        )
        .unwrap()
    }

    #[test]
    fn limits_require_a_positive_invoice_bound() {
        assert!(SponsoredRefreshLimits {
            max_invoices: 0,
            ..SponsoredRefreshLimits::default()
        }
        .validate()
        .is_err());
    }

    #[test]
    fn refresh_requires_the_paid_invoice_object_identity() {
        let error = validate_inputs(&inputs("invoice.paid", None), &coverage()).unwrap_err();
        assert!(matches!(
            error,
            super::SponsoredRefreshError::InvalidInput(_)
        ));
    }

    #[test]
    fn refresh_rejects_non_paid_events() {
        let error = validate_inputs(
            &inputs("customer.subscription.updated", Some("in_1")),
            &coverage(),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            super::SponsoredRefreshError::InvalidInput(_)
        ));
    }
}
