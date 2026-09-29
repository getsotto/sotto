import { describe, expect, it } from "vitest";
import {
  bytesToStandardB64,
  bytesToUrlSafeB64,
  standardB64ToBytes,
  urlSafeB64ToBytes,
} from "../base64";

function fixture(length: number): Uint8Array {
  return Uint8Array.from({ length }, (_, index) => (index * 131 + 17) & 0xff);
}

describe("base64 helpers", () => {
  it.each([
    [new Uint8Array([]), ""],
    [new Uint8Array([0]), "AA=="],
    [new Uint8Array([0, 255]), "AP8="],
    [new Uint8Array([0, 1, 255]), "AAH/"],
  ])("matches standard Base64 known answers", (bytes, expected) => {
    expect(bytesToStandardB64(bytes)).toBe(expected);
    expect(standardB64ToBytes(expected)).toEqual(bytes);
  });

  it("uses URL-safe characters without padding", () => {
    const bytes = new Uint8Array([251, 255]);
    expect(bytesToStandardB64(bytes)).toBe("+/8=");
    expect(bytesToUrlSafeB64(bytes)).toBe("-_8");
    expect(bytesToUrlSafeB64(bytes)).not.toMatch(/[+/=]/);
    expect(urlSafeB64ToBytes("-_8")).toEqual(bytes);
  });

  it.each([32767, 32768, 32769, 1024 * 1024])(
    "matches an independent encoder and round-trips %i binary bytes",
    (length) => {
      const bytes = fixture(length);
      const expected = Buffer.from(bytes).toString("base64");
      expect(bytesToStandardB64(bytes)).toBe(expected);
      expect(standardB64ToBytes(expected)).toEqual(bytes);

      const urlSafe = expected.replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
      expect(bytesToUrlSafeB64(bytes)).toBe(urlSafe);
      expect(urlSafeB64ToBytes(urlSafe)).toEqual(bytes);
    },
    15_000,
  );
});
