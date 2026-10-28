//! Sponsored Stripe coverage normalisation.
//!
//! Sponsored subscriptions are a different evidence shape from personal subscriptions: one
//! invoice line can pay for several named beneficiaries and a beneficiary can move between
//! price classes over time.  This module consumes already authenticated, normalised facts.  It
//! never reads Stripe or Postgres and it deliberately does not relax the personal invoice
//! validator.

use std::collections::BTreeMap;

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::cloud_coverage::{ConfirmedPaidInterval, PersonCoverage};
use crate::cloud_coverage_reconciliation::SourceObservation;
use crate::cloud_provider::{PayerKind, ProviderEnvironment};
use crate::cloud_provider_stripe_renewals::StripeRenewalFailureEvidence;

const SEMANTIC_DOMAIN: &[u8] = b"sotto-stripe-sponsored-coverage-v1\0";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SponsoredInterval {
    Month,
    Year,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SponsoredPriceClass {
    StandardMonthly,
    StandardAnnual,
    FoundingMonthly,
    FoundingAnnual,
}

impl SponsoredPriceClass {
    pub const fn interval(self) -> SponsoredInterval {
        match self {
            Self::StandardMonthly | Self::FoundingMonthly => SponsoredInterval::Month,
            Self::StandardAnnual | Self::FoundingAnnual => SponsoredInterval::Year,
        }
    }

    pub const fn is_founding(self) -> bool {
        matches!(self, Self::FoundingMonthly | Self::FoundingAnnual)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SponsoredStripeCoverageConfig {
    account_id: String,
    environment: ProviderEnvironment,
    standard_monthly_price_id: String,
    standard_annual_price_id: String,
    founding_monthly_price_id: String,
    founding_annual_price_id: String,
}

impl SponsoredStripeCoverageConfig {
    pub fn new(
        account_id: impl Into<String>,
        environment: ProviderEnvironment,
        standard_monthly_price_id: impl Into<String>,
        standard_annual_price_id: impl Into<String>,
        founding_monthly_price_id: impl Into<String>,
        founding_annual_price_id: impl Into<String>,
    ) -> Result<Self, SponsoredCoverageError> {
        let config = Self {
            account_id: account_id.into(),
            environment,
            standard_monthly_price_id: standard_monthly_price_id.into(),
            standard_annual_price_id: standard_annual_price_id.into(),
            founding_monthly_price_id: founding_monthly_price_id.into(),
            founding_annual_price_id: founding_annual_price_id.into(),
        };
        config.validate()?;
        Ok(config)
    }

    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    pub const fn environment(&self) -> ProviderEnvironment {
        self.environment
    }

    fn validate(&self) -> Result<(), SponsoredCoverageError> {
        let values = [
            (&self.account_id, "account"),
            (&self.standard_monthly_price_id, "standard monthly price"),
            (&self.standard_annual_price_id, "standard annual price"),
            (&self.founding_monthly_price_id, "founding monthly price"),
            (&self.founding_annual_price_id, "founding annual price"),
        ];
        for (value, name) in values {
            if value.trim().is_empty() {
                return Err(SponsoredCoverageError::InvalidInput(name));
            }
        }
        let ids = [
            &self.standard_monthly_price_id,
            &self.standard_annual_price_id,
            &self.founding_monthly_price_id,
            &self.founding_annual_price_id,
        ];
        for (index, id) in ids.iter().enumerate() {
            if ids[..index].contains(id) {
                return Err(SponsoredCoverageError::InvalidInput(
                    "price ids must differ",
                ));
            }
        }
        Ok(())
    }

    fn price_class(&self, price_id: &str) -> Option<SponsoredPriceClass> {
        [
            (
                self.standard_monthly_price_id.as_str(),
                SponsoredPriceClass::StandardMonthly,
            ),
            (
                self.standard_annual_price_id.as_str(),
                SponsoredPriceClass::StandardAnnual,
            ),
            (
                self.founding_monthly_price_id.as_str(),
                SponsoredPriceClass::FoundingMonthly,
            ),
            (
                self.founding_annual_price_id.as_str(),
                SponsoredPriceClass::FoundingAnnual,
            ),
        ]
        .into_iter()
        .find_map(|(id, class)| (id == price_id).then_some(class))
    }

    pub(crate) fn stripe_interval_for_price(
        &self,
        price_id: &str,
    ) -> Option<crate::cloud_provider_stripe::StripeInterval> {
        self.price_class(price_id)
            .map(|price_class| match price_class.interval() {
                SponsoredInterval::Month => crate::cloud_provider_stripe::StripeInterval::Month,
                SponsoredInterval::Year => crate::cloud_provider_stripe::StripeInterval::Year,
            })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SponsoredAllocationInterval {
    source_id: String,
    allocation_reference: String,
    beneficiary_id: String,
    provider_item_id: String,
    price_id: String,
    effective_from: i64,
    effective_until: Option<i64>,
}

impl SponsoredAllocationInterval {
    pub fn new(
        source_id: impl Into<String>,
        allocation_reference: impl Into<String>,
        beneficiary_id: impl Into<String>,
        provider_item_id: impl Into<String>,
        price_id: impl Into<String>,
        effective_from: i64,
        effective_until: Option<i64>,
    ) -> Result<Self, SponsoredCoverageError> {
        let interval = Self {
            source_id: source_id.into(),
            allocation_reference: allocation_reference.into(),
            beneficiary_id: beneficiary_id.into(),
            provider_item_id: provider_item_id.into(),
            price_id: price_id.into(),
            effective_from,
            effective_until,
        };
        interval.validate()?;
        Ok(interval)
    }

    pub fn allocation_reference(&self) -> &str {
        &self.allocation_reference
    }

    pub fn source_id(&self) -> &str {
        &self.source_id
    }

    pub fn beneficiary_id(&self) -> &str {
        &self.beneficiary_id
    }

    pub fn provider_item_id(&self) -> &str {
        &self.provider_item_id
    }

    pub fn price_id(&self) -> &str {
        &self.price_id
    }

    pub const fn effective_from(&self) -> i64 {
        self.effective_from
    }

    pub const fn effective_until(&self) -> Option<i64> {
        self.effective_until
    }

    fn validate(&self) -> Result<(), SponsoredCoverageError> {
        for (value, name) in [
            (&self.source_id, "source"),
            (&self.allocation_reference, "allocation reference"),
            (&self.beneficiary_id, "beneficiary"),
            (&self.provider_item_id, "provider item"),
            (&self.price_id, "price"),
        ] {
            if value.trim().is_empty() {
                return Err(SponsoredCoverageError::InvalidInput(name));
            }
        }
        if self.effective_from < 0
            || self
                .effective_until
                .is_some_and(|until| until <= self.effective_from)
        {
            return Err(SponsoredCoverageError::InvalidInput("allocation interval"));
        }
        Ok(())
    }

    fn contains(&self, start: i64, end: i64) -> bool {
        self.effective_from <= start && self.effective_until.is_none_or(|until| end <= until)
    }

    fn overlaps(&self, start: i64, end: i64) -> bool {
        self.effective_from < end && self.effective_until.is_none_or(|until| start < until)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SponsoredAllocationManifest {
    account_id: String,
    environment: ProviderEnvironment,
    customer_id: String,
    subscription_id: String,
    allocations: Vec<SponsoredAllocationInterval>,
}

impl SponsoredAllocationManifest {
    pub fn new(
        account_id: impl Into<String>,
        environment: ProviderEnvironment,
        customer_id: impl Into<String>,
        subscription_id: impl Into<String>,
        allocations: Vec<SponsoredAllocationInterval>,
    ) -> Result<Self, SponsoredCoverageError> {
        let manifest = Self {
            account_id: account_id.into(),
            environment,
            customer_id: customer_id.into(),
            subscription_id: subscription_id.into(),
            allocations,
        };
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    pub const fn environment(&self) -> ProviderEnvironment {
        self.environment
    }

    pub fn customer_id(&self) -> &str {
        &self.customer_id
    }

    pub fn subscription_id(&self) -> &str {
        &self.subscription_id
    }

    pub fn allocations(&self) -> &[SponsoredAllocationInterval] {
        &self.allocations
    }

    fn validate(&self) -> Result<(), SponsoredCoverageError> {
        for (value, name) in [
            (&self.account_id, "account"),
            (&self.customer_id, "customer"),
            (&self.subscription_id, "subscription"),
        ] {
            if value.trim().is_empty() {
                return Err(SponsoredCoverageError::InvalidInput(name));
            }
        }
        for (index, allocation) in self.allocations.iter().enumerate() {
            allocation.validate()?;
            if self.allocations[..index]
                .iter()
                .any(|previous| previous.source_id == allocation.source_id)
            {
                return Err(SponsoredCoverageError::ConflictingSourceReference {
                    source_id: allocation.source_id.clone(),
                });
            }
            if self.allocations[..index].iter().any(|previous| {
                previous.allocation_reference == allocation.allocation_reference
                    && previous.overlaps(
                        allocation.effective_from,
                        allocation.effective_until.unwrap_or(i64::MAX),
                    )
            }) {
                return Err(SponsoredCoverageError::ConflictingAllocationReference {
                    allocation_reference: allocation.allocation_reference.clone(),
                });
            }
            if self.allocations[..index].iter().any(|previous| {
                previous.beneficiary_id == allocation.beneficiary_id
                    && previous.overlaps(
                        allocation.effective_from,
                        allocation.effective_until.unwrap_or(i64::MAX),
                    )
            }) {
                return Err(SponsoredCoverageError::ConcurrentAllocation {
                    beneficiary_id: allocation.beneficiary_id.clone(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SponsoredInvoiceLine {
    line_id: String,
    provider_item_id: String,
    price_id: String,
    period_start: i64,
    period_end: i64,
    quantity: u64,
    amount: i64,
    proration: bool,
}

impl SponsoredInvoiceLine {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        line_id: impl Into<String>,
        provider_item_id: impl Into<String>,
        price_id: impl Into<String>,
        period_start: i64,
        period_end: i64,
        quantity: u64,
        proration: bool,
    ) -> Result<Self, SponsoredCoverageError> {
        Self::new_with_amount(
            line_id,
            provider_item_id,
            price_id,
            period_start,
            period_end,
            quantity,
            1,
            proration,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with_amount(
        line_id: impl Into<String>,
        provider_item_id: impl Into<String>,
        price_id: impl Into<String>,
        period_start: i64,
        period_end: i64,
        quantity: u64,
        amount: i64,
        proration: bool,
    ) -> Result<Self, SponsoredCoverageError> {
        let line = Self {
            line_id: line_id.into(),
            provider_item_id: provider_item_id.into(),
            price_id: price_id.into(),
            period_start,
            period_end,
            quantity,
            amount,
            proration,
        };
        if line.line_id.trim().is_empty()
            || line.provider_item_id.trim().is_empty()
            || line.price_id.trim().is_empty()
            || line.period_start < 0
            || line.period_end <= line.period_start
            || line.quantity == 0
        {
            return Err(SponsoredCoverageError::InvalidInput("invoice line"));
        }
        Ok(line)
    }

    pub fn line_id(&self) -> &str {
        &self.line_id
    }

    pub fn provider_item_id(&self) -> &str {
        &self.provider_item_id
    }

    pub fn price_id(&self) -> &str {
        &self.price_id
    }

    pub const fn period_start(&self) -> i64 {
        self.period_start
    }

    pub const fn period_end(&self) -> i64 {
        self.period_end
    }

    pub const fn quantity(&self) -> u64 {
        self.quantity
    }

    pub const fn amount(&self) -> i64 {
        self.amount
    }

    pub const fn proration(&self) -> bool {
        self.proration
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SponsoredInvoiceSettlement {
    invoice_id: String,
    customer_id: String,
    subscription_id: String,
    period_start: i64,
    period_end: i64,
    currency: String,
    amount_due: i64,
    amount_paid: i64,
    payment_intent_id: String,
    evidence_reference: String,
    lines: Vec<SponsoredInvoiceLine>,
}

impl SponsoredInvoiceSettlement {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        invoice_id: impl Into<String>,
        customer_id: impl Into<String>,
        subscription_id: impl Into<String>,
        period_start: i64,
        period_end: i64,
        currency: impl Into<String>,
        amount_due: i64,
        amount_paid: i64,
        payment_intent_id: impl Into<String>,
        evidence_reference: impl Into<String>,
        lines: Vec<SponsoredInvoiceLine>,
    ) -> Result<Self, SponsoredCoverageError> {
        let settlement = Self {
            invoice_id: invoice_id.into(),
            customer_id: customer_id.into(),
            subscription_id: subscription_id.into(),
            period_start,
            period_end,
            currency: currency.into(),
            amount_due,
            amount_paid,
            payment_intent_id: payment_intent_id.into(),
            evidence_reference: evidence_reference.into(),
            lines,
        };
        settlement.validate()?;
        Ok(settlement)
    }

    pub fn invoice_id(&self) -> &str {
        &self.invoice_id
    }

    pub fn evidence_reference(&self) -> &str {
        &self.evidence_reference
    }

    pub fn payment_intent_id(&self) -> &str {
        &self.payment_intent_id
    }

    pub fn lines(&self) -> &[SponsoredInvoiceLine] {
        &self.lines
    }

    pub const fn period_start(&self) -> i64 {
        self.period_start
    }

    pub const fn period_end(&self) -> i64 {
        self.period_end
    }

    fn validate(&self) -> Result<(), SponsoredCoverageError> {
        for (value, name) in [
            (&self.invoice_id, "invoice"),
            (&self.customer_id, "customer"),
            (&self.subscription_id, "subscription"),
            (&self.currency, "currency"),
            (&self.payment_intent_id, "payment intent"),
            (&self.evidence_reference, "evidence reference"),
        ] {
            if value.trim().is_empty() {
                return Err(SponsoredCoverageError::InvalidInput(name));
            }
        }
        if self.period_start < 0 || self.period_end <= self.period_start {
            return Err(SponsoredCoverageError::InvalidInput("invoice period"));
        }
        if self.amount_due <= 0 || self.amount_paid < 0 {
            return Err(SponsoredCoverageError::InvalidInput("invoice amounts"));
        }
        if self.lines.is_empty() {
            return Err(SponsoredCoverageError::InvalidInput("invoice lines"));
        }
        for (index, line) in self.lines.iter().enumerate() {
            if self.lines[..index]
                .iter()
                .any(|previous| previous.line_id == line.line_id)
            {
                return Err(SponsoredCoverageError::InvalidInput(
                    "invoice line ids must differ",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SponsoredBeneficiaryCoverage {
    beneficiary_id: String,
    paid_terms: Vec<SponsoredBeneficiaryPaidTerm>,
}

impl SponsoredBeneficiaryCoverage {
    pub fn beneficiary_id(&self) -> &str {
        &self.beneficiary_id
    }

    pub fn paid_terms(&self) -> &[SponsoredBeneficiaryPaidTerm] {
        &self.paid_terms
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SponsoredBeneficiaryPaidTerm {
    allocation_reference: String,
    price_class: SponsoredPriceClass,
    interval: ConfirmedPaidInterval,
}

impl SponsoredBeneficiaryPaidTerm {
    pub fn allocation_reference(&self) -> &str {
        &self.allocation_reference
    }

    pub const fn price_class(&self) -> SponsoredPriceClass {
        self.price_class
    }

    pub const fn interval(&self) -> &ConfirmedPaidInterval {
        &self.interval
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SponsoredCoverageCandidate {
    account_id: String,
    environment: ProviderEnvironment,
    customer_id: String,
    subscription_id: String,
    payer_kind: PayerKind,
    beneficiaries: Vec<SponsoredBeneficiaryCoverage>,
    semantic_reference: String,
}

impl SponsoredCoverageCandidate {
    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    pub const fn environment(&self) -> ProviderEnvironment {
        self.environment
    }

    pub fn customer_id(&self) -> &str {
        &self.customer_id
    }

    pub fn subscription_id(&self) -> &str {
        &self.subscription_id
    }

    pub const fn payer_kind(&self) -> PayerKind {
        self.payer_kind
    }

    pub fn beneficiaries(&self) -> &[SponsoredBeneficiaryCoverage] {
        &self.beneficiaries
    }

    pub fn semantic_reference(&self) -> &str {
        &self.semantic_reference
    }

    /// Convert the complete sponsored snapshot into the provider-neutral per-person input used
    /// by the coverage evaluator. The allocation source id remains attached to every interval.
    pub fn person_coverages(&self) -> Vec<PersonCoverage> {
        self.beneficiaries
            .iter()
            .map(|beneficiary| PersonCoverage {
                beneficiary_id: beneficiary.beneficiary_id.clone(),
                paid_intervals: beneficiary
                    .paid_terms
                    .iter()
                    .map(|term| term.interval.clone())
                    .collect(),
            })
            .collect()
    }

    /// Group the complete snapshot into the source observations expected by reconciliation.
    /// Every source carries only its own named beneficiary's intervals.
    pub fn source_observations(&self) -> Vec<SourceObservation> {
        let mut grouped = BTreeMap::<String, Vec<ConfirmedPaidInterval>>::new();
        for beneficiary in &self.beneficiaries {
            for term in &beneficiary.paid_terms {
                grouped
                    .entry(term.interval.source_id.clone())
                    .or_default()
                    .push(term.interval.clone());
            }
        }
        grouped
            .into_iter()
            .map(|(source_id, mut paid_intervals)| {
                paid_intervals.sort_by_key(|interval| {
                    (
                        interval.starts_at,
                        interval.paid_until,
                        interval.coverage_id.clone(),
                    )
                });
                SourceObservation::Complete {
                    evidence_reference: format!("{}:{source_id}", self.semantic_reference),
                    source_id,
                    paid_intervals,
                }
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SponsoredCoverageResult {
    Candidate(SponsoredCoverageCandidate),
    NeedsEvidence(SponsoredNeedsEvidence),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SponsoredNeedsEvidence {
    PartialPayment {
        invoice_id: String,
    },
    UnknownPrice {
        price_id: String,
    },
    MissingAllocationHistory {
        provider_item_id: String,
    },
    QuantityMismatch {
        provider_item_id: String,
        expected: u64,
        observed: u64,
    },
    NonPositiveLineAmount {
        line_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SponsoredCoverageError {
    #[error("sponsored Stripe coverage input is invalid: {0}")]
    InvalidInput(&'static str),
    #[error("sponsored Stripe coverage context does not match {field}")]
    ContextMismatch { field: &'static str },
    #[error("beneficiary has overlapping allocation intervals: {beneficiary_id}")]
    ConcurrentAllocation { beneficiary_id: String },
    #[error("allocation reference is reused over an overlapping interval: {allocation_reference}")]
    ConflictingAllocationReference { allocation_reference: String },
    #[error("source is reused over an overlapping interval: {source_id}")]
    ConflictingSourceReference { source_id: String },
}

/// Convert one fully settled sponsored invoice into beneficiary-scoped paid intervals.
///
/// The manifest is treated as a dated, named allocation history.  Every point in every paid line
/// must have exactly the line quantity of named allocations.  A line with an unknown price,
/// missing history, a quantity mismatch, or a non-positive line amount is returned as
/// `NeedsEvidence`; it is never converted into an optimistic partial projection. Positive
/// prorations are valid when Stripe supplies a concrete paid period for that line.
pub fn compose_sponsored_coverage(
    config: &SponsoredStripeCoverageConfig,
    manifest: &SponsoredAllocationManifest,
    settlement: &SponsoredInvoiceSettlement,
) -> Result<SponsoredCoverageResult, SponsoredCoverageError> {
    compose_sponsored_coverage_with_renewal_failures(config, manifest, settlement, &[])
}

/// Compose paid sponsored terms with only the renewal failures linked to that exact predecessor
/// invoice and allocation. The failure never changes the paid interval itself; the shared
/// eligibility policy derives the existing recovery window from its stable renewal identity.
pub fn compose_sponsored_coverage_with_renewal_failures(
    config: &SponsoredStripeCoverageConfig,
    manifest: &SponsoredAllocationManifest,
    settlement: &SponsoredInvoiceSettlement,
    renewal_failures: &[StripeRenewalFailureEvidence],
) -> Result<SponsoredCoverageResult, SponsoredCoverageError> {
    config.validate()?;
    settlement.validate()?;
    check_context(config, manifest, settlement)?;
    if settlement.amount_paid != settlement.amount_due {
        return Ok(SponsoredCoverageResult::NeedsEvidence(
            SponsoredNeedsEvidence::PartialPayment {
                invoice_id: settlement.invoice_id.clone(),
            },
        ));
    }

    let mut beneficiaries = Vec::<SponsoredBeneficiaryCoverage>::new();
    for line in settlement.lines() {
        let Some(price_class) = config.price_class(line.price_id()) else {
            return Ok(SponsoredCoverageResult::NeedsEvidence(
                SponsoredNeedsEvidence::UnknownPrice {
                    price_id: line.price_id().to_owned(),
                },
            ));
        };
        if line.amount() <= 0 {
            return Ok(SponsoredCoverageResult::NeedsEvidence(
                SponsoredNeedsEvidence::NonPositiveLineAmount {
                    line_id: line.line_id().to_owned(),
                },
            ));
        }
        // A subscription can contain monthly and annual items. Stripe gives each invoice
        // line its own paid term, so comparing every line to the invoice-wide envelope would
        // reject valid mixed-interval invoices (and withhold coverage for all seats on them).
        if line.period_start() < settlement.period_start()
            || line.period_end() > settlement.period_end()
        {
            return Ok(SponsoredCoverageResult::NeedsEvidence(
                SponsoredNeedsEvidence::MissingAllocationHistory {
                    provider_item_id: line.provider_item_id().to_owned(),
                },
            ));
        }

        let candidates: Vec<&SponsoredAllocationInterval> = manifest
            .allocations()
            .iter()
            .filter(|allocation| {
                allocation.provider_item_id() == line.provider_item_id()
                    && allocation.price_id() == line.price_id()
                    && allocation.overlaps(line.period_start(), line.period_end())
            })
            .collect();
        if candidates.is_empty() {
            return Ok(SponsoredCoverageResult::NeedsEvidence(
                SponsoredNeedsEvidence::MissingAllocationHistory {
                    provider_item_id: line.provider_item_id().to_owned(),
                },
            ));
        }

        let mut boundaries = vec![line.period_start(), line.period_end()];
        for allocation in &candidates {
            boundaries.push(allocation.effective_from().max(line.period_start()));
            if let Some(until) = allocation.effective_until() {
                boundaries.push(until.min(line.period_end()));
            }
        }
        boundaries.sort_unstable();
        boundaries.dedup();
        for window in boundaries.windows(2) {
            let (start, end) = (window[0], window[1]);
            if start >= end {
                continue;
            }
            let active: Vec<&SponsoredAllocationInterval> = candidates
                .iter()
                .copied()
                .filter(|allocation| allocation.contains(start, end))
                .collect();
            let observed = active.len() as u64;
            if observed != line.quantity() {
                return Ok(SponsoredCoverageResult::NeedsEvidence(
                    SponsoredNeedsEvidence::QuantityMismatch {
                        provider_item_id: line.provider_item_id().to_owned(),
                        expected: line.quantity(),
                        observed,
                    },
                ));
            }
            for allocation in active {
                let coverage_id = coverage_id(settlement, allocation, price_class, start, end);
                let failed_renewal_id = if end == line.period_end() {
                    sponsored_failure_for_predecessor(
                        config,
                        manifest,
                        renewal_failures,
                        allocation,
                        settlement,
                        line,
                    )?
                    .map(|failure| failure.renewal_id().to_owned())
                } else {
                    None
                };
                let interval = ConfirmedPaidInterval {
                    coverage_id,
                    source_id: allocation.source_id().to_owned(),
                    starts_at: start,
                    paid_until: end,
                    failed_renewal_id,
                };
                let term = SponsoredBeneficiaryPaidTerm {
                    allocation_reference: allocation.allocation_reference().to_owned(),
                    price_class,
                    interval,
                };
                if let Some(existing) = beneficiaries
                    .iter_mut()
                    .find(|beneficiary| beneficiary.beneficiary_id == allocation.beneficiary_id())
                {
                    if existing.paid_terms.iter().any(|previous| {
                        previous.interval.starts_at < end && start < previous.interval.paid_until
                    }) {
                        return Err(SponsoredCoverageError::ConcurrentAllocation {
                            beneficiary_id: allocation.beneficiary_id().to_owned(),
                        });
                    }
                    existing.paid_terms.push(term);
                } else {
                    beneficiaries.push(SponsoredBeneficiaryCoverage {
                        beneficiary_id: allocation.beneficiary_id().to_owned(),
                        paid_terms: vec![term],
                    });
                }
            }
        }
    }

    for beneficiary in &mut beneficiaries {
        beneficiary.paid_terms.sort_by_key(|term| {
            (
                term.interval.starts_at,
                term.interval.paid_until,
                term.interval.coverage_id.clone(),
            )
        });
    }
    beneficiaries.sort_by(|left, right| left.beneficiary_id.cmp(&right.beneficiary_id));
    Ok(SponsoredCoverageResult::Candidate(
        SponsoredCoverageCandidate {
            account_id: config.account_id.clone(),
            environment: config.environment,
            customer_id: manifest.customer_id.clone(),
            subscription_id: manifest.subscription_id.clone(),
            payer_kind: PayerKind::Sponsor,
            semantic_reference: semantic_reference(config, manifest, settlement, &beneficiaries),
            beneficiaries,
        },
    ))
}

fn sponsored_failure_for_predecessor<'a>(
    config: &SponsoredStripeCoverageConfig,
    manifest: &SponsoredAllocationManifest,
    failures: &'a [StripeRenewalFailureEvidence],
    allocation: &SponsoredAllocationInterval,
    settlement: &SponsoredInvoiceSettlement,
    line: &SponsoredInvoiceLine,
) -> Result<Option<&'a StripeRenewalFailureEvidence>, SponsoredCoverageError> {
    let expected_interval = match config.price_class(line.price_id()) {
        Some(SponsoredPriceClass::StandardMonthly | SponsoredPriceClass::FoundingMonthly) => {
            crate::cloud_provider_stripe::StripeInterval::Month
        }
        Some(SponsoredPriceClass::StandardAnnual | SponsoredPriceClass::FoundingAnnual) => {
            crate::cloud_provider_stripe::StripeInterval::Year
        }
        None => return Ok(None),
    };
    let expected_predecessor_reference =
        sponsored_predecessor_evidence_reference(manifest, settlement);
    let linked = failures
        .iter()
        .filter(|failure| {
            failure.allocation_reference() == allocation.allocation_reference()
                && failure.subscription_id() == settlement.subscription_id
                && failure.customer_id() == settlement.customer_id
                && failure.provider_item_id() == line.provider_item_id()
                && failure.predecessor_invoice_id() == settlement.invoice_id
                && failure.predecessor_period_start() == line.period_start()
                && failure.predecessor_period_end() == line.period_end()
                && failure.predecessor_evidence_reference() == expected_predecessor_reference
                && failure.renewal_period_start() == line.period_end()
                && failure.provider_price_id() == Some(line.price_id())
                && failure.interval() == expected_interval
        })
        .collect::<Vec<_>>();
    let Some(first) = linked.first().copied() else {
        return Ok(None);
    };
    if linked.iter().any(|failure| {
        failure.renewal_id() != first.renewal_id()
            || failure.invoice_line_id() != first.invoice_line_id()
            || failure.predecessor_evidence_reference() != first.predecessor_evidence_reference()
            || failure.renewal_period_end() != first.renewal_period_end()
    }) {
        return Err(SponsoredCoverageError::InvalidInput(
            "sponsored renewal failures disagree for one predecessor allocation",
        ));
    }
    Ok(linked
        .into_iter()
        .min_by_key(|failure| (failure.event_created_at(), failure.event_id())))
}

/// Stable identity for a verified paid invoice together with its exact dated allocation manifest.
/// A renewal failure stores this value so a changed seat set, item price or interval cannot reuse
/// yesterday's predecessor proof.
pub(crate) fn sponsored_predecessor_evidence_reference(
    manifest: &SponsoredAllocationManifest,
    settlement: &SponsoredInvoiceSettlement,
) -> String {
    let mut allocations = manifest.allocations().iter().collect::<Vec<_>>();
    allocations.sort_by(|left, right| {
        (
            left.allocation_reference(),
            left.source_id(),
            left.beneficiary_id(),
            left.provider_item_id(),
            left.price_id(),
            left.effective_from(),
            left.effective_until(),
        )
            .cmp(&(
                right.allocation_reference(),
                right.source_id(),
                right.beneficiary_id(),
                right.provider_item_id(),
                right.price_id(),
                right.effective_from(),
                right.effective_until(),
            ))
    });
    let mut bytes = SEMANTIC_DOMAIN.to_vec();
    for value in [
        manifest.account_id(),
        manifest.customer_id(),
        manifest.subscription_id(),
        settlement.invoice_id(),
        settlement.evidence_reference(),
    ] {
        bytes.extend_from_slice(&(value.len() as u64).to_be_bytes());
        bytes.extend_from_slice(value.as_bytes());
    }
    bytes.extend_from_slice(&settlement.period_start().to_be_bytes());
    bytes.extend_from_slice(&settlement.period_end().to_be_bytes());
    for allocation in allocations {
        for value in [
            allocation.allocation_reference(),
            allocation.source_id(),
            allocation.beneficiary_id(),
            allocation.provider_item_id(),
            allocation.price_id(),
        ] {
            bytes.extend_from_slice(&(value.len() as u64).to_be_bytes());
            bytes.extend_from_slice(value.as_bytes());
        }
        bytes.extend_from_slice(&allocation.effective_from().to_be_bytes());
        match allocation.effective_until() {
            Some(until) => {
                bytes.push(1);
                bytes.extend_from_slice(&until.to_be_bytes());
            }
            None => bytes.push(0),
        }
    }
    let digest = Sha256::digest(bytes);
    format!("stripe-sponsored-predecessor-v1:{digest:x}")
}

fn check_context(
    config: &SponsoredStripeCoverageConfig,
    manifest: &SponsoredAllocationManifest,
    settlement: &SponsoredInvoiceSettlement,
) -> Result<(), SponsoredCoverageError> {
    if manifest.account_id() != config.account_id() {
        return Err(SponsoredCoverageError::ContextMismatch { field: "account" });
    }
    if manifest.environment() != config.environment() {
        return Err(SponsoredCoverageError::ContextMismatch {
            field: "environment",
        });
    }
    if settlement.customer_id != manifest.customer_id {
        return Err(SponsoredCoverageError::ContextMismatch { field: "customer" });
    }
    if settlement.subscription_id != manifest.subscription_id {
        return Err(SponsoredCoverageError::ContextMismatch {
            field: "subscription",
        });
    }
    if settlement.currency != "gbp" {
        return Err(SponsoredCoverageError::ContextMismatch { field: "currency" });
    }
    Ok(())
}

fn coverage_id(
    settlement: &SponsoredInvoiceSettlement,
    allocation: &SponsoredAllocationInterval,
    price_class: SponsoredPriceClass,
    start: i64,
    end: i64,
) -> String {
    let mut bytes = SEMANTIC_DOMAIN.to_vec();
    for value in [
        settlement.invoice_id.as_str(),
        settlement.evidence_reference.as_str(),
        allocation.allocation_reference(),
        allocation.source_id(),
        allocation.beneficiary_id(),
        allocation.provider_item_id(),
        allocation.price_id(),
    ] {
        bytes.extend_from_slice(&(value.len() as u64).to_be_bytes());
        bytes.extend_from_slice(value.as_bytes());
    }
    bytes.extend_from_slice(&start.to_be_bytes());
    bytes.extend_from_slice(&end.to_be_bytes());
    bytes.push(match price_class {
        SponsoredPriceClass::StandardMonthly => 0,
        SponsoredPriceClass::StandardAnnual => 1,
        SponsoredPriceClass::FoundingMonthly => 2,
        SponsoredPriceClass::FoundingAnnual => 3,
    });
    let digest = Sha256::digest(bytes);
    format!("stripe-sponsored-coverage-v1:{digest:x}")
}

fn semantic_reference(
    config: &SponsoredStripeCoverageConfig,
    manifest: &SponsoredAllocationManifest,
    settlement: &SponsoredInvoiceSettlement,
    beneficiaries: &[SponsoredBeneficiaryCoverage],
) -> String {
    let mut bytes = SEMANTIC_DOMAIN.to_vec();
    for value in [
        config.account_id.as_str(),
        config.environment.as_str(),
        manifest.customer_id.as_str(),
        manifest.subscription_id.as_str(),
        settlement.invoice_id.as_str(),
        settlement.evidence_reference.as_str(),
    ] {
        bytes.extend_from_slice(&(value.len() as u64).to_be_bytes());
        bytes.extend_from_slice(value.as_bytes());
    }
    for beneficiary in beneficiaries {
        bytes.extend_from_slice(&(beneficiary.beneficiary_id.len() as u64).to_be_bytes());
        bytes.extend_from_slice(beneficiary.beneficiary_id.as_bytes());
        for term in &beneficiary.paid_terms {
            bytes.extend_from_slice(&term.interval.starts_at.to_be_bytes());
            bytes.extend_from_slice(&term.interval.paid_until.to_be_bytes());
            bytes.extend_from_slice(&(term.interval.coverage_id.len() as u64).to_be_bytes());
            bytes.extend_from_slice(term.interval.coverage_id.as_bytes());
        }
    }
    let digest = Sha256::digest(bytes);
    format!("stripe-sponsored-coverage-v1:{digest:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn allocation(
        reference: &str,
        beneficiary: &str,
        item: &str,
        price: &str,
        start: i64,
        end: Option<i64>,
    ) -> SponsoredAllocationInterval {
        SponsoredAllocationInterval::new(
            format!("source-{reference}"),
            reference,
            beneficiary,
            item,
            price,
            start,
            end,
        )
        .unwrap()
    }

    fn settlement(lines: Vec<SponsoredInvoiceLine>) -> SponsoredInvoiceSettlement {
        SponsoredInvoiceSettlement::new(
            "in_1", "cus_1", "sub_1", 100, 200, "gbp", 1_000, 1_000, "pi_1", "evt_1", lines,
        )
        .unwrap()
    }

    #[test]
    fn grouped_quantity_maps_to_named_beneficiaries() {
        let manifest = SponsoredAllocationManifest::new(
            "acct_test",
            ProviderEnvironment::Test,
            "cus_1",
            "sub_1",
            vec![
                allocation("a_1", "user_1", "item_1", "price_standard_month", 0, None),
                allocation("a_2", "user_2", "item_1", "price_standard_month", 0, None),
            ],
        )
        .unwrap();
        let line =
            SponsoredInvoiceLine::new("il_1", "item_1", "price_standard_month", 100, 200, 2, false)
                .unwrap();
        let result =
            compose_sponsored_coverage(&config(), &manifest, &settlement(vec![line])).unwrap();
        let SponsoredCoverageResult::Candidate(candidate) = result else {
            panic!("expected candidate")
        };
        assert_eq!(candidate.payer_kind(), PayerKind::Sponsor);
        assert_eq!(candidate.beneficiaries().len(), 2);
        assert_eq!(
            candidate.beneficiaries()[0].paid_terms()[0]
                .interval()
                .starts_at,
            100
        );
        assert_eq!(
            candidate.beneficiaries()[1].paid_terms()[0]
                .interval()
                .paid_until,
            200
        );
        let coverages = candidate.person_coverages();
        assert_eq!(coverages.len(), 2);
        assert_eq!(coverages[0].beneficiary_id, "user_1");
        assert_eq!(coverages[0].paid_intervals.len(), 1);
        assert_eq!(candidate.source_observations().len(), 2);
    }

    #[test]
    fn mixed_interval_invoice_uses_each_line_paid_term() {
        let manifest = SponsoredAllocationManifest::new(
            "acct_test",
            ProviderEnvironment::Test,
            "cus_1",
            "sub_1",
            vec![
                allocation(
                    "monthly",
                    "user_monthly",
                    "item_monthly",
                    "price_standard_month",
                    0,
                    None,
                ),
                allocation(
                    "monthly_2",
                    "user_monthly_2",
                    "item_monthly",
                    "price_standard_month",
                    0,
                    None,
                ),
                allocation(
                    "annual",
                    "user_annual",
                    "item_annual",
                    "price_standard_year",
                    0,
                    None,
                ),
            ],
        )
        .unwrap();
        let settlement = SponsoredInvoiceSettlement::new(
            "in_mixed",
            "cus_1",
            "sub_1",
            100,
            465,
            "gbp",
            3_351,
            3_351,
            "pi_mixed",
            "evt_mixed",
            vec![
                SponsoredInvoiceLine::new(
                    "il_monthly",
                    "item_monthly",
                    "price_standard_month",
                    100,
                    130,
                    2,
                    false,
                )
                .unwrap(),
                SponsoredInvoiceLine::new_with_amount(
                    "il_annual",
                    "item_annual",
                    "price_standard_year",
                    100,
                    465,
                    1,
                    1_835,
                    true,
                )
                .unwrap(),
            ],
        )
        .unwrap();

        let result = compose_sponsored_coverage(&config(), &manifest, &settlement).unwrap();
        let SponsoredCoverageResult::Candidate(candidate) = result else {
            panic!("expected candidate")
        };
        assert_eq!(candidate.beneficiaries().len(), 3);
        let monthly = candidate
            .beneficiaries()
            .iter()
            .find(|beneficiary| beneficiary.beneficiary_id() == "user_monthly")
            .unwrap()
            .paid_terms()[0]
            .interval();
        assert_eq!((monthly.starts_at, monthly.paid_until), (100, 130));
        let annual = candidate
            .beneficiaries()
            .iter()
            .find(|beneficiary| beneficiary.beneficiary_id() == "user_annual")
            .unwrap()
            .paid_terms()[0]
            .interval();
        assert_eq!((annual.starts_at, annual.paid_until), (100, 465));
    }

    #[test]
    fn invoice_line_outside_the_invoice_period_envelope_needs_evidence() {
        let manifest = SponsoredAllocationManifest::new(
            "acct_test",
            ProviderEnvironment::Test,
            "cus_1",
            "sub_1",
            vec![allocation(
                "a_1",
                "user_1",
                "item_1",
                "price_standard_month",
                0,
                None,
            )],
        )
        .unwrap();
        let settlement = settlement(vec![SponsoredInvoiceLine::new(
            "il_outside_envelope",
            "item_1",
            "price_standard_month",
            99,
            200,
            1,
            false,
        )
        .unwrap()]);

        assert!(matches!(
            compose_sponsored_coverage(&config(), &manifest, &settlement).unwrap(),
            SponsoredCoverageResult::NeedsEvidence(
                SponsoredNeedsEvidence::MissingAllocationHistory { .. }
            )
        ));
    }

    #[test]
    fn linked_grouped_renewal_failure_marks_only_the_matching_paid_interval() {
        let cfg = config();
        let predecessor_manifest = SponsoredAllocationManifest::new(
            "acct_test",
            ProviderEnvironment::Test,
            "cus_1",
            "sub_1",
            vec![allocation(
                "a_1",
                "user_1",
                "item_1",
                "price_standard_month",
                0,
                None,
            )],
        )
        .unwrap();
        let predecessor = SponsoredInvoiceSettlement::new(
            "in_previous",
            "cus_1",
            "sub_1",
            0,
            100,
            "gbp",
            1000,
            1000,
            "pi_previous",
            "paid_previous",
            vec![SponsoredInvoiceLine::new(
                "il_previous",
                "item_1",
                "price_standard_month",
                0,
                100,
                1,
                false,
            )
            .unwrap()],
        )
        .unwrap();
        let predecessor_reference =
            sponsored_predecessor_evidence_reference(&predecessor_manifest, &predecessor);
        let failure = crate::cloud_provider_stripe_renewals::StripeRenewalFailureEvidence::from_sponsored_parts(
            "evt_failed", "in_failed", "il_failed", "in_previous", &predecessor_reference,
            "acct_test", ProviderEnvironment::Test, "a_1", "cus_1", "sub_1", "item_1",
            "price_standard_month", 0, 100, 100, 200, 150, crate::cloud_provider_stripe::StripeInterval::Month,
        ).unwrap();
        let result = compose_sponsored_coverage_with_renewal_failures(
            &cfg,
            &predecessor_manifest,
            &predecessor,
            std::slice::from_ref(&failure),
        )
        .unwrap();
        let SponsoredCoverageResult::Candidate(candidate) = result else {
            panic!("expected candidate")
        };
        assert_eq!(
            candidate.person_coverages()[0].paid_intervals[0]
                .failed_renewal_id
                .as_deref(),
            Some(failure.renewal_id())
        );
    }

    #[test]
    fn sponsored_renewal_failure_cannot_attach_to_a_different_predecessor() {
        let cfg = config();
        let manifest = SponsoredAllocationManifest::new(
            "acct_test",
            ProviderEnvironment::Test,
            "cus_1",
            "sub_1",
            vec![allocation(
                "a_1",
                "user_1",
                "item_1",
                "price_standard_month",
                0,
                None,
            )],
        )
        .unwrap();
        let previous = SponsoredInvoiceSettlement::new(
            "in_previous",
            "cus_1",
            "sub_1",
            0,
            100,
            "gbp",
            1000,
            1000,
            "pi_previous",
            "paid_previous",
            vec![SponsoredInvoiceLine::new(
                "il_previous",
                "item_1",
                "price_standard_month",
                0,
                100,
                1,
                false,
            )
            .unwrap()],
        )
        .unwrap();
        let reference = sponsored_predecessor_evidence_reference(&manifest, &previous);
        let failure = crate::cloud_provider_stripe_renewals::StripeRenewalFailureEvidence::from_sponsored_parts(
            "evt_failed", "in_failed", "il_failed", "in_other", &reference,
            "acct_test", ProviderEnvironment::Test, "a_1", "cus_1", "sub_1", "item_1",
            "price_standard_month", 0, 100, 100, 200, 150, crate::cloud_provider_stripe::StripeInterval::Month,
        ).unwrap();
        let result = compose_sponsored_coverage_with_renewal_failures(
            &cfg,
            &manifest,
            &previous,
            &[failure],
        )
        .unwrap();
        let SponsoredCoverageResult::Candidate(candidate) = result else {
            panic!("expected candidate")
        };
        assert_eq!(
            candidate.person_coverages()[0].paid_intervals[0].failed_renewal_id,
            None
        );
    }

    #[test]
    fn replacement_at_billing_boundary_preserves_each_person_clock() {
        let manifest = SponsoredAllocationManifest::new(
            "acct_test",
            ProviderEnvironment::Test,
            "cus_1",
            "sub_1",
            vec![
                allocation(
                    "a_1",
                    "user_1",
                    "item_1",
                    "price_founding_month",
                    0,
                    Some(150),
                ),
                allocation(
                    "a_2",
                    "user_2",
                    "item_1",
                    "price_standard_month",
                    0,
                    Some(100),
                ),
                allocation(
                    "a_3",
                    "user_3",
                    "item_1",
                    "price_standard_month",
                    100,
                    Some(200),
                ),
            ],
        )
        .unwrap();
        let line =
            SponsoredInvoiceLine::new("il_1", "item_1", "price_standard_month", 100, 200, 1, false)
                .unwrap();
        let result = compose_sponsored_coverage(&config(), &manifest, &settlement(vec![line]));
        let SponsoredCoverageResult::Candidate(candidate) = result.unwrap() else {
            panic!("expected candidate")
        };
        assert_eq!(candidate.beneficiaries().len(), 1);
        assert_eq!(candidate.beneficiaries()[0].beneficiary_id(), "user_3");
        assert_eq!(
            candidate.beneficiaries()[0].paid_terms()[0].price_class(),
            SponsoredPriceClass::StandardMonthly
        );
    }

    #[test]
    fn partial_payment_never_publishes_named_coverage() {
        let manifest = SponsoredAllocationManifest::new(
            "acct_test",
            ProviderEnvironment::Test,
            "cus_1",
            "sub_1",
            vec![allocation(
                "a_1",
                "user_1",
                "item_1",
                "price_standard_month",
                0,
                None,
            )],
        )
        .unwrap();
        let line =
            SponsoredInvoiceLine::new("il_1", "item_1", "price_standard_month", 100, 200, 1, false)
                .unwrap();
        let mut paid = settlement(vec![line]);
        paid.amount_paid = 999;
        assert!(matches!(
            compose_sponsored_coverage(&config(), &manifest, &paid).unwrap(),
            SponsoredCoverageResult::NeedsEvidence(SponsoredNeedsEvidence::PartialPayment { .. })
        ));
    }

    #[test]
    fn positive_proration_uses_its_paid_period_without_inflating_quantity() {
        let manifest = SponsoredAllocationManifest::new(
            "acct_test",
            ProviderEnvironment::Test,
            "cus_1",
            "sub_1",
            vec![allocation(
                "a_1",
                "user_1",
                "item_1",
                "price_standard_month",
                0,
                None,
            )],
        )
        .unwrap();
        let line = SponsoredInvoiceLine::new_with_amount(
            "il_proration",
            "item_1",
            "price_standard_month",
            100,
            200,
            1,
            100,
            true,
        )
        .unwrap();
        assert!(matches!(
            compose_sponsored_coverage(&config(), &manifest, &settlement(vec![line])).unwrap(),
            SponsoredCoverageResult::Candidate(_)
        ));
    }

    #[test]
    fn non_positive_proration_is_needs_evidence() {
        let manifest = SponsoredAllocationManifest::new(
            "acct_test",
            ProviderEnvironment::Test,
            "cus_1",
            "sub_1",
            vec![allocation(
                "a_1",
                "user_1",
                "item_1",
                "price_standard_month",
                0,
                None,
            )],
        )
        .unwrap();
        let line = SponsoredInvoiceLine::new_with_amount(
            "il_credit",
            "item_1",
            "price_standard_month",
            100,
            200,
            1,
            -100,
            true,
        )
        .unwrap();
        assert!(matches!(
            compose_sponsored_coverage(&config(), &manifest, &settlement(vec![line])).unwrap(),
            SponsoredCoverageResult::NeedsEvidence(
                SponsoredNeedsEvidence::NonPositiveLineAmount { .. }
            )
        ));
    }

    #[test]
    fn overlapping_named_allocation_is_rejected_at_manifest_boundary() {
        let result = SponsoredAllocationManifest::new(
            "acct_test",
            ProviderEnvironment::Test,
            "cus_1",
            "sub_1",
            vec![
                allocation(
                    "a_1",
                    "user_1",
                    "item_1",
                    "price_standard_month",
                    0,
                    Some(200),
                ),
                allocation(
                    "a_2",
                    "user_1",
                    "item_1",
                    "price_standard_month",
                    100,
                    Some(300),
                ),
            ],
        );
        assert!(matches!(
            result,
            Err(SponsoredCoverageError::ConcurrentAllocation { .. })
        ));
    }

    #[test]
    fn allocation_reference_cannot_cover_two_people_at_once() {
        let result = SponsoredAllocationManifest::new(
            "acct_test",
            ProviderEnvironment::Test,
            "cus_1",
            "sub_1",
            vec![
                SponsoredAllocationInterval::new(
                    "source_a",
                    "same",
                    "user_1",
                    "item_1",
                    "price_standard_month",
                    0,
                    None,
                )
                .unwrap(),
                SponsoredAllocationInterval::new(
                    "source_b",
                    "same",
                    "user_2",
                    "item_1",
                    "price_standard_month",
                    0,
                    None,
                )
                .unwrap(),
            ],
        );
        assert!(matches!(
            result,
            Err(SponsoredCoverageError::ConflictingAllocationReference { .. })
        ));
    }

    #[test]
    fn source_reference_cannot_cover_two_people_at_once() {
        let result = SponsoredAllocationManifest::new(
            "acct_test",
            ProviderEnvironment::Test,
            "cus_1",
            "sub_1",
            vec![
                SponsoredAllocationInterval::new(
                    "source_same",
                    "allocation_a",
                    "user_1",
                    "item_1",
                    "price_standard_month",
                    0,
                    None,
                )
                .unwrap(),
                SponsoredAllocationInterval::new(
                    "source_same",
                    "allocation_b",
                    "user_2",
                    "item_1",
                    "price_standard_month",
                    0,
                    None,
                )
                .unwrap(),
            ],
        );
        assert!(matches!(
            result,
            Err(SponsoredCoverageError::ConflictingSourceReference { .. })
        ));
    }

    #[test]
    fn unknown_price_blocks_the_whole_invoice() {
        let manifest = SponsoredAllocationManifest::new(
            "acct_test",
            ProviderEnvironment::Test,
            "cus_1",
            "sub_1",
            vec![allocation(
                "a_1",
                "user_1",
                "item_1",
                "price_unknown",
                0,
                None,
            )],
        )
        .unwrap();
        let line = SponsoredInvoiceLine::new("il_1", "item_1", "price_unknown", 100, 200, 1, false)
            .unwrap();
        assert!(matches!(
            compose_sponsored_coverage(&config(), &manifest, &settlement(vec![line])).unwrap(),
            SponsoredCoverageResult::NeedsEvidence(SponsoredNeedsEvidence::UnknownPrice { .. })
        ));
    }
}
