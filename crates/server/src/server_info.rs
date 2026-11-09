//! Public, dependency-free information about the server deployment.

use axum::extract::State;
use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use crate::entitlements::ENTITLEMENT_MODEL;
use crate::state::AppState;

#[derive(Debug, Serialize)]
struct ServerInfo {
    deployment_mode: &'static str,
    entitlement_model: &'static str,
    payer_transfers_available: bool,
    payer_transfer_setup_available: bool,
}

pub fn router() -> Router<AppState> {
    Router::new().route("/server/info", get(get_server_info))
}

async fn get_server_info(State(state): State<AppState>) -> impl IntoResponse {
    let body = ServerInfo {
        deployment_mode: state.deployment_mode.as_str(),
        entitlement_model: ENTITLEMENT_MODEL,
        payer_transfers_available: state.billing_transfers_enabled
            && state.billing_transfer_execution_enabled,
        payer_transfer_setup_available: state.billing_transfers_enabled
            && state.billing_transfer_execution_enabled
            && state.billing_transfer_setup_enabled,
    };
    ([(header::CACHE_CONTROL, "no-store")], Json(body))
}
