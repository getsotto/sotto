import { describe, expect, it } from "vitest";
import { guideBySlug, type SeoPageData } from "./pages";
import { faqJsonLd, snapshotFor } from "./snapshot";

const syntheticGuide: SeoPageData = {
  slug: "rendering-contracts",
  navLabel: "Rendering contracts",
  tabTitle: "Rendering contracts",
  description: "Synthetic rendering fixture",
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
      a: 'Yes, including a literal </script> sequence & `backticks`.',
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
        text: syntheticGuide.faqs[0].a.replace(/`/g, ""),
      },
    });
  });
});
