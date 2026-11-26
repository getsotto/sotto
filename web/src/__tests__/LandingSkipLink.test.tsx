import "@testing-library/jest-dom/vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { Landing } from "../Landing";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

// Same order a Tab key walks the document: links and buttons that are in the tab sequence.
function tabOrder(container: HTMLElement): HTMLElement[] {
  return Array.from(
    container.querySelectorAll<HTMLElement>("a[href], button, input, [tabindex]"),
  ).filter((el) => el.tabIndex >= 0);
}

describe("Landing skip link", () => {
  function setup() {
    vi.stubGlobal("fetch", vi.fn().mockRejectedValue(new Error("offline")));
    return render(<Landing />);
  }

  it("is the first focusable element", () => {
    const { container } = setup();
    const skip = screen.getByRole("link", { name: "Skip to content" });
    expect(tabOrder(container)[0]).toBe(skip);
    expect(skip).toHaveAttribute("href", "#content");
  });

  it("moves focus to the hero when activated", () => {
    setup();
    const skip = screen.getByRole("link", { name: "Skip to content" });
    const target = document.getElementById("content");
    expect(target).not.toBeNull();
    expect(target).toHaveAttribute("tabindex", "-1");
    expect(target?.closest("header")).toBeNull();
    // Browsers focus the fragment target on activation; happy-dom does not, so mirror it here
    // and assert the target is focusable, which is what makes the real behaviour work.
    skip.focus();
    skip.click();
    (target as HTMLElement).focus();
    expect(target).toHaveFocus();
  });

  it("keeps the header links in their existing order", () => {
    setup();
    const nav = screen.getByRole("navigation", { name: "Site" });
    const names = Array.from(nav.querySelectorAll("a")).map((a) => a.textContent);
    expect(names).toEqual([
      "How it works",
      "Trust",
      "Pricing",
      "Contribute",
      "GitHub",
      "Log in",
    ]);
  });
});
