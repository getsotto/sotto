//! Internal operational metrics for the staged organisation-deletion worker.
//!
//! The snapshot is deliberately a database query rather than process-local state. The protected
//! exporter can therefore observe all workers after a restart or while several server instances
//! are running, without exposing deletion details through the user-facing API.

use serde::Serialize;
use sqlx::{PgPool, Postgres, Transaction};
use subtle::ConstantTimeEq;

use axum::extract::State;
use axum::http::header::{AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

use crate::error::{Error, Result};
use crate::state::AppState;

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
    /// Retention operations whose purge deadline has passed but have not entered purging.
    pub purge_due_count: i64,
}

const PROMETHEUS_CONTENT_TYPE: &str = "text/plain; version=0.0.4";

/// Build the protected Prometheus scrape endpoint. It stays unavailable until an operator sets a
/// dedicated bearer token, so adding the route cannot accidentally publish lifecycle data.
pub fn router() -> Router<AppState> {
    Router::new().route("/ops/organisation-deletion/metrics", get(export))
}

/// `GET /ops/organisation-deletion/metrics` - return aggregate deletion metrics to an operator
/// bearer token, never organisation identifiers or provider text.
async fn export(State(state): State<AppState>, headers: HeaderMap) -> Result<Response> {
    let expected = state
        .organisation_deletion_metrics_token
        .as_deref()
        .ok_or_else(|| {
            Error::NotConfigured("organisation-deletion metrics are not enabled".into())
        })?;
    let provided = bearer_token(&headers).ok_or(Error::Unauthorized)?;
    if !token_matches(expected, provided) {
        return Err(Error::Unauthorized);
    }

    let body = render_prometheus(&snapshot(&state.pool).await?);
    let mut response = body.into_response();
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static(PROMETHEUS_CONTENT_TYPE),
    );
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

// Compare the operator secret without making a token-length or prefix match observable.
fn token_matches(expected: &str, provided: &str) -> bool {
    expected.as_bytes().ct_eq(provided.as_bytes()).into()
}

fn render_prometheus(snapshot: &DeletionMetricsSnapshot) -> String {
    use std::fmt::Write;

    let mut output = String::new();
    output.push_str(
        "# HELP sotto_organisation_deletion_operations Current deletion operations by state.\n",
    );
    output.push_str("# TYPE sotto_organisation_deletion_operations gauge\n");
    output.push_str(
        "# HELP sotto_organisation_deletion_oldest_age_seconds Age of the oldest operation in its current state.\n",
    );
    output.push_str("# TYPE sotto_organisation_deletion_oldest_age_seconds gauge\n");
    for state_metric in &snapshot.states {
        let state = escape_label(&state_metric.state);
        writeln!(
            output,
            "sotto_organisation_deletion_operations{{state=\"{state}\"}} {}",
            state_metric.count
        )
        .expect("writing to a String cannot fail");
        writeln!(
            output,
            "sotto_organisation_deletion_oldest_age_seconds{{state=\"{state}\"}} {}",
            state_metric.oldest_age_seconds
        )
        .expect("writing to a String cannot fail");
    }

    output.push_str(
        "# HELP sotto_organisation_deletion_attempts_total Durable worker attempts by metric and outcome.\n",
    );
    output.push_str("# TYPE sotto_organisation_deletion_attempts_total counter\n");
    for counter in &snapshot.counters {
        let metric = escape_label(&counter.metric);
        let outcome = escape_label(&counter.outcome);
        writeln!(
            output,
            "sotto_organisation_deletion_attempts_total{{metric=\"{metric}\",outcome=\"{outcome}\"}} {}",
            counter.value
        )
        .expect("writing to a String cannot fail");
    }

    output.push_str(
        "# HELP sotto_organisation_deletion_purge_duration_count Completed purge count.\n",
    );
    output.push_str("# TYPE sotto_organisation_deletion_purge_duration_count gauge\n");
    writeln!(
        output,
        "sotto_organisation_deletion_purge_duration_count {}",
        snapshot.purge_duration.count
    )
    .expect("writing to a String cannot fail");
    output.push_str(
        "# HELP sotto_organisation_deletion_purge_duration_average_seconds Average completed purge duration.\n",
    );
    output.push_str("# TYPE sotto_organisation_deletion_purge_duration_average_seconds gauge\n");
    writeln!(
        output,
        "sotto_organisation_deletion_purge_duration_average_seconds {}",
        snapshot.purge_duration.average_seconds
    )
    .expect("writing to a String cannot fail");
    output.push_str(
        "# HELP sotto_organisation_deletion_purge_duration_maximum_seconds Longest completed purge duration.\n",
    );
    output.push_str("# TYPE sotto_organisation_deletion_purge_duration_maximum_seconds gauge\n");
    writeln!(
        output,
        "sotto_organisation_deletion_purge_duration_maximum_seconds {}",
        snapshot.purge_duration.maximum_seconds
    )
    .expect("writing to a String cannot fail");
    output.push_str(
        "# HELP sotto_organisation_deletion_purge_due_count Retention operations past their purge deadline.\n",
    );
    output.push_str("# TYPE sotto_organisation_deletion_purge_due_count gauge\n");
    writeln!(
        output,
        "sotto_organisation_deletion_purge_due_count {}",
        snapshot.purge_due_count
    )
    .expect("writing to a String cannot fail");
    output
}

fn escape_label(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
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

/// Read the aggregate state and worker counters for the protected exporter and operator tooling.
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

    let purge_due_count: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM organization_deletions \
         WHERE state = 'retention' AND purge_after <= now()",
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
        purge_due_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prometheus_render_uses_stable_metric_names_and_labels() {
        let text = render_prometheus(&DeletionMetricsSnapshot {
            states: vec![StateMetric {
                state: "cancelling_billing".into(),
                count: 2,
                oldest_age_seconds: 86_401,
            }],
            counters: vec![CounterMetric {
                metric: LEASE_EXPIRIES.into(),
                outcome: "reclaimed".into(),
                value: 3,
            }],
            purge_duration: PurgeDurationMetric {
                count: 4,
                average_seconds: 12,
                maximum_seconds: 30,
            },
            purge_due_count: 1,
        });

        assert!(
            text.contains("sotto_organisation_deletion_operations{state=\"cancelling_billing\"} 2")
        );
        assert!(text.contains(
            "sotto_organisation_deletion_attempts_total{metric=\"lease_expiries\",outcome=\"reclaimed\"} 3"
        ));
        assert!(text.contains("sotto_organisation_deletion_purge_due_count 1"));
    }

    #[test]
    fn bearer_tokens_require_the_exact_configured_value() {
        assert!(token_matches("metrics-secret", "metrics-secret"));
        assert!(!token_matches("metrics-secret", "metrics-secret-extra"));
        assert!(!token_matches("metrics-secret", " metrics-secret"));
    }
}
