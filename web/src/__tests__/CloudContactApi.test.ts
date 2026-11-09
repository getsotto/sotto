import { afterEach, describe, expect, it, vi } from "vitest";

import {
  confirmCloudContactVerification,
  fetchCloudContactStatus,
  UnsupportedCloudContactError,
} from "../api";

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("cloud contact API compatibility", () => {
  it("classifies an older server's missing status route as unsupported", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response("not found", { status: 404 })));

    await expect(fetchCloudContactStatus()).rejects.toBeInstanceOf(UnsupportedCloudContactError);
  });

  it("classifies an older server's missing confirmation route without exposing raw status text", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response("not found", { status: 404 })));

    await expect(confirmCloudContactVerification("a".repeat(64))).rejects.toThrow(
      "Email contact settings require a newer Sotto server version.",
    );
  });
});
