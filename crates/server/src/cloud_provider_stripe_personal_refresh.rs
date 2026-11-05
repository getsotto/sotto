//! Personal Stripe executor for the durable provider refresh queue.
//!
//! It commits a collection ticket before constructing the source-bound Stripe transport, performs
//! every Stripe read without a database transaction, and completes the exact ticket afterwards.

use async_trait::async_trait;
use sqlx::{PgPool, Row};
use thiserror::Error;
use uuid::Uuid;

use crate::cloud_provider::{
    complete_verified_event, prepare_verified_event, PayerKind, ProviderAdapterError,
};
use crate::cloud_provider_refresh_inputs::RefreshJobInputs;
use crate::cloud_provider_refresh_worker::{self, RefreshExecutionError, RefreshJobExecutor};
use crate::cloud_provider_stripe::{
    StripeAllocationBinding, StripeCoverageConfig, STRIPE_NAMESPACE,
};
use crate::cloud_provider_stripe_adapter::{
    collect_stripe_history, prepared_source, StripeHistoryClient, StripePreparedCollection,
};
use crate::cloud_provider_stripe_http::{StripeReadClient, StripeReadError};

#[derive(Debug, Error)]
enum PersonalRefreshError {
    #[error("invalid personal refresh inputs")]
    InvalidInput,
    #[error("personal Stripe read failed: {0}")]
    Stripe(#[from] StripeReadError),
    #[error("personal collection adapter rejected the ticket")]
    Adapter,
    #[error("personal provider refresh failed: {0}")]
    Provider(#[from] ProviderAdapterError),
    #[error("personal provider refresh transaction failed: {0}")]
    Transaction(#[from] sqlx::Error),
}

#[derive(Clone)]
pub struct PersonalStripeRefreshExecutor {
    pool: PgPool,
    stripe: StripeReadClient,
    coverage: StripeCoverageConfig,
}

impl PersonalStripeRefreshExecutor {
    pub fn new(pool: PgPool, stripe: StripeReadClient, coverage: StripeCoverageConfig) -> Self {
        Self {
            pool,
            stripe,
            coverage,
        }
    }

    async fn execute_inner(&self, inputs: &RefreshJobInputs) -> Result<(), PersonalRefreshError> {
        let allocation = &inputs.allocation;
        let context = &inputs.lease.context;
        if allocation.payer_kind != PayerKind::Personal
            || context.namespace != STRIPE_NAMESPACE
            || context.account_id != self.coverage.account_id
            || context.environment != self.coverage.environment
            || inputs.event.event_type != "invoice.paid"
        {
            return Err(PersonalRefreshError::InvalidInput);
        }

        let run_id = format!("personal-refresh:{}", Uuid::new_v4());
        let mut preparation_tx = self.pool.begin().await?;
        let preparation = match prepare_verified_event(
            &mut preparation_tx,
            context,
            &inputs.event,
            allocation,
            &run_id,
        )
        .await
        {
            Ok(preparation) => preparation,
            Err(error) => {
                preparation_tx.rollback().await?;
                return Err(PersonalRefreshError::Provider(error));
            }
        };
        preparation_tx.commit().await?;

        let mut sources = Vec::with_capacity(preparation.ticket.source_bindings.len());
        for binding in &preparation.ticket.source_bindings {
            let row = sqlx::query(
                "SELECT allocation.allocation_id, allocation.payer_id, payer.provider_customer_id, \
                        payer.payer_kind, allocation.provider_subscription_id, allocation.provider_item_id, \
                        allocation.external_allocation_reference, allocation.effective_from, \
                        allocation.effective_until, allocation.state, allocation.ownership_evidence_reference, \
                        source.registration_source_set_generation \
                 FROM cloud_provider_allocations AS allocation \
                 JOIN cloud_provider_payers AS payer ON payer.payer_id = allocation.payer_id \
                 JOIN cloud_coverage_sources AS source \
                   ON source.source_id = allocation.coverage_source_id \
                 WHERE allocation.coverage_source_id = $1 AND allocation.beneficiary_id = $2 \
                   AND allocation.provider_namespace = $3 AND allocation.provider_account_id = $4 \
                   AND allocation.provider_environment = $5",
            )
            .bind(&binding.source_id)
            .bind(&binding.beneficiary_id)
            .bind(&context.namespace)
            .bind(&context.account_id)
            .bind(context.environment.as_str())
            .fetch_optional(&self.pool)
            .await?
            .ok_or(PersonalRefreshError::InvalidInput)?;
            let payer_kind: String = row.try_get("payer_kind")?;
            if payer_kind != "personal" {
                return Err(PersonalRefreshError::InvalidInput);
            }
            let allocation = StripeAllocationBinding::new(
                row.try_get::<String, _>("external_allocation_reference")?,
                row.try_get::<String, _>("provider_customer_id")?,
                row.try_get::<String, _>("provider_subscription_id")?,
                row.try_get::<String, _>("provider_item_id")?,
                PayerKind::Personal,
            )
            .map_err(|_| PersonalRefreshError::InvalidInput)?;
            sources.push(prepared_source(
                binding.clone(),
                allocation,
                row.try_get::<i64, _>("registration_source_set_generation")?,
                row.try_get::<String, _>("ownership_evidence_reference")?,
            ));
        }
        let prepared =
            StripePreparedCollection::from_ticket(&self.coverage, &preparation.ticket, sources)
                .map_err(|_| PersonalRefreshError::Adapter)?;
        let mut history_client = StripeHistoryClient::new(self.stripe.clone(), prepared);
        let collection = collect_stripe_history(
            &mut history_client,
            context,
            &preparation.ticket.source_bindings,
        )
        .await
        .map_err(|_| PersonalRefreshError::Adapter)?;

        let mut completion_tx = self.pool.begin().await?;
        let result = complete_verified_event(
            &mut completion_tx,
            context,
            &inputs.event,
            &inputs.allocation,
            &preparation,
            &collection,
        )
        .await;
        match result {
            Ok(_) => completion_tx.commit().await.map_err(Into::into),
            Err(error) => {
                completion_tx.rollback().await?;
                Err(PersonalRefreshError::Provider(error))
            }
        }
    }
}

#[async_trait]
impl RefreshJobExecutor for PersonalStripeRefreshExecutor {
    async fn execute(&mut self, inputs: &RefreshJobInputs) -> Result<(), RefreshExecutionError> {
        self.execute_inner(inputs).await.map_err(|error| {
            let code = match error {
                PersonalRefreshError::InvalidInput | PersonalRefreshError::Adapter => {
                    "personal_refresh_invalid"
                }
                PersonalRefreshError::Stripe(_) => "personal_refresh_stripe",
                PersonalRefreshError::Provider(_) => "personal_refresh_provider",
                PersonalRefreshError::Transaction(_) => "personal_refresh_database",
            };
            RefreshExecutionError::new(code).expect("static refresh error code is nonempty")
        })
    }
}

pub fn spawn(pool: PgPool, stripe: StripeReadClient, coverage: StripeCoverageConfig) {
    tokio::spawn(async move {
        let worker_id = format!("personal-stripe-refresh:{}", Uuid::new_v4());
        let mut executor = PersonalStripeRefreshExecutor::new(pool.clone(), stripe, coverage);
        loop {
            match cloud_provider_refresh_worker::run_once(&pool, &worker_id, &mut executor).await {
                Ok(cloud_provider_refresh_worker::RefreshWorkerOutcome::Idle) => {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
                Ok(_) => {}
                Err(_) => {
                    eprintln!("warning: personal Stripe refresh worker iteration failed");
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
            }
        }
    });
}
