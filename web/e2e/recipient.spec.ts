import { createRequire } from "node:module";
import { expect, test, type Page } from "@playwright/test";

// Generate synthetic envelopes with the real crypto core, then decrypt them in
// the browser's WASM build. Only the API response is mocked.
const require = createRequire(import.meta.url);
const wasm = require("../.wasm-node/sotto_wasm.js") as {
    share_passphrase_key(fragment: Uint8Array, phrase: Uint8Array, salt: Uint8Array): Uint8Array;
    share_seal(key: Uint8Array, plaintext: Uint8Array): Uint8Array;
};

const secret = "  synthetic shared secret\nsecond line  ";
const passphrase = "synthetic correct passphrase";

function makeShare(protectedShare: boolean) {
    const fragment = new Uint8Array(32).fill(7);
    const salt = new Uint8Array(16).fill(8);
    const key = protectedShare
        ? wasm.share_passphrase_key(fragment, new TextEncoder().encode(passphrase), salt)
        : fragment;
    return {
        fragment: Buffer.from(fragment).toString("base64url"),
        payload: {
            enc_blob: Buffer.from(wasm.share_seal(key, new TextEncoder().encode(secret))).toString("base64"),
            passphrase_salt: protectedShare ? Buffer.from(salt).toString("base64") : null,
        },
    };
}

type ShareFixture = ReturnType<typeof makeShare>;
let protectedShare: ShareFixture;
let unprotectedShare: ShareFixture;

test.beforeAll(() => {
    protectedShare = makeShare(true);
    unprotectedShare = makeShare(false);
});

async function mockShare(page: Page, share: ShareFixture, waitForResponse?: () => Promise<void>) {
    let requests = 0;
    await page.route("**/shares/recipient-test", async (route) => {
        requests += 1;
        if (waitForResponse) await waitForResponse();
        // A second fetch fails like a real exhausted one-view link.
        await route.fulfill(requests === 1
            ? { status: 200, json: share.payload }
            : { status: 404, body: "share not found" });
    });
    await page.goto(`/s/recipient-test#${share.fragment}`);
    await expect(page.getByRole("button", { name: "Reveal secret" })).toBeVisible();
    return () => requests;
}

async function expectRevealed(page: Page) {
    await expect(page.getByRole("textbox", { name: "Shared secret" })).toHaveValue(secret);
    await expect(page.getByRole("heading", { name: "Shared secret" })).toBeFocused();
    await expect(page.getByRole("button", { name: "Copy secret" })).toBeVisible();
    await expect(page.getByRole("alert")).toHaveCount(0);
}

test("recipient page load and document prefetch do not claim a view", async ({ page }) => {
    const count = await mockShare(page, unprotectedShare);
    const prefetched = await page.request.get("/s/recipient-test", {
        headers: { Purpose: "prefetch", "Sec-Purpose": "prefetch" },
    });
    expect(prefetched.ok()).toBe(true);
    expect(prefetched.headers()["content-type"]).toContain("text/html");
    expect(count()).toBe(0);
    await page.getByRole("button", { name: "Reveal secret" }).click();
    await expectRevealed(page);
    expect(count()).toBe(1);
});

for (const protectedLink of [false, true]) {
    test(`successful reveal fetches once with passphrase protection ${protectedLink}`, async ({ page }) => {
        const count = await mockShare(page, protectedLink ? protectedShare : unprotectedShare);
        expect(count()).toBe(0);
        if (protectedLink) {
            await page.getByLabel("Passphrase (only if the sender set one)").fill(passphrase);
        }
        await page.getByRole("button", { name: "Reveal secret" }).click();
        await expectRevealed(page);
        expect(count()).toBe(1);
    });
}

for (const initialPhrase of ["", "synthetic wrong passphrase"]) {
    test(`corrects a ${initialPhrase === "" ? "missing" : "wrong"} passphrase without another fetch`, async ({ page }) => {
        const count = await mockShare(page, protectedShare);
        const input = page.getByLabel("Passphrase (only if the sender set one)");
        await input.fill(initialPhrase);
        await page.getByRole("button", { name: "Reveal secret" }).click();
        await expect(page.getByRole("alert")).toContainText(initialPhrase === ""
            ? "this link requires a passphrase"
            : "Couldn't reveal the secret:");
        await expect(page.getByRole("textbox", { name: "Shared secret" })).toHaveCount(0);
        expect(count()).toBe(1);

        await input.fill(passphrase);
        await page.getByRole("button", { name: "Reveal secret" }).click();
        await expectRevealed(page);
        expect(count()).toBe(1);
    });
}

test("concurrent submissions claim only one view", async ({ page }) => {
    let release!: () => void;
    const pending = new Promise<void>((resolve) => { release = resolve; });
    const count = await mockShare(page, unprotectedShare, () => pending);
    try {
        await page.locator("form").evaluate((form) => {
            form.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
            form.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
        });
        await expect.poll(count).toBe(1);
        await expect(page.getByRole("button", { name: "Revealing…" })).toBeDisabled();
        // Also exercise submissions while the HTTP response is pending.
        await page.locator("form").dispatchEvent("submit");
    } finally {
        release();
    }
    await expectRevealed(page);
    expect(count()).toBe(1);
});

test("navigation discards the previous share payload", async ({ page }) => {
    const count = await mockShare(page, protectedShare);
    await page.getByRole("button", { name: "Reveal secret" }).click();
    await expect(page.getByRole("alert")).toContainText("this link requires a passphrase");
    expect(count()).toBe(1);

    await page.route("**/shares/second-recipient", (route) => route.fulfill({ status: 404 }));
    await page.goto(`/s/second-recipient#${protectedShare.fragment}`);
    await page.getByLabel("Passphrase (only if the sender set one)").fill(passphrase);
    await page.getByRole("button", { name: "Reveal secret" }).click();
    await expect(page.getByRole("alert")).toHaveText(
        "This link is invalid, expired, revoked, or has already been viewed.",
    );
    await expect(page.getByRole("textbox", { name: "Shared secret" })).toHaveCount(0);
    expect(count()).toBe(1);
});

test("corrects a passphrase for a real one-view link after the server view is exhausted", async ({ page }) => {
    const { fixture, loginAs, SERVER_ORIGIN } = await import("./funnel-helpers");
    await page.goto("/app");
    await loginAs(page, fixture.owner_login_code);
    const api = page.context().request;
    const created = await api.post(`${SERVER_ORIGIN}/shares`, {
        // Use the server's default expiry so Free accounts keep their seven-day policy.
        data: { ...protectedShare.payload, max_views: 1 },
    });
    expect(created.status(), await created.text()).toBe(201);
    const { token } = await created.json() as { token: string };
    let browserFetches = 0;
    page.on("request", (request) => {
        if (request.method() === "GET" && new URL(request.url()).pathname === `/shares/${token}`) {
            browserFetches += 1;
        }
    });

    try {
        await page.goto(`/s/${token}#${protectedShare.fragment}`);
        expect(browserFetches).toBe(0);
        const input = page.getByLabel("Passphrase (only if the sender set one)");
        await input.fill("synthetic wrong passphrase");
        await page.getByRole("button", { name: "Reveal secret" }).click();
        await expect(page.getByRole("alert")).toContainText("Couldn't reveal the secret:");
        expect(browserFetches).toBe(1);

        // This separate API probe proves the view has already been consumed.
        expect((await api.get(`${SERVER_ORIGIN}/shares/${token}`)).status()).toBe(404);

        await input.fill(passphrase);
        await page.getByRole("button", { name: "Reveal secret" }).click();
        await expectRevealed(page);
        expect(browserFetches).toBe(1);
    } finally {
        const removed = await api.delete(`${SERVER_ORIGIN}/shares/${token}`);
        expect(removed.ok()).toBe(true);
    }
});
