import react from "@vitejs/plugin-react";
import { defineConfig } from "vitest/config";

// Component tests only. Kept apart from `vite.config.ts` so the build config (CSP, SRI, SEO
// prerender) stays untouched by the test environment.
export default defineConfig({
  plugins: [react()],
  test: {
    environment: "happy-dom",
    include: ["src/**/*.test.tsx"],
  },
});
