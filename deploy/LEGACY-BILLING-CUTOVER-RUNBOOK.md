# Legacy billing inventory and cutover rehearsal

This runbook is the first step of the hosted billing transition. It is deliberately read-only
until the existing cohort and notice policy has an approved owner and date. Do not use an
inventory report as permission to charge a customer, create a named seat, revoke a token or
change an entitlement.

## Produce an inventory

Run the command against a disposable copy first, then against the deployment's database using a
read-only database role. Keep the credential-bearing URL in the environment rather than putting
it in the command's arguments, where local process inspection can expose it.

```sh
export INVENTORY_DATABASE_URL="$DATABASE_URL"
scripts/inventory-legacy-billing \
  --output deploy/rehearsals/legacy-billing-$(date -u +%Y%m%dT%H%M%SZ).json
unset INVENTORY_DATABASE_URL
```

The report contains only aggregate counts, migration/Postgres provenance and proposed treatment.
It does not contain user IDs, organisation IDs, email addresses, Stripe IDs, tokens, links or
encrypted data. Keep it in the private `deploy/rehearsals/` directory and attach it to the
operator change record rather than committing it.

## Interpret the result

Organisation cohorts are classified as follows:

- `legacy_paid`: retain the existing provider coverage until an audited migration outcome exists;
- `legacy_trial`: retain the trial promise until its approved transition and notice date;
- `legacy_free`: retain free access;
- `legacy_manual_team`: quarantine for an operator decision rather than treating a manual tier as
  proof of payment;
- `quarantine_*`: quarantine contradictory provider, tier or trial state.

Personal billing states, sponsored subscriptions, project ownership, machine-token state, share
link state and membership roles are reported separately. Their counts are evidence for the
mapping review; they are not automatic actions. In particular, never grant an organisation's
paid state to every member without an explicit named beneficiary mapping, and never remove a
token or link because a report labels its owner ambiguous.

## Rehearse an upgrade

1. Restore a recent dump and its retention sidecar into an isolated Postgres instance using the
   [restore runbook](README.md#restore-verification). Keep traffic, notifications, checkout and
   destructive workers disabled.
2. Run the inventory against the restored database and record its migration and Postgres
   provenance.
3. Run it again after restarting the database. The cohort counts must be unchanged; only the
   generated timestamp may differ. Check that mechanically:

   ```sh
   scripts/compare-legacy-billing-inventory \
     deploy/rehearsals/legacy-before.json \
     deploy/rehearsals/legacy-after.json
   ```
4. Repeat with a fresh database migrated from this checkout and with Stripe unset. Sync, audit,
   machine authentication and free links must remain usable; the billing endpoint may remain
   unconfigured.
5. Record any unexpected shadow denials separately. A denial is not evidence to change a cohort
   mapping, and no enforcement switch is enabled by this rehearsal.

## Cutover gate

The report is an input to the D09 decision record. Cutover remains blocked until an accountable
operator approves cohort treatment, customer notices, legacy subscription end states, token/link
promises and the rollback owner. When that decision exists, a later migration may add a resumable
mapping table and backfill worker. This inventory command must remain available so the pre- and
post-cutover reports can be compared.
