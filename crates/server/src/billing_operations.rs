//! Durable identity and recovery state for hosted billing operations.
//!
//! This module deliberately stops at the provider boundary. A caller must persist an operation
//! before making a Stripe request, then record a provider result using the same operation id. An
//! unknown result remains reconcilable and can never be retried under a fresh financial identity.

use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Row, Transaction};
use thiserror::Error;
use uuid::Uuid;

use crate::billing_catalogue::BillingOffer;
use crate::error::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BillingOperationState {
    Pending,
    Succeeded,
    Failed,
    Unknown,
}

impl BillingOperationState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }

    fn parse(value: &str) -> Result<Self, BillingOperationError> {
        match value {
            "pending" => Ok(Self::Pending),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "unknown" => Ok(Self::Unknown),
            _ => Err(BillingOperationError::CorruptState),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BillingOperationRequest {
    operation_id: String,
    idempotency_key: String,
    request_hash: String,
    actor_user_id: String,
    payer_id: String,
    beneficiary_id: String,
    offer: BillingOffer,
    quote_version: i64,
    quote_expires_at_epoch: i64,
    provider_idempotency_key: String,
}

impl BillingOperationRequest {
    /// Build a personal operation from the authenticated actor. Payer and beneficiary are derived
    /// from that identity; callers cannot submit arbitrary billing identities or provider keys.
    pub fn personal(
        actor_user_id: impl Into<String>,
        idempotency_key: impl Into<String>,
        offer: BillingOffer,
        quote_version: i64,
        quote_expires_at_epoch: i64,
    ) -> Self {
        let actor_user_id = actor_user_id.into();
        let idempotency_key = idempotency_key.into();
        Self::from_parts(
            actor_user_id.clone(),
            idempotency_key,
            actor_user_id.clone(),
            actor_user_id,
            offer,
            quote_version,
            quote_expires_at_epoch,
        )
    }

    /// Build an organisation operation after the caller has proved membership in the
    /// organisation. The actor remains part of the request identity so two administrators cannot
    /// accidentally share a provider operation key for the same client idempotency key.
    pub fn organization(
        actor_user_id: impl Into<String>,
        organization_id: impl Into<String>,
        idempotency_key: impl Into<String>,
        offer: BillingOffer,
        quote_version: i64,
        quote_expires_at_epoch: i64,
    ) -> Self {
        let organization_id = organization_id.into();
        Self::from_parts(
            actor_user_id.into(),
            idempotency_key.into(),
            organization_id.clone(),
            organization_id,
            offer,
            quote_version,
            quote_expires_at_epoch,
        )
    }

    fn from_parts(
        actor_user_id: String,
        idempotency_key: String,
        payer_id: String,
        beneficiary_id: String,
        offer: BillingOffer,
        quote_version: i64,
        quote_expires_at_epoch: i64,
    ) -> Self {
        let request_hash = request_hash(
            &actor_user_id,
            &payer_id,
            &beneficiary_id,
            offer,
            quote_version,
            quote_expires_at_epoch,
        );
        let operation_id = format!("billing:{}", Uuid::new_v4());
        let provider_idempotency_key = format!(
            "sotto-billing:{}",
            identity_hash(&[&actor_user_id, &payer_id, &beneficiary_id, &idempotency_key])
        );
        Self {
            operation_id,
            idempotency_key,
            request_hash,
            actor_user_id,
            payer_id,
            beneficiary_id,
            offer,
            quote_version,
            quote_expires_at_epoch,
            provider_idempotency_key,
        }
    }

    pub fn validate(&self, now_epoch: i64) -> Result<(), BillingOperationError> {
        self.validate_identity()?;
        self.ensure_unexpired(now_epoch)
    }

    fn validate_identity(&self) -> Result<(), BillingOperationError> {
        for (value, field) in [
            (&self.operation_id, "operation_id"),
            (&self.idempotency_key, "idempotency_key"),
            (&self.request_hash, "request_hash"),
            (&self.actor_user_id, "actor_user_id"),
            (&self.payer_id, "payer_id"),
            (&self.beneficiary_id, "beneficiary_id"),
            (&self.provider_idempotency_key, "provider_idempotency_key"),
        ] {
            if value.trim().is_empty() {
                return Err(BillingOperationError::InvalidField(field));
            }
        }
        if self.quote_version < 1 {
            return Err(BillingOperationError::InvalidField("quote_version"));
        }
        Ok(())
    }

    fn ensure_unexpired(&self, now_epoch: i64) -> Result<(), BillingOperationError> {
        if self.quote_expires_at_epoch <= now_epoch {
            return Err(BillingOperationError::QuoteExpired);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BillingOperation {
    pub operation_id: String,
    pub idempotency_key: String,
    pub request_hash: String,
    pub actor_user_id: String,
    pub payer_id: String,
    pub beneficiary_id: String,
    pub offer: String,
    pub quote_version: i64,
    pub quote_expires_at_epoch: i64,
    pub provider_idempotency_key: String,
    pub provider_operation_id: Option<String>,
    pub provider_checkout_url: Option<String>,
    pub state: BillingOperationState,
    pub result_code: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BeginOperation {
    Created(BillingOperation),
    AlreadyExists(BillingOperation),
}

#[derive(Debug, Error)]
pub enum BillingOperationError {
    #[error("billing operation has invalid {0}")]
    InvalidField(&'static str),
    #[error("billing quote has expired")]
    QuoteExpired,
    #[error("billing idempotency key conflicts with a different request")]
    IdempotencyConflict,
    #[error("billing operation is not pending")]
    NotPending,
    #[error("billing operation actor is not authorised")]
    Unauthorised,
    #[error("organisation billing is unavailable while its lifecycle is not active")]
    OrganisationNotActive,
    #[error("billing operation return URL is not authorised")]
    UnauthorisedReturnUrl,
    #[error("billing operation provider result conflicts with its recorded result")]
    ResultConflict,
    #[error("billing operation state is corrupt")]
    CorruptState,
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
}

impl From<BillingOperationError> for Error {
    fn from(error: BillingOperationError) -> Self {
        match error {
            BillingOperationError::InvalidField(field) => {
                Self::BadRequest(format!("invalid billing operation {field}"))
            }
            BillingOperationError::QuoteExpired => Self::Conflict("billing quote expired".into()),
            BillingOperationError::IdempotencyConflict => {
                Self::Conflict("billing idempotency key conflicts with a different request".into())
            }
            BillingOperationError::NotPending => {
                Self::Conflict("billing operation is not pending".into())
            }
            BillingOperationError::Unauthorised => {
                Self::Forbidden("billing operation is not authorised for this account".into())
            }
            BillingOperationError::OrganisationNotActive => {
                Self::Conflict("organisation billing is unavailable during deletion".into())
            }
            BillingOperationError::UnauthorisedReturnUrl => {
                Self::BadRequest("billing return URL is not authorised".into())
            }
            BillingOperationError::ResultConflict => Self::Conflict(
                "billing operation provider result conflicts with its recorded result".into(),
            ),
            BillingOperationError::CorruptState => {
                Self::Internal("billing operation state is corrupt".into())
            }
            BillingOperationError::Database(error) => Self::Db(error),
        }
    }
}

/// Persist an operation identity before provider I/O. The caller may pass a transaction that also
/// holds payer/beneficiary authorisation locks; no provider call belongs inside this transaction.
pub(crate) async fn begin_operation(
    tx: &mut Transaction<'_, Postgres>,
    request: &BillingOperationRequest,
) -> Result<BeginOperation, BillingOperationError> {
    request.validate_identity()?;
    if let Some(existing) =
        load_by_idempotency(tx, &request.actor_user_id, &request.idempotency_key).await?
    {
        return compare_existing(request, existing);
    }
    request.ensure_unexpired(current_epoch())?;
    let inserted = sqlx::query(
        "INSERT INTO billing_operations (operation_id, idempotency_key, request_hash, actor_user_id, \
         payer_id, beneficiary_id, offer, quote_version, quote_expires_at_epoch, \
         provider_idempotency_key, reconciliation_lease_token, reconciliation_lease_until) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11, now() + interval '5 minutes') \
         ON CONFLICT (actor_user_id, idempotency_key) DO NOTHING RETURNING operation_id",
    )
    .bind(&request.operation_id)
    .bind(&request.idempotency_key)
    .bind(&request.request_hash)
    .bind(&request.actor_user_id)
    .bind(&request.payer_id)
    .bind(&request.beneficiary_id)
    .bind(request.offer.as_str())
    .bind(request.quote_version)
    .bind(request.quote_expires_at_epoch)
    .bind(&request.provider_idempotency_key)
    .bind(format!("creator:{}", request.operation_id))
    .fetch_optional(&mut **tx)
    .await?;

    if inserted.is_some() {
        return Ok(BeginOperation::Created(
            load_operation(tx, &request.operation_id).await?,
        ));
    }
    let existing = load_by_idempotency(tx, &request.actor_user_id, &request.idempotency_key)
        .await?
        .ok_or(BillingOperationError::CorruptState)?;
    compare_existing(request, existing)
}

fn compare_existing(
    request: &BillingOperationRequest,
    existing: BillingOperation,
) -> Result<BeginOperation, BillingOperationError> {
    if existing.request_hash != request.request_hash
        || existing.offer != request.offer.as_str()
        || existing.quote_version != request.quote_version
        || existing.quote_expires_at_epoch != request.quote_expires_at_epoch
        || existing.payer_id != request.payer_id
        || existing.beneficiary_id != request.beneficiary_id
        || existing.provider_idempotency_key != request.provider_idempotency_key
    {
        return Err(BillingOperationError::IdempotencyConflict);
    }
    Ok(BeginOperation::AlreadyExists(existing))
}

/// Lock the authenticated personal payer/beneficiary before persisting the operation identity.
/// Organisation and sponsored operations will use their own role-checked lock ordering.
#[allow(clippy::too_many_arguments)]
pub async fn begin_personal_operation(
    tx: &mut Transaction<'_, Postgres>,
    actor_user_id: &str,
    idempotency_key: &str,
    offer: BillingOffer,
    quote_version: i64,
    quote_expires_at_epoch: i64,
    configured_return_url: &str,
    requested_return_url: &str,
) -> Result<BeginOperation, BillingOperationError> {
    validate_return_url(configured_return_url, requested_return_url)?;
    let exists: Option<String> =
        sqlx::query_scalar("SELECT id FROM users WHERE id = $1 FOR UPDATE")
            .bind(actor_user_id)
            .fetch_optional(&mut **tx)
            .await?;
    if exists.is_none() {
        return Err(BillingOperationError::Unauthorised);
    }
    let request = BillingOperationRequest::personal(
        actor_user_id,
        idempotency_key,
        offer,
        quote_version,
        quote_expires_at_epoch,
    );
    begin_operation(tx, &request).await
}

/// Lock the organisation before its membership, then persist a billing operation for an owner or
/// administrator. This lock order matches other organisation writes and keeps role changes from
/// racing a checkout identity.
#[allow(clippy::too_many_arguments)]
pub async fn begin_organization_operation(
    tx: &mut Transaction<'_, Postgres>,
    actor_user_id: &str,
    organization_id: &str,
    idempotency_key: &str,
    offer: BillingOffer,
    quote_version: i64,
    quote_expires_at_epoch: i64,
    configured_return_url: &str,
    requested_return_url: &str,
) -> Result<BeginOperation, BillingOperationError> {
    validate_return_url(configured_return_url, requested_return_url)?;
    let lifecycle: Option<String> =
        sqlx::query_scalar("SELECT lifecycle_state FROM organizations WHERE id = $1 FOR UPDATE")
            .bind(organization_id)
            .fetch_optional(&mut **tx)
            .await?;
    let Some(lifecycle) = lifecycle else {
        return Err(BillingOperationError::Unauthorised);
    };
    if lifecycle != "active" {
        return Err(BillingOperationError::OrganisationNotActive);
    }
    let role: Option<String> = sqlx::query_scalar(
        "SELECT role FROM organization_memberships WHERE org_id = $1 AND user_id = $2 FOR UPDATE",
    )
    .bind(organization_id)
    .bind(actor_user_id)
    .fetch_optional(&mut **tx)
    .await?;
    if !matches!(role.as_deref(), Some("owner" | "admin")) {
        return Err(BillingOperationError::Unauthorised);
    }
    let request = BillingOperationRequest::organization(
        actor_user_id,
        organization_id,
        idempotency_key,
        offer,
        quote_version,
        quote_expires_at_epoch,
    );
    begin_operation(tx, &request).await
}

/// Accept only the server-configured return target. Client-provided URLs are never normalised or
/// reflected into a provider session, so a forged redirect cannot be attached to an operation.
pub fn validate_return_url(configured: &str, requested: &str) -> Result<(), BillingOperationError> {
    if configured.is_empty() || requested != configured {
        return Err(BillingOperationError::UnauthorisedReturnUrl);
    }
    Ok(())
}

/// Record a provider outcome using the persisted operation identity. Unknown outcomes remain
/// explicitly recoverable and never become a fresh operation.
pub async fn record_provider_result(
    tx: &mut Transaction<'_, Postgres>,
    operation_id: &str,
    state: BillingOperationState,
    provider_operation_id: Option<&str>,
    result_code: Option<&str>,
) -> Result<BillingOperation, BillingOperationError> {
    if !matches!(
        state,
        BillingOperationState::Succeeded
            | BillingOperationState::Failed
            | BillingOperationState::Unknown
    ) {
        return Err(BillingOperationError::InvalidField("provider result state"));
    }
    if matches!(
        state,
        BillingOperationState::Succeeded | BillingOperationState::Failed
    ) && result_code.is_none_or(str::is_empty)
    {
        return Err(BillingOperationError::InvalidField("result_code"));
    }
    let result = sqlx::query(
        "UPDATE billing_operations SET state = $2, provider_operation_id = COALESCE($3, provider_operation_id), \
         result_code = $4, reconciliation_lease_token = NULL, reconciliation_lease_until = NULL, updated_at = now() \
         WHERE operation_id = $1 AND state = 'pending' \
         AND reconciliation_lease_token LIKE 'creator:%' \
         RETURNING operation_id",
    )
    .bind(operation_id)
    .bind(state.as_str())
    .bind(provider_operation_id)
    .bind(result_code)
    .fetch_optional(&mut **tx)
    .await?;
    if result.is_none() {
        let existing = load_operation(tx, operation_id).await?;
        if !matches!(
            existing.state,
            BillingOperationState::Succeeded
                | BillingOperationState::Failed
                | BillingOperationState::Unknown
        ) {
            return Err(BillingOperationError::NotPending);
        }
        if existing.state != state
            || existing.provider_operation_id.as_deref() != provider_operation_id
            || existing.result_code.as_deref() != result_code
        {
            return Err(BillingOperationError::ResultConflict);
        }
    }
    load_operation(tx, operation_id).await
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderResolution {
    Succeeded {
        provider_operation_id: String,
        result_code: String,
    },
    Failed {
        provider_operation_id: Option<String>,
        result_code: String,
    },
    Unknown {
        result_code: Option<String>,
    },
}

#[derive(Debug, Error)]
pub enum BillingRecoveryError {
    #[error("billing operation error: {0}")]
    Operation(#[from] BillingOperationError),
    #[error("billing provider recovery failed: {0}")]
    Provider(String),
    #[error("billing operation recovery is already in progress")]
    InProgress,
}

/// Provider lookup used by a restartable reconciliation worker. The provider must use the stored
/// provider idempotency key and operation identity; it must not create a new charge.
#[async_trait]
pub trait BillingOperationProvider: Send + Sync {
    async fn resolve(
        &self,
        operation: &BillingOperation,
    ) -> Result<ProviderResolution, BillingRecoveryError>;
}

/// Resolve one pending or unknown operation after a process restart. The provider call happens
/// outside the SQL transaction; the result is then committed exactly once under the operation id.
pub async fn reconcile_operation(
    pool: &PgPool,
    provider: &dyn BillingOperationProvider,
    operation_id: &str,
) -> Result<BillingOperation, BillingRecoveryError> {
    let claim_token = format!("reconcile:{}", Uuid::new_v4());
    let current = claim_reconciliation(pool, operation_id, &claim_token).await?;
    if !matches!(
        current.state,
        BillingOperationState::Pending | BillingOperationState::Unknown
    ) {
        return Ok(current);
    }
    // A provider error leaves the lease until its bounded expiry. This prevents an outage from
    // turning every worker retry into another provider request while keeping the row visible to
    // the reconciliation candidate query.
    let resolution = provider.resolve(&current).await?;
    let mut tx = pool.begin().await.map_err(BillingOperationError::from)?;
    let resolved = match resolution {
        ProviderResolution::Succeeded {
            provider_operation_id,
            result_code,
        } => {
            record_claimed_provider_result(
                &mut tx,
                operation_id,
                &claim_token,
                BillingOperationState::Succeeded,
                Some(&provider_operation_id),
                Some(&result_code),
            )
            .await?
        }
        ProviderResolution::Failed {
            provider_operation_id,
            result_code,
        } => {
            record_claimed_provider_result(
                &mut tx,
                operation_id,
                &claim_token,
                BillingOperationState::Failed,
                provider_operation_id.as_deref(),
                Some(&result_code),
            )
            .await?
        }
        ProviderResolution::Unknown { result_code } => {
            record_claimed_provider_result(
                &mut tx,
                operation_id,
                &claim_token,
                BillingOperationState::Unknown,
                None,
                result_code.as_deref(),
            )
            .await?
        }
    };
    tx.commit().await.map_err(BillingOperationError::from)?;
    Ok(resolved)
}

async fn claim_reconciliation(
    pool: &PgPool,
    operation_id: &str,
    claim_token: &str,
) -> Result<BillingOperation, BillingRecoveryError> {
    let mut tx = pool.begin().await.map_err(BillingOperationError::from)?;
    let claimed = sqlx::query(
        "UPDATE billing_operations SET reconciliation_lease_token = $2, \
         reconciliation_lease_until = now() + interval '5 minutes', updated_at = now() \
         WHERE operation_id = $1 AND state IN ('pending','unknown') \
         AND (reconciliation_lease_until IS NULL OR reconciliation_lease_until < now()) \
         RETURNING operation_id",
    )
    .bind(operation_id)
    .bind(claim_token)
    .fetch_optional(&mut *tx)
    .await
    .map_err(BillingOperationError::from)?
    .is_some();
    tx.commit().await.map_err(BillingOperationError::from)?;
    let current = load_operation_by_id(pool, operation_id).await?;
    if claimed {
        return Ok(current);
    }
    if matches!(
        current.state,
        BillingOperationState::Succeeded | BillingOperationState::Failed
    ) {
        return Ok(current);
    }
    Err(BillingRecoveryError::InProgress)
}

async fn record_claimed_provider_result(
    tx: &mut Transaction<'_, Postgres>,
    operation_id: &str,
    claim_token: &str,
    state: BillingOperationState,
    provider_operation_id: Option<&str>,
    result_code: Option<&str>,
) -> Result<BillingOperation, BillingRecoveryError> {
    validate_provider_result(state, result_code)?;
    let updated = sqlx::query(
        "UPDATE billing_operations SET state = $2, provider_operation_id = COALESCE($3, provider_operation_id), \
         result_code = $4, reconciliation_lease_token = NULL, reconciliation_lease_until = NULL, updated_at = now() \
         WHERE operation_id = $1 AND reconciliation_lease_token = $5 AND state IN ('pending','unknown') \
         RETURNING operation_id",
    )
    .bind(operation_id)
    .bind(state.as_str())
    .bind(provider_operation_id)
    .bind(result_code)
    .bind(claim_token)
    .fetch_optional(&mut **tx)
    .await
    .map_err(BillingOperationError::from)?;
    if updated.is_none() {
        let existing = load_operation(tx, operation_id).await?;
        if matches!(
            existing.state,
            BillingOperationState::Succeeded | BillingOperationState::Failed
        ) && existing.state == state
            && existing.provider_operation_id.as_deref() == provider_operation_id
            && existing.result_code.as_deref() == result_code
        {
            return Ok(existing);
        }
        return Err(BillingRecoveryError::InProgress);
    }
    Ok(load_operation(tx, operation_id).await?)
}

fn validate_provider_result(
    state: BillingOperationState,
    result_code: Option<&str>,
) -> Result<(), BillingOperationError> {
    if !matches!(
        state,
        BillingOperationState::Succeeded
            | BillingOperationState::Failed
            | BillingOperationState::Unknown
    ) {
        return Err(BillingOperationError::InvalidField("provider result state"));
    }
    if matches!(
        state,
        BillingOperationState::Succeeded | BillingOperationState::Failed
    ) && result_code.is_none_or(str::is_empty)
    {
        return Err(BillingOperationError::InvalidField("result_code"));
    }
    Ok(())
}

pub async fn load_reconciliation_candidates(
    pool: &PgPool,
    limit: i64,
) -> Result<Vec<BillingOperation>, BillingOperationError> {
    if !(1..=1_000).contains(&limit) {
        return Err(BillingOperationError::InvalidField("limit"));
    }
    let rows = sqlx::query(
        "SELECT operation_id, idempotency_key, request_hash, actor_user_id, payer_id, \
         beneficiary_id, offer, quote_version, quote_expires_at_epoch, provider_idempotency_key, \
         provider_operation_id, provider_checkout_url, state, result_code FROM billing_operations \
         WHERE state IN ('pending','unknown') ORDER BY updated_at, operation_id LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(operation_from_row).collect()
}

/// Load one operation only when it belongs to the authenticated actor. A missing row and a row
/// owned by another person are intentionally indistinguishable to the HTTP surface.
pub async fn load_operation_for_actor(
    pool: &PgPool,
    operation_id: &str,
    actor_user_id: &str,
) -> Result<Option<BillingOperation>, BillingOperationError> {
    let row = sqlx::query(
        "SELECT operation_id, idempotency_key, request_hash, actor_user_id, payer_id, \
         beneficiary_id, offer, quote_version, quote_expires_at_epoch, provider_idempotency_key, \
         provider_operation_id, provider_checkout_url, state, result_code \
         FROM billing_operations WHERE operation_id = $1",
    )
    .bind(operation_id)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let operation = operation_from_row(row)?;
    Ok((operation.actor_user_id == actor_user_id).then_some(operation))
}

async fn load_operation(
    tx: &mut Transaction<'_, Postgres>,
    operation_id: &str,
) -> Result<BillingOperation, BillingOperationError> {
    let row = sqlx::query(
        "SELECT operation_id, idempotency_key, request_hash, actor_user_id, payer_id, \
         beneficiary_id, offer, quote_version, quote_expires_at_epoch, provider_idempotency_key, \
         provider_operation_id, provider_checkout_url, state, result_code FROM billing_operations WHERE operation_id = $1",
    )
    .bind(operation_id)
    .fetch_one(&mut **tx)
    .await?;
    operation_from_row(row)
}

async fn load_operation_by_id(
    pool: &PgPool,
    operation_id: &str,
) -> Result<BillingOperation, BillingOperationError> {
    let row = sqlx::query(
        "SELECT operation_id, idempotency_key, request_hash, actor_user_id, payer_id, \
         beneficiary_id, offer, quote_version, quote_expires_at_epoch, provider_idempotency_key, \
         provider_operation_id, provider_checkout_url, state, result_code FROM billing_operations WHERE operation_id = $1",
    )
    .bind(operation_id)
    .fetch_one(pool)
    .await?;
    operation_from_row(row)
}

async fn load_by_idempotency(
    tx: &mut Transaction<'_, Postgres>,
    actor_user_id: &str,
    idempotency_key: &str,
) -> Result<Option<BillingOperation>, BillingOperationError> {
    let row = sqlx::query(
        "SELECT operation_id, idempotency_key, request_hash, actor_user_id, payer_id, \
         beneficiary_id, offer, quote_version, quote_expires_at_epoch, provider_idempotency_key, \
         provider_operation_id, provider_checkout_url, state, result_code FROM billing_operations \
         WHERE actor_user_id = $1 AND idempotency_key = $2 FOR UPDATE",
    )
    .bind(actor_user_id)
    .bind(idempotency_key)
    .fetch_optional(&mut **tx)
    .await?;
    row.map(operation_from_row).transpose()
}

fn operation_from_row(
    row: sqlx::postgres::PgRow,
) -> Result<BillingOperation, BillingOperationError> {
    Ok(BillingOperation {
        operation_id: row.try_get("operation_id")?,
        idempotency_key: row.try_get("idempotency_key")?,
        request_hash: row.try_get("request_hash")?,
        actor_user_id: row.try_get("actor_user_id")?,
        payer_id: row.try_get("payer_id")?,
        beneficiary_id: row.try_get("beneficiary_id")?,
        offer: row.try_get("offer")?,
        quote_version: row.try_get("quote_version")?,
        quote_expires_at_epoch: row.try_get("quote_expires_at_epoch")?,
        provider_idempotency_key: row.try_get("provider_idempotency_key")?,
        provider_operation_id: row.try_get("provider_operation_id")?,
        provider_checkout_url: row.try_get("provider_checkout_url")?,
        state: BillingOperationState::parse(row.try_get::<String, _>("state")?.as_str())?,
        result_code: row.try_get("result_code")?,
    })
}

fn current_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_secs() as i64
}

fn request_hash(
    actor_user_id: &str,
    payer_id: &str,
    beneficiary_id: &str,
    offer: BillingOffer,
    quote_version: i64,
    quote_expires_at_epoch: i64,
) -> String {
    let quote_version = quote_version.to_string();
    let quote_expires_at_epoch = quote_expires_at_epoch.to_string();
    identity_hash(&[
        actor_user_id,
        payer_id,
        beneficiary_id,
        offer.as_str(),
        &quote_version,
        &quote_expires_at_epoch,
    ])
}

fn identity_hash(parts: &[&str]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"sotto-billing-request-v1\0");
    for part in parts {
        digest.update(part.as_bytes());
        digest.update([0]);
    }
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> BillingOperationRequest {
        BillingOperationRequest::personal(
            "user_1",
            "idem_1",
            BillingOffer::StandardMonthly,
            1,
            2_000_000_000,
        )
    }

    #[test]
    fn request_requires_unexpired_quote_and_non_empty_identity() {
        let mut request = request();
        request.validate(1_000_000_000).unwrap();
        request.quote_expires_at_epoch = 1_000_000_000;
        assert!(matches!(
            request.validate(1_000_000_000),
            Err(BillingOperationError::QuoteExpired)
        ));
        request.quote_expires_at_epoch = 2_000_000_000;
        request.idempotency_key.clear();
        assert!(matches!(
            request.validate(1_000_000_000),
            Err(BillingOperationError::InvalidField("idempotency_key"))
        ));
    }

    #[test]
    fn state_values_are_closed_and_unknown_provider_results_remain_recoverable() {
        assert_eq!(
            BillingOperationState::parse("pending").unwrap(),
            BillingOperationState::Pending
        );
        assert_eq!(
            BillingOperationState::parse("unknown").unwrap(),
            BillingOperationState::Unknown
        );
        assert!(matches!(
            BillingOperationState::parse("paid"),
            Err(BillingOperationError::CorruptState)
        ));
    }

    #[test]
    fn return_target_must_match_the_server_configuration_exactly() {
        assert!(validate_return_url(
            "https://app.sotto.test/billing",
            "https://app.sotto.test/billing"
        )
        .is_ok());
        assert!(matches!(
            validate_return_url("https://app.sotto.test/billing", "https://evil.test"),
            Err(BillingOperationError::UnauthorisedReturnUrl)
        ));
        assert!(matches!(
            validate_return_url("", "https://evil.test"),
            Err(BillingOperationError::UnauthorisedReturnUrl)
        ));
    }

    #[test]
    fn provider_identity_is_stable_for_replays_but_distinct_for_new_idempotency_keys() {
        let first = BillingOperationRequest::personal(
            "user_1",
            "idem_1",
            BillingOffer::StandardMonthly,
            1,
            2_000_000_000,
        );
        let replay = BillingOperationRequest::personal(
            "user_1",
            "idem_1",
            BillingOffer::StandardMonthly,
            1,
            2_000_000_000,
        );
        let separate = BillingOperationRequest::personal(
            "user_1",
            "idem_2",
            BillingOffer::StandardMonthly,
            1,
            2_000_000_000,
        );
        assert_eq!(first.request_hash, replay.request_hash);
        assert_eq!(
            first.provider_idempotency_key,
            replay.provider_idempotency_key
        );
        assert_ne!(
            first.provider_idempotency_key,
            separate.provider_idempotency_key
        );
        let changed_expiry = BillingOperationRequest::personal(
            "user_1",
            "idem_1",
            BillingOffer::StandardMonthly,
            1,
            2_000_000_001,
        );
        assert_ne!(first.request_hash, changed_expiry.request_hash);
        let first_org = BillingOperationRequest::organization(
            "user_1",
            "org_a",
            "shared_key",
            BillingOffer::StandardMonthly,
            1,
            2_000_000_000,
        );
        let second_org = BillingOperationRequest::organization(
            "user_1",
            "org_b",
            "shared_key",
            BillingOffer::StandardMonthly,
            1,
            2_000_000_000,
        );
        assert_ne!(
            first_org.provider_idempotency_key,
            second_org.provider_idempotency_key
        );
    }
}
