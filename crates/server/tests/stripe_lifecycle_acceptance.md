# Stripe Cloud lifecycle acceptance

This is the operator runbook for the paid lifecycle gate. It is deliberately separate from the
ordinary Stripe smoke test and the read-only coverage contract: a smoke run proves that one hosted
checkout can reach a webhook, while this run proves that the application and Stripe agree over the
whole paid lifecycle.

Run it only against a dedicated Stripe **test-mode** account and a disposable Sotto database. Do
not use a live key, a production customer, or a production webhook endpoint. Keep the Sotto billing
activation switch off until the report has passed and been reviewed.

## Required scenarios

Every row must be observed on the same application commit and pinned API version
`2026-07-29.dahlia`. A skipped, unsupported, or partly observed row is a failed acceptance run.

| Scenario | Required evidence |
| --- | --- |
| `personal_monthly_initial` | Checkout settles one monthly invoice; the named account is paid through the invoice period and the next amount is the configured standard monthly price. |
| `personal_annual_initial` | Checkout settles one annual invoice; the paid term and next amount are annual. |
| `personal_founding_monthly_initial` | The founding price and place allocation are recorded; the renewal amount is the configured standard monthly price. |
| `personal_founding_annual_initial` | The founding annual term and anniversary are recorded; the renewal amount is the configured standard annual price. |
| `personal_renewal` | A test clock or an equivalent supported Stripe test mechanism produces exactly one next invoice and one paid-through advance. |
| `personal_interval_switch` | During a founding term, attempt a monthly/annual interval change through the hosted portal. It must be unavailable; the stored offer, interval and paid-through date remain unchanged, and no schedule or invoice is created. Founders keep their original interval through the founding term and transition to the matching standard interval at the original anniversary. |
| `personal_cancellation` | Advance a monthly founder at least one renewal while still inside the founding phase, then cancel through Sotto's endpoint; verify the current paid phase remains intact, the schedule ends with cancellation at the paid-period boundary, and no renewal invoice follows. Confirm the subscription webhook's `cancel_at` keeps the account marked to cancel even if `cancel_at_period_end` is false. Also verify an unscheduled personal subscription retains period-end cancellation and that lifting it clears the cancellation intent and restores the founding transition. |
| `personal_full_refund_cancellation` | An explicitly approved full refund is recorded before termination; a failed or pending refund leaves the paid term available. |
| `personal_failed_renewal_recovery` | One failed renewal produces one recovery window and export deadline; a later paid recovery closes the episode without extending access twice. |
| `sponsored_initial_mixed_items` | One organisation subscription settles mixed monthly/annual named items and every named allocation links to the provider item on the invoice. |
| `sponsored_anniversary` | A sponsored renewal has one invoice and the same named seats remain covered. |
| `sponsored_seat_replacement` | A removal and replacement meet at one dated boundary; no invoice has a quantity mismatch or duplicate seat. |
| `sponsored_payer_transfer` | A consented payer transfer moves the payer once and does not create a second active subscription. |
| `webhook_replay_and_out_of_order_delivery` | Duplicate and out-of-order deliveries converge to the same durable state without a second charge or date movement. |
| `ambiguous_provider_outcome_and_worker_restart` | A deliberately interrupted provider call is retried after restart and converges without a second provider operation. |
| `human_export_and_restore` | A human export contains encrypted material, restores in a clean database, and excludes environments never granted to the person. |
| `retention_scope_and_restore` | A scoped retention run deletes only the named resources; the tombstone sidecar replays into a restored dump without resurrecting them. |

The first nine rows use the personal account surface. The next four use the organisation seat
surface. The final four exercise the lifecycle controls and restore tools. Record provider
identifiers while the run is private; the acceptance report below fingerprints them before it is
shared.

## Deterministic database acceptance

CI runs a curated set of 27 production-route, signed-webhook, worker-recovery, export and retention
integration tests with controlled provider responses. It first verifies that every exact test name
is still discoverable, then creates one uniquely named disposable Postgres database for the run,
executes the selected tests serially, and drops that database on success or failure. The test
bundle owns its fixtures within that database. To run it locally, point `DATABASE_URL` at a local
Postgres server using a throwaway database name ending in `_lifecycle_acceptance`; the runner
creates and drops a separate per-run database and refuses a non-local host:

```sh
DATABASE_URL=postgres://localhost/sotto_cloud_lifecycle_acceptance \
SOTTO_RUN_DB_TESTS=1 scripts/check-cloud-lifecycle-integration
```

This bundle exercises only the exact deterministic application cases listed by the runner. It does
not cover every sub-check in the table above: live Stripe billing and test clocks, all plan/interval
variants, real email delivery, released previous-minor clients, CLI restore, and managed backup
tombstone replay remain separate required evidence in the pinned test-mode/operator rehearsal. A
green CI bundle must not be substituted for those observations.

## Run procedure

1. Create a fresh test-mode Stripe account or an isolated test clock set. Configure the four hosted
   prices with the amounts and intervals in `billing_catalogue.rs`, and set the account API version
   to `2026-07-29.dahlia`. Create a restricted `rk_test_` key with only the reads and writes needed
   for this run.
2. Start a disposable Postgres and Sotto server with `SOTTO_DEPLOYMENT_MODE=cloud`, the four price
   ids, the Stripe webhook secret, and the relevant dormant lifecycle switches. Keep the server
   commit and the Stripe API version in the run notes.
3. Forward only the required test events to the disposable server. Include checkout, subscription,
   invoice, refund and payment events used by the selected cases. Save the redacted listener log
   privately; never put a signing secret or a full event payload in the shareable acceptance report.
4. Drive each scenario through the real web or operator entry point. For renewal and founding
   anniversary rows, use Stripe test clocks when the account supports them. If the account cannot
   reproduce a required row, mark it `blocked`; do not substitute a fixture or mark it passed.
5. Check the Sotto database after each event sequence: operation state, provider customer,
   subscription and item ids, named allocations, paid-through/recovery/export dates, durable
   notices, and retention receipts. Check Stripe invoices and subscriptions at the same boundary.
6. Exercise replay, out-of-order delivery, and a worker restart using the same event ids and
   operation keys. The expected result is one durable outcome, not merely a successful HTTP
   response.
7. Generate a private manifest template with `--template`. For each observed scenario, write a
   private JSON observation artifact with this exact shape:

   ```json
   {
     "schema": "sotto-stripe-lifecycle-observation-v1",
     "scenario": "personal_monthly_initial",
     "app_sha": "<tested commit SHA>",
     "api_version": "2026-07-29.dahlia",
     "captured_at": "<timezone-qualified ISO-8601 timestamp>",
     "wall_clock_at": "<timezone-qualified wall-clock timestamp>",
     "stripe_test_clock_epoch": 1791321600,
     "checks": [{"id": "checkout_settled", "source": "<private evidence reference>", "observed": "<observed fact>"}],
     "provider_ids": ["<private Stripe ids>"]
   }
   ```

   `stripe_test_clock_epoch` is null when that scenario uses wall-clock time; never place a test
   clock timestamp in `wall_clock_at`. The manifest also records the built image's immutable
   `sha256:` digest, latest applied migration with zero pending migrations, and exact versions for
   Postgres, Rust, Stripe CLI, Node, Playwright, the server/web/CLI build and supported previous-minor
   web/CLI clients. Record the path and SHA-256 of the exact artifact bytes in that scenario's
   manifest row, set the full 40-character lowercase tested SHA and sandbox account, and mark it
   `passed` only after every listed fact is observed. The
   validator reads the artifact, verifies its digest, app SHA, pinned API version, scenario and
   exact check identifiers, and requires a private evidence reference and recorded value for every
   assertion. Its result still depends on
   the reviewer checking that the private source references support the recorded observations; a
   digest proves which bytes were reviewed, not that an operator's claim is true. Keep raw ids and
   observation artifacts private:

   ```sh
   scripts/validate-stripe-lifecycle-evidence --template /private/path/stripe-lifecycle-evidence.json
   scripts/validate-stripe-lifecycle-evidence \
     /private/path/stripe-lifecycle-evidence.json \
     --output /private/path/stripe-lifecycle-report.json
   ```

   The command writes a report containing only the app SHA, pinned API version, manifest and
   observation-artifact SHA-256 digests, fixed assertion identifiers read from the artifacts, image digest,
   migration state, tool/client versions, aggregate check counts, and fingerprints of provider
   ids. It exits non-zero for a failed, blocked, or missing
   scenario. When updating the private Cloud launch index, bind the SHA-256 of this exact
   sanitised report to the `stripe_lifecycle_17_scenarios` check, set its `tested_sha` to the same
   full candidate SHA, and rerun the launch-index validator. Attach the sanitised report and the
   reviewer's signed run notes to the release decision; do not attach the input manifest or raw
   observations.

## Activation decision

A passing report is evidence for the exact application SHA and test account used. It is not a
blanket approval to enable hosted sales: review the report, confirm that the sandbox account used
all production-shaped webhook paths, and record any remaining restrictions. If a required case is
blocked or the report is stale, leave hosted sales and enforcement disabled. Clean up only the
customers, subscriptions, schedules, clocks, and database rows created by this run.
