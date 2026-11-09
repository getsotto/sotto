//! Verified lifecycle-notice contact enrolment.
//!
//! The database stores only a digest of each one-use token. Provider I/O starts only after its
//! message identity and attempt have committed, and no transaction is held while the sender runs.

use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use dryoc::rng::copy_randombytes;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Row, Transaction};
use url::Url;
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::error::{Error, Result};
use crate::state::AppState;

const CHALLENGE_TTL_SECONDS: i64 = 30 * 60;
const ACCOUNT_SEND_LIMIT: i32 = 5;
const DESTINATION_SEND_LIMIT: i32 = 3;

#[derive(Debug, Clone)]
pub struct VerificationMessage {
    pub message_id: String,
    pub destination: String,
    pub verification_url: String,
    pub expires_at_epoch: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendOutcome {
    Accepted { provider_message_id: String },
    Unknown,
    Rejected,
}

#[async_trait]
pub trait ContactVerificationSender: Send + Sync {
    async fn send(&self, message: VerificationMessage) -> SendOutcome;
}

#[derive(Clone)]
pub struct ContactVerificationService {
    pub enrolment_enabled: bool,
    pub sends_enabled: bool,
    web_origin: Url,
    sender: Arc<dyn ContactVerificationSender>,
}

impl ContactVerificationService {
    pub fn new(
        enrolment_enabled: bool,
        sends_enabled: bool,
        web_origin: &str,
        sender: Arc<dyn ContactVerificationSender>,
    ) -> Result<Self> {
        let origin = Url::parse(web_origin)
            .map_err(|_| Error::Config("contact verification web origin is invalid".into()))?;
        let local_http = origin.scheme() == "http"
            && matches!(origin.host_str(), Some("localhost" | "127.0.0.1" | "::1"));
        if !(origin.scheme() == "https" || local_http)
            || origin.host_str().is_none()
            || origin.path() != "/"
            || origin.query().is_some()
            || origin.fragment().is_some()
            || origin.username() != ""
            || origin.password().is_some()
        {
            return Err(Error::Config(
                "contact verification web origin must be an HTTPS origin".into(),
            ));
        }
        Ok(Self {
            enrolment_enabled,
            sends_enabled,
            web_origin: origin,
            sender,
        })
    }

    fn verification_url(&self, token: &str) -> String {
        let mut url = self.web_origin.clone();
        url.set_path("/cloud");
        url.set_fragment(Some(&format!("contact_token={token}")));
        url.to_string()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartVerificationRequest {
    pub email: String,
    pub idempotency_key: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfirmVerificationRequest {
    pub token: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContactStatus {
    pub email: Option<VerifiedEmailView>,
    pub latest_delivery: Option<EmailDeliveryView>,
    pub pending_expires_at_epoch: Option<i64>,
    pub enrolment_available: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VerifiedEmailView {
    pub contact_id: String,
    pub masked_destination: String,
    pub verified_at_epoch: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EmailDeliveryState {
    Queued,
    Accepted,
    DeliveryUnknown,
    Delivered,
    Failed,
    Bounced,
    Suppressed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EmailDeliveryView {
    pub state: EmailDeliveryState,
    pub updated_at_epoch: i64,
}

#[derive(Debug, Serialize)]
pub struct StartVerificationResponse {
    /// Deliberately identical for queued, duplicate, and unavailable destinations.
    pub accepted: bool,
}

#[derive(Debug, Serialize)]
pub struct ConfirmVerificationResponse {
    pub verified: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BeginOutcome {
    Send,
    AlreadyHandled,
}

#[derive(Debug)]
struct PreparedChallenge {
    challenge_id: String,
    token: String,
    expires_at_epoch: i64,
}

pub(crate) fn canonical_email(email: &str) -> std::result::Result<String, &'static str> {
    if email.chars().any(char::is_control) {
        return Err("email is invalid");
    }
    let email = email.trim();
    if email.len() < 3
        || email.len() > 320
        || !email.is_ascii()
        || email.chars().any(char::is_whitespace)
    {
        return Err("email is invalid");
    }
    let Some((local, domain)) = email.split_once('@') else {
        return Err("email is invalid");
    };
    if local.is_empty()
        || domain.is_empty()
        || domain.starts_with('.')
        || domain.ends_with('.')
        || !domain.contains('.')
        || domain.split('.').any(str::is_empty)
        || domain.len() > 253
        || email.matches('@').count() != 1
    {
        return Err("email is invalid");
    }
    Ok(email.to_ascii_lowercase())
}

fn digest(value: &[u8]) -> [u8; 32] {
    Sha256::digest(value).into()
}

fn digest_hex(value: &[u8]) -> String {
    digest(value)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn random_token() -> String {
    let mut bytes = [0_u8; 32];
    copy_randombytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn mask_email(email: &str) -> String {
    let (local, domain) = email.split_once('@').unwrap_or(("", ""));
    let first = local.chars().next().unwrap_or('*');
    format!("{first}***@{domain}")
}

async fn bump_limit(
    tx: &mut Transaction<'_, Postgres>,
    kind: &str,
    scope_digest: &str,
    limit: i32,
) -> Result<bool> {
    let count: i32 = sqlx::query_scalar(
        "INSERT INTO cloud_contact_verification_rate_limits \
         (scope_kind, scope_digest, window_started_at, attempt_count) \
         VALUES ($1, $2, now(), 1) \
         ON CONFLICT (scope_kind, scope_digest) DO UPDATE SET \
           window_started_at = CASE WHEN cloud_contact_verification_rate_limits.window_started_at \
             <= now() - interval '1 hour' THEN now() \
             ELSE cloud_contact_verification_rate_limits.window_started_at END, \
           attempt_count = CASE WHEN cloud_contact_verification_rate_limits.window_started_at \
             <= now() - interval '1 hour' THEN 1 \
             ELSE cloud_contact_verification_rate_limits.attempt_count + 1 END \
         RETURNING attempt_count",
    )
    .bind(kind)
    .bind(scope_digest)
    .fetch_one(&mut **tx)
    .await?;
    Ok(count <= limit)
}

async fn prune_old_data(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    sqlx::query(
        "WITH expired AS ( \
           SELECT challenge_id FROM cloud_contact_verification_challenges \
           WHERE status IN ('sending','pending','delivery_unknown') AND expires_at<=now() \
           ORDER BY expires_at LIMIT 200 FOR UPDATE SKIP LOCKED \
         ) \
         UPDATE cloud_contact_verification_challenges AS challenge SET status='expired' \
         FROM expired WHERE challenge.challenge_id=expired.challenge_id",
    )
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "WITH stale AS ( \
           SELECT challenge_id FROM cloud_contact_verification_challenges \
           WHERE status IN ('verified','superseded','expired','failed') \
             AND created_at < now() - interval '30 days' \
           ORDER BY created_at LIMIT 200 FOR UPDATE SKIP LOCKED \
         ) \
         DELETE FROM cloud_contact_verification_challenges AS challenge \
         USING stale WHERE challenge.challenge_id=stale.challenge_id",
    )
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "WITH stale AS ( \
           SELECT scope_kind,scope_digest FROM cloud_contact_verification_rate_limits \
           WHERE window_started_at < now() - interval '24 hours' \
           ORDER BY window_started_at LIMIT 200 FOR UPDATE SKIP LOCKED \
         ) \
         DELETE FROM cloud_contact_verification_rate_limits AS limits \
         USING stale WHERE limits.scope_kind=stale.scope_kind AND limits.scope_digest=stale.scope_digest",
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn prepare_challenge(
    pool: &PgPool,
    user_id: &str,
    email: &str,
    idempotency_key: &str,
) -> Result<(BeginOutcome, Option<PreparedChallenge>)> {
    let canonical = canonical_email(email).map_err(|error| Error::BadRequest(error.into()))?;
    let key = idempotency_key.trim();
    if key.is_empty() || key.len() > 128 || key.chars().any(char::is_control) || !key.is_ascii() {
        return Err(Error::BadRequest("idempotency key is invalid".into()));
    }

    let destination_digest = digest_hex(canonical.as_bytes());
    let request_digest = digest_hex(format!("email:{canonical}").as_bytes());
    let token = random_token();
    let token_digest = digest(token.as_bytes()).to_vec();
    let challenge_id = format!("contact-verification:{}", Uuid::new_v4());
    let message_id = challenge_id.clone();
    let mut tx = pool.begin().await?;

    // Serialise a user's challenge changes without holding a lock over provider I/O.
    let user_exists =
        sqlx::query_scalar::<_, String>("SELECT id FROM users WHERE id=$1 FOR UPDATE")
            .bind(user_id)
            .fetch_optional(&mut *tx)
            .await?;
    if user_exists.is_none() {
        return Err(Error::Unauthorized);
    }

    prune_old_data(&mut tx).await?;

    if let Some(existing_digest) = sqlx::query_scalar::<_, String>(
        "SELECT request_digest FROM cloud_contact_verification_challenges \
         WHERE user_id=$1 AND idempotency_key=$2",
    )
    .bind(user_id)
    .bind(key)
    .fetch_optional(&mut *tx)
    .await?
    {
        tx.commit().await?;
        if existing_digest != request_digest {
            return Err(Error::Conflict(
                "idempotency key was used for another request".into(),
            ));
        }
        return Ok((BeginOutcome::AlreadyHandled, None));
    }

    let account_allowed = bump_limit(
        &mut tx,
        "account",
        &digest_hex(user_id.as_bytes()),
        ACCOUNT_SEND_LIMIT,
    )
    .await?;
    let destination_allowed = bump_limit(
        &mut tx,
        "destination",
        &destination_digest,
        DESTINATION_SEND_LIMIT,
    )
    .await?;
    if !account_allowed || !destination_allowed {
        tx.commit().await?;
        return Err(Error::RateLimited(
            "verification requests are temporarily limited".into(),
        ));
    }

    // A destination already active on another account receives no message. Persist a generic
    // terminal challenge so retries with the same idempotency key behave exactly like any other.
    let destination_taken: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM cloud_verified_contacts \
         WHERE lower(destination)=lower($1) AND revoked_at IS NULL AND user_id<>$2)",
    )
    .bind(&canonical)
    .bind(user_id)
    .fetch_one(&mut *tx)
    .await?;

    sqlx::query(
        "UPDATE cloud_contact_verification_challenges SET status='superseded' \
         WHERE user_id=$1 AND status IN ('sending','pending','delivery_unknown')",
    )
    .bind(user_id)
    .execute(&mut *tx)
    .await?;

    if destination_taken {
        sqlx::query(
            "INSERT INTO cloud_contact_verification_challenges \
             (challenge_id,user_id,destination,destination_digest,token_digest,idempotency_key, \
              request_digest,status,expires_at,last_error_code) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,'failed',now()+interval '30 minutes','destination_unavailable')",
        )
        .bind(&challenge_id)
        .bind(user_id)
        .bind(&canonical)
        .bind(&destination_digest)
        .bind(&token_digest)
        .bind(key)
        .bind(&request_digest)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        return Ok((BeginOutcome::AlreadyHandled, None));
    }

    let expires_at_epoch = sqlx::query_scalar::<_, i64>(
        "INSERT INTO cloud_contact_verification_challenges \
         (challenge_id,user_id,destination,destination_digest,token_digest,idempotency_key,request_digest,status,expires_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,'sending',now()+($8 * interval '1 second')) \
         RETURNING extract(epoch from expires_at)::bigint",
    )
    .bind(&challenge_id)
    .bind(user_id)
    .bind(&canonical)
    .bind(&destination_digest)
    .bind(token_digest)
    .bind(key)
    .bind(&request_digest)
    .bind(CHALLENGE_TTL_SECONDS)
    .fetch_one(&mut *tx)
    .await?;

    sqlx::query(
        "INSERT INTO cloud_contact_verification_send_attempts \
         (challenge_id,attempt_number,message_id,outcome) VALUES ($1,1,$2,'prepared')",
    )
    .bind(&challenge_id)
    .bind(&message_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok((
        BeginOutcome::Send,
        Some(PreparedChallenge {
            challenge_id,
            token,
            expires_at_epoch,
        }),
    ))
}

async fn record_send_outcome(
    pool: &PgPool,
    challenge_id: &str,
    outcome: SendOutcome,
) -> Result<()> {
    let (attempt_outcome, challenge_status, error_code, provider_message_id) = match outcome {
        SendOutcome::Accepted {
            provider_message_id,
        } => ("accepted", "pending", None, Some(provider_message_id)),
        SendOutcome::Unknown => (
            "unknown",
            "delivery_unknown",
            Some("provider_unknown"),
            None,
        ),
        SendOutcome::Rejected => ("rejected", "failed", Some("provider_rejected"), None),
    };
    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE cloud_contact_verification_send_attempts SET outcome=$2,error_code=$3, \
           provider_message_id=$4,completed_at=now() \
         WHERE challenge_id=$1 AND attempt_number=1 AND outcome='prepared'",
    )
    .bind(challenge_id)
    .bind(attempt_outcome)
    .bind(error_code)
    .bind(provider_message_id)
    .execute(&mut *tx)
    .await?;
    // A fast user may already have confirmed, or a newer request may have superseded this one.
    sqlx::query(
        "UPDATE cloud_contact_verification_challenges SET status=$2,last_error_code=$3 \
         WHERE challenge_id=$1 AND status='sending'",
    )
    .bind(challenge_id)
    .bind(challenge_status)
    .bind(error_code)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn start_verification(
    pool: &PgPool,
    service: &ContactVerificationService,
    user_id: &str,
    request: StartVerificationRequest,
) -> Result<StartVerificationResponse> {
    if !service.enrolment_enabled || !service.sends_enabled {
        return Err(Error::NotConfigured(
            "contact verification is unavailable".into(),
        ));
    }
    let (outcome, prepared) =
        prepare_challenge(pool, user_id, &request.email, &request.idempotency_key).await?;
    if outcome == BeginOutcome::AlreadyHandled {
        return Ok(StartVerificationResponse { accepted: true });
    }
    let prepared =
        prepared.ok_or_else(|| Error::Internal("prepared contact challenge is missing".into()))?;
    let message = VerificationMessage {
        message_id: prepared.challenge_id.clone(),
        destination: canonical_email(&request.email)
            .map_err(|error| Error::BadRequest(error.into()))?,
        verification_url: service.verification_url(&prepared.token),
        expires_at_epoch: prepared.expires_at_epoch,
    };
    // Do not let provider latency reveal whether the destination belongs to another account.
    // Both the "already handled" path above and this path return after durable preparation;
    // delivery and its outcome recording happen outside the request.
    let sender = service.sender.clone();
    let pool = pool.clone();
    let challenge_id = prepared.challenge_id;
    tokio::spawn(async move {
        let outcome = sender.send(message).await;
        if let Err(error) = record_send_outcome(&pool, &challenge_id, outcome).await {
            eprintln!("contact verification send outcome could not be recorded: {error}");
        }
    });
    Ok(StartVerificationResponse { accepted: true })
}

pub async fn confirm_verification(pool: &PgPool, user_id: &str, token: &str) -> Result<bool> {
    if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Ok(false);
    }
    let token_digest = digest(token.as_bytes()).to_vec();
    let contact_id = format!("verified-contact:{}", Uuid::new_v4());
    let mut tx = pool.begin().await?;
    let challenge = sqlx::query(
        "SELECT challenge_id,destination FROM cloud_contact_verification_challenges \
         WHERE user_id=$1 AND token_digest=$2 AND status IN ('sending','pending','delivery_unknown') \
           AND expires_at>now() FOR UPDATE",
    )
    .bind(user_id)
    .bind(token_digest)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(challenge) = challenge else {
        tx.commit().await?;
        return Ok(false);
    };
    let challenge_id: String = challenge.try_get("challenge_id")?;
    let destination: String = challenge.try_get("destination")?;
    // The old contact remains valid unless the new destination can actually be committed.
    sqlx::query(
        "UPDATE cloud_verified_contacts SET revoked_at=now() \
         WHERE user_id=$1 AND channel='email' AND revoked_at IS NULL",
    )
    .bind(user_id)
    .execute(&mut *tx)
    .await?;
    let inserted = sqlx::query(
        "INSERT INTO cloud_verified_contacts(contact_id,user_id,channel,destination,verified_at) \
         VALUES ($1,$2,'email',$3,now()) ON CONFLICT DO NOTHING",
    )
    .bind(&contact_id)
    .bind(user_id)
    .bind(&destination)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if inserted != 1 {
        tx.rollback().await?;
        return Ok(false);
    }
    sqlx::query(
        "UPDATE cloud_notice_outbox SET status='cancelled',cancelled_at=now(),lease_owner=NULL, \
             lease_expires_at=NULL,updated_at=now() \
         WHERE recipient_user_id=$1 AND channel='email' AND contact_id<>$2 \
           AND status IN ('pending','leased')",
    )
    .bind(user_id)
    .bind(&contact_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE cloud_contact_verification_challenges SET status='verified',verified_at=now() \
         WHERE challenge_id=$1",
    )
    .bind(challenge_id)
    .execute(&mut *tx)
    .await?;
    crate::notifications::enqueue_current_for_verified_contact_in_tx(&mut tx, user_id, &contact_id)
        .await
        .map_err(|error| match error {
            crate::notifications::NoticeError::Database(error) => Error::Db(error),
            other => Error::Internal(other.to_string()),
        })?;
    tx.commit().await?;
    Ok(true)
}

pub async fn contact_status(pool: &PgPool, user_id: &str) -> Result<ContactStatus> {
    let email = sqlx::query(
        "SELECT contact_id,destination,extract(epoch from verified_at)::bigint AS verified_at \
         FROM cloud_verified_contacts WHERE user_id=$1 AND channel='email' AND revoked_at IS NULL",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?
    .map(|row| {
        Ok::<_, sqlx::Error>(VerifiedEmailView {
            contact_id: row.try_get("contact_id")?,
            masked_destination: mask_email(&row.try_get::<String, _>("destination")?),
            verified_at_epoch: row.try_get("verified_at")?,
        })
    })
    .transpose()?;
    let latest_delivery = sqlx::query(
        "SELECT status, extract(epoch FROM updated_at)::bigint AS updated_at \
         FROM cloud_notice_outbox WHERE recipient_user_id=$1 \
           AND channel='email' AND status <> 'cancelled' \
         ORDER BY created_at DESC, notice_id DESC LIMIT 1",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?
    .map(|row| {
        let status: String = row.try_get("status")?;
        let state = match status.as_str() {
            "pending" | "leased" => EmailDeliveryState::Queued,
            "accepted" => EmailDeliveryState::Accepted,
            "delivery_unknown" => EmailDeliveryState::DeliveryUnknown,
            "delivered" => EmailDeliveryState::Delivered,
            "failed" => EmailDeliveryState::Failed,
            "bounced" => EmailDeliveryState::Bounced,
            "suppressed" => EmailDeliveryState::Suppressed,
            // The database constraint protects this branch; keep the endpoint closed if a
            // future migration adds a status without updating this public summary.
            _ => {
                return Err(sqlx::Error::ColumnDecode {
                    index: "status".to_owned(),
                    source: Box::new(std::io::Error::other("unknown email delivery status")),
                })
            }
        };
        Ok::<_, sqlx::Error>(EmailDeliveryView {
            state,
            updated_at_epoch: row.try_get("updated_at")?,
        })
    })
    .transpose()?;
    // Keep the self-view response the same for usable and unavailable destinations. Otherwise a
    // caller could submit a known address and distinguish another account's active contact by
    // checking whether the request appears pending.
    let pending_expires_at_epoch = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT CASE WHEN status IN ('sending','pending','delivery_unknown','failed') \
                      AND expires_at>now() THEN extract(epoch from expires_at)::bigint \
                    ELSE NULL END \
         FROM cloud_contact_verification_challenges WHERE user_id=$1 \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?
    .flatten();
    Ok(ContactStatus {
        email,
        latest_delivery,
        pending_expires_at_epoch,
        enrolment_available: false,
    })
}

pub async fn revoke_verified_contact(pool: &PgPool, user_id: &str) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE cloud_verified_contacts SET revoked_at=now() \
         WHERE user_id=$1 AND channel='email' AND revoked_at IS NULL",
    )
    .bind(user_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE cloud_notice_outbox SET status='cancelled',cancelled_at=now(),lease_owner=NULL, \
             lease_expires_at=NULL,updated_at=now() \
         WHERE recipient_user_id=$1 AND channel='email' AND status IN ('pending','leased')",
    )
    .bind(user_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE cloud_contact_verification_challenges SET status='superseded' \
         WHERE user_id=$1 AND status IN ('sending','pending','delivery_unknown')",
    )
    .bind(user_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/account/contacts", get(get_status).delete(delete_contact))
        .route("/account/contacts/verification", post(start))
        .route("/account/contacts/verification/confirm", post(confirm))
}

async fn get_status(State(state): State<AppState>, user: AuthUser) -> Result<Response> {
    let mut status = contact_status(&state.pool, &user.user_id).await?;
    status.enrolment_available = state
        .contact_verification
        .as_deref()
        .is_some_and(|service| service.enrolment_enabled && service.sends_enabled);
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(status)).into_response())
}

async fn start(
    State(state): State<AppState>,
    user: AuthUser,
    Json(request): Json<StartVerificationRequest>,
) -> Result<Response> {
    let service = state
        .contact_verification
        .as_deref()
        .ok_or_else(|| Error::NotConfigured("contact verification is unavailable".into()))?;
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(start_verification(&state.pool, service, &user.user_id, request).await?),
    )
        .into_response())
}

async fn confirm(
    State(state): State<AppState>,
    user: AuthUser,
    Json(request): Json<ConfirmVerificationRequest>,
) -> Result<Response> {
    let service = state
        .contact_verification
        .as_deref()
        .ok_or_else(|| Error::NotConfigured("contact verification is unavailable".into()))?;
    if !service.enrolment_enabled || !service.sends_enabled {
        return Err(Error::NotConfigured(
            "contact verification is unavailable".into(),
        ));
    }
    let verified = confirm_verification(&state.pool, &user.user_id, &request.token).await?;
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(ConfirmVerificationResponse { verified }),
    )
        .into_response())
}

async fn delete_contact(State(state): State<AppState>, user: AuthUser) -> Result<Response> {
    revoke_verified_contact(&state.pool, &user.user_id).await?;
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        StatusCode::NO_CONTENT,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoopSender;

    #[async_trait]
    impl ContactVerificationSender for NoopSender {
        async fn send(&self, _message: VerificationMessage) -> SendOutcome {
            SendOutcome::Unknown
        }
    }

    #[test]
    fn email_input_and_callback_origin_are_strictly_validated() {
        assert_eq!(
            canonical_email(" Person@Example.Test ").unwrap(),
            "person@example.test"
        );
        for invalid in [
            "",
            "not-an-email",
            "a@b",
            "a@@example.test",
            "a b@example.test",
            "a@example.test\n",
        ] {
            assert!(canonical_email(invalid).is_err(), "accepted {invalid:?}");
        }
        let sender: Arc<dyn ContactVerificationSender> = Arc::new(NoopSender);
        for invalid_origin in [
            "http://example.test",
            "https://example.test/path",
            "https://example.test?next=x",
        ] {
            assert!(
                ContactVerificationService::new(true, true, invalid_origin, sender.clone())
                    .is_err()
            );
        }
        assert!(
            ContactVerificationService::new(true, true, "https://example.test", sender).is_ok()
        );
    }
}
