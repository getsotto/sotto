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
const CLOUD_OPERATIONS_METRICS_TOKEN_ENV: &str = "SOTTO_CLOUD_OPERATIONS_METRICS_TOKEN";
const CLOUD_COVERAGE_REPAIR_TOKEN_ENV: &str = "SOTTO_CLOUD_COVERAGE_REPAIR_TOKEN";
const ORGANISATION_DELETION_OPERATOR_TOKEN_ENV: &str = "SOTTO_ORGANISATION_DELETION_OPERATOR_TOKEN";
const PROVIDER_REFRESH_INGEST_ENV: &str = "SOTTO_PROVIDER_REFRESH_INGEST_ENABLED";
const PROVIDER_REFRESH_WORKER_ENV: &str = "SOTTO_PROVIDER_REFRESH_WORKER_ENABLED";
const PROVIDER_REFRESH_RECONCILIATION_ENV: &str = "SOTTO_PROVIDER_REFRESH_RECONCILIATION_ENABLED";
const PROVIDER_AUTHORITY_REVALIDATION_ENV: &str = "SOTTO_PROVIDER_AUTHORITY_REVALIDATION_ENABLED";
const CLOUD_COVERAGE_AUTHORITY_ENFORCEMENT_ENV: &str = "SOTTO_CLOUD_COVERAGE_AUTHORITY_ENFORCEMENT";
const CLOUD_COVERAGE_AUTHORITY_SHADOW_ENV: &str = "SOTTO_CLOUD_COVERAGE_AUTHORITY_SHADOW";
const CLOUD_COVERAGE_AUTHORITY_MAX_AGE_ENV: &str = "SOTTO_CLOUD_COVERAGE_AUTHORITY_MAX_AGE_SECONDS";
const CLOUD_ACTION_ENFORCEMENT_ENV: &str = "SOTTO_CLOUD_ACTION_ENFORCEMENT";
const MACHINE_ELIGIBILITY_ENFORCEMENT_ENV: &str = "SOTTO_MACHINE_ELIGIBILITY_ENFORCEMENT";
const CLOUD_SALES_ENV: &str = "SOTTO_CLOUD_SALES_ENABLED";
const FOUNDING_SCHEDULE_WORKER_ENV: &str = "SOTTO_FOUNDING_SCHEDULE_WORKER_ENABLED";
const FOUNDING_PRICE_NOTICES_ENV: &str = "SOTTO_FOUNDING_PRICE_NOTICES_ENABLED";
const FOUNDING_SCHEDULE_PREFLIGHT_ENV: &str = "SOTTO_FOUNDING_SCHEDULE_PREFLIGHT_VERIFIED";
const BILLING_TRANSFERS_ENV: &str = "SOTTO_BILLING_TRANSFERS_ENABLED";
const BILLING_TRANSFER_EXECUTION_ENV: &str = "SOTTO_BILLING_TRANSFER_EXECUTION_ENABLED";
const BILLING_TRANSFER_SETUP_ENV: &str = "SOTTO_BILLING_TRANSFER_SETUP_ENABLED";
const BILLING_TRANSFER_OPERATOR_TOKEN_ENV: &str = "SOTTO_BILLING_TRANSFER_OPERATOR_TOKEN";
const CLOUD_RETENTION_DRY_RUN_ENV: &str = "SOTTO_CLOUD_RETENTION_DRY_RUN_ENABLED";
const CLOUD_SHARED_RETENTION_DRY_RUN_ENV: &str = "SOTTO_CLOUD_SHARED_RETENTION_DRY_RUN_ENABLED";
const CLOUD_RETENTION_OPERATOR_TOKEN_ENV: &str = "SOTTO_CLOUD_RETENTION_OPERATOR_TOKEN";
const CLOUD_RETENTION_PURGE_ENV: &str = "SOTTO_CLOUD_RETENTION_PURGE_ENABLED";
const CLOUD_RETENTION_DESTRUCTIVE_TOKEN_ENV: &str = "SOTTO_CLOUD_RETENTION_DESTRUCTIVE_TOKEN";
const CLOUD_RETENTION_EVIDENCE_KEY_ENV: &str = "SOTTO_CLOUD_RETENTION_EVIDENCE_HMAC_KEY";
const CLOUD_RETENTION_BUILD_SHA_ENV: &str = "SOTTO_CLOUD_RETENTION_BUILD_SHA";
const CLOUD_CONTACT_ENROLMENT_ENV: &str = "SOTTO_CLOUD_CONTACT_ENROLMENT_ENABLED";
const CLOUD_CONTACT_EMAIL_SEND_ENV: &str = "SOTTO_CLOUD_CONTACT_EMAIL_SEND_ENABLED";
const CLOUD_CONTACT_EMAIL_FROM_ENV: &str = "SOTTO_CLOUD_CONTACT_EMAIL_FROM";
const CLOUD_NOTICE_DISPATCH_ENV: &str = "SOTTO_CLOUD_NOTICE_DISPATCH_ENABLED";
const BILLING_RECOVERY_WORKER_ENV: &str = "SOTTO_BILLING_RECOVERY_WORKER_ENABLED";
const SPONSORED_BILLING_RECOVERY_WORKER_ENV: &str =
    "SOTTO_SPONSORED_BILLING_RECOVERY_WORKER_ENABLED";
const RESEND_WEBHOOK_SECRET_ENV: &str = "RESEND_WEBHOOK_SECRET";

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
    /// Operator-selected Stripe account identity for provider evidence, when configured.
    pub stripe_account_id: Option<String>,
    /// Anonymous version-ping telemetry (see [`crate::telemetry`] and the README).
    pub telemetry: TelemetryConfig,
    /// Recovery window applied to new organisation-deletion requests.
    pub organisation_deletion_retention_days: i64,
    /// Whether this instance runs the staged organisation-deletion worker.
    pub organisation_deletion_worker_enabled: bool,
    /// Bearer token for the protected organisation-deletion metrics exporter.
    pub organisation_deletion_metrics_token: Option<String>,
    /// Dedicated bearer credential for protected, aggregate Cloud lifecycle diagnostics.
    pub cloud_operations_metrics_token: Option<String>,
    /// Separate write credential for supported provider-backed coverage repair.
    pub cloud_coverage_repair_token: Option<String>,
    /// Bearer token for the protected operator-observation endpoint.
    pub organisation_deletion_operator_token: Option<String>,
    /// Whether verified provider changes may enqueue durable refresh work.
    pub provider_refresh_ingest_enabled: bool,
    /// Whether a runtime worker may claim and process durable refresh work.
    pub provider_refresh_worker_enabled: bool,
    /// Whether the periodic registered-source repair scan is enabled.
    pub provider_refresh_reconciliation_enabled: bool,
    /// Whether the scheduled full-source authority revalidation worker is running.
    pub provider_authority_revalidation_enabled: bool,
    /// Maximum age of a complete Stripe authority collection, when configured.
    pub cloud_coverage_authority_max_age_seconds: Option<i64>,
    /// Whether Cloud eligibility requires a current authority receipt.
    pub cloud_coverage_authority_enforcement_enabled: bool,
    /// Whether stale authority is reflected in shadow decisions without blocking requests.
    pub cloud_coverage_authority_shadow_enabled: bool,
    /// Whether the dormant human hosted action policy rejects ineligible requests.
    pub cloud_action_enforcement_enabled: bool,
    /// Whether hosted machine retrieval and creation apply the accountable beneficiary policy.
    pub machine_eligibility_enforcement_enabled: bool,
    /// Whether new hosted subscriptions may be quoted or purchased. Existing billing management
    /// and verified webhooks stay available when this is off.
    pub cloud_sales_enabled: bool,
    /// Whether verified founding awards may be scheduled to transition to standard Stripe prices.
    pub founding_schedule_worker_enabled: bool,
    /// Whether verified founding-price transitions produce payer notices. Defaults off.
    pub founding_price_notices_enabled: bool,
    /// Whether authenticated payer-transfer API routes are registered. Defaults off until
    /// provider execution and recovery are ready.
    pub billing_transfers_enabled: bool,
    /// Whether the durable payer-transfer worker makes Stripe changes. Defaults off.
    pub billing_transfer_execution_enabled: bool,
    /// Whether beneficiaries may open the Stripe setup flow for a personal transfer. Defaults off.
    pub billing_transfer_setup_enabled: bool,
    /// Enables protected operator review controls for parked payer transfers when configured.
    pub billing_transfer_operator_token: Option<String>,
    /// Whether personal Cloud expiry candidates are reconciled and dry-run. Defaults off.
    pub cloud_retention_dry_run_enabled: bool,
    /// Whether shared organisation Cloud retention candidates are reconciled and dry-run. Defaults off.
    pub cloud_shared_retention_dry_run_enabled: bool,
    /// Dedicated bearer token for protected Cloud retention inspection and non-destructive actions.
    pub cloud_retention_operator_token: Option<String>,
    /// Whether the independently gated, reviewed Cloud retention purge worker starts. Defaults off.
    pub cloud_retention_purge_enabled: bool,
    /// Separate bearer token for destructive retention approval and review-evidence routes.
    pub cloud_retention_destructive_token: Option<String>,
    /// HMAC key used by the protected restore-evidence attestation path.
    pub cloud_retention_evidence_hmac_key: Option<String>,
    /// Exact release commit identity bound into restore evidence before any purge can be approved.
    pub cloud_retention_build_sha: Option<String>,
    /// Contact-enrolment and verification-email settings. Both switches default off.
    pub contact_verification: ContactVerificationConfig,
    /// Whether the separate lifecycle-notice dispatcher starts. Defaults off.
    pub cloud_notice_dispatch_enabled: bool,
    /// Whether the bounded hosted billing recovery worker starts. Defaults off.
    pub billing_recovery_worker_enabled: bool,
    /// Whether bounded sponsored-operation recovery runs. Defaults off independently of personal recovery.
    pub sponsored_billing_recovery_worker_enabled: bool,
    /// Resend signing secret used to authenticate delivery feedback.
    pub resend_webhook_secret: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ContactVerificationConfig {
    pub enrolment_enabled: bool,
    pub email_sends_enabled: bool,
    pub resend_api_key: Option<String>,
    pub from_address: Option<String>,
    pub web_origin: Option<String>,
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
    /// Stripe Customer Portal configuration used by Cloud. It must permit payment-method updates
    /// and period-end cancellation while disabling subscription changes.
    pub cloud_portal_configuration_id: Option<String>,
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
        let founding_schedule_worker_enabled =
            feature_flag_is_enabled(std::env::var(FOUNDING_SCHEDULE_WORKER_ENV).ok().as_deref());
        let founding_price_notices_enabled =
            feature_flag_is_enabled(std::env::var(FOUNDING_PRICE_NOTICES_ENV).ok().as_deref());
        let billing_transfers_enabled =
            feature_flag_is_enabled(std::env::var(BILLING_TRANSFERS_ENV).ok().as_deref());
        let billing_transfer_execution_enabled = feature_flag_is_enabled(
            std::env::var(BILLING_TRANSFER_EXECUTION_ENV)
                .ok()
                .as_deref(),
        );
        let billing_transfer_setup_enabled =
            feature_flag_is_enabled(std::env::var(BILLING_TRANSFER_SETUP_ENV).ok().as_deref());
        let billing_transfer_operator_token = env_nonempty(BILLING_TRANSFER_OPERATOR_TOKEN_ENV);
        let cloud_retention_dry_run_enabled =
            feature_flag_is_enabled(std::env::var(CLOUD_RETENTION_DRY_RUN_ENV).ok().as_deref());
        let cloud_shared_retention_dry_run_enabled = feature_flag_is_enabled(
            std::env::var(CLOUD_SHARED_RETENTION_DRY_RUN_ENV)
                .ok()
                .as_deref(),
        );
        let cloud_retention_operator_token = env_nonempty(CLOUD_RETENTION_OPERATOR_TOKEN_ENV);
        let cloud_retention_purge_enabled =
            feature_flag_is_enabled(std::env::var(CLOUD_RETENTION_PURGE_ENV).ok().as_deref());
        let cloud_retention_destructive_token = env_nonempty(CLOUD_RETENTION_DESTRUCTIVE_TOKEN_ENV);
        let cloud_retention_evidence_hmac_key = env_nonempty(CLOUD_RETENTION_EVIDENCE_KEY_ENV);
        let cloud_retention_build_sha = env_nonempty(CLOUD_RETENTION_BUILD_SHA_ENV);
        let founding_schedule_preflight_verified = feature_flag_is_enabled(
            std::env::var(FOUNDING_SCHEDULE_PREFLIGHT_ENV)
                .ok()
                .as_deref(),
        );
        let stripe_variables_present = [
            "STRIPE_API_KEY",
            "STRIPE_WEBHOOK_SECRET",
            "STRIPE_PRICE_ID",
            "STRIPE_STANDARD_MONTHLY_PRICE_ID",
            "STRIPE_STANDARD_ANNUAL_PRICE_ID",
            "STRIPE_FOUNDING_MONTHLY_PRICE_ID",
            "STRIPE_FOUNDING_ANNUAL_PRICE_ID",
            "STRIPE_CLOUD_PORTAL_CONFIGURATION_ID",
        ]
        .into_iter()
        .any(|name| env_nonempty(name).is_some());
        validate_deployment_billing_boundary(deployment_mode, stripe_variables_present)?;
        let public_base_url =
            env_nonempty("SOTTO_PUBLIC_URL").unwrap_or_else(|| DEFAULT_PUBLIC_URL.to_string());
        let web_origin = env_nonempty("SOTTO_WEB_ORIGIN");
        let contact_enrolment_enabled =
            feature_flag_is_enabled(std::env::var(CLOUD_CONTACT_ENROLMENT_ENV).ok().as_deref());
        let contact_email_sends_enabled =
            feature_flag_is_enabled(std::env::var(CLOUD_CONTACT_EMAIL_SEND_ENV).ok().as_deref());
        let cloud_notice_dispatch_enabled =
            feature_flag_is_enabled(std::env::var(CLOUD_NOTICE_DISPATCH_ENV).ok().as_deref());
        let billing_recovery_worker_enabled =
            feature_flag_is_enabled(std::env::var(BILLING_RECOVERY_WORKER_ENV).ok().as_deref());
        let sponsored_billing_recovery_worker_enabled = feature_flag_is_enabled(
            std::env::var(SPONSORED_BILLING_RECOVERY_WORKER_ENV)
                .ok()
                .as_deref(),
        );
        let resend_webhook_secret = env_nonempty(RESEND_WEBHOOK_SECRET_ENV);
        let contact_verification = ContactVerificationConfig {
            enrolment_enabled: contact_enrolment_enabled,
            email_sends_enabled: contact_email_sends_enabled,
            resend_api_key: env_nonempty("RESEND_API_KEY"),
            from_address: env_nonempty(CLOUD_CONTACT_EMAIL_FROM_ENV),
            web_origin: web_origin.clone(),
        };
        validate_contact_verification_configuration(&contact_verification)?;
        validate_notice_dispatch_configuration(
            deployment_mode,
            cloud_notice_dispatch_enabled,
            &contact_verification,
            resend_webhook_secret.as_deref(),
        )?;

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
                    cloud_portal_configuration_id: env_nonempty(
                        "STRIPE_CLOUD_PORTAL_CONFIGURATION_ID",
                    ),
                    cloud_sales_enabled,
                    return_url: billing_return_url(&public_base_url, web_origin.as_deref()),
                }),
                _ => None,
            }
        } else {
            None
        };
        if billing
            .as_ref()
            .and_then(|value| value.cloud_portal_configuration_id.as_deref())
            .is_some_and(|value| !is_portal_configuration_id(value))
        {
            return Err(Error::Config(
                "STRIPE_CLOUD_PORTAL_CONFIGURATION_ID must be a Stripe billing portal configuration id"
                    .into(),
            ));
        }
        let stripe_account_id = env_nonempty("STRIPE_ACCOUNT_ID");
        validate_founding_schedule_worker_configuration(
            founding_schedule_worker_enabled,
            founding_schedule_preflight_verified,
            billing.as_ref(),
        )?;
        validate_founding_price_notices_configuration(
            founding_price_notices_enabled,
            founding_schedule_worker_enabled,
            deployment_mode,
            billing.as_ref(),
            billing_transfer_operator_token.as_deref(),
        )?;
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
            billing
                .as_ref()
                .and_then(|value| value.cloud_portal_configuration_id.as_deref())
                .is_some(),
        )?;
        validate_billing_transfers_configuration(
            billing_transfers_enabled,
            deployment_mode,
            billing.as_ref(),
        )?;
        validate_billing_recovery_configuration(
            billing_recovery_worker_enabled,
            deployment_mode,
            billing.as_ref(),
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
        let cloud_operations_metrics_token = env_nonempty(CLOUD_OPERATIONS_METRICS_TOKEN_ENV);
        let cloud_coverage_repair_token = env_nonempty(CLOUD_COVERAGE_REPAIR_TOKEN_ENV);
        validate_cloud_operations_tokens(
            cloud_operations_metrics_token.as_deref(),
            cloud_coverage_repair_token.as_deref(),
            [
                organisation_deletion_metrics_token.as_deref(),
                organisation_deletion_operator_token.as_deref(),
                billing_transfer_operator_token.as_deref(),
                cloud_retention_operator_token.as_deref(),
                cloud_retention_destructive_token.as_deref(),
            ],
        )?;
        let provider_refresh_ingest_enabled =
            feature_flag_is_enabled(std::env::var(PROVIDER_REFRESH_INGEST_ENV).ok().as_deref());
        let provider_refresh_worker_enabled =
            feature_flag_is_enabled(std::env::var(PROVIDER_REFRESH_WORKER_ENV).ok().as_deref());
        let provider_refresh_reconciliation_enabled = feature_flag_is_enabled(
            std::env::var(PROVIDER_REFRESH_RECONCILIATION_ENV)
                .ok()
                .as_deref(),
        );
        let provider_authority_revalidation_enabled = feature_flag_is_enabled(
            std::env::var(PROVIDER_AUTHORITY_REVALIDATION_ENV)
                .ok()
                .as_deref(),
        );
        let cloud_coverage_authority_enforcement_enabled = feature_flag_is_enabled(
            std::env::var(CLOUD_COVERAGE_AUTHORITY_ENFORCEMENT_ENV)
                .ok()
                .as_deref(),
        );
        let cloud_coverage_authority_shadow_enabled = feature_flag_is_enabled(
            std::env::var(CLOUD_COVERAGE_AUTHORITY_SHADOW_ENV)
                .ok()
                .as_deref(),
        );
        let cloud_coverage_authority_max_age_seconds = cloud_coverage_authority_max_age_from_env()?;
        validate_provider_refresh_configuration(
            provider_refresh_ingest_enabled,
            provider_refresh_worker_enabled,
            provider_refresh_reconciliation_enabled,
            billing.as_ref(),
            stripe_account_id.as_deref(),
        )?;
        validate_coverage_authority_configuration(
            provider_authority_revalidation_enabled,
            cloud_coverage_authority_enforcement_enabled,
            cloud_coverage_authority_shadow_enabled,
            cloud_coverage_authority_max_age_seconds,
            deployment_mode,
            billing.as_ref(),
            stripe_account_id.as_deref(),
        )?;
        validate_billing_transfer_execution_configuration(
            billing_transfers_enabled,
            billing_transfer_execution_enabled,
            billing_transfer_setup_enabled,
            provider_refresh_ingest_enabled,
            deployment_mode,
            billing.as_ref(),
        )?;
        validate_billing_transfer_operator_configuration(
            billing_transfer_operator_token.as_deref(),
            deployment_mode,
            billing.as_ref(),
        )?;
        validate_sponsored_billing_recovery_configuration(
            sponsored_billing_recovery_worker_enabled,
            deployment_mode,
            billing.as_ref(),
        )?;
        validate_cloud_retention_configuration(
            cloud_retention_dry_run_enabled || cloud_shared_retention_dry_run_enabled,
            cloud_retention_operator_token.as_deref(),
            deployment_mode,
        )?;
        validate_cloud_retention_purge_configuration(
            cloud_retention_purge_enabled,
            cloud_retention_operator_token.as_deref(),
            cloud_retention_destructive_token.as_deref(),
            cloud_retention_evidence_hmac_key.as_deref(),
            cloud_retention_build_sha.as_deref(),
            deployment_mode,
            option_env!("SOTTO_COMPILED_BUILD_SHA"),
        )?;
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
            stripe_account_id,
            telemetry,
            organisation_deletion_retention_days,
            organisation_deletion_worker_enabled,
            organisation_deletion_metrics_token,
            cloud_operations_metrics_token,
            cloud_coverage_repair_token,
            organisation_deletion_operator_token,
            provider_refresh_ingest_enabled,
            provider_refresh_worker_enabled,
            provider_refresh_reconciliation_enabled,
            provider_authority_revalidation_enabled,
            cloud_coverage_authority_max_age_seconds,
            cloud_coverage_authority_enforcement_enabled,
            cloud_coverage_authority_shadow_enabled,
            cloud_action_enforcement_enabled,
            machine_eligibility_enforcement_enabled,
            cloud_sales_enabled,
            founding_schedule_worker_enabled,
            founding_price_notices_enabled,
            billing_transfers_enabled,
            billing_transfer_execution_enabled,
            billing_transfer_setup_enabled,
            billing_transfer_operator_token,
            cloud_retention_dry_run_enabled,
            cloud_shared_retention_dry_run_enabled,
            cloud_retention_operator_token,
            cloud_retention_purge_enabled,
            cloud_retention_destructive_token,
            cloud_retention_evidence_hmac_key,
            cloud_retention_build_sha,
            contact_verification,
            cloud_notice_dispatch_enabled,
            billing_recovery_worker_enabled,
            sponsored_billing_recovery_worker_enabled,
            resend_webhook_secret,
        })
    }
}

fn validate_cloud_operations_tokens<'a>(
    metrics: Option<&'a str>,
    repair: Option<&'a str>,
    existing_write_tokens: [Option<&'a str>; 5],
) -> Result<()> {
    if metrics.is_some() && metrics == repair {
        return Err(Error::Config(
            "Cloud operations metrics and coverage repair tokens must be different".into(),
        ));
    }
    for new_token in [metrics, repair].into_iter().flatten() {
        if new_token.len() < 32 {
            return Err(Error::Config(
                "Cloud operations metrics and coverage repair tokens must contain at least 32 characters".into(),
            ));
        }
        if existing_write_tokens
            .into_iter()
            .flatten()
            .any(|existing| existing == new_token)
        {
            return Err(Error::Config(
                "Cloud operations tokens must not be reused for an existing operator capability"
                    .into(),
            ));
        }
    }
    Ok(())
}

fn validate_founding_schedule_worker_configuration(
    enabled: bool,
    preflight_verified: bool,
    billing: Option<&BillingConfig>,
) -> Result<()> {
    if !enabled {
        return Ok(());
    }
    if !preflight_verified {
        return Err(Error::Config(
            "founding schedule worker requires SOTTO_FOUNDING_SCHEDULE_PREFLIGHT_VERIFIED=1 after Stripe test-mode schedule and interval-policy verification".into(),
        ));
    }
    let billing = billing
        .ok_or_else(|| Error::Config("founding schedule worker requires Stripe billing".into()))?;
    if billing.price_catalogue.is_none() {
        return Err(Error::Config(
            "founding schedule worker requires all four hosted Stripe prices".into(),
        ));
    }
    if billing.cloud_portal_configuration_id.is_none() {
        return Err(Error::Config(
            "founding schedule worker requires STRIPE_CLOUD_PORTAL_CONFIGURATION_ID".into(),
        ));
    }
    Ok(())
}

fn validate_sponsored_billing_recovery_configuration(
    enabled: bool,
    deployment_mode: DeploymentMode,
    billing: Option<&BillingConfig>,
) -> Result<()> {
    if !enabled {
        return Ok(());
    }
    if deployment_mode != DeploymentMode::Cloud
        || billing
            .and_then(|config| config.price_catalogue.as_ref())
            .is_none()
    {
        return Err(Error::Config(
            "sponsored billing recovery requires Cloud mode and the hosted Stripe price catalogue"
                .into(),
        ));
    }
    Ok(())
}

fn validate_contact_verification_configuration(config: &ContactVerificationConfig) -> Result<()> {
    if config.enrolment_enabled && !config.email_sends_enabled {
        return Err(Error::Config(
            "SOTTO_CLOUD_CONTACT_ENROLMENT_ENABLED and SOTTO_CLOUD_CONTACT_EMAIL_SEND_ENABLED must be enabled together".into(),
        ));
    }
    if !config.email_sends_enabled {
        return Ok(());
    }
    let key = config.resend_api_key.as_deref().unwrap_or_default();
    if !key.starts_with("re_") || key.len() < 8 || key.len() > 256 {
        return Err(Error::Config(
            "RESEND_API_KEY is required when contact email sends are enabled".into(),
        ));
    }
    if !config.enrolment_enabled {
        return Err(Error::Config(
            "SOTTO_CLOUD_CONTACT_EMAIL_SEND_ENABLED requires SOTTO_CLOUD_CONTACT_ENROLMENT_ENABLED"
                .into(),
        ));
    }
    let from = config.from_address.as_deref().unwrap_or_default();
    let valid_from = from.len() <= 320
        && from.is_ascii()
        && from.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty()
                && !domain.is_empty()
                && !domain.starts_with('.')
                && !domain.ends_with('.')
                && domain.contains('.')
                && !from.chars().any(char::is_whitespace)
        });
    if !valid_from {
        return Err(Error::Config(
            "SOTTO_CLOUD_CONTACT_EMAIL_FROM must be a valid email address when sends are enabled"
                .into(),
        ));
    }
    let web_origin = config.web_origin.as_deref().unwrap_or_default();
    let valid_origin = url::Url::parse(web_origin).is_ok_and(|origin| {
        (origin.scheme() == "https"
            || (origin.scheme() == "http"
                && matches!(origin.host_str(), Some("localhost" | "127.0.0.1" | "::1"))))
            && origin.host_str().is_some()
            && origin.path() == "/"
            && origin.query().is_none()
            && origin.fragment().is_none()
            && origin.username().is_empty()
            && origin.password().is_none()
    });
    if !valid_origin {
        return Err(Error::Config(
            "SOTTO_WEB_ORIGIN must be an HTTPS origin when contact email sends are enabled".into(),
        ));
    }
    Ok(())
}

fn validate_notice_dispatch_configuration(
    deployment_mode: DeploymentMode,
    enabled: bool,
    contact: &ContactVerificationConfig,
    webhook_secret: Option<&str>,
) -> Result<()> {
    if !enabled {
        return Ok(());
    }
    if deployment_mode != DeploymentMode::Cloud {
        return Err(Error::Config(
            "Cloud notice dispatch is available only in cloud deployment mode".into(),
        ));
    }
    let key = contact.resend_api_key.as_deref().unwrap_or_default();
    if !key.starts_with("re_") || key.len() < 8 || key.len() > 256 {
        return Err(Error::Config(
            "RESEND_API_KEY is required when Cloud notice dispatch is enabled".into(),
        ));
    }
    let from = contact.from_address.as_deref().unwrap_or_default();
    let valid_from = from.len() <= 320
        && from.is_ascii()
        && from.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty()
                && !domain.is_empty()
                && !domain.starts_with('.')
                && !domain.ends_with('.')
                && domain.contains('.')
                && !from.chars().any(char::is_whitespace)
        });
    if !valid_from {
        return Err(Error::Config(
            "SOTTO_CLOUD_CONTACT_EMAIL_FROM is required when Cloud notice dispatch is enabled"
                .into(),
        ));
    }
    let web_origin = contact.web_origin.as_deref().unwrap_or_default();
    let valid_origin = url::Url::parse(web_origin).is_ok_and(|origin| {
        (origin.scheme() == "https"
            || (origin.scheme() == "http"
                && matches!(origin.host_str(), Some("localhost" | "127.0.0.1" | "::1"))))
            && origin.host_str().is_some()
            && origin.path() == "/"
            && origin.query().is_none()
            && origin.fragment().is_none()
            && origin.username().is_empty()
            && origin.password().is_none()
    });
    if !valid_origin {
        return Err(Error::Config(
            "SOTTO_WEB_ORIGIN is required and must be an HTTPS origin when Cloud notice dispatch is enabled"
                .into(),
        ));
    }
    if !webhook_secret
        .is_some_and(|secret| secret.starts_with("whsec_") && (16..=256).contains(&secret.len()))
    {
        return Err(Error::Config(
            "RESEND_WEBHOOK_SECRET is required when Cloud notice dispatch is enabled".into(),
        ));
    }
    Ok(())
}

fn validate_founding_price_notices_configuration(
    enabled: bool,
    schedule_worker_enabled: bool,
    deployment_mode: DeploymentMode,
    billing: Option<&BillingConfig>,
    operator_token: Option<&str>,
) -> Result<()> {
    if !enabled {
        return Ok(());
    }
    if deployment_mode != DeploymentMode::Cloud || !schedule_worker_enabled {
        return Err(Error::Config(
            "founding price notices require Cloud mode and the founding schedule worker".into(),
        ));
    }
    if billing
        .and_then(|billing| billing.price_catalogue.as_ref())
        .is_none()
    {
        return Err(Error::Config(
            "founding price notices require the hosted Stripe price catalogue".into(),
        ));
    }
    if operator_token.is_none() {
        return Err(Error::Config(
            "founding price notices require SOTTO_BILLING_TRANSFER_OPERATOR_TOKEN for the review summary".into(),
        ));
    }
    Ok(())
}

fn validate_provider_refresh_configuration(
    ingest_enabled: bool,
    worker_enabled: bool,
    reconciliation_enabled: bool,
    billing: Option<&BillingConfig>,
    stripe_account_id: Option<&str>,
) -> Result<()> {
    if !ingest_enabled && !worker_enabled && !reconciliation_enabled {
        return Ok(());
    }
    let billing =
        billing.ok_or_else(|| Error::Config("provider refresh requires Stripe billing".into()))?;
    if billing.price_catalogue.is_none() {
        return Err(Error::Config(
            "provider refresh requires all four hosted Stripe prices".into(),
        ));
    }
    if stripe_account_id.is_none_or(str::is_empty) {
        return Err(Error::Config(
            "provider refresh requires STRIPE_ACCOUNT_ID".into(),
        ));
    }
    if !["sk_test_", "rk_test_", "sk_live_", "rk_live_"]
        .iter()
        .any(|prefix| billing.api_key.starts_with(prefix))
    {
        return Err(Error::Config(
            "provider refresh requires a Stripe test or live API key".into(),
        ));
    }
    Ok(())
}

const MIN_COVERAGE_AUTHORITY_MAX_AGE_SECONDS: i64 = 60;
const MAX_COVERAGE_AUTHORITY_MAX_AGE_SECONDS: i64 = 400 * 24 * 60 * 60;

fn cloud_coverage_authority_max_age_from_env() -> Result<Option<i64>> {
    let Some(value) = env_nonempty(CLOUD_COVERAGE_AUTHORITY_MAX_AGE_ENV) else {
        return Ok(None);
    };
    let seconds = value.parse::<i64>().map_err(|_| {
        Error::Config(format!(
            "{CLOUD_COVERAGE_AUTHORITY_MAX_AGE_ENV} must be a positive integer"
        ))
    })?;
    if !(MIN_COVERAGE_AUTHORITY_MAX_AGE_SECONDS..=MAX_COVERAGE_AUTHORITY_MAX_AGE_SECONDS)
        .contains(&seconds)
    {
        return Err(Error::Config(format!(
            "{CLOUD_COVERAGE_AUTHORITY_MAX_AGE_ENV} must be between {MIN_COVERAGE_AUTHORITY_MAX_AGE_SECONDS} and {MAX_COVERAGE_AUTHORITY_MAX_AGE_SECONDS} seconds"
        )));
    }
    Ok(Some(seconds))
}

fn validate_coverage_authority_configuration(
    revalidation_enabled: bool,
    enforcement_enabled: bool,
    shadow_enabled: bool,
    max_age_seconds: Option<i64>,
    deployment_mode: DeploymentMode,
    billing: Option<&BillingConfig>,
    stripe_account_id: Option<&str>,
) -> Result<()> {
    if let Some(seconds) = max_age_seconds {
        if !(MIN_COVERAGE_AUTHORITY_MAX_AGE_SECONDS..=MAX_COVERAGE_AUTHORITY_MAX_AGE_SECONDS)
            .contains(&seconds)
        {
            return Err(Error::Config(
                "coverage authority max age is outside the supported range".into(),
            ));
        }
    }
    if !revalidation_enabled && !enforcement_enabled && !shadow_enabled {
        return Ok(());
    }
    if deployment_mode != DeploymentMode::Cloud
        || max_age_seconds.is_none()
        || !revalidation_enabled
    {
        return Err(Error::Config(
            "coverage authority requires Cloud mode, scheduled revalidation, and SOTTO_CLOUD_COVERAGE_AUTHORITY_MAX_AGE_SECONDS"
                .into(),
        ));
    }
    validate_provider_refresh_configuration(true, false, false, billing, stripe_account_id)
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
    safe_portal_configuration_configured: bool,
) -> Result<()> {
    if enabled
        && (deployment_mode != DeploymentMode::Cloud
            || !billing_configured
            || !hosted_catalogue_configured
            || !safe_portal_configuration_configured)
    {
        return Err(Error::Config(format!(
            "{CLOUD_SALES_ENV}=1 requires SOTTO_DEPLOYMENT_MODE=cloud, complete Stripe credentials, all four hosted price ids, and STRIPE_CLOUD_PORTAL_CONFIGURATION_ID"
        )));
    }
    Ok(())
}

fn validate_billing_transfers_configuration(
    enabled: bool,
    deployment_mode: DeploymentMode,
    billing: Option<&BillingConfig>,
) -> Result<()> {
    if !enabled {
        return Ok(());
    }
    let billing = billing.ok_or_else(|| {
        Error::Config("payer transfer routes require configured Cloud Stripe billing".into())
    })?;
    if deployment_mode != DeploymentMode::Cloud || billing.price_catalogue.is_none() {
        return Err(Error::Config(
            "payer transfer routes require cloud mode and all four hosted Stripe prices".into(),
        ));
    }
    Ok(())
}

fn validate_billing_recovery_configuration(
    enabled: bool,
    deployment_mode: DeploymentMode,
    billing: Option<&BillingConfig>,
) -> Result<()> {
    if !enabled {
        return Ok(());
    }
    let billing = billing.ok_or_else(|| {
        Error::Config("billing recovery requires configured Cloud Stripe billing".into())
    })?;
    if deployment_mode != DeploymentMode::Cloud || billing.price_catalogue.is_none() {
        return Err(Error::Config(
            "billing recovery requires cloud mode and all four hosted Stripe prices".into(),
        ));
    }
    Ok(())
}

fn validate_billing_transfer_execution_configuration(
    _api_enabled: bool,
    execution_enabled: bool,
    setup_enabled: bool,
    provider_refresh_ingest_enabled: bool,
    deployment_mode: DeploymentMode,
    billing: Option<&BillingConfig>,
) -> Result<()> {
    if !execution_enabled && !setup_enabled {
        return Ok(());
    }
    if setup_enabled && !execution_enabled {
        return Err(Error::Config(format!(
            "{BILLING_TRANSFER_SETUP_ENV}=1 requires {BILLING_TRANSFER_EXECUTION_ENV}=1"
        )));
    }
    if execution_enabled && !provider_refresh_ingest_enabled {
        return Err(Error::Config(
            "payer transfer execution requires SOTTO_PROVIDER_REFRESH_INGEST_ENABLED=1 so paid invoices settle coverage evidence".into(),
        ));
    }
    let billing = billing.ok_or_else(|| {
        Error::Config("payer transfer execution requires configured Cloud Stripe billing".into())
    })?;
    if deployment_mode != DeploymentMode::Cloud || billing.price_catalogue.is_none() {
        return Err(Error::Config(
            "payer transfer execution requires cloud mode and all four hosted Stripe prices".into(),
        ));
    }
    // API intake can be disabled independently while an already-started provider operation is
    // recovered, so execution deliberately does not depend on it.
    Ok(())
}

fn validate_billing_transfer_operator_configuration(
    token: Option<&str>,
    deployment_mode: DeploymentMode,
    billing: Option<&BillingConfig>,
) -> Result<()> {
    let Some(token) = token else {
        return Ok(());
    };
    if deployment_mode != DeploymentMode::Cloud || billing.is_none() {
        return Err(Error::Config(
            "payer transfer operator controls require Cloud billing".into(),
        ));
    }
    if token.len() < 32 {
        return Err(Error::Config(
            "SOTTO_BILLING_TRANSFER_OPERATOR_TOKEN must contain at least 32 characters".into(),
        ));
    }
    Ok(())
}

fn validate_cloud_retention_configuration(
    dry_run_enabled: bool,
    operator_token: Option<&str>,
    deployment_mode: DeploymentMode,
) -> Result<()> {
    if dry_run_enabled && deployment_mode != DeploymentMode::Cloud {
        return Err(Error::Config(
            "Cloud retention dry runs require SOTTO_DEPLOYMENT_MODE=cloud".into(),
        ));
    }
    if dry_run_enabled && operator_token.is_none() {
        return Err(Error::Config(
            "Cloud retention dry runs require SOTTO_CLOUD_RETENTION_OPERATOR_TOKEN".into(),
        ));
    }
    if let Some(token) = operator_token {
        if deployment_mode != DeploymentMode::Cloud {
            return Err(Error::Config(
                "Cloud retention operator controls require SOTTO_DEPLOYMENT_MODE=cloud".into(),
            ));
        }
        if token.len() < 32 {
            return Err(Error::Config(
                "SOTTO_CLOUD_RETENTION_OPERATOR_TOKEN must contain at least 32 characters".into(),
            ));
        }
    }
    Ok(())
}

fn validate_cloud_retention_purge_configuration(
    purge_enabled: bool,
    operator_token: Option<&str>,
    destructive_token: Option<&str>,
    evidence_hmac_key: Option<&str>,
    build_sha: Option<&str>,
    deployment_mode: DeploymentMode,
    compiled_build_sha: Option<&str>,
) -> Result<()> {
    let any_purge_setting = purge_enabled
        || destructive_token.is_some()
        || evidence_hmac_key.is_some()
        || build_sha.is_some();
    if any_purge_setting && deployment_mode != DeploymentMode::Cloud {
        return Err(Error::Config(
            "Cloud retention purge controls require SOTTO_DEPLOYMENT_MODE=cloud".into(),
        ));
    }
    if let Some(token) = destructive_token {
        if token.len() < 32 {
            return Err(Error::Config(
                "SOTTO_CLOUD_RETENTION_DESTRUCTIVE_TOKEN must contain at least 32 characters"
                    .into(),
            ));
        }
        if operator_token.is_some_and(|operator| operator == token) {
            return Err(Error::Config(
                "Cloud retention destructive and inspection tokens must be different".into(),
            ));
        }
    }
    if let Some(key) = evidence_hmac_key {
        if key.len() < 32 {
            return Err(Error::Config(
                "SOTTO_CLOUD_RETENTION_EVIDENCE_HMAC_KEY must contain at least 32 bytes".into(),
            ));
        }
    }
    if let Some(sha) = build_sha {
        if !matches!(sha.len(), 40 | 64)
            || !sha
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(Error::Config(
                "SOTTO_CLOUD_RETENTION_BUILD_SHA must be a lowercase commit SHA".into(),
            ));
        }
        if compiled_build_sha != Some(sha) {
            return Err(Error::Config(
                "SOTTO_CLOUD_RETENTION_BUILD_SHA must match the commit SHA embedded in this server build".into(),
            ));
        }
    }
    if purge_enabled
        && (operator_token.is_none()
            || destructive_token.is_none()
            || evidence_hmac_key.is_none()
            || build_sha.is_none())
    {
        return Err(Error::Config(
            "Cloud retention purge requires separate operator tokens, restore evidence signing, and the exact build SHA".into(),
        ));
    }
    Ok(())
}

fn is_portal_configuration_id(value: &str) -> bool {
    value.starts_with("bpc_") && value.len() > 4 && value.trim() == value
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
    let ids = [
        standard_monthly.as_deref().expect("checked above"),
        standard_annual.as_deref().expect("checked above"),
        founding_monthly.as_deref().expect("checked above"),
        founding_annual.as_deref().expect("checked above"),
    ];
    if ids
        .iter()
        .enumerate()
        .any(|(index, id)| ids[..index].contains(id))
    {
        return Err(Error::Config(
            "hosted Stripe price ids must be distinct".into(),
        ));
    }
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
        is_portal_configuration_id, organisation_deletion_retention_from_env_result,
        organisation_deletion_worker_is_enabled, parse_organisation_deletion_retention_days,
        telemetry_ping_enabled, validate_billing_recovery_configuration,
        validate_billing_transfer_execution_configuration,
        validate_billing_transfer_operator_configuration, validate_billing_transfers_configuration,
        validate_cloud_operations_tokens, validate_cloud_retention_configuration,
        validate_cloud_retention_purge_configuration, validate_cloud_sales_configuration,
        validate_contact_verification_configuration, validate_coverage_authority_configuration,
        validate_deployment_billing_boundary, validate_founding_price_notices_configuration,
        validate_founding_schedule_worker_configuration, validate_notice_dispatch_configuration,
        validate_provider_refresh_configuration, validate_sponsored_billing_recovery_configuration,
        BillingConfig, BillingPriceIds, ContactVerificationConfig, DeploymentMode,
        DEFAULT_ORGANISATION_DELETION_RETENTION_DAYS, MAX_COVERAGE_AUTHORITY_MAX_AGE_SECONDS,
        MAX_ORGANISATION_DELETION_RETENTION_DAYS, MIN_COVERAGE_AUTHORITY_MAX_AGE_SECONDS,
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
    fn contact_email_is_default_off_and_requires_a_complete_sender_configuration() {
        assert!(
            validate_contact_verification_configuration(&ContactVerificationConfig {
                enrolment_enabled: false,
                email_sends_enabled: false,
                resend_api_key: None,
                from_address: None,
                web_origin: None,
            })
            .is_ok()
        );
        assert!(
            validate_contact_verification_configuration(&ContactVerificationConfig {
                enrolment_enabled: false,
                email_sends_enabled: true,
                resend_api_key: Some("re_12345678".into()),
                from_address: Some("no-reply@example.test".into()),
                web_origin: Some("https://app.example.test".into()),
            })
            .is_err()
        );
        assert!(
            validate_contact_verification_configuration(&ContactVerificationConfig {
                enrolment_enabled: true,
                email_sends_enabled: false,
                resend_api_key: None,
                from_address: None,
                web_origin: None,
            })
            .is_err()
        );
        assert!(
            validate_contact_verification_configuration(&ContactVerificationConfig {
                enrolment_enabled: true,
                email_sends_enabled: true,
                resend_api_key: None,
                from_address: Some("no-reply@example.test".into()),
                web_origin: Some("https://app.example.test".into()),
            })
            .is_err()
        );
        assert!(
            validate_contact_verification_configuration(&ContactVerificationConfig {
                enrolment_enabled: true,
                email_sends_enabled: true,
                resend_api_key: Some("re_12345678".into()),
                from_address: Some("no-reply@example.test".into()),
                web_origin: Some("https://app.example.test".into()),
            })
            .is_ok()
        );
        assert!(
            validate_contact_verification_configuration(&ContactVerificationConfig {
                enrolment_enabled: true,
                email_sends_enabled: true,
                resend_api_key: Some("re_12345678".into()),
                from_address: Some("no-reply@example.test".into()),
                web_origin: Some("https://app.example.test/path".into()),
            })
            .is_err()
        );
    }

    #[test]
    fn notice_dispatch_is_cloud_only_and_requires_authenticated_feedback() {
        let contact = ContactVerificationConfig {
            enrolment_enabled: false,
            email_sends_enabled: false,
            resend_api_key: Some("re_12345678".into()),
            from_address: Some("no-reply@example.test".into()),
            web_origin: None,
        };
        assert!(validate_notice_dispatch_configuration(
            DeploymentMode::SelfHosted,
            false,
            &contact,
            None
        )
        .is_ok());
        assert!(validate_notice_dispatch_configuration(
            DeploymentMode::SelfHosted,
            true,
            &contact,
            Some("whsec_dGVzdHNlY3JldA==")
        )
        .is_err());
        assert!(validate_notice_dispatch_configuration(
            DeploymentMode::Cloud,
            true,
            &ContactVerificationConfig {
                web_origin: Some("https://app.example.test".into()),
                ..contact.clone()
            },
            None
        )
        .is_err());
        assert!(validate_notice_dispatch_configuration(
            DeploymentMode::Cloud,
            true,
            &ContactVerificationConfig {
                web_origin: Some("https://app.example.test".into()),
                ..contact.clone()
            },
            Some("whsec_dGVzdHNlY3JldA==")
        )
        .is_ok());
    }

    #[test]
    fn provider_refresh_controls_are_dark_or_require_complete_stripe_identity() {
        assert!(validate_provider_refresh_configuration(false, false, false, None, None).is_ok());
        assert!(validate_provider_refresh_configuration(false, false, true, None, None).is_err());
        assert!(validate_provider_refresh_configuration(true, false, false, None, None).is_err());

        let incomplete = BillingConfig {
            api_key: "rk_test_secret".into(),
            webhook_secret: "whsec_test".into(),
            price_id: "price_legacy".into(),
            price_catalogue: None,
            cloud_portal_configuration_id: None,
            cloud_sales_enabled: false,
            return_url: "https://app.example.test".into(),
        };
        assert!(validate_provider_refresh_configuration(
            false,
            true,
            false,
            Some(&incomplete),
            Some("acct_test"),
        )
        .is_err());

        let complete = BillingConfig {
            price_catalogue: Some(BillingPriceIds {
                standard_monthly: "price_standard_month".into(),
                standard_annual: "price_standard_year".into(),
                founding_monthly: "price_founding_month".into(),
                founding_annual: "price_founding_year".into(),
            }),
            ..incomplete
        };
        assert!(validate_provider_refresh_configuration(
            true,
            true,
            false,
            Some(&complete),
            Some("acct_test"),
        )
        .is_ok());
        assert!(validate_provider_refresh_configuration(
            false,
            false,
            true,
            Some(&complete),
            Some("acct_test"),
        )
        .is_ok());
        assert!(validate_coverage_authority_configuration(
            true,
            true,
            true,
            Some(35 * 24 * 60 * 60),
            DeploymentMode::Cloud,
            Some(&complete),
            Some("acct_test"),
        )
        .is_ok());
        assert!(
            validate_provider_refresh_configuration(true, true, false, Some(&complete), None,)
                .is_err()
        );
    }

    #[test]
    fn coverage_authority_requires_scheduled_cloud_revalidation_and_bounded_freshness() {
        assert!(validate_coverage_authority_configuration(
            false,
            false,
            false,
            None,
            DeploymentMode::Cloud,
            None,
            None
        )
        .is_ok());
        assert!(validate_coverage_authority_configuration(
            false,
            true,
            false,
            Some(60),
            DeploymentMode::Cloud,
            None,
            Some("acct_test")
        )
        .is_err());
        assert!(validate_coverage_authority_configuration(
            true,
            false,
            false,
            None,
            DeploymentMode::Cloud,
            None,
            Some("acct_test")
        )
        .is_err());
        assert!(validate_coverage_authority_configuration(
            false,
            false,
            false,
            Some(MIN_COVERAGE_AUTHORITY_MAX_AGE_SECONDS - 1),
            DeploymentMode::Cloud,
            None,
            None,
        )
        .is_err());
        assert!(validate_coverage_authority_configuration(
            false,
            false,
            false,
            Some(MAX_COVERAGE_AUTHORITY_MAX_AGE_SECONDS + 1),
            DeploymentMode::Cloud,
            None,
            None,
        )
        .is_err());
    }

    #[test]
    fn payer_transfer_routes_require_cloud_billing_and_complete_catalogue() {
        assert!(
            validate_billing_transfers_configuration(false, DeploymentMode::SelfHosted, None)
                .is_ok()
        );
        assert!(
            validate_billing_transfers_configuration(true, DeploymentMode::SelfHosted, None)
                .is_err()
        );
        let billing = BillingConfig {
            api_key: "rk_test_secret".into(),
            webhook_secret: "whsec_test".into(),
            price_id: "price_legacy".into(),
            price_catalogue: Some(BillingPriceIds {
                standard_monthly: "price_standard_month".into(),
                standard_annual: "price_standard_year".into(),
                founding_monthly: "price_founding_month".into(),
                founding_annual: "price_founding_year".into(),
            }),
            cloud_portal_configuration_id: None,
            cloud_sales_enabled: false,
            return_url: "https://app.example.test".into(),
        };
        assert!(validate_billing_transfers_configuration(
            true,
            DeploymentMode::Cloud,
            Some(&billing)
        )
        .is_ok());
    }

    #[test]
    fn billing_recovery_is_independent_but_requires_hosted_catalogue_when_enabled() {
        assert!(
            validate_billing_recovery_configuration(false, DeploymentMode::SelfHosted, None,)
                .is_ok()
        );
        assert!(
            validate_billing_recovery_configuration(true, DeploymentMode::SelfHosted, None,)
                .is_err()
        );
        let billing = BillingConfig {
            api_key: "rk_test_secret".into(),
            webhook_secret: "whsec_test".into(),
            price_id: "price_legacy".into(),
            price_catalogue: Some(BillingPriceIds {
                standard_monthly: "price_standard_month".into(),
                standard_annual: "price_standard_year".into(),
                founding_monthly: "price_founding_month".into(),
                founding_annual: "price_founding_year".into(),
            }),
            cloud_portal_configuration_id: None,
            cloud_sales_enabled: false,
            return_url: "https://app.example.test".into(),
        };
        assert!(validate_billing_recovery_configuration(
            true,
            DeploymentMode::Cloud,
            Some(&billing),
        )
        .is_ok());
    }

    #[test]
    fn payer_transfer_execution_is_independent_and_off_by_default() {
        assert!(validate_billing_transfer_execution_configuration(
            false,
            false,
            false,
            false,
            DeploymentMode::SelfHosted,
            None,
        )
        .is_ok());
        assert!(validate_billing_transfer_execution_configuration(
            false,
            true,
            false,
            false,
            DeploymentMode::Cloud,
            None,
        )
        .is_err());
        assert!(validate_billing_transfer_execution_configuration(
            false,
            false,
            true,
            false,
            DeploymentMode::Cloud,
            None,
        )
        .is_err());
        assert!(validate_billing_transfer_execution_configuration(
            false,
            true,
            true,
            false,
            DeploymentMode::Cloud,
            None,
        )
        .is_err());
        assert!(validate_billing_transfer_execution_configuration(
            false,
            true,
            false,
            true,
            DeploymentMode::Cloud,
            Some(&BillingConfig {
                api_key: "rk_test_secret".into(),
                webhook_secret: "whsec_test".into(),
                price_id: "price_legacy".into(),
                price_catalogue: Some(BillingPriceIds {
                    standard_monthly: "price_standard_month".into(),
                    standard_annual: "price_standard_year".into(),
                    founding_monthly: "price_founding_month".into(),
                    founding_annual: "price_founding_year".into(),
                }),
                cloud_portal_configuration_id: None,
                cloud_sales_enabled: false,
                return_url: "https://app.example.test".into(),
            }),
        )
        .is_ok());
    }

    #[test]
    fn payer_transfer_operator_route_requires_a_long_cloud_only_token() {
        assert!(validate_billing_transfer_operator_configuration(
            None,
            DeploymentMode::SelfHosted,
            None,
        )
        .is_ok());
        assert!(validate_billing_transfer_operator_configuration(
            Some("too-short"),
            DeploymentMode::Cloud,
            None,
        )
        .is_err());
        assert!(validate_billing_transfer_operator_configuration(
            Some("0123456789abcdef0123456789abcdef"),
            DeploymentMode::SelfHosted,
            None,
        )
        .is_err());
        let billing = BillingConfig {
            api_key: "rk_test_secret".into(),
            webhook_secret: "whsec_test".into(),
            price_id: "price_legacy".into(),
            price_catalogue: None,
            cloud_portal_configuration_id: None,
            cloud_sales_enabled: false,
            return_url: "https://app.example.test".into(),
        };
        assert!(validate_billing_transfer_operator_configuration(
            Some("0123456789abcdef0123456789abcdef"),
            DeploymentMode::Cloud,
            Some(&billing),
        )
        .is_ok());
    }

    #[test]
    fn cloud_retention_is_dark_by_default_and_requires_its_own_cloud_token() {
        assert!(
            validate_cloud_retention_configuration(false, None, DeploymentMode::SelfHosted,)
                .is_ok()
        );
        assert!(
            validate_cloud_retention_configuration(true, None, DeploymentMode::Cloud,).is_err()
        );
        assert!(validate_cloud_retention_configuration(
            true,
            Some("0123456789abcdef0123456789abcdef"),
            DeploymentMode::SelfHosted,
        )
        .is_err());
        assert!(
            validate_cloud_retention_configuration(true, Some("short"), DeploymentMode::Cloud,)
                .is_err()
        );
        assert!(validate_cloud_retention_configuration(
            true,
            Some("0123456789abcdef0123456789abcdef"),
            DeploymentMode::Cloud,
        )
        .is_ok());
    }

    #[test]
    fn cloud_retention_purge_requires_separate_credentials_and_exact_build_identity() {
        let inspect = "i".repeat(32);
        let destructive = "d".repeat(32);
        let signing_key = "s".repeat(32);
        let sha = "a".repeat(40);
        assert!(validate_cloud_retention_purge_configuration(
            false,
            None,
            None,
            None,
            None,
            DeploymentMode::SelfHosted,
            None,
        )
        .is_ok());
        assert!(validate_cloud_retention_purge_configuration(
            true,
            Some(&inspect),
            Some(&destructive),
            Some(&signing_key),
            Some(&sha),
            DeploymentMode::Cloud,
            Some(&sha),
        )
        .is_ok());
        assert!(validate_cloud_retention_purge_configuration(
            true,
            Some(&inspect),
            Some(&inspect),
            Some(&signing_key),
            Some(&sha),
            DeploymentMode::Cloud,
            Some(&sha),
        )
        .is_err());
        assert!(validate_cloud_retention_purge_configuration(
            true,
            Some(&inspect),
            Some(&destructive),
            Some(&signing_key),
            Some(&sha),
            DeploymentMode::Cloud,
            Some(&"b".repeat(40)),
        )
        .is_err());
        assert!(validate_cloud_retention_purge_configuration(
            true,
            Some(&inspect),
            Some(&destructive),
            Some(&signing_key),
            Some(&sha),
            DeploymentMode::Cloud,
            None,
        )
        .is_err());
        assert!(validate_cloud_retention_purge_configuration(
            true,
            Some(&inspect),
            Some(&destructive),
            None,
            Some(&sha),
            DeploymentMode::Cloud,
            Some(&sha),
        )
        .is_err());
        assert!(validate_cloud_retention_purge_configuration(
            true,
            Some(&inspect),
            Some(&destructive),
            Some(&signing_key),
            Some("not-a-commit"),
            DeploymentMode::Cloud,
            Some(&sha),
        )
        .is_err());
    }

    #[test]
    fn build_sha_is_embedded_from_the_build_argument() {
        let expected = std::env::var("SOTTO_BUILD_SHA")
            .ok()
            .filter(|value| !value.is_empty());
        assert_eq!(option_env!("SOTTO_COMPILED_BUILD_SHA"), expected.as_deref());
    }

    #[test]
    fn cloud_sales_requires_cloud_billing_and_all_hosted_prices() {
        assert!(validate_cloud_sales_configuration(
            false,
            DeploymentMode::SelfHosted,
            false,
            false,
            false,
        )
        .is_ok());
        assert!(validate_cloud_sales_configuration(
            true,
            DeploymentMode::SelfHosted,
            true,
            true,
            true
        )
        .is_err());
        assert!(
            validate_cloud_sales_configuration(true, DeploymentMode::Cloud, false, true, true)
                .is_err()
        );
        assert!(
            validate_cloud_sales_configuration(true, DeploymentMode::Cloud, true, false, true)
                .is_err()
        );
        assert!(
            validate_cloud_sales_configuration(true, DeploymentMode::Cloud, true, true, false)
                .is_err()
        );
        assert!(
            validate_cloud_sales_configuration(true, DeploymentMode::Cloud, true, true, true)
                .is_ok()
        );
        assert!(is_portal_configuration_id("bpc_123"));
        assert!(!is_portal_configuration_id("cus_123"));
        assert!(!is_portal_configuration_id("bpc_"));
        assert!(!is_portal_configuration_id(" bpc_123"));
    }

    #[test]
    fn founding_schedule_worker_is_opt_in_and_requires_the_hosted_catalogue() {
        assert!(validate_founding_schedule_worker_configuration(false, false, None).is_ok());
        assert!(validate_founding_schedule_worker_configuration(true, false, None).is_err());
        let billing = BillingConfig {
            api_key: "rk_test_secret".into(),
            webhook_secret: "whsec_test".into(),
            price_id: "price_legacy".into(),
            price_catalogue: None,
            cloud_portal_configuration_id: None,
            cloud_sales_enabled: false,
            return_url: "https://app.example.test".into(),
        };
        assert!(
            validate_founding_schedule_worker_configuration(true, true, Some(&billing)).is_err()
        );
        let catalogue_only = BillingConfig {
            price_catalogue: Some(BillingPriceIds {
                standard_monthly: "price_standard_month".into(),
                standard_annual: "price_standard_year".into(),
                founding_monthly: "price_founding_month".into(),
                founding_annual: "price_founding_year".into(),
            }),
            ..billing.clone()
        };
        assert!(
            validate_founding_schedule_worker_configuration(true, true, Some(&catalogue_only))
                .is_err()
        );
        let complete = BillingConfig {
            cloud_portal_configuration_id: Some("bpc_test".into()),
            ..catalogue_only
        };
        assert!(
            validate_founding_schedule_worker_configuration(true, false, Some(&complete)).is_err()
        );
        assert!(
            validate_founding_schedule_worker_configuration(true, true, Some(&complete)).is_ok()
        );
    }

    #[test]
    fn sponsored_billing_recovery_is_opt_in_and_requires_cloud_catalogue() {
        assert!(validate_sponsored_billing_recovery_configuration(
            false,
            DeploymentMode::SelfHosted,
            None
        )
        .is_ok());
        assert!(validate_sponsored_billing_recovery_configuration(
            true,
            DeploymentMode::SelfHosted,
            None
        )
        .is_err());
        let billing = BillingConfig {
            api_key: "rk_test_secret".into(),
            webhook_secret: "whsec_test".into(),
            price_id: "price_legacy".into(),
            price_catalogue: Some(BillingPriceIds {
                standard_monthly: "price_standard_month".into(),
                standard_annual: "price_standard_year".into(),
                founding_monthly: "price_founding_month".into(),
                founding_annual: "price_founding_year".into(),
            }),
            cloud_portal_configuration_id: None,
            cloud_sales_enabled: false,
            return_url: "https://app.example.test".into(),
        };
        assert!(validate_sponsored_billing_recovery_configuration(
            true,
            DeploymentMode::Cloud,
            Some(&billing)
        )
        .is_ok());
        let mut no_catalogue = billing;
        no_catalogue.price_catalogue = None;
        assert!(validate_sponsored_billing_recovery_configuration(
            true,
            DeploymentMode::Cloud,
            Some(&no_catalogue)
        )
        .is_err());
    }

    #[test]
    fn founding_price_notices_are_opt_in_and_require_verified_schedule_operations() {
        assert!(validate_founding_price_notices_configuration(
            false,
            false,
            DeploymentMode::SelfHosted,
            None,
            None
        )
        .is_ok());
        assert!(validate_founding_price_notices_configuration(
            true,
            false,
            DeploymentMode::Cloud,
            None,
            Some("operator")
        )
        .is_err());
        let billing = BillingConfig {
            api_key: "rk_test_secret".into(),
            webhook_secret: "whsec_test".into(),
            price_id: "price_legacy".into(),
            price_catalogue: Some(BillingPriceIds {
                standard_monthly: "price_standard_month".into(),
                standard_annual: "price_standard_year".into(),
                founding_monthly: "price_founding_month".into(),
                founding_annual: "price_founding_year".into(),
            }),
            cloud_portal_configuration_id: Some("bpc_test".into()),
            cloud_sales_enabled: false,
            return_url: "https://app.example.test".into(),
        };
        assert!(validate_founding_price_notices_configuration(
            true,
            true,
            DeploymentMode::Cloud,
            Some(&billing),
            None
        )
        .is_err());
        assert!(validate_founding_price_notices_configuration(
            true,
            true,
            DeploymentMode::Cloud,
            Some(&billing),
            Some("operator")
        )
        .is_ok());
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

    #[test]
    fn cloud_diagnostic_credentials_cannot_share_existing_operator_tokens() {
        let metrics = "m".repeat(32);
        let repair = "r".repeat(32);
        let shared = "s".repeat(32);
        assert!(validate_cloud_operations_tokens(Some(&metrics), Some(&repair), [None; 5]).is_ok());
        assert!(
            validate_cloud_operations_tokens(Some(&metrics), Some(&metrics), [None; 5]).is_err()
        );
        assert!(validate_cloud_operations_tokens(Some("short"), None, [None; 5]).is_err());
        assert!(validate_cloud_operations_tokens(
            Some(&shared),
            None,
            [None, Some(&shared), None, None, None]
        )
        .is_err());
        assert!(validate_cloud_operations_tokens(
            None,
            Some(&shared),
            [None, None, None, Some(&shared), None]
        )
        .is_err());
    }
}
