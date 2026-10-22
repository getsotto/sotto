import "@testing-library/jest-dom/vitest";
import { render, cleanup } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { Landing } from "../Landing";
import { seoSnapshot } from "../../vite.config";

vi.mock("../api", () => ({
  fetchCommunity: vi.fn().mockResolvedValue(null),
}));

afterEach(cleanup);

describe("Cloud pricing visibility", () => {
  it.each([true, false])(
    "keeps browser and crawler pricing aligned when visibility is %s",
    (visible) => {
      const browser = render(<Landing cloudPricingVisible={visible} />).container;
      const browserPricing = browser.querySelector("#pricing");
      const crawler = document.createElement("div");
      crawler.innerHTML = seoSnapshot(undefined, visible);
      const crawlerPricing = crawler.querySelector("#pricing");
      const normalisedText = (element: Element | null) =>
        element?.textContent?.replace(/\s+/g, " ").trim();

      expect(browserPricing).not.toBeNull();
      expect(crawlerPricing).not.toBeNull();
      expect(normalisedText(crawlerPricing)).toBe(normalisedText(browserPricing));
      if (visible) {
        expect(browserPricing).toHaveTextContent("Sotto Cloud");
        expect(browserPricing).toHaveTextContent("£2.99");
        expect(browserPricing).toHaveTextContent("£1.99");
        expect(browserPricing).not.toHaveTextContent("Team");
      } else {
        expect(browserPricing).toHaveTextContent("Local and self-hosted");
        expect(browserPricing).toHaveTextContent("current billing options are shown after sign-in");
        expect(browserPricing).not.toHaveTextContent("free forever");
        expect(browserPricing).not.toHaveTextContent("14-day Team trial");
        expect(browserPricing).not.toHaveTextContent("£15");
        expect(browserPricing).not.toHaveTextContent("Sotto Cloud");
      }
    },
  );
});
