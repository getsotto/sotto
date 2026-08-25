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

On a 1 GB host, give the kernel some headroom before the first start:

```sh
sudo fallocate -l 2G /swapfile && sudo chmod 600 /swapfile
sudo mkswap /swapfile && sudo swapon /swapfile
echo '/swapfile none swap sw 0 0' | sudo tee -a /etc/fstab
```

Smoke test:

```sh
curl -fsS https://<your-domain>/health    # → ok
```

Then open `https://<your-domain>` in a browser and sign in with GitHub. Point the CLI at your
instance with `sotto login --server https://<your-domain>`.

## Upgrading

```sh
git pull    # picks up compose/runbook changes
docker compose -f docker-compose.prod.yml pull
docker compose -f docker-compose.prod.yml up -d
```

Migrations are forward-only and applied on boot. Check the release notes for anything flagged as
a compatibility break before upgrading past a minor version.

## Backups

Postgres holds only ciphertext and metadata, but losing it loses your users' synced vaults.
[`backup.sh`](./backup.sh) takes a custom-format `pg_dump` inside the container, **verifies the
archive** (`pg_restore --list`) before anything leaves the box, and uploads it to whatever
object storage `SOTTO_BACKUP_BUCKET` names - the scheme picks the tool:

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

2. **Write-only credentials for the host.** The box should be able to add backups but never
   read or delete them - a compromised host then can't destroy your history. On GCS that is
   `roles/storage.objectCreator` for the VM's service account; on AWS, an IAM policy allowing
   only `s3:PutObject` on the bucket.

   ```sh
   # Google Cloud:
   gcloud storage buckets add-iam-policy-binding gs://<bucket> \
     --member="serviceAccount:<vm-service-account>" --role="roles/storage.objectCreator"
   ```

3. **Point the script at it** (`deploy/.env`): `SOTTO_BACKUP_BUCKET=gs://<bucket>` (or the
   `s3://` / rclone form).

4. **Nightly cron**, with failures landing in a log you can check:

   ```sh
   crontab -e     # add:
   # 17 2 * * * cd $HOME/sotto/deploy && ./backup.sh >> $HOME/sotto-backup.log 2>&1
   ```

**Restore** (into a running instance; drops and recreates objects from the dump). Fetch the
dump with your provider's tool (`gsutil cp` / `aws s3 cp` / `rclone copyto`), then:

```sh
docker compose -f docker-compose.prod.yml exec -T postgres \
  pg_restore -U sotto -d sotto --clean --if-exists < sotto-<stamp>.dump
docker compose -f docker-compose.prod.yml restart server
```

Run one backup by hand now (`./backup.sh`) and rehearse the restore once against a scratch
database - a backup that has never been restored is a hope, not a backup.

## Access logs

Caddy writes JSON access logs to the `caddy_logs` volume (`/var/log/caddy/access.log` in the
container), rotated at 50 MiB, 10 files kept, 90 days retained (the `log` block in the
`Caddyfile`). Credential headers (`Cookie`, `Authorization`, `Set-Cookie`) are **deleted from
every entry by an explicit filter in the `Caddyfile`** - not left to Caddy's default redaction
- so no session material ever reaches disk. Request paths and statuses are logged.

The number that matters for a hosted instance - free-tier limit hits (HTTP 402, one per person
who wanted more than the free tier allows):

```sh
docker compose -f docker-compose.prod.yml exec caddy \
  sh -c 'grep -c "\"status\":402" /var/log/caddy/access.log'
```

## Uptime monitoring

`GET /health` returns `ok` with no auth and no rate limit - point any external checker at
`https://<SOTTO_DOMAIN>/health` (e.g. a free UptimeRobot monitor, 5-minute interval, keyword
`ok`). Alerting from *outside* the box is the point: a dead VM cannot report itself.

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

## Enabling organisation deletion

Deletion is irreversible once purge begins, so treat enablement as a release of its own. Work
through it on a staging deployment first, then repeat it on production with the same pinned image
tag. The full operator procedure, including the rehearsal record you must complete, is in
[ORGANISATION-DELETION-RUNBOOK.md](ORGANISATION-DELETION-RUNBOOK.md).

**Prerequisites** - all of these before either flag changes:

1. A managed backup or export lifecycle covering the configured recovery window, with a restore
   into an isolated scratch database rehearsed and recorded.
2. `SOTTO_ORGANISATION_DELETION_METRICS_TOKEN` set from the deployment secret store, the
   [alert rules](ORGANISATION-DELETION-ALERTS.yml) loaded, and one notification tested.
3. `SOTTO_ORGANISATION_DELETION_OPERATOR_TOKEN` set from the deployment secret store, with the
   authenticated observation procedure reviewed and rehearsed.
4. Billing configured and verified end to end: the provider's API version, restricted key, and
   webhook endpoint matching [Billing](#billing-optional).
5. `SOTTO_IMAGE_TAG` pinned to a released version, so the server and web images cannot skew.

**Enablement**, on staging first:

```sh
# in deploy/.env
SOTTO_ORGANISATION_DELETION_WORKER_ENABLED=1
VITE_ORGANISATION_DELETION_ENABLED=true
```

```sh
docker compose -f docker-compose.prod.yml up -d
```

The web flag is compiled in, so a source build (`up -d --build`) is what applies it; with prebuilt
images, use the image built with the flag set.

**Verify**, before repeating any of this on production:

- `https://<SOTTO_DOMAIN>/health` returns `ok`;
- the protected metrics endpoint answers with its token and `503` without one;
- the operator observation endpoint answers with its token, `503` without one, and `401` for a
  wrong one;
- an owner on a disposable test organisation can complete the confirmation flow, see the recovery
  window, and cancel it again;
- the audit trail and server logs show the request, cancellation, and operator observation, with
  no bearer token or provider text in them.

**Turning it back off** stops new requests and idles the worker, but does not restore an
organisation whose purge has already begun. Existing operations keep their stored `purge_after`
deadline; the routes return `404` again and the worker stops advancing the queue, leaving frozen
organisations frozen until deletion is re-enabled or an operator recovers them through the runbook.

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

## Billing (optional)

The server ships with Stripe billing dark: without the `STRIPE_*` variables, billing endpoints
return 503 and orgs are tiered manually. To turn it on:

1. In the Stripe dashboard: create a Product with one monthly Price (the flat per-org Team
   subscription) and note the `price_…` id.
2. Add a webhook endpoint for `https://<SOTTO_DOMAIN>/billing/webhook`, set its API version to
   `2026-07-29.dahlia`, and subscribe it to `checkout.session.completed`,
   `customer.subscription.updated`, and `customer.subscription.deleted`; note its `whsec_…`
   signing secret. The endpoint version must match the server's pinned Stripe version.
3. Fill `STRIPE_API_KEY`, `STRIPE_WEBHOOK_SECRET`, and `STRIPE_PRICE_ID` in `.env`, then
   `docker compose -f docker-compose.prod.yml up -d --force-recreate server`.

Card data never touches the server - checkout and subscription management happen on
Stripe-hosted pages, and the webhook only assigns the org's tier.

## Operations

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
