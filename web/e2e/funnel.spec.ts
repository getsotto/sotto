import { expect, test } from "@playwright/test";
import { guidePages } from "../src/seo/pages";
import {
  fixture,
  loginAndUnlock,
  selectOwnerOrganisation,
  unlockCurrentPage,
} from "./funnel-helpers";

for (const javaScriptEnabled of [true, false]) {
  test(`guide navigation follows the central list with scripting ${javaScriptEnabled ? "on" : "off"}`, async ({
    browser, baseURL,
  }) => {
    const context = await browser.newContext({ baseURL, javaScriptEnabled });
    try {
      const page = await context.newPage();
      await page.goto("/");
      // The static copy button is disabled. Wait for React when scripting is on.
      if (javaScriptEnabled) await expect(page.getByRole("button", { name: "Copy", exact: true })).toBeEnabled();
      const nav = page.getByRole("navigation", { name: "Guides", exact: true });
      const expected = guidePages.map((guide) => ({ href: `/${guide.slug}`, label: guide.navLabel }));
      const readLinks = () => nav.locator("a").evaluateAll((anchors) => anchors.map((anchor) => ({
        href: anchor.getAttribute("href"), label: anchor.textContent,
      })));
      expect(await readLinks()).toEqual(expected);
      for (const guide of guidePages) {
        await page.goto(`/${guide.slug}.html`);
        await expect(page.getByRole("heading", { name: guide.h1, exact: true })).toBeVisible();
        expect(await readLinks()).toEqual([
          ...expected.filter((link) => link.href !== `/${guide.slug}`),
          { href: "/#pricing", label: "Pricing" },
          { href: "https://github.com/getsotto/sotto", label: "GitHub" },
        ]);
      }
    } finally {
      await context.close();
    }
  });
}

// The funnel regression suite (Launch gate 4): login → unlock → TeamPanel invite → Upgrade →
// checkout handoff → return. See docs/OUTREACH.md and
// docs/adr/0001-continuous-deploy-during-launch-waves.md for why this suite exists, and
// e2e/README.md for how to run it locally. Asserts on observable UI state only - text, URL,
// visible elements - never component internals.

// An unreachable server must say so in words a worried person can act on. The browser's own
// "Failed to fetch" reads like the data failed rather than the connection, which in a secrets
// manager is the difference between an inconvenience and a catastrophe.
test("an unreachable server says so, rather than showing the browser's wording", async ({
  page,
}) => {
  // Fail the request at the network layer, which is what a stopped server looks like from here.
  // An HTTP error would not do: fetch resolves for those, and it is the rejection path being
  // tested.
  await page.route("**/auth/me", (route) => route.abort("connectionrefused"));

  await page.goto("/app");

  await expect(page.getByText(/could not reach the server/i)).toBeVisible();
  await expect(page.getByText(/says nothing about your data/i)).toBeVisible();
  await expect(page.getByText(/failed to fetch/i)).toHaveCount(0);
});

test("a body the app cannot read is not blamed on the network", async ({ page }) => {
  // Headers arrive and the body is nonsense. `fetch` has already resolved by then, so this
  // rejects in the body read rather than in the request, which is the half the wrapper was
  // extended to cover.
  //
  // It asserts the *other* branch of that wrapper, deliberately. A body that is present but
  // malformed is the server misbehaving, and telling somebody their connection failed would
  // send them to check their wifi over a server bug. A genuine mid-stream disconnect takes the
  // same wrapper and reports as unreachable; Playwright's routing cannot cut a response short
  // once it has begun, so that half is not reachable from here.
  await page.route("**/auth/me", async (route) => {
    await route.fulfill({
      status: 200,
      headers: { "content-type": "application/json" },
      body: '{"user_id": "trunc',
    });
  });

  await page.goto("/app");

  await expect(page.getByText(/could not read/i)).toBeVisible();
  await expect(page.getByText(/failed to fetch/i)).toHaveCount(0);
  await expect(page.getByText(/unexpected end of json/i)).toHaveCount(0);
});

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

// fetchCommunity() (web/src/api.ts) returns null for a rejected request, a non-2xx response, or a
// body that isn't valid JSON, and the landing page treats all three the same: hide the optional
// stats, keep everything else usable. One test per failure mode so a regression that only breaks
// one path (e.g. a JSON.parse change) doesn't hide behind the other two passing.
async function expectCommunityFallback(page: import("@playwright/test").Page) {
  await page.goto("/");
  await expect(page.getByRole("heading", { name: "Open source" })).toBeVisible();
  await expect(page.getByRole("link", { name: "Star on GitHub" })).toHaveAttribute(
    "href",
    "https://github.com/getsotto/sotto",
  );
  await expect(page.getByRole("link", { name: "Good first issues" })).toBeVisible();
  await expect(page.getByRole("link", { name: "Contributing" })).toBeVisible();
  await expect(page.locator(".community-stats")).toHaveCount(0);
  await expect(page.getByText(/curl -fsSL/)).toBeVisible();
  await expect(page.getByRole("button", { name: "Copy", exact: true }).first()).toBeVisible();
}

test("landing page falls back gracefully when the community request is rejected", async ({ page }) => {
  await page.route("**/community", (route) => route.abort());
  await expectCommunityFallback(page);
});

test("landing page falls back gracefully on a failed community response", async ({ page }) => {
  await page.route("**/community", async (route) => {
    await route.fulfill({ status: 503 });
  });
  await expectCommunityFallback(page);
});

test("landing page falls back gracefully when the community response is not JSON", async ({ page }) => {
  await page.route("**/community", async (route) => {
    await route.fulfill({ status: 200, contentType: "application/json", body: "not JSON" });
  });
  await expectCommunityFallback(page);
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
        contributor_count: 2,
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

// The snapshot gate: web/vite.config.ts inlines a static copy of the landing page into the
// built index.html for crawlers and visitors without scripting. With scripting off React
// never runs, so every assertion below reads the snapshot alone - a regression that empties
// or drifts it fails here, not in production.
test.describe("landing page prerender (no scripting)", () => {
  test.use({ javaScriptEnabled: false });

  test("crawlers and no-scripting visitors get the landing copy", async ({ page }) => {
    await page.goto("/");
    await expect(
      page.getByRole("heading", { name: "Stop Slacking your .env files." }),
    ).toBeVisible();
    await expect(page.getByText("curl -fsSL", { exact: false })).toBeVisible();
    await expect(page.getByRole("heading", { name: "How it works" })).toBeVisible();
    await expect(page.getByRole("heading", { name: "Should you trust this?" })).toBeVisible();
    await expect(page.getByRole("heading", { name: "Pricing" })).toBeVisible();
    await expect(page.getByRole("heading", { name: "Open source" })).toBeVisible();
    await expect(page.getByRole("heading", { name: "Get started" })).toBeVisible();
    // Terminal transcript and quickstart, one unique line each.
    await expect(
      page.getByText("share link (acme-api/dev) - burns after 1 view(s):", { exact: false }),
    ).toBeVisible();
    await expect(page.getByText("sotto login && sotto push", { exact: false })).toBeVisible();
    // Discovery metadata ships in the static head.
    await expect(page.locator('link[rel="canonical"]')).toHaveAttribute("href", /.+\/$/);
  });

  test("a configured status link reaches the snapshot, not just the React footer", async ({
    page,
  }) => {
    // The snapshot must stay text-identical to what <Landing> renders for the same content, so
    // a link that only one of them carries is cloaking, not a missing feature. It agrees
    // trivially when the variable is unset, which is why the e2e build sets it.
    await page.goto("/");
    await expect(page.getByRole("link", { name: "Status" })).toHaveAttribute(
      "href",
      "https://status.example.test",
    );
  });

  // One entry per guide route, loaded by file name: the bytes the build emits, which Caddy serves
  // at the clean path. With scripting off, only the prerendered copy can satisfy these.
  const guides = [
    {
      slug: "share-secrets-securely",
      h1: "Share secrets securely.",
      faq: "Can Sotto read the secrets I share?",
    },
    {
      slug: "share-env-files",
      h1: "Share .env files without the screenshot dance.",
      faq: "Do I have to delete my .env file?",
    },
    {
      slug: "one-time-secret-links",
      h1: "One-time links that burn after reading.",
      faq: "Can the secret be read twice?",
    },
    {
      slug: "share-api-keys-securely",
      h1: "Share API keys without pasting them into chat.",
      faq: "How does CI get secrets?",
    },
    {
      slug: "send-password-securely",
      h1: "Send a password that can only be read once.",
      faq: "Does my mum need to install anything?",
    },
    {
      slug: "self-hosted-secret-management",
      h1: "Secret management you can self-host.",
      faq: "What leaves my box?",
    },
  ] as const;

  for (const guide of guides) {
    test(`${guide.slug} serves its own prerendered page`, async ({ page }) => {
      await page.goto(`/${guide.slug}.html`);
      await expect(page.getByRole("heading", { name: guide.h1, exact: true })).toBeVisible();
      await expect(page.getByText(guide.faq, { exact: false })).toBeVisible();
      await expect(page.locator('link[rel="canonical"]')).toHaveAttribute(
        "href",
        new RegExp(`/${guide.slug}$`),
      );
    });
  }

  test("a trailing-slash guide URL redirects to the prerendered page, not the homepage", async ({
    page,
  }) => {
    // One slug stands in for all six: the edge (and vite preview, which mirrors it) must 301
    // `/<slug>/` onto the canonical clean path before try_files can fall through to index.html.
    await page.goto("/share-env-files/");
    await expect(page).toHaveURL(/\/share-env-files$/);
    await expect(
      page.getByRole("heading", { name: "Share .env files without the screenshot dance." }),
    ).toBeVisible();
    await expect(
      page.getByRole("heading", { name: "Stop Slacking your .env files." }),
    ).toHaveCount(0);
    await expect(page.locator('link[rel="canonical"]')).toHaveAttribute(
      "href",
      /\/share-env-files$/,
    );
  });
});

test("the status link survives React replacing the snapshot", async ({ page }) => {
  // Scripting on: React discards the prerendered markup and renders its own. Both paths have to
  // end up in the same place, which is the whole point of the snapshot contract.
  await page.goto("/");
  await expect(page.getByRole("link", { name: "Status" })).toHaveAttribute(
    "href",
    "https://status.example.test",
  );
});

test("guide trailing slashes redirect; SPA and .html paths do not", async ({ request }) => {
  const redirected = await request.get("/share-env-files/", { maxRedirects: 0 });
  expect(redirected.status()).toBe(301);
  expect(redirected.headers().location).toMatch(/\/share-env-files$/);

  const html = await request.get("/share-env-files.html", { maxRedirects: 0 });
  expect(html.status()).toBe(200);

  const app = await request.get("/app/", { maxRedirects: 0 });
  expect(app.status()).toBe(200);

  const unknown = await request.get("/definitely-not-a-guide/", { maxRedirects: 0 });
  expect(unknown.status()).toBe(200);
});

test("guide routes render their page client-side", async ({ page }) => {
  // Trailing slash is folded onto the canonical path at the edge (and in vite preview). After
  // the redirect, React still has to keep the guide on screen rather than replacing the
  // prerendered copy with the landing page. One route stands in for all six; the per-file
  // content is pinned by the no-scripting tests above.
  await page.goto("/share-env-files/");
  await expect(page).toHaveURL(/\/share-env-files$/);
  await expect(
    page.getByRole("heading", { name: "Share .env files without the screenshot dance." }),
  ).toBeVisible();
  await expect(
    page.getByText("Do I have to delete my .env file?", { exact: false }),
  ).toBeVisible();
  await expect(page.locator('link[rel="canonical"]')).toHaveAttribute(
    "href",
    /\/share-env-files$/,
  );
  // The direct file address renders the same guide: without the suffix strip
  // above, React would replace the prerendered guide with the landing page.
  await page.goto("/share-env-files.html");
  await expect(
    page.getByRole("heading", { name: "Share .env files without the screenshot dance." }),
  ).toBeVisible();
});
