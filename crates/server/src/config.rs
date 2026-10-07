//! Server configuration, read from the environment.

pub use crate::billing_catalogue::BillingPriceIds;
use crate::error::{Error, Result};

/// Identifies who operates this server. This is deployment metadata, not an entitlement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeploymentMode {
    SelfHosted,
    Cloud,
}

impl DeploymentMode {
    pub const ENV: &'static str = "SOTTO_DEPLOYMENT_MODE";

    fn from_env_result(value: std::result::Result<String, std::env::VarError>) -> Result<Self> {
        match value {
            Ok(value) => Self::parse(Some(&value)),
            Err(std::env::VarError::NotPresent) => Self::parse(None),
            Err(std::env::VarError::NotUnicode(_)) => Err(Error::Config(format!(
                "{} must be valid UTF-8 and either self_hosted or cloud",
                Self::ENV
            ))),
        }
    }

    fn parse(value: Option<&str>) -> Result<Self> {
        match value.map(str::trim).filter(|value| !value.is_empty()) {
            None | Some("self_hosted") => Ok(Self::SelfHosted),
            Some("cloud") => Ok(Self::Cloud),
            Some(value) => Err(Error::Config(format!(
                "{} must be either self_hosted or cloud, got {value:?}",
                Self::ENV
            ))),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SelfHosted => "self_hosted",
            Self::Cloud => "cloud",
        }
    }
}

/// Default address the server binds to when `SOTTO_BIND` is unset.
const DEFAULT_BIND: &str = "127.0.0.1:8080";
/// Default public base URL used to build the OAuth callback when `SOTTO_PUBLIC_URL` is unset.
const DEFAULT_PUBLIC_URL: &str = "http://localhost:8080";
/// Default endpoint the anonymous version ping reports to (the hosted instance).
const DEFAULT_TELEMETRY_URL: &str = "https://getsotto.co.uk/telemetry/v1/ping";
/// Default recovery window for a new organisation-deletion request.
pub const DEFAULT_ORGANISATION_DELETION_RETENTION_DAYS: i64 = 30;
/// Maximum recovery window for a new organisation-deletion request, preventing fat-fingered
/// values from freezing an organisation for years while leaving room for long backup lifecycles.
pub const MAX_ORGANISATION_DELETION_RETENTION_DAYS: i64 = 365;
const ORGANISATION_DELETION_RETENTION_ENV: &str = "SOTTO_ORGANISATION_DELETION_RETENTION_DAYS";
const ORGANISATION_DELETION_WORKER_ENV: &str = "SOTTO_ORGANISATION_DELETION_WORKER_ENABLED";
const ORGANISATION_DELETION_METRICS_TOKEN_ENV: &str = "SOTTO_ORGANISATION_DELETION_METRICS_TOKEN";
const ORGANISATION_DELETION_OPERATOR_TOKEN_ENV: &str = "SOTTO_ORGANISATION_DELETION_OPERATOR_TOKEN";
const PROVIDER_REFRESH_INGEST_ENV: &str = "SOTTO_PROVIDER_REFRESH_INGEST_ENABLED";
const PROVIDER_REFRESH_WORKER_ENV: &str = "SOTTO_PROVIDER_REFRESH_WORKER_ENABLED";
const PROVIDER_REFRESH_RECONCILIATION_ENV: &str = "SOTTO_PROVIDER_REFRESH_RECONCILIATION_ENABLED";
const CLOUD_ACTION_ENFORCEMENT_ENV: &str = "SOTTO_CLOUD_ACTION_ENFORCEMENT";
const MACHINE_ELIGIBILITY_ENFORCEMENT_ENV: &str = "SOTTO_MACHINE_ELIGIBILITY_ENFORCEMENT";
const CLOUD_SALES_ENV: &str = "SOTTO_CLOUD_SALES_ENABLED";

#[derive(Debug, Clone)]
pub struct Config {
    /// Postgres connection string.
    pub database_url: String,
    /// Address to bind the HTTP listener to.
    pub bind_addr: String,
    /// Whether this server is operated by Sotto or by a self-hosting customer.
    pub deployment_mode: DeploymentMode,
    /// GitHub OAuth configuration, present only when credentials are set in the environment.
    pub oauth: Option<OAuthConfig>,
    /// Stripe billing configuration, present only for Cloud deployments when the `STRIPE_*` variables are set.
    pub billing: Option<BillingConfig>,
    /// Anonymous version-ping telemetry (see [`crate::telemetry`] and the README).
    pub telemetry: TelemetryConfig,
    /// Recovery window applied to new organisation-deletion requests.
    pub organisation_deletion_retention_days: i64,
    /// Whether this instance runs the staged organisation-deletion worker.
    pub organisation_deletion_worker_enabled: bool,
    /// Bearer token for the protected organisation-deletion metrics exporter.
    pub organisation_deletion_metrics_token: Option<String>,
    /// Bearer token for the protected operator-observation endpoint.
    pub organisation_deletion_operator_token: Option<String>,
    /// Whether verified provider changes may enqueue durable refresh work.
    pub provider_refresh_ingest_enabled: bool,
    /// Whether a runtime worker may claim and process durable refresh work.
    pub provider_refresh_worker_enabled: bool,
    /// Whether the periodic registered-source repair scan is enabled.
    pub provider_refresh_reconciliation_enabled: bool,
    /// Whether the dormant human hosted action policy rejects ineligible requests.
    pub cloud_action_enforcement_enabled: bool,
    /// Whether hosted machine retrieval and creation apply the accountable beneficiary policy.
    pub machine_eligibility_enforcement_enabled: bool,
    /// Whether new hosted subscriptions may be quoted or purchased. Existing billing management
    /// and verified webhooks stay available when this is off.
    pub cloud_sales_enabled: bool,
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

/// Stripe billing credentials, legacy organisation price, and optional hosted catalogue.
///
/// The ids come from the Stripe dashboard; amounts and recurrence are validated separately so
/// pricing remains an operational decision without allowing arbitrary client-selected prices.
/// Legacy billing endpoints return 503 when this configuration is absent.
#[derive(Debug, Clone)]
pub struct BillingConfig {
    /// Restricted API key (`rk_test_…` / `rk_live_…`).
    pub api_key: String,
    /// Webhook signing secret (`whsec_…`) for `POST /billing/webhook`.
    pub webhook_secret: String,
    /// The legacy Price id (`price_…`) of the flat per-org monthly Team subscription.
    pub price_id: String,
    /// The optional server-owned four-offer catalogue. Partial configuration is rejected at
    /// boot, and new hosted sales require all four prices plus the explicit sales switch.
    pub price_catalogue: Option<BillingPriceIds>,
    /// Explicit purchase gate. Management, cancellation, refunds, and webhooks do not use it.
    pub cloud_sales_enabled: bool,
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
    /// `GITHUB_CLIENT_SECRET` are set, and Cloud billing only when all three legacy `STRIPE_*`
    /// variables are present, so the server still boots (health, migrations) without them. Empty
    /// values count as unset - docker compose interpolation (`${VAR:-}`) exports empties for every
    /// blank `.env` line.
    pub fn from_env() -> Result<Self> {
        let database_url = std::env::var("DATABASE_URL")
            .map_err(|_| Error::Config("DATABASE_URL is not set".into()))?;
        let bind_addr = std::env::var("SOTTO_BIND").unwrap_or_else(|_| DEFAULT_BIND.to_string());
        let deployment_mode = DeploymentMode::from_env_result(std::env::var(DeploymentMode::ENV))?;
        let cloud_sales_enabled =
            feature_flag_is_enabled(std::env::var(CLOUD_SALES_ENV).ok().as_deref());
        let stripe_variables_present = [
            "STRIPE_API_KEY",
            "STRIPE_WEBHOOK_SECRET",
            "STRIPE_PRICE_ID",
            "STRIPE_STANDARD_MONTHLY_PRICE_ID",
            "STRIPE_STANDARD_ANNUAL_PRICE_ID",
            "STRIPE_FOUNDING_MONTHLY_PRICE_ID",
            "STRIPE_FOUNDING_ANNUAL_PRICE_ID",
        ]
        .into_iter()
        .any(|name| env_nonempty(name).is_some());
        validate_deployment_billing_boundary(deployment_mode, stripe_variables_present)?;
        let public_base_url =
            env_nonempty("SOTTO_PUBLIC_URL").unwrap_or_else(|| DEFAULT_PUBLIC_URL.to_string());
        let web_origin = env_nonempty("SOTTO_WEB_ORIGIN");

        let oauth = match (
            env_nonempty("GITHUB_CLIENT_ID"),
            env_nonempty("GITHUB_CLIENT_SECRET"),
        ) {
            (Some(github_client_id), Some(github_client_secret)) => Some(OAuthConfig {
                github_client_id,
                github_client_secret,
                public_base_url: public_base_url.clone(),
                web_origin: web_origin.clone(),
            }),
            _ => None,
        };

        // The boundary above rejects Stripe credentials on self-hosted deployments before any
        // billing state is built. Cloud remains the only mode that can construct a provider.
        let billing_price_catalogue = if deployment_mode == DeploymentMode::Cloud {
            billing_price_catalogue_from_env()?
        } else {
            None
        };
        let hosted_catalogue_configured = billing_price_catalogue.is_some();
        let billing = if deployment_mode == DeploymentMode::Cloud {
            match (
                env_nonempty("STRIPE_API_KEY"),
                env_nonempty("STRIPE_WEBHOOK_SECRET"),
                env_nonempty("STRIPE_PRICE_ID"),
            ) {
                (Some(api_key), Some(webhook_secret), Some(price_id)) => Some(BillingConfig {
                    api_key,
                    webhook_secret,
                    price_id,
                    price_catalogue: billing_price_catalogue,
                    cloud_sales_enabled,
                    return_url: billing_return_url(&public_base_url, web_origin.as_deref()),
                }),
                _ => None,
            }
        } else {
            None
        };
        if hosted_catalogue_configured && billing.is_none() {
            // The hosted catalogue is dormant until the hosted billing route lands. Keep one
            // complete Stripe configuration boundary for now so legacy billing cannot silently
            // disappear while the new ids are present.
            return Err(Error::Config(
                "hosted Stripe price ids require STRIPE_API_KEY, STRIPE_WEBHOOK_SECRET, and STRIPE_PRICE_ID"
                    .into(),
            ));
        }
        validate_cloud_sales_configuration(
            cloud_sales_enabled,
            deployment_mode,
            billing.is_some(),
            hosted_catalogue_configured,
        )?;

        let telemetry = TelemetryConfig {
            ping_enabled: telemetry_ping_enabled(
                env_nonempty("SOTTO_TELEMETRY").as_deref(),
                env_nonempty("DO_NOT_TRACK").as_deref(),
            ),
            endpoint: env_nonempty("SOTTO_TELEMETRY_URL")
                .unwrap_or_else(|| DEFAULT_TELEMETRY_URL.to_string()),
            ingest_enabled: env_nonempty("SOTTO_TELEMETRY_INGEST").as_deref() == Some("1"),
        };
        let organisation_deletion_retention_days = organisation_deletion_retention_from_env()?;
        let worker_enabled_value = std::env::var(ORGANISATION_DELETION_WORKER_ENV).ok();
        let organisation_deletion_worker_enabled =
            organisation_deletion_worker_is_enabled(worker_enabled_value.as_deref());
        let organisation_deletion_metrics_token =
            env_nonempty(ORGANISATION_DELETION_METRICS_TOKEN_ENV);
        let organisation_deletion_operator_token =
            env_nonempty(ORGANISATION_DELETION_OPERATOR_TOKEN_ENV);
        let provider_refresh_ingest_enabled =
            feature_flag_is_enabled(std::env::var(PROVIDER_REFRESH_INGEST_ENV).ok().as_deref());
        let provider_refresh_worker_enabled =
            feature_flag_is_enabled(std::env::var(PROVIDER_REFRESH_WORKER_ENV).ok().as_deref());
        let provider_refresh_reconciliation_enabled = feature_flag_is_enabled(
            std::env::var(PROVIDER_REFRESH_RECONCILIATION_ENV)
                .ok()
                .as_deref(),
        );
        let cloud_action_enforcement_enabled =
            feature_flag_is_enabled(std::env::var(CLOUD_ACTION_ENFORCEMENT_ENV).ok().as_deref());
        let machine_eligibility_enforcement_enabled = feature_flag_is_enabled(
            std::env::var(MACHINE_ELIGIBILITY_ENFORCEMENT_ENV)
                .ok()
                .as_deref(),
        );

        Ok(Self {
            database_url,
            bind_addr,
            deployment_mode,
            oauth,
            billing,
            telemetry,
            organisation_deletion_retention_days,
            organisation_deletion_worker_enabled,
            organisation_deletion_metrics_token,
            organisation_deletion_operator_token,
            provider_refresh_ingest_enabled,
            provider_refresh_worker_enabled,
            provider_refresh_reconciliation_enabled,
            cloud_action_enforcement_enabled,
            machine_eligibility_enforcement_enabled,
            cloud_sales_enabled,
        })
    }
}

const BILLING_CATALOGUE_ENV: [&str; 4] = [
    "STRIPE_STANDARD_MONTHLY_PRICE_ID",
    "STRIPE_STANDARD_ANNUAL_PRICE_ID",
    "STRIPE_FOUNDING_MONTHLY_PRICE_ID",
    "STRIPE_FOUNDING_ANNUAL_PRICE_ID",
];

fn validate_deployment_billing_boundary(
    deployment_mode: DeploymentMode,
    stripe_variables_present: bool,
) -> Result<()> {
    if deployment_mode == DeploymentMode::SelfHosted && stripe_variables_present {
        return Err(Error::Config(
            "Stripe configuration requires SOTTO_DEPLOYMENT_MODE=cloud".into(),
        ));
    }
    Ok(())
}

fn validate_cloud_sales_configuration(
    enabled: bool,
    deployment_mode: DeploymentMode,
    billing_configured: bool,
    hosted_catalogue_configured: bool,
) -> Result<()> {
    if enabled
        && (deployment_mode != DeploymentMode::Cloud
            || !billing_configured
            || !hosted_catalogue_configured)
    {
        return Err(Error::Config(format!(
            "{CLOUD_SALES_ENV}=1 requires SOTTO_DEPLOYMENT_MODE=cloud, complete Stripe credentials, and all four hosted price ids"
        )));
    }
    Ok(())
}

fn billing_price_catalogue_from_env() -> Result<Option<BillingPriceIds>> {
    let values = BILLING_CATALOGUE_ENV
        .iter()
        .map(|name| env_nonempty(name))
        .collect::<Vec<_>>()
        .try_into()
        .expect("billing catalogue has four environment variables");
    billing_price_catalogue_from_values(values)
}

fn billing_price_catalogue_from_values(
    values: [Option<String>; 4],
) -> Result<Option<BillingPriceIds>> {
    if values.iter().all(Option::is_none) {
        return Ok(None);
    }
    if values.iter().any(Option::is_none) {
        return Err(Error::Config(
            "all four hosted Stripe price ids must be configured together".into(),
        ));
    }
    let [standard_monthly, standard_annual, founding_monthly, founding_annual] = values;
    Ok(Some(BillingPriceIds {
        standard_monthly: standard_monthly.expect("checked above"),
        standard_annual: standard_annual.expect("checked above"),
        founding_monthly: founding_monthly.expect("checked above"),
        founding_annual: founding_annual.expect("checked above"),
    }))
}

/// Enable the destructive worker only for the exact opt-in value, so empty or unexpected values
/// keep the staged lifecycle disabled until an operator has completed the enablement checklist.
fn organisation_deletion_worker_is_enabled(value: Option<&str>) -> bool {
    value == Some("1")
}

/// Provider refresh switches are deliberately exact and independently opt-in. Empty, malformed,
/// or whitespace-padded values leave the corresponding path disabled during rollout.
fn feature_flag_is_enabled(value: Option<&str>) -> bool {
    value == Some("1")
}

/// Parse the configurable recovery window while keeping a safe default for existing deployments.
/// Whitespace around a present value is ignored before validating it against the accepted range.
fn parse_organisation_deletion_retention_days(value: Option<&str>) -> Result<i64> {
    let Some(value) = value else {
        return Ok(DEFAULT_ORGANISATION_DELETION_RETENTION_DAYS);
    };
    let days = value
        .trim()
        .parse::<i64>()
        .ok()
        .filter(|days| (1..=MAX_ORGANISATION_DELETION_RETENTION_DAYS).contains(days))
        .ok_or_else(invalid_organisation_deletion_retention)?;
    Ok(days)
}

/// Read the retention setting from the process environment.
fn organisation_deletion_retention_from_env() -> Result<i64> {
    organisation_deletion_retention_from_env_result(std::env::var(
        ORGANISATION_DELETION_RETENTION_ENV,
    ))
}

/// Preserve the distinction between an unset setting and an explicitly invalid value. Unlike
/// optional URL settings, an empty recovery window must fail boot rather than hide a deployment
/// policy error behind the 30-day default.
fn organisation_deletion_retention_from_env_result(
    value: std::result::Result<String, std::env::VarError>,
) -> Result<i64> {
    match value {
        Ok(value) => parse_organisation_deletion_retention_days(Some(&value)),
        Err(std::env::VarError::NotPresent) => parse_organisation_deletion_retention_days(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(invalid_organisation_deletion_retention()),
    }
}

fn invalid_organisation_deletion_retention() -> Error {
    Error::Config(format!(
        "{ORGANISATION_DELETION_RETENTION_ENV} must be an integer between 1 and {MAX_ORGANISATION_DELETION_RETENTION_DAYS}"
    ))
}

/// Return users to the browser application's origin when it is deployed separately from the API.
/// A shared origin remains the fallback for the hosted reverse-proxy layout and existing installs.
fn billing_return_url(public_base_url: &str, web_origin: Option<&str>) -> String {
    web_origin.unwrap_or(public_base_url).to_string()
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
    use super::{
        billing_price_catalogue_from_values, billing_return_url, feature_flag_is_enabled,
        organisation_deletion_retention_from_env_result, organisation_deletion_worker_is_enabled,
        parse_organisation_deletion_retention_days, telemetry_ping_enabled,
        validate_cloud_sales_configuration, validate_deployment_billing_boundary, DeploymentMode,
        DEFAULT_ORGANISATION_DELETION_RETENTION_DAYS, MAX_ORGANISATION_DELETION_RETENTION_DAYS,
    };

    #[test]
    fn deployment_mode_defaults_to_self_hosted_and_rejects_typos() {
        assert_eq!(
            DeploymentMode::parse(None).unwrap(),
            DeploymentMode::SelfHosted
        );
        assert_eq!(
            DeploymentMode::parse(Some("self_hosted")).unwrap(),
            DeploymentMode::SelfHosted
        );
        assert_eq!(
            DeploymentMode::parse(Some(" cloud ")).unwrap(),
            DeploymentMode::Cloud
        );
        assert!(DeploymentMode::parse(Some("hosted")).is_err());
        assert!(DeploymentMode::parse(Some("SELF_HOSTED")).is_err());

        #[cfg(unix)]
        {
            use std::ffi::OsString;
            use std::os::unix::ffi::OsStringExt;

            assert!(
                DeploymentMode::from_env_result(Err(std::env::VarError::NotUnicode(
                    OsString::from_vec(vec![0xff])
                )))
                .is_err()
            );
        }
    }

    #[test]
    fn stripe_configuration_requires_cloud_mode() {
        assert!(validate_deployment_billing_boundary(DeploymentMode::SelfHosted, true).is_err());
        assert!(validate_deployment_billing_boundary(DeploymentMode::SelfHosted, false).is_ok());
        assert!(validate_deployment_billing_boundary(DeploymentMode::Cloud, true).is_ok());
    }

    #[test]
    fn cloud_sales_requires_cloud_billing_and_all_hosted_prices() {
        assert!(validate_cloud_sales_configuration(
            false,
            DeploymentMode::SelfHosted,
            false,
            false,
        )
        .is_ok());
        assert!(
            validate_cloud_sales_configuration(true, DeploymentMode::SelfHosted, true, true,)
                .is_err()
        );
        assert!(
            validate_cloud_sales_configuration(true, DeploymentMode::Cloud, false, true,).is_err()
        );
        assert!(
            validate_cloud_sales_configuration(true, DeploymentMode::Cloud, true, false,).is_err()
        );
        assert!(
            validate_cloud_sales_configuration(true, DeploymentMode::Cloud, true, true,).is_ok()
        );
    }

    #[test]
    fn billing_returns_to_the_web_origin_when_configured() {
        assert_eq!(
            billing_return_url("https://api.sotto.test", Some("https://app.sotto.test")),
            "https://app.sotto.test"
        );
        assert_eq!(
            billing_return_url("https://sotto.test", None),
            "https://sotto.test"
        );
    }

    #[test]
    fn billing_price_catalogue_requires_all_four_ids() {
        assert_eq!(
            billing_price_catalogue_from_values([None, None, None, None]).unwrap(),
            None
        );
        assert!(billing_price_catalogue_from_values([
            Some("price_month".into()),
            None,
            None,
            None,
        ])
        .is_err());
        assert_eq!(
            billing_price_catalogue_from_values([
                Some("price_month".into()),
                Some("price_year".into()),
                Some("price_founder_month".into()),
                Some("price_founder_year".into()),
            ])
            .unwrap()
            .unwrap()
            .standard_monthly,
            "price_month"
        );
    }

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

    #[test]
    fn organisation_deletion_retention_config_covers_environment_and_parser() {
        assert_eq!(
            parse_organisation_deletion_retention_days(None).unwrap(),
            DEFAULT_ORGANISATION_DELETION_RETENTION_DAYS
        );
        assert_eq!(
            parse_organisation_deletion_retention_days(Some(" 45 ")).unwrap(),
            45
        );
        assert_eq!(
            parse_organisation_deletion_retention_days(Some("365")).unwrap(),
            MAX_ORGANISATION_DELETION_RETENTION_DAYS
        );
        assert_eq!(
            parse_organisation_deletion_retention_days(Some("1")).unwrap(),
            1
        );
        for value in ["", "0", "-1", "366", "thirty"] {
            assert!(parse_organisation_deletion_retention_days(Some(value)).is_err());
        }
        let huge = i64::MAX.to_string();
        assert!(parse_organisation_deletion_retention_days(Some(&huge)).is_err());
        assert_eq!(
            organisation_deletion_retention_from_env_result(Err(std::env::VarError::NotPresent,))
                .unwrap(),
            DEFAULT_ORGANISATION_DELETION_RETENTION_DAYS
        );
        assert_eq!(
            organisation_deletion_retention_from_env_result(Ok(" 45 ".into())).unwrap(),
            45
        );
        for value in ["", "  "] {
            assert!(organisation_deletion_retention_from_env_result(Ok(value.into())).is_err());
        }
        #[cfg(unix)]
        {
            // Invalid bytes are constructible here; keep the NotUnicode branch covered locally.
            use std::ffi::OsString;
            use std::os::unix::ffi::OsStringExt;

            assert!(organisation_deletion_retention_from_env_result(Err(
                std::env::VarError::NotUnicode(OsString::from_vec(vec![0xff])),
            ))
            .is_err());
        }
    }

    #[test]
    fn organisation_deletion_worker_requires_exact_opt_in() {
        assert!(!organisation_deletion_worker_is_enabled(None));
        assert!(!organisation_deletion_worker_is_enabled(Some("")));
        assert!(!organisation_deletion_worker_is_enabled(Some("true")));
        assert!(!organisation_deletion_worker_is_enabled(Some(" 1 ")));
        assert!(organisation_deletion_worker_is_enabled(Some("1")));
    }

    #[test]
    fn dormant_policy_switches_require_independent_exact_opt_in() {
        assert!(!feature_flag_is_enabled(None));
        assert!(!feature_flag_is_enabled(Some("")));
        assert!(!feature_flag_is_enabled(Some("true")));
        assert!(!feature_flag_is_enabled(Some(" 1 ")));
        assert!(feature_flag_is_enabled(Some("1")));
    }
}
