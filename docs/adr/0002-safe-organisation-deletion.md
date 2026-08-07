# Organisation deletion is an asynchronous retained workflow

Status: proposed for issue #77. Merging this ADR accepts the design, but does not enable an
organisation deletion endpoint.

## Context

The former `DELETE /orgs/{org_id}` route synchronously deleted the `organizations` row. Database
cascades then removed the organisation's memberships, projects, environments, secrets, grants,
machine tokens, and audit events. The route did not cancel an attached Stripe subscription first,
did not provide a recovery window, and could not distinguish a safe retry from a second destructive
request. It was removed in #76.

Organisation deletion crosses three systems with different failure modes: PostgreSQL, Stripe, and
managed backups. A successful database transaction is not proof that billing stopped, a webhook can
arrive late or out of order, and a backup can retain data after the primary database is purged. The
server also cannot retract ciphertext or keys that a member already downloaded.

## Decision

Organisation deletion will be an asynchronous lifecycle owned by a new `org_deletion` module. The
HTTP layer will request, inspect, or cancel that lifecycle. It will not delete organisation data
directly.

The public operation will be an explicitly confirmed `POST /orgs/{org_id}/deletion`, with separate
status and cancellation operations. A request atomically marks the organisation as deleting and
records an audit event. Existing reads remain available during the recovery window so members can
export their data, but organisation, billing, membership, grant, project, environment, secret, and
machine-token writes fail with `409 Conflict`.

The lifecycle will pass through persisted `requested`, `cancelling_billing`, `retention`, and
`purging` states before it becomes `completed`. `failed` records a fail-closed pause that an
idempotent retry can resume. An owner can move any pre-purge state to `recovering`; a fresh billing
lookup then settles it as `cancelled`. The normal recovery window is 30 days from the original
request. If billing was already cancelled, recovery restores the organisation on the free tier and
requires a new checkout for paid access.

Billing cancellation will sit behind a small `SubscriptionProvider` interface with production
Stripe and deterministic test adapters. A linked subscription must be authoritatively inactive or
absent before retention can advance to purge. Provider timeouts, authentication failures, unknown
statuses, and exhausted retries leave the organisation intact and write-frozen. Webhooks only wake
reconciliation; their arrival order is never treated as proof of the current subscription state.

Final purge will not delete the `organizations` row. It will clear the encrypted name, creator,
trial, and billing linkage, set the row to `deleted`, and explicitly remove the organisation's
memberships and project tree. The retained row is a minimal tombstone. It preserves the audit-event
foreign key, prevents identifier reuse, and gives stale billing events a terminal record to ignore.
The deletion workflow row and lifecycle audit events also remain. They contain identifiers and
sanitised state only, never encrypted names, secret material, provider payloads, or raw provider
errors.

Primary data is recoverable only until purge. Managed backups may retain the pre-purge ciphertext
until their configured lifecycle expires. The hosted deployment will keep its current approximately
30-day backup lifecycle, document the latest possible expiry in deletion status, and require the
restore runbook to reconcile deletion tombstones before traffic resumes. Self-hosters must configure
and honour their own backup lifecycle. Sotto cannot erase unmanaged copies, recipient share links,
or data and key material already downloaded by a client, so the product will not describe
organisation deletion as cryptographic erasure.

Only an owner can request, inspect, or cancel an active deletion. A non-member receives the same
`404 Not Found` as for an unknown organisation, while a member without the owner role receives
`403 Forbidden`. Repeated requests return the same active operation. Workers use database leases,
versioned compare-and-set transitions, and idempotent provider calls so concurrent requests,
multiple server instances, and webhook races cannot skip a safety gate.

The route will remain absent until the data model, lifecycle worker, billing adapter, access guards,
client recovery experience, integration tests, monitoring, and restore procedure described in
[the implementation specification](../ORGANISATION-DELETION.md) have landed.

## Consequences

- Deletion becomes slower and operationally more involved, but no single request or webhook can
  both cancel billing and destroy data.
- A paid organisation may be restored during retention, but its cancelled subscription is not
  silently recreated.
- Organisation identifiers and minimal lifecycle metadata are retained after purge. This is the
  smallest durable record needed to prevent identifier reuse and stale-event corruption.
- Read-only access during retention is deliberate. It provides export and recovery, but it means a
  deletion request is not immediate revocation.
- The existing cascade constraints remain useful for child cleanup, but application code will
  never again use deletion of the `organizations` row as the organisation purge operation.

## Rejected alternatives

- **A synchronous `DELETE` handler:** cannot safely span database, provider, retry, webhook, and
  backup boundaries.
- **Webhook-confirmed deletion:** delivery can be delayed, duplicated, or reordered. A fresh
  provider lookup is required before purge.
- **Immediate purge:** removes the recovery and export window and makes an accidental request
  irreversible.
- **Permanent soft deletion of all ciphertext:** avoids destructive work but does not satisfy a
  deletion request and grows retained sensitive data without a bound.
- **Stripe calls inside the HTTP handler:** couples the lifecycle to one provider and makes
  deterministic failure and race testing difficult.
