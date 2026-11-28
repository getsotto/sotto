import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import { Landing } from "../Landing";
import { seoSnapshot } from "../../vite.config";

function sectionTexts(root: Element): Array<[string, string]> {
  const main = root.querySelector("main.landing");
  if (!main) return [];
  return Array.from(main.children, (child) => [
    `${child.tagName.toLowerCase()}${child.id ? `#${child.id}` : ""}`,
    (child.textContent ?? "").replace(/\s+/g, " ").trim(),
  ]);
}

function extractLinks(element: Element | null): Array<[string | null, string]> {
  if (!element) return [];
  return Array.from(element.querySelectorAll("a"), (anchor) => [
    anchor.getAttribute("href"),
    anchor.textContent?.replace(/\s+/g, " ").trim() ?? "",
  ]);
}

describe("Landing and prerendered SEO snapshot parity", () => {
  it.each([false, true])(
    "matches every section's visible text and links when cloudPricingVisible is %s",
    (cloudPricingVisible) => {
      const browserRoot = document.createElement("div");
      browserRoot.innerHTML = renderToStaticMarkup(
        <Landing cloudPricingVisible={cloudPricingVisible} />,
      );

      const crawlerRoot = document.createElement("div");
      crawlerRoot.innerHTML = seoSnapshot(undefined, cloudPricingVisible);

      expect(sectionTexts(crawlerRoot)).toEqual(sectionTexts(browserRoot));
      expect(extractLinks(crawlerRoot)).toEqual(extractLinks(browserRoot));
    },
  );
});
