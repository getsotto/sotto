//! Server configuration, read from the environment.

use crate::error::{Error, Result};

/// Default address the server binds to when `SOTTO_BIND` is unset.
const DEFAULT_BIND: &str = "127.0.0.1:8080";
/// Default public base URL used to build the OAuth callback when `SOTTO_PUBLIC_URL` is unset.
const DEFAULT_PUBLIC_URL: &str = "http://localhost:8080";

#[derive(Debug, Clone)]
pub struct Config {
    /// Postgres connection string.
    pub database_url: String,
    /// Address to bind the HTTP listener to.
    pub bind_addr: String,
    /// GitHub OAuth configuration, present only when credentials are set in the environment.
    pub oauth: Option<OAuthConfig>,
}

/// GitHub OAuth application credentials and the server's public origin.
#[derive(Debug, Clone)]
pub struct OAuthConfig {
    pub github_client_id: String,
    pub github_client_secret: String,
    /// Public origin of this server (e.g. `https://api.sotto.dev`), used to build the callback URL
    /// that GitHub redirects to. Must match the OAuth app's registered callback.
    pub public_base_url: String,
    /// Allowed web-app origin (e.g. `https://app.sotto.dev`), if a web client is deployed. A login
    /// whose `redirect_uri` matches this origin gets a cookie session; loopback stays CLI (URL
    /// token). `None` means no web client (loopback only).
    pub web_origin: Option<String>,
}

impl OAuthConfig {
    /// The fixed callback URL registered with the GitHub OAuth app.
    pub fn callback_url(&self) -> String {
        format!(
            "{}/auth/github/callback",
            self.public_base_url.trim_end_matches('/')
        )
    }

    /// Whether session cookies should carry the `Secure` attribute (inferred from the web origin
    /// scheme, so local http dev still works).
    pub fn secure_cookies(&self) -> bool {
        self.web_origin
            .as_deref()
            .is_some_and(|origin| origin.starts_with("https://"))
    }
}

impl Config {
    /// Load configuration from the environment.
    ///
    /// `DATABASE_URL` is required. OAuth is enabled only when both `GITHUB_CLIENT_ID` and
    /// `GITHUB_CLIENT_SECRET` are set, so the server still boots (health, migrations) without them.
    pub fn from_env() -> Result<Self> {
        let database_url = std::env::var("DATABASE_URL")
            .map_err(|_| Error::Config("DATABASE_URL is not set".into()))?;
        let bind_addr = std::env::var("SOTTO_BIND").unwrap_or_else(|_| DEFAULT_BIND.to_string());

        let oauth = match (
            std::env::var("GITHUB_CLIENT_ID"),
            std::env::var("GITHUB_CLIENT_SECRET"),
        ) {
            (Ok(github_client_id), Ok(github_client_secret)) => Some(OAuthConfig {
                github_client_id,
                github_client_secret,
                public_base_url: std::env::var("SOTTO_PUBLIC_URL")
                    .unwrap_or_else(|_| DEFAULT_PUBLIC_URL.to_string()),
                web_origin: std::env::var("SOTTO_WEB_ORIGIN").ok(),
            }),
            _ => None,
        };

        Ok(Self {
            database_url,
            bind_addr,
            oauth,
        })
    }
}
