//! Authority boundary for a complete personal Stripe coverage observation.
//!
//! This module consumes facts that have already been authenticated and normalised by the Stripe
//! adapter. It does not read Stripe, write Postgres, or grant access by itself. The returned
//! authority is time and generation bound: a later provider invalidation or an expired freshness
//! window makes it unusable for publication and action checks.

#![allow(dead_code)]

use std::collections::BTreeMap;

use thiserror::Error;

use crate::cloud_coverage::{
    evaluate, ConfirmedPaidInterval, CoverageDecision, InvalidCoverage, PersonCoverage, Timestamp,
};

/// Version of the authority policy and its serialised contract.
pub const AUTHORITY_POLICY_VERSION: u16 = 1;

/// One registered provider source covered by an observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeAuthoritySource {
    pub(crate) source_id: String,
    pub(crate) generation: i64,
    pub(crate) evidence_reference: String,
}

/// Facts needed to turn a complete Stripe observation into a publication authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeAuthorityFacts {
    pub(crate) beneficiary_id: String,
    pub(crate) provider_account_id: String,
    pub(crate) environment: String,
    pub(crate) allocation_reference: String,
    pub(crate) complete_source_set: bool,
    pub(crate) sources: Vec<StripeAuthoritySource>,
    pub(crate) invalidation_generation: i64,
    pub(crate) observed_at: Timestamp,
    pub(crate) fresh_until: Timestamp,
    pub(crate) paid_terms: Vec<StripeAuthorityPaidTerm>,
    pub(crate) renewals: Vec<StripeAuthorityRenewal>,
    pub(crate) cancellation: StripeAuthorityCancellation,
    pub(crate) correction: StripeAuthorityCorrection,
}

/// A paid term identified by the provider invoice it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeAuthorityPaidTerm {
    pub(crate) coverage_id: String,
    pub(crate) source_id: String,
    pub(crate) invoice_id: String,
    pub(crate) starts_at: Timestamp,
    pub(crate) paid_until: Timestamp,
}

/// The current, already-normalised state of one renewal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeAuthorityRenewal {
    pub(crate) renewal_id: String,
    pub(crate) evidence_reference: String,
    pub(crate) predecessor_invoice_id: String,
    pub(crate) predecessor_period_end: Timestamp,
    pub(crate) renewal_period_start: Timestamp,
    pub(crate) automatic_cycle: bool,
    pub(crate) state: StripeAuthorityRenewalState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StripeAuthorityRenewalState {
    Paid,
    Open,
    ClosedUnpaid { status: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StripeAuthorityCancellation {
    None,
    Scheduled {
        effective_at: Timestamp,
    },
    Immediate {
        effective_at: Timestamp,
    },
    ConfirmedEarlyTermination {
        effective_at: Timestamp,
        evidence_reference: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StripeAuthorityCorrection {
    PaidTermPreserved,
    PartialRefund,
    OpenDispute,
    Unresolved,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StripeAuthorityAssessment {
    Ready(StripePublicationAuthority),
    NeedsEvidence(StripeAuthorityError),
    Unsupported(StripeAuthorityError),
}

/// A complete, freshness-bound authority for one beneficiary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripePublicationAuthority {
    policy_version: u16,
    beneficiary_id: String,
    provider_account_id: String,
    environment: String,
    allocation_reference: String,
    coverage: PersonCoverage,
    sources: Vec<StripeAuthoritySource>,
    invalidation_generation: i64,
    observed_at: Timestamp,
    fresh_until: Timestamp,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum StripeAuthorityError {
    #[error("Stripe coverage observation is incomplete")]
    IncompleteSourceSet,
    #[error("Stripe coverage authority has invalid {0}")]
    InvalidMetadata(&'static str),
    #[error("Stripe coverage source is invalid: {0}")]
    InvalidSource(&'static str),
    #[error("Stripe renewal {renewal_id} has no unique paid predecessor {invoice_id}")]
    MissingPredecessor {
        renewal_id: String,
        invoice_id: String,
    },
    #[error("Stripe renewal {renewal_id} has ambiguous paid predecessor {invoice_id}")]
    AmbiguousPredecessor {
        renewal_id: String,
        invoice_id: String,
    },
    #[error("Stripe renewal {renewal_id} has conflicting observations")]
    ConflictingRenewal { renewal_id: String },
    #[error("Stripe renewal {renewal_id} is not confirmed as an automatic cycle")]
    UnconfirmedRenewal { renewal_id: String },
    #[error("Stripe renewal {renewal_id} has a non-contiguous predecessor period")]
    NonContiguousRenewal { renewal_id: String },
    #[error("Stripe renewal {renewal_id} does not meet its paid predecessor boundary")]
    PredecessorBoundaryMismatch { renewal_id: String },
    #[error("Stripe renewal {renewal_id} has unsupported terminal status {status}")]
    UnsupportedTerminalStatus { renewal_id: String, status: String },
    #[error("Stripe cancellation evidence is incomplete")]
    IncompleteCancellationEvidence,
    #[error("Stripe correction state is unresolved")]
    UnresolvedCorrection,
    #[error("Stripe publication authority is stale")]
    Stale,
    #[error("Stripe coverage is invalid: {0}")]
    Coverage(#[from] InvalidCoverage),
}

impl StripePublicationAuthority {
    /// Build authority only from complete, already-authenticated adapter facts.
    pub(crate) fn from_facts(facts: StripeAuthorityFacts) -> Result<Self, StripeAuthorityError> {
        validate_metadata(&facts)?;
        validate_cancellation(&facts.cancellation)?;
        if matches!(facts.correction, StripeAuthorityCorrection::Unresolved) {
            return Err(StripeAuthorityError::UnresolvedCorrection);
        }
        let sources = normalise_sources(facts.sources)?;
        if !facts.complete_source_set || sources.is_empty() {
            return Err(StripeAuthorityError::IncompleteSourceSet);
        }

        let source_ids: std::collections::BTreeSet<_> = sources
            .iter()
            .map(|source| source.source_id.as_str())
            .collect();
        let mut paid_intervals = Vec::with_capacity(facts.paid_terms.len());
        let mut invoice_positions = BTreeMap::<String, Vec<usize>>::new();
        for term in facts.paid_terms {
            if term.coverage_id.trim().is_empty()
                || term.invoice_id.trim().is_empty()
                || !source_ids.contains(term.source_id.as_str())
            {
                return Err(StripeAuthorityError::InvalidSource("paid term provenance"));
            }
            let position = paid_intervals.len();
            invoice_positions
                .entry(term.invoice_id)
                .or_default()
                .push(position);
            paid_intervals.push(ConfirmedPaidInterval {
                coverage_id: term.coverage_id,
                source_id: term.source_id,
                starts_at: term.starts_at,
                paid_until: term.paid_until,
                failed_renewal_id: None,
            });
        }

        let mut renewals = BTreeMap::<String, StripeAuthorityRenewal>::new();
        for renewal in facts.renewals {
            if renewal.renewal_id.trim().is_empty()
                || renewal.predecessor_invoice_id.trim().is_empty()
                || renewal.evidence_reference.trim().is_empty()
            {
                return Err(StripeAuthorityError::InvalidMetadata("renewal identity"));
            }
            if renewal.predecessor_period_end != renewal.renewal_period_start {
                return Err(StripeAuthorityError::NonContiguousRenewal {
                    renewal_id: renewal.renewal_id,
                });
            }
            if let Some(previous) = renewals.get(&renewal.renewal_id) {
                if previous != &renewal {
                    return Err(StripeAuthorityError::ConflictingRenewal {
                        renewal_id: renewal.renewal_id,
                    });
                }
                continue;
            }
            let state = match &renewal.state {
                StripeAuthorityRenewalState::Paid => StripeAuthorityRenewalState::Paid,
                StripeAuthorityRenewalState::Open => StripeAuthorityRenewalState::Open,
                StripeAuthorityRenewalState::ClosedUnpaid { status }
                    if matches!(status.as_str(), "void" | "uncollectible") =>
                {
                    StripeAuthorityRenewalState::ClosedUnpaid {
                        status: status.clone(),
                    }
                }
                StripeAuthorityRenewalState::ClosedUnpaid { status } => {
                    return Err(StripeAuthorityError::UnsupportedTerminalStatus {
                        renewal_id: renewal.renewal_id,
                        status: status.clone(),
                    });
                }
            };
            renewals.insert(renewal.renewal_id.clone(), renewal.clone());
            if !matches!(state, StripeAuthorityRenewalState::Paid) {
                if !renewal.automatic_cycle {
                    return Err(StripeAuthorityError::UnconfirmedRenewal {
                        renewal_id: renewal.renewal_id,
                    });
                }
                let positions = invoice_positions
                    .get(&renewal.predecessor_invoice_id)
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                let position = match positions {
                    [] => {
                        return Err(StripeAuthorityError::MissingPredecessor {
                            renewal_id: renewal.renewal_id,
                            invoice_id: renewal.predecessor_invoice_id,
                        });
                    }
                    [position] => *position,
                    _ => {
                        return Err(StripeAuthorityError::AmbiguousPredecessor {
                            renewal_id: renewal.renewal_id,
                            invoice_id: renewal.predecessor_invoice_id,
                        });
                    }
                };
                if paid_intervals[position].paid_until != renewal.predecessor_period_end {
                    return Err(StripeAuthorityError::PredecessorBoundaryMismatch {
                        renewal_id: renewal.renewal_id,
                    });
                }
                if paid_intervals[position].failed_renewal_id.is_some() {
                    return Err(StripeAuthorityError::ConflictingRenewal {
                        renewal_id: renewal.renewal_id,
                    });
                }
                paid_intervals[position].failed_renewal_id = Some(renewal.renewal_id);
            }
        }

        let early_termination = match facts.cancellation {
            StripeAuthorityCancellation::ConfirmedEarlyTermination { effective_at, .. } => {
                Some(effective_at)
            }
            _ => None,
        };
        if matches!(facts.correction, StripeAuthorityCorrection::OpenDispute) {
            // An open dispute does not shorten a paid term, but it does keep the
            // observation provisional until the next bounded refresh.
        }
        if let Some(effective_at) = early_termination {
            for interval in &mut paid_intervals {
                let original_paid_until = interval.paid_until;
                interval.paid_until = original_paid_until.min(effective_at);
                if interval.paid_until != original_paid_until
                    || interval.paid_until <= interval.starts_at
                {
                    interval.failed_renewal_id = None;
                }
            }
        }
        let coverage = PersonCoverage {
            beneficiary_id: facts.beneficiary_id.clone(),
            paid_intervals,
        };
        crate::cloud_coverage::normalise_confirmed_intervals(&coverage)?;
        Ok(Self {
            policy_version: AUTHORITY_POLICY_VERSION,
            beneficiary_id: facts.beneficiary_id,
            provider_account_id: facts.provider_account_id,
            environment: facts.environment,
            allocation_reference: facts.allocation_reference,
            coverage,
            sources,
            invalidation_generation: facts.invalidation_generation,
            observed_at: facts.observed_at,
            fresh_until: facts.fresh_until,
        })
    }

    pub(crate) fn assess(facts: StripeAuthorityFacts) -> StripeAuthorityAssessment {
        match Self::from_facts(facts) {
            Ok(authority) => StripeAuthorityAssessment::Ready(authority),
            Err(error @ StripeAuthorityError::UnsupportedTerminalStatus { .. }) => {
                StripeAuthorityAssessment::Unsupported(error)
            }
            Err(error) => StripeAuthorityAssessment::NeedsEvidence(error),
        }
    }

    pub const fn policy_version(&self) -> u16 {
        self.policy_version
    }

    pub fn beneficiary_id(&self) -> &str {
        &self.beneficiary_id
    }

    pub fn provider_account_id(&self) -> &str {
        &self.provider_account_id
    }

    pub fn environment(&self) -> &str {
        &self.environment
    }

    pub fn allocation_reference(&self) -> &str {
        &self.allocation_reference
    }

    pub fn coverage(&self) -> &PersonCoverage {
        &self.coverage
    }

    pub fn sources(&self) -> &[StripeAuthoritySource] {
        &self.sources
    }

    pub const fn observed_at(&self) -> Timestamp {
        self.observed_at
    }

    pub const fn fresh_until(&self) -> Timestamp {
        self.fresh_until
    }

    pub const fn invalidation_generation(&self) -> i64 {
        self.invalidation_generation
    }

    /// Authority is usable only before its freshness deadline and at its captured fence.
    pub fn is_current(
        &self,
        now: Timestamp,
        invalidation_generation: i64,
        current_sources: &[StripeAuthoritySource],
    ) -> bool {
        let Ok(current_sources) = normalise_sources(current_sources.to_vec()) else {
            return false;
        };
        self.observed_at <= now
            && now < self.fresh_until
            && invalidation_generation == self.invalidation_generation
            && current_sources == self.sources
    }

    pub fn evaluate(
        &self,
        now: Timestamp,
        invalidation_generation: i64,
        current_sources: &[StripeAuthoritySource],
    ) -> Result<CoverageDecision, StripeAuthorityError> {
        if !self.is_current(now, invalidation_generation, current_sources) {
            return Err(StripeAuthorityError::Stale);
        }
        Ok(evaluate(&self.coverage, now)?)
    }
}

fn validate_metadata(facts: &StripeAuthorityFacts) -> Result<(), StripeAuthorityError> {
    if facts.beneficiary_id.trim().is_empty() {
        return Err(StripeAuthorityError::InvalidMetadata("beneficiary"));
    }
    if facts.provider_account_id.trim().is_empty() {
        return Err(StripeAuthorityError::InvalidMetadata("provider account"));
    }
    if facts.environment.trim().is_empty() {
        return Err(StripeAuthorityError::InvalidMetadata("environment"));
    }
    if facts.allocation_reference.trim().is_empty() {
        return Err(StripeAuthorityError::InvalidMetadata(
            "allocation reference",
        ));
    }
    if facts.invalidation_generation < 0 {
        return Err(StripeAuthorityError::InvalidMetadata(
            "invalidation generation",
        ));
    }
    if facts.observed_at < 0 || facts.fresh_until <= facts.observed_at {
        return Err(StripeAuthorityError::InvalidMetadata("freshness window"));
    }
    Ok(())
}

fn validate_cancellation(
    cancellation: &StripeAuthorityCancellation,
) -> Result<(), StripeAuthorityError> {
    if let StripeAuthorityCancellation::ConfirmedEarlyTermination {
        evidence_reference, ..
    } = cancellation
    {
        if evidence_reference.trim().is_empty() {
            return Err(StripeAuthorityError::IncompleteCancellationEvidence);
        }
    }
    Ok(())
}

fn normalise_sources(
    mut sources: Vec<StripeAuthoritySource>,
) -> Result<Vec<StripeAuthoritySource>, StripeAuthorityError> {
    sources.sort_by(|left, right| left.source_id.cmp(&right.source_id));
    for source in &sources {
        if source.source_id.trim().is_empty() {
            return Err(StripeAuthorityError::InvalidSource("source id"));
        }
        if source.generation <= 0 {
            return Err(StripeAuthorityError::InvalidSource("source generation"));
        }
        if source.evidence_reference.trim().is_empty() {
            return Err(StripeAuthorityError::InvalidSource("source evidence"));
        }
    }
    if sources
        .windows(2)
        .any(|window| window[0].source_id == window[1].source_id)
    {
        return Err(StripeAuthorityError::InvalidSource("duplicate source"));
    }
    Ok(sources)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud_coverage::CoverageState;

    fn source(id: &str) -> StripeAuthoritySource {
        StripeAuthoritySource {
            source_id: id.into(),
            generation: 1,
            evidence_reference: format!("evidence:{id}"),
        }
    }

    fn paid_term(invoice_id: &str) -> StripeAuthorityPaidTerm {
        StripeAuthorityPaidTerm {
            coverage_id: format!("coverage:{invoice_id}"),
            source_id: "source-a".into(),
            invoice_id: invoice_id.into(),
            starts_at: 0,
            paid_until: 100,
        }
    }

    fn facts(renewals: Vec<StripeAuthorityRenewal>) -> StripeAuthorityFacts {
        StripeAuthorityFacts {
            beneficiary_id: "beneficiary".into(),
            provider_account_id: "acct_test".into(),
            environment: "test".into(),
            allocation_reference: "allocation:beneficiary".into(),
            complete_source_set: true,
            sources: vec![source("source-a")],
            invalidation_generation: 1,
            observed_at: 10,
            fresh_until: 1_000,
            paid_terms: vec![paid_term("invoice-paid")],
            renewals,
            cancellation: StripeAuthorityCancellation::None,
            correction: StripeAuthorityCorrection::PaidTermPreserved,
        }
    }

    #[test]
    fn paid_term_is_authoritative_until_freshness_or_generation_changes() {
        let authority = StripePublicationAuthority::from_facts(facts(vec![])).unwrap();
        let sources = vec![source("source-a")];
        assert_eq!(authority.policy_version(), AUTHORITY_POLICY_VERSION);
        assert_eq!(
            authority.evaluate(50, 1, &sources).unwrap().state,
            CoverageState::Paid
        );
        assert!(!authority.is_current(1_000, 1, &sources));
        assert!(matches!(
            authority.evaluate(100, 2, &sources),
            Err(StripeAuthorityError::Stale)
        ));
    }

    #[test]
    fn open_renewal_attaches_recovery_to_its_exact_predecessor() {
        let authority =
            StripePublicationAuthority::from_facts(facts(vec![StripeAuthorityRenewal {
                renewal_id: "renewal-1".into(),
                evidence_reference: "event:renewal-1".into(),
                predecessor_invoice_id: "invoice-paid".into(),
                predecessor_period_end: 100,
                renewal_period_start: 100,
                automatic_cycle: true,
                state: StripeAuthorityRenewalState::Open,
            }]))
            .unwrap();
        assert_eq!(
            authority.coverage().paid_intervals[0]
                .failed_renewal_id
                .as_deref(),
            Some("renewal-1")
        );
        assert_eq!(
            authority
                .evaluate(100, 1, &[source("source-a")])
                .unwrap()
                .state,
            CoverageState::RenewalRecovery
        );
    }

    #[test]
    fn known_closed_unpaid_statuses_preserve_recovery() {
        for status in ["void", "uncollectible"] {
            let authority =
                StripePublicationAuthority::from_facts(facts(vec![StripeAuthorityRenewal {
                    renewal_id: format!("renewal-{status}"),
                    evidence_reference: format!("event:renewal-{status}"),
                    predecessor_invoice_id: "invoice-paid".into(),
                    predecessor_period_end: 100,
                    renewal_period_start: 100,
                    automatic_cycle: true,
                    state: StripeAuthorityRenewalState::ClosedUnpaid {
                        status: status.into(),
                    },
                }]))
                .unwrap();
            assert_eq!(
                authority
                    .evaluate(100, 1, &[source("source-a")])
                    .unwrap()
                    .state,
                CoverageState::RenewalRecovery
            );
        }
    }

    #[test]
    fn unknown_terminal_status_and_missing_predecessor_fail_closed() {
        let mut unknown = facts(vec![StripeAuthorityRenewal {
            renewal_id: "renewal-1".into(),
            evidence_reference: "event:renewal-1".into(),
            predecessor_invoice_id: "invoice-paid".into(),
            predecessor_period_end: 100,
            renewal_period_start: 100,
            automatic_cycle: true,
            state: StripeAuthorityRenewalState::ClosedUnpaid {
                status: "pending".into(),
            },
        }]);
        assert!(matches!(
            StripePublicationAuthority::from_facts(unknown),
            Err(StripeAuthorityError::UnsupportedTerminalStatus { .. })
        ));
        unknown = facts(vec![StripeAuthorityRenewal {
            renewal_id: "renewal-1".into(),
            evidence_reference: "event:renewal-1".into(),
            predecessor_invoice_id: "missing".into(),
            predecessor_period_end: 100,
            renewal_period_start: 100,
            automatic_cycle: true,
            state: StripeAuthorityRenewalState::Open,
        }]);
        assert!(matches!(
            StripePublicationAuthority::from_facts(unknown),
            Err(StripeAuthorityError::MissingPredecessor { .. })
        ));
    }

    #[test]
    fn incomplete_or_duplicate_sources_cannot_become_authority() {
        let mut incomplete = facts(vec![]);
        incomplete.complete_source_set = false;
        assert!(matches!(
            StripePublicationAuthority::from_facts(incomplete),
            Err(StripeAuthorityError::IncompleteSourceSet)
        ));
        let mut duplicate = facts(vec![]);
        duplicate.sources.push(source("source-a"));
        assert!(matches!(
            StripePublicationAuthority::from_facts(duplicate),
            Err(StripeAuthorityError::InvalidSource("duplicate source"))
        ));
    }

    #[test]
    fn changed_source_generation_invalidates_authority() {
        let authority = StripePublicationAuthority::from_facts(facts(vec![])).unwrap();
        let mut changed = source("source-a");
        changed.generation = 2;
        assert!(!authority.is_current(50, 1, &[changed]));

        let mut baseline = facts(vec![]);
        baseline.invalidation_generation = 0;
        assert!(StripePublicationAuthority::from_facts(baseline).is_ok());
    }

    #[test]
    fn unconfirmed_or_non_contiguous_renewals_fail_closed() {
        let mut unconfirmed = facts(vec![StripeAuthorityRenewal {
            renewal_id: "renewal-1".into(),
            evidence_reference: "event:renewal-1".into(),
            predecessor_invoice_id: "invoice-paid".into(),
            predecessor_period_end: 100,
            renewal_period_start: 100,
            automatic_cycle: false,
            state: StripeAuthorityRenewalState::Open,
        }]);
        assert!(matches!(
            StripePublicationAuthority::from_facts(unconfirmed.clone()),
            Err(StripeAuthorityError::UnconfirmedRenewal { .. })
        ));
        unconfirmed.renewals[0].automatic_cycle = true;
        unconfirmed.renewals[0].renewal_period_start = 101;
        assert!(matches!(
            StripePublicationAuthority::from_facts(unconfirmed),
            Err(StripeAuthorityError::NonContiguousRenewal { .. })
        ));

        let mut mismatch = facts(vec![StripeAuthorityRenewal {
            renewal_id: "renewal-2".into(),
            evidence_reference: "event:renewal-2".into(),
            predecessor_invoice_id: "invoice-paid".into(),
            predecessor_period_end: 99,
            renewal_period_start: 99,
            automatic_cycle: true,
            state: StripeAuthorityRenewalState::Open,
        }]);
        assert!(matches!(
            StripePublicationAuthority::from_facts(mismatch.clone()),
            Err(StripeAuthorityError::PredecessorBoundaryMismatch { .. })
        ));
        mismatch.paid_terms[0].paid_until = 99;
        assert!(StripePublicationAuthority::from_facts(mismatch).is_ok());
    }

    #[test]
    fn refunds_and_disputes_preserve_paid_term_but_confirmed_termination_truncates_it() {
        for correction in [
            StripeAuthorityCorrection::PartialRefund,
            StripeAuthorityCorrection::OpenDispute,
        ] {
            let mut input = facts(vec![]);
            input.correction = correction;
            let authority = StripePublicationAuthority::from_facts(input).unwrap();
            assert_eq!(authority.coverage().paid_intervals[0].paid_until, 100);
        }

        let mut input = facts(vec![]);
        input.cancellation = StripeAuthorityCancellation::ConfirmedEarlyTermination {
            effective_at: 50,
            evidence_reference: "event:termination".into(),
        };
        let authority = StripePublicationAuthority::from_facts(input).unwrap();
        assert_eq!(authority.coverage().paid_intervals[0].paid_until, 50);

        let mut input = facts(vec![StripeAuthorityRenewal {
            renewal_id: "renewal-terminated".into(),
            evidence_reference: "event:renewal-terminated".into(),
            predecessor_invoice_id: "invoice-paid".into(),
            predecessor_period_end: 100,
            renewal_period_start: 100,
            automatic_cycle: true,
            state: StripeAuthorityRenewalState::Open,
        }]);
        input.cancellation = StripeAuthorityCancellation::ConfirmedEarlyTermination {
            effective_at: 50,
            evidence_reference: "event:termination".into(),
        };
        let authority = StripePublicationAuthority::from_facts(input).unwrap();
        assert_eq!(
            authority.coverage().paid_intervals[0].failed_renewal_id,
            None
        );
    }

    #[test]
    fn missing_cancellation_or_correction_evidence_is_not_ready() {
        let mut input = facts(vec![]);
        input.cancellation = StripeAuthorityCancellation::ConfirmedEarlyTermination {
            effective_at: 50,
            evidence_reference: String::new(),
        };
        assert!(matches!(
            StripePublicationAuthority::assess(input),
            StripeAuthorityAssessment::NeedsEvidence(
                StripeAuthorityError::IncompleteCancellationEvidence
            )
        ));

        let mut input = facts(vec![]);
        input.correction = StripeAuthorityCorrection::Unresolved;
        assert!(matches!(
            StripePublicationAuthority::assess(input),
            StripeAuthorityAssessment::NeedsEvidence(StripeAuthorityError::UnresolvedCorrection)
        ));
    }
}
