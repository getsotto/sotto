# Deploying Sotto

One command brings up a complete hosted instance: Postgres, the sync server, and Caddy serving
the web app with automatic HTTPS. By default it pulls prebuilt multi-arch (amd64 + arm64) images
from GHCR, so the host never compiles anything - a 1 GB free-tier VM with Docker and ports 80/443
open is enough.

```text
internet ──▶ caddy (80/443, web app + API reverse proxy)
                 │ internal network only
                 ├──▶ server (axum, ciphertext-only API)
                 └──▶ ─┘ postgres (named volume)
```

The web app and API share **one origin** (`https://<SOTTO_DOMAIN>`), so the session cookie and
CSP stay same-origin and no CORS is involved. The server stores only ciphertext plus minimal
metadata - see [THREAT-MODEL.md](../THREAT-MODEL.md) - so the box hosts nothing that can decrypt
your secrets; still, treat it as production infrastructure.

## Prerequisites

1. **A host** with Docker + Docker Compose, ports 80 and 443 reachable from the internet.
2. **DNS**: an A (and/or AAAA) record for your domain pointing at the host. Caddy provisions the
   TLS certificate automatically once the name resolves.
3. **A GitHub OAuth app** (github.com → Settings → Developer settings → OAuth Apps → New) with
   the authorization callback URL set to exactly:

   ```text
   https://<your-domain>/auth/github/callback
   ```

## First deployment

```sh
git clone https://github.com/getsotto/sotto.git && cd sotto/deploy
cp .env.example .env
$EDITOR .env        # domain, a generated Postgres password, OAuth client id + secret

docker compose -f docker-compose.prod.yml pull
docker compose -f docker-compose.prod.yml up -d
```

`SOTTO_DEPLOYMENT_MODE` identifies who operates the instance and defaults to `self_hosted`.
Set it to `cloud` on the operated Sotto service. Stripe credentials with `self_hosted` are rejected
at boot so a deployment cannot silently lose billing or accidentally apply the wrong entitlement
policy. Check the value after deployment with:

```sh
curl -fsS https://<your-domain>/server/info
```

The response is safe to expose publicly and reports the deployment mode plus the current
entitlement model. Stripe configuration must be paired with `cloud`; the mode remains an explicit operator setting.

New hosted purchases have a separate default-off switch, `SOTTO_CLOUD_SALES_ENABLED=0`. Turning it
off pauses new quotes and checkout sessions while existing billing portals, cancellation, refund
handling, and verified webhook settlement remain available. Enabling it requires Cloud mode, all
Stripe credentials, the complete four-price hosted catalogue, and a safe Customer Portal
configuration. It does not authorise a launch by itself. Operator procedures and deployment
controls are maintained separately from the public deployment guide.

Hosted catalogue variables are `STRIPE_STANDARD_MONTHLY_PRICE_ID`,
`STRIPE_STANDARD_ANNUAL_PRICE_ID`, `STRIPE_FOUNDING_MONTHLY_PRICE_ID`, and
`STRIPE_FOUNDING_ANNUAL_PRICE_ID`. Compose passes them separately from the legacy
`STRIPE_PRICE_ID`. The catalogue must be complete; partial configuration fails at boot.
Founding prices remain unavailable unless `SOTTO_FOUNDING_SCHEDULE_WORKER_ENABLED=1`; the switch
defaults to `0` and requires a complete hosted catalogue plus
`SOTTO_FOUNDING_SCHEDULE_PREFLIGHT_VERIFIED=1`. Keep both switches at `0` until the founding
interval-change protection, safe Customer Portal configuration, and complete schedule and renewal
flow have passed a Stripe test-mode rehearsal. The worker requires
`STRIPE_CLOUD_PORTAL_CONFIGURATION_ID` even when new sales are off. Set the preflight value to `1`
only after recording that evidence. The worker switch does not enable Cloud sales by itself.
Founding price-change notices have their own default-off `SOTTO_FOUNDING_PRICE_NOTICES_ENABLED`
switch. It requires Cloud mode, the founding schedule worker, the hosted catalogue, and the
protected billing operator token so late or unresolved notice cohorts can be reviewed. It creates
only in-app notices after a schedule has been verified; it does not send email or change billing.

Create a dedicated Stripe Customer Portal configuration for Cloud and set its id in
`STRIPE_CLOUD_PORTAL_CONFIGURATION_ID`. Enable payment-method updates and period-end cancellation
with no cancellation proration, and disable subscription updates. Sotto retrieves and checks this
configuration before issuing Cloud quotes and checkout sessions, and passes it to Cloud portal
sessions when configured. This applies the server owned price catalogue consistently: customers
can't switch to a plan that Sotto doesn't track for invoice verification. When the variable is
unset, portal sessions keep Stripe's default configuration as before. The configured portal allows
payment-method updates and period-end cancellation, with subscription changes disabled.
This applies to all Cloud customers, including existing organisation subscriptions that use the
legacy Team billing path; those customers also receive period-end cancellation and cannot change
plans in the portal once this configuration is set.

Stripe does not allow customers to update or cancel a subscription while a subscription schedule
is attached. Sotto's founding transition worker attaches such a schedule, so this portal setting
does not provide a cancellation path for a schedule-managed founder. Keep the worker disabled until
Sotto's cancellation route can safely end a scheduled subscription and that flow has passed a
Stripe test-mode rehearsal. The id is not a secret; the API key needs read access to Customer
Portal configurations and write access to create portal sessions.

Hosted paid-invoice coverage intake and its refresh worker are staged separately and default off:
`SOTTO_PROVIDER_REFRESH_INGEST_ENABLED=0` and `SOTTO_PROVIDER_REFRESH_WORKER_ENABLED=0`. These
controls apply only to the hosted billing deployment; self-hosted installations can leave them
disabled. For the current Stripe integration, enabling either control requires `STRIPE_ACCOUNT_ID`,
a test or live restricted API key, and the complete hosted price catalogue. Startup verifies the
provider account and configured prices before enabling either path. Intake accepts only verified,
fully paid personal subscription invoices with a matching durable allocation reference; its source
registration, invoice receipt, invalidation, and queued refresh are committed together. The worker
reads bounded provider history outside database transactions and publishes coverage only after the
prepared collection still matches its registered source set. An invoice received before its
Checkout subscription is linked is retried without recording a receipt. Neither switch enables
sales, reconciliation, or Cloud action enforcement; leave these values disabled until the hosted
provider lifecycle has been exercised and reviewed.

Coverage authority is a separate opt-in. `SOTTO_PROVIDER_AUTHORITY_REVALIDATION_ENABLED=1` starts
the durable full-source scheduler; set `SOTTO_CLOUD_COVERAGE_AUTHORITY_MAX_AGE_SECONDS` to a
positive freshness window between 60 seconds and 400 days. New and existing registered sources
are queued independently of Stripe event ids, and a complete, generation-matched collection is
required before it can be treated as current. Existing projections without an authority receipt
remain unavailable to strict authority checks until revalidated. `SOTTO_CLOUD_COVERAGE_AUTHORITY_SHADOW=1`
uses that result in non-blocking policy observations. Retention always requires current authority
and holds candidates when it is missing or stale. Keep
`SOTTO_CLOUD_COVERAGE_AUTHORITY_ENFORCEMENT=0` until the annual/no-event, mixed-source, invalidation,
and restart cases have passed the pinned-version rehearsal. Enabling shadow or enforcement without
scheduled revalidation is rejected at startup.

Human hosted action checks are shadow-only by default. They record would-deny decisions while
existing ACL, grant, lifecycle, and export responses remain unchanged. Do not set
`SOTTO_CLOUD_ACTION_ENFORCEMENT=1` until the Cloud transition and export gates have been
rehearsed and approved.

Hosted machine-token eligibility is separately dormant. Do not set
`SOTTO_MACHINE_ELIGIBILITY_ENFORCEMENT=1` until the accountable-beneficiary migration and legacy
token inventory have been reviewed; self-hosted machine access does not depend on Cloud billing.

Database migrations run automatically on server boot. Pin a released version with
`SOTTO_IMAGE_TAG=vX.Y.Z` in `.env` (default: `latest`). To build everything from source instead -
for unreleased changes, or if you'd rather not trust prebuilt images - use
`up -d --build`; that needs ~4 GB of RAM and takes several minutes the first time.

Organisation deletion ships disabled. `SOTTO_ORGANISATION_DELETION_WORKER_ENABLED=1` turns on both
halves of the server side at once - the lifecycle worker and the owner-facing deletion routes - and
`VITE_ORGANISATION_DELETION_ENABLED=true` turns on the client control. The default prebuilt images
and source builds keep every side unavailable. Enabling it is a deliberate procedure with
prerequisites, not a single switch: follow
[Enabling organisation deletion](#enabling-organisation-deletion) below.

New deletion requests use a 30-day recovery window by default. Set
`SOTTO_ORGANISATION_DELETION_RETENTION_DAYS` to an integer from 1 to 365 in `deploy/.env` to change
the window for new requests. The organisation stays frozen for the whole configured window.
Changing it never shortens an existing operation's stored `purge_after` deadline.
Choose a value that covers the managed-backup and export lifecycle: backups taken before purge can
retain ciphertext after the recovery window, and self-hosted operators must remove unmanaged copies.

Personal Cloud expiry review is separately disabled by default. `SOTTO_CLOUD_RETENTION_DRY_RUN_ENABLED=1`
starts a bounded reconciler that snapshots only explicitly personal resources and validates them
after the person's existing export deadline. It never deletes resources, writes deletion receipts or
tombstones, or enables the retention purge path. Keep it off until a current-candidate backup and
tombstone restore has been verified and the customer-notice evidence has been reviewed.

Shared organisation expiry review has a separate default-off switch:
`SOTTO_CLOUD_SHARED_RETENTION_DRY_RUN_ENABLED=1`. It scans organisations whose current owners and
admins have all lost Cloud eligibility, queues an in-app export notice for each, and starts the
30-day shared-data export window no earlier than when those notices are first queued. The worker
snapshots shared scope and performs validation-only dry runs; it cannot purge shared data. Keep this
switch off until the shared-scope evidence, customer notices, backup and tombstone restore have been
reviewed. Enabling personal expiry review does not enable the shared scanner.

The protected `/ops/cloud/retention` inspection and action routes are mounted only when
`SOTTO_CLOUD_RETENTION_OPERATOR_TOKEN` is configured. Use a dedicated value of at least 32
characters; do not reuse a billing, deployment, or metrics token. The surface returns fixed reason
codes, counts, deadlines, and opaque job fingerprints. `hold`, `cancel`, and `retry_dry_run` never
enable purge; an operator-supplied label is audit text, not an authenticated identity. A successful
dry run is review evidence, not permission to delete data. The purge route and worker have a
separate default-off gate: `SOTTO_CLOUD_RETENTION_PURGE_ENABLED=1`. They also require a distinct
destructive operator token, a separate evidence HMAC key, and the exact running build SHA. The
server binary embeds its build SHA, and startup rejects a runtime SHA that differs from that
compiled identity. Published images embed the GitHub commit that built them. For a source build,
set `SOTTO_BUILD_SHA` to the checked-out commit before building and set
`SOTTO_CLOUD_RETENTION_BUILD_SHA` to the same value at runtime. A source build without embedded
identity cannot mount or run purge controls. The server refuses incomplete configuration and
non-Cloud deployment mode. Keep this switch off
until the current retention policy, customer notices, exact candidate scope, backup and tombstone
restore rehearsal have all been independently reviewed. Enabling it does not delete anything by
itself: a reviewer must register signed restore evidence, inspect the opaque scope fingerprint,
and approve the exact dry-run candidate. An `operator_hold` can be lifted only by explicitly
approving the current scope with fresh restore evidence. Before each bounded batch the worker
rechecks the live approval, scope, eligibility, notice and restore-evidence journal count. A
successful batch adds tombstones, invalidates that approval, and holds the remaining work for a
fresh restore rehearsal and approval. The destructive operator label is audit text, not an
authenticated individual; use the credentials only through the protected operator channel and do
not place them in logs or support messages.

Cloud lifecycle observability has its own optional credentials. See
[`cloud-operations.md`](cloud-operations.md) for the generic protected routes and
their separation from liveness, readiness, customer retrieval, payment-to-access acceptance and
destructive retention. Leave the metrics and coverage-repair credentials blank until the scraper
and private operator procedure are configured; never reuse the retention, billing-review or purge
credentials.

The signed request is produced offline from the matching backup and sidecar after restoring them
to a dedicated local database. Name that database `retention_restore_<name>`, export its URL as
`SOTTO_RETENTION_REHEARSAL_DATABASE_URL` (never pass it as a command-line argument), and materialise
the evidence HMAC key from the secret store in a file readable only by the operator. From the exact
release checkout, run:

```sh
scripts/retention-restore-rehearsal \
  --backup ./sotto-<stamp>.dump \
  --journal ./sotto-<stamp>.tombstones.jsonl \
  --evidence-key-file ./retention-evidence.key \
  --release-sha "$(git rev-parse HEAD)" \
  --operator "<operator label>" \
  --output ./restore-evidence-request.json
```

The command requires a local scratch database and key file mode `0600`; it validates the restored
schema before and after replay, runs the matching sidecar twice, checks that the second replay is a
no-op, and tests stale-incarnation guards against temporary rows in the scratch database. It writes
the exact JSON body for `/ops/cloud/retention/purge/restore-evidence` with mode `0600`. Review and
submit that body through the protected operator channel. The script does not contact the live
service or register evidence itself; the evidence key must match the running build's configured
key, and the release SHA must match that build.

On a 1 GB host, give the kernel some headroom before the first start:

```sh
sudo fallocate -l 2G /swapfile && sudo chmod 600 /swapfile
sudo mkswap /swapfile && sudo swapon /swapfile
echo '/swapfile none swap sw 0 0' | sudo tee -a /etc/fstab
```

Smoke test:

```sh
curl -fsS https://<your-domain>/health          # → ok    the process is up
curl -fsS https://<your-domain>/health/ready    # → ok    and it can reach Postgres
```

The second is the one that catches a first deployment wired to the wrong database: the process
starts and serves `/health` regardless, so only the readiness probe fails when `DATABASE_URL` is
wrong. See [Uptime monitoring](#uptime-monitoring).

Then open `https://<your-domain>` in a browser and sign in with GitHub. Point the CLI at your
instance with `sotto login --server https://<your-domain>`.

## Legacy billing transition

Before changing an existing hosted account, run the read-only inventory and rehearsal in
[`LEGACY-BILLING-CUTOVER-RUNBOOK.md`](./LEGACY-BILLING-CUTOVER-RUNBOOK.md). It reports aggregate
legacy paid, trial, manual and contradictory cohorts plus token and link provenance without
printing customer or provider identifiers. The inventory does not charge, backfill, revoke or
change entitlements; an approved cohort and notice decision is required before a later cutover.

## Upgrading

```sh
git pull    # picks up compose/runbook changes
docker compose -f docker-compose.prod.yml pull
docker compose -f docker-compose.prod.yml up -d
```

Migrations are forward-only and applied on boot. Check the release notes for anything flagged as
a compatibility break before upgrading past a minor version.

**Renamed settings are the one thing `git pull` cannot fix for you.** Your `.env` is deliberately
untracked, so a variable renamed upstream leaves the old name sitting in your file where nothing
reads it, and the new name unset. Compose substitutes an empty value, the server treats the setting
as absent, and the feature ships dark rather than failing loudly.

Running containers keep their environment until `up -d` recreates them, so the moment to check is
after `git pull` has refreshed `.env.example` and before `up -d`. In that window, compare the
setting names in your `.env` against the fresh template (only names reach the temporary files,
never values):

```sh
sed -n 's/^\([A-Z_][A-Z_0-9]*\)=.*/\1/p' .env | sort > /tmp/env.mine
sed -n 's/^\([A-Z_][A-Z_0-9]*\)=.*/\1/p' .env.example | sort > /tmp/env.upstream
diff /tmp/env.mine /tmp/env.upstream; rm -f /tmp/env.mine /tmp/env.upstream
```

Lines marked `<` are settings nothing reads any more, which usually means a rename to apply to your
`.env`; lines marked `>` are ones you have not set, most of which are optional. The known rename:
`STRIPE_SECRET_KEY` became `STRIPE_API_KEY`. Upgrading across it with the old name in place leaves
billing unconfigured, so `POST /billing/webhook` answers `503` instead of `401` and providers
eventually disable an endpoint that keeps failing. Apply the rename before `up -d`, not after.

**Machine tokens now expire.** Earlier versions let a machine token live until someone revoked it.
The first upgrade to a version with expiry gives every existing token 90 days, counted from the
moment the migration runs rather than from when the token was made, so nothing stops working on the
day you upgrade. After that, each token fails exactly as a revoked one does. Run `sotto token ls` in
each project that has CI to see the end dates, and replace a token before its date by creating a new
one, updating the CI secret, and revoking the old one. Machine-mode runs also print a warning in the
CI log once a token has less than two weeks left.

## Backups

Postgres holds only ciphertext and metadata, but losing it loses your users' synced vaults.
[`backup.sh`](./backup.sh) takes a custom-format `pg_dump` inside the container, **verifies the
archive** (`pg_restore --list`) before anything leaves the box, and uploads it to whatever
object storage `SOTTO_BACKUP_BUCKET` names - the scheme picks the tool:

It also uploads `sotto-<stamp>.tombstones.jsonl`, a cumulative sidecar containing the retention
deletion journal. The sidecar is deliberately separate from the dump: a dump taken before a
purge must not resurrect ciphertext when it is restored. It contains resource identifiers and
ownership checks, never ciphertext or secret values, and is validated before upload by
[`replay-retention-journal`](../scripts/replay-retention-journal). Keep both objects under the
same bucket lifecycle policy; a dump without its sidecar is not a restorable backup.

| `SOTTO_BACKUP_BUCKET` | Uploads with | Works for |
|---|---|---|
| `gs://<bucket>` | `gsutil` | Google Cloud Storage |
| `s3://<bucket>` | `aws s3 cp` | S3 and S3-compatibles |
| `<remote>:<path>` | `rclone` | 40+ backends: B2, SFTP, a NAS, … |

One-time setup, any provider:

1. **A bucket that deletes objects after ~30 days.**

   ```sh
   # Google Cloud:
   gcloud storage buckets create gs://<bucket> --location=<region>
   printf '{"rule":[{"action":{"type":"Delete"},"condition":{"age":30}}]}' > /tmp/lifecycle.json
   gcloud storage buckets update gs://<bucket> --lifecycle-file=/tmp/lifecycle.json

   # AWS:
   aws s3 mb s3://<bucket> --region <region>
   aws s3api put-bucket-lifecycle-configuration --bucket <bucket> --lifecycle-configuration \
     '{"Rules":[{"ID":"expire","Status":"Enabled","Filter":{},"Expiration":{"Days":30}}]}'
   ```

2. **Append-only credentials for the host.** The box should be able to add backups but never
   read or delete them - a compromised host then can't destroy or exfiltrate your history. Two
   subtleties make this stricter than it looks on GCS:

   - `roles/storage.objectCreator` alone cannot even upload: `gsutil cp` checks whether the
     destination is a "directory" first, and that check is a list operation. Pair it with
     `roles/storage.legacyBucketReader`, which adds exactly `storage.objects.list` and
     `storage.buckets.get` - object names and bucket metadata, nothing that reads contents. (`gcloud storage cp` does not help here - it stats the destination
     object before writing, which needs `storage.objects.get`, the very permission to withhold.)
   - On a bucket with fine-grained ACLs, the uploader is granted owner on every object it
     creates and can read its own uploads back regardless of IAM. Enable uniform bucket-level
     access so IAM alone decides.

   ```sh
   # Google Cloud:
   gcloud storage buckets update gs://<bucket> --uniform-bucket-level-access
   gcloud storage buckets add-iam-policy-binding gs://<bucket> \
     --member="serviceAccount:<vm-service-account>" --role="roles/storage.objectCreator"
   gcloud storage buckets add-iam-policy-binding gs://<bucket> \
     --member="serviceAccount:<vm-service-account>" --role="roles/storage.legacyBucketReader"
   ```

   The host can then list backup names but read none of them; the posture check at the end of
   this section proves it once everything is configured.

   On AWS, an IAM policy allowing only `s3:PutObject` on the bucket serves the same purpose.
   Restore rehearsals fetch the dump with your own credentials on another machine, never on the
   host - by design, the host can no longer read what it wrote.

3. **Point the script at it** (`deploy/.env`): `SOTTO_BACKUP_BUCKET=gs://<bucket>` (or the
   `s3://` / rclone form).

4. **Nightly cron**, with failures landing in a log you can check:

   ```sh
   crontab -e     # add:
   # 17 2 * * * cd $HOME/sotto/deploy && ./backup.sh >> $HOME/sotto-backup.log 2>&1
   ```

5. **Watch that it keeps running.** Cron reports to nobody, and a bucket that has stopped
   receiving objects looks exactly like a bucket nobody has checked. Installing step 4 and
   assuming it worked is how an instance ends up with months of no backups and no symptom.

   The check is one question, and it is the same on every object store: **is the newest object
   younger than 26 hours?** That threshold exceeds a full day on purpose, so a single missed night
   breaches it rather than hiding inside it. Run it anywhere except the host, because a host that
   has stopped taking backups cannot be relied on to report that it has. Have it ping a heartbeat
   URL when it passes, so any external checker (e.g. a free UptimeRobot heartbeat monitor) alerts
   you when the pings stop; alerting only on failure cannot report the checker's own death, which
   is the failure that hides longest.

   [`backup-freshness.yml`](../.github/workflows/backup-freshness.yml) is that check as a worked
   example, for `gs://` destinations, running daily on GitHub Actions. It lists object names and
   creation times, never fetches a backup, and the identity it uses holds no permission that would
   let it, so the append-only posture above is unaffected.

   Configure it with repository variables `SOTTO_BACKUP_BUCKET` (the same value as in `.env`,
   scheme included), `GCP_WORKLOAD_IDENTITY_PROVIDER` and `GCP_MONITOR_SERVICE_ACCOUNT`, plus a
   repository secret `BACKUP_HEARTBEAT_URL`. Until all three variables are set the job skips
   rather than failing.

   Grant the identity by federation rather than by issuing a key, so that no long-lived credential
   exists to leak or rotate:

   ```sh
   # Google Cloud:
   # Federating the identity is not enough on its own. Exchanging the OIDC token for one the
   # service account can use goes through this API, so a project that has never enabled it fails
   # at that last hop only, long after the pool and the binding both look correct.
   gcloud services enable iamcredentials.googleapis.com

   gcloud iam workload-identity-pools create github --location=global
   gcloud iam workload-identity-pools providers create-oidc github \
     --location=global --workload-identity-pool=github \
     --issuer-uri="https://token.actions.githubusercontent.com" \
     --attribute-mapping="google.subject=assertion.sub,attribute.repository=assertion.repository" \
     --attribute-condition="assertion.repository=='<owner>/<repo>'"

   gcloud iam service-accounts create sotto-backup-monitor
   gcloud storage buckets add-iam-policy-binding gs://<bucket> \
     --member="serviceAccount:sotto-backup-monitor@<project>.iam.gserviceaccount.com" \
     --role="roles/storage.legacyBucketReader"
   gcloud iam service-accounts add-iam-policy-binding \
     sotto-backup-monitor@<project>.iam.gserviceaccount.com \
     --role="roles/iam.workloadIdentityUser" \
     --member="principalSet://iam.googleapis.com/projects/<project-number>/locations/global/workloadIdentityPools/github/attribute.repository/<owner>/<repo>"
   ```

   **The attribute condition is the security boundary, not a filter.** Without it the provider
   trusts every GitHub Actions token in existence, so any repository anywhere could assume this
   identity. `legacyBucketReader` is deliberate too: it grants `storage.objects.list` and
   `storage.buckets.get` and nothing that reads an object, which is all a freshness check needs.

   **Any other store.** The workflow skips cleanly unless the destination is `gs://`, so nothing
   is ever half configured without saying so. Because `rclone` reaches every backend `backup.sh`
   can write to, one command answers the question for all of them, from any scheduler that can
   send a heartbeat afterwards:

   ```sh
   # Anything rclone reaches - gs://, s3://, B2, SFTP, a NAS:
   rclone lsjson --max-age 26h "$SOTTO_BACKUP_BUCKET" | jq -e 'length > 0'

   # AWS, without rclone:
   aws s3api list-objects-v2 --bucket <bucket> \
     --query 'max_by(Contents, &LastModified).LastModified'
   ```

   A non-zero exit is the alarm. On AWS the federated equivalent of the grant above is registering
   GitHub's OIDC issuer as an identity provider and giving the role a trust policy conditioned on
   the repository, with `s3:ListBucket` and nothing else.

**Restore** (into a running instance; drops and recreates objects from the dump). Fetch the
dump on your own machine, never the host - the append-only posture means the host cannot read
what it wrote. One consequence of uniform bucket-level access is easy to miss: it disables the
object ACLs that basic project roles relied on, so until you grant yourself read explicitly,
nobody at all can fetch a backup - project owner included. Grant it once:

```sh
# Google Cloud:
gcloud storage buckets add-iam-policy-binding gs://<bucket> \
  --member="user:<your-account>" --role="roles/storage.objectViewer"
```

Then fetch with your provider's tool (`gsutil cp` / `aws s3 cp` / `rclone copyto`) and restore:

```sh
docker compose -f docker-compose.prod.yml exec -T postgres \
  pg_restore -U sotto -d sotto --clean --if-exists < sotto-<stamp>.dump
docker compose -f docker-compose.prod.yml restart server
```

Run one backup by hand now and verify all three properties of the append-only posture from the
host - the upload must succeed and both refusals must appear, because an unverified posture and a
working one look identical from the outside:

```sh
./backup.sh                                      # upload succeeds
gsutil cat gs://<bucket>/sotto-<stamp>.dump      # AccessDenied: needs storage.objects.get
gsutil rm gs://<bucket>/sotto-<stamp>.dump       # AccessDenied: needs storage.objects.delete
```

Then rehearse the restore once against a scratch database - a backup that has never been restored
is a hope, not a backup. Fetch the matching `.tombstones.jsonl` sidecar as well as the dump and,
after the dump is restored, replay it before starting the server:

```sh
scripts/replay-retention-journal \
  --database-url postgres://sotto@localhost:5432/restored \
  --journal sotto-<stamp>.tombstones.jsonl
```

The replay is idempotent and checks owner, creation time and environment revision before each
delete. Do not admit traffic or start workers until the restore checks and journal replay have
both succeeded.

## Access logs

Caddy writes JSON access logs to the `caddy_logs` volume (`/var/log/caddy/access.log` in the
container), rotated at 50 MiB, 10 files kept, 90 days retained (the `log` block in the
`Caddyfile`). Credential headers (`Cookie`, `Authorization`, `Proxy-Authorization`,
`Set-Cookie`) and the `Location` response header are **deleted from every entry by an
explicit filter in the `Caddyfile`** - not left to Caddy's default redaction - so no
session material ever reaches disk. Request paths and statuses are logged.

The number that matters for a hosted instance - free-tier limit hits (HTTP 402, one per person
who wanted more than the free tier allows):

```sh
docker compose -f docker-compose.prod.yml exec caddy \
  sh -c 'grep -c "\"status\":402" /var/log/caddy/access.log'
```

## Uptime monitoring

Point the monitor at **`GET /health/ready`**, not at `/health`. Both are unauthenticated and both
return `ok` with a `200`, and the difference only shows up on the day it matters:

| Path            | Answers                                   | Reports a database outage |
| --------------- | ----------------------------------------- | ------------------------- |
| `/health`       | is this process running?                  | no                        |
| `/health/ready` | can this instance serve a request?        | yes, `503 unavailable`    |

Postgres holds every secret, session and project, so an outage there fails every real request.
`/health` never touches it, and a checker watching that path stays green for the whole outage. The
readiness probe makes an explicit round trip to the database and returns `503` with the body
`unavailable` when it cannot.

Configure any external checker against `https://<SOTTO_DOMAIN>/health/ready` (e.g. a free
UptimeRobot monitor, 5-minute interval, keyword `ok`). Alerting from *outside* the box is the
point: a dead VM cannot report itself.

Two properties are worth knowing before tuning the interval. The verdict is cached for a second
and concurrent checks share one query, so probing more often costs the database nothing extra but
also tells you nothing extra. And the check gives up after five seconds, so a database that hangs
rather than refuses is reported as unavailable rather than holding the request open.

`/health` is still there and still worth a second monitor if you want to tell "the box is gone"
apart from "the box is up and the database is not". Keep it pointed at `/health` for that
distinction to mean anything.

### The checks that watch the checks

Everything else here runs on GitHub or in your cloud project. That is fine for doing the work and
useless for noticing that the work stopped: a job that is no longer scheduled reports nothing, and
nothing is exactly what a healthy job also reports. The external checker is the only layer that
survives its own subject being gone, so it owns two jobs that nothing else can do. It watches the
deployment, and it watches the watchers.

Each scheduled job pings a URL on success. The checker alarms when the pings **stop**, which is
the inversion that matters: a job that fails, a job that is disabled, a job whose credentials
expired and a job somebody deleted all look identical from inside, and identical to silence from
outside. Silence is the signal.

Five monitors, whichever checker you use (a free UptimeRobot account covers all five, but nothing
here depends on it):

| Monitor          | Kind      | Target                                   | Grace  |
| ---------------- | --------- | ---------------------------------------- | ------ |
| API              | keyword   | `https://<SOTTO_DOMAIN>/health/ready`, `ok` | 5 min  |
| Web app          | HTTP      | `https://<SOTTO_DOMAIN>/`                | 5 min  |
| Backup freshness | heartbeat | `BACKUP_HEARTBEAT_URL`                   | 36 h   |
| Status collector | heartbeat | `STATUS_HEARTBEAT_URL`                   | 12 h   |
| Restore drill    | heartbeat | `RESTORE_HEARTBEAT_URL`                  | 40 days |

**The grace periods are derived, not chosen, and the derivation is the point.** Scheduled
workflows on GitHub are queued rather than guaranteed, and on this deployment they arrive hours
after their cron. Set a heartbeat tighter than the observed lateness and it pages you for a queue
you do not control, which is how a pager gets ignored, and an ignored pager is worse than none.

- **Backup freshness** asks for 09:43 daily and has arrived 4.1, 4.2 and 5.2 hours late on three
  consecutive days. Consecutive pings can therefore be 24 h plus that lateness apart, close to
  30 h, so the grace has to clear it. 36 h is the first comfortable number above.
- **The status collector** asks for every ten minutes and manages about eight runs a day, with
  gaps between runs of up to 4.6 h. 12 h leaves headroom without waiting a full day to tell you
  it has died.
- **The restore drill** runs monthly, so anything over 31 days plus lateness works.

Re-measure these if the schedules change. The numbers above are what this deployment actually
observed, not what its crons request, and the difference between those two is the whole reason
this section exists.

### What each alarm means

- **API or web app down**: users are affected now. The status page will show it at its next
  sample, which is slower than this alert; the alert is the one to act on.
- **Backup heartbeat stopped**: nobody is checking that backups arrive. The backups themselves may
  be fine. Run the workflow by hand to find out which.
- **Collector heartbeat stopped**: the status page is going stale and will keep publishing its
  last good data without saying so.
- **Restore heartbeat stopped**: the monthly drill is not running, which means the backups are
  once again unverified, which is the state this whole section was built to leave.

### Linking it from the app

Set the repository variable `VITE_STATUS_URL` to the page's address and the landing footer gains
a **Status** link on the next image build. It is empty by default and the link is simply absent
when unset, because sending a self-hosted deployment's users to somebody else's uptime page
would be worse than sending them nowhere.

That variable reaches the build through `images.yml`, along with `SOTTO_PUBLIC_URL` and
`VITE_ORGANISATION_DELETION_ENABLED`. Those had been declared in `deploy/Dockerfile.web` and
passed by nothing, so every published image took the defaults and the only way to change one was
to build on the host.

### Setting it up

1. Create the account and the first two monitors. The web app one is a plain HTTP check; the API
   one must be a **keyword** check matching `ok`, and the difference is not cosmetic. A plain HTTP
   check passes on any `200`, including the single-page app answering because the deployment has
   no route for `/health/ready`, which is a real state this deployment has been in. The keyword is
   what tells the readiness endpoint apart from the page that replaced it.

   Confirm alerts go somewhere you will actually see: an email address you read, or the checker's
   mobile app. Avoid SMS or voice if the provider charges for them separately, since that is the
   one way this arrangement costs money.
2. Create the three heartbeat monitors and copy their ping URLs.
3. Set them as repository secrets, which is where the workflows read them from:

```sh
gh secret set BACKUP_HEARTBEAT_URL  --body '<url>'
gh secret set STATUS_HEARTBEAT_URL  --body '<url>'
gh secret set RESTORE_HEARTBEAT_URL --body '<url>'
```

4. **Prove each one arrives**, rather than assuming. Dispatch each workflow and watch its monitor
   turn green:

```sh
gh workflow run backup-freshness.yml
gh workflow run status-collector.yml
gh workflow run backup-restore.yml
```

A heartbeat nobody has seen arrive is indistinguishable from one that never will, and it is
supposed to be the thing that catches everything else, so it is the worst place in this system to
take on trust. Until each URL is set, the workflows say on their own logs that nothing is watching
them, which is honest and no substitute.

**In a fork, change the repository guards before doing this.** All three workflows carry
`if: github.repository == 'getsotto/sotto'`, so a dispatch in a fork succeeds, skips the job and
sends nothing. The run goes green and the heartbeat never arrives, which reads exactly like a
broken heartbeat and is not one. Point each guard at your own repository, or drop the line; it is
there so a fork does not start probing somebody else's deployment merely by existing.

## Restore verification

A backup nobody has restored is a hope. `backup.sh` validates each archive with
`pg_restore --list` before upload, which proves the file is not truncated and nothing more. It
cannot tell you the bytes survived the trip to the bucket, and it cannot tell you that what
comes back is a database this code could run on. Only restoring one answers those.

`deploy/restore-verification.yaml` does it monthly: fetch the newest dump and newest cumulative
sidecar, restore the dump into a throwaway Postgres that dies with the build, check what came back,
then replay the sidecar before the heartbeat is sent.

Where the dump goes is worth stating rather than leaving to inference. It is downloaded into the
build's own workspace and restored into a container beside it, both inside your Cloud project,
and both destroyed when the build ends. It never touches the production host, which by design
cannot read what it wrote, and it never reaches a GitHub runner: the workflow that schedules
this uploads only this repository's source, which is public.

What it asserts, which is the rehearsal of 2026-08-31 written down:

- `pg_restore` completes with no errors;
- a retention sidecar exists and passes strict JSON-lines validation;
- every deletion in that sidecar is replayed against the restored database with its ownership,
  creation-time and revision guards, so purged rows cannot reappear;
- every migration the dump recorded is marked successful, and none is a version this checkout
  does not carry. A deployment **behind** the branch passes: production is often a release or
  two back, and failing every month in between would train everyone to ignore the job. A
  deployment **ahead** fails, because a dump carrying schema this code does not know is not one
  this code can be restored onto;
- the organisation-deletion tables are present, named because deletion is the one operation
  Sotto cannot undo from inside the product, so this backup is the only thing behind it;
- the tables that must never be empty are not. A dump of the wrong database, or one taken after
  a truncation, passes every structural check ever written and fails this one.

Nothing the job prints is data. Counts, table names and migration versions are safe to say out
loud; `pg_restore`'s error log stays in the build workspace even on failure, because it can quote
the SQL it choked on. The build logs are private to the project, and this is careful anyway: a
log that only holds what it should is one you can paste into an issue without reading it twice.

### Where it runs, and why not with the other checks

Every other check here runs its work on a GitHub runner. This one is scheduled from one and
runs its work in your Cloud project, and the reason is what a dump contains. Secret names and
values are ciphertext, so a backup gives up nothing about them. The metadata is plain text: user
emails, OAuth identities, the grant graph, timestamps. Running
the drill inside the project that already holds the bucket means none of that is copied to
another party to prove a point about it, and the same-region read costs nothing.

`deploy/restore-verification.yaml` is a Cloud Build config. It starts a throwaway Postgres,
fetches the newest dump, restores it, and runs the checks above. Everything dies with the build.

### Configuration

Restoring needs to **read** objects, which the daily freshness check deliberately cannot do, so
it gets its own identity rather than widening that one:

```sh
gcloud services enable cloudbuild.googleapis.com cloudscheduler.googleapis.com

gcloud iam service-accounts create sotto-backup-restorer \
  --display-name "Reads backups for monthly restore verification"

gcloud storage buckets add-iam-policy-binding gs://sotto-backups-prod \
  --member "serviceAccount:sotto-backup-restorer@<project>.iam.gserviceaccount.com" \
  --role roles/storage.objectViewer

# A build running as this account writes its own logs, so without this it cannot start. The
# separate grant that lets something *act as* this account is further down, with the schedule:
# a human with project access already has it, which is why the manual submit below works before
# that grant exists.
gcloud projects add-iam-policy-binding <project> \
  --member "serviceAccount:sotto-backup-restorer@<project>.iam.gserviceaccount.com" \
  --role roles/logging.logWriter

# And to read the source it was handed. A build running as its own service account fetches the
# uploaded source itself rather than inheriting the caller's access, so without this the build
# fails before it starts with a 403 on the staging bucket, which reads like a problem with the
# backup bucket and is not one.
gcloud storage buckets add-iam-policy-binding gs://<project>_cloudbuild \
  --member "serviceAccount:sotto-backup-restorer@<project>.iam.gserviceaccount.com" \
  --role roles/storage.objectViewer
```

The staging bucket appears the first time you submit a build, so run the submit below once,
expect that failure, then grant this and run it again. Granting it up front works too if the
bucket already exists.

**Run it by hand before scheduling it.** A drill that has never run is not a drill:

```sh
gcloud builds submit --no-source --config deploy/restore-verification.yaml \
  --service-account projects/<project>/serviceAccounts/sotto-backup-restorer@<project>.iam.gserviceaccount.com \
  --substitutions _BACKUP_BUCKET=gs://sotto-backups-prod
```

Nothing is uploaded: the build fetches the source from the public repository itself, so no
staging bucket is involved. Add `_REVISION=<branch or sha>` to drill a branch rather than `main`.


### Scheduling it

`.github/workflows/backup-restore.yml` runs that same command monthly. It schedules the drill
and does not perform it: the build happens inside the project, so the dump never reaches a
runner, and what travels from GitHub is this repository's own source, which is public anyway.

A Cloud Build trigger would have kept the scheduling in the project too, and is the obvious
choice until you try it. It needs a GitHub App connection, which is a standing authorisation
over the repository, and nothing else in this project requires one. Scheduling from a workflow
that already federates costs no new trust, and keeps the schedule in the repository next to the
drill it runs.

On another provider the shape is the same and only the nouns change: a scheduled container with
read access to the backup store, running the three commands from **Doing it by hand** below. A
scheduled task or a build service will do it; nothing here depends on Cloud Build beyond it
being what this deployment already has.

That workflow needs an identity that may **start** a build without being able to read a backup:

```sh
gcloud iam service-accounts create sotto-build-submitter \
  --display-name "Starts the monthly restore build"

gcloud projects add-iam-policy-binding <project> \
  --member "serviceAccount:sotto-build-submitter@<project>.iam.gserviceaccount.com" \
  --role roles/cloudbuild.builds.editor

# Start a build that runs as the restorer, without holding the restorer's read access itself.
gcloud iam service-accounts add-iam-policy-binding \
  sotto-backup-restorer@<project>.iam.gserviceaccount.com \
  --member "serviceAccount:sotto-build-submitter@<project>.iam.gserviceaccount.com" \
  --role roles/iam.serviceAccountUser

# Let this repository's workflows assume it, through the provider the freshness check already uses.
gcloud iam service-accounts add-iam-policy-binding \
  sotto-build-submitter@<project>.iam.gserviceaccount.com \
  --role roles/iam.workloadIdentityUser \
  --member "principalSet://iam.googleapis.com/projects/<number>/locations/global/workloadIdentityPools/github/attribute.repository/<owner>/<repo>"

# And to upload the source it submits. `roles/cloudbuild.builds.editor` carries eleven
# permissions and not one of them is storage, so without these the submit fails on the staging
# bucket rather than on anything to do with backups.
#
# Both roles, because neither is enough alone: objectAdmin has no `buckets.*` permission at all
# and the client looks the bucket up before writing to it. The failure blames
# `serviceusage.services.use` and suggests Service Usage Admin, which is a much larger grant
# than the missing one and would not be the reason it worked.
gcloud storage buckets add-iam-policy-binding gs://<project>_cloudbuild \
  --member "serviceAccount:sotto-build-submitter@<project>.iam.gserviceaccount.com" \
  --role roles/storage.objectAdmin

gcloud storage buckets add-iam-policy-binding gs://<project>_cloudbuild \
  --member "serviceAccount:sotto-build-submitter@<project>.iam.gserviceaccount.com" \
  --role roles/storage.legacyBucketReader
```

Then set the repository variables `GCP_BUILD_SUBMITTER_SERVICE_ACCOUNT` and
`GCP_RESTORE_SERVICE_ACCOUNT`, and optionally the secret `RESTORE_HEARTBEAT_URL`. The workflow
skips while any of them is unset.

Note the separation, which is the point of the second account: the identity GitHub can assume
may start builds and act as the restorer, but holds no access to the bucket. The identity that
can read backups cannot be assumed from GitHub at all.

**What this costs.** Cloud Build bills build-minutes, of which 2,500 a month are free, and a run
of this takes about five. The schedule is a GitHub Actions cron, which is free for a public
repository, so no Cloud Scheduler job is needed. Reading the bucket from the same region is not
charged. So the expected bill is nothing, and at list price with no free tier at all, five
minutes a month is a few pence a year.

### Doing it by hand, on any object store

The bundled config implements `gs://` because that is what the hosted deployment uses. The drill is
the same everywhere and is worth running by hand once, whatever you store backups in:

```sh
# Fetch the newest dump. Use whatever your store speaks; rclone speaks most of them.
rclone copy "$SOTTO_BACKUP_BUCKET/$(rclone lsf "$SOTTO_BACKUP_BUCKET" | sort | tail -1)" .

# Restore into a scratch database, never the live one.
createdb restored
pg_restore -d restored --no-owner --no-privileges <dump>

# Check what came back.
scripts/check-restore --database-url postgres://localhost/restored   # no password in the URL
```

Record the result in `deploy/rehearsals/` the way the existing entries do. A drill nobody writes
down is one nobody can prove happened, which is how the nightly backup went uninstalled for two
months with the runbook describing it the whole time.

## Status history

A status page needs history, and history cannot be backfilled: every day nothing samples the
deployment is a permanent gap in the record. So the collector starts before the page exists.

`.github/workflows/status-collector.yml` samples the public surface roughly every ten minutes
and appends what it saw to an orphan `status-history` branch, which shares no history with
`main` and so never appears in a source diff. Two files accumulate there:

- `summary.json`, the current state of each component and a per-day tally over ninety days,
  which is what a page renders;
- `samples/<date>.jsonl`, one line per observation, so the tallies can be recomputed if the
  aggregation ever turns out to be wrong. A published uptime figure nobody can recheck is a
  figure nobody should have to take on trust.

Both age out at the same ninety days, which is the point: the audit trail covers exactly the
window the summary publishes, and nothing is kept that no longer backs a number anyone can see.
If you want a longer record than you publish, the sample files are plain JSONL and copying them
somewhere else before they age out is the whole of what that takes.

Set the repository variable `SOTTO_PUBLIC_URL` to the deployment to watch. Without it the job
skips rather than probing a default, so a fork cannot point it at somebody else's deployment.
Set `SOTTO_CANARY_TOKEN` and `SOTTO_HUMAN_CANARY_TOKEN` plus the variable
`SOTTO_HUMAN_CANARY_ENVIRONMENT_ID` if you want both authenticated retrieval rows measured rather
than reported unconfigured; the probe table and setup notes below explain their boundaries.

**In a fork, one line has to change as well.** The job carries
`if: github.repository == 'getsotto/sotto'`, so a fork of this repository collects nothing and
does so silently, which is a poor way to find out. That guard exists because a fork inherits
both the schedule and the write permission, and neither probing another project's deployment
nor pushing history into its own branch is something a fork should start doing by being made.
Change the name to your own repository, or drop the line if you are happy for every fork of
your fork to sample too. The alternative is to skip the workflow entirely and run the script
from your own scheduler, below.
Set the secret `STATUS_HEARTBEAT_URL` too, from any external checker: the collector pings it
after each round of samples is pushed, and the checker alerting when those pings stop is the
only thing that can notice this job dying or running green while sampling nothing. Allow that
monitor a generous grace period, hours rather than minutes, because GitHub queues scheduled
workflows rather than guaranteeing them and a run arriving late is not a run that failed.

Two things to get right before setting the variable, because both write a wrong answer into a
record that is meant to be permanent:

- **Point it at the origin the deployment actually serves**, with the scheme it serves on. No
  probe follows redirects, so a `www` host or an `http` URL that the deployment folds onto its
  canonical origin would otherwise read as every component being down at once. A round where
  every probe was redirected is treated as a wrong setting rather than an outage: nothing is
  written and the run fails, which withholds the heartbeat and makes the mistake noticeable
  instead of accruing invented downtime. A deployment that is genuinely gone refuses
  connections rather than redirecting them, so a real outage is still recorded. A single
  redirected component among working ones is recorded too, since only unanimity is
  unambiguous.
- **Wait until the deployment serves `/health/ready`**, which means version 0.7.0 or later.
  Before that the path falls through to the single-page app, and the API row records real
  downtime for a deployment that is working.

Four of the six probes are unauthenticated and ask only what a visitor could ask. The other two
use ordinary authentication against a synthetic environment:

| Component        | Probe                                               | Healthy answer                                |
| ---------------- | --------------------------------------------------- | --------------------------------------------- |
| API              | `GET /health/ready`                                 | `200` with the body `ok`                      |
| Web app          | `GET /`                                             | `200` and an HTML content type                |
| Sign in          | `GET /auth/github/login` with a loopback callback   | a redirect to `github.com`                    |
| Billing          | `POST /billing/webhook` with no signature           | `401`                                         |
| Secret sync       | `GET /machine/grant` and `GET /machine/secrets`     | a usable grant and a usable secret            |
| Human API access  | `GET /environments/{id}/secrets` with a user session | a usable synthetic encrypted secret snapshot |

Two of those distinguish "not configured" from "broken", because they are not the same thing
and only one of them belongs in an uptime figure. A `503` from sign-in or billing means the
deployment has no OAuth or no Stripe credentials, which is a choice; it is recorded as
unconfigured and left out of the tally, so a self-hoster running neither does not watch their
published uptime fall for features they decided not to run. A `503` from `/health/ready` is
the opposite: it has exactly one cause, an unreachable database, and it counts as downtime.

The billing probe deliberately sends an unsigned payload and requires a `401`. A `200` there
would mean signature verification is not happening, so that case is recorded as down rather
than as a passing request.

The two retrieval probes authenticate because these questions cannot be asked from outside: can a
machine still get its sealed grant and ciphertext, and can an ordinary signed-in user still read
an environment they are authorised to access? A deployment can serve every public surface
perfectly while either authenticated path is broken.

It needs a throwaway project holding junk secrets and a machine token scoped to one of its
environments. A personal project is enough; an organisation is only needed for sharing, and a
machine token binds an environment rather than an org. Create the token from that project's
directory, because `sotto` finds its project by walking up from the working directory and will
otherwise mint a token for whichever project it finds first. Set it as the repository secret
`SOTTO_CANARY_TOKEN`, and **store only the bearer half**, the
`smt_...` part before the dot:

```sh
cd ~/sotto-canary                         # or wherever the canary project lives
sotto token create --name status-canary --expires-in-days 365   # prints a SOTTO_TOKEN once
gh secret set SOTTO_CANARY_TOKEN --body 'smt_...'   # the part BEFORE the dot, nothing after it
```

The machine canary token expires like any other, a year at most, and an expired token is refused exactly
as a revoked one is. The probe cannot tell the two apart, so a lapsed canary token is recorded as
secret sync being down. `sotto token ls`, run in the canary project, shows its end date; replace
it before then, and put that date somewhere you will see it, because this job never alerts.

A `SOTTO_TOKEN` is the server-issued bearer joined to the machine's private key. Only the bearer
is needed here, because the server authenticates on it alone, and the probe refuses to run with a
whole token rather than trimming one: trimming would leave the private key in the collector's
environment, where anything able to read it could pair that key with the ciphertext just fetched
and decrypt the canary. Refusing is what makes "the collector cannot decrypt" a property of what
it holds rather than a promise about what it does. If the whole token is stored, the row reports
unconfigured and says so.

Without the machine secret the row reports unconfigured and the other five probes are unaffected, which
is what a self-hoster with no canary should see: an unconfigured component is not an outage and
is left out of the tally.

What it checks is deliberately short of decrypting. The sealed vault key is decoded and measured,
because a truncated or empty grant answers `200` and looks exactly like a good one while leaving a
machine unable to open anything; and the secrets snapshot must still hold a secret a machine could
use, meaning one that is not deleted and carries both its ciphertext and its wrapped data key. An
empty list is not the only way to have nothing: `/machine/secrets` returns soft-deleted rows as
well, flagged, so an environment whose secrets have all been removed answers with a full-looking
list holding nothing usable. Keep the canary environment small, a couple of junk secrets, because
that snapshot is read whole and a canary that has outgrown the budget reports so rather than
guessing. That case is recorded as unconfigured rather than down: it is your environment having
grown, not the deployment having failed, and it has no business in the published uptime figure. Opening the grant would need the private key, which is the thing this is built
not to have. The credential is also withheld from a target that has not answered for itself:
unauthenticated probes run first, and if the configured URL is not `https`, or every one of them
was redirected elsewhere, no token is sent at all.

The human row uses a normal Sotto user session, not a special canary header or bypass. Keep its
account dedicated to the canary, with access only to one throwaway project containing junk
secrets. Configure the session bearer in the repository secret `SOTTO_HUMAN_CANARY_TOKEN` and
the environment id in `SOTTO_HUMAN_CANARY_ENVIRONMENT_ID`. User sessions expire after 30 days, so
rotate this secret through the ordinary sign-in flow before expiry. Do not use a staff account or
an account with customer or organisation access. With either setting absent, the human row reports
unconfigured and no authenticated request is made.

The job records and never alerts. A component being down leaves the workflow green, because
paging belongs to an external monitor that survives this repository being unreachable, and a
workflow that went red on every blip would train everyone to ignore the failure that matters
here, which is the collector itself dying.

The sign-in probe is the one that writes: starting the OAuth flow records a short-lived login
row, which the same endpoint clears on its next call. That is deliberate, since it exercises
the write path rather than only a read, but it is worth knowing that this probe is not purely
an observer.

### Running it somewhere other than GitHub Actions

Nothing about the check needs GitHub. `scripts/status-probe --base-url <url> --data-dir <dir>`
is Python 3 with no dependencies beyond the standard library, and the data directory is a
directory of files. Run it from cron, a systemd timer, or any other scheduler:

```sh
*/10 * * * * /path/to/scripts/status-probe --base-url https://example.com --data-dir /var/lib/sotto-status
```

Serve or sync that directory however suits you: a static host, an object store, a commit to
any git host. The workflow adds three things and no more, so anything that does them is
equivalent: it runs the script on a schedule, keeps the output somewhere durable, and pings a
heartbeat afterwards so the check being dead is noticeable.

The verdict logic is covered by `scripts/tests/test_status_probe.py`.

One caveat if you keep the history in git, as the bundled workflow does: the samples age out
with the summary, but the commits do not. At this interval that is roughly fifty thousand
commits a year on a branch nothing else reads. Deleting the branch is a safe reset if it ever
becomes awkward, since the next run recreates it, at the cost of the history it held.

## Status page

`scripts/build-status` renders one self-contained HTML file from two inputs: the summary the
collector maintains, and the issues labelled `incident`. No scripts, no external assets, no
fonts to fail to load. A status page whose own availability depends on a CDN is a joke told at
its own expense.

`.github/workflows/status-page.yml` publishes it to GitHub Pages, on purpose rather than beside
the deployment: a status page hosted on the thing it reports on tells you nothing on the only
day anybody visits it.

### What it will not claim

The collector runs when GitHub's scheduler gets round to it, which in practice is a few times a
day rather than the six an hour its cron asks for. Three consequences are built into the page
rather than papered over:

- **A day with no checks is drawn blank**, not green and not red. A day nobody looked at is not
  a day that was up, and shading it in either direction invents history.
- **Every percentage carries its denominator**: `100.00% of 4 checks`, never a bare `100%`. At
  this cadence a figure without its sample count is a number nobody could stand behind.
- **The sampling rate quoted in the footer is measured from the data**, not the interval the
  cron requests.

If you want a denser uptime record than this, the external checker that watches
`/health/ready` is the better source; it runs every few minutes and does not depend on a queue.

### Setup

**In a fork, change the guard first.** The build job carries
`if: github.repository == 'getsotto/sotto'`, so a fork publishes nothing and does so silently.
That guard exists because a fork inherits the schedule and would otherwise rebuild a page about
somebody else's deployment; change the name to your own repository, or drop the line. The same
applies to the collector, which the page reads from.

1. **Enable Pages**: repository settings, Pages, source **GitHub Actions**.
2. **Point the DNS**: a `CNAME` from `status.<your domain>` to `<owner>.github.io`, then set the
   custom domain in the same settings page and wait for the certificate.
3. **Create the label** the page reads: `gh label create incident --color B60205 --description
   "A user-visible incident, rendered on the status page"`. The build tolerates it not existing;
   an unlabelled repository simply has no incidents to show.

### Running an incident

Incidents are GitHub issues, chosen for one reason: at three in the morning on a phone, opening
an issue and adding comments is realistic and committing files is not. The authoring path has to
be the easiest available or the log stops being written exactly when it matters.

- **Open** an issue labelled `incident`. The title is what the page shows.
- **Update** by commenting. Each comment becomes a timestamped entry, in order.
- **Stage** it by adding `investigating`, `identified` or `monitoring`. The page shows the
  furthest one reached.
- **Resolve** by closing the issue. That wins over any stage label still attached, so an
  incident closed in a hurry does not sit on the page claiming to be under investigation.
  Closing is the *only* thing that resolves one: a `resolved` label is ignored, because a stale
  one would publish an outage as over while it was still happening.

The log covers the same ninety days as the bars, with one exception worth knowing: an incident
still open is shown however old it is. If more incidents ever match than the build fetches, the
page says the list is incomplete rather than quietly showing a short one.

### Publishing it somewhere else

The generator is Python 3 with no dependencies beyond the standard library and reads two JSON
files:

```sh
scripts/build-status --summary summary.json --incidents incidents.json --out site/
```

`site/index.html` is the whole page. Serve it from any static host. Nothing about it needs
GitHub Pages beyond it being free and outside the deployment's failure domain.

## Organisation-deletion metrics

The deletion worker stores aggregate lifecycle counters in Postgres. Their fixed vocabulary, alert
conditions, and the protected Prometheus scrape are documented in
[DELETION-METRICS.md](DELETION-METRICS.md). Set `SOTTO_ORGANISATION_DELETION_METRICS_TOKEN` only
when the monitoring system is ready to send the bearer token securely.

The operator observation endpoint is separately protected by
`SOTTO_ORGANISATION_DELETION_OPERATOR_TOKEN`. Leave it blank until the deletion runbook has been
rehearsed and the authenticated observation procedure is ready. Never reuse the metrics token for
this write-capable operational control.

Both endpoints are independent of the deletion flags: configuring either token does not enable
deletion, and neither is enabled by turning deletion on.

### Rotating them

`./rotate-deletion-tokens.sh`, run from this directory on the host. It checks itself against the
deployment's served origin, taken from `SOTTO_DOMAIN` in `.env`, because the server publishes no
port to the host: compose gives it an address on its own network and Caddy reaches it as
`server:8080`, so `127.0.0.1:8080` answers nothing. Pass `SOTTO_ORIGIN=https://your.domain` for a
deployment whose domain is not in `.env`, and `SOTTO_ROTATE_HEALTH_ATTEMPTS` to wait longer than
a minute for the restart. It replaces both, restarts
the server with `up -d` rather than `restart` (which reuses the old environment and would leave
the previous tokens live while `.env` claimed otherwise), and then proves the rotation took.

Both go together because they are one blast radius: they live two lines apart in the same file,
so an exposure that reached one plausibly reached the other.

It refuses to run if either variable appears twice in `.env`. That is not tidiness: compose reads
the last occurrence and the script reads the first, so a duplicate would let the rotation verify
itself against a value that was never live. An ambiguous secrets file is worth fixing before
rotating the secrets in it.

The proof is the part worth having. It checks that the new token is accepted **and that the old
one is now refused**, because without that second half a no-op edit and a real rotation look
identical from outside. If any check fails it says so, leaves the previous `.env` beside the new
one, and tells you the two commands to go back.

No token is passed as an argument either, which is a different exposure from printing one:
argv is world readable, so `ps` on a shared host would hand the bearer token to any local user
for as long as a check runs. Each request reads its `Authorization` header from a file at mode
600, removed immediately afterwards and on the way out if something fails first.

No token is printed at any point, not even the new one. The values are written to `.env` and
read back from there for the checks, because a rotation that shows you the replacement on the
way past has published it to whatever is recording the session. Delete the backup file once you
are satisfied: it still holds the old tokens, and a rotation that leaves them on disk has moved
them rather than retired them.

## Enabling organisation deletion

Deletion is irreversible once purge begins, so treat enablement as a release of its own. Work
through it on a staging deployment first, then repeat it on production with the same pinned image
tag. The full operator procedure, including the rehearsal record you must complete, is in
[ORGANISATION-DELETION-RUNBOOK.md](ORGANISATION-DELETION-RUNBOOK.md).

**Prerequisites** - all of these before either flag changes:

1. A managed backup or export lifecycle covering the configured recovery window, with a restore
   into an isolated scratch database rehearsed and recorded.
2. `SOTTO_ORGANISATION_DELETION_METRICS_TOKEN` set from the deployment secret store, alerting on
   the exporter arranged, and one notification tested. Prometheus deployments load the
   [alert rules](ORGANISATION-DELETION-ALERTS.yml); everyone else can use
   `scripts/check-deletion-metrics`, which decides three of those five rules from one scrape.

   **The checker runs somewhere else, so it needs its own copy of the token.** Any scheduler will
   do: the script takes `--url` and reads the token from `DELETION_METRICS_TOKEN`, so cron on a
   separate host, a CI job, or whatever your fleet already uses are all equivalent. It exits 0
   when nothing needs attention, 1 when something does, and 2 when it could not tell, which is
   enough to drive a notification anywhere.

   What matters is that wherever it runs has its own copy of the token, because setting the
   deployment variable alone leaves the checker unable to reach the exporter. This repository runs
   it from GitHub Actions, which reads the repository secret `SOTTO_DELETION_METRICS_TOKEN` and
   the repository variable `SOTTO_PUBLIC_URL`; substitute your own equivalents:

   **Pipe the value in; do not type it.** Whatever holds the copy, the token should reach it from
   the secret store rather than through a command line, because an argument is visible to `ps`
   while the command runs and stays in shell history afterwards. That is the same rule this
   document applies to the deployment variable itself.

   ```sh
   # Run where deploy/.env lives. Refuse a missing, empty, or duplicate assignment before `gh`
   # starts, so an ambiguous file cannot overwrite the repository secret with the wrong value.
   count=$(grep -c '^SOTTO_ORGANISATION_DELETION_METRICS_TOKEN=' .env 2>/dev/null || true)
   count=${count:-0}
   if [ "$count" -ne 1 ]; then
     echo 'expected exactly one SOTTO_ORGANISATION_DELETION_METRICS_TOKEN assignment in .env' >&2
     exit 1
   fi
   if grep -q '^SOTTO_ORGANISATION_DELETION_METRICS_TOKEN=$' .env; then
     echo 'SOTTO_ORGANISATION_DELETION_METRICS_TOKEN must not be empty' >&2
     exit 1
   fi

   # `gh secret set` reads standard input when --body is omitted, so the value is never an argument,
   # never echoed, and does not enter shell history.
   sed -n 's/^SOTTO_ORGANISATION_DELETION_METRICS_TOKEN=//p' .env \
     | gh secret set SOTTO_DELETION_METRICS_TOKEN --repo <owner>/<repo>

   # The URL is not a secret.
   gh variable set SOTTO_PUBLIC_URL --body 'https://your.domain' --repo <owner>/<repo>
   ```

   Rotating the deployment's token means setting this one again. Nothing checks that they agree;
   a stale copy shows up as `401` from the exporter, reported as exit 2 rather than as an alert.
3. `SOTTO_ORGANISATION_DELETION_OPERATOR_TOKEN` set from the deployment secret store, with the
   authenticated observation procedure reviewed and rehearsed.
4. Billing configured and verified end to end: the provider's API version, restricted key, and
   webhook endpoint matching [Billing](#billing-optional).
5. `SOTTO_IMAGE_TAG` pinned to a released version, so the server and web images cannot skew.

**Enablement**, on staging first:

```sh
# in deploy/.env
SOTTO_ORGANISATION_DELETION_WORKER_ENABLED=1
```

```sh
docker compose -f docker-compose.prod.yml up -d
```

That enables the whole server side, the deletion routes and the lifecycle worker together, and it
works with prebuilt images. Everything in the verification list below goes through the API, so the
server side can be enabled and verified on its own.

The client control is a separate step with a trap in it. `VITE_ORGANISATION_DELETION_ENABLED` is
compiled into the web bundle when the image is built, and the published images are built with it
`false`, so setting it in `deploy/.env` changes nothing while you pull prebuilt images. To show the
control in the web app, set it in `deploy/.env` and rebuild the web image from source, which routes
the value through the compose build argument:

```sh
docker compose -f docker-compose.prod.yml up -d --build caddy
```

or publish your own web image with that build argument set and pin the deployment to it. A source
build needs the RAM headroom noted under [First deployment](#first-deployment).

**Verify**, before repeating any of this on production:

- `https://<SOTTO_DOMAIN>/health/ready` returns `ok`, which is the check to make here rather than
  `/health`: everything below this line is stored in Postgres, and `/health` answers the same
  whether the deployment can reach it or not (see [Uptime monitoring](#uptime-monitoring));
- the protected metrics endpoint answers `200` with its token and `401` with a missing or wrong
  one; a `503` instead means its token is not configured, so prerequisite 2 is unmet;
- the operator observation endpoint answers `401` for a missing token, for a wrong one, and for
  the metrics token, which it must never accept; a `503` instead means prerequisite 3 is unmet;
- an owner on a disposable test organisation can complete the confirmation flow, see the recovery
  window, and cancel it again;
- the audit trail and server logs show the request, cancellation, and operator observation, with
  no bearer token or provider text in them.

**Turning it back off** stops new requests and idles the worker, but does not restore an
organisation whose purge has already begun. Existing operations keep their stored `purge_after`
deadline; the routes return `404` again and the worker stops advancing the queue, leaving frozen
organisations frozen until deletion is re-enabled or an operator recovers them through the runbook.

## Cloud lifecycle email contacts

Contact enrolment is optional and independent of billing and vault access. Both
`SOTTO_CLOUD_CONTACT_ENROLMENT_ENABLED` and `SOTTO_CLOUD_CONTACT_EMAIL_SEND_ENABLED` default to
`0` and must be enabled together; the server rejects a partial configuration. Keep both off until
the sender domain is authenticated and a test recipient has completed the full challenge flow.
The server requires an HTTPS `SOTTO_WEB_ORIGIN`, a configured sender address, and the transport
credential from the deployment secret store. Do not put the credential in a checked-in file or
pass it on a command line.

The contact endpoints let a signed-in user set, verify, view, and revoke their own lifecycle email
address without unlocking the vault. Verification proves control of that address; it does not
change billing, access, or export rights, and it does not mean future notices have been delivered.
The verification callback token is one-use and short-lived. The server stores only its digest.

To disable the journey, turn both flags off and restart the server. Existing verified contacts are
retained, and users can still revoke their own contact. Re-enabling the flags does not resend an
old challenge; the user starts a fresh verification request.

Lifecycle-notice email dispatch is a separate Cloud-only control:
`SOTTO_CLOUD_NOTICE_DISPATCH_ENABLED` defaults to `0`. It requires the configured Resend API
credential, sender address and webhook signing secret. The callback path is rate-limited at the
edge and accepts only signed provider feedback. Relay acceptance is recorded separately from a
verified delivery event. Notices with a future due date are not sent early; uncertain sends are
retried with one stable message identity only inside the provider's idempotency window. The
dispatcher never enables sales or coverage enforcement and is not started on self-hosted instances.
Each email links to the no-vault `/cloud` account surface.
Impending-purge email remains unavailable until the purge notice timing policy is approved; dispatch
does not infer a cadence from the retention deadline.

Email intents are recorded for users with a verified contact even while dispatch is disabled. Before
enabling dispatch after contact enrolment has been available, review the pending queue through the
protected notice diagnostics and suppress notices that are no longer timely; due backlog becomes
eligible for sending as soon as the worker starts. A send whose outcome remains unknown is moved to
`delivery_unknown` 23 hours after its first attempt to stay inside Resend's idempotency window. Check
the provider dashboard before deciding how to handle that notice; do not create a new message identity
to retry it.

The recovery-period warning is scheduled three days before the recovery deadline and is sent only
while the same renewal remains in recovery. If payment resolves or the recovery period ends before the
warning is delivered, current eligibility suppresses it.

Protected notice diagnostics are available at `/ops/cloud/notices` when the existing Cloud retention
operator token is configured. The endpoint returns bounded opaque fingerprints and fixed reason
codes only. `retry` is limited to a known rejected request with a current verified contact and a
remaining idempotency window; `suppress` only stops work that has not been accepted by the provider.
Operator labels are declared audit text, not authenticated identities. A sent message cannot be
recalled, and this surface is not a live-delivery rehearsal.

## Database security

The default `docker-compose.prod.yml` keeps Postgres on the **internal compose network only** - it
is never published to a port, so the server↔database link never leaves the host and the plaintext
connection (`DATABASE_URL` carries no `sslmode`) is not exposed. That is the recommended topology.

If you instead point `DATABASE_URL` at a **remote or managed Postgres**, the link now crosses a
network, so encrypt it. The server binary is built with system TLS (native-tls), so it is enough to
ask for it in the connection string:

```sh
# require encryption:
DATABASE_URL=postgres://user:pass@db.example.com:5432/sotto?sslmode=require
# or verify the server certificate against a CA (strongest):
DATABASE_URL=postgres://user:pass@db.example.com:5432/sotto?sslmode=verify-full&sslrootcert=/path/to/ca.pem
```

Even without TLS the database only ever holds ciphertext and the key-wrapping graph - secret names
and values are encrypted client-side and are never decryptable server-side (see
[THREAT-MODEL.md](../THREAT-MODEL.md)). TLS to the database protects the **metadata** (emails, the
sharing graph, timestamps) in transit, and is a hard requirement for any deployment where that link
is not a trusted private network.

## Rate limiting & perimeter

Abuse control lives at the edge, where the real client IP is visible. The deploy Caddy image is an
[xcaddy](https://github.com/caddyserver/xcaddy) build bundling the
[caddy-ratelimit](https://github.com/mholt/caddy-ratelimit) plugin (pinned in
`deploy/Dockerfile.web`), and the `Caddyfile` applies a per-client-IP limit to the **unauthenticated**
endpoints - the OAuth login/callback and the public share fetch, the only API surface with no
credential wall. Authenticated sync is intentionally left unthrottled at the edge: it is bearer-gated
and includes high-frequency CI polling that a per-IP cap could wrongly block when a whole team shares
one office/NAT egress IP. Tune the threshold (or split it into per-endpoint zones) in the `Caddyfile`.

Two honest limits, consistent with the [threat model](../THREAT-MODEL.md) (availability is an
accepted residual risk, and self-hosting is the escape hatch):

- **Per-IP, not global.** A distributed flood from many source addresses is not stopped by this;
  put a CDN/WAF in front if you need volumetric protection.
- **This lives in *this* Caddy.** If you front the server with your own proxy, or expose
  `sotto-server` directly, the server does **not** self-throttle - supply equivalent rate limiting
  at your own edge.

## Legacy organisation billing (optional)

Without Stripe credentials, billing endpoints return 503 and existing organisations remain on their
stored tier. The legacy per-organisation checkout is still present for compatibility, but new
Cloud sales stay disabled unless `SOTTO_CLOUD_SALES_ENABLED=1` and the complete hosted catalogue
are configured. Do not use this procedure as a Cloud launch checklist; hosted sales require a
separate operator review and the dedicated lifecycle evidence gate.

1. In Stripe Workbench, configure the account and Prices that match the server-owned catalogue.
   Keep the four hosted Price ids together: standard monthly, standard annual, founding monthly,
   founding annual.
2. Configure a restricted API key and add a webhook endpoint for
   `https://<SOTTO_DOMAIN>/billing/webhook`, set its API version to
   `2026-07-29.dahlia`, and subscribe it to `checkout.session.completed`,
   `checkout.session.async_payment_succeeded`, `checkout.session.expired`,
   `customer.subscription.updated`, `customer.subscription.deleted`, and `invoice.paid`; keep the
   endpoint's `whsec_…` signing secret private. Its version must match the server pin.
3. Fill the API key, webhook secret, legacy `STRIPE_PRICE_ID`, and all four hosted Price ids in
   `.env`. Leave `SOTTO_CLOUD_SALES_ENABLED=0` until the sandbox lifecycle and policy checklist
   are reviewed. Recreate the server to apply configuration.

Card data never touches the server - checkout and subscription management happen on
Stripe-hosted pages, and the webhook only assigns the org's tier.

## Operations

The default-off `SOTTO_BILLING_RECOVERY_WORKER_ENABLED` retries interrupted personal Checkout
creation from the stored offer, allocation, expiry and original Stripe idempotency key. It only
creates or reopens the same Checkout session; payment and coverage still require the signed
`invoice.paid` webhook. Retries stop before Stripe's documented idempotency-key pruning window, and
ambiguous older operations are parked for review. The worker also checks Stripe's current status
for locally expired pending personal accounts with a stored Checkout session id. For older rows
that contain only the Checkout URL, it extracts the session id from the URL path; it also accepts
`provider_operation_id` when that field actually contains a Checkout session id. Stripe's response
must confirm the exact personal operation before the account is released, and the session must be
expired and unpaid. This repairs missed or previously acknowledged expiry events; an open or
completed session is never released by this check. This switch does not enable sales.

The default-off `SOTTO_SPONSORED_BILLING_RECOVERY_WORKER_ENABLED` reconciles saved sponsored
Checkout and shared-subscription schedule requests. It reuses the exact saved request and original
Stripe idempotency key, and parks missing, changed or out-of-window evidence for review. Signed
payment webhooks remain authoritative for activating sponsored seats. This switch requires Cloud
mode and the hosted Stripe price catalogue, and it does not enable sponsored sales or refund
processing. Older unresolved operations without saved request facts are parked by migration 0071;
do not reconstruct a provider request manually.

Signed Stripe expiry and paid webhooks can still close an operation while it is in `manual_review`.
For an operation parked without a recorded Checkout session, first search Stripe for
`client_reference_id=sponsored:<operation_id>`, subscription metadata
`sponsored_operation_id=<operation_id>`, and related invoices and payments. Do not clear the seat
or founding reservation if any provider object or payment may exist. If Stripe confirms that no
Checkout session, subscription, invoice, or payment exists, record the incident and perform this
guarded repair. The capacity row and operation query must each return exactly one row; the seat
query may return zero or one pending seat; the reservation query may return zero or one reserved
reservation. Roll back if another state or provider reference is present. The three updates must
affect one operation and no more than one seat or reservation; otherwise roll back and investigate.

```sql
BEGIN;
SELECT singleton FROM billing_founding_capacity WHERE singleton = TRUE FOR UPDATE;
SELECT operation_id FROM billing_sponsored_operations
 WHERE operation_id = '<operation_id>'
   AND state = 'manual_review'
   AND recovery_manual_review_required = TRUE
   AND action = 'add'
   AND provider_customer_id IS NULL
   AND provider_subscription_id IS NULL
   AND provider_checkout_session_id IS NULL
   AND provider_payment_reference IS NULL
   AND provider_item_id IS NULL
   AND provider_operation_id IS NULL
   AND provider_checkout_url IS NULL
 FOR UPDATE;
SELECT seat_id FROM billing_sponsored_seats
 WHERE operation_id = '<operation_id>' AND state = 'pending'
 FOR UPDATE;
SELECT reservation_id FROM billing_founding_reservations
 WHERE operation_id = '<operation_id>' AND status = 'reserved'
 FOR UPDATE;
UPDATE billing_founding_reservations
   SET status = 'released', updated_at = now()
 WHERE operation_id = '<operation_id>' AND status = 'reserved';
UPDATE billing_sponsored_seats
   SET state = 'canceled', updated_at = now()
 WHERE operation_id = '<operation_id>' AND state = 'pending';
UPDATE billing_sponsored_operations
   SET state = 'failed', result_code = 'operator_confirmed_no_provider_object',
       recovery_manual_review_required = FALSE,
       recovery_last_error_code = 'operator_confirmed_no_provider_object',
       recovery_lease_token = NULL, recovery_lease_until = NULL, updated_at = now()
 WHERE operation_id = '<operation_id>'
   AND state = 'manual_review'
   AND recovery_manual_review_required = TRUE
   AND action = 'add'
   AND provider_customer_id IS NULL
   AND provider_subscription_id IS NULL
   AND provider_checkout_session_id IS NULL
   AND provider_payment_reference IS NULL
   AND provider_item_id IS NULL
   AND provider_operation_id IS NULL
   AND provider_checkout_url IS NULL
 RETURNING operation_id;
COMMIT;
```

Notify the organisation owner and beneficiary after commit. If the session, subscription, invoice,
or payment cannot be ruled out, leave the operation and seat parked and escalate; do not create a
replacement Checkout or rebuild the missing request.

For a parked personal Checkout with no session id, do not clear the account because its local
deadline elapsed. Check Stripe's event log and the customer's Checkout/subscription/payment history
for the exact `client_reference_id` `personal:<operation_id>`. If a session exists, keep the account
blocked and have an operator record its exact `cs_…` id on that operation so the recovery worker can
verify its status. Bind it only when the row is still parked for this reason; require one returned
row, then let the worker inspect Stripe before changing the account:

```sql
UPDATE billing_operations
   SET provider_checkout_session_id = '<cs_session_id>',
       reconciliation_next_attempt_at = now(), updated_at = now()
 WHERE operation_id = '<operation_id>'
   AND state = 'manual_review'
   AND result_code = 'provider_idempotency_window_elapsed'
   AND provider_checkout_session_id IS NULL
   AND provider_operation_id IS NULL
   AND provider_checkout_url IS NULL
   AND EXISTS (SELECT 1 FROM billing_personal_accounts AS account
                WHERE account.operation_id = billing_operations.operation_id
                  AND account.state = 'pending'
                  AND account.pending_expires_at_epoch <= extract(epoch FROM now())::bigint)
 RETURNING operation_id;
```

If Stripe confirms that no session, payment, invoice or subscription exists,
perform the following guarded repair in one database transaction. First record the operation id and
Stripe review in the incident log. The first two `SELECT`s must each return exactly one row; if
either returns no row or multiple rows, roll back and investigate. The `UPDATE`s must affect one
operation and one account; otherwise roll back. Notify the customer after commit so they can retry.

```sql
BEGIN;
SELECT singleton FROM billing_founding_capacity WHERE singleton = TRUE FOR UPDATE;
SELECT operation_id FROM billing_operations
 WHERE operation_id = '<operation_id>'
   AND state = 'manual_review'
   AND result_code = 'provider_idempotency_window_elapsed'
   AND provider_checkout_session_id IS NULL
   AND provider_operation_id IS NULL
   AND provider_checkout_url IS NULL
 FOR UPDATE;
SELECT operation_id FROM billing_personal_accounts
 WHERE operation_id = '<operation_id>'
   AND state = 'pending'
   AND pending_expires_at_epoch <= extract(epoch FROM now())::bigint
 FOR UPDATE;
UPDATE billing_founding_reservations SET status = 'released', updated_at = now()
 WHERE operation_id = '<operation_id>' AND status = 'reserved';
UPDATE billing_personal_accounts SET state = 'canceled', updated_at = now()
 WHERE operation_id = '<operation_id>' AND state = 'pending';
UPDATE billing_operations SET state = 'failed', result_code = 'operator_confirmed_no_session',
       reconciliation_manual_review_required = FALSE,
       reconciliation_last_error_code = 'operator_confirmed_no_session',
       reconciliation_lease_token = NULL, reconciliation_lease_until = NULL, updated_at = now()
 WHERE operation_id = '<operation_id>'
   AND state = 'manual_review'
   AND result_code = 'provider_idempotency_window_elapsed'
   AND provider_checkout_session_id IS NULL
   AND provider_operation_id IS NULL
   AND provider_checkout_url IS NULL;
COMMIT;
```

If a provider session or payment cannot be ruled out, leave the operation parked and escalate; do
not create another Checkout or mark the account canceled.

```sh
docker compose -f docker-compose.prod.yml logs -f server   # API logs
docker compose -f docker-compose.prod.yml logs -f caddy    # access/TLS logs
docker compose -f docker-compose.prod.yml ps               # health at a glance
```

- Postgres is **not** exposed outside the compose network; only Caddy publishes ports.
- Certificates and Caddy state persist in the `caddy_data` volume; database data in `pgdata`.
- The API route list lives in the repo-root [`Caddyfile`](../Caddyfile) (baked into the web
  image at build time) - pulling the matching image version picks up route changes automatically.
- To try it without a public domain, set `SOTTO_DOMAIN=localhost`: Caddy serves a self-signed
  certificate (`curl -k https://localhost/health`). GitHub login still requires a callback URL
  reachable by your browser.
- Organisation-deletion incidents follow the
  [`ORGANISATION-DELETION-RUNBOOK.md`](ORGANISATION-DELETION-RUNBOOK.md). It forbids direct SQL
  lifecycle changes and requires an isolated restore rehearsal before enablement.
