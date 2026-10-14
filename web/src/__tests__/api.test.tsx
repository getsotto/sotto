import { afterEach, describe, expect, it, vi } from "vitest";

import { createShare, me, ServerUnreachableError } from "../api";

afterEach(() => vi.unstubAllGlobals());

describe("share creation responses", () => {
  it("preserves a known quota explanation without retrying", async () => {
    const fetch = vi.fn().mockResolvedValue(new Response("free accounts may have only 3 active share links", {
      status: 402,
      headers: { "x-sotto-error-code": "quota" },
    }));
    vi.stubGlobal("fetch", fetch);

    await expect(createShare(new Uint8Array([1]), 1)).rejects.toThrow(
      "Could not create the share link: free accounts may have only 3 active share links",
    );
    expect(fetch).toHaveBeenCalledOnce();
  });

  it.each(["", "   "])("retains the status fallback for an empty quota body %j", async (body) => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response(body, {
      status: 402,
      headers: { "x-sotto-error-code": "quota" },
    })));
    await expect(createShare(new Uint8Array([1]), 1)).rejects.toThrow("server error (402)");
  });

  it("retains the status fallback for an unrecognised response", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response("untrusted explanation", {
      status: 500,
      headers: { "x-sotto-error-code": "unknown" },
    })));
    await expect(createShare(new Uint8Array([1]), 1)).rejects.toThrow("server error (500)");
  });

  it.each(["network", "abort"] as const)("preserves a quota body-read %s failure", async (kind) => {
    const cause = kind === "abort"
      ? Object.assign(new Error("cancelled"), { name: "AbortError" })
      : new TypeError("connection closed");
    const response = new Response(null, { status: 402, headers: { "x-sotto-error-code": "quota" } });
    vi.spyOn(response, "text").mockRejectedValue(cause);
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(response));
    const error: unknown = await createShare(new Uint8Array([1]), 1).catch((caught: unknown) => caught);
    if (kind === "abort") {
      expect(error).toBe(cause);
    } else {
      expect(error).toBeInstanceOf(ServerUnreachableError);
      expect((error as ServerUnreachableError).cause).toBe(cause);
    }
  });

  it("returns a successful token", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(Response.json({ token: "synthetic-token" })));
    await expect(createShare(new Uint8Array([1]), 1)).resolves.toBe("synthetic-token");
  });
});

describe("me response errors", () => {
  it.each([
    ["fetch", (cause: TypeError) => Promise.reject(cause)],
    ["json", (cause: TypeError) => {
      const response = Response.json({ user_id: "alice" });
      vi.spyOn(response, "json").mockRejectedValue(cause);
      return Promise.resolve(response);
    }],
  ])("reports a failed %s as an unreachable server with the original cause", async (_stage, result) => {
    const cause = new TypeError("connection closed");
    vi.stubGlobal("fetch", vi.fn().mockImplementation(() => result(cause)));

    const error: unknown = await me().catch((caught: unknown) => caught);

    expect(error).toBeInstanceOf(ServerUnreachableError);
    expect((error as Error).message).toBe(
      "Could not reach the server. This is a connection problem and says nothing about your " +
        "data: Sotto encrypts secrets before they leave your device, so nothing readable is " +
        "stored anywhere else.",
    );
    expect((error as ServerUnreachableError).cause).toBe(cause);
  });

  it.each([
    ["fetch", (abort: Error) => Promise.reject(abort)],
    ["json", (abort: Error) => {
      const response = Response.json({ user_id: "alice" });
      vi.spyOn(response, "json").mockRejectedValue(abort);
      return Promise.resolve(response);
    }],
  ])("preserves an AbortError from %s", async (_stage, result) => {
    const abort = Object.assign(new Error("cancelled"), { name: "AbortError" });
    vi.stubGlobal("fetch", vi.fn().mockImplementation(() => result(abort)));

    await expect(me()).rejects.toBe(abort);
  });

  it("reports malformed JSON without calling the server unreachable", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response("invalid json")));

    const error: unknown = await me().catch((caught: unknown) => caught);

    expect(error).toBeInstanceOf(Error);
    expect(error).not.toBeInstanceOf(ServerUnreachableError);
    expect((error as Error).message).toBe("The server sent a response this app could not read.");
  });

  it("reports an HTTP 503 as a status error", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response(null, { status: 503 })));

    await expect(me()).rejects.toMatchObject({ name: "Error", message: "server error (503)" });
  });

  it("returns the user from a readable response", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(Response.json({ user_id: "alice" })));

    await expect(me()).resolves.toEqual({ userId: "alice" });
  });
});
