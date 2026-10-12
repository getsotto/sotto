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
| `personal_interval_switch` | A monthly-to-annual or annual-to-monthly change has one effective boundary and no duplicate invoice or overlapping term. |
| `personal_cancellation` | End-of-term cancellation preserves the paid term and produces no further invoice after the boundary. |
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

## Run procedure

1. Create a fresh test-mode Stripe account or an isolated test clock set. Configure the four hosted
   prices with the amounts and intervals in `billing_catalogue.rs`, and set the account API version
   to `2026-07-29.dahlia`. Create a restricted `rk_test_` key with only the reads and writes needed
   for this run.
2. Start a disposable Postgres and Sotto server with `SOTTO_DEPLOYMENT_MODE=cloud`, the four price
   ids, the Stripe webhook secret, and the relevant dormant lifecycle switches. Keep the server
   commit and the Stripe API version in the run notes.
3. Forward only the required test events to the disposable server. Include checkout, subscription,
   invoice, refund and payment events used by the selected cases. Save the redacted listener log;
   never put a signing secret or a full event payload in the evidence file.
4. Drive each scenario through the real web or operator entry point. For renewal and founding
   anniversary rows, use Stripe test clocks when the account supports them. If the account cannot
   reproduce a required row, mark it `blocked`; do not substitute a fixture or mark it passed.
5. Check the Sotto database after each event sequence: operation state, provider customer,
   subscription and item ids, named allocations, paid-through/recovery/export dates, durable
   notices, and retention receipts. Check Stripe invoices and subscriptions at the same boundary.
6. Exercise replay, out-of-order delivery, and a worker restart using the same event ids and
   operation keys. The expected result is one durable outcome, not merely a successful HTTP
   response.
7. Copy the observations into a private manifest matching the input shape accepted by
   `scripts/validate-stripe-lifecycle-evidence`. Keep raw ids in that private file only:

   ```sh
   scripts/validate-stripe-lifecycle-evidence \
     /private/path/stripe-lifecycle-evidence.json \
     --output /private/path/stripe-lifecycle-report.json
   ```

   The command writes a report containing only the app SHA, pinned API version, aggregate check
   counts, and fingerprints of provider ids. It exits non-zero for a failed, blocked, or missing
   scenario. Attach that sanitised report and the reviewer's signed run notes to the release
   decision; do not attach the input manifest.

## Activation decision

A passing report is evidence for the exact application SHA and test account used. It is not a
blanket approval to enable hosted sales: review the report, confirm that the sandbox account used
all production-shaped webhook paths, and record any remaining restrictions. If a required case is
blocked or the report is stale, leave hosted sales and enforcement disabled. Clean up only the
customers, subscriptions, schedules, clocks, and database rows created by this run.
