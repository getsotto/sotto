//! Background runner for the staged organisation-deletion lifecycle.
//!
//! The worker is opt-in until the deletion enablement checklist is complete. Once enabled, every
//! server instance may poll the shared queue; database leases and compare-and-set transitions
//! provide the cross-instance coordination.

use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;
use uuid::Uuid;

use crate::billing::SubscriptionProvider;
use crate::error::Result;
use crate::org_deletion::{advance, claim_due};

/// Keep queue polling bounded without turning a quiet or repeatedly-ready server into a busy loop.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Start one queue runner with a process-unique lease owner.
pub fn spawn(pool: PgPool, provider: Option<Arc<dyn SubscriptionProvider>>) {
    let worker_id = format!("sotto-deletion-worker-{}", Uuid::new_v4());
    tokio::spawn(run(pool, worker_id, provider));
}

/// Claim and advance at most one due operation. This small seam keeps the loop testable and makes
/// a database or internal error leaves the lease for normal expiry rather than guessing a state.
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
            Ok(true) => {
                // Pace successive claims so an inconsistent provider cannot create a tight loop.
                tokio::time::sleep(POLL_INTERVAL).await;
                continue;
            }
            Ok(false) => tokio::time::sleep(POLL_INTERVAL).await,
            Err(error) => {
                // Provider failures become lifecycle retries in advance; this arm covers database
                // and internal errors that must leave the lease for expiry.
                eprintln!("organisation deletion worker: {error}");
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        }
    }
}
