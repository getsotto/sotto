import "@testing-library/jest-dom/vitest";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import * as api from "../api";
import { RecipientPage } from "../RecipientPage";
import * as wasm from "../wasm";

vi.mock("../api", () => ({
  fetchShare: vi.fn(),
  ShareUnavailable: class ShareUnavailable extends Error {},
}));
vi.mock("../base64", () => ({
  urlSafeB64ToBytes: vi.fn(() => new Uint8Array([1])),
}));
vi.mock("../wasm", () => ({
  loadWasm: vi.fn(),
  share_open: vi.fn(),
  share_passphrase_key: vi.fn(),
}));

afterEach(cleanup);

const secret = "  synthetic first line\nsecond line  ";

function deferred() {
  let resolve!: () => void;
  const promise = new Promise<void>((res) => {
    resolve = res;
  });
  return { promise, resolve };
}

async function revealSecret() {
  render(<RecipientPage token="share-token" />);
  fireEvent.click(screen.getByRole("button", { name: "Reveal secret" }));
  return await screen.findByRole("textbox", { name: "Shared secret" });
}

describe("RecipientPage copy", () => {
  beforeEach(() => {
    vi.resetAllMocks();
    window.location.hash = "#synthetic-key";
    vi.mocked(api.fetchShare).mockResolvedValue({
      encBlob: new Uint8Array([2]),
      passphraseSalt: null,
    });
    vi.mocked(wasm.loadWasm).mockResolvedValue(undefined);
    vi.mocked(wasm.share_open).mockReturnValue(new TextEncoder().encode(secret));
  });

  it("copies the exact revealed value and announces success after the write completes", async () => {
    const write = deferred();
    const writeText = vi.fn(() => write.promise);
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: { writeText },
    });

    const textarea = await revealSecret();
    expect(textarea).toHaveValue(secret);
    expect(writeText).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole("button", { name: "Copy secret" }));
    expect(writeText).toHaveBeenCalledWith(secret);
    expect(screen.queryByRole("status")).not.toBeInTheDocument();

    await act(async () => write.resolve());
    expect(await screen.findByRole("status")).toHaveTextContent("Copied to clipboard.");
    expect(api.fetchShare).toHaveBeenCalledTimes(1);
  });

  it("keeps the secret available and gives manual-copy guidance when a write rejects", async () => {
    const writeText = vi.fn().mockRejectedValue(new Error("permission denied"));
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: { writeText },
    });

    const textarea = await revealSecret();
    fireEvent.click(screen.getByRole("button", { name: "Copy secret" }));

    expect(await screen.findByRole("alert")).toHaveTextContent("copy it manually");
    expect(textarea).toHaveValue(secret);
    expect(api.fetchShare).toHaveBeenCalledTimes(1);
  });

  it("falls back to manual copy when the clipboard API is unavailable", async () => {
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: undefined,
    });

    const textarea = await revealSecret();
    fireEvent.click(screen.getByRole("button", { name: "Copy secret" }));

    expect(await screen.findByRole("alert")).toHaveTextContent("copy it manually");
    expect(textarea).toHaveValue(secret);
    expect(api.fetchShare).toHaveBeenCalledTimes(1);
  });
});
