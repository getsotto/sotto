# Organisation-deletion metrics

The staged deletion worker records aggregate operational metrics in Postgres. The counters are
separate from the organisation audit stream: retries and compare-and-set races are useful to an
operator, but are not user-visible organisation events. No organisation id, provider message, or
secret value is stored in a metric row.

The server exposes the aggregate snapshot at
`GET /ops/organisation-deletion/metrics` as Prometheus text. The endpoint requires
`Authorization: Bearer <SOTTO_ORGANISATION_DELETION_METRICS_TOKEN>` and returns `503` while that
token is unset. Keep the token in the deployment secrets vault and never include it in a scrape
log or dashboard URL.

## Recorded data

`organization_deletions` is the source for the current operation gauges:

- `state` and `count(*)` show deletion attempts by every lifecycle state;
- `state_entered_at` gives the age of the oldest operation in each non-terminal state;
- `purge_started_at` and `completed_at` give completed purge duration;
- `purge_due_count` counts retention operations past their deadline that have not entered purging.

`state_entered_at` is reset by the database whenever an operation changes phase, so a retry or
provider reconciliation is measured from its current state rather than from the original request.

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

The Prometheus exporter publishes the following stable series: operation and oldest-age gauges
labelled by `state`, `sotto_organisation_deletion_attempts_total` labelled by `metric` and
`outcome`, purge duration gauges, and `sotto_organisation_deletion_purge_due_count`. The label
values come from the fixed database vocabularies; they are not arbitrary provider strings.

The internal `org_deletion_metrics::snapshot` function reads these gauges and counters in one
consistent interface for the protected exporter. The metrics route is operational-only and does
not expose organisation identifiers or provider text. The deletion routes and client control
remain staged, so this endpoint does not enable deletion by itself.

## Alert conditions

An exporter or an operator query should alert when:

- any operation is in `failed`;
- an operation has remained in `cancelling_billing` for more than 24 hours;
- a retention operation is due for purge but has not advanced;
- `lease_expiries{outcome="reclaimed"}` or `stale_compare_and_set{outcome="rejected"}` rises
  repeatedly;
- `purge_attempts{outcome="failed"}` rises.

The repository includes starter Prometheus rules in
[`ORGANISATION-DELETION-ALERTS.yml`](ORGANISATION-DELETION-ALERTS.yml). Tune the `for` durations
and notification channels to the deployment, and keep the rules fail-closed: they identify work
that needs the runbook but never bypass the billing observation gate or permit direct deletion of
an `organizations` row.

Example scrape configuration, with the token supplied by the secret manager rather than committed
to a file:

```sh
curl --fail --silent --show-error \
  -H "Authorization: Bearer $SOTTO_ORGANISATION_DELETION_METRICS_TOKEN" \
  https://<SOTTO_DOMAIN>/ops/organisation-deletion/metrics
```
