//! Wall and business clocks.
//!
//! Stripe Test Clocks can move provider time independently of the host running Sotto. The
//! `e2e-test-clock` feature lets an isolated rehearsal process align business timestamps with the
//! test clock. Webhook signature freshness must continue to use [`wall_epoch_seconds`].

use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(feature = "e2e-test-clock")]
const E2E_CLOCK_EPOCH_ENV: &str = "SOTTO_E2E_CLOCK_EPOCH";
#[cfg(feature = "e2e-test-clock")]
const E2E_CLOCK_CUSTOMER_ENV: &str = "SOTTO_E2E_TEST_CLOCK_CUSTOMER_ID";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClockOverrideError {
    Invalid,
}

fn resolve_business_epoch(
    wall_epoch: i64,
    configured_override: Option<&str>,
) -> Result<i64, ClockOverrideError> {
    let Some(value) = configured_override else {
        return Ok(wall_epoch);
    };
    let epoch = value
        .parse::<i64>()
        .map_err(|_| ClockOverrideError::Invalid)?;
    if epoch <= 0 {
        return Err(ClockOverrideError::Invalid);
    }
    Ok(epoch)
}

#[cfg(feature = "e2e-test-clock")]
fn validate_test_clock_customer(
    customer_id: &str,
    api_key: &str,
    has_clock_epoch: bool,
) -> Result<(), &'static str> {
    if !customer_id.starts_with("cus_") || customer_id.len() < 8 {
        return Err("test customer id is invalid");
    }
    if !(api_key.starts_with("sk_test_") || api_key.starts_with("rk_test_")) {
        return Err("test customer requires a Stripe test-mode API key");
    }
    if !has_clock_epoch {
        return Err("test customer requires a test clock epoch");
    }
    Ok(())
}

/// Real host time, used for time-sensitive authentication such as Stripe webhook signatures.
pub fn wall_epoch_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_secs() as i64
}

/// Business time. A private test-clock override exists only in rehearsal builds.
pub fn business_epoch_seconds() -> i64 {
    let wall_epoch = wall_epoch_seconds();
    #[cfg(feature = "e2e-test-clock")]
    let configured_override = std::env::var(E2E_CLOCK_EPOCH_ENV).ok();
    #[cfg(not(feature = "e2e-test-clock"))]
    let configured_override: Option<String> = None;

    resolve_business_epoch(wall_epoch, configured_override.as_deref())
        .unwrap_or_else(|_| panic!("SOTTO_E2E_CLOCK_EPOCH must be a positive Unix timestamp"))
}

/// Optional Stripe customer created under the rehearsal's Test Clock.
///
/// This only exists in explicitly enabled rehearsal builds. It lets Checkout attach a session to
/// the already clock-bound customer instead of asking Stripe to create a wall-clock customer.
#[cfg(feature = "e2e-test-clock")]
pub fn test_clock_customer_id() -> Option<String> {
    std::env::var(E2E_CLOCK_CUSTOMER_ENV).ok()
}

/// Validate test-clock configuration before the server binds a listener.
pub fn validate_configuration() -> Result<(), String> {
    #[cfg(feature = "e2e-test-clock")]
    if let Some(value) = std::env::var_os(E2E_CLOCK_EPOCH_ENV) {
        let value = value
            .to_str()
            .ok_or_else(|| format!("{E2E_CLOCK_EPOCH_ENV} must be a positive Unix timestamp"))?;
        resolve_business_epoch(0, Some(value))
            .map_err(|_| format!("{E2E_CLOCK_EPOCH_ENV} must be a positive Unix timestamp"))?;
    }
    #[cfg(feature = "e2e-test-clock")]
    if let Some(value) = std::env::var_os(E2E_CLOCK_CUSTOMER_ENV) {
        let customer_id = value
            .to_str()
            .ok_or_else(|| format!("{E2E_CLOCK_CUSTOMER_ENV} must be a test customer id"))?;
        let api_key = std::env::var("STRIPE_API_KEY").unwrap_or_default();
        validate_test_clock_customer(
            customer_id,
            &api_key,
            std::env::var_os(E2E_CLOCK_EPOCH_ENV).is_some(),
        )
        .map_err(|reason| format!("{E2E_CLOCK_CUSTOMER_ENV} {reason}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "e2e-test-clock")]
    use super::validate_test_clock_customer;
    use super::{resolve_business_epoch, ClockOverrideError};

    #[test]
    fn business_time_uses_wall_clock_when_no_override_is_configured() {
        assert_eq!(
            resolve_business_epoch(1_800_000_000, None),
            Ok(1_800_000_000)
        );
    }

    #[test]
    fn business_time_uses_the_test_clock_epoch_when_configured() {
        assert_eq!(
            resolve_business_epoch(1_800_000_000, Some("1810000000")),
            Ok(1_810_000_000)
        );
    }

    #[test]
    fn business_time_rejects_invalid_test_clock_epochs() {
        for value in ["", "0", "-1", "not-an-epoch", "9223372036854775808"] {
            assert_eq!(
                resolve_business_epoch(1_800_000_000, Some(value)),
                Err(ClockOverrideError::Invalid)
            );
        }
    }

    #[cfg(feature = "e2e-test-clock")]
    #[test]
    fn test_clock_customer_requires_a_clock_epoch_and_test_key() {
        assert_eq!(
            validate_test_clock_customer("cus_12345678", "rk_test_secret", true),
            Ok(())
        );
        assert!(validate_test_clock_customer("cus_12345678", "sk_live_secret", true).is_err());
        assert!(validate_test_clock_customer("cus_12345678", "rk_test_secret", false).is_err());
        assert!(validate_test_clock_customer("not_customer", "rk_test_secret", true).is_err());
    }
}
