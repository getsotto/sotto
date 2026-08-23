# Organisation-deletion metrics

The staged deletion worker records aggregate operational metrics in PostgreSQL. The counters are
separate from the organisation audit stream: retries and compare-and-set races are useful to an
operator, but are not user-visible organisation events. No organisation id, provider message, or
secret value is stored in a metric row.

## Recorded data

`organization_deletions` is the source for the current operation gauges:

- `state` and `count(*)` show deletion attempts by lifecycle state;
- `requested_at` gives the age of the oldest operation in each non-terminal state;
- `purge_started_at` and `completed_at` give completed purge duration.

`organization_deletion_metric_counters` stores these monotonic counters:

| Metric | Outcomes | Meaning |
| --- | --- | --- |
| `provider_cancellation_attempts` | `terminal`, `missing`, `authentication`, `resource_missing`, `retryable`, `unknown` | Provider calls made while cancelling billing |
| `provider_reconciliation_attempts` | `terminal`, `missing`, `blocking`, `authentication`, `resource_missing`, `retryable`, `unknown` | Provider status calls made while reconciling retention or recovery |
| `lease_expiries` | `reclaimed` | Due work reclaimed after a worker lease expired |
| `stale_compare_and_set` | `rejected` | A worker result lost its version or lease race |
| `purge_attempts` | `completed`, `failed` | Final purge outcomes |

Provider labels are a fixed vocabulary. Raw provider error codes remain in the sanitised operation
row where the existing owner-facing error mapping can handle them; they never become metric labels.

The server's internal `org_deletion_metrics::snapshot` function reads these gauges and counters in
one consistent interface for a future protected exporter. It is not a public HTTP endpoint. The
deletion routes and worker remain staged, so a follow-up enablement change must connect this seam to
an authenticated exporter before operators depend on a live dashboard.

## Alert conditions

An exporter or an operator query should alert when:

- any operation is in `failed`;
- an operation has remained in `cancelling_billing` for more than 24 hours;
- a retention operation is due for purge but has not advanced;
- `lease_expiries{outcome="reclaimed"}` or `stale_compare_and_set{outcome="rejected"}` rises
  repeatedly;
- `purge_attempts{outcome="failed"}` rises.

These alerts identify work that needs the runbook. They do not bypass the billing observation gate
or permit direct deletion of an `organizations` row.
