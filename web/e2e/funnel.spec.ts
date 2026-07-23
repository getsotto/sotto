import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { expect, test } from "@playwright/test";

const __dirname = path.dirname(fileURLToPath(import.meta.url));

// The funnel regression suite (Launch gate 4): login → unlock → TeamPanel invite → Upgrade →
// Stripe checkout → return. See docs/OUTREACH.md and
// docs/adr/0001-continuous-deploy-during-launch-waves.md for why this suite exists, and
// e2e/README.md for how to run it locally. Asserts on observable UI state only - text, URL,
// visible elements - never component internals.

const SERVER_ORIGIN = "http://127.0.0.1:8099";

interface Fixture {
  owner_login_code: string;
  owner_password: string;
  owner_secret_key: string;
  invitee_login_code: string;
  invitee_user_id: string;
  invitee_email: string;
  org_id: string;
  project_name: string;
  secret_name: string;
  secret_value: string;
}

const fixture: Fixture = JSON.parse(
  readFileSync(path.resolve(__dirname, ".fixture.json"), "utf-8"),
);

// Drives the real "Log in with GitHub" click, but never lets the browser leave for real
// github.com in the first place: a browser-issued cross-origin *server* redirect (our own
// `/auth/github/login` 303-ing to github.com) isn't reliably interceptable mid-flight, so instead
// this intercepts the one same-origin request the click makes (`/auth/github/login`, same origin
// as the web app via the vite proxy) and fulfils it directly - after resolving the real CSRF
// `state` server-side out-of-band (a plain `fetch`, not browser-mediated) so the fulfilled
// redirect lands on a state the server actually issued. The mock `OAuthProvider`
// (`e2e-mock-oauth`) accepts `code` as the literal subject, so `code` selects which seeded
// identity this login resolves to.
async function loginAs(page: import("@playwright/test").Page, loginCode: string) {
  const context = page.context();
  await context.route("**/auth/github/login**", async (route) => {
    const clickedUrl = new URL(route.request().url());
    const loginUrl = new URL(`${SERVER_ORIGIN}/auth/github/login`);
    loginUrl.search = clickedUrl.search;

    // Resolve the real, server-issued CSRF state without the browser ever seeing this hop.
    const serverRedirect = await fetch(loginUrl, { redirect: "manual" });
    const serverState = new URL(serverRedirect.headers.get("location")!).searchParams.get(
      "state",
    )!;

    await route.fulfill({
      status: 303,
      headers: {
        location: `${SERVER_ORIGIN}/auth/github/callback?code=${loginCode}&state=${serverState}`,
      },
    });
  });
  await page.getByRole("button", { name: "Log in with GitHub" }).click();
  await page.waitForURL("**/app");
  await context.unroute("**/auth/github/login**");
}

test("login, unlock, invite, and upgrade", async ({ page }) => {
  await page.goto("/app");

  // --- Login ---
  await expect(page.getByRole("button", { name: "Log in with GitHub" })).toBeVisible();
  await loginAs(page, fixture.owner_login_code);

  // --- Unlock ---
  await expect(page.getByRole("heading", { name: "Unlock your vault" })).toBeVisible();
  await page.getByLabel("Master password").fill(fixture.owner_password);
  await page.getByLabel("Secret key (SK1-…)").fill(fixture.owner_secret_key);
  await page.getByRole("button", { name: "Unlock" }).click();
  await expect(page.getByRole("heading", { name: "Your vault" })).toBeVisible();
  await expect(page.getByRole("alert")).toHaveCount(0);

  // The seeded project is visible - proves the browser decrypted real, server-synced data, not
  // just that the unlock form accepted input.
  await expect(page.getByRole("button", { name: new RegExp(fixture.project_name) })).toBeVisible();

  // --- TeamPanel: select the org, invite the seeded invitee by email ---
  await expect(page.getByRole("heading", { name: "Organisations" })).toBeVisible();
  await page.getByRole("button", { name: /E2E Org/ }).click();
  await expect(page.getByRole("heading", { name: /^Members of/ })).toBeVisible();

  await page.getByLabel("Invite by email").fill(fixture.invitee_email);
  await page.getByRole("button", { name: "Invite" }).click();

  await expect(page.getByText(`invited ${fixture.invitee_email}`, { exact: false })).toBeVisible();
  // The member row (keyed by user id, not email - TeamPanel renders `m.userId`) has no
  // "no keys yet" marker: the invitee's public key (pushed by the seed fixture) resolved, so the
  // org-key grant went through cleanly, not just the bare invite.
  const invitedRow = page.getByRole("listitem").filter({ hasText: fixture.invitee_user_id });
  await expect(invitedRow).not.toContainText("no keys yet");

  // --- Upgrade → Stripe checkout (test mode) → return ---
  // Needs STRIPE_SECRET_KEY/STRIPE_WEBHOOK_SECRET/STRIPE_PRICE_ID (test-mode) in the environment
  // this suite runs under - the server ships checkout dark otherwise (billingEnabled: false) and
  // the button never renders. Provision these as CI secrets to enable this leg.
  const upgrade = page.getByRole("button", { name: "Upgrade to Team" });
  if (!(await upgrade.isVisible().catch(() => false))) {
    test.skip(true, "STRIPE_* test-mode credentials not configured in this environment");
  }

  await Promise.all([page.waitForURL(/checkout\.stripe\.com/), upgrade.click()]);

  // Stripe's own hosted Checkout page - stable, Stripe-documented test-mode UI, not ours.
  await page.getByPlaceholder("1234 1234 1234 1234").fill("4242424242424242");
  await page.getByPlaceholder("MM / YY").fill("12/34");
  await page.getByPlaceholder("CVC").fill("123");
  await page.getByLabel("Cardholder name").fill("E2E Test");
  await page.getByTestId("hosted-payment-submit-button").click();

  await page.waitForURL(/billing=success/);
  await expect(page.getByText("Payment received.")).toBeVisible();
});

// The cancelled return leg doesn't need a live Stripe session to verify: Stripe's own redirect is
// just a fresh page load carrying `?billing=cancelled`, and what's actually under test is the
// app's own handling of that outcome (TeamPanel.tsx's `parseBillingOutcome`/`clearBillingParam`),
// not Stripe's checkout UI. So this runs unconditionally, unlike the success leg above.
test("checkout cancelled return is handled", async ({ page }) => {
  await page.goto("/app");
  await loginAs(page, fixture.owner_login_code);
  await page.getByLabel("Master password").fill(fixture.owner_password);
  await page.getByLabel("Secret key (SK1-…)").fill(fixture.owner_secret_key);
  await page.getByRole("button", { name: "Unlock" }).click();
  await expect(page.getByRole("heading", { name: "Your vault" })).toBeVisible();

  // Simulate Stripe's redirect back after a cancelled checkout: a fresh load carrying the outcome.
  await page.goto("/app?billing=cancelled");
  await page.getByLabel("Master password").fill(fixture.owner_password);
  await page.getByLabel("Secret key (SK1-…)").fill(fixture.owner_secret_key);
  await page.getByRole("button", { name: "Unlock" }).click();

  await expect(page.getByText("Checkout cancelled. Nothing was charged.")).toBeVisible();
  // The consumed `billing` param is stripped so a reload doesn't repeat the banner.
  await expect(page).not.toHaveURL(/billing=cancelled/);
});
