import { defineConfig, devices } from "@playwright/test";

// The funnel regression suite (Launch gate 4 - see docs/OUTREACH.md and
// docs/adr/0001-continuous-deploy-during-launch-waves.md). Runs against a real, already-built
// web app and a real `sotto-server` process (built with `--features e2e-mock-oauth`, see
// crates/server/src/auth/mock_oauth.rs) - both started here, against a Postgres the caller must
// already have running (`DATABASE_URL` in the environment; see e2e/README.md).
//
// `vite preview` serves the actual production bundle (`vite build` must have already run), not
// dev-server HMR, so this exercises the same asset pipeline production does.
const SERVER_PORT = 8099;
const WEB_PORT = 5199;

export default defineConfig({
  testDir: "./e2e",
  fullyParallel: false, // shared server + Postgres state; the funnel is one linear story
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 1 : 0,
  workers: 1,
  reporter: process.env.CI ? [["github"], ["html", { open: "never" }]] : "list",
  globalSetup: "./e2e/global-setup.ts",
  use: {
    baseURL: `http://127.0.0.1:${WEB_PORT}`,
    trace: "retain-on-failure",
  },
  projects: [{ name: "chromium", use: { ...devices["Desktop Chrome"] } }],
  webServer: [
    {
      // `cargo run`, not a bare path to the debug binary: a plain `cargo build -p sotto-server`
      // run for any other reason (e.g. while working on an unrelated change) overwrites the same
      // binary path without the `e2e-mock-oauth` feature, and the suite would then silently try
      // to authenticate against real GitHub - a confusing timeout, not an obvious "wrong build"
      // error. `cargo run` re-links whenever the feature set differs from the last build.
      command: "cargo run -p sotto-server --features e2e-mock-oauth",
      url: `http://127.0.0.1:${SERVER_PORT}/health`,
      reuseExistingServer: !process.env.CI,
      // `cargo run`'s cold-compile path can comfortably exceed the 60s default when the feature
      // flag changed since the last build (a full sotto-server + its deps rebuild).
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
      reuseExistingServer: !process.env.CI,
      env: { ...process.env, SOTTO_API_URL: `http://127.0.0.1:${SERVER_PORT}` },
    },
  ],
});
