import { describe, expect, it } from "vitest";
import { guideBySlug, guidePages, landingFaqs, type SeoPageData } from "./pages";
import {
  faqJsonLd,
  guideJsonLd,
  inlineHtml,
  landingJsonLd,
  llmsTxt,
  SITE_DESCRIPTION,
  SITE_TITLE,
  snapshotFor,
} from "./snapshot";

const syntheticGuide: SeoPageData = {
  slug: "rendering-contracts",
  navLabel: "Rendering contracts",
  tabTitle: "Rendering contracts",
  description: "Synthetic rendering fixture",
  keywords: ["synthetic", "rendering"],
  h1: 'Guide & <not-a-heading> > "quoted"',
  lead: 'Use `sotto show` when A & B < C > D and keep "quotes".',
  ctaSecondary: {
    label: 'Read "A&B" <now>',
    href: '/guides?mode="safe"&next=<done>',
  },
  stepsTitle: 'Steps & <checks> > "expectations"',
  steps: [
    { head: 'Keep & <head> > "text".', body: 'Run `sotto inspect` & verify <output>.' },
    { head: "Keep attributes intact.", body: "Query strings remain data." },
    { head: "Keep terminal text literal.", body: "Backticks there are not markup." },
  ],
  terminal: [
    { text: 'printf "<terminal-node>&value" `literal` > output', kind: "cmd" },
    { text: 'result & <literal> > "quoted" `ticks`', kind: "value" },
  ],
  faqs: [
    {
      q: 'Can JSON-LD keep "quotes" & `code`?',
      a: 'Yes, including a literal </script> sequence & `backticks` and [a link](/share-env-files).',
    },
    { q: "Is the script still singular?", a: "Yes." },
    { q: "Does it parse?", a: "Yes." },
  ],
  closingTitle: "Rendering stays predictable",
  closingBody: "Authored content remains text.",
};

function parsedSnapshot(page: SeoPageData): HTMLDivElement {
  const root = document.createElement("div");
  root.innerHTML = snapshotFor(page);
  return root;
}

function guide(slug: string) {
  const page = guideBySlug(slug);
  if (!page) throw new Error(`unknown guide: ${slug}`);
  return page;
}

describe("share guide security copy", () => {
  it("describes bearer access and retained ciphertext", () => {
    const overviewPage = guide("share-secrets-securely");
    const overview = snapshotFor(overviewPage);
    expect(overview).toContain("Anyone with the complete link can use it while it is active.");
    expect(overview).not.toContain("only the person you send it to");
    expect(overviewPage.description).not.toContain("readable only by the recipient");

    const oneTime = snapshotFor(guide("one-time-secret-links"));
    expect(oneTime).toContain("The complete link is a bearer credential");
    expect(oneTime).toContain("The server retains the encrypted blob after the link is used");
    expect(oneTime).not.toContain("useless to anyone but its first reader");

    const passwordPage = guide("send-password-securely");
    const password = snapshotFor(passwordPage);
    expect(password).toContain("without putting the password in chat or email");
    expect(password).not.toContain("no copy left in chat or email");
    expect(passwordPage.description).toContain("without putting the password in chat or email");
  });
});

describe("guide rendering contracts", () => {
  it("preserves special characters and applies backtick markup only to prose", () => {
    const root = parsedSnapshot(syntheticGuide);

    const heading = root.querySelector("h1");
    expect(heading?.textContent).toBe(syntheticGuide.h1);
    expect(root.querySelector("not-a-heading")).toBeNull();

    const lead = root.querySelector("p.lead");
    expect(lead?.textContent).toBe(syntheticGuide.lead.replace(/`/g, ""));
    expect(lead?.querySelector("code")?.textContent).toBe("sotto show");

    const terminal = root.querySelector("pre.term");
    expect(terminal?.textContent).toContain(syntheticGuide.terminal[0].text);
    expect(terminal?.textContent).toContain(syntheticGuide.terminal[1].text);
    expect(terminal?.querySelector("terminal-node")).toBeNull();
    expect(terminal?.querySelector("code code")).toBeNull();
  });

  it("renders safe markdown links and keeps unsafe schemes as plain text", () => {
    const root = document.createElement("div");
    root.innerHTML = inlineHtml(
      "Read [the `.env` guide](/share-env-files), [docs](https://getsotto.co.uk/docs), and [bad](javascript:alert(1)).",
    );
    const anchors = Array.from(root.querySelectorAll("a"));
    expect(anchors).toHaveLength(2);
    expect(anchors[0].getAttribute("href")).toBe("/share-env-files");
    expect(anchors[0].querySelector("code")?.textContent).toBe(".env");
    expect(anchors[1].getAttribute("href")).toBe("https://getsotto.co.uk/docs");
    expect(root.textContent).toContain("[bad](javascript:alert(1))");
  });

  it("includes semantic breadcrumb navigation on guide pages", () => {
    const root = parsedSnapshot(guide("open-source-secret-manager"));
    const breadcrumb = root.querySelector('nav[aria-label="Breadcrumb"]');
    expect(breadcrumb).not.toBeNull();
    expect(breadcrumb?.querySelector("a")?.getAttribute("href")).toBe("/");
    expect(breadcrumb?.querySelector('[aria-current="page"]')?.textContent).toBe(
      "Open-source secret manager",
    );
  });

  it("preserves quoted secondary-link text and its query-string href", () => {
    const root = parsedSnapshot(syntheticGuide);
    const link = root.querySelector<HTMLAnchorElement>("a.btn.ghost");

    expect(link?.textContent).toBe(syntheticGuide.ctaSecondary.label);
    expect(link?.getAttribute("href")).toBe(syntheticGuide.ctaSecondary.href);
    expect(Array.from(link?.attributes ?? [], (attribute) => attribute.name).sort()).toEqual([
      "class",
      "href",
    ]);
    expect(link?.querySelector("now")).toBeNull();
  });

  it("keeps FAQ JSON-LD as one parseable script when an answer contains </script>", () => {
    const root = document.createElement("div");
    root.innerHTML = faqJsonLd(syntheticGuide);
    const scripts = root.querySelectorAll('script[type="application/ld+json"]');

    expect(scripts).toHaveLength(1);
    const data = JSON.parse(scripts[0].textContent ?? "") as {
      mainEntity: Array<{ name: string; acceptedAnswer: { text: string } }>;
    };
    expect(data.mainEntity[0]).toEqual({
      "@type": "Question",
      name: syntheticGuide.faqs[0].q.replace(/`/g, ""),
      acceptedAnswer: {
        "@type": "Answer",
        text: "Yes, including a literal </script> sequence & backticks and a link.",
      },
    });
  });

  it("emits TechArticle, BreadcrumbList, HowTo, and FAQPage in guide JSON-LD", () => {
    const root = document.createElement("div");
    root.innerHTML = guideJsonLd(syntheticGuide, "https://getsotto.co.uk");
    const scripts = root.querySelectorAll('script[type="application/ld+json"]');
    expect(scripts).toHaveLength(1);

    const parsed = JSON.parse(scripts[0].textContent ?? "") as {
      "@graph": Array<Record<string, unknown>>;
    };
    const types = parsed["@graph"].map((node) => node["@type"]);
    expect(types).toEqual([
      "WebSite",
      "Organization",
      "TechArticle",
      "BreadcrumbList",
      "HowTo",
      "FAQPage",
    ]);
  });
});

describe("landing structured data and llms.txt", () => {
  it("exposes brand alternate names, Secret Manager category, and pricing parity", () => {
    const hiddenRoot = document.createElement("div");
    hiddenRoot.innerHTML = landingJsonLd("https://getsotto.co.uk", "0.11.0", false);
    const hiddenData = JSON.parse(
      hiddenRoot.querySelector('script[type="application/ld+json"]')?.textContent ?? "",
    ) as { "@graph": Array<Record<string, unknown>> };

    const website = hiddenData["@graph"].find((n) => n["@type"] === "WebSite");
    expect(website?.name).toBe("Sotto");
    expect(website?.alternateName).toContain("getsotto");

    const software = hiddenData["@graph"].find((n) => n["@type"] === "SoftwareApplication");
    expect(software?.applicationSubCategory).toBe("Secret Manager");
    expect(software?.operatingSystem).toEqual(["macOS", "Linux", "Windows", "Web"]);
    expect(software?.softwareVersion).toBe("0.11.0");
    expect(software?.offers).toMatchObject({ "@type": "Offer", price: "0", priceCurrency: "GBP" });

    const visibleRoot = document.createElement("div");
    visibleRoot.innerHTML = landingJsonLd("https://getsotto.co.uk", "0.11.0", true);
    const visibleData = JSON.parse(
      visibleRoot.querySelector('script[type="application/ld+json"]')?.textContent ?? "",
    ) as { "@graph": Array<Record<string, unknown>> };
    const visibleSoftware = visibleData["@graph"].find((n) => n["@type"] === "SoftwareApplication");
    const offers = visibleSoftware?.offers as Array<{ price: string }>;
    expect(offers.map((o) => o.price)).toEqual(["0", "2.99", "29.99"]);
  });

  it("lists every guide and honest security note in llms.txt", () => {
    const output = llmsTxt("https://getsotto.co.uk");
    expect(output).toContain("# Sotto (getsotto)");
    expect(output).toContain("zero-knowledge secret manager");
    expect(output).toContain("has not yet undergone a third-party cryptographic audit");
    for (const page of guidePages) {
      expect(output).toContain(`https://getsotto.co.uk/${page.slug}`);
    }
  });
});

describe("SEO content hygiene across all routes", () => {
  it("enforces title length, description length, uniqueness, and house punctuation rules", () => {
    expect(SITE_TITLE.length).toBeLessThanOrEqual(65);
    expect(SITE_DESCRIPTION.length).toBeGreaterThanOrEqual(110);
    expect(SITE_DESCRIPTION.length).toBeLessThanOrEqual(165);
    expect(SITE_TITLE).toContain("Sotto");
    expect(SITE_TITLE.toLowerCase()).toContain("secret manager");
    expect(SITE_DESCRIPTION.toLowerCase()).toContain("getsotto");

    const slugs = new Set<string>();
    const titles = new Set<string>();
    const descriptions = new Set<string>();

    for (const page of guidePages) {
      expect(slugs.has(page.slug)).toBe(false);
      slugs.add(page.slug);

      expect(titles.has(page.tabTitle)).toBe(false);
      titles.add(page.tabTitle);
      expect(page.tabTitle.length).toBeLessThanOrEqual(65);
      expect(page.tabTitle).toContain("Sotto");

      expect(descriptions.has(page.description)).toBe(false);
      descriptions.add(page.description);
      expect(page.description.length).toBeGreaterThanOrEqual(110);
      expect(page.description.length).toBeLessThanOrEqual(165);

      const rendered = snapshotFor(page);
      expect(rendered).not.toMatch(/\[[^\]]+\]\(https?:\/\//);
      expect(rendered).not.toMatch(/\[[^\]]+\]\(\//);
      expect(rendered).not.toContain("—");
      expect(rendered).not.toContain("–");
    }

    for (const faq of landingFaqs) {
      expect(faq.q).not.toContain("—");
      expect(faq.a).not.toContain("—");
      expect(faq.q).not.toContain("–");
      expect(faq.a).not.toContain("–");
    }
  });
});
