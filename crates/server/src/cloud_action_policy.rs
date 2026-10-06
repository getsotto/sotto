//! Shadow policy for human hosted data actions.
//!
//! This is the first enforcement seam for person-level Cloud eligibility. It classifies the
//! authenticated human routes and records which requests would be denied by the account policy,
//! while deliberately leaving the existing response unchanged. A later rollout can turn the
//! decision into a rejection after transition and export contracts are approved.

use axum::extract::{Request, State};
use axum::http::Method;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::auth::session;
use crate::auth::AuthUser;
use crate::config::DeploymentMode;
use crate::person_eligibility::{self, EligibilityState};
use crate::state::AppState;

/// The human action families that share one account-level eligibility decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionClass {
    AccountBootstrap,
    AccountRead,
    AccountReset,
    OrganisationRead,
    OrganisationWrite,
    AuditRead,
    ProjectRead,
    ProjectWrite,
    EnvironmentRead,
    EnvironmentWrite,
    GrantRead,
    GrantWrite,
    SecretRead,
    SecretWrite,
    SecurityControl,
}

/// A route decision is intentionally separate from HTTP enforcement. The shadow seam can report
/// an ineligible hosted action without changing the ACL, grant, lifecycle, or export response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShadowDecision {
    Allowed,
    WouldDeny,
}

/// Convert a shadow observation into an enforcement message. `None` means the eligibility read was
/// unavailable, which is fail-closed only when the explicit rollout switch is enabled.
pub fn enforcement_message(
    enforcement_enabled: bool,
    action: ActionClass,
    decision: Option<ShadowDecision>,
) -> Option<&'static str> {
    if !enforcement_enabled || is_always_available(action) {
        return None;
    }
    match decision {
        Some(ShadowDecision::Allowed) => None,
        Some(ShadowDecision::WouldDeny) => Some("hosted eligibility is required for this action"),
        None => Some("hosted eligibility is unavailable"),
    }
}

fn is_always_available(action: ActionClass) -> bool {
    matches!(
        action,
        ActionClass::AccountBootstrap
            | ActionClass::AccountRead
            | ActionClass::AccountReset
            | ActionClass::SecurityControl
    )
}

fn is_export_read(action: ActionClass) -> bool {
    matches!(
        action,
        ActionClass::OrganisationRead
            | ActionClass::AuditRead
            | ActionClass::ProjectRead
            | ActionClass::EnvironmentRead
            | ActionClass::GrantRead
            | ActionClass::SecretRead
    )
}

/// Classify the human routes covered by this slice. Provider billing, machine access, free share
/// links, and operational endpoints are deliberately absent; they have separate lifecycle and
/// rollout contracts.
pub fn classify(method: &Method, path: &str) -> Option<ActionClass> {
    if path == "/account" {
        return match *method {
            Method::GET => Some(ActionClass::AccountRead),
            Method::PUT => Some(ActionClass::AccountBootstrap),
            _ => None,
        };
    }
    if path == "/account/reset" {
        return (method == Method::POST).then_some(ActionClass::AccountReset);
    }
    if path == "/orgs" {
        return match *method {
            Method::GET => Some(ActionClass::OrganisationRead),
            Method::POST => Some(ActionClass::OrganisationWrite),
            _ => None,
        };
    }
    if path == "/projects" {
        return match *method {
            Method::GET => Some(ActionClass::ProjectRead),
            Method::POST => Some(ActionClass::ProjectWrite),
            _ => None,
        };
    }
    if path.starts_with("/orgs/") {
        let route = path
            .strip_prefix("/orgs/")
            .and_then(|remainder| remainder.split('/').nth(1));
        // Billing, eligibility discovery, and deletion/exit controls have their own contracts and
        // must remain reachable while a person is free, expired, or exporting.
        if matches!(route, Some("billing" | "entitlements" | "deletion")) {
            return None;
        }
        if route == Some("audit") {
            return (method == Method::GET).then_some(ActionClass::AuditRead);
        }
        return match *method {
            Method::GET => Some(ActionClass::OrganisationRead),
            Method::POST | Method::DELETE => Some(ActionClass::OrganisationWrite),
            _ => None,
        };
    }
    if path.starts_with("/projects/") && path.ends_with("/environments") {
        return match *method {
            Method::GET => Some(ActionClass::EnvironmentRead),
            Method::POST => Some(ActionClass::EnvironmentWrite),
            _ => None,
        };
    }
    if path.starts_with("/environments/") {
        // Machine-token lifecycle has its own accountable-beneficiary contract and is intentionally
        // outside this human-action slice.
        let mut segments = path
            .strip_prefix("/environments/")
            .into_iter()
            .flat_map(|remainder| remainder.split('/'));
        let _environment_id = segments.next();
        let is_machine_token_route = matches!(
            (segments.next(), segments.next(), segments.next()),
            (Some("tokens"), None, None) | (Some("tokens"), Some(_), None)
        );
        if is_machine_token_route {
            return None;
        }
        if path.ends_with("/grant") || path.ends_with("/grants") {
            return match *method {
                Method::GET => Some(ActionClass::GrantRead),
                Method::POST => Some(ActionClass::GrantWrite),
                _ => None,
            };
        }
        if path.ends_with("/rotate") {
            return (method == Method::POST).then_some(ActionClass::SecurityControl);
        }
        if path.ends_with("/history") {
            return (method == Method::GET).then_some(ActionClass::SecretRead);
        }
        return match *method {
            Method::GET => Some(ActionClass::SecretRead),
            Method::POST | Method::PUT | Method::PATCH | Method::DELETE => {
                Some(ActionClass::SecretWrite)
            }
            _ => None,
        };
    }
    None
}

/// Decide whether an action would need hosted eligibility. Security controls and account bootstrap
/// remain available so a person can initialise, recover, export, or revoke access safely.
pub fn decide(action: ActionClass, state: EligibilityState) -> ShadowDecision {
    let always_available = matches!(
        action,
        ActionClass::AccountBootstrap
            | ActionClass::AccountRead
            | ActionClass::AccountReset
            | ActionClass::SecurityControl
    );
    if always_available
        || (state == EligibilityState::ExportOnly && is_export_read(action))
        || matches!(
            state,
            EligibilityState::Paid | EligibilityState::RenewalRecovery
        )
    {
        ShadowDecision::Allowed
    } else {
        ShadowDecision::WouldDeny
    }
}

/// Observe a classified request without changing its response. Invalid or unavailable policy
/// reads are retained as diagnostics only; the existing resource authorisation remains the source
/// of truth until the later activation gate.
pub async fn shadow(State(state): State<AppState>, mut request: Request, next: Next) -> Response {
    if state.deployment_mode == DeploymentMode::Cloud {
        if let Some(action) = classify(request.method(), request.uri().path()) {
            if let Some(token) = session::token_from_headers(request.headers()) {
                if let Ok(Some(user_id)) = session::resolve(&state.pool, &token).await {
                    request.extensions_mut().insert(AuthUser {
                        user_id: user_id.clone(),
                    });
                    match person_eligibility::load_view(&state, &user_id).await {
                        Ok(view) if decide(action, view.state) == ShadowDecision::WouldDeny => {
                            eprintln!(
                                "cloud action shadow denial: action={action:?} state={:?}",
                                view.state
                            );
                            if let Some(message) = enforcement_message(
                                state.cloud_action_enforcement_enabled,
                                action,
                                Some(ShadowDecision::WouldDeny),
                            ) {
                                return crate::error::Error::CloudEligibility(message.into())
                                    .into_response();
                            }
                        }
                        Err(error) => {
                            eprintln!("cloud action eligibility unavailable: {error}");
                            if let Some(message) = enforcement_message(
                                state.cloud_action_enforcement_enabled,
                                action,
                                None,
                            ) {
                                return crate::error::Error::CloudEligibility(message.into())
                                    .into_response();
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_account_and_resource_boundaries() {
        assert_eq!(
            classify(&Method::PUT, "/account"),
            Some(ActionClass::AccountBootstrap)
        );
        assert_eq!(
            classify(&Method::GET, "/orgs/acme/audit"),
            Some(ActionClass::AuditRead)
        );
        assert_eq!(
            classify(&Method::POST, "/projects/p1/environments"),
            Some(ActionClass::EnvironmentWrite)
        );
        assert_eq!(
            classify(&Method::GET, "/environments/e1/history"),
            Some(ActionClass::SecretRead)
        );
        assert_eq!(
            classify(&Method::POST, "/environments/e1/grants"),
            Some(ActionClass::GrantWrite)
        );
        assert_eq!(classify(&Method::POST, "/orgs/acme/billing/checkout"), None);
        assert_eq!(classify(&Method::GET, "/orgs/acme/entitlements"), None);
        assert_eq!(classify(&Method::POST, "/orgs/acme/deletion/cancel"), None);
        assert_eq!(
            classify(&Method::GET, "/orgs/billing/members"),
            Some(ActionClass::OrganisationRead)
        );
        assert_eq!(
            classify(&Method::POST, "/orgs/deletion/members"),
            Some(ActionClass::OrganisationWrite)
        );
        assert_eq!(
            classify(&Method::GET, "/orgs/entitlements/members"),
            Some(ActionClass::OrganisationRead)
        );
        assert_eq!(classify(&Method::GET, "/environments/e1/tokens"), None);
        assert_eq!(
            classify(&Method::DELETE, "/environments/e1/tokens/t1"),
            None
        );
        assert_eq!(
            classify(&Method::GET, "/environments/tokens/secrets"),
            Some(ActionClass::SecretRead)
        );
        assert_eq!(classify(&Method::GET, "/shares/token"), None);
    }

    #[test]
    fn shadow_decision_preserves_bootstrap_and_recovery_controls() {
        assert_eq!(
            decide(ActionClass::AccountBootstrap, EligibilityState::Free),
            ShadowDecision::Allowed
        );
        assert_eq!(
            decide(ActionClass::SecurityControl, EligibilityState::Expired),
            ShadowDecision::Allowed
        );
        assert_eq!(
            decide(ActionClass::ProjectRead, EligibilityState::Free),
            ShadowDecision::WouldDeny
        );
        assert_eq!(
            decide(ActionClass::ProjectRead, EligibilityState::ExportOnly),
            ShadowDecision::Allowed
        );
        assert_eq!(
            decide(ActionClass::AuditRead, EligibilityState::ExportOnly),
            ShadowDecision::Allowed
        );
        assert_eq!(
            decide(ActionClass::ProjectWrite, EligibilityState::ExportOnly),
            ShadowDecision::WouldDeny
        );
        assert_eq!(
            decide(ActionClass::ProjectWrite, EligibilityState::RenewalRecovery),
            ShadowDecision::Allowed
        );
    }

    #[test]
    fn enforcement_is_opt_in_and_unavailable_is_fail_closed_when_enabled() {
        assert_eq!(
            enforcement_message(
                false,
                ActionClass::ProjectRead,
                Some(ShadowDecision::WouldDeny)
            ),
            None
        );
        assert_eq!(
            enforcement_message(
                true,
                ActionClass::ProjectRead,
                Some(ShadowDecision::Allowed)
            ),
            None
        );
        assert_eq!(
            enforcement_message(
                true,
                ActionClass::ProjectRead,
                Some(ShadowDecision::WouldDeny)
            ),
            Some("hosted eligibility is required for this action")
        );
        assert_eq!(
            enforcement_message(true, ActionClass::ProjectRead, None),
            Some("hosted eligibility is unavailable")
        );
        assert_eq!(
            enforcement_message(true, ActionClass::AccountRead, None),
            None
        );
    }
}
