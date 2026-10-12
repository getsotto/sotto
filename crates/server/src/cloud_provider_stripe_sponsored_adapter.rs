//! Convert a composed sponsored Stripe snapshot into one beneficiary collection.
//!
//! Sponsored invoices cover several named beneficiaries at once, while the reconciliation
//! boundary publishes one beneficiary's registered source set at a time.  This module is the
//! narrow, provider-specific seam between those shapes.  It consumes only the authenticated,
//! composed candidates and a durable collection ticket; it does not read Stripe or Postgres and
//! it does not start a refresh job.

#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;

use crate::cloud_coverage_reconciliation::{CollectionStatus, CollectionTicket, SourceObservation};
use crate::cloud_provider::{PayerKind, ProviderContext, VerifiedCollection};
use crate::cloud_provider_stripe::STRIPE_NAMESPACE;
use crate::cloud_provider_stripe_sponsored::SponsoredCoverageCandidate;

/// Build the provider-neutral collection for the beneficiary captured by a ticket.
///
/// Each candidate may cover several beneficiaries, and a beneficiary may have sources on several
/// invoices. Only observations whose source belongs to the ticket beneficiary are returned. The
/// candidates must collectively cover the ticket's complete source set exactly. This prevents a
/// grouped invoice from publishing a sibling's facts or from silently publishing an incomplete
/// source batch.
pub(crate) fn collection_for_ticket(
    context: &ProviderContext,
    ticket: &CollectionTicket,
    candidates: &[&SponsoredCoverageCandidate],
) -> Result<VerifiedCollection, SponsoredAdapterError> {
    if ticket.status != CollectionStatus::Pending || ticket.completed_revision.is_some() {
        return Err(SponsoredAdapterError::InvalidTicket);
    }
    if ticket.source_bindings.is_empty() || ticket.beneficiary_id.trim().is_empty() {
        return Err(SponsoredAdapterError::InvalidTicket);
    }

    let expected_sources = ticket
        .source_bindings
        .iter()
        .filter(|binding| binding.beneficiary_id == ticket.beneficiary_id)
        .map(|binding| binding.source_id.as_str())
        .collect::<BTreeSet<_>>();
    if expected_sources.len() != ticket.source_bindings.len() {
        return Err(SponsoredAdapterError::InvalidTicket);
    }

    if candidates.is_empty() {
        return Err(SponsoredAdapterError::SourceSetMismatch);
    }

    let mut observations_by_source = BTreeMap::new();
    let mut candidate_references = BTreeSet::new();
    for candidate in candidates {
        validate_context(context, ticket, candidate)?;
        candidate_references.insert(candidate.semantic_reference().to_owned());

        let Some(beneficiary) = candidate
            .beneficiaries()
            .iter()
            .find(|beneficiary| beneficiary.beneficiary_id() == ticket.beneficiary_id)
        else {
            continue;
        };

        let mut allocation_references = BTreeMap::new();
        for term in beneficiary.paid_terms() {
            let source_id = term.interval().source_id.clone();
            if !expected_sources.contains(source_id.as_str()) {
                continue;
            }
            if let Some(existing) = allocation_references
                .insert(source_id.clone(), term.allocation_reference().to_owned())
            {
                if existing != term.allocation_reference() {
                    return Err(SponsoredAdapterError::AllocationReferenceMismatch);
                }
            }
        }

        let candidate_observations = candidate.source_observations();
        for (source_id, allocation_reference) in allocation_references {
            let binding = ticket
                .source_bindings
                .iter()
                .find(|binding| binding.source_id == source_id)
                .ok_or(SponsoredAdapterError::MissingObservation)?;
            if allocation_reference != binding.external_allocation_reference {
                return Err(SponsoredAdapterError::AllocationReferenceMismatch);
            }
            let observation = candidate_observations
                .iter()
                .find(|observation| observation_source_id(observation) == source_id)
                .cloned()
                .ok_or(SponsoredAdapterError::MissingObservation)?;
            if observations_by_source
                .insert(source_id, observation)
                .is_some()
            {
                return Err(SponsoredAdapterError::DuplicateObservation);
            }
        }
    }

    if observations_by_source.len() != expected_sources.len() {
        return Err(SponsoredAdapterError::SourceSetMismatch);
    }

    let observations = expected_sources
        .iter()
        .map(|source_id| {
            observations_by_source
                .remove(*source_id)
                .ok_or(SponsoredAdapterError::MissingObservation)
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(VerifiedCollection {
        aggregate_evidence_reference: format!(
            "stripe-sponsored-collection-v1:{}:{}",
            candidate_references
                .into_iter()
                .collect::<Vec<_>>()
                .join(","),
            ticket.beneficiary_id
        ),
        observations,
    })
}

/// Build a collection when each registered source has already selected its newest candidate.
///
/// A candidate can contain observations for several beneficiaries and sources. The source map is
/// therefore explicit: it prevents a candidate selected for one source from re-introducing a
/// duplicate observation for another source while retaining the same context and allocation
/// checks as [`collection_for_ticket`].
pub(crate) fn collection_for_ticket_by_source(
    context: &ProviderContext,
    ticket: &CollectionTicket,
    candidates: &BTreeMap<String, &SponsoredCoverageCandidate>,
) -> Result<VerifiedCollection, SponsoredAdapterError> {
    if ticket.status != CollectionStatus::Pending || ticket.completed_revision.is_some() {
        return Err(SponsoredAdapterError::InvalidTicket);
    }
    if ticket.source_bindings.is_empty() || ticket.beneficiary_id.trim().is_empty() {
        return Err(SponsoredAdapterError::InvalidTicket);
    }

    let expected_sources = ticket
        .source_bindings
        .iter()
        .filter(|binding| binding.beneficiary_id == ticket.beneficiary_id)
        .map(|binding| binding.source_id.as_str())
        .collect::<BTreeSet<_>>();
    if expected_sources.len() != ticket.source_bindings.len()
        || candidates.len() != expected_sources.len()
        || candidates
            .keys()
            .any(|source_id| !expected_sources.contains(source_id.as_str()))
    {
        return Err(SponsoredAdapterError::SourceSetMismatch);
    }

    let mut observations = Vec::with_capacity(expected_sources.len());
    let mut candidate_references = BTreeSet::new();
    for source_id in expected_sources {
        let candidate = candidates
            .get(source_id)
            .ok_or(SponsoredAdapterError::MissingObservation)?;
        validate_context(context, ticket, candidate)?;
        candidate_references.insert(candidate.semantic_reference().to_owned());

        let beneficiary = candidate
            .beneficiaries()
            .iter()
            .find(|beneficiary| beneficiary.beneficiary_id() == ticket.beneficiary_id)
            .ok_or(SponsoredAdapterError::MissingObservation)?;
        let mut allocation_reference = None;
        for term in beneficiary.paid_terms() {
            if term.interval().source_id != source_id {
                continue;
            }
            if let Some(existing) = allocation_reference.replace(term.allocation_reference()) {
                if existing != term.allocation_reference() {
                    return Err(SponsoredAdapterError::AllocationReferenceMismatch);
                }
            }
        }
        let allocation_reference =
            allocation_reference.ok_or(SponsoredAdapterError::MissingObservation)?;
        let binding = ticket
            .source_bindings
            .iter()
            .find(|binding| binding.source_id == source_id)
            .ok_or(SponsoredAdapterError::MissingObservation)?;
        if allocation_reference != binding.external_allocation_reference {
            return Err(SponsoredAdapterError::AllocationReferenceMismatch);
        }
        let observation = candidate
            .source_observations()
            .into_iter()
            .find(|observation| observation_source_id(observation) == source_id)
            .ok_or(SponsoredAdapterError::MissingObservation)?;
        observations.push(observation);
    }

    Ok(VerifiedCollection {
        aggregate_evidence_reference: format!(
            "stripe-sponsored-collection-v1:{}:{}",
            candidate_references
                .into_iter()
                .collect::<Vec<_>>()
                .join(","),
            ticket.beneficiary_id
        ),
        observations,
    })
}

fn observation_source_id(observation: &SourceObservation) -> &str {
    match observation {
        SourceObservation::Complete { source_id, .. }
        | SourceObservation::Unavailable { source_id, .. } => source_id,
    }
}

fn validate_context(
    context: &ProviderContext,
    ticket: &CollectionTicket,
    candidate: &SponsoredCoverageCandidate,
) -> Result<(), SponsoredAdapterError> {
    if context.namespace != STRIPE_NAMESPACE
        || context.account_id != candidate.account_id()
        || context.environment != candidate.environment()
    {
        return Err(SponsoredAdapterError::ContextMismatch);
    }
    if candidate.payer_kind() != PayerKind::Sponsor {
        return Err(SponsoredAdapterError::UnsupportedPayer);
    }
    if ticket.attempt_id.trim().is_empty()
        || ticket.collection_epoch <= 0
        || ticket.source_set_generation <= 0
        || ticket
            .provider_invalidation_generation
            .is_some_and(|generation| generation < 0)
        || ticket
            .expected_projection_revision
            .is_some_and(|revision| revision <= 0)
        || ticket
            .completed_revision
            .is_some_and(|revision| revision <= 0)
        || ticket.beneficiary_id.trim().is_empty()
        || ticket.source_bindings.iter().any(|binding| {
            binding.provider_namespace != STRIPE_NAMESPACE
                || binding.beneficiary_id.trim().is_empty()
                || binding.source_id.trim().is_empty()
        })
    {
        return Err(SponsoredAdapterError::InvalidTicket);
    }
    Ok(())
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SponsoredAdapterError {
    #[error("sponsored candidate does not match the provider context")]
    ContextMismatch,
    #[error("sponsored collection ticket is invalid")]
    InvalidTicket,
    #[error("sponsored candidate is not payer-owned")]
    UnsupportedPayer,
    #[error("sponsored candidate does not cover the ticket source set")]
    SourceSetMismatch,
    #[error("sponsored candidate is missing a ticket source observation")]
    MissingObservation,
    #[error("sponsored candidate contains duplicate source observations")]
    DuplicateObservation,
    #[error("sponsored candidate allocation reference does not match the ticket")]
    AllocationReferenceMismatch,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud_coverage_reconciliation::SourceBinding;
    use crate::cloud_provider::ProviderEnvironment;
    use crate::cloud_provider_stripe_sponsored::{
        compose_sponsored_coverage, SponsoredAllocationInterval, SponsoredAllocationManifest,
        SponsoredCoverageResult, SponsoredInvoiceLine, SponsoredInvoiceSettlement,
        SponsoredStripeCoverageConfig,
    };

    fn config() -> SponsoredStripeCoverageConfig {
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

    fn candidate() -> SponsoredCoverageCandidate {
        let manifest = SponsoredAllocationManifest::new(
            "acct_test",
            ProviderEnvironment::Test,
            "cus_1",
            "sub_1",
            vec![
                SponsoredAllocationInterval::new(
                    "source_a",
                    "allocation_a",
                    "user_1",
                    "item_1",
                    "price_standard_month",
                    0,
                    None,
                )
                .unwrap(),
                SponsoredAllocationInterval::new(
                    "source_b",
                    "allocation_b",
                    "user_2",
                    "item_1",
                    "price_standard_month",
                    0,
                    None,
                )
                .unwrap(),
            ],
        )
        .unwrap();
        let settlement = SponsoredInvoiceSettlement::new(
            "in_1",
            "cus_1",
            "sub_1",
            100,
            200,
            "gbp",
            200,
            200,
            "evt_1",
            vec![SponsoredInvoiceLine::new(
                "line_1",
                "item_1",
                "price_standard_month",
                100,
                200,
                2,
                false,
            )
            .unwrap()],
        )
        .unwrap();
        let SponsoredCoverageResult::Candidate(candidate) =
            compose_sponsored_coverage(&config(), &manifest, &settlement).unwrap()
        else {
            panic!("expected sponsored candidate")
        };
        candidate
    }

    fn single_source_candidate(
        source_id: &str,
        allocation_reference: &str,
        beneficiary_id: &str,
        invoice_id: &str,
    ) -> SponsoredCoverageCandidate {
        let manifest = SponsoredAllocationManifest::new(
            "acct_test",
            ProviderEnvironment::Test,
            "cus_1",
            "sub_1",
            vec![SponsoredAllocationInterval::new(
                source_id,
                allocation_reference,
                beneficiary_id,
                "item_1",
                "price_standard_month",
                0,
                None,
            )
            .unwrap()],
        )
        .unwrap();
        let settlement = SponsoredInvoiceSettlement::new(
            invoice_id,
            "cus_1",
            "sub_1",
            100,
            200,
            "gbp",
            100,
            100,
            format!("event_{invoice_id}"),
            vec![SponsoredInvoiceLine::new(
                format!("line_{invoice_id}"),
                "item_1",
                "price_standard_month",
                100,
                200,
                1,
                false,
            )
            .unwrap()],
        )
        .unwrap();
        let SponsoredCoverageResult::Candidate(candidate) =
            compose_sponsored_coverage(&config(), &manifest, &settlement).unwrap()
        else {
            panic!("expected sponsored candidate")
        };
        candidate
    }

    fn ticket(source_id: &str, beneficiary_id: &str) -> CollectionTicket {
        CollectionTicket {
            beneficiary_id: beneficiary_id.into(),
            attempt_id: "attempt_1".into(),
            collection_epoch: 1,
            source_set_generation: 1,
            provider_invalidation_generation: Some(0),
            expected_projection_revision: None,
            source_bindings: vec![SourceBinding {
                beneficiary_id: beneficiary_id.into(),
                source_id: source_id.into(),
                provider_namespace: STRIPE_NAMESPACE.into(),
                external_allocation_reference: format!(
                    "allocation_{}",
                    source_id.strip_prefix("source_").unwrap_or(source_id)
                ),
                ownership_evidence_reference: "ownership".into(),
            }],
            status: CollectionStatus::Pending,
            completed_revision: None,
        }
    }

    fn multi_source_ticket() -> CollectionTicket {
        let mut ticket = ticket("source_a", "user_1");
        ticket.source_bindings.push(SourceBinding {
            beneficiary_id: "user_1".into(),
            source_id: "source_b".into(),
            provider_namespace: STRIPE_NAMESPACE.into(),
            external_allocation_reference: "allocation_b".into(),
            ownership_evidence_reference: "ownership".into(),
        });
        ticket
    }

    fn context() -> ProviderContext {
        ProviderContext::new(STRIPE_NAMESPACE, "acct_test", ProviderEnvironment::Test).unwrap()
    }

    #[test]
    fn grouped_candidate_is_reduced_to_the_ticket_beneficiary() {
        let collection =
            collection_for_ticket(&context(), &ticket("source_a", "user_1"), &[&candidate()])
                .unwrap();
        assert_eq!(collection.observations.len(), 1);
        let SourceObservation::Complete { source_id, .. } = &collection.observations[0] else {
            panic!("expected complete observation")
        };
        assert_eq!(source_id, "source_a");
        assert!(collection
            .aggregate_evidence_reference
            .starts_with("stripe-sponsored-collection-v1:"));
    }

    #[test]
    fn source_selection_uses_one_candidate_without_duplicate_observations() {
        let candidate = candidate();
        let mut selected = BTreeMap::new();
        selected.insert("source_a".into(), &candidate);
        let collection =
            collection_for_ticket_by_source(&context(), &ticket("source_a", "user_1"), &selected)
                .unwrap();
        assert_eq!(collection.observations.len(), 1);
    }

    #[test]
    fn sibling_source_cannot_be_published_into_the_ticket() {
        let error =
            collection_for_ticket(&context(), &ticket("source_b", "user_1"), &[&candidate()])
                .unwrap_err();
        assert_eq!(error, SponsoredAdapterError::SourceSetMismatch);
    }

    #[test]
    fn separate_invoice_candidates_can_cover_all_registered_sources() {
        let first = single_source_candidate("source_a", "allocation_a", "user_1", "in_a");
        let second = single_source_candidate("source_b", "allocation_b", "user_1", "in_b");
        let collection =
            collection_for_ticket(&context(), &multi_source_ticket(), &[&first, &second]).unwrap();
        assert_eq!(collection.observations.len(), 2);
        assert_eq!(
            collection
                .observations
                .iter()
                .map(observation_source_id)
                .collect::<Vec<_>>(),
            vec!["source_a", "source_b"]
        );
    }

    #[test]
    fn allocation_reference_must_match_the_ticket_binding() {
        let mut ticket = ticket("source_a", "user_1");
        ticket.source_bindings[0].external_allocation_reference = "allocation_forged".into();
        let error = collection_for_ticket(&context(), &ticket, &[&candidate()]).unwrap_err();
        assert_eq!(error, SponsoredAdapterError::AllocationReferenceMismatch);
    }

    #[test]
    fn wrong_context_fails_before_observations() {
        let mut wrong_context = context();
        wrong_context.account_id = "acct_other".into();
        assert_eq!(
            collection_for_ticket(
                &wrong_context,
                &ticket("source_a", "user_1"),
                &[&candidate()],
            )
            .unwrap_err(),
            SponsoredAdapterError::ContextMismatch
        );
    }

    #[test]
    fn impossible_reconciliation_ticket_values_are_rejected() {
        let mut invalid_epoch = ticket("source_a", "user_1");
        invalid_epoch.collection_epoch = 0;
        assert_eq!(
            collection_for_ticket(&context(), &invalid_epoch, &[&candidate()]).unwrap_err(),
            SponsoredAdapterError::InvalidTicket
        );

        let mut invalid_revision = ticket("source_a", "user_1");
        invalid_revision.expected_projection_revision = Some(0);
        assert_eq!(
            collection_for_ticket(&context(), &invalid_revision, &[&candidate()]).unwrap_err(),
            SponsoredAdapterError::InvalidTicket
        );
    }
}
