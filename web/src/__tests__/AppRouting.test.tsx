import "@testing-library/jest-dom/vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { StrictMode } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { App } from "../App";

vi.mock("../Landing", () => ({ Landing: () => <div>landing</div> }));
vi.mock("../VaultApp", () => ({ VaultApp: () => <div>vault</div> }));
vi.mock("../AccountApp", () => ({ AccountApp: () => <div>account</div> }));
vi.mock("../AuthCallback", () => ({ AuthCallback: () => <div>callback</div> }));
vi.mock("../seo/SeoPage", () => ({ SeoPage: () => <div>guide</div> }));
vi.mock("../RecipientPage", () => ({
  RecipientPage: ({ token }: { token: string }) => <div>recipient:{token}</div>,
}));

// Vitest globals are off, so Testing Library's automatic cleanup never registers: without this the
// DOM of one test leaks into the next and role queries match more than one element.
const originalTitle = document.title;
afterEach(() => {
  cleanup();
  document.title = originalTitle;
  visit("/");
});

function visit(path: string) {
  window.history.replaceState(null, "", path);
}

describe("App document titles", () => {
  const landingTitle = "Sotto: end-to-end encrypted secret sync";
  beforeEach(() => {
    document.title = landingTitle;
  });

  it.each([
    ["/app", "Vault | Sotto"],
    ["/app/", "Vault | Sotto"],
    ["/cloud", "Cloud account | Sotto"],
    ["/cloud/", "Cloud account | Sotto"],
  ])("sets a static title for %s", (path, title) => {
    visit(path);
    const { unmount } = render(<StrictMode><App /></StrictMode>);

    expect(document.title).toBe(title);
    unmount();
    expect(document.title).toBe(landingTitle);
  });

  it("updates the title when switching application surfaces and restores the landing title", () => {
    visit("/cloud");
    const { rerender } = render(<App />);
    expect(document.title).toBe("Cloud account | Sotto");

    visit("/app");
    rerender(<App />);
    expect(document.title).toBe("Vault | Sotto");

    visit("/");
    rerender(<App />);
    expect(document.title).toBe(landingTitle);
  });

  it.each(["/", "/share-env-files"])("leaves the existing title alone on %s", (path) => {
    visit(path);
    const title = path === "/" ? landingTitle : "A prerendered guide | Sotto";
    document.title = title;
    render(<App />);
    expect(screen.getByText(path === "/" ? "landing" : "guide")).toBeInTheDocument();
    expect(document.title).toBe(title);
  });
});

describe("App share-link routing", () => {
  it("routes the account surface separately from the vault", () => {
    visit("/cloud");

    render(<App />);

    expect(screen.getByText("account")).toBeInTheDocument();
    expect(screen.queryByText("vault")).not.toBeInTheDocument();
  });

  it.each(["/s/%", "/s/%FF"])("renders an invalid link for the malformed share path %s", (path) => {
    visit(path);

    expect(() => render(<App />)).not.toThrow();

    expect(screen.getByRole("heading")).toHaveTextContent("This link is invalid");
    expect(screen.getByRole("alert")).toHaveTextContent("malformed");
    // A malformed token must not reach the recipient page (and its share fetch).
    expect(screen.queryByText(/^recipient:/)).not.toBeInTheDocument();
  });

  it("routes an ordinary token to the recipient page unchanged", () => {
    visit("/s/example-token");

    render(<App />);

    expect(screen.getByText("recipient:example-token")).toBeInTheDocument();
  });

  it("decodes a valid percent-encoded token before routing", () => {
    visit("/s/hello%20world");

    render(<App />);

    expect(screen.getByText("recipient:hello world")).toBeInTheDocument();
  });
});
