//! OAuth login/callback handlers and the authenticated `/auth/me` endpoint.

use axum::extract::{Query, State};
use axum::http::header::SET_COOKIE;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
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

    validate_redirect(&params.redirect_uri, config.web_origin.as_deref())?;

    // Opportunistically drop login states that can no longer succeed (older than the freshness
    // window the callback enforces), so abandoned or bot-initiated logins don't accumulate.
    sqlx::query(&format!(
        "DELETE FROM oauth_logins WHERE created_at < now() - interval '{LOGIN_FRESHNESS}'"
    ))
    .execute(&state.pool)
    .await?;

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
/// user, mint a session, and hand it back: a web login (redirect matches the web origin) gets an
/// httpOnly cookie; a CLI login (loopback) gets the token in the redirect URL.
pub async fn callback(
    State(state): State<AppState>,
    Query(params): Query<CallbackParams>,
) -> Result<Response> {
    let config = state
        .oauth_config
        .as_ref()
        .ok_or_else(|| Error::NotConfigured("oauth is not configured".into()))?;
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

    let mut url = Url::parse(&cli_redirect_uri)
        .map_err(|_| Error::BadRequest("stored redirect_uri is invalid".into()))?;

    if is_web_redirect(&url, config.web_origin.as_deref()) {
        // Web: set an httpOnly cookie; keep the token out of the URL/history.
        url.query_pairs_mut().append_pair("state", &cli_state);
        let cookie = session::session_cookie(&token, config.secure_cookies());
        Ok(([(SET_COOKIE, cookie)], Redirect::to(url.as_str())).into_response())
    } else {
        // CLI loopback: hand the token to the local listener via the redirect URL.
        url.query_pairs_mut()
            .append_pair("session", &token)
            .append_pair("state", &cli_state);
        Ok(Redirect::to(url.as_str()).into_response())
    }
}

/// `POST /auth/logout` — delete the session and clear the web cookie. Works for either transport.
pub async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Result<Response> {
    if let Some(token) = session::token_from_headers(&headers) {
        session::revoke(&state.pool, &token).await?;
    }
    let secure = state
        .oauth_config
        .as_ref()
        .is_some_and(|c| c.secure_cookies());
    Ok((
        [(SET_COOKIE, session::clear_cookie(secure))],
        StatusCode::NO_CONTENT,
    )
        .into_response())
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

/// Reject any redirect target that is neither an `http` loopback address (CLI) nor the configured
/// web origin (web) — otherwise a crafted `redirect_uri` could exfiltrate the session.
fn validate_redirect(redirect_uri: &str, web_origin: Option<&str>) -> Result<()> {
    let url = Url::parse(redirect_uri)
        .map_err(|_| Error::BadRequest("redirect_uri is not a valid URL".into()))?;
    if is_loopback(&url) || is_web_redirect(&url, web_origin) {
        Ok(())
    } else {
        Err(Error::BadRequest(
            "redirect_uri must be a loopback address or the configured web origin".into(),
        ))
    }
}

/// An `http` loopback address (`127.0.0.0/8`, `::1`, or `localhost`) — the CLI's local listener.
fn is_loopback(url: &Url) -> bool {
    if url.scheme() != "http" {
        return false;
    }
    match url.host() {
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        Some(Host::Domain(d)) => d == "localhost",
        None => false,
    }
}

/// Whether `url` is same-origin (scheme + host + port) with the configured web origin.
fn is_web_redirect(url: &Url, web_origin: Option<&str>) -> bool {
    let Some(origin) = web_origin.and_then(|o| Url::parse(o).ok()) else {
        return false;
    };
    url.scheme() == origin.scheme()
        && url.host() == origin.host()
        && url.port_or_known_default() == origin.port_or_known_default()
}

#[cfg(test)]
mod tests {
    use super::validate_redirect;

    const WEB: Option<&str> = Some("https://app.sotto.dev");

    #[test]
    fn accepts_loopback_targets() {
        assert!(validate_redirect("http://127.0.0.1:1234/cb", None).is_ok());
        assert!(validate_redirect("http://localhost:55555/", None).is_ok());
        assert!(validate_redirect("http://[::1]:9000/cb", None).is_ok());
    }

    #[test]
    fn accepts_the_configured_web_origin() {
        assert!(validate_redirect("https://app.sotto.dev/auth/callback", WEB).is_ok());
        assert!(validate_redirect("https://app.sotto.dev/", WEB).is_ok());
        // A different origin is rejected even when a web origin is configured.
        assert!(validate_redirect("https://evil.example.com/cb", WEB).is_err());
        // The web origin isn't accepted when unconfigured.
        assert!(validate_redirect("https://app.sotto.dev/auth/callback", None).is_err());
    }

    #[test]
    fn rejects_non_loopback_or_non_http() {
        assert!(validate_redirect("https://evil.example.com/cb", None).is_err());
        assert!(validate_redirect("http://evil.example.com/cb", None).is_err());
        assert!(validate_redirect("http://169.254.169.254/", None).is_err());
        assert!(validate_redirect("not a url", None).is_err());
    }
}
