//! Worker orchestration for durable provider refresh jobs.
//!
//! The executor is injected because provider clients are deployment-specific and may perform
//! network I/O. The worker claims a lease, runs the executor without a database connection held,
//! then records completion or bounded retry/poison state using the lease owner. This module does
//! not decide when a deployment enables the worker; the caller owns that rollout switch.

use async_trait::async_trait;
use sqlx::PgPool;
use thiserror::Error;

use crate::cloud_provider_refresh_inputs::{self, RefreshInputError, RefreshJobInputs};
use crate::cloud_provider_refresh_jobs::{self, RefreshJobError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshExecutionError {
    code: String,
}

impl RefreshExecutionError {
    pub fn new(code: impl Into<String>) -> Result<Self, RefreshExecutionErrorInput> {
        let code = code.into();
        if code.trim().is_empty() {
            return Err(RefreshExecutionErrorInput::EmptyCode);
        }
        Ok(Self { code })
    }

    fn code(&self) -> &str {
        &self.code
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum RefreshExecutionErrorInput {
    #[error("refresh execution error code is empty")]
    EmptyCode,
}

#[async_trait]
pub trait RefreshJobExecutor: Send {
    async fn execute(&mut self, inputs: &RefreshJobInputs) -> Result<(), RefreshExecutionError>;
}

#[derive(Debug, Error)]
pub enum RefreshWorkerError {
    #[error("refresh queue error: {0}")]
    Queue(#[from] RefreshJobError),
    #[error("refresh input error: {0}")]
    Input(#[from] RefreshInputError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshWorkerOutcome {
    Idle,
    Completed,
    Retried,
    Poisoned,
}

/// Claim and execute at most one due refresh job.
///
/// The executor runs after `claim_due` has committed, so a slow provider request never holds a
/// SQL transaction or connection. A failed execution becomes a retry or poison transition; a
/// stale worker cannot transition the row because the queue checks both owner and lease expiry.
pub async fn run_once<E: RefreshJobExecutor + ?Sized>(
    pool: &PgPool,
    worker_id: &str,
    executor: &mut E,
) -> Result<RefreshWorkerOutcome, RefreshWorkerError> {
    let Some(lease) = cloud_provider_refresh_jobs::claim_due(pool, worker_id).await? else {
        return Ok(RefreshWorkerOutcome::Idle);
    };
    let inputs = match cloud_provider_refresh_inputs::load(pool, &lease).await {
        Ok(inputs) => inputs,
        Err(RefreshInputError::Database(error)) => {
            return Err(RefreshWorkerError::Input(RefreshInputError::Database(
                error,
            )));
        }
        Err(_error) => {
            let retrying =
                cloud_provider_refresh_jobs::fail(pool, &lease, "refresh_input_invalid").await?;
            return Ok(if retrying {
                RefreshWorkerOutcome::Retried
            } else {
                RefreshWorkerOutcome::Poisoned
            });
        }
    };
    match executor.execute(&inputs).await {
        Ok(()) => {
            cloud_provider_refresh_jobs::complete(pool, &lease).await?;
            Ok(RefreshWorkerOutcome::Completed)
        }
        Err(error) => {
            let retrying = cloud_provider_refresh_jobs::fail(pool, &lease, error.code()).await?;
            Ok(if retrying {
                RefreshWorkerOutcome::Retried
            } else {
                RefreshWorkerOutcome::Poisoned
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{RefreshExecutionError, RefreshExecutionErrorInput};

    #[test]
    fn execution_errors_require_a_nonempty_code() {
        assert_eq!(
            RefreshExecutionError::new("  ").unwrap_err(),
            RefreshExecutionErrorInput::EmptyCode
        );
        assert_eq!(
            RefreshExecutionError::new("provider_timeout").unwrap().code,
            "provider_timeout"
        );
    }
}
