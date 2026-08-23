//! Internal operational metrics for the staged organisation-deletion worker.
//!
//! The snapshot is deliberately a database query rather than process-local state. A future
//! exporter can therefore observe all workers after a restart or while several server instances
//! are running, without exposing deletion details through the public API.

use serde::Serialize;
use sqlx::{PgPool, Postgres, Transaction};

use crate::error::Result;

/// Provider calls made while cancelling a linked subscription.
pub const PROVIDER_CANCELLATION_ATTEMPTS: &str = "provider_cancellation_attempts";
/// Provider status calls made while reconciling retention or recovery.
pub const PROVIDER_RECONCILIATION_ATTEMPTS: &str = "provider_reconciliation_attempts";
/// Due work reclaimed after another worker's lease expired.
pub const LEASE_EXPIRIES: &str = "lease_expiries";
/// Worker results rejected by a state-version or lease compare-and-set.
pub const STALE_COMPARE_AND_SET: &str = "stale_compare_and_set";
/// Final purge outcomes.
pub const PURGE_ATTEMPTS: &str = "purge_attempts";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StateMetric {
    /// The persisted lifecycle state.
    pub state: String,
    /// Number of operations currently in this state.
    pub count: i64,
    /// Age of the oldest operation in its current non-terminal state, or zero for terminal states.
    pub oldest_age_seconds: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CounterMetric {
    /// The fixed operational metric name.
    pub metric: String,
    /// The fixed, sanitised outcome label.
    pub outcome: String,
    /// Monotonic counter value.
    pub value: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PurgeDurationMetric {
    /// Number of completed purges with both timestamps present.
    pub count: i64,
    /// Average completed purge duration in seconds.
    pub average_seconds: i64,
    /// Longest completed purge duration in seconds.
    pub maximum_seconds: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeletionMetricsSnapshot {
    /// Current counts and oldest ages by lifecycle state.
    pub states: Vec<StateMetric>,
    /// Durable worker counters by metric and outcome.
    pub counters: Vec<CounterMetric>,
    /// Aggregate duration of completed purges.
    pub purge_duration: PurgeDurationMetric,
}

/// Increment one aggregate counter inside the lifecycle transaction that caused the event.
/// Provider outcomes must be sanitised labels, never provider response text.
pub async fn increment_tx(
    tx: &mut Transaction<'_, Postgres>,
    metric: &str,
    outcome: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO organization_deletion_metric_counters (metric, outcome, value) \
         VALUES ($1, $2, 1) \
         ON CONFLICT (metric, outcome) DO UPDATE SET value = \
         organization_deletion_metric_counters.value + 1, updated_at = now()",
    )
    .bind(metric)
    .bind(outcome)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Read the aggregate state and worker counters for a future exporter or operator adapter.
pub async fn snapshot(pool: &PgPool) -> Result<DeletionMetricsSnapshot> {
    let states = sqlx::query_as::<_, (String, i64, i64)>(
        "SELECT state, count(*)::bigint, \
                CASE WHEN state NOT IN ('cancelled', 'completed') \
                     THEN EXTRACT(EPOCH FROM (now() - min(state_entered_at)))::bigint \
                     ELSE 0 END \
         FROM organization_deletions \
         GROUP BY state ORDER BY state",
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|(state, count, oldest_age_seconds)| StateMetric {
        state,
        count,
        oldest_age_seconds,
    })
    .collect();

    let counters = sqlx::query_as::<_, (String, String, i64)>(
        "SELECT metric, outcome, value \
         FROM organization_deletion_metric_counters \
         ORDER BY metric, outcome",
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|(metric, outcome, value)| CounterMetric {
        metric,
        outcome,
        value,
    })
    .collect();

    let (count, average_seconds, maximum_seconds): (i64, i64, i64) = sqlx::query_as(
        "SELECT count(*)::bigint, \
                COALESCE(AVG(EXTRACT(EPOCH FROM (completed_at - purge_started_at)))::bigint, 0), \
                COALESCE(MAX(EXTRACT(EPOCH FROM (completed_at - purge_started_at)))::bigint, 0) \
         FROM organization_deletions \
         WHERE state = 'completed' AND purge_started_at IS NOT NULL AND completed_at IS NOT NULL",
    )
    .fetch_one(pool)
    .await?;

    Ok(DeletionMetricsSnapshot {
        states,
        counters,
        purge_duration: PurgeDurationMetric {
            count,
            average_seconds,
            maximum_seconds,
        },
    })
}
