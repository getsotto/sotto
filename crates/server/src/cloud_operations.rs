//! Protected, low-cardinality Cloud lifecycle diagnostics and supported coverage repair.
//!
//! Public liveness/readiness remains in [`crate::health`]. This surface is dark unless a dedicated
//! metrics or repair credential is configured. Metrics contain only finite durable-state labels;
//! the repair queue exposes opaque fingerprints and bounded reason codes, never provider ids.

use axum::extract::{Path, Query, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, Request};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{middleware, Json, Router};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use std::time::Duration;
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::cloud_provider::ProviderEnvironment;
use crate::cloud_provider_stripe_event_repair::StripeEventRepairDisposition;
use crate::cloud_provider_stripe_http::StripeReadError;
use crate::error::{Error, Result};
use crate::state::AppState;

const MAX_PAGE_SIZE: i64 = 50;
const REPAIR_LEASE_SECONDS: i64 = 120;
const REPAIR_COOLDOWN_SECONDS: i64 = 300;
const PROMETHEUS_CONTENT_TYPE: &str = "text/plain; version=0.0.4";
const COVERAGE_REJECTION_REASONS: &[&str] = &[
    "allocation_reference_invalid",
    "allocation_reference_missing",
    "allocation_reference_unrecognised",
    "founding_boundary_missing",
    "founding_invoice_boundary_missing",
    "invoice_binding_invalid",
    "invoice_binding_mismatch",
    "invoice_customer_missing",
    "invoice_evidence_changed",
    "invoice_evidence_unsupported",
    "invoice_id_missing",
    "invoice_item_missing",
    "invoice_payload_malformed",
    "invoice_subscription_missing",
    "payment_count_unsupported",
    "payment_settlement_unsupported",
    "provider_allocation_invalid",
    "provider_allocation_mismatch",
    "sponsored_invoice_allocation_mismatch",
    "sponsored_invoice_binding_mismatch",
    "sponsored_invoice_evidence_rejected",
    "sponsored_invoice_has_no_named_seats",
    "sponsored_invoice_item_binding_mismatch",
    "sponsored_invoice_manifest_mismatch",
    "sponsored_invoice_manifest_rejected",
    "sponsored_invoice_no_eligible_seats",
    "sponsored_invoice_price_unsupported",
    "sponsored_invoice_scope_exceeded",
    "sponsored_invoice_seat_ambiguous",
    "stripe_context_invalid",
    "stripe_coverage_binding_required",
    "stripe_invoice_evidence_rejected",
    "stripe_payment_evidence_rejected",
    "unlisted",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LifecycleStateMetric {
    pub subsystem: String,
    pub state: String,
    pub count: i64,
    pub oldest_age_seconds: i64,
    pub attempts: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuntimeFeatureMetric {
    pub feature: String,
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CloudOperationsSnapshot {
    pub states: Vec<LifecycleStateMetric>,
    pub features: Vec<RuntimeFeatureMetric>,
    pub coverage_rejections_open: i64,
    pub coverage_rejections_unrecoverable: i64,
    pub coverage_rejections_oldest_age_seconds: i64,
    pub tombstones_total: i64,
    pub founding_places_awarded: i64,
    pub founding_reservations_open: i64,
    pub founding_refunds_required: i64,
    pub founding_price_notices_unresolved: i64,
    pub stripe_event_history_gaps: i64,
    pub coverage_publication_latency_seconds: i64,
    pub coverage_publication_events_24h: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CoverageRejectionSummary {
    pub fingerprint: String,
    pub coverage_kind: String,
    pub reason_code: String,
    pub state: String,
    pub repair_attempt_count: i32,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CoverageRejectionPage {
    pub incidents: Vec<CoverageRejectionSummary>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CoverageRepairRequest {
    /// Declared attribution only; the shared bearer credential authenticates the action.
    pub operator_label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CoverageRepairResponse {
    pub fingerprint: String,
    pub outcome: &'static str,
}

#[derive(Debug, Deserialize)]
pub struct PageQuery {
    pub cursor: Option<String>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/ops/cloud/metrics", get(metrics))
        .route("/ops/cloud/coverage-rejections", get(list_rejections))
        .route(
            "/ops/cloud/coverage-rejections/{fingerprint}/repair",
            post(repair_rejection),
        )
        .layer(middleware::from_fn(no_store))
}

async fn no_store(request: Request<axum::body::Body>, next: Next) -> Response {
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn metrics(State(state): State<AppState>, headers: HeaderMap) -> Result<Response> {
    authenticate(
        &headers,
        state.cloud_operations_metrics_token.as_deref(),
        "Cloud operations metrics are not enabled",
    )?;
    let snapshot = tokio::time::timeout(Duration::from_secs(5), snapshot(&state))
        .await
        .map_err(|_| Error::Internal("Cloud operations metrics query timed out".into()))??;
    let mut response = render_prometheus(&snapshot).into_response();
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static(PROMETHEUS_CONTENT_TYPE),
    );
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

fn render_prometheus(snapshot: &CloudOperationsSnapshot) -> String {
    use std::fmt::Write;

    let mut output = String::new();
    output.push_str("# HELP sotto_cloud_lifecycle_operations Current durable Cloud operations by component and state.\n");
    output.push_str("# TYPE sotto_cloud_lifecycle_operations gauge\n");
    output.push_str(
        "# HELP sotto_cloud_lifecycle_oldest_age_seconds Age of oldest nonterminal work.\n",
    );
    output.push_str("# TYPE sotto_cloud_lifecycle_oldest_age_seconds gauge\n");
    output.push_str(
        "# HELP sotto_cloud_lifecycle_retry_attempts Retry attempts on current work records.\n",
    );
    output.push_str("# TYPE sotto_cloud_lifecycle_retry_attempts gauge\n");
    for metric in &snapshot.states {
        writeln!(
            output,
            "sotto_cloud_lifecycle_operations{{component=\"{}\",state=\"{}\"}} {}",
            metric.subsystem, metric.state, metric.count
        )
        .expect("writing to a String cannot fail");
        writeln!(
            output,
            "sotto_cloud_lifecycle_oldest_age_seconds{{component=\"{}\",state=\"{}\"}} {}",
            metric.subsystem, metric.state, metric.oldest_age_seconds
        )
        .expect("writing to a String cannot fail");
        writeln!(
            output,
            "sotto_cloud_lifecycle_retry_attempts{{component=\"{}\",state=\"{}\"}} {}",
            metric.subsystem, metric.state, metric.attempts
        )
        .expect("writing to a String cannot fail");
    }
    output.push_str("# HELP sotto_cloud_runtime_feature_state Optional Cloud feature configuration state (enabled, disabled, unconfigured).\n");
    output.push_str("# TYPE sotto_cloud_runtime_feature_state gauge\n");
    for feature in &snapshot.features {
        writeln!(
            output,
            "sotto_cloud_runtime_feature_state{{feature=\"{}\",state=\"{}\"}} 1",
            feature.feature, feature.state
        )
        .expect("writing to a String cannot fail");
    }
    output.push_str("# HELP sotto_cloud_coverage_rejections_open Current paid invoices without accepted coverage evidence.\n");
    output.push_str("# TYPE sotto_cloud_coverage_rejections_open gauge\n");
    writeln!(
        output,
        "sotto_cloud_coverage_rejections_open {}",
        snapshot.coverage_rejections_open
    )
    .expect("writing to a String cannot fail");
    output.push_str("# HELP sotto_cloud_coverage_rejections_oldest_age_seconds Age of oldest open coverage rejection.\n");
    output.push_str("# TYPE sotto_cloud_coverage_rejections_oldest_age_seconds gauge\n");
    writeln!(
        output,
        "sotto_cloud_coverage_rejections_oldest_age_seconds {}",
        snapshot.coverage_rejections_oldest_age_seconds
    )
    .expect("writing to a String cannot fail");
    output.push_str("# HELP sotto_cloud_coverage_rejections_unrecoverable Current coverage rejections whose provider event is no longer retrievable.\n");
    output.push_str("# TYPE sotto_cloud_coverage_rejections_unrecoverable gauge\n");
    writeln!(
        output,
        "sotto_cloud_coverage_rejections_unrecoverable {}",
        snapshot.coverage_rejections_unrecoverable
    )
    .expect("writing to a String cannot fail");
    output.push_str(
        "# HELP sotto_cloud_retention_tombstones_total Durable deletion tombstones recorded.\n",
    );
    output.push_str("# TYPE sotto_cloud_retention_tombstones_total counter\n");
    writeln!(
        output,
        "sotto_cloud_retention_tombstones_total {}",
        snapshot.tombstones_total
    )
    .expect("writing to a String cannot fail");
    output.push_str(
        "# HELP sotto_cloud_founding_places_awarded Founding places confirmed by payment.\n",
    );
    output.push_str("# TYPE sotto_cloud_founding_places_awarded gauge\n");
    writeln!(
        output,
        "sotto_cloud_founding_places_awarded {}",
        snapshot.founding_places_awarded
    )
    .expect("writing to a String cannot fail");
    output.push_str(
        "# HELP sotto_cloud_founding_reservations_open Temporary founding reservations.\n",
    );
    output.push_str("# TYPE sotto_cloud_founding_reservations_open gauge\n");
    writeln!(
        output,
        "sotto_cloud_founding_reservations_open {}",
        snapshot.founding_reservations_open
    )
    .expect("writing to a String cannot fail");
    output.push_str(
        "# HELP sotto_cloud_founding_refunds_required Founding payments awaiting refund.\n",
    );
    output.push_str("# TYPE sotto_cloud_founding_refunds_required gauge\n");
    writeln!(
        output,
        "sotto_cloud_founding_refunds_required {}",
        snapshot.founding_refunds_required
    )
    .expect("writing to a String cannot fail");
    output.push_str("# HELP sotto_cloud_founding_price_notices_unresolved Founding price notices not safely queued.\n");
    output.push_str("# TYPE sotto_cloud_founding_price_notices_unresolved gauge\n");
    writeln!(
        output,
        "sotto_cloud_founding_price_notices_unresolved {}",
        snapshot.founding_price_notices_unresolved
    )
    .expect("writing to a String cannot fail");
    output.push_str("# HELP sotto_cloud_stripe_event_history_gaps Accounts with an unresolved Stripe history gap.\n");
    output.push_str("# TYPE sotto_cloud_stripe_event_history_gaps gauge\n");
    writeln!(
        output,
        "sotto_cloud_stripe_event_history_gaps {}",
        snapshot.stripe_event_history_gaps
    )
    .expect("writing to a String cannot fail");
    output.push_str("# HELP sotto_cloud_coverage_publication_latency_seconds Maximum webhook-receipt-to-coverage publication latency in the last 24 hours.\n");
    output.push_str("# TYPE sotto_cloud_coverage_publication_latency_seconds gauge\n");
    writeln!(
        output,
        "sotto_cloud_coverage_publication_latency_seconds {}",
        snapshot.coverage_publication_latency_seconds
    )
    .expect("writing to a String cannot fail");
    output.push_str("# HELP sotto_cloud_coverage_publication_events_24h Applied provider events published in the last 24 hours.\n");
    output.push_str("# TYPE sotto_cloud_coverage_publication_events_24h gauge\n");
    writeln!(
        output,
        "sotto_cloud_coverage_publication_events_24h {}",
        snapshot.coverage_publication_events_24h
    )
    .expect("writing to a String cannot fail");
    output
}

async fn list_rejections(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<PageQuery>,
) -> Result<Response> {
    authenticate(
        &headers,
        state.cloud_operations_metrics_token.as_deref(),
        "Cloud operations metrics are not enabled",
    )?;
    if query
        .cursor
        .as_deref()
        .is_some_and(|cursor| !is_fingerprint(cursor))
    {
        return Err(Error::BadRequest("invalid cursor".into()));
    }
    let Some(billing) = state.billing.as_ref() else {
        return Err(Error::NotConfigured(
            "Cloud billing is not configured".into(),
        ));
    };
    let Some((coverage, _)) = billing.personal_coverage() else {
        return Err(Error::NotConfigured(
            "Cloud coverage is not configured".into(),
        ));
    };
    let page = tokio::time::timeout(
        Duration::from_secs(5),
        rejection_page(
            &state.pool,
            &coverage.account_id,
            coverage.environment,
            query.cursor.as_deref(),
        ),
    )
    .await
    .map_err(|_| Error::Internal("Cloud rejection query timed out".into()))??;
    let mut response = Json(page).into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

async fn repair_rejection(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(fingerprint): Path<String>,
    Json(request): Json<CoverageRepairRequest>,
) -> Result<Response> {
    authenticate(
        &headers,
        state.cloud_coverage_repair_token.as_deref(),
        "Cloud coverage repair is not enabled",
    )?;
    if !is_fingerprint(&fingerprint) || !valid_operator_label(&request.operator_label) {
        return Err(Error::BadRequest("invalid repair request".into()));
    }
    let Some(billing) = state.billing.as_ref() else {
        return Err(Error::NotConfigured(
            "Cloud billing is not configured".into(),
        ));
    };
    let Some((coverage, stripe)) = billing.personal_coverage() else {
        return Err(Error::NotConfigured(
            "Cloud coverage is not configured".into(),
        ));
    };
    let lease = claim_rejection(
        &state.pool,
        &fingerprint,
        &coverage.account_id,
        coverage.environment,
    )
    .await?;
    let mut session = stripe.session();
    let authenticated = match stripe.event_payload(&mut session, &lease.event_id).await {
        Ok(event) => event,
        Err(StripeReadError::ResourceMissing) => {
            finish_repair(
                &state.pool,
                &lease,
                &request.operator_label,
                "event_unavailable",
                false,
            )
            .await?;
            let mut response = Json(CoverageRepairResponse {
                fingerprint,
                outcome: "event_unavailable",
            })
            .into_response();
            response
                .headers_mut()
                .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
            return Ok(response);
        }
        Err(_) => {
            finish_repair(
                &state.pool,
                &lease,
                &request.operator_label,
                "provider_error",
                false,
            )
            .await?;
            return Err(Error::Upstream(
                "coverage repair provider read failed".into(),
            ));
        }
    };
    let outcome = crate::billing::repair_authenticated_stripe_event(
        &state.pool,
        billing,
        &authenticated,
        epoch_now(),
    )
    .await;
    let (label, resolved) = match outcome {
        Ok(StripeEventRepairDisposition::Applied) => ("applied", true),
        Ok(StripeEventRepairDisposition::AlreadyApplied) => ("already_applied", true),
        Ok(StripeEventRepairDisposition::Rejected(_)) => ("still_rejected", false),
        Ok(
            StripeEventRepairDisposition::Unresolved(_) | StripeEventRepairDisposition::Unrelated,
        ) => ("unresolved", false),
        Err(_) => {
            finish_repair(
                &state.pool,
                &lease,
                &request.operator_label,
                "provider_error",
                false,
            )
            .await?;
            return Err(Error::Upstream(
                "coverage repair could not be completed".into(),
            ));
        }
    };
    finish_repair(
        &state.pool,
        &lease,
        &request.operator_label,
        label,
        resolved,
    )
    .await?;
    let mut response = Json(CoverageRepairResponse {
        fingerprint,
        outcome: label,
    })
    .into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

fn authenticate(headers: &HeaderMap, expected: Option<&str>, missing: &str) -> Result<()> {
    let expected = expected.ok_or_else(|| Error::NotConfigured(missing.into()))?;
    let supplied = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or(Error::Unauthorized)?;
    let expected: [u8; 32] = Sha256::digest(expected.as_bytes()).into();
    let supplied: [u8; 32] = Sha256::digest(supplied.as_bytes()).into();
    if bool::from(expected.ct_eq(&supplied)) {
        Ok(())
    } else {
        Err(Error::Unauthorized)
    }
}

pub async fn record_coverage_rejection(
    pool: &PgPool,
    account_id: &str,
    environment: ProviderEnvironment,
    event_id: &str,
    coverage_kind: &str,
    reason_code: &str,
) -> Result<()> {
    if !matches!(coverage_kind, "personal" | "sponsored") || event_id.trim().is_empty() {
        return Err(Error::Internal(
            "invalid coverage rejection evidence".into(),
        ));
    }
    let reason_code = coverage_rejection_reason_code(reason_code);
    let fingerprint = rejection_fingerprint(account_id, environment, event_id, coverage_kind);
    sqlx::query(
        "INSERT INTO cloud_coverage_rejection_incidents \
         (fingerprint, provider_account_id, provider_environment, event_id, event_type, \
          coverage_kind, reason_code) VALUES ($1, $2, $3, $4, 'invoice.paid', $5, $6) \
         ON CONFLICT (provider_account_id, provider_environment, event_id, coverage_kind) \
         DO UPDATE SET reason_code = EXCLUDED.reason_code, updated_at = now() \
         WHERE cloud_coverage_rejection_incidents.state <> 'resolved'",
    )
    .bind(fingerprint)
    .bind(account_id)
    .bind(environment.as_str())
    .bind(event_id)
    .bind(coverage_kind)
    .bind(reason_code)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn resolve_coverage_rejections(
    pool: &PgPool,
    account_id: &str,
    environment: ProviderEnvironment,
    event_id: &str,
) -> Result<()> {
    sqlx::query(
        "UPDATE cloud_coverage_rejection_incidents SET state = 'resolved', \
         resolved_at = now(), repair_lease_token = NULL, repair_lease_until = NULL, updated_at = now() \
         WHERE provider_account_id = $1 AND provider_environment = $2 AND event_id = $3 \
           AND state = 'open'",
    )
    .bind(account_id)
    .bind(environment.as_str())
    .bind(event_id)
    .execute(pool)
    .await?;
    Ok(())
}

fn rejection_fingerprint(
    account_id: &str,
    environment: ProviderEnvironment,
    event_id: &str,
    coverage_kind: &str,
) -> String {
    let mut digest = Sha256::new();
    for part in [
        "sotto-cloud-rejection-v1",
        account_id,
        environment.as_str(),
        event_id,
        coverage_kind,
    ] {
        digest.update((part.len() as u64).to_be_bytes());
        digest.update(part.as_bytes());
    }
    format!("{:x}", digest.finalize())
}

async fn snapshot(state: &AppState) -> Result<CloudOperationsSnapshot> {
    let pool = &state.pool;
    let billing_configured = state.billing.is_some();
    let coverage_configured = state
        .billing
        .as_ref()
        .is_some_and(|billing| billing.personal_coverage().is_some());
    let notice_configured =
        state.contact_verification.is_some() && state.resend_webhook_secret.is_some();
    let retention_operator_configured = state.cloud_retention_operator_token.is_some();
    let purge_configured = retention_operator_configured
        && state.cloud_retention_destructive_token.is_some()
        && state.cloud_retention_evidence_hmac_key.is_some()
        && state.cloud_retention_build_sha.is_some();
    let features = vec![
        feature("cloud_sales", state.cloud_sales_enabled, billing_configured),
        feature(
            "provider_refresh_ingest",
            state.provider_refresh_ingest_enabled,
            coverage_configured,
        ),
        feature(
            "provider_refresh_worker",
            state.provider_refresh_worker_enabled,
            coverage_configured,
        ),
        feature(
            "event_reconciliation",
            state.provider_refresh_reconciliation_enabled,
            coverage_configured,
        ),
        feature(
            "authority_revalidation",
            state.provider_authority_revalidation_enabled,
            coverage_configured,
        ),
        feature(
            "coverage_authority_enforcement",
            state.cloud_coverage_authority_enforcement_enabled,
            coverage_configured,
        ),
        feature(
            "human_action_enforcement",
            state.cloud_action_enforcement_enabled,
            state.deployment_mode == crate::config::DeploymentMode::Cloud,
        ),
        feature(
            "machine_eligibility_enforcement",
            state.machine_eligibility_enforcement_enabled,
            state.deployment_mode == crate::config::DeploymentMode::Cloud,
        ),
        feature(
            "personal_billing_recovery",
            state.billing_recovery_worker_enabled,
            billing_configured,
        ),
        feature(
            "sponsored_billing_recovery",
            state.sponsored_billing_recovery_worker_enabled,
            billing_configured,
        ),
        feature(
            "founding_schedule",
            state.founding_schedule_worker_enabled,
            billing_configured,
        ),
        feature(
            "founding_price_notices",
            state.founding_price_notices_enabled,
            billing_configured,
        ),
        feature(
            "payer_transfer_routes",
            state.billing_transfers_enabled,
            billing_configured,
        ),
        feature(
            "payer_transfer_execution",
            state.billing_transfer_execution_enabled,
            billing_configured,
        ),
        feature(
            "notice_dispatch",
            state.cloud_notice_dispatch_enabled,
            notice_configured,
        ),
        feature(
            "personal_retention_dry_run",
            state.cloud_retention_dry_run_enabled,
            retention_operator_configured,
        ),
        feature(
            "shared_retention_dry_run",
            state.cloud_shared_retention_dry_run_enabled,
            retention_operator_configured,
        ),
        feature(
            "retention_purge",
            state.cloud_retention_purge_enabled,
            purge_configured,
        ),
    ];
    let rows = sqlx::query(
        "SELECT subsystem, state, count::bigint, oldest_age_seconds::bigint, attempts::bigint FROM ( \
         SELECT 'provider_refresh' AS subsystem, status AS state, count(*) AS count, \
           COALESCE(EXTRACT(EPOCH FROM (now() - min(created_at))), 0) AS oldest_age_seconds, \
           sum(attempt_count) AS attempts \
           FROM cloud_provider_refresh_jobs WHERE status <> 'completed' GROUP BY status \
         UNION ALL SELECT 'authority_revalidation', status, count(*), \
           COALESCE(EXTRACT(EPOCH FROM (now() - min(updated_at))), 0), sum(attempt_count) \
           FROM cloud_coverage_revalidation_jobs GROUP BY status \
         UNION ALL SELECT 'billing_recovery', state, count(*), \
           COALESCE(EXTRACT(EPOCH FROM (now() - min(updated_at))), 0), sum(reconciliation_attempt_count) \
           FROM billing_operations WHERE state IN ('pending','unknown','manual_review') GROUP BY state \
         UNION ALL SELECT 'sponsored_recovery', state, count(*), \
           COALESCE(EXTRACT(EPOCH FROM (now() - min(updated_at))), 0), sum(recovery_attempt_count) \
           FROM billing_sponsored_operations WHERE state IN ('pending','checkout_created','provider_pending','unknown','manual_review') GROUP BY state \
         UNION ALL SELECT 'founding_schedule', state, count(*), \
           COALESCE(EXTRACT(EPOCH FROM (now() - min(updated_at))), 0), sum(attempt_count) \
           FROM billing_founding_schedule_jobs WHERE state IN ('pending','unresolved') GROUP BY state \
         UNION ALL SELECT 'transfer_review', 'manual_review', count(*), \
           COALESCE(EXTRACT(EPOCH FROM (now() - min(updated_at))), 0), sum(execution_attempt_count) \
           FROM billing_transfer_intents WHERE execution_manual_review_required \
           GROUP BY execution_manual_review_required \
         UNION ALL SELECT 'notice_delivery', status, count(*), \
           CASE WHEN status IN ('pending','leased','accepted','delivery_unknown') \
             THEN COALESCE(EXTRACT(EPOCH FROM (now() - min(updated_at))), 0) ELSE 0 END, \
           sum(attempt_count) \
           FROM cloud_notice_outbox GROUP BY status \
         UNION ALL SELECT 'retention', state, count(*), \
           COALESCE(EXTRACT(EPOCH FROM (now() - min(updated_at))), 0), sum(attempt_count) \
           FROM cloud_retention_jobs WHERE state IN ('planned','dry_run','ready','leased','held','failed') GROUP BY state \
         UNION ALL SELECT 'authority', CASE WHEN receipt.fresh_until > now() THEN 'fresh' ELSE 'stale' END, count(*), \
           COALESCE(EXTRACT(EPOCH FROM (now() - min(receipt.observed_at))), 0), 0::bigint \
           FROM cloud_coverage_authority_receipts receipt \
           JOIN cloud_coverage_heads head ON head.beneficiary_id = receipt.beneficiary_id \
             AND head.current_revision = receipt.projection_revision \
           GROUP BY CASE WHEN receipt.fresh_until > now() THEN 'fresh' ELSE 'stale' END \
         UNION ALL SELECT 'authority', 'missing', count(DISTINCT source.beneficiary_id), \
           COALESCE(EXTRACT(EPOCH FROM (now() - min(source.registered_at))), 0), 0::bigint \
           FROM cloud_coverage_sources source \
           WHERE NOT EXISTS (SELECT 1 FROM cloud_coverage_source_retirements retirement \
             WHERE retirement.source_id = source.source_id) \
             AND NOT EXISTS (SELECT 1 FROM cloud_coverage_heads head \
               JOIN cloud_coverage_authority_receipts receipt \
                 ON receipt.beneficiary_id = head.beneficiary_id \
                AND receipt.projection_revision = head.current_revision \
               WHERE head.beneficiary_id = source.beneficiary_id) \
         UNION ALL SELECT 'stripe_event_repair', 'watermark', count(*), \
           COALESCE(EXTRACT(EPOCH FROM (now() - to_timestamp(min(completed_through)))), 0), 0::bigint \
           FROM stripe_event_reconciliation_state WHERE completed_through IS NOT NULL \
         UNION ALL SELECT 'stripe_event_repair', 'unresolved_event', count(*), \
           COALESCE(EXTRACT(EPOCH FROM (now() - to_timestamp(min(provider_created_at)))), 0), 0::bigint \
           FROM stripe_event_repair_receipts WHERE disposition = 'unresolved' \
         UNION ALL SELECT 'stripe_event_repair', COALESCE(last_outcome, 'not_started'), count(*), \
           COALESCE(EXTRACT(EPOCH FROM (now() - min(updated_at))), 0), 0::bigint \
           FROM stripe_event_reconciliation_state GROUP BY last_outcome \
         UNION ALL SELECT 'founding_price_notice', outcome, count(*), \
           CASE WHEN outcome <> 'retired' \
             THEN COALESCE(EXTRACT(EPOCH FROM (now() - min(updated_at))), 0) ELSE 0 END, \
           0::bigint \
           FROM billing_founding_price_notice_state GROUP BY outcome \
         UNION ALL SELECT 'stripe_event_repair', 'history_gap', count(*), \
           COALESCE(EXTRACT(EPOCH FROM (now() - min(updated_at))), 0), 0::bigint \
           FROM stripe_event_reconciliation_state WHERE history_gap_start IS NOT NULL \
           GROUP BY history_gap_start IS NOT NULL \
         ) AS lifecycle ORDER BY subsystem, state",
    )
    .fetch_all(pool)
    .await?;
    let states = rows
        .into_iter()
        .map(|row| {
            Ok(LifecycleStateMetric {
                subsystem: row.try_get("subsystem")?,
                state: row.try_get("state")?,
                count: row.try_get("count")?,
                oldest_age_seconds: row.try_get("oldest_age_seconds")?,
                attempts: row.try_get("attempts")?,
            })
        })
        .collect::<std::result::Result<Vec<_>, sqlx::Error>>()?;
    let (open_count, oldest_age): (i64, i64) = sqlx::query_as(
        "SELECT count(*)::bigint, COALESCE(EXTRACT(EPOCH FROM (now() - min(created_at)))::bigint, 0) \
         FROM cloud_coverage_rejection_incidents WHERE state IN ('open','repairing')",
    )
    .fetch_one(pool)
    .await?;
    let unrecoverable_count: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM cloud_coverage_rejection_incidents \
         WHERE state = 'unrecoverable'",
    )
    .fetch_one(pool)
    .await?;
    let tombstones_total: i64 =
        sqlx::query_scalar("SELECT count(*)::bigint FROM cloud_retention_tombstone_journal")
            .fetch_one(pool)
            .await?;
    let founding_places_awarded: i64 =
        sqlx::query_scalar("SELECT count(*)::bigint FROM billing_founding_awards")
            .fetch_one(pool)
            .await?;
    let founding_reservations_open: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM billing_founding_reservations WHERE status = 'reserved'",
    )
    .fetch_one(pool)
    .await?;
    let founding_refunds_required: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM billing_founding_reservations WHERE status = 'refund_required'",
    )
    .fetch_one(pool)
    .await?;
    let founding_price_notices_unresolved: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM billing_founding_price_notice_state \
         WHERE outcome IN ('late_unresolved', 'schedule_unresolved')",
    )
    .fetch_one(pool)
    .await?;
    let stripe_event_history_gaps: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM stripe_event_reconciliation_state \
         WHERE history_gap_start IS NOT NULL",
    )
    .fetch_one(pool)
    .await?;
    let (coverage_publication_latency_seconds, coverage_publication_events_24h): (i64, i64) =
        sqlx::query_as(
            "SELECT COALESCE(EXTRACT(EPOCH FROM max(GREATEST(processed_at - received_at, interval '0 seconds')))::bigint, 0), \
                    count(*)::bigint \
             FROM cloud_provider_event_receipts \
             WHERE event_type = 'invoice.paid' AND status = 'applied' \
               AND processed_at >= now() - interval '24 hours'",
        )
        .fetch_one(pool)
        .await?;
    Ok(CloudOperationsSnapshot {
        states,
        features,
        coverage_rejections_open: open_count,
        coverage_rejections_unrecoverable: unrecoverable_count,
        coverage_rejections_oldest_age_seconds: oldest_age,
        tombstones_total,
        founding_places_awarded,
        founding_reservations_open,
        founding_refunds_required,
        founding_price_notices_unresolved,
        stripe_event_history_gaps,
        coverage_publication_latency_seconds,
        coverage_publication_events_24h,
    })
}

fn feature(name: &str, enabled: bool, configured: bool) -> RuntimeFeatureMetric {
    let state = if !configured {
        "unconfigured"
    } else if enabled {
        "enabled"
    } else {
        "disabled"
    };
    RuntimeFeatureMetric {
        feature: name.into(),
        state: state.into(),
    }
}

async fn rejection_page(
    pool: &PgPool,
    account_id: &str,
    environment: ProviderEnvironment,
    cursor: Option<&str>,
) -> Result<CoverageRejectionPage> {
    if let Some(cursor) = cursor {
        let cursor_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM cloud_coverage_rejection_incidents \
             WHERE fingerprint = $1 AND provider_account_id = $2 AND provider_environment = $3)",
        )
        .bind(cursor)
        .bind(account_id)
        .bind(environment.as_str())
        .fetch_one(pool)
        .await?;
        if !cursor_exists {
            return Err(Error::BadRequest("invalid cursor".into()));
        }
    }
    let rows = sqlx::query(
        "SELECT fingerprint, coverage_kind, reason_code, state, repair_attempt_count, \
                to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS created_at \
         FROM cloud_coverage_rejection_incidents incident \
         WHERE provider_account_id = $1 AND provider_environment = $2 \
           AND state IN ('open','repairing') \
           AND ($3::text IS NULL OR (created_at, fingerprint) > ( \
             SELECT created_at, fingerprint FROM cloud_coverage_rejection_incidents WHERE fingerprint = $3)) \
         ORDER BY created_at, fingerprint LIMIT $4",
    )
    .bind(account_id)
    .bind(environment.as_str())
    .bind(cursor)
    .bind(MAX_PAGE_SIZE + 1)
    .fetch_all(pool)
    .await?;
    let has_more = rows.len() as i64 > MAX_PAGE_SIZE;
    let mut incidents = rows
        .into_iter()
        .take(MAX_PAGE_SIZE as usize)
        .map(|row| {
            Ok(CoverageRejectionSummary {
                fingerprint: row.try_get("fingerprint")?,
                coverage_kind: row.try_get("coverage_kind")?,
                reason_code: row.try_get("reason_code")?,
                state: row.try_get("state")?,
                repair_attempt_count: row.try_get("repair_attempt_count")?,
                created_at: row.try_get("created_at")?,
            })
        })
        .collect::<std::result::Result<Vec<_>, sqlx::Error>>()?;
    let next_cursor = if has_more {
        incidents
            .last()
            .map(|incident| incident.fingerprint.clone())
    } else {
        None
    };
    Ok(CoverageRejectionPage {
        incidents: std::mem::take(&mut incidents),
        next_cursor,
    })
}

#[derive(Debug)]
struct RejectionLease {
    fingerprint: String,
    event_id: String,
    lease_token: String,
}

async fn claim_rejection(
    pool: &PgPool,
    fingerprint: &str,
    account_id: &str,
    environment: ProviderEnvironment,
) -> Result<RejectionLease> {
    let lease_token = Uuid::new_v4().to_string();
    let row = sqlx::query(
        "UPDATE cloud_coverage_rejection_incidents \
         SET state = 'repairing', repair_lease_token = $4, \
             repair_lease_until = now() + ($5 * interval '1 second'), \
             repair_attempt_count = LEAST(repair_attempt_count + 1, 2147483647), \
             last_attempt_at = now(), updated_at = now() \
         WHERE fingerprint = $1 AND provider_account_id = $2 AND provider_environment = $3 \
           AND (last_attempt_at IS NULL OR last_attempt_at <= now() - ($6 * interval '1 second')) \
           AND (state = 'open' OR (state = 'repairing' AND repair_lease_until <= now())) \
         RETURNING event_id",
    )
    .bind(fingerprint)
    .bind(account_id)
    .bind(environment.as_str())
    .bind(&lease_token)
    .bind(REPAIR_LEASE_SECONDS)
    .bind(REPAIR_COOLDOWN_SECONDS)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM cloud_coverage_rejection_incidents \
             WHERE fingerprint = $1 AND provider_account_id = $2 AND provider_environment = $3)",
        )
        .bind(fingerprint)
        .bind(account_id)
        .bind(environment.as_str())
        .fetch_one(pool)
        .await?;
        return Err(if exists {
            Error::Conflict("coverage repair is unavailable or its retry cooldown is active".into())
        } else {
            Error::NotFound("coverage rejection was not found".into())
        });
    };
    Ok(RejectionLease {
        fingerprint: fingerprint.to_owned(),
        event_id: row.try_get("event_id")?,
        lease_token,
    })
}

async fn finish_repair(
    pool: &PgPool,
    lease: &RejectionLease,
    actor_label: &str,
    outcome: &str,
    resolved: bool,
) -> Result<()> {
    let state = if outcome == "event_unavailable" {
        "unrecoverable"
    } else if resolved {
        "resolved"
    } else {
        "open"
    };
    let mut tx = pool.begin().await?;
    let result = sqlx::query(
        "UPDATE cloud_coverage_rejection_incidents SET state = $3, \
         repair_lease_token = NULL, repair_lease_until = NULL, \
         resolved_at = CASE WHEN $4 THEN now() ELSE NULL END, updated_at = now() \
         WHERE fingerprint = $1 AND state = 'repairing' AND repair_lease_token = $2",
    )
    .bind(&lease.fingerprint)
    .bind(&lease.lease_token)
    .bind(state)
    .bind(resolved)
    .execute(&mut *tx)
    .await?;
    if result.rows_affected() != 1 {
        return Err(Error::Conflict("coverage repair lease was lost".into()));
    }
    sqlx::query(
        "INSERT INTO cloud_coverage_rejection_actions (fingerprint, actor_label, outcome) \
         VALUES ($1, $2, $3)",
    )
    .bind(&lease.fingerprint)
    .bind(actor_label)
    .bind(outcome)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

fn valid_operator_label(value: &str) -> bool {
    (1..=80).contains(&value.trim().len()) && !value.chars().any(char::is_control)
}

fn coverage_rejection_reason_code(reason_code: &str) -> &str {
    if COVERAGE_REJECTION_REASONS.contains(&reason_code) {
        reason_code
    } else {
        "unlisted"
    }
}

fn is_fingerprint(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn epoch_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejection_fingerprint_is_stable_and_scoped_by_environment_and_kind() {
        let first = rejection_fingerprint("acct", ProviderEnvironment::Live, "evt", "personal");
        assert_eq!(first.len(), 64);
        assert_eq!(
            first,
            rejection_fingerprint("acct", ProviderEnvironment::Live, "evt", "personal")
        );
        assert_ne!(
            first,
            rejection_fingerprint("acct", ProviderEnvironment::Test, "evt", "personal")
        );
        assert_ne!(
            first,
            rejection_fingerprint("acct", ProviderEnvironment::Live, "evt", "sponsored")
        );
    }

    #[test]
    fn operator_labels_and_reason_codes_are_bounded() {
        assert!(valid_operator_label("operator-1"));
        assert!(!valid_operator_label("bad\nlabel"));
        assert!(!valid_operator_label(&"x".repeat(81)));
        assert!(COVERAGE_REJECTION_REASONS.contains(&"invoice_evidence_changed"));
        assert_eq!(coverage_rejection_reason_code("future_reason"), "unlisted");
        assert!(!COVERAGE_REJECTION_REASONS.contains(&"provider text"));
    }

    #[test]
    fn fingerprints_are_lowercase_sha256_hex() {
        assert!(is_fingerprint(&"a".repeat(64)));
        assert!(!is_fingerprint(&"A".repeat(64)));
        assert!(!is_fingerprint(&"z".repeat(64)));
    }

    #[test]
    fn metrics_use_only_component_and_state_labels() {
        let rendered = render_prometheus(&CloudOperationsSnapshot {
            states: vec![
                LifecycleStateMetric {
                    subsystem: "provider_refresh".into(),
                    state: "pending".into(),
                    count: 3,
                    oldest_age_seconds: 42,
                    attempts: 7,
                },
                LifecycleStateMetric {
                    subsystem: "billing_recovery".into(),
                    state: "manual_review".into(),
                    count: 1,
                    oldest_age_seconds: 120,
                    attempts: 20,
                },
            ],
            features: vec![
                feature("cloud_sales", false, true),
                feature("provider_refresh_worker", false, false),
            ],
            coverage_rejections_open: 1,
            coverage_rejections_unrecoverable: 2,
            coverage_rejections_oldest_age_seconds: 12,
            tombstones_total: 9,
            founding_places_awarded: 1,
            founding_reservations_open: 2,
            founding_refunds_required: 3,
            founding_price_notices_unresolved: 4,
            stripe_event_history_gaps: 5,
            coverage_publication_latency_seconds: 600,
            coverage_publication_events_24h: 9,
        });
        assert!(rendered.contains(
            "sotto_cloud_lifecycle_operations{component=\"provider_refresh\",state=\"pending\"} 3"
        ));
        assert!(rendered.contains(
            "sotto_cloud_lifecycle_retry_attempts{component=\"provider_refresh\",state=\"pending\"} 7"
        ));
        assert!(rendered.contains("sotto_cloud_coverage_rejections_open 1"));
        assert!(rendered.contains("sotto_cloud_coverage_rejections_unrecoverable 2"));
        assert!(rendered.contains("sotto_cloud_retention_tombstones_total 9"));
        assert!(rendered.contains("sotto_cloud_founding_places_awarded 1"));
        assert!(rendered.contains("sotto_cloud_stripe_event_history_gaps 5"));
        assert!(rendered.contains("sotto_cloud_coverage_publication_latency_seconds 600"));
        assert!(rendered.contains("sotto_cloud_coverage_publication_events_24h 9"));
        assert!(rendered.contains(
            "sotto_cloud_lifecycle_operations{component=\"billing_recovery\",state=\"manual_review\"} 1"
        ));
        assert!(rendered.contains(
            "sotto_cloud_runtime_feature_state{feature=\"cloud_sales\",state=\"disabled\"} 1"
        ));
        assert!(rendered.contains(
            "sotto_cloud_runtime_feature_state{feature=\"provider_refresh_worker\",state=\"unconfigured\"} 1"
        ));
        assert!(!rendered.contains("event_id"));
    }

    #[tokio::test]
    async fn claimed_repair_records_its_audit_before_resolving_the_incident() {
        if std::env::var("SOTTO_RUN_DB_TESTS").as_deref() != Ok("1") {
            eprintln!("skipping Cloud repair lease DB test: set SOTTO_RUN_DB_TESTS=1");
            return;
        }
        let database_url = std::env::var("DATABASE_URL")
            .expect("DATABASE_URL is required when SOTTO_RUN_DB_TESTS=1");
        let pool = crate::db::connect(&database_url).await.expect("connect");
        crate::db::migrate(&pool).await.expect("migrate");
        let event_id = format!("evt_cloud_repair_{}", Uuid::new_v4());
        let account_id = format!("acct_cloud_repair_{}", Uuid::new_v4());
        record_coverage_rejection(
            &pool,
            &account_id,
            ProviderEnvironment::Test,
            &event_id,
            "personal",
            "invoice_evidence_changed",
        )
        .await
        .expect("record rejection");
        let fingerprint = rejection_fingerprint(
            &account_id,
            ProviderEnvironment::Test,
            &event_id,
            "personal",
        );
        let lease = claim_rejection(&pool, &fingerprint, &account_id, ProviderEnvironment::Test)
            .await
            .expect("claim repair");

        resolve_coverage_rejections(&pool, &account_id, ProviderEnvironment::Test, &event_id)
            .await
            .expect("automatic resolution must not steal the repair lease");
        finish_repair(&pool, &lease, "operator-1", "applied", true)
            .await
            .expect("lease owner records the outcome");

        let (state, actions): (String, i64) = sqlx::query_as(
            "SELECT incident.state, count(action.action_id)::bigint \
             FROM cloud_coverage_rejection_incidents incident \
             LEFT JOIN cloud_coverage_rejection_actions action USING (fingerprint) \
             WHERE incident.fingerprint = $1 GROUP BY incident.state",
        )
        .bind(&fingerprint)
        .fetch_one(&pool)
        .await
        .expect("read repaired incident");
        assert_eq!(state, "resolved");
        assert_eq!(actions, 1);

        sqlx::query("DELETE FROM cloud_coverage_rejection_actions WHERE fingerprint = $1")
            .bind(&fingerprint)
            .execute(&pool)
            .await
            .expect("clean repair action");
        sqlx::query("DELETE FROM cloud_coverage_rejection_incidents WHERE fingerprint = $1")
            .bind(&fingerprint)
            .execute(&pool)
            .await
            .expect("clean repair incident");
    }

    #[tokio::test]
    async fn missing_stripe_event_closes_repair_as_unrecoverable_and_audits_it() {
        if std::env::var("SOTTO_RUN_DB_TESTS").as_deref() != Ok("1") {
            eprintln!("skipping Cloud repair lease DB test: set SOTTO_RUN_DB_TESTS=1");
            return;
        }
        let database_url = std::env::var("DATABASE_URL")
            .expect("DATABASE_URL is required when SOTTO_RUN_DB_TESTS=1");
        let pool = crate::db::connect(&database_url).await.expect("connect");
        crate::db::migrate(&pool).await.expect("migrate");
        let event_id = format!("evt_cloud_expired_repair_{}", Uuid::new_v4());
        let account_id = format!("acct_cloud_expired_repair_{}", Uuid::new_v4());
        record_coverage_rejection(
            &pool,
            &account_id,
            ProviderEnvironment::Test,
            &event_id,
            "sponsored",
            "sponsored_invoice_evidence_rejected",
        )
        .await
        .expect("record rejection");
        let fingerprint = rejection_fingerprint(
            &account_id,
            ProviderEnvironment::Test,
            &event_id,
            "sponsored",
        );
        let lease = claim_rejection(&pool, &fingerprint, &account_id, ProviderEnvironment::Test)
            .await
            .expect("claim repair");
        finish_repair(&pool, &lease, "operator-1", "event_unavailable", false)
            .await
            .expect("record event outside provider history window");

        let (state, outcome): (String, String) = sqlx::query_as(
            "SELECT incident.state, action.outcome \
             FROM cloud_coverage_rejection_incidents incident \
             JOIN cloud_coverage_rejection_actions action USING (fingerprint) \
             WHERE incident.fingerprint = $1",
        )
        .bind(&fingerprint)
        .fetch_one(&pool)
        .await
        .expect("read unavailable event result");
        assert_eq!(state, "unrecoverable");
        assert_eq!(outcome, "event_unavailable");
        assert!(
            claim_rejection(&pool, &fingerprint, &account_id, ProviderEnvironment::Test)
                .await
                .is_err()
        );

        sqlx::query("DELETE FROM cloud_coverage_rejection_actions WHERE fingerprint = $1")
            .bind(&fingerprint)
            .execute(&pool)
            .await
            .expect("clean repair action");
        sqlx::query("DELETE FROM cloud_coverage_rejection_incidents WHERE fingerprint = $1")
            .bind(&fingerprint)
            .execute(&pool)
            .await
            .expect("clean repair incident");
    }
}
