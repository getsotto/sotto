import "@testing-library/jest-dom/vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { AuthCallback } from "../AuthCallback";
import { PENDING_CLOUD_CONTACT_TOKEN_KEY, startLogin } from "../login";

vi.mock("../login", () => ({
  PENDING_CLOUD_CONTACT_TOKEN_KEY: "sotto_pending_cloud_contact_token",
  startLogin: vi.fn(),
}));

// Vitest globals are off, so Testing Library's automatic cleanup never registers: without this the
// DOM of one test leaks into the next and role queries match more than one element.
afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  sessionStorage.clear();
  visit("/");
});

function visit(path: string) {
  window.history.replaceState(null, "", path);
}

describe("AuthCallback", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    sessionStorage.clear();
    visit("/auth/callback");
    vi.spyOn(window.location, "replace").mockImplementation(() => {});
  });

  it.each([
    ["/cloud", "/cloud"],
    ["/app", "/app"],
    [null, "/app"],
    ["https://example.com", "/app"],
  ])("finishes login for stored destination %s", (stored, expected) => {
    sessionStorage.setItem("sotto_oauth_state", "expected-state");
    if (stored !== null) sessionStorage.setItem("sotto_oauth_destination", stored);
    visit("/auth/callback?state=expected-state");

    render(<AuthCallback />);

    expect(window.location.replace).toHaveBeenCalledExactlyOnceWith(expected);
    expect(sessionStorage.getItem("sotto_oauth_state")).toBeNull();
    expect(sessionStorage.getItem("sotto_oauth_destination")).toBeNull();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Try again" })).not.toBeInTheDocument();
    expect(startLogin).not.toHaveBeenCalled();
  });

  it("restores a contact verification token after Cloud login", () => {
    const token = "a".repeat(64);
    sessionStorage.setItem("sotto_oauth_state", "expected-state");
    sessionStorage.setItem("sotto_oauth_destination", "/cloud");
    sessionStorage.setItem(PENDING_CLOUD_CONTACT_TOKEN_KEY, token);
    visit("/auth/callback?state=expected-state");

    render(<AuthCallback />);

    expect(window.location.replace).toHaveBeenCalledExactlyOnceWith(`/cloud#contact_token=${token}`);
    expect(sessionStorage.getItem(PENDING_CLOUD_CONTACT_TOKEN_KEY)).toBeNull();
  });

  it("keeps the Cloud destination for retry when state does not match", () => {
    sessionStorage.setItem("sotto_oauth_state", "expected-state");
    sessionStorage.setItem("sotto_oauth_destination", "/cloud");
    visit("/auth/callback?state=other-state");

    render(<AuthCallback />);

    expect(window.location.replace).not.toHaveBeenCalled();
    expect(sessionStorage.getItem("sotto_oauth_state")).toBeNull();
    expect(sessionStorage.getItem("sotto_oauth_destination")).toBe("/cloud");
    expect(screen.getByRole("alert")).toHaveTextContent("state mismatch");
    expect(startLogin).not.toHaveBeenCalled();
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
