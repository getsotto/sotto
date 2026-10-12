//! Person-level Cloud eligibility and action discovery.
//!
//! This module is deliberately a read-only policy boundary. It combines the deployment mode,
//! durable coverage projection, hosted billing state, and account bootstrap state into one stable
//! response for clients. It never grants a secret or replaces the existing authorisation checks.

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use crate::auth::AuthUser;
use crate::cloud_coverage::{evaluate, CoverageDecision, CoverageState};
use crate::cloud_coverage_store::{self, StoreError};
use crate::config::DeploymentMode;
use crate::error::{Error, Result};
use crate::personal_billing::{self, PersonalBillingState};
use crate::state::AppState;

/// Stable public identifier for this action policy. Keep this in sync with the endpoint contract.
pub const ELIGIBILITY_MODEL: &str = "person_eligibility_v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EligibilityState {
    Free,
    PendingInitialPayment,
    Paid,
    RenewalRecovery,
    ExportOnly,
    Expired,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverageInput {
    Missing,
    Decision(CoverageDecision),
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EligibilityInput {
    pub deployment_mode: DeploymentMode,
    pub billing_available: bool,
    pub account_initialized: bool,
    pub personal_billing_state: Option<PersonalBillingState>,
    pub coverage: CoverageInput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct EligibilityActions {
    pub setup: bool,
    pub billing: bool,
    pub export: bool,
    pub revoke: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EligibilityView {
    pub model: &'static str,
    pub deployment_mode: &'static str,
    pub state: EligibilityState,
    pub account_initialized: bool,
    pub billing_available: bool,
    pub paid_through_epoch: Option<i64>,
    pub recovery_until_epoch: Option<i64>,
    pub export_until_epoch: Option<i64>,
    pub actions: EligibilityActions,
    pub next_actions: Vec<&'static str>,
    pub payer: Option<&'static str>,
}

/// Evaluate the account policy without I/O or a clock. The caller supplies a decision evaluated at
/// its chosen instant, making all boundary tests deterministic.
pub fn evaluate_input(input: &EligibilityInput) -> EligibilityView {
    if input.deployment_mode == DeploymentMode::SelfHosted {
        return view(input, EligibilityState::Free, None, None, None);
    }

    let state = match input.personal_billing_state {
        Some(PersonalBillingState::Pending) => match input.coverage {
            CoverageInput::Decision(_) => EligibilityState::PendingInitialPayment,
            CoverageInput::Missing | CoverageInput::Unavailable => EligibilityState::Unavailable,
        },
        Some(PersonalBillingState::Active | PersonalBillingState::PastDue) => {
            coverage_state(input.coverage)
        }
        Some(PersonalBillingState::Unpaid | PersonalBillingState::Canceled) => {
            coverage_state(input.coverage)
        }
        Some(PersonalBillingState::RefundRequired) => EligibilityState::Unavailable,
        None => match input.coverage {
            CoverageInput::Missing => EligibilityState::Free,
            CoverageInput::Decision(decision) => map_coverage_state(decision.state),
            CoverageInput::Unavailable => EligibilityState::Unavailable,
        },
    };
    let decision = match input.coverage {
        CoverageInput::Decision(decision) => Some(decision),
        CoverageInput::Missing | CoverageInput::Unavailable => None,
    };
    view(
        input,
        state,
        decision.and_then(|d| d.active_until),
        decision.and_then(|d| d.recovery_until),
        decision.and_then(|d| d.export_until),
    )
}

fn coverage_state(coverage: CoverageInput) -> EligibilityState {
    match coverage {
        CoverageInput::Decision(decision) if decision.state == CoverageState::Free => {
            EligibilityState::Unavailable
        }
        CoverageInput::Decision(decision) => map_coverage_state(decision.state),
        CoverageInput::Missing | CoverageInput::Unavailable => EligibilityState::Unavailable,
    }
}

fn map_coverage_state(state: CoverageState) -> EligibilityState {
    match state {
        CoverageState::Free => EligibilityState::Free,
        CoverageState::Paid => EligibilityState::Paid,
        CoverageState::RenewalRecovery => EligibilityState::RenewalRecovery,
        CoverageState::ExportOnly => EligibilityState::ExportOnly,
        CoverageState::Expired => EligibilityState::Expired,
    }
}

fn view(
    input: &EligibilityInput,
    state: EligibilityState,
    paid_through_epoch: Option<i64>,
    recovery_until_epoch: Option<i64>,
    export_until_epoch: Option<i64>,
) -> EligibilityView {
    let setup = !input.account_initialized;
    let export = input.account_initialized
        && matches!(
            state,
            EligibilityState::ExportOnly | EligibilityState::Expired
        );
    let billing = input.deployment_mode == DeploymentMode::Cloud
        && input.billing_available
        && matches!(state, EligibilityState::Free | EligibilityState::Expired);
    let revoke = matches!(
        state,
        EligibilityState::Paid | EligibilityState::RenewalRecovery
    );
    let mut next_actions = Vec::new();
    if setup {
        next_actions.push("setup");
    }
    if billing {
        next_actions.push("billing");
    }
    if export {
        next_actions.push("export");
    }
    if revoke {
        next_actions.push("revoke");
    }
    EligibilityView {
        model: ELIGIBILITY_MODEL,
        deployment_mode: input.deployment_mode.as_str(),
        state,
        account_initialized: input.account_initialized,
        billing_available: input.billing_available
            && input.deployment_mode == DeploymentMode::Cloud,
        paid_through_epoch,
        recovery_until_epoch,
        export_until_epoch,
        actions: EligibilityActions {
            setup,
            billing,
            export,
            revoke,
        },
        next_actions,
        payer: input.personal_billing_state.map(|_| "personal"),
    }
}

pub fn router() -> Router<AppState> {
    Router::new().route("/account/eligibility", get(get_eligibility))
}

/// Load the account-level eligibility view used by both the public discovery endpoint and the
/// dormant human-action policy. Keeping the read in one function prevents a route from applying a
/// different interpretation of billing or coverage than `/account/eligibility`.
pub(crate) async fn load_view(state: &AppState, user_id: &str) -> Result<EligibilityView> {
    let account_initialized: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM users WHERE id = $1 AND public_key IS NOT NULL)",
    )
    .bind(user_id)
    .fetch_one(&state.pool)
    .await?;

    if state.deployment_mode == DeploymentMode::SelfHosted {
        return Ok(evaluate_input(&EligibilityInput {
            deployment_mode: state.deployment_mode,
            billing_available: false,
            account_initialized,
            personal_billing_state: None,
            coverage: CoverageInput::Missing,
        }));
    }

    let mut tx = state.pool.begin().await?;
    let personal = personal_billing::load_account(&mut tx, user_id)
        .await
        .map_err(|error| match error {
            personal_billing::PersonalBillingError::Database(error) => Error::Db(error),
            other => Error::Internal(other.to_string()),
        })?;
    tx.commit().await?;

    let coverage = match cloud_coverage_store::load(&state.pool, user_id).await {
        Ok(loaded) => {
            CoverageInput::Decision(evaluate(&loaded.coverage, epoch_now()).map_err(|error| {
                Error::Internal(format!("stored coverage failed validation: {error}"))
            })?)
        }
        Err(StoreError::ProjectionMissing) => CoverageInput::Missing,
        Err(StoreError::ProjectionUnavailable(_)) => CoverageInput::Unavailable,
        Err(StoreError::Database(error)) => return Err(Error::Db(error)),
        Err(error) => return Err(Error::Internal(error.to_string())),
    };

    Ok(evaluate_input(&EligibilityInput {
        deployment_mode: state.deployment_mode,
        billing_available: state.billing.as_ref().is_some_and(|billing| {
            billing.cloud_sales_enabled() && billing.price_catalogue().is_some()
        }),
        account_initialized,
        personal_billing_state: personal.as_ref().map(|account| account.state),
        coverage,
    }))
}

async fn get_eligibility(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<EligibilityView>> {
    Ok(Json(load_view(&state, &user.user_id).await?))
}

fn epoch_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud_coverage::CoverageDecision;

    fn input(state: Option<PersonalBillingState>, coverage: CoverageInput) -> EligibilityInput {
        EligibilityInput {
            deployment_mode: DeploymentMode::Cloud,
            billing_available: true,
            account_initialized: true,
            personal_billing_state: state,
            coverage,
        }
    }

    #[test]
    fn self_hosted_is_free_without_billing_or_coverage() {
        let mut value = input(None, CoverageInput::Missing);
        value.deployment_mode = DeploymentMode::SelfHosted;
        value.billing_available = false;
        let result = evaluate_input(&value);
        assert_eq!(result.state, EligibilityState::Free);
        assert!(!result.actions.billing);
    }

    #[test]
    fn pending_payment_is_distinct_from_free_when_evidence_is_available() {
        let result = evaluate_input(&input(
            Some(PersonalBillingState::Pending),
            CoverageInput::Decision(CoverageDecision {
                state: CoverageState::Free,
                active_until: None,
                recovery_until: None,
                export_until: None,
            }),
        ));
        assert_eq!(result.state, EligibilityState::PendingInitialPayment);
        assert!(!result.actions.billing);
    }

    #[test]
    fn pending_payment_with_unavailable_evidence_fails_closed() {
        let result = evaluate_input(&input(
            Some(PersonalBillingState::Pending),
            CoverageInput::Unavailable,
        ));
        assert_eq!(result.state, EligibilityState::Unavailable);
        assert!(!result.actions.billing);
    }

    #[test]
    fn recovery_exposes_recovery_and_revoke_without_upgrade() {
        let result = evaluate_input(&input(
            Some(PersonalBillingState::Active),
            CoverageInput::Decision(CoverageDecision {
                state: CoverageState::RenewalRecovery,
                active_until: Some(100),
                recovery_until: Some(200),
                export_until: None,
            }),
        ));
        assert_eq!(result.state, EligibilityState::RenewalRecovery);
        assert_eq!(result.recovery_until_epoch, Some(200));
        assert!(result.actions.revoke);
        assert!(!result.actions.billing);
    }

    #[test]
    fn export_and_expired_require_a_bootstrapped_account() {
        let mut value = input(
            None,
            CoverageInput::Decision(CoverageDecision {
                state: CoverageState::ExportOnly,
                active_until: None,
                recovery_until: None,
                export_until: Some(300),
            }),
        );
        value.account_initialized = false;
        let result = evaluate_input(&value);
        assert_eq!(result.state, EligibilityState::ExportOnly);
        assert!(!result.actions.export);
        assert!(result.actions.setup);

        value.account_initialized = true;
        let result = evaluate_input(&value);
        assert!(result.actions.export);
        assert_eq!(result.export_until_epoch, Some(300));
    }

    #[test]
    fn active_billing_with_a_free_projection_is_unavailable() {
        let result = evaluate_input(&input(
            Some(PersonalBillingState::Active),
            CoverageInput::Decision(CoverageDecision {
                state: CoverageState::Free,
                active_until: None,
                recovery_until: None,
                export_until: None,
            }),
        ));
        assert_eq!(result.state, EligibilityState::Unavailable);
        assert!(!result.actions.billing);
    }

    #[test]
    fn unavailable_does_not_become_an_upgrade_prompt() {
        let result = evaluate_input(&input(
            Some(PersonalBillingState::Active),
            CoverageInput::Unavailable,
        ));
        assert_eq!(result.state, EligibilityState::Unavailable);
        assert!(!result.actions.billing);
    }
}
