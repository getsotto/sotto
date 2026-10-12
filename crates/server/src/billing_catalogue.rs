//! Server-owned Stripe price catalogue.
//!
//! The browser chooses an offer key such as `standard_monthly`; it never chooses a Stripe price
//! id or amount. This module validates the operator's four configured Stripe prices before a
//! catalogue can be used by a billing operation.

use std::collections::BTreeMap;
use std::fmt;

use thiserror::Error;

use crate::cloud_provider::ProviderEnvironment;

/// Provider price ids configured by the operator for the four hosted offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BillingPriceIds {
    pub standard_monthly: String,
    pub standard_annual: String,
    pub founding_monthly: String,
    pub founding_annual: String,
}

impl BillingPriceIds {
    pub fn id_for(&self, offer: BillingOffer) -> &str {
        match offer {
            BillingOffer::StandardMonthly => &self.standard_monthly,
            BillingOffer::StandardAnnual => &self.standard_annual,
            BillingOffer::FoundingMonthly => &self.founding_monthly,
            BillingOffer::FoundingAnnual => &self.founding_annual,
        }
    }
}

pub const STANDARD_MONTHLY_PENCE: i64 = 299;
pub const STANDARD_ANNUAL_PENCE: i64 = 2_999;
pub const FOUNDING_MONTHLY_PENCE: i64 = 199;
pub const FOUNDING_ANNUAL_PENCE: i64 = 1_999;
pub const CURRENCY_GBP: &str = "gbp";

/// The only offers a client may request from the hosted billing surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BillingOffer {
    StandardMonthly,
    StandardAnnual,
    FoundingMonthly,
    FoundingAnnual,
}

impl BillingOffer {
    pub const ALL: [Self; 4] = [
        Self::StandardMonthly,
        Self::StandardAnnual,
        Self::FoundingMonthly,
        Self::FoundingAnnual,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StandardMonthly => "standard_monthly",
            Self::StandardAnnual => "standard_annual",
            Self::FoundingMonthly => "founding_monthly",
            Self::FoundingAnnual => "founding_annual",
        }
    }

    pub const fn expected_amount_pence(self) -> i64 {
        match self {
            Self::StandardMonthly => STANDARD_MONTHLY_PENCE,
            Self::StandardAnnual => STANDARD_ANNUAL_PENCE,
            Self::FoundingMonthly => FOUNDING_MONTHLY_PENCE,
            Self::FoundingAnnual => FOUNDING_ANNUAL_PENCE,
        }
    }

    pub const fn expected_interval(self) -> BillingInterval {
        match self {
            Self::StandardMonthly | Self::FoundingMonthly => BillingInterval::Month,
            Self::StandardAnnual | Self::FoundingAnnual => BillingInterval::Year,
        }
    }
}

impl fmt::Display for BillingOffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Stripe recurring intervals supported by the Cloud offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BillingInterval {
    Month,
    Year,
}

impl BillingInterval {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Month => "month",
            Self::Year => "year",
        }
    }
}

/// The authenticated fields read from one Stripe Price object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripePriceObservation {
    pub id: String,
    pub account_id: String,
    pub environment: ProviderEnvironment,
    pub active: bool,
    pub livemode: bool,
    pub currency: String,
    pub unit_amount: Option<i64>,
    pub interval: Option<BillingInterval>,
    pub interval_count: Option<i64>,
    pub usage_type: Option<String>,
}

impl StripePriceObservation {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: impl Into<String>,
        account_id: impl Into<String>,
        environment: ProviderEnvironment,
        active: bool,
        livemode: bool,
        currency: impl Into<String>,
        unit_amount: Option<i64>,
        interval: Option<BillingInterval>,
        interval_count: Option<i64>,
        usage_type: Option<String>,
    ) -> Self {
        Self {
            id: id.into(),
            account_id: account_id.into(),
            environment,
            active,
            livemode,
            currency: currency.into(),
            unit_amount,
            interval,
            interval_count,
            usage_type,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BillingPrice {
    offer: BillingOffer,
    provider_price_id: String,
    amount_pence: i64,
    interval: BillingInterval,
}

impl BillingPrice {
    pub fn offer(&self) -> BillingOffer {
        self.offer
    }

    pub fn provider_price_id(&self) -> &str {
        &self.provider_price_id
    }

    pub fn amount_pence(&self) -> i64 {
        self.amount_pence
    }

    pub fn interval(&self) -> BillingInterval {
        self.interval
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BillingPriceCatalogue {
    account_id: String,
    environment: ProviderEnvironment,
    prices: BTreeMap<BillingOffer, BillingPrice>,
}

impl BillingPriceCatalogue {
    /// Validate the complete four-price catalogue returned by the authenticated provider reads.
    pub fn from_provider_prices(
        account_id: impl Into<String>,
        environment: ProviderEnvironment,
        configured_ids: &BillingPriceIds,
        observations: impl IntoIterator<Item = (BillingOffer, StripePriceObservation)>,
    ) -> Result<Self, BillingCatalogueError> {
        let account_id = account_id.into();
        if account_id.trim().is_empty() {
            return Err(BillingCatalogueError::InvalidContext("account"));
        }

        let mut prices = BTreeMap::new();
        for (offer, observation) in observations {
            if prices
                .insert(
                    offer,
                    validate_price(
                        offer,
                        &account_id,
                        environment,
                        configured_ids.id_for(offer),
                        observation,
                    )?,
                )
                .is_some()
            {
                return Err(BillingCatalogueError::DuplicateOffer(offer));
            }
        }
        for offer in BillingOffer::ALL {
            if !prices.contains_key(&offer) {
                return Err(BillingCatalogueError::MissingOffer(offer));
            }
        }
        Ok(Self {
            account_id,
            environment,
            prices,
        })
    }

    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    pub fn environment(&self) -> ProviderEnvironment {
        self.environment
    }

    pub fn price(&self, offer: BillingOffer) -> &BillingPrice {
        self.prices
            .get(&offer)
            .expect("a catalogue always contains all supported offers")
    }

    /// Resolve a client-selected offer without accepting a client-selected provider price id.
    pub fn resolve(&self, offer: BillingOffer) -> (&str, i64, BillingInterval) {
        let price = self.price(offer);
        (
            price.provider_price_id(),
            price.amount_pence(),
            price.interval(),
        )
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum BillingCatalogueError {
    #[error("billing catalogue has an invalid {0} context")]
    InvalidContext(&'static str),
    #[error("billing catalogue is missing {0}")]
    MissingOffer(BillingOffer),
    #[error("billing catalogue contains duplicate {0}")]
    DuplicateOffer(BillingOffer),
    #[error("Stripe price {offer} has invalid {field}")]
    InvalidPrice {
        offer: BillingOffer,
        field: &'static str,
    },
    #[error("Stripe price {offer} has mismatched {field}")]
    MismatchedPrice {
        offer: BillingOffer,
        field: &'static str,
    },
}

fn validate_price(
    offer: BillingOffer,
    account_id: &str,
    environment: ProviderEnvironment,
    configured_id: &str,
    observation: StripePriceObservation,
) -> Result<BillingPrice, BillingCatalogueError> {
    if configured_id.trim().is_empty() {
        return Err(BillingCatalogueError::InvalidPrice {
            offer,
            field: "configured_id",
        });
    }
    if observation.id.trim().is_empty() {
        return Err(BillingCatalogueError::InvalidPrice { offer, field: "id" });
    }
    if observation.id != configured_id {
        return Err(BillingCatalogueError::MismatchedPrice { offer, field: "id" });
    }
    if observation.account_id != account_id {
        return Err(BillingCatalogueError::MismatchedPrice {
            offer,
            field: "account",
        });
    }
    if observation.environment != environment {
        return Err(BillingCatalogueError::MismatchedPrice {
            offer,
            field: "environment",
        });
    }
    if !observation.active {
        return Err(BillingCatalogueError::InvalidPrice {
            offer,
            field: "active",
        });
    }
    let expected_livemode = matches!(environment, ProviderEnvironment::Live);
    if observation.livemode != expected_livemode {
        return Err(BillingCatalogueError::MismatchedPrice {
            offer,
            field: "livemode",
        });
    }
    if observation.currency != CURRENCY_GBP {
        return Err(BillingCatalogueError::MismatchedPrice {
            offer,
            field: "currency",
        });
    }
    if observation.unit_amount != Some(offer.expected_amount_pence()) {
        return Err(BillingCatalogueError::MismatchedPrice {
            offer,
            field: "unit_amount",
        });
    }
    if observation.interval != Some(offer.expected_interval()) {
        return Err(BillingCatalogueError::MismatchedPrice {
            offer,
            field: "interval",
        });
    }
    if observation.interval_count != Some(1) {
        return Err(BillingCatalogueError::MismatchedPrice {
            offer,
            field: "interval_count",
        });
    }
    if observation.usage_type.as_deref() != Some("licensed") {
        return Err(BillingCatalogueError::MismatchedPrice {
            offer,
            field: "usage_type",
        });
    }
    Ok(BillingPrice {
        offer,
        provider_price_id: observation.id,
        amount_pence: offer.expected_amount_pence(),
        interval: offer.expected_interval(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(offer: BillingOffer) -> StripePriceObservation {
        StripePriceObservation::new(
            format!("price_{}", offer.as_str()),
            "acct_test",
            ProviderEnvironment::Test,
            true,
            false,
            CURRENCY_GBP,
            Some(offer.expected_amount_pence()),
            Some(offer.expected_interval()),
            Some(1),
            Some("licensed".into()),
        )
    }

    fn catalogue() -> BillingPriceCatalogue {
        BillingPriceCatalogue::from_provider_prices(
            "acct_test",
            ProviderEnvironment::Test,
            &BillingPriceIds {
                standard_monthly: "price_standard_monthly".into(),
                standard_annual: "price_standard_annual".into(),
                founding_monthly: "price_founding_monthly".into(),
                founding_annual: "price_founding_annual".into(),
            },
            BillingOffer::ALL
                .into_iter()
                .map(|offer| (offer, observation(offer))),
        )
        .unwrap()
    }

    #[test]
    fn validates_all_four_server_owned_offers() {
        let catalogue = catalogue();
        assert_eq!(
            catalogue
                .price(BillingOffer::StandardMonthly)
                .amount_pence(),
            299
        );
        assert_eq!(
            catalogue.price(BillingOffer::StandardAnnual).amount_pence(),
            2_999
        );
        assert_eq!(
            catalogue
                .price(BillingOffer::FoundingMonthly)
                .amount_pence(),
            199
        );
        assert_eq!(
            catalogue.price(BillingOffer::FoundingAnnual).amount_pence(),
            1_999
        );
        assert_eq!(
            catalogue.resolve(BillingOffer::StandardMonthly).0,
            "price_standard_monthly"
        );
    }

    #[test]
    fn rejects_a_price_that_is_not_the_configured_id() {
        let mut invalid = observation(BillingOffer::StandardMonthly);
        invalid.id = "price_unconfigured".into();
        let result = BillingPriceCatalogue::from_provider_prices(
            "acct_test",
            ProviderEnvironment::Test,
            &BillingPriceIds {
                standard_monthly: "price_standard_monthly".into(),
                standard_annual: "price_standard_annual".into(),
                founding_monthly: "price_founding_monthly".into(),
                founding_annual: "price_founding_annual".into(),
            },
            [(BillingOffer::StandardMonthly, invalid)],
        );
        assert!(matches!(
            result,
            Err(BillingCatalogueError::MismatchedPrice { field: "id", .. })
        ));
    }

    #[test]
    fn rejects_wrong_currency_amount_recurrence_or_mode() {
        for field in [
            "currency",
            "unit_amount",
            "interval",
            "interval_count",
            "usage_type",
            "livemode",
        ] {
            let mut invalid = observation(BillingOffer::StandardMonthly);
            match field {
                "currency" => invalid.currency = "usd".into(),
                "unit_amount" => invalid.unit_amount = Some(1),
                "interval" => invalid.interval = Some(BillingInterval::Year),
                "interval_count" => invalid.interval_count = Some(2),
                "usage_type" => invalid.usage_type = Some("metered".into()),
                "livemode" => invalid.livemode = true,
                _ => unreachable!(),
            }
            let result = BillingPriceCatalogue::from_provider_prices(
                "acct_test",
                ProviderEnvironment::Test,
                &BillingPriceIds {
                    standard_monthly: "price_standard_monthly".into(),
                    standard_annual: "price_standard_annual".into(),
                    founding_monthly: "price_founding_monthly".into(),
                    founding_annual: "price_founding_annual".into(),
                },
                [(BillingOffer::StandardMonthly, invalid)],
            );
            assert!(
                matches!(result, Err(BillingCatalogueError::MismatchedPrice { field: actual, .. }) if actual == field),
                "expected {field}, got {result:?}"
            );
        }
    }

    #[test]
    fn incomplete_catalogue_cannot_resolve_an_offer() {
        let result = BillingPriceCatalogue::from_provider_prices(
            "acct_test",
            ProviderEnvironment::Test,
            &BillingPriceIds {
                standard_monthly: "price_standard_monthly".into(),
                standard_annual: "price_standard_annual".into(),
                founding_monthly: "price_founding_monthly".into(),
                founding_annual: "price_founding_annual".into(),
            },
            [(
                BillingOffer::StandardMonthly,
                observation(BillingOffer::StandardMonthly),
            )],
        );
        assert!(matches!(
            result,
            Err(BillingCatalogueError::MissingOffer(
                BillingOffer::StandardAnnual
            ))
        ));
    }
}
