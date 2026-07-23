import { execFileSync } from "node:child_process";
import { writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));

// Runs once, after both `webServer` entries in playwright.config.ts report healthy: seeds two
// real accounts, an org, a project/environment with a secret, and a pending invite, by running
// the `e2e_seed` example binary (crates/cli/examples/e2e_seed.rs) against the just-started
// server. See e2e/README.md for how to build that binary and run this suite locally.
export default function globalSetup(): void {
  const seedBinary = path.resolve(__dirname, "../../target/debug/examples/e2e_seed");
  const serverUrl = "http://127.0.0.1:8099";

  const output = execFileSync(seedBinary, [serverUrl], {
    env: process.env,
    encoding: "utf-8",
  });
  writeFileSync(path.resolve(__dirname, ".fixture.json"), output);
}
