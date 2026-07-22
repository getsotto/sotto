//! Server configuration, read from the environment.

use crate::error::{Error, Result};

/// Default address the server binds to when `SOTTO_BIND` is unset.
const DEFAULT_BIND: &str = "127.0.0.1:8080";
/// Default public base URL used to build the OAuth callback when `SOTTO_PUBLIC_URL` is unset.
const DEFAULT_PUBLIC_URL: &str = "http://localhost:8080";
/// Default endpoint the anonymous version ping reports to (the hosted instance).
const DEFAULT_TELEMETRY_URL: &str = "https://getsotto.co.uk/telemetry/v1/ping";

#[derive(Debug, Clone)]
pub struct Config {
    /// Postgres connection string.
    pub database_url: String,
    /// Address to bind the HTTP listener to.
    pub bind_addr: String,
    /// GitHub OAuth configuration, present only when credentials are set in the environment.
    pub oauth: Option<OAuthConfig>,
    /// Stripe billing configuration, present only when the `STRIPE_*` variables are set.
    pub billing: Option<BillingConfig>,
    /// Anonymous version-ping telemetry (see [`crate::telemetry`] and the README).
    pub telemetry: TelemetryConfig,
}

/// Anonymous version-ping telemetry settings (see [`crate::telemetry`]).
#[derive(Debug, Clone)]
pub struct TelemetryConfig {
    /// Send the daily anonymous ping. Default **on**; `SOTTO_TELEMETRY=off` (also `0`/`false`/
    /// `no`) or the cross-tool `DO_NOT_TRACK=1` turns it off before any request is ever made.
    pub ping_enabled: bool,
    /// Where the ping goes; `SOTTO_TELEMETRY_URL` overrides it (tests, private fleets).
    pub endpoint: String,
    /// Receive and count pings (`SOTTO_TELEMETRY_INGEST=1`) - set only on the hosted instance.
    /// Everywhere else `POST /telemetry/v1/ping` returns 503 (the ships-dark pattern).
    pub ingest_enabled: bool,
}

/// Stripe billing credentials and the single subscription price.
///
/// All three come from the Stripe dashboard; the price id (not a number) lives here so pricing is
/// an operational decision, never a code change. Billing endpoints return 503 when this is absent
/// - the integration ships dark and is enabled by setting the environment variables.
#[derive(Debug, Clone)]
pub struct BillingConfig {
    /// Secret API key (`sk_test_…` / `sk_live_…`).
    pub secret_key: String,
    /// Webhook signing secret (`whsec_…`) for `POST /billing/webhook`.
    pub webhook_secret: String,
    /// The Price id (`price_…`) of the flat per-org monthly Team subscription.
    pub price_id: String,
    /// Where Stripe-hosted pages send the browser back to (the web app origin).
    pub return_url: String,
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
    /// `GITHUB_CLIENT_SECRET` are set, and billing only when all three `STRIPE_*` variables are,
    /// so the server still boots (health, migrations) without them. Empty values count as unset -
    /// docker compose interpolation (`${VAR:-}`) exports empties for every blank `.env` line.
    pub fn from_env() -> Result<Self> {
        let database_url = std::env::var("DATABASE_URL")
            .map_err(|_| Error::Config("DATABASE_URL is not set".into()))?;
        let bind_addr = std::env::var("SOTTO_BIND").unwrap_or_else(|_| DEFAULT_BIND.to_string());
        let public_base_url =
            env_nonempty("SOTTO_PUBLIC_URL").unwrap_or_else(|| DEFAULT_PUBLIC_URL.to_string());

        let oauth = match (
            env_nonempty("GITHUB_CLIENT_ID"),
            env_nonempty("GITHUB_CLIENT_SECRET"),
        ) {
            (Some(github_client_id), Some(github_client_secret)) => Some(OAuthConfig {
                github_client_id,
                github_client_secret,
                public_base_url: public_base_url.clone(),
                web_origin: env_nonempty("SOTTO_WEB_ORIGIN"),
            }),
            _ => None,
        };

        let billing = match (
            env_nonempty("STRIPE_SECRET_KEY"),
            env_nonempty("STRIPE_WEBHOOK_SECRET"),
            env_nonempty("STRIPE_PRICE_ID"),
        ) {
            (Some(secret_key), Some(webhook_secret), Some(price_id)) => Some(BillingConfig {
                secret_key,
                webhook_secret,
                price_id,
                return_url: public_base_url,
            }),
            _ => None,
        };

        let telemetry = TelemetryConfig {
            ping_enabled: telemetry_ping_enabled(
                env_nonempty("SOTTO_TELEMETRY").as_deref(),
                env_nonempty("DO_NOT_TRACK").as_deref(),
            ),
            endpoint: env_nonempty("SOTTO_TELEMETRY_URL")
                .unwrap_or_else(|| DEFAULT_TELEMETRY_URL.to_string()),
            ingest_enabled: env_nonempty("SOTTO_TELEMETRY_INGEST").as_deref() == Some("1"),
        };

        Ok(Self {
            database_url,
            bind_addr,
            oauth,
            billing,
            telemetry,
        })
    }
}

/// The telemetry opt-out decision, separated from env access so the matrix is unit-testable.
/// Any opt-out signal wins: `SOTTO_TELEMETRY` set to an off-value, or `DO_NOT_TRACK` set to
/// anything but `"0"` (the <https://consoledonottrack.com> convention).
fn telemetry_ping_enabled(sotto_telemetry: Option<&str>, do_not_track: Option<&str>) -> bool {
    if let Some(v) = sotto_telemetry {
        if matches!(
            v.to_ascii_lowercase().as_str(),
            "off" | "0" | "false" | "no"
        ) {
            return false;
        }
    }
    if let Some(v) = do_not_track {
        if v != "0" {
            return false;
        }
    }
    true
}

/// An environment variable, with empty/whitespace values treated as unset.
fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::telemetry_ping_enabled;

    #[test]
    fn telemetry_defaults_on_and_every_opt_out_signal_wins() {
        assert!(telemetry_ping_enabled(None, None)); // the default
        assert!(telemetry_ping_enabled(Some("on"), None)); // explicit on
        assert!(telemetry_ping_enabled(Some("anything-else"), None)); // unrecognised ≠ off
        assert!(telemetry_ping_enabled(None, Some("0"))); // DNT explicitly cleared

        assert!(!telemetry_ping_enabled(Some("off"), None));
        assert!(!telemetry_ping_enabled(Some("OFF"), None));
        assert!(!telemetry_ping_enabled(Some("0"), None));
        assert!(!telemetry_ping_enabled(Some("false"), None));
        assert!(!telemetry_ping_enabled(Some("no"), None));
        assert!(!telemetry_ping_enabled(None, Some("1")));
        assert!(!telemetry_ping_enabled(None, Some("true")));
        // Opt-out beats an explicit opt-in - when signals disagree, privacy wins.
        assert!(!telemetry_ping_enabled(Some("on"), Some("1")));
    }
}
