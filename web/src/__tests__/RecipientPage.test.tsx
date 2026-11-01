import "@testing-library/jest-dom/vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
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

  it("focuses the revealed heading after a successful reveal", async () => {
    render(<RecipientPage token="share-token" />);
    const reveal = screen.getByRole("button", { name: "Reveal secret" });
    reveal.focus();
    expect(reveal).toHaveFocus();
    fireEvent.click(reveal);
    expect(await screen.findByRole("textbox", { name: "Shared secret" })).toHaveValue(secret);
    await waitFor(() => {
      expect(screen.getByRole("heading", { name: "Shared secret" })).toHaveFocus();
    });
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

    const copy = screen.getByRole("button", { name: "Copy secret" });
    copy.focus();
    fireEvent.click(copy);
    expect(writeText).toHaveBeenCalledWith(secret);
    expect(screen.queryByRole("status")).not.toBeInTheDocument();
    expect(screen.getByRole("heading", { name: "Shared secret" })).not.toHaveFocus();

    await act(async () => write.resolve());
    expect(await screen.findByRole("status")).toHaveTextContent("Copied to clipboard.");
    expect(copy).toHaveFocus();
    expect(api.fetchShare).toHaveBeenCalledTimes(1);
  });

  it("keeps the secret available and gives manual-copy guidance when a write rejects", async () => {
    const writeText = vi.fn().mockRejectedValue(new Error("permission denied"));
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: { writeText },
    });

    const textarea = await revealSecret();
    await waitFor(() =>
      expect(screen.getByRole("heading", { name: "Shared secret" })).toHaveFocus(),
    );
    const copy = screen.getByRole("button", { name: "Copy secret" });
    copy.focus();
    fireEvent.click(copy);

    expect(await screen.findByRole("alert")).toHaveTextContent("copy it manually");
    expect(copy).toHaveFocus();
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

  it("reveals after correcting the passphrase without fetching again", async () => {
    const encBlob = new Uint8Array([2]);
    const salt = new Uint8Array(16).fill(3);
    const wrongKey = new Uint8Array(32).fill(4);
    const correctKey = new Uint8Array(32).fill(5);

    vi.mocked(api.fetchShare).mockResolvedValue({
      encBlob,
      passphraseSalt: salt,
    });

    vi.mocked(wasm.share_passphrase_key).mockImplementation(
        (_fragmentKey, passphraseBytes) => {
          const passphrase = new TextDecoder().decode(passphraseBytes);
          return passphrase === "correct-passphrase" ? correctKey : wrongKey;
        },
    );

    vi.mocked(wasm.share_open).mockImplementation((key) => {
      if (key !== correctKey) {
        throw new Error("decryption failed");
      }
      return new TextEncoder().encode(secret);
    });

    render(<RecipientPage token="share-token" />);

    const input = screen.getByLabelText(
        "Passphrase (only if the sender set one)",
    );

    // First attempt fetches the payload but cannot decrypt it.
    fireEvent.change(input, {
      target: { value: "wrong-passphrase" },
    });
    fireEvent.click(
        screen.getByRole("button", { name: "Reveal secret" }),
    );

    expect(await screen.findByRole("alert")).toHaveTextContent(
        "decryption failed",
    );
    expect(api.fetchShare).toHaveBeenCalledExactlyOnceWith("share-token");
    expect(
        screen.queryByRole("textbox", { name: "Shared secret" }),
    ).not.toBeInTheDocument();

    // Correcting the passphrase should decrypt the cached payload.
    fireEvent.change(input, {
      target: { value: "correct-passphrase" },
    });
    fireEvent.click(
        screen.getByRole("button", { name: "Reveal secret" }),
    );

    expect(
        await screen.findByRole("textbox", { name: "Shared secret" }),
    ).toHaveValue(secret);
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(wasm.share_open).toHaveBeenLastCalledWith(correctKey, encBlob);
    expect(api.fetchShare).toHaveBeenCalledExactlyOnceWith("share-token");
  });

  it("reveals after supplying a missing passphrase without fetching again", async () => {
    const encBlob = new Uint8Array([2]);
    const salt = new Uint8Array(16).fill(3);
    const correctKey = new Uint8Array(32).fill(5);

    vi.mocked(api.fetchShare).mockResolvedValue({
      encBlob,
      passphraseSalt: salt,
    });
    vi.mocked(wasm.share_passphrase_key).mockReturnValue(correctKey);

    render(<RecipientPage token="share-token" />);

    // First attempt has no passphrase.
    fireEvent.click(
        screen.getByRole("button", { name: "Reveal secret" }),
    );

    expect(await screen.findByRole("alert")).toHaveTextContent(
        "this link requires a passphrase",
    );
    expect(api.fetchShare).toHaveBeenCalledExactlyOnceWith("share-token");
    expect(wasm.share_passphrase_key).not.toHaveBeenCalled();
    expect(wasm.share_open).not.toHaveBeenCalled();

    // Supplying the passphrase should use the cached payload.
    fireEvent.change(
        screen.getByLabelText("Passphrase (only if the sender set one)"),
        { target: { value: "correct-passphrase" } },
    );
    fireEvent.click(
        screen.getByRole("button", { name: "Reveal secret" }),
    );

    expect(
        await screen.findByRole("textbox", { name: "Shared secret" }),
    ).toHaveValue(secret);
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();

    expect(wasm.share_passphrase_key).toHaveBeenCalledExactlyOnceWith(
        new Uint8Array(32),
        new TextEncoder().encode("correct-passphrase"),
        salt,
    );
    expect(wasm.share_open).toHaveBeenCalledExactlyOnceWith(
        correctKey,
        encBlob,
    );
    expect(api.fetchShare).toHaveBeenCalledExactlyOnceWith("share-token");
  });

  it.each(["wasm", "fetch"] as const)(
      "prevents overlapping submissions while waiting for %s",
      async (phase) => {
        const pending = deferred();
        const payload = {
          encBlob: new Uint8Array([2]),
          passphraseSalt: null,
        };

        if (phase === "wasm") {
          vi.mocked(wasm.loadWasm).mockReturnValue(pending.promise);
        } else {
          vi.mocked(api.fetchShare).mockImplementation(async () => {
            await pending.promise;
            return payload;
          });
        }

        render(
            <StrictMode>
              <RecipientPage token="share-token" />
            </StrictMode>,
        );

        const form = screen
            .getByRole("button", { name: "Reveal secret" })
            .closest("form")!;

        // Submit twice before React can render a disabled button.
        act(() => {
          fireEvent.submit(form);
          fireEvent.submit(form);
        });

        await act(async () => {});

        expect(wasm.loadWasm).toHaveBeenCalledTimes(1);
        expect(api.fetchShare).toHaveBeenCalledTimes(
            phase === "wasm" ? 0 : 1,
        );
        expect(
            screen.getByRole("button", { name: "Revealing…" }),
        ).toBeDisabled();

        await act(async () => pending.resolve());

        expect(
            await screen.findByRole("textbox", { name: "Shared secret" }),
        ).toHaveValue(secret);
        expect(api.fetchShare).toHaveBeenCalledExactlyOnceWith(
            "share-token",
        );
      },
  );

  it("discards the cached payload and passphrase when the token changes", async () => {
    const firstBlob = new Uint8Array([10]);
    const secondBlob = new Uint8Array([20]);

    vi.mocked(api.fetchShare)
        .mockResolvedValueOnce({
          encBlob: firstBlob,
          passphraseSalt: new Uint8Array(16).fill(3),
        })
        .mockResolvedValueOnce({
          encBlob: secondBlob,
          passphraseSalt: null,
        });

    vi.mocked(wasm.share_passphrase_key).mockReturnValue(
        new Uint8Array(32).fill(4),
    );

    vi.mocked(wasm.share_open)
        .mockImplementationOnce(() => {
          throw new Error("decryption failed");
        })
        .mockReturnValueOnce(new TextEncoder().encode(secret));

    const { rerender } = render(
        <RecipientPage token="first-token" />,
    );

    fireEvent.change(
        screen.getByLabelText("Passphrase (only if the sender set one)"),
        { target: { value: "first-link-passphrase" } },
    );
    fireEvent.click(
        screen.getByRole("button", { name: "Reveal secret" }),
    );

    expect(await screen.findByRole("alert")).toHaveTextContent(
        "decryption failed",
    );

    rerender(<RecipientPage token="second-token" />);

    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(
        screen.getByLabelText("Passphrase (only if the sender set one)"),
    ).toHaveValue("");

    // Changing tokens must not automatically claim another view.
    expect(api.fetchShare).toHaveBeenCalledTimes(1);

    fireEvent.click(
        screen.getByRole("button", { name: "Reveal secret" }),
    );

    expect(
        await screen.findByRole("textbox", { name: "Shared secret" }),
    ).toHaveValue(secret);

    expect(api.fetchShare).toHaveBeenCalledTimes(2);
    expect(api.fetchShare).toHaveBeenNthCalledWith(1, "first-token");
    expect(api.fetchShare).toHaveBeenNthCalledWith(2, "second-token");
    expect(wasm.share_open).toHaveBeenLastCalledWith(
        new Uint8Array(32),
        secondBlob,
    );
  });

  it.each(["resolve", "reject"] as const)(
      "ignores a previous token's request when it finishes with %s",
      async (outcome) => {
        let resolveFirst!: (share: api.Share) => void;
        let rejectFirst!: (error: Error) => void;

        const firstResponse = new Promise<api.Share>((resolve, reject) => {
          resolveFirst = resolve;
          rejectFirst = reject;
        });

        const secondBlob = new Uint8Array([20]);

        vi.mocked(api.fetchShare)
            .mockReturnValueOnce(firstResponse)
            .mockResolvedValueOnce({
              encBlob: secondBlob,
              passphraseSalt: null,
            });

        const { rerender } = render(
            <RecipientPage token="first-token" />,
        );

        fireEvent.click(
            screen.getByRole("button", { name: "Reveal secret" }),
        );
        await act(async () => {});

        expect(api.fetchShare).toHaveBeenCalledExactlyOnceWith(
            "first-token",
        );

        rerender(<RecipientPage token="second-token" />);
        fireEvent.click(
            screen.getByRole("button", { name: "Reveal secret" }),
        );

        expect(
            await screen.findByRole("textbox", { name: "Shared secret" }),
        ).toHaveValue(secret);

        await act(async () => {
          if (outcome === "resolve") {
            resolveFirst({
              encBlob: new Uint8Array([10]),
              passphraseSalt: null,
            });
          } else {
            rejectFirst(new Error("old request failed"));
          }
        });

        expect(
            screen.getByRole("textbox", { name: "Shared secret" }),
        ).toHaveValue(secret);
        expect(screen.queryByRole("alert")).not.toBeInTheDocument();

        // Only the second link's payload should reach decryption.
        expect(wasm.share_open).toHaveBeenCalledExactlyOnceWith(
            new Uint8Array(32),
            secondBlob,
        );
        expect(api.fetchShare).toHaveBeenCalledTimes(2);
      },
  );

  it("does not claim a view after leaving during WASM loading", async () => {
    const loading = deferred();
    vi.mocked(wasm.loadWasm).mockReturnValue(loading.promise);

    const { unmount } = render(
        <RecipientPage token="share-token" />,
    );

    fireEvent.click(
        screen.getByRole("button", { name: "Reveal secret" }),
    );

    expect(wasm.loadWasm).toHaveBeenCalledTimes(1);
    expect(api.fetchShare).not.toHaveBeenCalled();

    unmount();

    await act(async () => loading.resolve());

    expect(api.fetchShare).not.toHaveBeenCalled();
    expect(wasm.share_open).not.toHaveBeenCalled();
  });
});
