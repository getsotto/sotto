//! Bounded recovery loop for interrupted hosted personal Checkout creation.
//!
//! The worker retries only the persisted offer, allocation binding, expiry and Stripe
//! idempotency key. A session is never treated as payment; invoice webhooks remain authoritative.

use std::time::Duration;

use crate::billing::{self, BillingState};
use sqlx::PgPool;

const MAX_OPERATIONS_PER_TICK: usize = 8;

pub fn spawn(pool: PgPool, billing: BillingState) {
    tokio::spawn(async move {
        loop {
            let mut processed = 0;
            while processed < MAX_OPERATIONS_PER_TICK {
                match billing::recover_personal_billing_operation_once(&pool, &billing).await {
                    Ok(true) => processed += 1,
                    Ok(false) => break,
                    Err(error) => {
                        eprintln!("Cloud billing recovery failed: {error}");
                        break;
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(if processed == 0 { 30 } else { 2 })).await;
        }
    });
}
