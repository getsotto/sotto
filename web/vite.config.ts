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

// Pre-rendered SEO snapshot of the landing page (`/`). The source index.html ships an
// otherwise empty `<div id="root">`, which leaves crawlers and no-JS visitors with nothing.
// This plugin inlines a static snapshot of `src/Landing.tsx` into that div at build time;
// React replaces it on load (`createRoot`), so interactive users see no difference. The
// snapshot MUST stay text-identical to what <Landing> renders for the same content -
// showing crawlers different copy than users is cloaking. Source of truth: src/Landing.tsx.
// Two deliberate exceptions: the copy button is disabled (no clipboard without JS) and the
// community stats paragraph is omitted (it only appears after a live fetch, which crawlers
// never perform). (The snapshot carries no executable inline scripts or inline styles:
// the build-time CSP bans both. The JSON-LD block in index.html is inert data rather than
// an executable script, so script-src does not reach it.)
// Machine-readable metadata (canonical address, social tags, structured data, crawler rules,
// sitemap) carries the deployment's own origin from SOTTO_PUBLIC_URL at build time.
// Illustrative copy keeps the hosted addresses verbatim, exactly as <Landing> renders them -
// the transcript and quickstart are prose, not identity claims.
function publicOrigin(): string {
  const raw = (process.env.SOTTO_PUBLIC_URL ?? "https://getsotto.co.uk").trim().replace(/\/+$/, "");
  if (!/^https?:\/\/[^/]+$/i.test(raw)) {
    throw new Error(
      `sotto-seo: SOTTO_PUBLIC_URL must be a bare origin like https://example.co.uk, got ${JSON.stringify(process.env.SOTTO_PUBLIC_URL)}`,
    );
  }
  return raw;
}

function robotsTxt(origin: string): string {
  return `User-agent: *
Allow: /
Disallow: /app
Disallow: /s/
Disallow: /auth
Disallow: /account
Disallow: /projects
Disallow: /environments
Disallow: /orgs
Disallow: /machine
Disallow: /billing
Disallow: /shares
Disallow: /community
Disallow: /telemetry
Disallow: /health
Disallow: /ops

Sitemap: ${origin}/sitemap.xml
`;
}

function sitemapXml(origin: string): string {
  return `<?xml version="1.0" encoding="UTF-8"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
  <url>
    <loc>${origin}/</loc>
    <changefreq>weekly</changefreq>
    <priority>1.0</priority>
  </url>
</urlset>
`;
}
const SEO_SNAPSHOT = `<main class="landing">
<header><span class="wordmark">Sotto</span><nav aria-label="Site"><a href="#how">How it works</a><a href="#trust">Trust</a><a href="#pricing">Pricing</a><a href="#open-source">Contribute</a><a href="https://github.com/getsotto/sotto">GitHub</a><a class="login" href="/app">Log in</a></nav></header>
<section class="hero"><h1>Stop Slacking your <code>.env</code> files.</h1><p class="lead">Sotto syncs secrets across your team with end-to-end encryption. Values are encrypted on your machine before they leave it and decrypted only on your teammates’ machines. The server stores ciphertext it cannot read.</p><div class="install"><code>curl -fsSL https://raw.githubusercontent.com/getsotto/sotto/main/install.sh | sh</code><button class="sm" type="button" disabled aria-live="polite">Copy</button></div><p class="muted">Signed binaries for macOS and Linux. The installer verifies the checksum, and the Sigstore signature when <code>cosign</code> is installed. Prefer to <a href="https://github.com/getsotto/sotto/blob/main/install.sh">read it first</a>? Or grab a tarball from <a href="https://github.com/getsotto/sotto/releases">releases</a>.</p></section>
<pre class="term"><code>$ sotto init
  Save your Emergency Kit - these cannot be recovered:
    Secret Key:   SK1-9FKQ-XXXX-XXXX-XXXX
initialised &#96;acme-api&#96; (dev)

$ sotto set DATABASE_URL
Value:
set DATABASE_URL (acme-api/dev)

$ sotto run -- npm start
ready on http://localhost:3000

$ sotto push
pushed acme-api/dev - revision 1

$ sotto share DATABASE_URL
share link (acme-api/dev) - burns after 1 view(s):
https://getsotto.co.uk/s/9fK2xQ#k=Vq3TzEjm…

$ </code></pre>
<section id="how"><h2>How it works</h2><ol class="steps"><li><strong>Encrypt locally.</strong> Your vault key is derived on your machine from your master password and secret key. Neither is ever sent anywhere.</li><li><strong>Sync ciphertext.</strong> The server stores and versions encrypted blobs. It never receives a plaintext value or a usable key, so there is nothing on it worth stealing.</li><li><strong>Decrypt on your devices.</strong> One Rust crypto core runs everywhere: the CLI natively, the browser through WebAssembly, with golden vectors in CI proving both produce identical bytes.</li></ol><p>Teams work the same way: sharing an environment grants its key to a member (an X25519 sealed box), so access is cryptographic, not a permission bit on the server. Removing a member rotates the keys.</p></section>
<section id="trust"><h2>Should you trust this?</h2><p>Not blindly. Sotto is pre-1.0 and has <strong>not had a third-party cryptographic audit</strong> yet. You should know that before putting anything important in it. Here is what you can verify yourself, today:</p><ul><li>A published <a href="https://github.com/getsotto/sotto/blob/main/THREAT-MODEL.md">threat model</a> with explicit non-goals.</li><li>One shared crypto core: the CLI and the browser client run the same Rust code, held to byte-for-byte golden vectors in CI.</li><li>Sigstore-signed releases with a documented <a href="https://github.com/getsotto/sotto/blob/main/SECURITY.md">verification procedure</a>.</li><li>Telemetry is four anonymous fields, opt-out, and pinned by a unit test so it cannot quietly grow.</li><li>Apache-2.0, and self-hostable from one docker-compose.</li></ul><p>Honest guidance: use it for your team’s development and staging secrets today. Keep the production crown jewels where they are until the audit.</p></section>
<section id="pricing"><h2>Pricing</h2><div class="plans"><div class="plan"><h3>Free</h3><p class="price">£0</p><ul><li>Personal projects: unlimited, free forever</li><li>Organisations with up to 3 members and 1 shared project</li><li>One-time, burn-after-reading share links</li><li>Every new org starts a 14-day Team trial</li></ul></div><div class="plan"><h3>Team</h3><p class="price">£15<span class="per"> / month per organisation</span></p><ul><li>Unlimited members</li><li>Unlimited shared projects</li><li>Audit log</li><li>Flat: the price doesn’t scale with team size</li></ul></div></div><p class="muted">Or run it yourself: the <a href="https://github.com/getsotto/sotto/blob/main/deploy/README.md">server is self-hostable</a> and Apache-2.0. Self-hosting has no tiers.</p></section>
<section id="open-source"><h2>Open source</h2><p>Apache-2.0. One repo, one crypto core. Star it if Sotto saved you from pasting a <code>.env</code> into Slack; pick a good first issue if you want to help.</p><p class="community-actions"><a href="https://github.com/getsotto/sotto">Star on GitHub</a><a href="https://github.com/getsotto/sotto/issues?q=is%3Aissue+is%3Aopen+label%3A%22good+first+issue%22">Good first issues</a><a href="https://github.com/getsotto/sotto/blob/main/CONTRIBUTING.md">Contributing</a></p></section>
<section id="start"><h2>Get started</h2><pre class="quickstart"><code>sotto init                   # create your identity; SAVE the Emergency Kit
sotto set DATABASE_URL       # hidden prompt; encrypted before it touches disk
sotto import .env            # optional: pull in an existing file, still encrypted locally
sotto run -- npm start       # inject secrets into any command
sotto login &amp;&amp; sotto push    # optional: sync ciphertext via getsotto.co.uk
sotto share DATABASE_URL     # one-time link for a single secret</code></pre><p>Sotto works fully offline until you <code>sotto login</code>. Sync is a feature, not a requirement. The web vault at this address decrypts in your browser, with keys that never leave your devices.</p></section>
<footer><nav aria-label="Footer"><a href="https://github.com/getsotto/sotto">GitHub</a><a href="#open-source">Contribute</a><a href="https://github.com/getsotto/sotto/releases">Releases</a><a href="https://github.com/getsotto/sotto/blob/main/THREAT-MODEL.md">Threat model</a><a href="https://github.com/getsotto/sotto/blob/main/SECURITY.md">Security policy</a><a href="https://github.com/getsotto/sotto/blob/main/deploy/README.md">Run your own</a><a href="/app">Log in</a></nav><p class="muted">Sotto takes its name from <em>sotto voce</em>: in a low voice, in confidence. Apache-2.0.</p></footer>
</main>`;

// Tolerates reformatting of the root div (whitespace, extra attributes) but still fails
// the build when the mount point itself goes missing.
const ROOT_PATTERN = /<div\s+id="root"[^>]*>\s*<\/div>/;

function seoPrerenderPlugin(): Plugin {
  return {
    name: "sotto-seo-prerender",
    apply: "build",
    transformIndexHtml(html) {
      if (!ROOT_PATTERN.test(html)) {
        throw new Error("sotto-seo-prerender: <div id=\"root\"></div> not found in index.html");
      }
      const origin = publicOrigin();
      return html
        .replace(ROOT_PATTERN, `<div id="root">${SEO_SNAPSHOT}</div>`)
        .split("__SOTTO_PUBLIC_URL__")
        .join(origin);
    },
    generateBundle() {
      const origin = publicOrigin();
      this.emitFile({ type: "asset", fileName: "robots.txt", source: robotsTxt(origin) });
      this.emitFile({ type: "asset", fileName: "sitemap.xml", source: sitemapXml(origin) });
    },
  };
}

// Dev only: proxy the API endpoints so the browser talks to a single origin (keeps CSP
// `connect-src 'self'` and the session cookie same-origin). Production serves the web app and API
// from one origin (a reverse proxy). `/auth/callback` is intentionally NOT proxied - it's the SPA's
// post-login page, whereas `/auth/github/*` are the server's OAuth endpoints.
// `SOTTO_API_URL` overrides the target (the funnel regression suite runs its own server instance
// on a non-default port so it can't collide with one a developer already has running locally).
const api = {
  target: process.env.SOTTO_API_URL ?? "http://localhost:8080",
  changeOrigin: true,
};

const apiProxy = {
  "/auth/github": api,
  "/auth/me": api,
  "/auth/logout": api,
  "/account": api,
  "/projects": api,
  "/environments": api,
  "/orgs": api,
  "/shares": api,
  "/community": api,
};

export default defineConfig({
  plugins: [react(), cspPlugin(), sriPlugin(), seoPrerenderPlugin()],
  build: { target: "es2022" },
  server: { proxy: apiProxy },
  // Same proxy for `vite preview` (the built production bundle, not dev-server HMR) - the funnel
  // regression suite drives this against a real server, and needs the same single-origin
  // cookie/CSP behaviour production's Caddy topology provides, without standing up Caddy itself.
  preview: { proxy: apiProxy },
});
