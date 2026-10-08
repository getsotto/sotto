import "@testing-library/jest-dom/vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { AuthCallback } from "../AuthCallback";

describe("AuthCallback retry destination", () => {
  beforeEach(() => {
    sessionStorage.clear();
    window.history.replaceState(null, "", "/auth/callback?state=stale-state");
  });

  afterEach(() => {
    cleanup();
    vi.restoreAllMocks();
  });

  it.each([
    ["/cloud", "/cloud"],
    ["/app", "/app"],
    [null, "/app"],
    ["https://example.com", "/app"],
  ])("starts a fresh login for stored destination %s", (stored, expected) => {
    const freshState = "00000000-0000-4000-8000-000000000003";
    vi.spyOn(crypto, "randomUUID").mockReturnValue(freshState);
    const assign = vi.spyOn(window.location, "assign").mockImplementation(() => {});
    sessionStorage.setItem("sotto_oauth_state", "expected-state");
    if (stored !== null) sessionStorage.setItem("sotto_oauth_destination", stored);

    render(<AuthCallback />);
    expect(screen.getByRole("alert")).toHaveTextContent("state mismatch");
    fireEvent.click(screen.getByRole("button", { name: "Try again" }));

    expect(window.location.pathname + window.location.search).toBe("/auth/callback");
    expect(sessionStorage.getItem("sotto_oauth_state")).toBe(freshState);
    expect(sessionStorage.getItem("sotto_oauth_destination")).toBe(expected);
    expect(assign).toHaveBeenCalledTimes(1);
    const redirect = new URL(String(assign.mock.calls[0][0]), window.location.origin);
    expect(redirect.pathname).toBe("/auth/github/login");
    expect(redirect.searchParams.get("state")).toBe(freshState);
    expect(redirect.searchParams.get("redirect_uri")).toBe(window.location.origin + "/auth/callback");
  });
});
