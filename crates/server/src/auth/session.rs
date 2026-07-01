//! Session tokens: generation, hashing, issuance, and the [`AuthUser`] request extractor.
//!
//! The raw token (`st_<hex>`) is returned to the client exactly once. Only its BLAKE2b hash is
//! stored, so a database leak cannot be replayed as a valid session.

use axum::extract::FromRequestParts;
use axum::http::header::{AUTHORIZATION, COOKIE};
use axum::http::request::Parts;
use axum::http::HeaderMap;
use dryoc::generichash::{GenericHash, Key as GhKey};
use sqlx::PgPool;

use crate::error::{Error, Result};
use crate::state::AppState;

/// Bytes of randomness in a session token (256 bits).
const TOKEN_BYTES: usize = 32;
/// Human-recognizable prefix on the opaque bearer token.
const TOKEN_PREFIX: &str = "st_";
/// Session lifetime, expressed as a Postgres interval (TTL math stays in SQL).
const SESSION_TTL: &str = "30 days";
/// Same lifetime in seconds, for the cookie `Max-Age`.
const SESSION_TTL_SECONDS: i64 = 30 * 24 * 60 * 60;
/// Name of the web session cookie.
pub const SESSION_COOKIE: &str = "sotto_session";

/// The authenticated principal, produced by extracting and validating the bearer token.
pub struct AuthUser {
    pub user_id: String,
}

/// Issue a fresh session for `user_id` and return the raw bearer token (shown to the client once).
pub async fn issue(pool: &PgPool, user_id: &str) -> Result<String> {
    let mut raw = [0u8; TOKEN_BYTES];
    dryoc::rng::copy_randombytes(&mut raw);
    let token = format!("{TOKEN_PREFIX}{}", to_hex(&raw));

    sqlx::query(&format!(
        "INSERT INTO sessions (token_hash, user_id, expires_at) \
         VALUES ($1, $2, now() + interval '{SESSION_TTL}')"
    ))
    .bind(hash_token(&token))
    .bind(user_id)
    .execute(pool)
    .await?;

    Ok(token)
}

/// Resolve a raw bearer token to its `user_id`, bumping `last_used_at`. Returns `None` when the
/// token is unknown or expired.
pub async fn resolve(pool: &PgPool, token: &str) -> Result<Option<String>> {
    let user_id: Option<String> = sqlx::query_scalar(
        "UPDATE sessions SET last_used_at = now() \
         WHERE token_hash = $1 AND expires_at > now() RETURNING user_id",
    )
    .bind(hash_token(token))
    .fetch_optional(pool)
    .await?;
    Ok(user_id)
}

/// Delete a session (logout). A no-op if the token is unknown.
pub async fn revoke(pool: &PgPool, token: &str) -> Result<()> {
    sqlx::query("DELETE FROM sessions WHERE token_hash = $1")
        .bind(hash_token(token))
        .execute(pool)
        .await?;
    Ok(())
}

/// Extract the session token from a request: `Authorization: Bearer …` (CLI) or the
/// `sotto_session` cookie (web).
pub fn token_from_headers(headers: &HeaderMap) -> Option<String> {
    if let Some(bearer) = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        return Some(bearer.to_string());
    }
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|cookies| cookies.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(name, _)| *name == SESSION_COOKIE)
        .map(|(_, value)| value.to_string())
}

/// Build a `Set-Cookie` value carrying the session (web login).
pub fn session_cookie(token: &str, secure: bool) -> String {
    let mut cookie = format!(
        "{SESSION_COOKIE}={token}; HttpOnly; SameSite=Lax; Path=/; Max-Age={SESSION_TTL_SECONDS}"
    );
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

/// Build a `Set-Cookie` value that clears the session cookie (logout).
pub fn clear_cookie(secure: bool) -> String {
    let mut cookie = format!("{SESSION_COOKIE}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0");
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

/// BLAKE2b hash of the token string; this is what we persist and compare against.
fn hash_token(token: &str) -> Vec<u8> {
    GenericHash::hash_with_defaults_to_vec::<_, GhKey>(token.as_bytes(), None)
        .expect("BLAKE2b over a small input cannot fail")
}

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self> {
        let token = token_from_headers(&parts.headers).ok_or(Error::Unauthorized)?;
        let user_id = resolve(&state.pool, &token)
            .await?
            .ok_or(Error::Unauthorized)?;
        Ok(AuthUser { user_id })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashing_is_deterministic_and_distinct() {
        assert_eq!(hash_token("st_abc"), hash_token("st_abc"));
        assert_ne!(hash_token("st_abc"), hash_token("st_abd"));
        assert_eq!(hash_token("st_abc").len(), 32);
    }

    #[test]
    fn hex_round_trips_known_values() {
        assert_eq!(to_hex(&[0x00, 0x0f, 0xff]), "000fff");
        assert_eq!(to_hex(&[]), "");
    }
}
