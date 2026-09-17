import "@testing-library/jest-dom/vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { App } from "../App";

vi.mock("../Landing", () => ({ Landing: () => <div>landing</div> }));
vi.mock("../VaultApp", () => ({ VaultApp: () => <div>vault</div> }));
vi.mock("../AuthCallback", () => ({ AuthCallback: () => <div>callback</div> }));
vi.mock("../seo/SeoPage", () => ({ SeoPage: () => <div>guide</div> }));
vi.mock("../RecipientPage", () => ({
  RecipientPage: ({ token }: { token: string }) => <div>recipient:{token}</div>,
}));

// Vitest globals are off, so Testing Library's automatic cleanup never registers: without this the
// DOM of one test leaks into the next and role queries match more than one element.
afterEach(cleanup);

function visit(path: string) {
  window.history.replaceState(null, "", path);
}

describe("App share-link routing", () => {
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
