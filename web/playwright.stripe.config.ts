import { defineConfig, devices } from "@playwright/test";

// Real Stripe smoke is intentionally opt-in. It runs the production billing adapter with test
// credentials, while the required pull-request gate uses playwright.config.ts and local fixtures.
const SERVER_ORIGIN = process.env.SOTTO_E2E_SERVER_ORIGIN ?? "http://127.0.0.1:8099";
const SERVER_PORT = new URL(SERVER_ORIGIN).port || "8099";
const WEB_PORT = 5199;
const USE_TEST_CLOCK_CHECKOUT = process.env.SOTTO_E2E_TEST_CLOCK_CHECKOUT_ENABLED === "1";
const SERVER_FEATURES = USE_TEST_CLOCK_CHECKOUT
  ? "e2e-mock-oauth,e2e-stripe-test-clock"
  : "e2e-mock-oauth";
const TEST_MATCH = USE_TEST_CLOCK_CHECKOUT
  ? /stripe(?:-test-clock)?\.spec\.ts/
  : /stripe\.spec\.ts/;

export default defineConfig({
  testDir: "./e2e",
  testMatch: TEST_MATCH,
  timeout: 120_000,
  fullyParallel: false,
  forbidOnly: true,
  retries: 0,
  workers: 1,
  // The workflow reads the JSON report after Playwright exits to reject a silently skipped smoke.
  reporter: process.env.CI
    ? [
        ["github"],
        ["json", { outputFile: "test-results/stripe-playwright.json" }],
        ["html", { open: "never" }],
      ]
    : "list",
  globalSetup: "./e2e/global-setup.ts",
  use: {
    baseURL: `http://127.0.0.1:${WEB_PORT}`,
    trace: "retain-on-failure",
  },
  projects: [{ name: "chromium", use: { ...devices["Desktop Chrome"] } }],
  webServer: [
    {
      // `cargo run`, not a bare path to the debug binary: a plain `cargo build -p sotto-server`
      // can overwrite the binary without e2e-mock-oauth, and the suite would then silently try
      // to authenticate against real GitHub. `cargo run` re-links when the feature set differs.
      // Do not include e2e-mock-billing here. The smoke must exercise StripeBilling and its real
      // Checkout redirect, with the webhook listener supplying STRIPE_WEBHOOK_SECRET.
      // Piped so the server's own warnings reach the run log. Playwright swallows this output
      // by default, which is why a webhook that arrived, was accepted and did nothing left no
      // trace anywhere: Stripe recorded a delivery, the test saw a tier that had not moved, and
      // the one process that knew why said it into a void.
      stdout: "pipe",
      stderr: "pipe",
      command: `cargo run -p sotto-server --features ${SERVER_FEATURES}`,
      url: `${SERVER_ORIGIN}/health`,
      reuseExistingServer: process.env.SOTTO_E2E_EXTERNAL_SERVER === "1",
      // The first feature-specific cargo run can rebuild the server and its dependencies.
      timeout: 180_000,
      env: {
        ...process.env,
        SOTTO_BIND: `127.0.0.1:${SERVER_PORT}`,
        SOTTO_PUBLIC_URL: SERVER_ORIGIN,
        SOTTO_WEB_ORIGIN: `http://127.0.0.1:${WEB_PORT}`,
        GITHUB_CLIENT_ID: "e2e-mock",
        GITHUB_CLIENT_SECRET: "e2e-mock",
        SOTTO_TELEMETRY: "off",
        SOTTO_E2E_TEST_CLOCK_CHECKOUT_ENABLED: USE_TEST_CLOCK_CHECKOUT ? "1" : "0",
      },
    },
    {
      command: `npm run preview -- --port ${WEB_PORT} --strictPort --host 127.0.0.1`,
      url: `http://127.0.0.1:${WEB_PORT}`,
      reuseExistingServer: false,
      env: { ...process.env, SOTTO_API_URL: SERVER_ORIGIN },
    },
  ],
});
