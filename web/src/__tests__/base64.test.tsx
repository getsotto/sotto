import { describe, expect, it } from "vitest";

import {
  bytesToStandardB64,
  bytesToUrlSafeB64,
  standardB64ToBytes,
  urlSafeB64ToBytes,
} from "../base64";

// The chunk size in `bytesToStandardB64`: lengths around it guard against a refactor back to a
// single `String.fromCharCode(...bytes)` spread, which overflows the call-argument limit here.
const CHUNK = 0x8000;

// Node's Buffer implements RFC 4648 independently of the browser helpers, so comparing against it
// catches an encoder and decoder that agree with each other but not with the standard.
function expectedStandardB64(bytes: Uint8Array): string {
  return Buffer.from(bytes).toString("base64");
}

// RFC 4648 section 5: swap `+` and `/` for `-` and `_`, then drop the padding. Deriving this from
// the independent standard encoding keeps the alphabet mapping itself part of the assertion.
function expectedUrlSafeB64(bytes: Uint8Array): string {
  return expectedStandardB64(bytes)
    .replace(/\+/g, "-")
    .replace(/\//g, "_")
    .replace(/=+$/, "");
}

// Deterministic bytes that include the extremes 0x00 and 0xff plus a spread of other values, so a
// sign- or char-code-based mistake cannot hide behind ASCII-only input.
function patternBytes(length: number): Uint8Array {
  const bytes = new Uint8Array(length);
  for (let i = 0; i < length; i++) {
    if (i % 3 === 0) {
      bytes[i] = 0x00;
    } else if (i % 3 === 1) {
      bytes[i] = 0xff;
    } else {
      bytes[i] = (i * 251 + 17) % 256;
    }
  }
  return bytes;
}

describe("Base64 helpers", () => {
  it("encodes empty and short inputs with zero, one and two padding characters", () => {
    expect(bytesToStandardB64(new Uint8Array(0))).toBe("");
    // One byte needs two padding characters.
    expect(bytesToStandardB64(Uint8Array.of(0xfb))).toBe("+w==");
    // Two bytes need one padding character.
    expect(bytesToStandardB64(Uint8Array.of(0xfb, 0xff))).toBe("+/8=");
    // Three bytes need no padding.
    expect(bytesToStandardB64(Uint8Array.of(0xfb, 0xff, 0x00))).toBe("+/8A");
  });

  it("decodes known answers that exercise +, /, - and _", () => {
    // [251, 255] is the shortest byte pair whose encodings contain all four special characters.
    expect(Array.from(standardB64ToBytes("+/8="))).toEqual([251, 255]);
    expect(Array.from(urlSafeB64ToBytes("-_8"))).toEqual([251, 255]);
  });

  it("matches the independent Buffer oracle across padding, boundary and large lengths", () => {
    const lengths = [0, 1, 2, 3, 4, 256, CHUNK - 1, CHUNK, CHUNK + 1, 0x100000];
    for (const length of lengths) {
      const bytes = patternBytes(length);
      expect(bytesToStandardB64(bytes), `standard, length ${length}`).toBe(
        expectedStandardB64(bytes),
      );
      expect(bytesToUrlSafeB64(bytes), `url-safe, length ${length}`).toBe(
        expectedUrlSafeB64(bytes),
      );
    }
    // The megabyte-scale lengths above approach vitest's five-second default on the slow
    // pure-JS atob/btoa implementations of the happy-dom environment.
  }, 60_000);

  it("keeps URL-safe output free of standard-only characters and padding", () => {
    const lengths = [1, 2, 3, 255, CHUNK - 1, CHUNK, CHUNK + 1, 0x100000];
    for (const length of lengths) {
      const encoded = bytesToUrlSafeB64(patternBytes(length));
      expect(encoded, `length ${length}`).not.toMatch(/[+/=]/);
      expect(encoded, `length ${length}`).toMatch(/^[-_A-Za-z0-9]*$/);
    }
  }, 60_000);

  // Round-tripping the megabyte payload through the slow pure-JS atob of the happy-dom
  // environment needs more than vitest's five-second default, hence the explicit timeout.
  it("round-trips arbitrary binary bytes including 0x00 and 0xff", () => {
    const lengths = [0, 1, 2, 3, 4, 5, 63, 64, 65, CHUNK - 1, CHUNK, CHUNK + 1, 0x100000];
    for (const length of lengths) {
      const bytes = patternBytes(length);
      expect(standardB64ToBytes(bytesToStandardB64(bytes)), `standard, length ${length}`).toEqual(
        bytes,
      );
      expect(
        urlSafeB64ToBytes(bytesToUrlSafeB64(bytes)),
        `url-safe, length ${length}`,
      ).toEqual(bytes);
    }
  }, 60_000);
});
