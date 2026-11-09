//! Public server discovery is independent of Postgres, OAuth and Stripe.

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;

use sotto_server::config::{DeploymentMode, DEFAULT_ORGANISATION_DELETION_RETENTION_DAYS};
use sotto_server::state::AppState;

fn app(
    mode: DeploymentMode,
    billing_transfers_enabled: bool,
    billing_transfer_execution_enabled: bool,
    billing_transfer_setup_enabled: bool,
) -> Router {
    let pool = PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(50))
        .connect_lazy("postgres://sotto:sotto@127.0.0.1:1/sotto")
        .expect("a well-formed URL");
    sotto_server::app(AppState {
        pool,
        deployment_mode: mode,
        oauth: None,
        oauth_config: None,
        billing: None,
        telemetry_ingest: false,
        contact_verification: None,
        cloud_notice_dispatch_enabled: false,
        resend_webhook_secret: None,
        cloud_action_enforcement_enabled: false,
        cloud_coverage_authority_enforcement_enabled: false,
        cloud_coverage_authority_shadow_enabled: false,
        machine_eligibility_enforcement_enabled: false,
        billing_transfers_enabled,
        billing_transfer_execution_enabled,
        billing_transfer_setup_enabled,
        cloud_retention_dry_run_enabled: false,
        cloud_shared_retention_dry_run_enabled: false,
        cloud_retention_operator_token: None,
        cloud_retention_purge_enabled: false,
        cloud_retention_destructive_token: None,
        cloud_retention_evidence_hmac_key: None,
        cloud_retention_build_sha: None,
        organisation_deletion_enabled: false,
        organisation_deletion_retention_days: DEFAULT_ORGANISATION_DELETION_RETENTION_DAYS,
        organisation_deletion_metrics_token: None,
        cloud_operations_metrics_token: None,
        cloud_coverage_repair_token: None,
        provider_refresh_ingest_enabled: false,
        provider_refresh_worker_enabled: false,
        provider_refresh_reconciliation_enabled: false,
        provider_authority_revalidation_enabled: false,
        cloud_sales_enabled: false,
        founding_schedule_worker_enabled: false,
        founding_price_notices_enabled: false,
        billing_recovery_worker_enabled: false,
        sponsored_billing_recovery_worker_enabled: false,
        organisation_deletion_operator_token: None,
        billing_transfer_operator_token: None,
    })
}

#[tokio::test]
async fn reports_mode_without_database_or_credentials() {
    for (mode, expected) in [
        (DeploymentMode::SelfHosted, "self_hosted"),
        (DeploymentMode::Cloud, "cloud"),
    ] {
        let response = app(mode, false, false, false)
            .oneshot(
                Request::builder()
                    .uri("/server/info")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["cache-control"], "no-store");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["deployment_mode"], expected);
        assert_eq!(value["entitlement_model"], "organisation_tiers_v1");
        assert_eq!(value["payer_transfers_available"], false);
        assert_eq!(value["payer_transfer_setup_available"], false);
        assert_eq!(value.as_object().unwrap().len(), 4);
    }
}

#[tokio::test]
async fn payer_transfer_routes_are_not_registered_by_default() {
    let response = app(DeploymentMode::Cloud, false, false, false)
        .oneshot(
            Request::builder()
                .uri("/billing/transfers/quote")
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn enabled_payer_transfer_routes_still_require_a_session() {
    let response = app(DeploymentMode::Cloud, true, false, false)
        .oneshot(
            Request::builder()
                .uri("/billing/transfers")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn payer_transfer_capabilities_require_their_complete_control_set() {
    for (flags, expected_transfers, expected_setup) in [
        ((true, false, false), false, false),
        ((true, true, false), true, false),
        ((true, true, true), true, true),
        ((false, true, true), false, false),
    ] {
        let response = app(DeploymentMode::Cloud, flags.0, flags.1, flags.2)
            .oneshot(
                Request::builder()
                    .uri("/server/info")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["payer_transfers_available"], expected_transfers);
        assert_eq!(value["payer_transfer_setup_available"], expected_setup);
    }
}
