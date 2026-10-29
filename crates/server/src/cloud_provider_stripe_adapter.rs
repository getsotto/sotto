//! Prepared inputs for the concrete Stripe coverage adapter.
//!
//! The refresh lifecycle captures a source-set ticket before network I/O. This module turns that
//! ticket plus caller-validated Stripe allocations into an immutable adapter input. It refuses
//! incomplete, mixed-beneficiary, mixed-provider or sponsored source sets before a provider read
//! can silently publish only the convenient personal subset.

#![allow(dead_code)]

use std::collections::BTreeSet;

use thiserror::Error;

use crate::cloud_coverage::ConfirmedPaidInterval;
use crate::cloud_coverage_reconciliation::{CollectionTicket, SourceBinding};
use crate::cloud_provider::{
    ProviderCollectionError, ProviderContext, ProviderEnvironment, ProviderHistoryClient,
    ProviderHistoryPage, VerifiedAllocation,
};
use crate::cloud_provider_stripe::STRIPE_NAMESPACE;
use crate::cloud_provider_stripe::{StripeAllocationBinding, StripeCoverageConfig};
use crate::cloud_provider_stripe_authority::StripeAuthoritySource;
use crate::cloud_provider_stripe_corrections::StripeRetainedPaidTerm;
use crate::cloud_provider_stripe_http::{
    StripePersonalInvoiceHistory, StripePersonalInvoiceHistoryEntry,
    StripePersonalInvoiceHistoryResult, StripeReadClient, StripeReadSession,
};
use crate::cloud_provider_stripe_renewals::StripeRenewalFailureEvidence;
use async_trait::async_trait;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StripePreparedSource {
    pub(crate) binding: SourceBinding,
    pub(crate) allocation: StripeAllocationBinding,
    pub(crate) generation: i64,
    pub(crate) evidence_reference: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StripePreparedCollection {
    beneficiary_id: String,
    account_id: String,
    environment: ProviderEnvironment,
    source_set_generation: i64,
    invalidation_generation: i64,
    sources: Vec<StripePreparedSource>,
}

impl StripePreparedCollection {
    pub(crate) fn from_ticket(
        config: &StripeCoverageConfig,
        ticket: &CollectionTicket,
        sources: Vec<StripePreparedSource>,
    ) -> Result<Self, StripeAdapterInputError> {
        config.validate().map_err(StripeAdapterInputError::Config)?;
        if ticket.beneficiary_id.trim().is_empty()
            || ticket.source_set_generation < 0
            || ticket
                .provider_invalidation_generation
                .is_none_or(|generation| generation < 0)
        {
            return Err(StripeAdapterInputError::InvalidTicket);
        }
        if sources.len() != ticket.source_bindings.len() || sources.is_empty() {
            return Err(StripeAdapterInputError::IncompleteSources);
        }
        let expected = ticket
            .source_bindings
            .iter()
            .map(|binding| binding.source_id.as_str())
            .collect::<BTreeSet<_>>();
        let actual = sources
            .iter()
            .map(|source| source.binding.source_id.as_str())
            .collect::<BTreeSet<_>>();
        if expected != actual || expected.len() != sources.len() {
            return Err(StripeAdapterInputError::IncompleteSources);
        }
        for source in &sources {
            validate_source(config, ticket, source)?;
        }
        let invalidation_generation = ticket.provider_invalidation_generation.unwrap_or(0);
        Ok(Self {
            beneficiary_id: ticket.beneficiary_id.clone(),
            account_id: config.account_id.clone(),
            environment: config.environment,
            source_set_generation: ticket.source_set_generation,
            invalidation_generation,
            sources,
        })
    }

    pub(crate) fn beneficiary_id(&self) -> &str {
        &self.beneficiary_id
    }
    pub(crate) fn account_id(&self) -> &str {
        &self.account_id
    }
    pub(crate) fn environment(&self) -> ProviderEnvironment {
        self.environment
    }
    pub(crate) fn source_set_generation(&self) -> i64 {
        self.source_set_generation
    }
    pub(crate) fn invalidation_generation(&self) -> i64 {
        self.invalidation_generation
    }
    pub(crate) fn sources(&self) -> &[StripePreparedSource] {
        &self.sources
    }

    pub(crate) fn authority_sources(&self) -> Vec<StripeAuthoritySource> {
        self.sources
            .iter()
            .map(|source| StripeAuthoritySource {
                source_id: source.binding.source_id.clone(),
                generation: source.generation,
                evidence_reference: source.evidence_reference.clone(),
            })
            .collect()
    }
}

#[derive(Debug, Error)]
pub(crate) enum StripeAdapterInputError {
    #[error("Stripe adapter configuration is invalid: {0}")]
    Config(crate::cloud_provider_stripe::StripeContractError),
    #[error("prepared collection ticket is invalid")]
    InvalidTicket,
    #[error("prepared Stripe source set is incomplete")]
    IncompleteSources,
    #[error("prepared Stripe source is invalid: {0}")]
    InvalidSource(&'static str),
    #[error("prepared Stripe source is not personal")]
    UnsupportedPayer,
}

fn validate_source(
    config: &StripeCoverageConfig,
    ticket: &CollectionTicket,
    source: &StripePreparedSource,
) -> Result<(), StripeAdapterInputError> {
    let binding = &source.binding;
    if binding.beneficiary_id != ticket.beneficiary_id
        || binding.provider_namespace != STRIPE_NAMESPACE
        || binding.external_allocation_reference != source.allocation.allocation_reference()
        || binding.source_id.trim().is_empty()
    {
        return Err(StripeAdapterInputError::InvalidSource("ownership"));
    }
    if source.allocation.payer_kind() != crate::cloud_provider::PayerKind::Personal {
        return Err(StripeAdapterInputError::UnsupportedPayer);
    }
    if source.generation <= 0 || source.evidence_reference.trim().is_empty() {
        return Err(StripeAdapterInputError::InvalidSource("source evidence"));
    }
    if source.allocation.customer_id().trim().is_empty()
        || source.allocation.subscription_id().trim().is_empty()
        || source.allocation.provider_item_id().trim().is_empty()
        || config.account_id.trim().is_empty()
    {
        return Err(StripeAdapterInputError::InvalidSource(
            "allocation identity",
        ));
    }
    Ok(())
}

/// Construct one prepared source from the durable allocation identity. The caller must still
/// supply the ticket's source id and generation; those values are never inferred from Stripe.
pub(crate) fn prepared_source(
    binding: SourceBinding,
    allocation: StripeAllocationBinding,
    generation: i64,
    evidence_reference: impl Into<String>,
) -> StripePreparedSource {
    StripePreparedSource {
        binding,
        allocation,
        generation,
        evidence_reference: evidence_reference.into(),
    }
}

/// Keep the allocation type visible at this seam without allowing callers to construct a source
/// from unvalidated database columns.
pub(crate) fn allocation_is_active(allocation: &VerifiedAllocation) -> bool {
    matches!(
        allocation.state,
        crate::cloud_provider::AllocationState::Active
    )
}

/// Concrete provider-neutral history transport for one prepared Stripe collection.
///
/// A single read session is shared across every source. The client therefore applies one request,
/// page, record, byte and deadline budget instead of resetting limits for each sibling source.
pub(crate) struct StripeHistoryClient {
    client: StripeReadClient,
    session: StripeReadSession,
    prepared: StripePreparedCollection,
    renewal_failures: Vec<StripeRenewalFailureEvidence>,
}

impl StripeHistoryClient {
    pub(crate) fn new(client: StripeReadClient, prepared: StripePreparedCollection) -> Self {
        let session = client.session();
        Self {
            client,
            session,
            prepared,
            renewal_failures: Vec::new(),
        }
    }

    pub(crate) fn with_renewal_failures(
        mut self,
        renewal_failures: Vec<StripeRenewalFailureEvidence>,
    ) -> Self {
        self.renewal_failures = renewal_failures;
        self
    }
}

#[async_trait]
impl ProviderHistoryClient for StripeHistoryClient {
    async fn fetch_page(
        &mut self,
        context: &ProviderContext,
        binding: &SourceBinding,
        cursor: Option<&str>,
    ) -> Result<ProviderHistoryPage, ProviderCollectionError> {
        if context.namespace != STRIPE_NAMESPACE
            || context.account_id != self.prepared.account_id
            || context.environment != self.prepared.environment
        {
            return Err(ProviderCollectionError::ContextMismatch);
        }
        if cursor.is_some() {
            return Err(ProviderCollectionError::RepeatedCursor);
        }
        let source = self
            .prepared
            .sources
            .iter()
            .find(|source| source.binding.source_id == binding.source_id)
            .ok_or(ProviderCollectionError::SourceMismatch)?;
        let history = self
            .client
            .personal_invoice_history(&mut self.session, &source.allocation)
            .await
            .map_err(|error| ProviderCollectionError::Fetch(error.to_string()))?;
        let history = match history {
            StripePersonalInvoiceHistoryResult::Observed(history) => history,
            StripePersonalInvoiceHistoryResult::NeedsEvidence(_) => {
                return Err(ProviderCollectionError::InvalidEvidence(
                    "Stripe invoice history needs more evidence".into(),
                ));
            }
        };
        let mut paid_intervals = Vec::new();
        let mut evidence = Vec::new();
        for entry in history.entries() {
            if let StripePersonalInvoiceHistoryEntry::Paid(term) = entry {
                let active_failure = active_failure_for_term(
                    &history,
                    &self.renewal_failures,
                    &source.allocation,
                    term,
                )?;
                paid_intervals.push(ConfirmedPaidInterval {
                    coverage_id: term.invoice_id().to_owned(),
                    source_id: binding.source_id.clone(),
                    starts_at: term.period_start(),
                    paid_until: term.period_end(),
                    failed_renewal_id: active_failure
                        .map(|failure| failure.renewal_id().to_owned()),
                });
                evidence.push(term.evidence_reference().to_owned());
                if let Some(failure) = active_failure {
                    evidence.push(failure.evidence_reference().to_owned());
                }
            }
        }
        evidence.sort();
        Ok(ProviderHistoryPage {
            context: context.clone(),
            source_id: binding.source_id.clone(),
            evidence_reference: format!("stripe-history-v1:{}", evidence.join(",")),
            paid_intervals,
            next_cursor: None,
            authoritative_end: true,
        })
    }
}

fn active_failure_for_term<'a>(
    history: &StripePersonalInvoiceHistory,
    failures: &'a [StripeRenewalFailureEvidence],
    allocation: &StripeAllocationBinding,
    term: &StripeRetainedPaidTerm,
) -> Result<Option<&'a StripeRenewalFailureEvidence>, ProviderCollectionError> {
    let linked = failures
        .iter()
        .filter(|failure| {
            failure.allocation_reference() == allocation.allocation_reference()
                && failure.customer_id() == allocation.customer_id()
                && failure.subscription_id() == allocation.subscription_id()
                && failure.predecessor_invoice_id() == term.invoice_id()
        })
        .collect::<Vec<_>>();
    let Some(renewal_id) = linked.first().map(|failure| failure.renewal_id()) else {
        return Ok(None);
    };
    if linked
        .iter()
        .any(|failure| failure.renewal_id() != renewal_id)
    {
        return Err(ProviderCollectionError::InvalidEvidence(
            "multiple renewal invoices target one paid term".into(),
        ));
    }
    let first_claim = linked[0];
    if linked.iter().skip(1).any(|failure| {
        failure.invoice_line_id() != first_claim.invoice_line_id()
            || failure.predecessor_evidence_reference()
                != first_claim.predecessor_evidence_reference()
            || failure.predecessor_period_start() != first_claim.predecessor_period_start()
            || failure.predecessor_period_end() != first_claim.predecessor_period_end()
            || failure.renewal_period_start() != first_claim.renewal_period_start()
            || failure.renewal_period_end() != first_claim.renewal_period_end()
            || failure.interval() != first_claim.interval()
    }) {
        return Err(ProviderCollectionError::InvalidEvidence(
            "failure events disagree about their renewal evidence".into(),
        ));
    }
    let Some(failure) = linked
        .into_iter()
        .min_by_key(|failure| (failure.event_created_at(), failure.event_id()))
    else {
        return Ok(None);
    };
    Ok((history.recovery_currently_eligible()
        && history.entries().iter().any(|entry| {
            matches!(
                entry,
                StripePersonalInvoiceHistoryEntry::NonPaid(invoice)
                    if invoice.invoice_id() == failure.invoice_id()
                        && invoice.status() == "open"
            )
        }))
    .then_some(failure))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud_coverage_reconciliation::CollectionStatus;
    use crate::cloud_provider::PayerKind;
    use crate::cloud_provider_stripe::{StripeInterval, StripePersonalInvoiceObservation};
    use crate::cloud_provider_stripe_corrections::StripeRetainedPaidTerm;
    use crate::cloud_provider_stripe_http::StripeNonPaidInvoice;
    use crate::cloud_provider_stripe_renewals::StripeRenewalFailureEvidence;

    fn config() -> StripeCoverageConfig {
        StripeCoverageConfig::new(
            "acct_test",
            ProviderEnvironment::Test,
            "price_month",
            "price_year",
        )
        .unwrap()
    }

    fn ticket() -> CollectionTicket {
        CollectionTicket {
            beneficiary_id: "user_1".into(),
            attempt_id: "attempt_1".into(),
            collection_epoch: 1,
            source_set_generation: 4,
            provider_invalidation_generation: Some(0),
            expected_projection_revision: None,
            source_bindings: vec![SourceBinding {
                beneficiary_id: "user_1".into(),
                source_id: "source_1".into(),
                provider_namespace: STRIPE_NAMESPACE.into(),
                external_allocation_reference: "allocation:one".into(),
                ownership_evidence_reference: "ownership:one".into(),
            }],
            status: CollectionStatus::Pending,
            completed_revision: None,
        }
    }

    fn source() -> StripePreparedSource {
        prepared_source(
            ticket().source_bindings[0].clone(),
            StripeAllocationBinding::new(
                "allocation:one",
                "cus_one",
                "sub_one",
                "si_one",
                PayerKind::Personal,
            )
            .unwrap(),
            1,
            "source:evidence",
        )
    }

    fn paid_term() -> StripeRetainedPaidTerm {
        StripeRetainedPaidTerm::for_test(StripePersonalInvoiceObservation::for_test(
            "pi_predecessor",
            "cus_one",
            "sub_one",
            "si_one",
            "allocation:one",
            StripeInterval::Month,
            1_700_000_000,
            1_702_592_000,
            "paid-evidence",
        ))
    }

    fn renewal_failure() -> StripeRenewalFailureEvidence {
        StripeRenewalFailureEvidence::for_test(
            "allocation:one",
            "cus_one",
            "sub_one",
            "si_one",
            "pi_failed",
            "pi_predecessor",
            "renewal_one",
        )
    }

    #[test]
    fn renewal_recovery_requires_a_current_open_invoice_and_live_subscription() {
        let source = source();
        let term = paid_term();
        let failure = renewal_failure();
        let open = StripePersonalInvoiceHistory::for_test(
            "acct_test",
            ProviderEnvironment::Test,
            "sub_one",
            "cus_one",
            vec![
                StripePersonalInvoiceHistoryEntry::Paid(term.clone()),
                StripePersonalInvoiceHistoryEntry::NonPaid(StripeNonPaidInvoice::for_test(
                    "pi_failed",
                    "open",
                )),
            ],
        );
        assert_eq!(
            active_failure_for_term(
                &open,
                std::slice::from_ref(&failure),
                &source.allocation,
                &term
            )
            .unwrap()
            .map(|failure| failure.renewal_id()),
            Some("renewal_one")
        );
        let retry_event = failure
            .clone()
            .with_test_event("evt_retry_2", 1_702_592_010);
        let retries = [failure.clone(), retry_event];
        let selected = active_failure_for_term(&open, &retries, &source.allocation, &term)
            .expect("retries for one renewal are one failure")
            .expect("open renewal stays eligible");
        assert_eq!(selected.renewal_id(), "renewal_one");
        assert_eq!(selected.event_id(), failure.event_id());
        let different_renewal = StripeRenewalFailureEvidence::for_test(
            "allocation:one",
            "cus_one",
            "sub_one",
            "si_one",
            "pi_failed_two",
            "pi_predecessor",
            "renewal_two",
        );
        assert!(matches!(
            active_failure_for_term(
                &open,
                &[failure.clone(), different_renewal],
                &source.allocation,
                &term
            ),
            Err(ProviderCollectionError::InvalidEvidence(_))
        ));

        for (status, recovery_allowed) in [("void", true), ("uncollectible", true), ("open", false)]
        {
            let history = StripePersonalInvoiceHistory::for_test_with_recovery(
                "acct_test",
                ProviderEnvironment::Test,
                "sub_one",
                "cus_one",
                recovery_allowed,
                vec![
                    StripePersonalInvoiceHistoryEntry::Paid(term.clone()),
                    StripePersonalInvoiceHistoryEntry::NonPaid(StripeNonPaidInvoice::for_test(
                        "pi_failed",
                        status,
                    )),
                ],
            );
            assert!(active_failure_for_term(
                &history,
                std::slice::from_ref(&failure),
                &source.allocation,
                &term
            )
            .unwrap()
            .is_none());
        }
    }

    #[test]
    fn prepared_input_keeps_every_source_and_the_zero_fence() {
        let prepared =
            StripePreparedCollection::from_ticket(&config(), &ticket(), vec![source()]).unwrap();
        assert_eq!(prepared.invalidation_generation(), 0);
        assert_eq!(prepared.source_set_generation(), 4);
        assert_eq!(prepared.authority_sources().len(), 1);
    }

    #[test]
    fn missing_sibling_and_sponsored_sources_fail_before_reads() {
        let error =
            StripePreparedCollection::from_ticket(&config(), &ticket(), Vec::new()).unwrap_err();
        assert!(matches!(error, StripeAdapterInputError::IncompleteSources));

        let mut wrong_owner = source();
        wrong_owner.binding.beneficiary_id = "user_2".into();
        let error = StripePreparedCollection::from_ticket(&config(), &ticket(), vec![wrong_owner])
            .unwrap_err();
        assert!(matches!(
            error,
            StripeAdapterInputError::InvalidSource("ownership")
        ));
    }
}
