# Deploying Sotto

One command brings up a complete hosted instance: Postgres, the sync server, and Caddy serving
the web app with automatic HTTPS. Everything is built from this repository — the host needs only
Docker (with the compose plugin) and ports 80/443 open.

```text
internet ──▶ caddy (80/443, web app + API reverse proxy)
                 │ internal network only
                 ├──▶ server (axum, ciphertext-only API)
                 └──▶ ─┘ postgres (named volume)
```

The web app and API share **one origin** (`https://<SOTTO_DOMAIN>`), so the session cookie and
CSP stay same-origin and no CORS is involved. The server stores only ciphertext plus minimal
metadata — see [THREAT-MODEL.md](../THREAT-MODEL.md) — so the box hosts nothing that can decrypt
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

docker compose -f docker-compose.prod.yml up -d --build
```

The first build compiles the server and the WASM web client from source and takes several
minutes; subsequent builds reuse Docker layer caching. Database migrations run automatically on
server boot.

Smoke test:

```sh
curl -fsS https://<your-domain>/health    # → ok
```

Then open `https://<your-domain>` in a browser and sign in with GitHub. Point the CLI at your
instance with `sotto login --server https://<your-domain>`.

## Upgrading

```sh
git pull
docker compose -f docker-compose.prod.yml up -d --build
```

Migrations are forward-only and applied on boot. Check the release notes for anything flagged as
a compatibility break before upgrading past a minor version.

## Backups

Postgres holds only ciphertext and metadata, but losing it loses your users' synced vaults:

```sh
docker compose -f docker-compose.prod.yml exec postgres \
  pg_dump -U sotto sotto > "sotto-$(date +%F).sql"
```

Restore into a fresh instance with `psql -U sotto sotto < backup.sql` (via `exec -T postgres`).
Run the dump on a cron schedule and ship it off the box.

## Operations

```sh
docker compose -f docker-compose.prod.yml logs -f server   # API logs
docker compose -f docker-compose.prod.yml logs -f caddy    # access/TLS logs
docker compose -f docker-compose.prod.yml ps               # health at a glance
```

- Postgres is **not** exposed outside the compose network; only Caddy publishes ports.
- Certificates and Caddy state persist in the `caddy_data` volume; database data in `pgdata`.
- The API route list lives in the repo-root [`Caddyfile`](../Caddyfile) (baked into the Caddy
  image at build time) — rebuilding after `git pull` picks up route changes automatically.
- To try it without a public domain, set `SOTTO_DOMAIN=localhost`: Caddy serves a self-signed
  certificate (`curl -k https://localhost/health`). GitHub login still requires a callback URL
  reachable by your browser.
