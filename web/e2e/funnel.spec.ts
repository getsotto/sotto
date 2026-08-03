import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { expect, test } from "@playwright/test";

const __dirname = path.dirname(fileURLToPath(import.meta.url));

// The funnel regression suite (Launch gate 4): login → unlock → TeamPanel invite → Upgrade →
// checkout handoff → return. See docs/OUTREACH.md and
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

async function unlockCurrentPage(page: import("@playwright/test").Page) {
  await expect(page.getByRole("heading", { name: "Unlock your vault" })).toBeVisible();
  await page.getByLabel("Master password").fill(fixture.owner_password);
  await page.getByLabel("Secret key (SK1-…)").fill(fixture.owner_secret_key);
  await page.getByRole("button", { name: "Unlock" }).click();
  await expect(page.getByRole("heading", { name: "Your vault" })).toBeVisible();
  await expect(page.getByRole("alert")).toHaveCount(0);
}

async function loginAndUnlock(page: import("@playwright/test").Page) {
  await page.goto("/app");
  await expect(page.getByRole("button", { name: "Log in with GitHub" })).toBeVisible();
  await loginAs(page, fixture.owner_login_code);
  await unlockCurrentPage(page);
}

async function selectOwnerOrganisation(page: import("@playwright/test").Page) {
  await expect(page.getByRole("heading", { name: "Organisations" })).toBeVisible();
  await page.getByRole("button", { name: /E2E Org/ }).click();
  await expect(page.getByRole("heading", { name: /^Members of/ })).toBeVisible();
}

test("login, unlock, invite, and checkout", async ({ page }) => {
  await loginAndUnlock(page);

  // The seeded project is visible - proves the browser decrypted real, server-synced data, not
  // just that the unlock form accepted input.
  await expect(page.getByRole("button", { name: new RegExp(fixture.project_name) })).toBeVisible();

  await selectOwnerOrganisation(page);

  // The member row (keyed by user id, not email - TeamPanel renders `m.userId`) has no
  // "no keys yet" marker: the invitee's public key (pushed by the seed fixture) resolved, so the
  // org-key grant went through cleanly, not just the bare invite. Reusing an existing row makes a
  // CI retry safe if the first attempt completed the invite before a later assertion failed.
  // Wait for the member fetch to settle before deciding whether the row already exists; otherwise
  // a retry can mistake the loading state for an absent invite and submit a duplicate.
  const memberLoading = page
    .getByRole("heading", { name: /^Members of/ })
    .locator("xpath=following-sibling::p[normalize-space()='Loading…']");
  await expect(memberLoading).toHaveCount(0);
  const invitedRow = page.getByRole("listitem").filter({ hasText: fixture.invitee_user_id });
  if (!(await invitedRow.isVisible().catch(() => false))) {
    await page.getByLabel("Invite by email").fill(fixture.invitee_email);
    await page.getByRole("button", { name: "Invite" }).click();
    await expect(
      page.getByText(`invited ${fixture.invitee_email}`, { exact: false }),
    ).toBeVisible();
  }
  await expect(invitedRow).toBeVisible();
  await expect(invitedRow).not.toContainText("no keys yet");

  // The seeded organisation is still free because the test billing adapter does not emit a
  // webhook. That keeps the Team upgrade control available for the rest of this linear funnel.
  const upgrade = page.getByRole("button", { name: "Upgrade to Team" });
  await expect(upgrade).toBeVisible();
  await Promise.all([page.waitForURL(/\/e2e\/billing\/checkout/), upgrade.click()]);
  await page.getByRole("link", { name: "Complete payment" }).click();

  await page.waitForURL(/billing=success/);
  await unlockCurrentPage(page);
  await expect(page.getByText("Payment received.")).toBeVisible();
});

test("checkout cancelled return is handled", async ({ page }) => {
  await loginAndUnlock(page);
  await selectOwnerOrganisation(page);
  const upgrade = page.getByRole("button", { name: "Upgrade to Team" });
  await expect(upgrade).toBeVisible();
  await Promise.all([page.waitForURL(/\/e2e\/billing\/checkout/), upgrade.click()]);
  await page.getByRole("link", { name: "Cancel payment" }).click();

  await page.waitForURL(/billing=cancelled/);
  await unlockCurrentPage(page);
  await expect(page.getByText("Checkout cancelled. Nothing was charged.")).toBeVisible();
  // The consumed `billing` param is stripped so a reload doesn't repeat the banner.
  await expect(page).not.toHaveURL(/billing=cancelled/);
});
