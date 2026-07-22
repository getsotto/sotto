//! OAuth identity providers.
//!
//! The provider is abstracted behind [`OAuthProvider`] so handlers depend on the trait, not on
//! GitHub specifically - production wires up [`GithubOAuth`], tests inject a mock.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{ACCEPT, AUTHORIZATION, USER_AGENT};

use crate::error::{Error, Result};

/// A verified identity returned by an OAuth provider after a successful code exchange.
#[derive(Debug, Clone)]
pub struct Identity {
    /// Provider name, e.g. `"github"`.
    pub provider: String,
    /// Stable, provider-assigned account id (GitHub's numeric user id - never the username, which
    /// can be renamed).
    pub subject: String,
    /// Primary verified email, if the provider exposes one.
    pub email: Option<String>,
}

/// Exchanges an OAuth authorisation code for a verified [`Identity`].
#[async_trait]
pub trait OAuthProvider: Send + Sync {
    async fn exchange_code(&self, code: &str) -> Result<Identity>;
}

/// GitHub OAuth: exchanges the code for an access token, then reads the user's identity.
pub struct GithubOAuth {
    client_id: String,
    client_secret: String,
    redirect_uri: String,
    http: reqwest::Client,
}

impl GithubOAuth {
    pub fn new(client_id: String, client_secret: String, redirect_uri: String) -> Self {
        Self {
            client_id,
            client_secret,
            redirect_uri,
            // Bound every upstream call: a stalled GitHub (slow DNS/TLS, hung connection) must not
            // tie up the request task and its socket indefinitely.
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .connect_timeout(Duration::from_secs(5))
                .build()
                .expect("reqwest client with static config builds"),
        }
    }

    /// GitHub may return a null email on `/user` (privacy setting); fall back to the primary
    /// verified address from `/user/emails`.
    async fn primary_email(&self, access_token: &str) -> Option<String> {
        #[derive(serde::Deserialize)]
        struct Email {
            email: String,
            primary: bool,
            verified: bool,
        }
        let emails: Vec<Email> = self
            .http
            .get("https://api.github.com/user/emails")
            .header(AUTHORIZATION, format!("Bearer {access_token}"))
            .header(USER_AGENT, "sotto-server")
            .header(ACCEPT, "application/vnd.github+json")
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;
        emails
            .into_iter()
            .find(|e| e.primary && e.verified)
            .map(|e| e.email)
    }
}

#[async_trait]
impl OAuthProvider for GithubOAuth {
    async fn exchange_code(&self, code: &str) -> Result<Identity> {
        #[derive(serde::Deserialize)]
        struct TokenResponse {
            access_token: Option<String>,
        }
        let token: TokenResponse = self
            .http
            .post("https://github.com/login/oauth/access_token")
            .header(ACCEPT, "application/json")
            .form(&[
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
                ("code", code),
                ("redirect_uri", self.redirect_uri.as_str()),
            ])
            .send()
            .await
            .map_err(|e| Error::Upstream(e.to_string()))?
            .error_for_status()
            .map_err(|e| Error::Upstream(e.to_string()))?
            .json()
            .await
            .map_err(|e| Error::Upstream(e.to_string()))?;

        let access_token = token
            .access_token
            .ok_or_else(|| Error::Upstream("github did not return an access token".into()))?;

        #[derive(serde::Deserialize)]
        struct GithubUser {
            id: i64,
            email: Option<String>,
        }
        let user: GithubUser = self
            .http
            .get("https://api.github.com/user")
            .header(AUTHORIZATION, format!("Bearer {access_token}"))
            .header(USER_AGENT, "sotto-server")
            .header(ACCEPT, "application/vnd.github+json")
            .send()
            .await
            .map_err(|e| Error::Upstream(e.to_string()))?
            .error_for_status()
            .map_err(|e| Error::Upstream(e.to_string()))?
            .json()
            .await
            .map_err(|e| Error::Upstream(e.to_string()))?;

        let email = match user.email {
            Some(email) => Some(email),
            None => self.primary_email(&access_token).await,
        };

        Ok(Identity {
            provider: "github".into(),
            subject: user.id.to_string(),
            email,
        })
    }
}
