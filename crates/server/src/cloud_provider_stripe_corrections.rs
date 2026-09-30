//! Pure association of a personal invoice with its Stripe correction evidence.
//!
//! This module turns transport resources into explicitly associated evidence or an unresolved
//! result, then applies the personal invoice access policy to that sealed result.

use std::collections::HashSet;

use crate::cloud_provider::ProviderEnvironment;
use crate::cloud_provider_stripe::StripePersonalInvoiceObservation;
use crate::cloud_provider_stripe_http::{
    StripeCreditNoteResource, StripeCreditNoteStatus, StripeCreditNoteType, StripeDisputeResource,
    StripeDisputeStatus, StripeReadError, StripeRefundResource, StripeRefundStatus,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StripeCorrectionUnresolved {
    RefundMissingPaymentIntent {
        refund_id: String,
    },
    RefundMissingStatus {
        refund_id: String,
    },
    RefundUnknownStatus {
        refund_id: String,
        status: String,
    },
    DisputeMissingPaymentIntent {
        dispute_id: String,
    },
    DisputeUnknownStatus {
        dispute_id: String,
        status: String,
    },
    CreditNoteUnknownStatus {
        credit_note_id: String,
        status: String,
    },
    CreditNoteUnknownType {
        credit_note_id: String,
        note_type: String,
    },
}

impl StripeCorrectionUnresolved {
    fn sort_key(&self) -> (&'static str, &str) {
        match self {
            Self::RefundMissingPaymentIntent { refund_id }
            | Self::RefundMissingStatus { refund_id }
            | Self::RefundUnknownStatus { refund_id, .. } => ("refund", refund_id),
            Self::DisputeMissingPaymentIntent { dispute_id }
            | Self::DisputeUnknownStatus { dispute_id, .. } => ("dispute", dispute_id),
            Self::CreditNoteUnknownStatus { credit_note_id, .. }
            | Self::CreditNoteUnknownType { credit_note_id, .. } => ("credit_note", credit_note_id),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StripePersonalInvoiceCorrectionEvidence {
    Associated(Box<StripeAssociatedCorrectionEvidence>),
    Unresolved(StripeUnresolvedCorrections),
}

/// The access-policy result for one validated personal invoice observation.
///
/// Known refunds, disputes and credit notes preserve the original paid term. They do not get
/// converted into a net amount or an entitlement end date. Unresolved association evidence stays
/// unresolved until a later boundary can establish it; it cannot produce a partial policy result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StripePersonalInvoiceAccessDecision {
    RetainPaidTerm(StripeRetainedPaidTerm),
    NeedsEvidence(StripeUnresolvedCorrections),
}

/// The original paid interval retained by the personal correction policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeRetainedPaidTerm {
    observation: StripePersonalInvoiceObservation,
}

impl StripeRetainedPaidTerm {
    pub fn invoice_id(&self) -> &str {
        self.observation.invoice_id()
    }

    pub fn allocation_reference(&self) -> &str {
        self.observation.allocation_reference()
    }

    pub fn customer_id(&self) -> &str {
        self.observation.customer_id()
    }

    pub fn subscription_id(&self) -> &str {
        self.observation.subscription_id()
    }

    pub fn provider_item_id(&self) -> &str {
        self.observation.provider_item_id()
    }

    pub const fn interval(&self) -> crate::cloud_provider_stripe::StripeInterval {
        self.observation.interval()
    }

    pub fn evidence_reference(&self) -> &str {
        self.observation.evidence_reference()
    }

    pub const fn period_start(&self) -> i64 {
        self.observation.period_start()
    }

    pub const fn period_end(&self) -> i64 {
        self.observation.period_end()
    }
}

/// Apply the agreed personal correction policy to sealed invoice evidence.
///
/// A partial or full refund, dispute, or credit note does not shorten the paid term by itself.
/// Early termination requires a separate confirmed cancellation workflow, which is deliberately
/// absent from this operation. This function is pure and does not establish current account
/// eligibility or publish a coverage projection.
pub fn evaluate_personal_invoice_access(
    evidence: &StripePersonalInvoiceCorrectionEvidence,
) -> StripePersonalInvoiceAccessDecision {
    match evidence {
        StripePersonalInvoiceCorrectionEvidence::Associated(associated) => {
            let mut unresolved = Vec::new();
            for refund in associated.refunds() {
                match refund.status() {
                    StripeRefundStatus::Pending
                    | StripeRefundStatus::RequiresAction
                    | StripeRefundStatus::Succeeded
                    | StripeRefundStatus::Failed
                    | StripeRefundStatus::Canceled => {}
                    StripeRefundStatus::Unknown(status) => {
                        unresolved.push(StripeCorrectionUnresolved::RefundUnknownStatus {
                            refund_id: refund.id().to_owned(),
                            status: status.clone(),
                        });
                    }
                }
            }
            for dispute in associated.disputes() {
                match dispute.status() {
                    StripeDisputeStatus::WarningNeedsResponse
                    | StripeDisputeStatus::WarningUnderReview
                    | StripeDisputeStatus::WarningClosed
                    | StripeDisputeStatus::NeedsResponse
                    | StripeDisputeStatus::UnderReview
                    | StripeDisputeStatus::Won
                    | StripeDisputeStatus::Lost
                    | StripeDisputeStatus::Prevented => {}
                    StripeDisputeStatus::Unknown(status) => {
                        unresolved.push(StripeCorrectionUnresolved::DisputeUnknownStatus {
                            dispute_id: dispute.id().to_owned(),
                            status: status.clone(),
                        });
                    }
                }
            }
            for credit_note in associated.credit_notes() {
                match credit_note.status() {
                    StripeCreditNoteStatus::Issued | StripeCreditNoteStatus::Void => {}
                    StripeCreditNoteStatus::Unknown(status) => {
                        unresolved.push(StripeCorrectionUnresolved::CreditNoteUnknownStatus {
                            credit_note_id: credit_note.id().to_owned(),
                            status: status.clone(),
                        });
                    }
                }
                match credit_note.note_type() {
                    StripeCreditNoteType::PrePayment
                    | StripeCreditNoteType::PostPayment
                    | StripeCreditNoteType::Mixed => {}
                    StripeCreditNoteType::Unknown(note_type) => {
                        unresolved.push(StripeCorrectionUnresolved::CreditNoteUnknownType {
                            credit_note_id: credit_note.id().to_owned(),
                            note_type: note_type.clone(),
                        });
                    }
                }
            }
            if !unresolved.is_empty() {
                unresolved.sort_by(|left, right| left.sort_key().cmp(&right.sort_key()));
                return StripePersonalInvoiceAccessDecision::NeedsEvidence(
                    StripeUnresolvedCorrections {
                        invoice_id: associated.observation().invoice_id().to_owned(),
                        reasons: unresolved,
                    },
                );
            }
            StripePersonalInvoiceAccessDecision::RetainPaidTerm(StripeRetainedPaidTerm {
                observation: associated.observation.clone(),
            })
        }
        StripePersonalInvoiceCorrectionEvidence::Unresolved(unresolved) => {
            StripePersonalInvoiceAccessDecision::NeedsEvidence(unresolved.clone())
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeAssociatedCorrectionEvidence {
    observation: StripePersonalInvoiceObservation,
    refunds: Vec<StripeVerifiedRefund>,
    disputes: Vec<StripeVerifiedDispute>,
    credit_notes: Vec<StripeVerifiedCreditNote>,
}

impl StripeAssociatedCorrectionEvidence {
    pub fn observation(&self) -> &StripePersonalInvoiceObservation {
        &self.observation
    }

    pub fn refunds(&self) -> &[StripeVerifiedRefund] {
        &self.refunds
    }

    pub fn disputes(&self) -> &[StripeVerifiedDispute] {
        &self.disputes
    }

    pub fn credit_notes(&self) -> &[StripeVerifiedCreditNote] {
        &self.credit_notes
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeUnresolvedCorrections {
    invoice_id: String,
    reasons: Vec<StripeCorrectionUnresolved>,
}

impl StripeUnresolvedCorrections {
    pub fn invoice_id(&self) -> &str {
        &self.invoice_id
    }

    pub fn reasons(&self) -> &[StripeCorrectionUnresolved] {
        &self.reasons
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeVerifiedRefund {
    id: String,
    payment_intent_id: String,
    charge_id: Option<String>,
    amount: i64,
    currency: String,
    created: i64,
    status: StripeRefundStatus,
}

impl StripeVerifiedRefund {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn payment_intent_id(&self) -> &str {
        &self.payment_intent_id
    }

    pub fn charge_id(&self) -> Option<&str> {
        self.charge_id.as_deref()
    }

    pub const fn amount(&self) -> i64 {
        self.amount
    }

    pub fn currency(&self) -> &str {
        &self.currency
    }

    pub const fn created(&self) -> i64 {
        self.created
    }

    pub fn status(&self) -> &StripeRefundStatus {
        &self.status
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeVerifiedDispute {
    id: String,
    payment_intent_id: String,
    charge_id: String,
    amount: i64,
    currency: String,
    created: i64,
    status: StripeDisputeStatus,
}

impl StripeVerifiedDispute {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn payment_intent_id(&self) -> &str {
        &self.payment_intent_id
    }

    pub fn charge_id(&self) -> &str {
        &self.charge_id
    }

    pub const fn amount(&self) -> i64 {
        self.amount
    }

    pub fn currency(&self) -> &str {
        &self.currency
    }

    pub const fn created(&self) -> i64 {
        self.created
    }

    pub fn status(&self) -> &StripeDisputeStatus {
        &self.status
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeVerifiedCreditNote {
    id: String,
    invoice_id: String,
    customer_id: String,
    amount: i64,
    pre_payment_amount: i64,
    post_payment_amount: i64,
    currency: String,
    created: i64,
    status: StripeCreditNoteStatus,
    note_type: StripeCreditNoteType,
}

impl StripeVerifiedCreditNote {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn invoice_id(&self) -> &str {
        &self.invoice_id
    }

    pub fn customer_id(&self) -> &str {
        &self.customer_id
    }

    pub const fn amount(&self) -> i64 {
        self.amount
    }

    pub const fn pre_payment_amount(&self) -> i64 {
        self.pre_payment_amount
    }

    pub const fn post_payment_amount(&self) -> i64 {
        self.post_payment_amount
    }

    pub fn currency(&self) -> &str {
        &self.currency
    }

    pub const fn created(&self) -> i64 {
        self.created
    }

    pub fn status(&self) -> &StripeCreditNoteStatus {
        &self.status
    }

    pub fn note_type(&self) -> &StripeCreditNoteType {
        &self.note_type
    }
}

pub(crate) fn assemble(
    observation: StripePersonalInvoiceObservation,
    mut refunds: Vec<StripeRefundResource>,
    mut disputes: Vec<StripeDisputeResource>,
    mut credit_notes: Vec<StripeCreditNoteResource>,
    environment: ProviderEnvironment,
) -> Result<StripePersonalInvoiceCorrectionEvidence, StripeReadError> {
    refunds.sort_by(|left, right| left.id.cmp(&right.id));
    disputes.sort_by(|left, right| left.id.cmp(&right.id));
    credit_notes.sort_by(|left, right| left.id.cmp(&right.id));

    let mut errors = Vec::new();
    let mut unresolved = Vec::new();
    let mut verified_refunds = Vec::new();
    let mut verified_disputes = Vec::new();
    let mut verified_credit_notes = Vec::new();

    let mut refund_ids = HashSet::new();
    for refund in refunds {
        if !refund_ids.insert(refund.id.clone()) {
            errors.push(StripeReadError::InvalidPagination(
                "duplicate correction id",
            ));
            continue;
        }
        if refund
            .livemode
            .is_some_and(|mode| mode != is_live(environment))
        {
            errors.push(StripeReadError::ContextMismatch);
            continue;
        }
        if refund.currency != observation.currency() {
            errors.push(StripeReadError::ContextMismatch);
            continue;
        }
        let Some(payment_intent_id) = &refund.payment_intent_id else {
            unresolved.push(StripeCorrectionUnresolved::RefundMissingPaymentIntent {
                refund_id: refund.id.clone(),
            });
            continue;
        };
        if payment_intent_id != observation.payment_intent_id() {
            errors.push(StripeReadError::ParentMismatch);
            continue;
        }
        let Some(status) = refund.status else {
            unresolved.push(StripeCorrectionUnresolved::RefundMissingStatus {
                refund_id: refund.id.clone(),
            });
            continue;
        };
        if let StripeRefundStatus::Unknown(status) = &status {
            unresolved.push(StripeCorrectionUnresolved::RefundUnknownStatus {
                refund_id: refund.id.clone(),
                status: status.clone(),
            });
            continue;
        }
        verified_refunds.push(StripeVerifiedRefund {
            id: refund.id,
            payment_intent_id: payment_intent_id.clone(),
            charge_id: refund.charge_id,
            amount: refund.amount,
            currency: refund.currency,
            created: refund.created,
            status,
        });
    }

    let mut dispute_ids = HashSet::new();
    for dispute in disputes {
        if !dispute_ids.insert(dispute.id.clone()) {
            errors.push(StripeReadError::InvalidPagination(
                "duplicate correction id",
            ));
            continue;
        }
        if dispute.livemode != is_live(environment) || dispute.currency != observation.currency() {
            errors.push(StripeReadError::ContextMismatch);
            continue;
        }
        let Some(payment_intent_id) = &dispute.payment_intent_id else {
            unresolved.push(StripeCorrectionUnresolved::DisputeMissingPaymentIntent {
                dispute_id: dispute.id.clone(),
            });
            continue;
        };
        if payment_intent_id != observation.payment_intent_id() {
            errors.push(StripeReadError::ParentMismatch);
            continue;
        }
        if let StripeDisputeStatus::Unknown(status) = &dispute.status {
            unresolved.push(StripeCorrectionUnresolved::DisputeUnknownStatus {
                dispute_id: dispute.id.clone(),
                status: status.clone(),
            });
            continue;
        }
        verified_disputes.push(StripeVerifiedDispute {
            id: dispute.id,
            payment_intent_id: payment_intent_id.clone(),
            charge_id: dispute.charge_id,
            amount: dispute.amount,
            currency: dispute.currency,
            created: dispute.created,
            status: dispute.status,
        });
    }

    let mut credit_note_ids = HashSet::new();
    for note in credit_notes {
        if !credit_note_ids.insert(note.id.clone()) {
            errors.push(StripeReadError::InvalidPagination(
                "duplicate correction id",
            ));
            continue;
        }
        if note.invoice_id != observation.invoice_id()
            || note.customer_id != observation.customer_id()
        {
            errors.push(StripeReadError::ParentMismatch);
            continue;
        }
        if note.livemode != is_live(environment) || note.currency != observation.currency() {
            errors.push(StripeReadError::ContextMismatch);
            continue;
        }
        let unknown_status = if let StripeCreditNoteStatus::Unknown(status) = &note.status {
            unresolved.push(StripeCorrectionUnresolved::CreditNoteUnknownStatus {
                credit_note_id: note.id.clone(),
                status: status.clone(),
            });
            true
        } else {
            false
        };
        let unknown_type = if let StripeCreditNoteType::Unknown(note_type) = &note.credit_note_type
        {
            unresolved.push(StripeCorrectionUnresolved::CreditNoteUnknownType {
                credit_note_id: note.id.clone(),
                note_type: note_type.clone(),
            });
            true
        } else {
            false
        };
        if unknown_status || unknown_type {
            continue;
        }
        verified_credit_notes.push(StripeVerifiedCreditNote {
            id: note.id,
            invoice_id: note.invoice_id,
            customer_id: note.customer_id,
            amount: note.amount,
            pre_payment_amount: note.pre_payment_amount,
            post_payment_amount: note.post_payment_amount,
            currency: note.currency,
            created: note.created,
            status: note.status,
            note_type: note.credit_note_type,
        });
    }

    if let Some(error) = errors.into_iter().next() {
        return Err(error);
    }
    if !unresolved.is_empty() {
        unresolved.sort_by(|left, right| left.sort_key().cmp(&right.sort_key()));
        return Ok(StripePersonalInvoiceCorrectionEvidence::Unresolved(
            StripeUnresolvedCorrections {
                invoice_id: observation.invoice_id().to_owned(),
                reasons: unresolved,
            },
        ));
    }

    Ok(StripePersonalInvoiceCorrectionEvidence::Associated(
        Box::new(StripeAssociatedCorrectionEvidence {
            observation,
            refunds: verified_refunds,
            disputes: verified_disputes,
            credit_notes: verified_credit_notes,
        }),
    ))
}

fn is_live(environment: ProviderEnvironment) -> bool {
    matches!(environment, ProviderEnvironment::Live)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_table_preserves_unresolved_reasons_without_promoting_them() {
        let cases = vec![
            (
                "missing refund status",
                vec![StripeCorrectionUnresolved::RefundMissingStatus {
                    refund_id: "re_missing".into(),
                }],
            ),
            (
                "multiple reasons",
                vec![
                    StripeCorrectionUnresolved::CreditNoteUnknownStatus {
                        credit_note_id: "cn_unknown".into(),
                        status: "future_status".into(),
                    },
                    StripeCorrectionUnresolved::CreditNoteUnknownType {
                        credit_note_id: "cn_unknown".into(),
                        note_type: "future_type".into(),
                    },
                ],
            ),
        ];

        for (name, reasons) in cases {
            let evidence =
                StripePersonalInvoiceCorrectionEvidence::Unresolved(StripeUnresolvedCorrections {
                    invoice_id: "in_test".into(),
                    reasons: reasons.clone(),
                });
            let decision = evaluate_personal_invoice_access(&evidence);
            let StripePersonalInvoiceAccessDecision::NeedsEvidence(unresolved) = decision else {
                panic!("{name} was promoted to an access decision");
            };
            assert_eq!(unresolved.reasons(), reasons.as_slice(), "{name}");
        }
    }
}
