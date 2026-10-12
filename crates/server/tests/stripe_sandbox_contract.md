# Stripe coverage sandbox contract

`cloud_provider_stripe_sandbox.rs` is an opt-in, read-only probe for the Stripe test account used
to verify the coverage adapter's real resource shapes. It uses the production
`StripeReadClient`, so account binding, mode checks, pagination, response limits and the shared
session deadline are the same as a runtime read.

The test is not part of ordinary pull-request CI. Run it only with a disposable Stripe test-mode
account and these values:

| Variable | Meaning |
| --- | --- |
| `SOTTO_RUN_STRIPE_SANDBOX_TESTS` | Must be `1` to opt in. |
| `STRIPE_SANDBOX_READ_KEY` | Restricted `rk_test_` key with read access only. |
| `STRIPE_SANDBOX_ACCOUNT_ID` | Account returned by `GET /v1/account`. |
| `STRIPE_SANDBOX_CUSTOMER_ID` | Customer attached to the fixture subscription. |
| `STRIPE_SANDBOX_SUBSCRIPTION_ID` | Subscription used for the history read. |
| `STRIPE_SANDBOX_MONTHLY_PRICE_ID` | Configured monthly price. |
| `STRIPE_SANDBOX_ANNUAL_PRICE_ID` | Configured annual price. |
| `STRIPE_SANDBOX_ALLOCATION_REFERENCE` | Allocation value in invoice metadata. |
| `STRIPE_SANDBOX_PROVIDER_ITEM_ID` | Subscription item attached to the fixture price. |
| `STRIPE_SANDBOX_REPORT` | Optional path for the sanitised JSON report. |

The fixture must contain at least one paid and one non-paid invoice for the configured customer and
subscription. The test fails on an unresolved invoice shape, account or mode mismatch, unsupported
subscription status, missing required variable, or a provider read/budget error. It never creates,
updates, refunds, cancels or deletes a Stripe resource.

The report contains only the pinned API version, hashed identifiers, mode/status, entry counts and
the fact that the shared request/page/record/byte/deadline budgets were used. It does not write
raw Stripe responses, credentials, invoice amounts, metadata or signed payloads. The manual
workflow `.github/workflows/stripe-coverage-sandbox.yml` runs this probe from `main` inside the
`stripe-coverage-sandbox` environment and retains the report for 30 days.

This probe is evidence for the current read contract, not proof of an atomic Stripe snapshot or a
production account. A failed or unavailable sandbox run leaves runtime provider activation off.
