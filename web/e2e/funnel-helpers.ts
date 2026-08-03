import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { expect } from "@playwright/test";

const __dirname = path.dirname(fileURLToPath(import.meta.url));

export const SERVER_ORIGIN = "http://127.0.0.1:8099";

export interface Fixture {
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

export const fixture: Fixture = JSON.parse(
  readFileSync(path.resolve(__dirname, ".fixture.json"), "utf-8"),
);

// Drives the real "Log in with GitHub" click, but never lets the browser leave for real
// github.com in the first place. The mock OAuth provider accepts the selected fixture code as its
// subject, while the server still issues and checks the normal CSRF state.
export async function loginAs(page: import("@playwright/test").Page, loginCode: string) {
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

export async function unlockCurrentPage(page: import("@playwright/test").Page) {
  await expect(page.getByRole("heading", { name: "Unlock your vault" })).toBeVisible();
  await page.getByLabel("Master password").fill(fixture.owner_password);
  await page.getByLabel("Secret key (SK1-…)").fill(fixture.owner_secret_key);
  await page.getByRole("button", { name: "Unlock" }).click();
  await expect(page.getByRole("heading", { name: "Your vault" })).toBeVisible();
  await expect(page.getByRole("alert")).toHaveCount(0);
}

export async function loginAndUnlock(page: import("@playwright/test").Page) {
  await page.goto("/app");
  await expect(page.getByRole("button", { name: "Log in with GitHub" })).toBeVisible();
  await loginAs(page, fixture.owner_login_code);
  await unlockCurrentPage(page);
}

export async function selectOwnerOrganisation(page: import("@playwright/test").Page) {
  await expect(page.getByRole("heading", { name: "Organisations" })).toBeVisible();
  await page.getByRole("button", { name: /E2E Org/ }).click();
  await expect(page.getByRole("heading", { name: /^Members of/ })).toBeVisible();
}
