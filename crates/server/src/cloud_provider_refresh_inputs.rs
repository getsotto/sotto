//! Durable inputs for one provider refresh job.
//!
//! A queue row stores stable ids so it remains small and provider-neutral. This loader joins the
//! receipt, allocation, payer, and source records after a lease is claimed, then reconstructs the
//! validated provider values used by the refresh lifecycle. It never returns raw payload bytes.

use sqlx::{PgPool, Row};
use thiserror::Error;

use crate::cloud_provider::{
    AllocationState, PayerKind, VerifiedAllocation, VerifiedProviderEvent,
};
use crate::cloud_provider_refresh_jobs::RefreshJobLease;

#[derive(Debug, Error)]
pub enum RefreshInputError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("refresh job inputs are missing")]
    Missing,
    #[error("refresh job inputs are corrupt: {0}")]
    Corrupt(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshJobInputs {
    pub lease: RefreshJobLease,
    pub event: VerifiedProviderEvent,
    pub allocation: VerifiedAllocation,
    pub receipt_status: String,
}

/// Load and validate all durable inputs for a claimed refresh job.
pub async fn load(
    pool: &PgPool,
    lease: &RefreshJobLease,
) -> Result<RefreshJobInputs, RefreshInputError> {
    let row = sqlx::query(
        "SELECT receipt.event_type, receipt.provider_created_at, receipt.subscription_id, \
                receipt.allocation_reference, receipt.normalized_payload_hash, receipt.status, \
                allocation.payer_id, payer.provider_customer_id, payer.payer_kind, \
                allocation.provider_subscription_id, allocation.provider_item_id, \
                allocation.external_allocation_reference, allocation.effective_from, \
                allocation.effective_until, allocation.state, allocation.ownership_evidence_reference \
         FROM cloud_provider_event_receipts AS receipt \
         JOIN cloud_provider_allocations AS allocation \
           ON allocation.allocation_id = $5 \
         JOIN cloud_provider_payers AS payer ON payer.payer_id = allocation.payer_id \
         JOIN cloud_coverage_sources AS source \
           ON source.source_id = allocation.coverage_source_id \
         WHERE receipt.provider_namespace = $1 \
           AND receipt.provider_account_id = $2 \
           AND receipt.provider_environment = $3 \
           AND receipt.event_id = $4 \
           AND allocation.provider_namespace = $1 \
           AND allocation.provider_account_id = $2 \
           AND allocation.provider_environment = $3 \
           AND allocation.beneficiary_id = $6 \
           AND allocation.coverage_source_id = $7 \
           AND source.beneficiary_id = allocation.beneficiary_id \
           AND payer.provider_namespace = $1 \
           AND payer.provider_account_id = $2 \
           AND payer.provider_environment = $3",
    )
    .bind(&lease.context.namespace)
    .bind(&lease.context.account_id)
    .bind(lease.context.environment.as_str())
    .bind(&lease.event_id)
    .bind(&lease.allocation_id)
    .bind(&lease.beneficiary_id)
    .bind(&lease.source_id)
    .fetch_optional(pool)
    .await?
    .ok_or(RefreshInputError::Missing)?;

    let event = VerifiedProviderEvent::from_stored(
        lease.event_id.clone(),
        row.try_get("event_type")?,
        row.try_get("provider_created_at")?,
        row.try_get("subscription_id")?,
        row.try_get("allocation_reference")?,
        row.try_get("normalized_payload_hash")?,
    )
    .map_err(|error| RefreshInputError::Corrupt(error.to_string()))?;
    let payer_kind = parse_payer_kind(row.try_get::<String, _>("payer_kind")?.as_str())?;
    let state = parse_allocation_state(row.try_get::<String, _>("state")?.as_str())?;
    let allocation = VerifiedAllocation::new(
        lease.allocation_id.clone(),
        row.try_get::<String, _>("payer_id")?,
        row.try_get::<String, _>("provider_customer_id")?,
        payer_kind,
        lease.beneficiary_id.clone(),
        row.try_get::<String, _>("provider_subscription_id")?,
        row.try_get::<String, _>("provider_item_id")?,
        row.try_get::<String, _>("external_allocation_reference")?,
        lease.source_id.clone(),
        row.try_get("effective_from")?,
        row.try_get("effective_until")?,
        state,
        row.try_get::<String, _>("ownership_evidence_reference")?,
    )
    .map_err(|error| RefreshInputError::Corrupt(error.to_string()))?;
    if !event_matches_allocation(&event, &allocation) {
        return Err(RefreshInputError::Corrupt(
            "receipt and allocation identities differ".into(),
        ));
    }
    Ok(RefreshJobInputs {
        lease: lease.clone(),
        event,
        allocation,
        receipt_status: row.try_get("status")?,
    })
}

fn event_matches_allocation(
    event: &VerifiedProviderEvent,
    allocation: &VerifiedAllocation,
) -> bool {
    event
        .subscription_id
        .as_deref()
        .is_none_or(|subscription_id| subscription_id == allocation.subscription_id)
        && event
            .allocation_reference
            .as_deref()
            .is_none_or(|reference| reference == allocation.external_allocation_reference)
}

fn parse_payer_kind(value: &str) -> Result<PayerKind, RefreshInputError> {
    match value {
        "personal" => Ok(PayerKind::Personal),
        "sponsor" => Ok(PayerKind::Sponsor),
        _ => Err(RefreshInputError::Corrupt(format!(
            "unknown payer kind {value:?}"
        ))),
    }
}

fn parse_allocation_state(value: &str) -> Result<AllocationState, RefreshInputError> {
    match value {
        "pending" => Ok(AllocationState::Pending),
        "active" => Ok(AllocationState::Active),
        "ended" => Ok(AllocationState::Ended),
        _ => Err(RefreshInputError::Corrupt(format!(
            "unknown allocation state {value:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::event_matches_allocation;
    use crate::cloud_provider::{
        AllocationState, PayerKind, VerifiedAllocation, VerifiedProviderEvent,
    };

    fn allocation() -> VerifiedAllocation {
        VerifiedAllocation::new(
            "allocation",
            "payer",
            "customer",
            PayerKind::Personal,
            "beneficiary",
            "subscription",
            "item",
            "external",
            "source",
            0,
            None,
            AllocationState::Active,
            "ownership",
        )
        .unwrap()
    }

    fn event(
        subscription_id: Option<&str>,
        allocation_reference: Option<&str>,
    ) -> VerifiedProviderEvent {
        VerifiedProviderEvent::from_payload(
            "event",
            "invoice.paid",
            1,
            subscription_id.map(str::to_owned),
            allocation_reference.map(str::to_owned),
            b"payload",
        )
        .unwrap()
    }

    #[test]
    fn optional_event_identity_claims_are_allowed() {
        let allocation = allocation();
        assert!(event_matches_allocation(&event(None, None), &allocation));
        assert!(event_matches_allocation(
            &event(Some("subscription"), None),
            &allocation
        ));
        assert!(event_matches_allocation(
            &event(None, Some("external")),
            &allocation
        ));
    }

    #[test]
    fn present_event_identity_claims_must_match() {
        let allocation = allocation();
        assert!(!event_matches_allocation(
            &event(Some("other-subscription"), None),
            &allocation
        ));
        assert!(!event_matches_allocation(
            &event(None, Some("other-external")),
            &allocation
        ));
    }
}
