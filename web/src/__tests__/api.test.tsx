import { afterEach, describe, expect, it, vi } from "vitest";

import { me, ServerUnreachableError } from "../api";

afterEach(() => vi.unstubAllGlobals());

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
