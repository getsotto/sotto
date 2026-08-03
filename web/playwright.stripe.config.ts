import { defineConfig, devices } from "@playwright/test";

// Real Stripe smoke is intentionally opt-in. It runs the production billing adapter with test
// credentials, while the required pull-request gate uses playwright.config.ts and local fixtures.
const SERVER_PORT = 8099;
const WEB_PORT = 5199;

export default defineConfig({
  testDir: "./e2e",
  testMatch: /stripe\.spec\.ts/,
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
      command: "cargo run -p sotto-server --features e2e-mock-oauth",
      url: `http://127.0.0.1:${SERVER_PORT}/health`,
      reuseExistingServer: false,
      // The first feature-specific cargo run can rebuild the server and its dependencies.
      timeout: 180_000,
      env: {
        ...process.env,
        SOTTO_BIND: `127.0.0.1:${SERVER_PORT}`,
        SOTTO_PUBLIC_URL: `http://127.0.0.1:${SERVER_PORT}`,
        SOTTO_WEB_ORIGIN: `http://127.0.0.1:${WEB_PORT}`,
        GITHUB_CLIENT_ID: "e2e-mock",
        GITHUB_CLIENT_SECRET: "e2e-mock",
        SOTTO_TELEMETRY: "off",
      },
    },
    {
      command: `npm run preview -- --port ${WEB_PORT} --strictPort --host 127.0.0.1`,
      url: `http://127.0.0.1:${WEB_PORT}`,
      reuseExistingServer: false,
      env: { ...process.env, SOTTO_API_URL: `http://127.0.0.1:${SERVER_PORT}` },
    },
  ],
});
