import "@testing-library/jest-dom/vitest";
import { act, cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { Landing } from "../Landing";

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
  expect(screen.getByRole("button", { name: "Copy" })).toBeInTheDocument();
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
