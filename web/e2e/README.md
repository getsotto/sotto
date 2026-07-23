# Funnel regression suite

Launch gate 4 (see [`docs/OUTREACH.md`](../../docs/OUTREACH.md) and
[ADR 0001](../../docs/adr/0001-continuous-deploy-during-launch-waves.md)): the browser-layer
portion of the core funnel (install → login → org create → invite → hit the Wall → pay). The API
layer is already covered by `crates/server/tests/` and `crates/cli/tests/e2e.rs`, which already
gate merges via `ci.yml`'s `server` job - this suite covers what those can't reach: login,
unlock, TeamPanel, and Stripe checkout, exercised in a real browser.

## What it needs

- A Postgres reachable via `DATABASE_URL` (e.g. `docker compose up -d` from the repo root).
- The seed fixture example built, from the repo root: `cargo build -p sotto-cli --example e2e_seed`
  (the server binary doesn't need a separate manual build - see below).
- The web app's production bundle built (`npm run build`, from `web/`).
- Playwright's browser binaries: `npx playwright install --with-deps chromium` (once).

Then, from `web/`:

```sh
DATABASE_URL=postgres://sotto:sotto@localhost:5432/sotto npm run e2e
```

`playwright.config.ts` starts the server and `vite preview` for you. The server's `command` is
`cargo run -p sotto-server --features e2e-mock-oauth`, deliberately - not a bare path to the
debug binary - because a plain `cargo build -p sotto-server` run for any unrelated reason
overwrites that same binary path *without* the feature, and the suite would then silently try to
authenticate against real GitHub (a confusing timeout, not an obvious "wrong build" error).
`cargo run` re-links whenever the feature set differs from the last build, so this is safe
regardless of what else you've built locally; it also runs on a fixed non-default port so it
can't collide with a `cargo run -p sotto-server` you already have open for other work.

`globalSetup` runs `e2e_seed` once both servers are healthy, seeding two real accounts (an owner
and an invitee, each with genuine crypto material), an org, a project/environment with a secret,
and the invitee's account material (so they're inviteable) - see
[`crates/cli/examples/e2e_seed.rs`](../../crates/cli/examples/e2e_seed.rs) for the exact shape.
The spec drives the invite itself live, through the UI.

## Rerunning locally

CI always seeds against a fresh, ephemeral Postgres service container, so this never comes up
there. Locally, against a persistent Postgres, the seed script is idempotent for the **org** (it
reuses one named `E2E Org` rather than creating a duplicate) but **not** for the owner's project:
each run pushes from a fresh in-memory store, so the project accumulates a fresh copy every time,
and the suite's `e2e-project` locator then matches more than one. Wipe the relevant tables before
rerunning locally:

```sh
docker compose exec postgres psql -U sotto -d sotto -c \
  "TRUNCATE users, organizations, organization_memberships, projects, environments CASCADE;"
```

## The mock login

`e2e-mock-oauth` is a `sotto-server` Cargo feature, off by default - it does not exist in
`release.yml` builds or the production Docker image, so it can never ship. When compiled in, the
server's `OAuthProvider` resolves any authorisation `code` to an identity whose subject **is**
that code, verbatim (see `MockOAuth::exchange_code`). `login()`'s redirect target is still the
real `https://github.com/login/oauth/authorize` regardless of the feature, so the spec never lets
the browser reach it: it intercepts the one *same-origin* request the click makes
(`/auth/github/login`), resolves the real server-issued CSRF state out-of-band with a plain
`fetch` (not browser-mediated), and fulfils the browser's request directly with a redirect
straight to the server's own callback endpoint - using whichever seeded login code it wants to
authenticate as. No real GitHub involved, and the login/callback handlers themselves are
untouched. (An earlier attempt intercepted the cross-origin redirect to `github.com` directly;
that isn't reliably interceptable mid-navigation, hence the same-origin approach instead.)

## The Stripe checkout leg

The "Upgrade to Team" → Stripe Checkout (test mode) → return leg needs `STRIPE_SECRET_KEY`,
`STRIPE_WEBHOOK_SECRET`, and `STRIPE_PRICE_ID` (test-mode values) in the environment this suite
runs under - without them the server ships checkout dark (`billingEnabled: false`) and the
"Upgrade to Team" button never renders, so the spec skips that leg with a clear reason rather
than failing. Provision test-mode Stripe credentials as CI secrets to enable full coverage.
