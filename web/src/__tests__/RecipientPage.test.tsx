import "@testing-library/jest-dom/vitest";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { StrictMode } from "react";
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

afterEach(() => {
  cleanup();
  window.history.replaceState(null, "", originalUrl);
});

const secret = "  synthetic first line\nsecond line  ";

function deferred() {
  let resolve!: () => void;
  let reject!: (err?: unknown) => void;
  const promise = new Promise<void>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

async function revealSecret() {
  render(<RecipientPage token="share-token" />);
  fireEvent.click(screen.getByRole("button", { name: "Reveal secret" }));
  return await screen.findByRole("textbox", { name: "Shared secret" });
}

describe("RecipientPage", () => {
  beforeEach(() => {
    vi.resetAllMocks();
    window.location.hash = "#AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    vi.mocked(api.fetchShare).mockResolvedValue({
      encBlob: new Uint8Array([2]),
      passphraseSalt: null,
    });
    vi.mocked(wasm.loadWasm).mockResolvedValue(undefined);
    vi.mocked(wasm.share_open).mockReturnValue(new TextEncoder().encode(secret));
  });

  it("waits for an explicit reveal before fetching the share", async () => {
    render(
      <StrictMode>
        <RecipientPage token="share-token" />
      </StrictMode>,
    );
    await act(async () => {});

    expect(screen.getByRole("heading", { name: "You’ve received a secret" })).toBeVisible();
    expect(screen.getByRole("button", { name: "Reveal secret" })).toBeEnabled();
    expect(wasm.loadWasm).not.toHaveBeenCalled();
    expect(api.fetchShare).not.toHaveBeenCalled();
  });

  it("reports a missing fragment without fetching the share", async () => {
    window.location.hash = "";
    render(<RecipientPage token="share-token" />);

    fireEvent.click(screen.getByRole("button", { name: "Reveal secret" }));

    expect(await screen.findByRole("alert")).toHaveTextContent("this link is missing its decryption key");
    expect(screen.getByRole("button", { name: "Reveal secret" })).toBeEnabled();
    expect(api.fetchShare).not.toHaveBeenCalled();
  });

  it("reports an invalid base64 fragment without fetching the share", async () => {
    window.location.hash = "#%%%";
    render(<RecipientPage token="share-token" />);

    fireEvent.click(screen.getByRole("button", { name: "Reveal secret" }));

    expect(await screen.findByRole("alert")).toHaveTextContent("Couldn't reveal the secret:");
    expect(screen.getByRole("button", { name: "Reveal secret" })).toBeEnabled();
    expect(screen.queryByRole("textbox", { name: "Shared secret" })).not.toBeInTheDocument();
    expect(wasm.loadWasm).not.toHaveBeenCalled();
    expect(api.fetchShare).not.toHaveBeenCalled();
  });

  it("allows retry after wasm initialization fails without consuming the share", async () => {
    const initialization = deferred();
    vi.mocked(wasm.loadWasm)
      .mockImplementationOnce(() => initialization.promise)
      .mockResolvedValueOnce(undefined);
    render(<RecipientPage token="share-token" />);

    expect(api.fetchShare).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Reveal secret" }));

    expect(screen.getByRole("button", { name: "Revealing…" })).toBeDisabled();
    expect(api.fetchShare).not.toHaveBeenCalled();
    await act(async () => initialization.reject(new Error("wasm unavailable")));

    expect(await screen.findByRole("alert")).toHaveTextContent("wasm unavailable");
    expect(screen.getByRole("button", { name: "Reveal secret" })).toBeEnabled();
    expect(api.fetchShare).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole("button", { name: "Reveal secret" }));

    expect(await screen.findByRole("textbox", { name: "Shared secret" })).toHaveValue(secret);
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(api.fetchShare).toHaveBeenCalledExactlyOnceWith("share-token");
    expect(wasm.share_open).toHaveBeenCalledWith(new Uint8Array(32), new Uint8Array([2]));
  });

  it("reports an unavailable share without attempting decryption", async () => {
    const unavailable = "This link is invalid, expired, revoked, or has already been viewed.";
    vi.mocked(api.fetchShare).mockRejectedValue(new api.ShareUnavailable(unavailable));
    render(<RecipientPage token="share-token" />);

    fireEvent.click(screen.getByRole("button", { name: "Reveal secret" }));

    expect((await screen.findByRole("alert")).textContent).toBe(unavailable);
    expect(screen.getByRole("button", { name: "Reveal secret" })).toBeEnabled();
    expect(screen.queryByRole("textbox", { name: "Shared secret" })).not.toBeInTheDocument();
    expect(api.fetchShare).toHaveBeenCalledExactlyOnceWith("share-token");
    expect(wasm.share_open).not.toHaveBeenCalled();
    expect(wasm.share_passphrase_key).not.toHaveBeenCalled();
  });

  it("retries a failed share fetch only after another explicit reveal", async () => {
    vi.mocked(api.fetchShare).mockRejectedValueOnce(new Error("temporary fetch failure"));
    render(<RecipientPage token="share-token" />);
    fireEvent.click(screen.getByRole("button", { name: "Reveal secret" }));

    expect((await screen.findByRole("alert")).textContent).toBe(
      "Couldn't reveal the secret: temporary fetch failure",
    );
    expect(screen.getByRole("button", { name: "Reveal secret" })).toBeEnabled();
    expect(screen.queryByRole("textbox", { name: "Shared secret" })).not.toBeInTheDocument();
    expect(wasm.share_open).not.toHaveBeenCalled();
    expect(wasm.share_passphrase_key).not.toHaveBeenCalled();
    await act(async () => {});
    expect(api.fetchShare).toHaveBeenCalledTimes(1);

    fireEvent.click(screen.getByRole("button", { name: "Reveal secret" }));

    expect(await screen.findByRole("textbox", { name: "Shared secret" })).toHaveValue(secret);
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(api.fetchShare).toHaveBeenCalledTimes(2);
    expect(api.fetchShare).toHaveBeenNthCalledWith(2, "share-token");
    expect(wasm.share_open).toHaveBeenCalledExactlyOnceWith(new Uint8Array(32), new Uint8Array([2]));
    expect(wasm.share_passphrase_key).not.toHaveBeenCalled();
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

  it("prevents overlapping clipboard writes during repeated activation and indicates copying state", async () => {
    const write = deferred();
    const writeText = vi.fn(() => write.promise);
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: { writeText },
    });

    await revealSecret();

    const copyBtn = screen.getByRole("button", { name: "Copy secret" });
    fireEvent.click(copyBtn);
    expect(writeText).toHaveBeenCalledTimes(1);
    expect(writeText).toHaveBeenCalledWith(secret);

    // Repeated clicks while in flight do not trigger additional clipboard writes
    const copyingBtn = screen.getByRole("button", { name: "Copying…" });
    expect(copyingBtn).toBeDisabled();
    fireEvent.click(copyingBtn);
    expect(writeText).toHaveBeenCalledTimes(1);

    await act(async () => write.resolve());

    expect(await screen.findByRole("status")).toHaveTextContent("Copied to clipboard.");
    expect(screen.getByRole("button", { name: "Copy secret" })).toBeEnabled();
    expect(api.fetchShare).toHaveBeenCalledTimes(1);
  });

  it("clears previous failure feedback on retry and announces the successful result", async () => {
    const firstWrite = deferred();
    const secondWrite = deferred();
    const writeText = vi
      .fn()
      .mockImplementationOnce(() => firstWrite.promise)
      .mockImplementationOnce(() => secondWrite.promise);
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: { writeText },
    });

    await revealSecret();

    // First attempt fails
    fireEvent.click(screen.getByRole("button", { name: "Copy secret" }));
    expect(writeText).toHaveBeenCalledTimes(1);
    await act(async () => firstWrite.reject(new Error("write failed")));

    expect(await screen.findByRole("alert")).toHaveTextContent("copy it manually");
    const retryBtn = screen.getByRole("button", { name: "Copy secret" });
    expect(retryBtn).toBeEnabled();

    // Retry attempt clears failure alert and shows copying state
    fireEvent.click(retryBtn);
    expect(writeText).toHaveBeenCalledTimes(2);
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(screen.queryByRole("status")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Copying…" })).toBeDisabled();

    // Successful completion announces success and re-enables control
    await act(async () => secondWrite.resolve());
    expect(await screen.findByRole("status")).toHaveTextContent("Copied to clipboard.");
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Copy secret" })).toBeEnabled();
    expect(api.fetchShare).toHaveBeenCalledTimes(1);
  });
});
