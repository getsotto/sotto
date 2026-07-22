import { createHash } from "node:crypto";

import react from "@vitejs/plugin-react";
import { defineConfig, type Plugin } from "vite";

// Strict Content-Security-Policy for the web client. `wasm-unsafe-eval` is required to instantiate
// WebAssembly; everything else is locked to same-origin with no inline scripts, no embedding, and
// no plugins. `connect-src` gains the API origin when network calls are introduced (PR4+).
const CSP = [
  "default-src 'self'",
  "script-src 'self' 'wasm-unsafe-eval'",
  "style-src 'self'",
  "img-src 'self'",
  "connect-src 'self'",
  "object-src 'none'",
  "base-uri 'none'",
  "frame-ancestors 'none'",
].join("; ");

// Inject the CSP into the *production* HTML only - the dev server needs inline HMR + a WebSocket,
// which a strict policy would block. Production hosting will also set this as a real header (PR7).
function cspPlugin(): Plugin {
  return {
    name: "sotto-csp",
    apply: "build",
    transformIndexHtml(html) {
      return html.replace(
        "</head>",
        `    <meta http-equiv="Content-Security-Policy" content="${CSP}" />\n  </head>`,
      );
    },
  };
}

// Add Subresource Integrity to every emitted <script>/<link> so the browser verifies each asset's
// bytes against a pinned hash - a compromised host/CDN can't swap in tampered code undetected. Runs
// post-build, hashing the final bundle output. (Vite already adds `crossorigin` to module scripts.)
function sriPlugin(): Plugin {
  return {
    name: "sotto-sri",
    apply: "build",
    transformIndexHtml: {
      order: "post",
      handler(html, ctx) {
        const bundle = ctx.bundle;
        if (bundle === undefined) {
          return html;
        }
        return html.replace(
          /<(?:script|link)\b[^>]*\b(?:src|href)="([^"]+)"[^>]*>/g,
          (tag, url: string) => {
            if (tag.includes("integrity=")) {
              return tag;
            }
            const entry = bundle[url.replace(/^\//, "")];
            if (entry === undefined) {
              return tag;
            }
            const source = entry.type === "chunk" ? entry.code : entry.source;
            const hash = createHash("sha384").update(source).digest("base64");
            const crossorigin = tag.includes("crossorigin") ? "" : ' crossorigin="anonymous"';
            return tag.replace(/\s*\/?>$/, ` integrity="sha384-${hash}"${crossorigin}>`);
          },
        );
      },
    },
  };
}

// Dev only: proxy the API endpoints so the browser talks to a single origin (keeps CSP
// `connect-src 'self'` and the session cookie same-origin). Production serves the web app and API
// from one origin (a reverse proxy). `/auth/callback` is intentionally NOT proxied - it's the SPA's
// post-login page, whereas `/auth/github/*` are the server's OAuth endpoints.
const api = { target: "http://localhost:8080", changeOrigin: true };

export default defineConfig({
  plugins: [react(), cspPlugin(), sriPlugin()],
  build: { target: "es2022" },
  server: {
    proxy: {
      "/auth/github": api,
      "/auth/me": api,
      "/auth/logout": api,
      "/account": api,
      "/projects": api,
      "/environments": api,
      "/orgs": api,
      "/shares": api,
    },
  },
});
