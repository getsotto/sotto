//! Internal operational metrics for the staged organisation-deletion worker.
//!
//! The snapshot is deliberately a database query rather than process-local state. A future
//! exporter can therefore observe all workers after a restart or while several server instances
//! are running, without exposing deletion details through the public API.

use serde::Serialize;
use sqlx::{PgPool, Postgres, Transaction};

use crate::error::Result;

pub const PROVIDER_CANCELLATION_ATTEMPTS: &str = "provider_cancellation_attempts";
pub const PROVIDER_RECONCILIATION_ATTEMPTS: &str = "provider_reconciliation_attempts";
pub const LEASE_EXPIRIES: &str = "lease_expiries";
pub const STALE_COMPARE_AND_SET: &str = "stale_compare_and_set";
pub const PURGE_ATTEMPTS: &str = "purge_attempts";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StateMetric {
    pub state: String,
    pub count: i64,
    pub oldest_age_seconds: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CounterMetric {
    pub metric: String,
    pub outcome: String,
    pub value: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PurgeDurationMetric {
    pub count: i64,
    pub average_seconds: i64,
    pub maximum_seconds: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeletionMetricsSnapshot {
    pub states: Vec<StateMetric>,
    pub counters: Vec<CounterMetric>,
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
                EXTRACT(EPOCH FROM (now() - min(requested_at)))::bigint \
         FROM organization_deletions \
         WHERE state NOT IN ('cancelled', 'completed') \
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
