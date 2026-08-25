# Organisation-deletion recovery runbook

This runbook is for an operator who owns the Sotto deployment and its billing account. It is
deliberately fail-closed: do not edit lifecycle state in SQL, delete an `organizations` row by hand,
or bypass the billing observation gate. Every intervention must leave an audit event and a support
record.

## Before enabling deletion

- Configure a managed backup or export lifecycle that covers the configured recovery window.
- Set `SOTTO_ORGANISATION_DELETION_METRICS_TOKEN` in the deployment secret store. Do not put it in
  a scrape URL or an access-loggable command history entry.
- Keep `SOTTO_ORGANISATION_DELETION_OPERATOR_TOKEN` unset until the authenticated observation
  procedure below has been reviewed and rehearsed; it is a separate write-capable secret.
- Load [`ORGANISATION-DELETION-ALERTS.yml`](ORGANISATION-DELETION-ALERTS.yml) into the monitoring
  system and test one notification without using a real deletion.
- Run the backup script and restore the dump into an isolated scratch database. Complete the
  rehearsal record at the end of this document before enabling the client control.
- Confirm that the configured billing provider's API version, restricted key, and webhook endpoint
  match the [billing deployment settings](README.md#billing-optional).

## Enabling deletion

`SOTTO_ORGANISATION_DELETION_WORKER_ENABLED=1` enables the lifecycle worker and the owner-facing
deletion routes together; `VITE_ORGANISATION_DELETION_ENABLED=true` enables the client control.
The single server flag is deliberate: an instance can never accept a deletion request that no
worker will advance, which would freeze an organisation with nothing watching it.

Enable on a staging deployment first, verify it there, then repeat on production with the same
pinned image tag. The deployment steps and the verification list are in
[README.md](README.md#enabling-organisation-deletion). Do not enable production on the strength of a
staging result alone if the two deployments differ in billing configuration or image tag.

Until both flags are set, the deletion routes return `404` and the worker leaves the queue
untouched. Turning the flags back off restores that state for new requests, but it does not
un-purge an organisation and does not thaw one already inside its recovery window.

## Inspect a failed or delayed operation

First check the protected metrics endpoint without exposing the bearer token in shell output:

```sh
curl --fail --silent --show-error \
  -H "Authorization: Bearer $SOTTO_ORGANISATION_DELETION_METRICS_TOKEN" \
  https://<SOTTO_DOMAIN>/ops/organisation-deletion/metrics
```

For one operation, use a read-only Postgres session and select only the sanitised lifecycle fields:

```sql
SELECT id::text, org_id, state, requested_at, purge_after, next_attempt_at,
       attempt_count, resume_state, last_error_code, billing_observation_source,
       billing_checked_at, lease_owner, lease_expires_at
  FROM organization_deletions
 WHERE id = '<operation-uuid>';
```

`last_error_code` is a sanitised code. Raw provider response text is never stored in this table and
must not be copied into an incident or an owner-facing response. Check server logs for the request
correlation and the provider dashboard separately.

## A failed operation

The owner can repeat the original, explicit deletion request. The request is idempotent and, when
the operation is in `failed`, restores its recorded `resume_state`, clears the retry counter, and
creates `org.deletion.retry_requested`. Ask the owner to use the normal authenticated request with
the same organisation confirmation and subscription-cancellation acknowledgement.

Do not move `state`, `resume_state`, `next_attempt_at`, or `lease_owner` with SQL. If the owner cannot
repeat the request, keep the operation failed and open an incident until the authenticated operator
entrypoint is available.

## Billing credentials unavailable

An authentication or permission failure is `billing_unavailable` and must be fixed at the provider
configuration first. A self-hoster with a historical subscription may instead need an authoritative
operator observation. The internal lifecycle seam requires the exact subscription ID, a terminal or
missing status, an observation time no more than 15 minutes old at purge, an actor, a reason, and an
evidence reference such as a provider request or subscription URL.

This is an operational endpoint, not a public user route, and it stays separate from the
owner-facing deletion API whether or not deletion is enabled. Set
`SOTTO_ORGANISATION_DELETION_OPERATOR_TOKEN` only after this procedure has been reviewed and
rehearsed. Record the exact values from the provider dashboard or request log without putting the
bearer token in shell history:

```sh
read -r -s OPERATOR_TOKEN
printf '\n'
curl --fail --silent --show-error -X POST \
  -H "Authorization: Bearer ${OPERATOR_TOKEN}" \
  -H "Content-Type: application/json" \
  "https://<SOTTO_DOMAIN>/ops/organisation-deletion/<org-id>/billing-observation" \
  -d '{"operator":"<operator-id>","subscription_id":"<subscription-id>","observed_status":"canceled","observed_at":"<RFC3339-time>","reason":"<why-provider-was-unavailable>","evidence":"<provider-request-or-subscription-url>","managed_backup_expiry_by":"<RFC3339-backup-expiry>"}'
unset OPERATOR_TOKEN
```

The endpoint remains unavailable with `503` until its dedicated token is configured. Never write the
billing fields directly or reuse the metrics token for this write-capable control. Omit
`managed_backup_expiry_by` when no managed lifecycle has been configured; a supplied expiry must be
after the operation's recovery deadline.

## Cancel before purge

An owner may cancel through the normal endpoint while the operation is before `purging`:

```sh
curl --fail --silent --show-error -X POST \
  -H "Authorization: Bearer <owner-session-token>" \
  "https://<SOTTO_DOMAIN>/orgs/<org-id>/deletion/cancel"
```

The worker reconciles the billing provider after the cancellation request. Do not assume a local `recovering`
state means Team access has already been restored, and do not resume collection for a purged
organisation.

## A failed purge

1. Capture the operation ID, sanitised error code, state version, and the alert timestamp.
2. Confirm that the managed backup or export covering `purge_after` still exists.
3. Check the provider status and freshness gate. If the provider is unavailable, use the reviewed
   operator-observation path above rather than clearing the gate.
4. Ask the owner to retry the recorded resume state if the operation is recoverable. A purge that
   has started is not recoverable; preserve the tombstone and escalate instead.
5. Do not run `DELETE FROM organizations` or a hand-written cascade. The lifecycle transaction is
   the only code allowed to remove the ciphertext tree and retain the tombstone and audit history.

## Isolated restore rehearsal

Use [`backup.sh`](backup.sh) and the restore procedure in [`README.md`](README.md) against a scratch
database, never the live database. Verify that:

- migrations apply cleanly;
- deletion rows, tombstones, audit events, and metric counters are present;
- the restored server remains healthy with both deletion flags disabled;
- the deletion routes return `404` on that restored server, confirming the gate holds after a
  restore; and
- the metrics endpoint remains `503` when its token is absent.

Record the rehearsal before enablement:

| Field | Value |
| --- | --- |
| Date and operator | _fill in_ |
| Backup object and checksum | _fill in_ |
| Scratch database | _fill in_ |
| Restore command and result | _fill in_ |
| Deletion and metrics checks | _fill in_ |
| Follow-up or rollback note | _fill in_ |
