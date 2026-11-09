import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { PENDING_CLOUD_CONTACT_TOKEN_KEY, startLogin } from "../login";

describe("startLogin", () => {
  beforeEach(() => {
    sessionStorage.clear();
    window.history.replaceState(null, "", "/cloud");
    vi.spyOn(crypto, "randomUUID").mockReturnValue("00000000-0000-4000-8000-000000000000");
    vi.spyOn(window.location, "assign").mockImplementation(() => {});
  });

  afterEach(() => {
    vi.restoreAllMocks();
    sessionStorage.clear();
    window.history.replaceState(null, "", "/");
  });

  it("keeps a valid contact callback token for the Cloud OAuth return", () => {
    const token = "a".repeat(64);
    window.history.replaceState(null, "", `/cloud#contact_token=${token}`);

    startLogin("/cloud");

    expect(sessionStorage.getItem(PENDING_CLOUD_CONTACT_TOKEN_KEY)).toBe(token);
    expect(sessionStorage.getItem("sotto_oauth_destination")).toBe("/cloud");
    expect(window.location.hash).toBe("");
    expect(window.location.assign).toHaveBeenCalledOnce();
  });

  it("does not preserve malformed callback tokens", () => {
    window.history.replaceState(null, "", "/cloud#contact_token=not-a-token");

    startLogin("/cloud");

    expect(sessionStorage.getItem(PENDING_CLOUD_CONTACT_TOKEN_KEY)).toBeNull();
  });
});
