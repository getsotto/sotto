//! The Sotto sync / API backend.
//!
//! Boots the server: connect to Postgres, apply migrations, and serve the zero-knowledge API
//! (health, GitHub OAuth login/sessions, account + secret sync). The router itself lives in
//! [`sotto_server::app`] so the binary and the end-to-end tests share one wiring.

use std::sync::Arc;

#[cfg(not(feature = "e2e-mock-oauth"))]
use sotto_server::auth::GithubOAuth;
use sotto_server::auth::OAuthProvider;
use sotto_server::billing::BillingState;
#[cfg(not(feature = "e2e-mock-billing"))]
use sotto_server::billing_catalogue::{BillingOffer, BillingPriceCatalogue};
#[cfg(not(feature = "e2e-mock-billing"))]
use sotto_server::cloud_provider::ProviderEnvironment;
#[cfg(not(feature = "e2e-mock-billing"))]
use sotto_server::cloud_provider_stripe::StripeCoverageConfig;
#[cfg(not(feature = "e2e-mock-billing"))]
use sotto_server::cloud_provider_stripe_http::StripeReadClient;
#[cfg(not(feature = "e2e-mock-billing"))]
use sotto_server::cloud_provider_stripe_http::StripeReadLimits;
#[cfg(not(feature = "e2e-mock-billing"))]
use sotto_server::cloud_provider_stripe_sponsored::SponsoredStripeCoverageConfig;
use sotto_server::config::Config;
use sotto_server::contact_email::ResendSender;
use sotto_server::contact_verification::ContactVerificationService;
use sotto_server::db;
use sotto_server::error::{Error, Result};
use sotto_server::state::AppState;

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let config = Config::from_env()?;
    let pool = db::connect(&config.database_url).await?;
    db::migrate(&pool).await?;

    let oauth: Option<Arc<dyn OAuthProvider>> = config.oauth.as_ref().map(|o| {
        #[cfg(feature = "e2e-mock-oauth")]
        {
            eprintln!(
                "warning: built with `e2e-mock-oauth` - GitHub logins are FAKE (any code \
                 authenticates as its own literal value). Never deploy this build."
            );
            let _ = o; // the mock needs no credentials
            Arc::new(sotto_server::auth::MockOAuth) as Arc<dyn OAuthProvider>
        }
        #[cfg(not(feature = "e2e-mock-oauth"))]
        {
            Arc::new(GithubOAuth::new(
                o.github_client_id.clone(),
                o.github_client_secret.clone(),
                o.callback_url(),
            )) as Arc<dyn OAuthProvider>
        }
    });
    if oauth.is_none() {
        eprintln!("warning: GITHUB_CLIENT_ID/SECRET unset - OAuth endpoints will return 503");
    }

    #[allow(unused_mut)]
    let mut personal_coverage_runtime = None;
    let billing = if let Some(billing) = config.billing.clone() {
        Some({
            #[cfg(feature = "e2e-mock-billing")]
            {
                eprintln!(
                "warning: built with `e2e-mock-billing` - checkout pages are local test fixtures. \
                 Never deploy this build."
            );
                // The browser leaves the Vite preview for this provider page, so use the API origin
                // already used by the E2E OAuth adapter. The fallback only covers a feature-only local
                // server run where no public OAuth origin has been configured.
                let provider_origin = config
                    .oauth
                    .as_ref()
                    .map(|oauth| oauth.public_base_url.clone())
                    .unwrap_or_else(|| "http://127.0.0.1:8080".into());
                BillingState::with_e2e_provider(billing, provider_origin)
            }
            #[cfg(not(feature = "e2e-mock-billing"))]
            {
                let mut state = BillingState::from_config(billing.clone())
                    .with_founding_schedule_worker_enabled(config.founding_schedule_worker_enabled);
                if config.provider_refresh_ingest_enabled
                    || config.provider_refresh_worker_enabled
                    || config.provider_refresh_reconciliation_enabled
                    || config.provider_authority_revalidation_enabled
                {
                    let account_id = config.stripe_account_id.as_deref().ok_or_else(|| {
                        Error::Config("provider refresh requires STRIPE_ACCOUNT_ID".into())
                    })?;
                    let environment = stripe_environment(&billing.api_key).ok_or_else(|| {
                        Error::Config("provider refresh requires a Stripe test or live key".into())
                    })?;
                    let ids = billing.price_catalogue.as_ref().ok_or_else(|| {
                        Error::Config(
                            "provider refresh requires all four hosted Stripe prices".into(),
                        )
                    })?;
                    let coverage = StripeCoverageConfig::new(
                        account_id,
                        environment,
                        ids.standard_monthly.clone(),
                        ids.standard_annual.clone(),
                    )
                    .and_then(|coverage| {
                        coverage.with_founding_prices(
                            ids.founding_monthly.clone(),
                            ids.founding_annual.clone(),
                        )
                    })
                    .map_err(|_| {
                        Error::Config(
                            "configured Stripe prices do not form a valid coverage identity".into(),
                        )
                    })?;
                    let client = StripeReadClient::new(
                        billing.api_key.clone(),
                        &coverage,
                        StripeReadLimits::default(),
                    )
                    .map_err(|_| {
                        Error::Config("configured provider refresh limits are invalid".into())
                    })?;
                    let sponsored_coverage = SponsoredStripeCoverageConfig::new(
                        account_id,
                        environment,
                        ids.standard_monthly.clone(),
                        ids.standard_annual.clone(),
                        ids.founding_monthly.clone(),
                        ids.founding_annual.clone(),
                    )
                    .map_err(|_| {
                        Error::Config(
                            "configured sponsored Stripe prices do not form a valid coverage identity".into(),
                        )
                    })?;
                    personal_coverage_runtime =
                        Some((coverage.clone(), sponsored_coverage, client.clone()));
                    state = state.with_personal_coverage(
                        coverage,
                        client,
                        config.provider_refresh_ingest_enabled,
                    );
                }
                state
            }
        })
    } else {
        None
    };
    #[cfg(not(feature = "e2e-mock-billing"))]
    if let Some((coverage, _, stripe)) = &personal_coverage_runtime {
        if let Some(ids) = config
            .billing
            .as_ref()
            .and_then(|billing| billing.price_catalogue.as_ref())
        {
            verify_provider_refresh_configuration(stripe, coverage, ids).await?;
        }
    }
    // AppState owns billing; keep only the provider handle for the optional worker.
    let worker_provider = billing.as_ref().map(BillingState::provider);

    let contact_verification = if config.contact_verification.enrolment_enabled
        && config.contact_verification.email_sends_enabled
    {
        let sender = ResendSender::new(
            config
                .contact_verification
                .resend_api_key
                .clone()
                .expect("contact email configuration validated at startup"),
            config
                .contact_verification
                .from_address
                .clone()
                .expect("contact email configuration validated at startup"),
            config
                .contact_verification
                .web_origin
                .clone()
                .expect("contact email configuration validated at startup"),
        )?;
        let origin = config
            .contact_verification
            .web_origin
            .as_deref()
            .expect("contact email configuration validated at startup");
        Some(Arc::new(ContactVerificationService::new(
            config.contact_verification.enrolment_enabled,
            true,
            origin,
            Arc::new(sender),
        )?))
    } else {
        None
    };
    let notice_sender = if config.cloud_notice_dispatch_enabled {
        Some(ResendSender::new(
            config
                .contact_verification
                .resend_api_key
                .clone()
                .expect("notice dispatch configuration validated at startup"),
            config
                .contact_verification
                .from_address
                .clone()
                .expect("notice dispatch configuration validated at startup"),
            config
                .contact_verification
                .web_origin
                .clone()
                .expect("notice dispatch configuration validated at startup"),
        )?)
    } else {
        None
    };

    let repair_billing = billing.clone();
    let repair_runtime = personal_coverage_runtime.clone();
    let billing_transfer_operator_review_enabled = config.billing_transfer_operator_token.is_some();
    let state = AppState {
        pool: pool.clone(),
        deployment_mode: config.deployment_mode,
        oauth,
        oauth_config: config.oauth.clone(),
        billing,
        contact_verification,
        cloud_notice_dispatch_enabled: config.cloud_notice_dispatch_enabled,
        resend_webhook_secret: config
            .cloud_notice_dispatch_enabled
            .then(|| config.resend_webhook_secret.clone())
            .flatten(),
        telemetry_ingest: config.telemetry.ingest_enabled,
        // One opt-in drives both halves: the routes that accept work and the worker that performs
        // it. Splitting them would let a deployment freeze organisations it can never purge.
        organisation_deletion_enabled: config.organisation_deletion_worker_enabled,
        organisation_deletion_retention_days: config.organisation_deletion_retention_days,
        organisation_deletion_metrics_token: config.organisation_deletion_metrics_token,
        cloud_operations_metrics_token: config.cloud_operations_metrics_token,
        cloud_coverage_repair_token: config.cloud_coverage_repair_token,
        provider_refresh_ingest_enabled: config.provider_refresh_ingest_enabled,
        provider_refresh_worker_enabled: config.provider_refresh_worker_enabled,
        provider_refresh_reconciliation_enabled: config.provider_refresh_reconciliation_enabled,
        provider_authority_revalidation_enabled: config.provider_authority_revalidation_enabled,
        cloud_sales_enabled: config.cloud_sales_enabled,
        founding_schedule_worker_enabled: config.founding_schedule_worker_enabled,
        founding_price_notices_enabled: config.founding_price_notices_enabled,
        billing_recovery_worker_enabled: config.billing_recovery_worker_enabled,
        sponsored_billing_recovery_worker_enabled: config.sponsored_billing_recovery_worker_enabled,
        organisation_deletion_operator_token: config.organisation_deletion_operator_token,
        billing_transfer_operator_token: config.billing_transfer_operator_token,
        cloud_action_enforcement_enabled: config.cloud_action_enforcement_enabled,
        cloud_coverage_authority_enforcement_enabled: config
            .cloud_coverage_authority_enforcement_enabled,
        cloud_coverage_authority_shadow_enabled: config.cloud_coverage_authority_shadow_enabled,
        machine_eligibility_enforcement_enabled: config.machine_eligibility_enforcement_enabled,
        billing_transfers_enabled: config.billing_transfers_enabled,
        billing_transfer_execution_enabled: config.billing_transfer_execution_enabled,
        billing_transfer_setup_enabled: config.billing_transfer_setup_enabled,
        cloud_retention_dry_run_enabled: config.cloud_retention_dry_run_enabled,
        cloud_shared_retention_dry_run_enabled: config.cloud_shared_retention_dry_run_enabled,
        cloud_retention_operator_token: config.cloud_retention_operator_token.clone(),
        cloud_retention_purge_enabled: config.cloud_retention_purge_enabled,
        cloud_retention_destructive_token: config.cloud_retention_destructive_token.clone(),
        cloud_retention_evidence_hmac_key: config.cloud_retention_evidence_hmac_key.clone(),
        cloud_retention_build_sha: config.cloud_retention_build_sha.clone(),
    };

    println!("deployment mode: {}", config.deployment_mode.as_str());
    println!("cloud sales: {}", config.cloud_sales_enabled);
    println!(
        "cloud contact enrolment: {}, email sends: {}",
        config.contact_verification.enrolment_enabled,
        config.contact_verification.email_sends_enabled
    );
    println!(
        "Cloud lifecycle notice email dispatch: {}",
        config.cloud_notice_dispatch_enabled
    );
    println!(
        "Cloud billing recovery worker: {}",
        config.billing_recovery_worker_enabled
    );
    println!(
        "Sponsored billing recovery worker: {}",
        config.sponsored_billing_recovery_worker_enabled
    );
    println!(
        "provider refresh: ingest={}, worker={}, reconciliation={}",
        config.provider_refresh_ingest_enabled,
        config.provider_refresh_worker_enabled,
        config.provider_refresh_reconciliation_enabled
    );
    println!(
        "coverage authority: revalidation={}, shadow={}, enforcement={}, max_age_seconds={:?}",
        config.provider_authority_revalidation_enabled,
        config.cloud_coverage_authority_shadow_enabled,
        config.cloud_coverage_authority_enforcement_enabled,
        config.cloud_coverage_authority_max_age_seconds,
    );
    println!(
        "cloud action enforcement: {}",
        config.cloud_action_enforcement_enabled
    );
    println!(
        "machine eligibility enforcement: {}",
        config.machine_eligibility_enforcement_enabled
    );
    println!("billing transfers: {}", config.billing_transfers_enabled);
    println!(
        "billing transfer execution: {}",
        config.billing_transfer_execution_enabled
    );
    println!(
        "billing transfer setup: {}",
        config.billing_transfer_setup_enabled
    );
    println!(
        "billing transfer operator review: {}",
        billing_transfer_operator_review_enabled
    );
    println!(
        "founding schedule worker: {}",
        config.founding_schedule_worker_enabled
    );
    println!(
        "sponsored billing event handling: {}",
        std::env::var("SOTTO_SPONSORED_BILLING_ENABLED").as_deref() == Ok("1")
    );
    println!(
        "billing corrections: {}",
        std::env::var("SOTTO_BILLING_CORRECTIONS_ENABLED").as_deref() == Ok("1")
    );
    println!(
        "organisation deletion routes and worker: {}",
        config.organisation_deletion_worker_enabled
    );
    println!(
        "Cloud personal retention dry run: {}",
        config.cloud_retention_dry_run_enabled
    );
    println!(
        "Cloud shared organisation retention dry run: {}",
        config.cloud_shared_retention_dry_run_enabled
    );

    // Default-on telemetry must never be a surprise: say so at boot, with the off switch.
    if config.telemetry.ping_enabled && !config.telemetry.ingest_enabled {
        println!(
            "telemetry: daily anonymous version ping is on (random instance uuid + version + \
             os/arch, nothing else) - set SOTTO_TELEMETRY=off to disable; see README §Telemetry"
        );
    }
    if config.organisation_deletion_worker_enabled {
        sotto_server::org_deletion_worker::spawn(pool.clone(), worker_provider.clone());
    }
    if config.billing_recovery_worker_enabled {
        let billing = state.billing.clone().ok_or_else(|| {
            Error::Config("billing recovery worker requires configured Stripe billing".into())
        })?;
        sotto_server::billing_recovery_executor::spawn(pool.clone(), billing);
    }
    if config.sponsored_billing_recovery_worker_enabled {
        let billing = state.billing.clone().ok_or_else(|| {
            Error::Config(
                "sponsored billing recovery worker requires configured Stripe billing".into(),
            )
        })?;
        sotto_server::sponsored_billing_recovery_executor::spawn(pool.clone(), billing);
    }
    if config.cloud_retention_dry_run_enabled || config.cloud_shared_retention_dry_run_enabled {
        sotto_server::cloud_retention_lifecycle::spawn(state.clone());
    }
    if config.founding_schedule_worker_enabled {
        #[cfg(not(feature = "e2e-mock-billing"))]
        {
            let provider = worker_provider.as_ref().ok_or_else(|| {
                Error::Config("founding schedule worker requires Stripe billing".into())
            })?;
            let catalogue = config
                .billing
                .as_ref()
                .and_then(|billing| billing.price_catalogue.as_ref())
                .ok_or_else(|| {
                    Error::Config(
                        "founding schedule worker requires the hosted price catalogue".into(),
                    )
                })?;
            for offer in BillingOffer::ALL {
                provider
                    .validate_personal_price(catalogue.id_for(offer), offer)
                    .await
                    .map_err(|_| {
                        Error::Config(
                            "founding schedule worker could not verify the Stripe price catalogue"
                                .into(),
                        )
                    })?;
            }
        }
        let provider = worker_provider.clone().ok_or_else(|| {
            Error::Config("founding schedule worker requires Stripe billing".into())
        })?;
        let catalogue = config
            .billing
            .as_ref()
            .and_then(|billing| billing.price_catalogue.clone())
            .ok_or_else(|| {
                Error::Config("founding schedule worker requires the hosted price catalogue".into())
            })?;
        sotto_server::founding_schedule_worker::spawn(pool.clone(), provider, catalogue);
    }
    if config.founding_price_notices_enabled {
        sotto_server::founding_price_notices::spawn(pool.clone());
    }
    if let Some(sender) = notice_sender {
        sotto_server::notifications::spawn(pool.clone(), sender);
    }
    if config.billing_transfer_execution_enabled {
        let provider = worker_provider.as_ref().ok_or_else(|| {
            Error::Config("payer transfer execution requires Stripe billing".into())
        })?;
        let catalogue = config
            .billing
            .as_ref()
            .and_then(|billing| billing.price_catalogue.clone())
            .ok_or_else(|| {
                Error::Config("payer transfer execution requires the hosted price catalogue".into())
            })?;
        sotto_server::billing_transfer_worker::spawn(pool.clone(), provider.clone(), catalogue);
    }
    if config.provider_refresh_worker_enabled {
        let Some((coverage, sponsored_coverage, stripe)) = personal_coverage_runtime.clone() else {
            return Err(Error::Config(
                "provider refresh worker requires configured personal Stripe coverage".into(),
            ));
        };
        sotto_server::cloud_provider_stripe_refresh_executor::spawn(
            pool.clone(),
            stripe,
            coverage,
            sponsored_coverage,
            config
                .provider_authority_revalidation_enabled
                .then_some(config.cloud_coverage_authority_max_age_seconds)
                .flatten(),
        );
    }
    if config.provider_authority_revalidation_enabled {
        let Some((coverage, sponsored_coverage, stripe)) = personal_coverage_runtime.clone() else {
            return Err(Error::Config(
                "Cloud authority revalidation requires configured Stripe coverage".into(),
            ));
        };
        let max_age = config
            .cloud_coverage_authority_max_age_seconds
            .ok_or_else(|| Error::Config("Cloud authority max age is required".into()))?;
        sotto_server::cloud_provider_stripe_refresh_executor::spawn_revalidation(
            pool.clone(),
            stripe,
            coverage,
            sponsored_coverage,
            max_age,
        );
    }
    if config.provider_refresh_reconciliation_enabled {
        let (Some((coverage, _, stripe)), Some(billing)) = (repair_runtime, repair_billing) else {
            return Err(Error::Config(
                "Stripe event reconciliation requires configured Stripe billing".into(),
            ));
        };
        sotto_server::cloud_provider_stripe_event_repair_executor::spawn(
            pool.clone(),
            stripe,
            coverage,
            billing,
        );
    }
    sotto_server::telemetry::spawn(pool, config.telemetry.clone());

    let listener = tokio::net::TcpListener::bind(&config.bind_addr)
        .await
        .map_err(|e| Error::Io(e.to_string()))?;
    println!("sotto-server listening on http://{}", config.bind_addr);
    axum::serve(listener, sotto_server::app(state))
        .await
        .map_err(|e| Error::Io(e.to_string()))
}

#[cfg(not(feature = "e2e-mock-billing"))]
fn stripe_environment(api_key: &str) -> Option<ProviderEnvironment> {
    if api_key.starts_with("sk_test_") || api_key.starts_with("rk_test_") {
        Some(ProviderEnvironment::Test)
    } else if api_key.starts_with("sk_live_") || api_key.starts_with("rk_live_") {
        Some(ProviderEnvironment::Live)
    } else {
        None
    }
}

#[cfg(not(feature = "e2e-mock-billing"))]
async fn verify_provider_refresh_configuration(
    stripe: &StripeReadClient,
    coverage: &StripeCoverageConfig,
    ids: &sotto_server::config::BillingPriceIds,
) -> Result<()> {
    let mut session = stripe.session();
    stripe.account(&mut session).await.map_err(|_| {
        Error::Config("Stripe account identity or mode could not be verified".into())
    })?;
    let mut observations = Vec::new();
    for (offer, price_id) in [
        (BillingOffer::StandardMonthly, &ids.standard_monthly),
        (BillingOffer::StandardAnnual, &ids.standard_annual),
        (BillingOffer::FoundingMonthly, &ids.founding_monthly),
        (BillingOffer::FoundingAnnual, &ids.founding_annual),
    ] {
        let observation = stripe
            .price_observation(&mut session, price_id)
            .await
            .map_err(|_| Error::Config("a configured Stripe price could not be verified".into()))?;
        observations.push((offer, observation));
    }
    BillingPriceCatalogue::from_provider_prices(
        coverage.account_id.clone(),
        coverage.environment,
        ids,
        observations,
    )
    .map_err(|_| {
        Error::Config("configured Stripe prices do not match the Cloud catalogue".into())
    })?;
    Ok(())
}
