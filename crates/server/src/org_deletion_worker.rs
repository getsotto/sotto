//! Background runner for the staged organisation-deletion lifecycle.
//!
//! The worker is opt-in until the deletion enablement checklist is complete. Once enabled, every
//! server instance may poll the shared queue; database leases and compare-and-set transitions
//! provide the cross-instance coordination.

use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;
use uuid::Uuid;

use crate::billing::{BillingState, SubscriptionProvider};
use crate::error::Result;
use crate::org_deletion::{advance, claim_due};

/// Keep idle queue polling bounded without turning a quiet server into a busy loop.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Start one queue runner with a process-unique lease owner.
pub fn spawn(pool: PgPool, billing: Option<BillingState>) {
    let worker_id = format!("sotto-deletion-worker-{}", Uuid::new_v4());
    let provider = billing.map(|billing| billing.provider());
    tokio::spawn(run(pool, worker_id, provider));
}

/// Claim and advance at most one due operation. This small seam keeps the loop testable and makes
/// a database or provider error leave the lease for normal expiry rather than guessing a state.
pub async fn run_once(
    pool: &PgPool,
    worker_id: &str,
    provider: Option<&dyn SubscriptionProvider>,
) -> Result<bool> {
    let Some(lease) = claim_due(pool, worker_id).await? else {
        return Ok(false);
    };
    advance(pool, &lease, provider).await?;
    Ok(true)
}

async fn run(pool: PgPool, worker_id: String, provider: Option<Arc<dyn SubscriptionProvider>>) {
    loop {
        let provider_ref = provider.as_deref();
        match run_once(&pool, &worker_id, provider_ref).await {
            Ok(true) => continue,
            Ok(false) => tokio::time::sleep(POLL_INTERVAL).await,
            Err(error) => {
                eprintln!("organisation deletion worker: {error}");
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        }
    }
}
