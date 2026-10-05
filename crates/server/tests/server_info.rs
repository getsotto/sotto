//! Public server discovery is independent of Postgres, OAuth and Stripe.

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;

use sotto_server::config::{DeploymentMode, DEFAULT_ORGANISATION_DELETION_RETENTION_DAYS};
use sotto_server::state::AppState;

fn app(mode: DeploymentMode) -> Router {
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
        cloud_action_enforcement_enabled: false,
        machine_eligibility_enforcement_enabled: false,
        organisation_deletion_enabled: false,
        organisation_deletion_retention_days: DEFAULT_ORGANISATION_DELETION_RETENTION_DAYS,
        organisation_deletion_metrics_token: None,
        organisation_deletion_operator_token: None,
    })
}

#[tokio::test]
async fn reports_mode_without_database_or_credentials() {
    for (mode, expected) in [
        (DeploymentMode::SelfHosted, "self_hosted"),
        (DeploymentMode::Cloud, "cloud"),
    ] {
        let response = app(mode)
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
        assert_eq!(value.as_object().unwrap().len(), 2);
    }
}
