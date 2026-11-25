import "@testing-library/jest-dom/vitest";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { Landing } from "../Landing";

const INSTALL_CMD =
  "curl -fsSL https://raw.githubusercontent.com/getsotto/sotto/main/install.sh | sh";
const POWERSHELL_INSTALL_CMD =
  "irm https://raw.githubusercontent.com/getsotto/sotto/main/install.ps1 | iex";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

// Only the network boundary is stubbed: the real `Landing` renders and calls the real
// `fetchCommunity`, so these tests exercise the same code path a visitor hits.
async function renderLanding(fetchStub: ReturnType<typeof vi.fn>) {
  vi.stubGlobal("fetch", fetchStub);
  const view = render(<Landing />);
  // Let the rejected/resolved fetch and the `.then(setStats)` that follows it settle.
  await act(async () => {
    await new Promise((resolve) => setTimeout(resolve, 0));
  });
  return view;
}

function expectLandingStillUsable() {
  expect(screen.getByRole("link", { name: "Star on GitHub" })).toBeInTheDocument();
  expect(screen.getByRole("link", { name: "Good first issues" })).toBeInTheDocument();
  expect(screen.getByRole("link", { name: "Contributing" })).toBeInTheDocument();
  expect(
    screen.getByRole("button", { name: "Copy macOS/Linux install command" }),
  ).toBeInTheDocument();
  expect(
    screen.getByRole("button", { name: "Copy Windows PowerShell install command" }),
  ).toBeInTheDocument();
}

function expectNoStatsAndNoFallbackNoise(container: HTMLElement) {
  expect(container.querySelector(".community-stats")).toBeNull();
  expect(screen.queryByText(/\b0 (stars?|forks?|contributors?)\b/)).not.toBeInTheDocument();
  expect(screen.queryByRole("alert")).not.toBeInTheDocument();
}

describe("Landing community statistics", () => {
  it("shows the statistics for a valid snapshot", async () => {
    const fetchStub = vi.fn().mockResolvedValue(
      new Response(
        JSON.stringify({
          stars: 12,
          forks: 3,
          repo_url: "https://github.com/getsotto/sotto",
          contributor_count: 4,
          contributors: [
            { login: "alice", html_url: "https://github.com/alice", contributions: 5 },
            { login: "bob", html_url: "https://github.com/bob", contributions: 2 },
          ],
        }),
        { status: 200, headers: { "Content-Type": "application/json" } },
      ),
    );

    const { container } = await renderLanding(fetchStub);

    expect(fetchStub).toHaveBeenCalledTimes(1);
    expect(container.querySelector(".community-stats")).toHaveTextContent(
      "12 stars · 3 forks · 4 contributors",
    );
    expectLandingStillUsable();
  });

  it("omits the statistics when the request is rejected", async () => {
    const fetchStub = vi.fn().mockRejectedValue(new TypeError("Failed to fetch"));

    const { container } = await renderLanding(fetchStub);

    expect(fetchStub).toHaveBeenCalledTimes(1);
    expectNoStatsAndNoFallbackNoise(container);
    expectLandingStillUsable();
  });

  it("omits the statistics on an HTTP 503", async () => {
    const fetchStub = vi.fn().mockResolvedValue(new Response(null, { status: 503 }));

    const { container } = await renderLanding(fetchStub);

    expect(fetchStub).toHaveBeenCalledTimes(1);
    expectNoStatsAndNoFallbackNoise(container);
    expectLandingStillUsable();
  });

  it("omits the statistics when the response body is not valid JSON", async () => {
    const fetchStub = vi.fn().mockResolvedValue(
      new Response("not JSON", { status: 200, headers: { "Content-Type": "application/json" } }),
    );

    const { container } = await renderLanding(fetchStub);

    expect(fetchStub).toHaveBeenCalledTimes(1);
    expectNoStatsAndNoFallbackNoise(container);
    expectLandingStillUsable();
  });
});

describe("Landing install copy buttons", () => {
  it("copies each install command from a distinctly named button", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: { writeText },
    });
    const fetchStub = vi.fn().mockRejectedValue(new TypeError("Failed to fetch"));

    await renderLanding(fetchStub);

    const unix = screen.getByRole("button", { name: "Copy macOS/Linux install command" });
    const windows = screen.getByRole("button", {
      name: "Copy Windows PowerShell install command",
    });

    fireEvent.click(unix);
    expect(await screen.findByRole("button", { name: "Copied macOS/Linux install command" })).toBeInTheDocument();
    expect(writeText).toHaveBeenCalledExactlyOnceWith(INSTALL_CMD);

    fireEvent.click(windows);
    expect(
      await screen.findByRole("button", { name: "Copied Windows PowerShell install command" }),
    ).toBeInTheDocument();
    expect(writeText).toHaveBeenNthCalledWith(2, POWERSHELL_INSTALL_CMD);
    expect(writeText).toHaveBeenCalledTimes(2);
  });
});
