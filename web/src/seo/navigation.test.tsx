import { renderToStaticMarkup } from "react-dom/server";
import { afterEach, describe, expect, it } from "vitest";
import type { IndexHtmlTransformContext, Plugin } from "vite";

import config from "../../vite.config";
import { Landing } from "../Landing";
import { guidePages } from "./pages";
import { snapshotFor } from "./snapshot";

const originalPages = [...guidePages];
const originalLinks = [
  ["/share-secrets-securely", "Share secrets"],
  ["/share-env-files", "Share .env files"],
  ["/one-time-secret-links", "One-time links"],
  ["/share-api-keys-securely", "Share API keys"],
  ["/send-password-securely", "Send passwords"],
  ["/self-hosted-secret-management", "Self-hosting"],
  ["/open-source-secret-manager", "Open-source secret manager"],
  ["/cli-secret-manager", "CLI secret manager"],
  ["/dotenv-alternative", "Dotenv alternative"],
  ["/ci-cd-secret-manager", "CI/CD secrets"],
];

afterEach(() => {
  guidePages.splice(0, guidePages.length, ...originalPages);
});

function links(html: string, name = "Guides") {
  const root = document.createElement("div");
  root.innerHTML = html;
  const nav = root.querySelector(`nav[aria-label="${name}"]`);
  expect(nav).not.toBeNull();
  return Array.from(nav!.querySelectorAll("a"), (link) => [
    link.getAttribute("href"), link.textContent,
  ]);
}

async function landingSnapshot() {
  const plugin = config.plugins?.find(
    (entry) => entry && typeof entry === "object" && "name" in entry
      && entry.name === "sotto-seo-prerender",
  ) as Plugin;
  const transform = plugin.transformIndexHtml;
  if (typeof transform !== "function") throw new Error("missing SEO HTML transform");
  const result = await transform('<div id="root"></div>', {} as IndexHtmlTransformContext);
  if (typeof result !== "string") throw new Error("expected prerendered HTML");
  return result;
}

async function expectNavigation(expected: string[][]) {
  expect(links(renderToStaticMarkup(<Landing />))).toEqual(expected);
  expect(links(await landingSnapshot())).toEqual(expected);
  for (const page of guidePages) {
    expect(links(snapshotFor(page))).toEqual([
      ...expected.filter(([href]) => href !== `/${page.slug}`),
      ["/#pricing", "Pricing"],
      ["https://github.com/getsotto/sotto", "GitHub"],
    ]);
  }
}

describe("central guide navigation", () => {
  it("preserves link order, short labels and each guide's self-exclusion", async () => {
    await expectNavigation(originalLinks);
    const footer = links(renderToStaticMarkup(<Landing />), "Footer");
    expect(links(await landingSnapshot(), "Footer")).toEqual(footer);
    expect(footer).toEqual([
      ["https://github.com/getsotto/sotto", "GitHub"],
      ["#open-source", "Contribute"],
      ["https://github.com/getsotto/sotto/releases", "Releases"],
      ["https://github.com/getsotto/sotto/blob/main/THREAT-MODEL.md", "Threat model"],
      ["https://github.com/getsotto/sotto/blob/main/SECURITY.md", "Security policy"],
      ["https://github.com/getsotto/sotto/blob/main/deploy/README.md", "Run your own"],
      ["/app", "Log in"],
    ]);
  });

  it("adds a guide at its central list position and escapes its label", async () => {
    const added = { ...originalPages[0], slug: "new-guide", navLabel: 'New <guide> & "tips"' };
    guidePages.splice(1, 0, added);
    await expectNavigation([
      originalLinks[0], ["/new-guide", added.navLabel], ...originalLinks.slice(1),
    ]);
  });

  it("removes links when a guide is removed", async () => {
    guidePages.splice(1, 1);
    await expectNavigation(originalLinks.filter((_, index) => index !== 1));
  });

  it("updates links and labels when a guide is renamed", async () => {
    const renamed = { ...originalPages[0], slug: "renamed-guide", navLabel: "Renamed guide" };
    guidePages[0] = renamed;
    await expectNavigation([["/renamed-guide", renamed.navLabel], ...originalLinks.slice(1)]);
  });
});
