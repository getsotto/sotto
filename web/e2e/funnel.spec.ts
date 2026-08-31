import { expect, test } from "@playwright/test";
import {
  fixture,
  loginAndUnlock,
  selectOwnerOrganisation,
  unlockCurrentPage,
} from "./funnel-helpers";

// The funnel regression suite (Launch gate 4): login → unlock → TeamPanel invite → Upgrade →
// checkout handoff → return. See docs/OUTREACH.md and
// docs/adr/0001-continuous-deploy-during-launch-waves.md for why this suite exists, and
// e2e/README.md for how to run it locally. Asserts on observable UI state only - text, URL,
// visible elements - never component internals.

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
  const membersList = page
    .getByRole("heading", { name: /^Members of/ })
    .locator("xpath=following-sibling::ul[1]");
  const invitedRow = membersList
    .getByRole("listitem")
    .filter({ hasText: fixture.invitee_user_id });
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

test("landing page offers a star and a contributor path", async ({ page }) => {
  await page.route("**/community", async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({
        stars: 12,
        forks: 3,
        repo_url: "https://github.com/getsotto/sotto",
        contributors: [
          { login: "alice", html_url: "https://github.com/alice", contributions: 8 },
          { login: "bob", html_url: "https://github.com/bob", contributions: 2 },
        ],
      }),
    });
  });
  await page.goto("/");
  await expect(page.getByRole("heading", { name: "Open source" })).toBeVisible();
  await expect(page.getByRole("link", { name: "Star on GitHub" })).toHaveAttribute(
    "href",
    "https://github.com/getsotto/sotto",
  );
  await expect(page.getByRole("link", { name: "Good first issues" })).toBeVisible();
  await expect(page.getByRole("link", { name: "Contributing" })).toBeVisible();
  await expect(page.getByText("12 stars · 3 forks · 2 contributors")).toBeVisible();
  await expect(page.getByRole("link", { name: "alice" })).toHaveAttribute(
    "href",
    "https://github.com/alice",
  );
  await expect(page.getByRole("link", { name: "bob" })).toHaveAttribute(
    "href",
    "https://github.com/bob",
  );
});

// Deletion is opt-in per deployment and the repository defaults keep it off, so a build made
// without `VITE_ORGANISATION_DELETION_ENABLED=true` must never show a destructive control. This
// suite builds with those defaults, which is what makes it the regression test for them.
test("organisation deletion stays disabled in a default build", async ({ page }) => {
  await loginAndUnlock(page);
  await selectOwnerOrganisation(page);

  await expect(page.getByRole("heading", { name: "Delete organisation" })).toBeVisible();
  await expect(
    page.getByText("Deletion controls are not enabled on this server yet.", { exact: true }),
  ).toBeVisible();
  await expect(page.getByRole("button", { name: "Request deletion" })).toHaveCount(0);
});
