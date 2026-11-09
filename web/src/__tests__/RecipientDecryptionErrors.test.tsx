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
vi.mock("../wasm", () => ({
  loadWasm: vi.fn(),
  share_open: vi.fn(),
  share_passphrase_key: vi.fn(),
}));

const originalUrl = window.location.href;
const encBlob = new Uint8Array([2]);
const salt = new Uint8Array([3]);

afterEach(() => {
  cleanup();
  window.history.replaceState(null, "", originalUrl);
});

beforeEach(() => {
  vi.resetAllMocks();
  window.location.hash = "#AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
  vi.mocked(wasm.loadWasm).mockResolvedValue(undefined);
  vi.mocked(api.fetchShare).mockResolvedValue({ encBlob, passphraseSalt: salt });
});

async function expectFailedReveal(message: string) {
  expect(await screen.findByRole("alert")).toHaveTextContent(message);
  expect(screen.queryByRole("textbox", { name: "Shared secret" })).not.toBeInTheDocument();
  expect(screen.queryByRole("button", { name: "Copy secret" })).not.toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Reveal secret" })).toBeEnabled();
  await act(async () => {});
  expect(api.fetchShare).toHaveBeenCalledExactlyOnceWith("synthetic-token");
}

describe("RecipientPage decryption errors", () => {
  it("reports a required passphrase without deriving a key or opening the ciphertext", async () => {
    render(<RecipientPage token="synthetic-token" />);
    fireEvent.click(screen.getByRole("button", { name: "Reveal secret" }));

    await expectFailedReveal("this link requires a passphrase");
    expect(wasm.share_passphrase_key).not.toHaveBeenCalled();
    expect(wasm.share_open).not.toHaveBeenCalled();
  });

  it("reports passphrase key derivation failure without opening the ciphertext", async () => {
    vi.mocked(wasm.share_passphrase_key).mockImplementation(() => {
      throw new Error("synthetic derivation failure");
    });
    render(<RecipientPage token="synthetic-token" />);
    fireEvent.change(screen.getByLabelText("Passphrase (only if the sender set one)"), {
      target: { value: "synthetic passphrase" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Reveal secret" }));

    await expectFailedReveal("Couldn't reveal the secret: synthetic derivation failure");
    expect(wasm.share_passphrase_key).toHaveBeenCalledExactlyOnceWith(
      new Uint8Array(32),
      new TextEncoder().encode("synthetic passphrase"),
      salt,
    );
    expect(wasm.share_open).not.toHaveBeenCalled();
  });

  it.each([false, true])("reports opening failure for a passphrase-protected share: %s", async (protectedShare) => {
    const derivedKey = new Uint8Array([4]);
    vi.mocked(api.fetchShare).mockResolvedValue({
      encBlob,
      passphraseSalt: protectedShare ? salt : null,
    });
    vi.mocked(wasm.share_passphrase_key).mockReturnValue(derivedKey);
    vi.mocked(wasm.share_open).mockImplementation(() => {
      throw new Error("synthetic authentication failure");
    });
    render(<RecipientPage token="synthetic-token" />);
    if (protectedShare) {
      fireEvent.change(screen.getByLabelText("Passphrase (only if the sender set one)"), {
        target: { value: "synthetic passphrase" },
      });
    }
    fireEvent.click(screen.getByRole("button", { name: "Reveal secret" }));

    await expectFailedReveal("Couldn't reveal the secret: synthetic authentication failure");
    expect(wasm.share_open).toHaveBeenCalledExactlyOnceWith(
      protectedShare ? derivedKey : new Uint8Array(32),
      encBlob,
    );
    expect(wasm.share_passphrase_key).toHaveBeenCalledTimes(protectedShare ? 1 : 0);
  });
});
