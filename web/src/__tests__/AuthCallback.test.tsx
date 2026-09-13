import "@testing-library/jest-dom/vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { AuthCallback } from "../AuthCallback";
import { startLogin } from "../login";

vi.mock("../login", () => ({ startLogin: vi.fn() }));

// Vitest globals are off, so Testing Library's automatic cleanup never registers: without this the
// DOM of one test leaks into the next and role queries match more than one element.
afterEach(cleanup);

function visit(path: string) {
  window.history.replaceState(null, "", path);
}

describe("AuthCallback", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    sessionStorage.clear();
    visit("/auth/callback");
  });

  it("offers a retry action when the callback carries no state", () => {
    render(<AuthCallback />);

    expect(screen.getByRole("alert")).toHaveTextContent("state mismatch");
    expect(screen.getByRole("button", { name: "Try again" })).toBeInTheDocument();
  });

  it("offers a retry action when the returned state does not match", () => {
    sessionStorage.setItem("sotto_oauth_state", "expected-state");
    visit("/auth/callback?state=other-state");

    render(<AuthCallback />);

    expect(screen.getByRole("alert")).toHaveTextContent("state mismatch");
    expect(screen.getByRole("button", { name: "Try again" })).toBeInTheDocument();
  });

  it("consumes the stored state, so a replay of the callback cannot reuse it", () => {
    sessionStorage.setItem("sotto_oauth_state", "expected-state");
    visit("/auth/callback?state=expected-state");

    render(<AuthCallback />);

    expect(sessionStorage.getItem("sotto_oauth_state")).toBeNull();
  });

  it("drops the stale query and starts a fresh flow when retry is clicked", () => {
    sessionStorage.setItem("sotto_oauth_state", "expected-state");
    visit("/auth/callback?state=stale-state");

    render(<AuthCallback />);
    fireEvent.click(screen.getByRole("button", { name: "Try again" }));

    expect(startLogin).toHaveBeenCalledTimes(1);
    expect(window.location.search).toBe("");
  });
});
