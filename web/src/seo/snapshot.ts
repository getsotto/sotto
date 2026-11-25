// Guide page HTML & structured data: the single renderer for guide routes
// (`/<slug>`) and machine-readable SEO outputs (JSON-LD, llms.txt).
//
// React (`SeoPage`) and the build (`seoPrerenderPlugin`) both call
// `snapshotFor`, so the prerendered copy and the interactive page are the
// same string by construction.
//
// The content below is authored constants from `pages.ts`, never user input,
// so building HTML by string concatenation is safe. Everything is escaped on
// the way out regardless.
//
import { guidePages, landingFaqs, type SeoPageData, type SeoTerminalLine } from "./pages";

export const SITE_TITLE = "Sotto - Open-Source Encrypted Secret Manager for Developers";
export const SITE_DESCRIPTION =
  "Sotto (getsotto) is an open-source, zero-knowledge secret manager for developer teams. Sync .env files, inject CLI secrets, self-host, or send one-time links.";

function escapeHtml(text: string): string {
  return text
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

function isSafeHref(href: string): boolean {
  return (
    href.startsWith("https://") ||
    href.startsWith("http://") ||
    href.startsWith("/") ||
    href.startsWith("#")
  );
}

// Prose fields: escaped, with safe `[label](url)` links promoted to <a> and
// `backticks` promoted to <code>. Links are matched before backticks so
// backticks inside link labels still render as code, while unsafe schemes stay
// literal escaped text.
export function inlineHtml(text: string): string {
  const linkPattern = /\[([^\]]+)\]\(([^)\s]+)\)/g;
  let result = "";
  let lastIndex = 0;
  let match: RegExpExecArray | null;
  while ((match = linkPattern.exec(text)) !== null) {
    const [full, label, href] = match;
    result += renderCodeOnly(text.slice(lastIndex, match.index));
    if (isSafeHref(href)) {
      result += `<a href="${escapeHtml(href)}">${renderCodeOnly(label)}</a>`;
    } else {
      result += renderCodeOnly(full);
    }
    lastIndex = match.index + full.length;
  }
  result += renderCodeOnly(text.slice(lastIndex));
  return result;
}

function renderCodeOnly(text: string): string {
  return escapeHtml(text).replace(/`([^`]+)`/g, "<code>$1</code>");
}

// Strip inline markup (`code` and `[label](url)`) for JSON-LD plain text.
export function plainText(text: string): string {
  return text
    .replace(/\[([^\]]+)\]\(([^)\s]+)\)/g, "$1")
    .replace(/`/g, "");
}

function jsonLdScript(data: unknown): string {
  // `<` is escaped so that no answer can close the script element early.
  return `<script type="application/ld+json">${JSON.stringify(data).replace(/</g, "\\u003c")}</script>`;
}

const TERMINAL_PROMPT = '<span class="dim">$</span> ';

function terminalHtml(lines: SeoTerminalLine[]): string {
  const rendered = lines.map((line) => {
    const text = escapeHtml(line.text);
    switch (line.kind) {
      case "cmd":
        return `${TERMINAL_PROMPT}${text}`;
      case "dim":
        return `<span class="dim">${text}</span>`;
      case "value":
        return `<span class="value">${text}</span>`;
    }
  });
  return `<pre class="term"><code>${rendered.join("\n")}</code></pre>`;
}

export function guideLinksHtml(excludeSlug?: string): string {
  return guidePages
    .filter((page) => page.slug !== excludeSlug)
    .map((page) => `<a href="/${escapeHtml(page.slug)}">${escapeHtml(page.navLabel)}</a>`)
    .join("");
}

function footerHtml(page: SeoPageData): string {
  const guides = guideLinksHtml(page.slug);
  return `<footer><nav aria-label="Guides">${guides}<a href="/#pricing">Pricing</a><a href="https://github.com/getsotto/sotto">GitHub</a></nav><p class="muted">Sotto (getsotto): open-source zero-knowledge secret manager. Apache-2.0.</p></footer>`;
}

export function snapshotFor(page: SeoPageData): string {
  const steps = page.steps
    .map((step) => `<li><strong>${inlineHtml(step.head)}</strong> ${inlineHtml(step.body)}</li>`)
    .join("");
  const faqs = page.faqs
    .map(
      (faq, index) =>
        `<details${index === 0 ? " open" : ""}><summary>${escapeHtml(faq.q)}</summary><p>${inlineHtml(faq.a)}</p></details>`,
    )
    .join("");
  return `<main class="seo">
<header class="mast"><a class="wordmark" href="/">Sotto</a><nav class="site" aria-label="Site"><a href="/#how">How it works</a><a href="/#pricing">Pricing</a><a href="https://github.com/getsotto/sotto">GitHub</a><a href="/app">Log in</a></nav></header>
<nav class="breadcrumb" aria-label="Breadcrumb"><a href="/">Sotto</a><span aria-hidden="true">/</span><span aria-current="page">${escapeHtml(page.navLabel)}</span></nav>
<section class="plain"><h1>${escapeHtml(page.h1)}</h1><p class="lead">${inlineHtml(page.lead)}</p><div class="cta-row"><a class="btn primary" href="/#start">Get Sotto</a><a class="btn ghost" href="${escapeHtml(page.ctaSecondary.href)}">${escapeHtml(page.ctaSecondary.label)}</a></div></section>
<section><h2>${escapeHtml(page.stepsTitle)}</h2><ol class="steps">${steps}</ol>${terminalHtml(page.terminal)}</section>
<section><h2>Questions, answered</h2>${faqs}</section>
<section><h2>${escapeHtml(page.closingTitle)}</h2><p>${inlineHtml(page.closingBody)}</p><div class="cta-row"><a class="btn primary" href="/#start">Get Sotto</a></div></section>
${footerHtml(page)}
</main>`;
}

// FAQ structured data for the guide page head. Plain text answers - backticks
// and markdown links are markup for the visible page, not for the data block.
export function faqJsonLd(page: SeoPageData): string {
  return jsonLdScript({
    "@context": "https://schema.org",
    "@type": "FAQPage",
    mainEntity: page.faqs.map((faq) => ({
      "@type": "Question",
      name: plainText(faq.q),
      acceptedAnswer: { "@type": "Answer", text: plainText(faq.a) },
    })),
  });
}

function orgNode(origin: string) {
  return {
    "@type": "Organization",
    "@id": `${origin}/#org`,
    name: "Sotto",
    alternateName: ["getsotto", "Sotto Secret Manager"],
    url: `${origin}/`,
    description:
      "Makers of Sotto (getsotto), an open-source, zero-knowledge secret manager for developer teams.",
    logo: {
      "@type": "ImageObject",
      url: `${origin}/icon-512.png`,
      width: 512,
      height: 512,
    },
    sameAs: [
      "https://github.com/getsotto",
      "https://github.com/getsotto/sotto",
      "https://github.com/getsotto/homebrew-tap",
      "https://github.com/getsotto/sotto-action",
      "https://github.com/getsotto/sotto-example",
    ],
  };
}

function websiteNode(origin: string) {
  return {
    "@type": "WebSite",
    "@id": `${origin}/#website`,
    url: `${origin}/`,
    name: "Sotto",
    alternateName: ["getsotto", "getsotto.co.uk", "Sotto Secret Manager"],
    description: SITE_DESCRIPTION,
    inLanguage: "en",
    publisher: { "@id": `${origin}/#org` },
  };
}

function softwareNode(origin: string, version: string, cloudPricingVisible: boolean) {
  const freeOffer = {
    "@type": "Offer",
    name: "Local and self-hosted",
    price: "0",
    priceCurrency: "GBP",
    availability: "https://schema.org/InStock",
  };
  const offers = cloudPricingVisible
    ? [
        freeOffer,
        {
          "@type": "Offer",
          name: "Sotto Cloud (Monthly)",
          price: "2.99",
          priceCurrency: "GBP",
          availability: "https://schema.org/InStock",
          priceSpecification: {
            "@type": "UnitPriceSpecification",
            price: "2.99",
            priceCurrency: "GBP",
            unitCode: "MON",
          },
        },
        {
          "@type": "Offer",
          name: "Sotto Cloud (Annual)",
          price: "29.99",
          priceCurrency: "GBP",
          availability: "https://schema.org/InStock",
          priceSpecification: {
            "@type": "UnitPriceSpecification",
            price: "29.99",
            priceCurrency: "GBP",
            unitCode: "ANN",
          },
        },
      ]
    : freeOffer;

  return {
    "@type": "SoftwareApplication",
    "@id": `${origin}/#software`,
    name: "Sotto",
    alternateName: ["getsotto", "Sotto CLI", "Sotto Secret Manager"],
    url: `${origin}/`,
    description: SITE_DESCRIPTION,
    applicationCategory: "DeveloperApplication",
    applicationSubCategory: "Secret Manager",
    operatingSystem: ["macOS", "Linux", "Windows", "Web"],
    softwareVersion: version,
    license: "https://www.apache.org/licenses/LICENSE-2.0",
    isAccessibleForFree: true,
    downloadUrl: "https://github.com/getsotto/sotto/releases",
    image: `${origin}/og-image.png`,
    screenshot: `${origin}/og-image.png`,
    keywords:
      "secret manager, secrets manager, open-source secret manager, CLI secret manager, zero-knowledge secret manager, encrypted secret manager, self-hosted secret manager, dotenv alternative, .env manager, one-time secret links, sotto, getsotto",
    featureList: [
      "Client-side XChaCha20-Poly1305 encryption and Argon2id key derivation",
      "Single Rust crypto core shared across native CLI and browser WebAssembly vault",
      "Import, diff, sync, and inject .env files and secrets with sotto run",
      "Team organisations with X25519 cryptographic environment grants and key rotation",
      "Scoped, expiring machine tokens (SOTTO_TOKEN) and GitHub Actions integration",
      "Burn-after-reading one-time secret links with optional passphrase",
      "Self-hostable Apache-2.0 server via Docker Compose",
    ],
    offers,
    publisher: { "@id": `${origin}/#org` },
  };
}

export function landingJsonLd(
  origin: string,
  version: string,
  cloudPricingVisible: boolean,
): string {
  return jsonLdScript({
    "@context": "https://schema.org",
    "@graph": [
      websiteNode(origin),
      orgNode(origin),
      softwareNode(origin, version, cloudPricingVisible),
      {
        "@type": "FAQPage",
        "@id": `${origin}/#faq`,
        mainEntity: landingFaqs.map((faq) => ({
          "@type": "Question",
          name: plainText(faq.q),
          acceptedAnswer: { "@type": "Answer", text: plainText(faq.a) },
        })),
      },
    ],
  });
}

export function guideJsonLd(page: SeoPageData, origin: string): string {
  const url = `${origin}/${page.slug}`;
  return jsonLdScript({
    "@context": "https://schema.org",
    "@graph": [
      websiteNode(origin),
      orgNode(origin),
      {
        "@type": "TechArticle",
        "@id": `${url}#article`,
        headline: plainText(page.h1),
        name: plainText(page.tabTitle),
        description: plainText(page.description),
        keywords: page.keywords.join(", "),
        url,
        inLanguage: "en",
        image: `${origin}/og-image.png`,
        mainEntityOfPage: { "@type": "WebPage", "@id": url },
        author: { "@id": `${origin}/#org` },
        publisher: { "@id": `${origin}/#org` },
        about: { "@id": `${origin}/#software` },
      },
      {
        "@type": "BreadcrumbList",
        "@id": `${url}#breadcrumb`,
        itemListElement: [
          {
            "@type": "ListItem",
            position: 1,
            name: "Sotto",
            item: `${origin}/`,
          },
          {
            "@type": "ListItem",
            position: 2,
            name: page.navLabel,
            item: url,
          },
        ],
      },
      {
        "@type": "HowTo",
        "@id": `${url}#howto`,
        name: plainText(page.stepsTitle),
        description: plainText(page.description),
        step: page.steps.map((step, index) => ({
          "@type": "HowToStep",
          position: index + 1,
          name: plainText(step.head),
          text: plainText(step.body),
        })),
      },
      {
        "@type": "FAQPage",
        "@id": `${url}#faq`,
        mainEntity: page.faqs.map((faq) => ({
          "@type": "Question",
          name: plainText(faq.q),
          acceptedAnswer: { "@type": "Answer", text: plainText(faq.a) },
        })),
      },
    ],
  });
}

export function llmsTxt(origin: string): string {
  const guides = guidePages
    .map((page) => `- [${page.navLabel}](${origin}/${page.slug}): ${page.description}`)
    .join("\n");
  return `# Sotto (getsotto)

> Sotto is an open-source, zero-knowledge secret manager for developer teams. Plaintext secrets and .env files are encrypted on the user's machine before they leave it; the server only ever stores ciphertext.

## Summary

- Official website: ${origin}/
- GitHub organisation: https://github.com/getsotto
- Primary repository: https://github.com/getsotto/sotto
- Licence: Apache-2.0
- Platforms: macOS (Apple Silicon & Intel), Linux (x86_64 & ARM64), Windows (x86_64), and browser WebAssembly vault
- Cryptography: Single Rust crypto core (Argon2id KDF, XChaCha20-Poly1305 AEAD, X25519 sealed-box team grants) shared across CLI and WebAssembly, verified by byte-for-byte golden vectors in CI
- Security note: Pre-1.0 software; has not yet undergone a third-party cryptographic audit. Recommended for development and staging secrets until audited.

## Ways to run Sotto

- Local: Free, offline-first CLI with an encrypted vault on your machine. No account and no server required.
- Self-hosted: Free Apache-2.0 sync server via Docker Compose (server, Postgres, Caddy) with no plan limits.
- Sotto Cloud: Managed sync service at ${origin}/.

## Core CLI commands

- \`sotto init\` - initialise identity and project (prints one-time Emergency Kit)
- \`sotto set KEY\` - store a secret via hidden prompt
- \`sotto import .env\` - import an existing .env file into the encrypted vault
- \`sotto run -- <cmd>\` - inject environment secrets directly into a child process
- \`sotto push\` / \`sotto pull\` - sync encrypted snapshots across devices and teammates
- \`sotto share KEY\` - create a burn-after-reading one-time secret link
- \`sotto env ls|use|diff|copy\` - manage dev, staging, and prod environments
- \`sotto token create|ls|revoke\` - issue scoped, expiring CI/CD machine tokens (\`SOTTO_TOKEN\`)

## Guides

${guides}

## Documentation

- [README](https://github.com/getsotto/sotto/blob/main/README.md)
- [Threat model](https://github.com/getsotto/sotto/blob/main/THREAT-MODEL.md)
- [Security policy](https://github.com/getsotto/sotto/blob/main/SECURITY.md)
- [Self-hosting deployment runbook](https://github.com/getsotto/sotto/blob/main/deploy/README.md)
- [GitHub Action (getsotto/sotto-action)](https://github.com/getsotto/sotto-action)
- [Homebrew tap (getsotto/homebrew-tap)](https://github.com/getsotto/homebrew-tap)
`;
}
