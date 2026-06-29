//! OAuth login/callback handlers and the authenticated `/auth/me` endpoint.

use axum::extract::{Query, State};
use axum::response::Redirect;
use axum::Json;
use serde::{Deserialize, Serialize};
use url::{Host, Url};

use crate::auth::session::{self, AuthUser};
use crate::auth::Identity;
use crate::error::{Error, Result};
use crate::state::AppState;

/// GitHub's authorization endpoint and the scopes we request (read-only identity + email).
const GITHUB_AUTHORIZE_URL: &str = "https://github.com/login/oauth/authorize";
const GITHUB_SCOPE: &str = "read:user user:email";
/// How long an in-flight login may sit before its CSRF state is rejected.
const LOGIN_FRESHNESS: &str = "10 minutes";

#[derive(Deserialize)]
pub struct LoginParams {
    /// The CLI's loopback callback (must be an `http` loopback address).
    redirect_uri: String,
    /// Opaque value the CLI uses to correlate its own request; echoed back unchanged.
    state: String,
}

/// `GET /auth/github/login` — begin the flow: record CSRF state, redirect the browser to GitHub.
pub async fn login(
    State(state): State<AppState>,
    Query(params): Query<LoginParams>,
) -> Result<Redirect> {
    let config = state
        .oauth_config
        .as_ref()
        .ok_or_else(|| Error::NotConfigured("oauth is not configured".into()))?;

    validate_loopback(&params.redirect_uri)?;

    let server_state = random_state();
    sqlx::query(
        "INSERT INTO oauth_logins (state, cli_redirect_uri, cli_state) VALUES ($1, $2, $3)",
    )
    .bind(&server_state)
    .bind(&params.redirect_uri)
    .bind(&params.state)
    .execute(&state.pool)
    .await?;

    let mut url = Url::parse(GITHUB_AUTHORIZE_URL).expect("static URL is valid");
    url.query_pairs_mut()
        .append_pair("client_id", &config.github_client_id)
        .append_pair("redirect_uri", &config.callback_url())
        .append_pair("scope", GITHUB_SCOPE)
        .append_pair("state", &server_state);

    Ok(Redirect::to(url.as_str()))
}

#[derive(Deserialize)]
pub struct CallbackParams {
    code: String,
    state: String,
}

/// `GET /auth/github/callback` — GitHub redirects here. Verify state, exchange the code, upsert the
/// user, mint a session, and hand the token back to the CLI's loopback listener.
pub async fn callback(
    State(state): State<AppState>,
    Query(params): Query<CallbackParams>,
) -> Result<Redirect> {
    let provider = state
        .oauth
        .as_ref()
        .ok_or_else(|| Error::NotConfigured("oauth is not configured".into()))?;

    // Consume the login state (single-use) and learn whether it is still fresh, in one statement.
    let login: Option<(String, String, bool)> = sqlx::query_as(&format!(
        "DELETE FROM oauth_logins WHERE state = $1 \
         RETURNING cli_redirect_uri, cli_state, (created_at > now() - interval '{LOGIN_FRESHNESS}')"
    ))
    .bind(&params.state)
    .fetch_optional(&state.pool)
    .await?;

    let (cli_redirect_uri, cli_state, fresh) =
        login.ok_or_else(|| Error::BadRequest("unknown login state".into()))?;
    if !fresh {
        return Err(Error::BadRequest("login state expired".into()));
    }

    let identity = provider.exchange_code(&params.code).await?;
    let user_id = upsert_user(&state.pool, &identity).await?;
    let token = session::issue(&state.pool, &user_id).await?;

    // redirect_uri was validated as a loopback when the login began.
    let mut url = Url::parse(&cli_redirect_uri)
        .map_err(|_| Error::BadRequest("stored redirect_uri is invalid".into()))?;
    url.query_pairs_mut()
        .append_pair("session", &token)
        .append_pair("state", &cli_state);

    Ok(Redirect::to(url.as_str()))
}

#[derive(Serialize)]
pub struct MeResponse {
    user_id: String,
}

/// `GET /auth/me` — returns the authenticated user; exercises the [`AuthUser`] extractor.
pub async fn me(user: AuthUser) -> Json<MeResponse> {
    Json(MeResponse {
        user_id: user.user_id,
    })
}

/// Insert the user on first login, or return the existing id on subsequent logins (refreshing the
/// email only when the provider supplies one, so a later login without an email doesn't erase a
/// previously stored address). The `(oauth_provider, oauth_subject)` pair is the stable identity key.
async fn upsert_user(pool: &sqlx::PgPool, identity: &Identity) -> Result<String> {
    let user_id: String = sqlx::query_scalar(
        "INSERT INTO users (id, oauth_provider, oauth_subject, email) VALUES ($1, $2, $3, $4) \
         ON CONFLICT (oauth_provider, oauth_subject) \
         DO UPDATE SET email = COALESCE(EXCLUDED.email, users.email) \
         RETURNING id",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(&identity.provider)
    .bind(&identity.subject)
    .bind(&identity.email)
    .fetch_one(pool)
    .await?;
    Ok(user_id)
}

/// A 256-bit random, hex-encoded CSRF/login state value.
fn random_state() -> String {
    let mut raw = [0u8; 32];
    dryoc::rng::copy_randombytes(&mut raw);
    raw.iter().map(|b| format!("{b:02x}")).collect()
}

/// Reject any redirect target that is not an `http` loopback address — otherwise a crafted
/// `redirect_uri` could exfiltrate the freshly minted session token.
fn validate_loopback(redirect_uri: &str) -> Result<()> {
    let url = Url::parse(redirect_uri)
        .map_err(|_| Error::BadRequest("redirect_uri is not a valid URL".into()))?;
    if url.scheme() != "http" {
        return Err(Error::BadRequest(
            "redirect_uri must use http on a loopback address".into(),
        ));
    }
    let is_loopback = match url.host() {
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        Some(Host::Domain(d)) => d == "localhost",
        None => false,
    };
    if is_loopback {
        Ok(())
    } else {
        Err(Error::BadRequest(
            "redirect_uri must be a loopback address".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::validate_loopback;

    #[test]
    fn accepts_loopback_targets() {
        assert!(validate_loopback("http://127.0.0.1:1234/cb").is_ok());
        assert!(validate_loopback("http://localhost:55555/").is_ok());
        assert!(validate_loopback("http://[::1]:9000/cb").is_ok());
    }

    #[test]
    fn rejects_non_loopback_or_non_http() {
        assert!(validate_loopback("https://evil.example.com/cb").is_err());
        assert!(validate_loopback("http://evil.example.com/cb").is_err());
        assert!(validate_loopback("http://169.254.169.254/").is_err());
        assert!(validate_loopback("not a url").is_err());
    }
}
