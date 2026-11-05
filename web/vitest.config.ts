import react from "@vitejs/plugin-react";
import { defineConfig } from "vitest/config";

// Unit tests under `src`, components (`.test.tsx`) and modules (`.test.ts`). Playwright tests live in
// `e2e` and generated WASM bindings in `src/wasm`, both outside this run. Kept apart from
// `vite.config.ts` so the build config (CSP, SRI, SEO prerender) stays untouched by the test environment.
export default defineConfig({
  plugins: [react()],
  test: {
    environment: "happy-dom",
    include: ["src/**/*.test.{ts,tsx}"],
    exclude: ["src/wasm/**"],
  },
});
