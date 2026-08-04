import { expect, test } from "@playwright/test";

import {
  fixture,
  loginAndUnlock,
  selectOwnerOrganisation,
  unlockCurrentPage,
} from "./funnel-helpers";

// This spec is deliberately separate from funnel.spec.ts. It runs against the production Stripe
// adapter with test-mode credentials, while the required PR gate keeps its local billing adapter.

async function fillOptional(
  page: import("@playwright/test").Page,
  selector: string,
  value: string,
) {
  const field = page.locator(selector).first();
  // Stripe may omit optional billing fields depending on the account and payment configuration;
  // the required card fields below remain strict assertions.
  if ((await field.count()) > 0 && (await field.isVisible())) {
    await field.fill(value);
  }
}

test("real Stripe Checkout completes and applies the Team tier", async ({ page }) => {
  await loginAndUnlock(page);
  await selectOwnerOrganisation(page);

  const upgrade = page.getByRole("button", { name: "Upgrade to Team" });
  await expect(upgrade).toBeVisible();
  await upgrade.click();

  // Checkout is hosted by Stripe, so this proves the server handed the browser to the real
  // provider rather than the local e2e-mock-billing page.
  await page.waitForURL(/checkout\.stripe\.com/, { timeout: 60_000 });
  await expect(
    page.locator('input[name="cardNumber"], input[autocomplete="cc-number"]').first(),
  ).toBeVisible({ timeout: 60_000 });

  await fillOptional(page, 'input[name="email"], input[type="email"]', "e2e-stripe@sotto.test");
  await page
    .locator('input[name="cardNumber"], input[autocomplete="cc-number"]')
    .first()
    .fill("4242 4242 4242 4242");
  await page
    .locator('input[name="cardExpiry"], input[autocomplete="cc-exp"]')
    .first()
    .fill("12/34");
  await page
    .locator('input[name="cardCvc"], input[autocomplete="cc-csc"]')
    .first()
    .fill("123");
  await fillOptional(page, 'input[name="billingName"]', "Sotto E2E");
  await fillOptional(page, 'input[name="billingPostalCode"]', "94107");
  await fillOptional(page, 'input[name="phoneNumber"], input[autocomplete="tel"]', "4155552671");

  await page.getByRole("button", { name: /Pay|Subscribe|Start trial/i }).click();
  await page.waitForURL(/billing=success/, { timeout: 60_000 });
  await unlockCurrentPage(page);
  await expect(page.getByText("Payment received.")).toBeVisible();

  // Stripe delivers the entitlement asynchronously through the forwarded webhook. Poll the same
  // authenticated endpoint the TeamPanel uses, then reload once to prove the visible plan agrees.
  await expect
    .poll(
      async () =>
        page.evaluate(async (orgId) => {
          const response = await fetch(`/orgs/${encodeURIComponent(orgId)}/entitlements`, {
            credentials: "include",
          });
          if (!response.ok) return `status:${response.status}`;
          const body = (await response.json()) as {
            tier: string;
            effective_tier: string;
          };
          return body.tier === "team" && body.effective_tier === "team";
        }, fixture.org_id),
      { intervals: [2_000], timeout: 60_000 },
    )
    .toBe(true);

  await page.reload();
  await unlockCurrentPage(page);
  await selectOwnerOrganisation(page);
  await expect(page.getByText(/Plan:\s*team/)).toBeVisible();
});
