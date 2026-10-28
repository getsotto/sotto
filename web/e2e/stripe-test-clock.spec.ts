import { randomUUID } from "node:crypto";

import { expect, test } from "@playwright/test";

import { fixture, loginAs } from "./funnel-helpers";

const binding = {
  customerId: process.env.STRIPE_TEST_CLOCK_CUSTOMER_ID,
  clockId: process.env.STRIPE_TEST_CLOCK_ID,
  runId: process.env.STRIPE_TEST_CLOCK_RUN_ID,
};

const configured = Object.values(binding).every((value) => value && value.length > 0);

async function fillOptional(
  page: import("@playwright/test").Page,
  selector: string,
  value: string,
) {
  const field = page.locator(selector).first();
  if ((await field.count()) > 0 && (await field.isVisible())) await field.fill(value);
}

test("real personal Checkout reuses the runner-owned test-clock Customer", async ({ page }) => {
  test.skip(
    !configured,
    "set the three test-clock ids from the private run manifest to enable this real-Stripe case",
  );

  await page.goto("/app");
  await expect(page.getByRole("button", { name: "Log in with GitHub" })).toBeVisible();
  await loginAs(page, fixture.invitee_login_code);

  const checkout = await page.evaluate(
    async ({ customerId, clockId, runId, idempotencyKey, returnUrl }) => {
      const quoteResponse = await fetch(
        "/billing/personal/quote?offer=standard_monthly",
        { credentials: "include" },
      );
      if (!quoteResponse.ok) return { status: quoteResponse.status, body: await quoteResponse.text() };
      const quote = (await quoteResponse.json()) as {
        offer: string;
        quote_version: number;
        quote_expires_at_epoch: number;
      };
      const response = await fetch("/billing/personal/checkout", {
        method: "POST",
        credentials: "include",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          offer: quote.offer,
          idempotency_key: idempotencyKey,
          quote_version: quote.quote_version,
          quote_expires_at_epoch: quote.quote_expires_at_epoch,
          return_url: returnUrl,
          test_clock_customer: {
            customer_id: customerId,
            clock_id: clockId,
            run_id: runId,
          },
        }),
      });
      return {
        status: response.status,
        body: response.ok ? await response.json() : await response.text(),
      };
    },
    {
      ...binding,
      idempotencyKey: randomUUID(),
      returnUrl: new URL("/cloud", page.url()).origin,
    },
  );
  expect(checkout.status, String(checkout.body)).toBe(200);
  const result = checkout.body as { checkout_url?: string | null };
  expect(result.checkout_url).toMatch(/^https:\/\/checkout\.stripe\.com\//);

  await page.goto(result.checkout_url!);
  const cardOption = page.getByText("Card", { exact: true }).first();
  await expect(cardOption).toBeVisible();
  const cardOptionBox = await cardOption.boundingBox();
  expect(cardOptionBox).not.toBeNull();
  // Stripe Checkout's accordion cover intercepts normal locator clicks on its hidden radio.
  // Clicking the visible label's measured position selects Card without depending on internals.
  await page.mouse.click(
    cardOptionBox!.x + cardOptionBox!.width / 2,
    cardOptionBox!.y + cardOptionBox!.height / 2,
  );
  await expect(
    page.locator('input[name="cardNumber"], input[autocomplete="cc-number"]').first(),
  ).toBeVisible({ timeout: 60_000 });
  await fillOptional(
    page,
    'input[name="email"], input[type="email"]',
    `e2e-clock-${Date.now()}@sotto.test`,
  );
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
  await page.getByRole("button", { name: "Pay and subscribe" }).click();
  await page.waitForURL(/billing=success/, { timeout: 60_000 });

  await expect
    .poll(
      async () =>
        page.evaluate(async () => {
          const response = await fetch("/account/eligibility", { credentials: "include" });
          if (!response.ok) return `http ${response.status}`;
          const body = (await response.json()) as { state?: string };
          return body.state ?? "missing";
        }),
      { intervals: [2_000], timeout: 60_000 },
    )
    .toBe("paid");
});
