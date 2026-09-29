import { afterEach, describe, expect, it, vi } from "vitest";
import { startLogin } from "./login";

describe("startLogin", () => {
  afterEach(() => {
    sessionStorage.clear();
    vi.restoreAllMocks();
  });

  it("stores fresh state before navigating and builds the callback from the origin", () => {
    const states = ["00000000-0000-4000-8000-000000000001", "00000000-0000-4000-8000-000000000002"];
    vi.spyOn(crypto, "randomUUID").mockImplementation(() => states.shift() as ReturnType<typeof crypto.randomUUID>);
    const assigned: string[] = [];
    const assign = vi.spyOn(window.location, "assign").mockImplementation((url) => {
      const expected = assigned.length === 0 ? "00000000-0000-4000-8000-000000000001" : "00000000-0000-4000-8000-000000000002";
      expect(sessionStorage.getItem("sotto_oauth_state")).toBe(expected);
      assigned.push(String(url));
    });
    window.history.replaceState({}, "", "/vault?from=current#fragment");

    startLogin();
    let target = new URL(assigned[0], window.location.origin);
    expect(target.pathname).toBe("/auth/github/login");
    expect(target.searchParams.get("state")).toBe("00000000-0000-4000-8000-000000000001");
    expect(target.searchParams.get("redirect_uri")).toBe(window.location.origin + "/auth/callback");

    startLogin();
    target = new URL(assigned[1], window.location.origin);
    expect(assign).toHaveBeenCalledTimes(2);
    expect(target.searchParams.get("state")).toBe("00000000-0000-4000-8000-000000000002");
    expect(target.searchParams.get("redirect_uri")).toBe(window.location.origin + "/auth/callback");
    expect(sessionStorage.getItem("sotto_oauth_state")).toBe("00000000-0000-4000-8000-000000000002");
  });
});
