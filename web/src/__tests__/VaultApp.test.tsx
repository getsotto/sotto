import "@testing-library/jest-dom/vitest";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import * as api from "../api";
import * as vaultCrypto from "../vault";
import { VaultApp } from "../VaultApp";

vi.mock("../api", () => ({
  me: vi.fn(),
  fetchAccount: vi.fn(),
  logout: vi.fn(),
}));

vi.mock("../vault", () => ({
  deriveMasterKey: vi.fn(),
}));

vi.mock("../VaultView", () => ({
  VaultView: ({ onLogout }: { onLogout: () => void }) => (
    <div>
      vault-view
      <button onClick={onLogout}>Log out</button>
    </div>
  ),
}));

// Real startLogin touches window.location; the login-action tests only need to know it was
// invoked, not what it does.
vi.mock("../login", () => ({ startLogin: vi.fn() }));

afterEach(cleanup);

// Mirrors the `deferred()` helper in VaultView.test.tsx: lets a test control exactly when a
// mocked request settles, so it can assert on the state in between.
function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

function account(): api.Account {
  return { salt: new Uint8Array([1, 2, 3]), encPrivateKeys: new Uint8Array([4, 5, 6]) };
}

beforeEach(() => {
  vi.resetAllMocks();
});

describe("VaultApp startup transitions", () => {
  it("shows loading and does not request the account before the session check resolves", async () => {
    const session = deferred<{ userId: string } | null>();
    vi.mocked(api.me).mockReturnValue(session.promise);

    render(<VaultApp />);

    expect(screen.getByText("Loading…")).toBeInTheDocument();
    expect(api.fetchAccount).not.toHaveBeenCalled();

    // Let the pending check settle so no unresolved promise leaks into the next test.
    await act(async () => {
      session.resolve(null);
    });
  });

  it("shows the login action for a logged-out session and never requests the account", async () => {
    vi.mocked(api.me).mockResolvedValue(null);

    render(<VaultApp />);

    expect(await screen.findByRole("button", { name: "Log in with GitHub" })).toBeInTheDocument();
    expect(api.fetchAccount).not.toHaveBeenCalled();
  });

  it("requests the account once authenticated, without showing the unlock form while it is pending", async () => {
    vi.mocked(api.me).mockResolvedValue({ userId: "u1" });
    const accountReq = deferred<api.Account | null>();
    vi.mocked(api.fetchAccount).mockReturnValue(accountReq.promise);

    render(<VaultApp />);

    // The session check has to resolve before the account request fires; wait for that instead
    // of asserting immediately, which would only prove the initial render looked right.
    await screen.findByText("Loading…");
    await vi.waitFor(() => {
      expect(api.fetchAccount).toHaveBeenCalledTimes(1);
    });
    expect(screen.queryByText("Unlock your vault")).not.toBeInTheDocument();

    await act(async () => {
      accountReq.resolve(account());
    });
  });

  it("shows the locked form on a successful account lookup, without deriving a key or rendering the vault", async () => {
    vi.mocked(api.me).mockResolvedValue({ userId: "u1" });
    vi.mocked(api.fetchAccount).mockResolvedValue(account());

    render(<VaultApp />);

    expect(await screen.findByText("Unlock your vault")).toBeInTheDocument();
    expect(vaultCrypto.deriveMasterKey).not.toHaveBeenCalled();
    expect(screen.queryByText("vault-view")).not.toBeInTheDocument();
  });

  it("shows an alert and Reload on an account-request rejection, and Reload triggers one reload", async () => {
    vi.mocked(api.me).mockResolvedValue({ userId: "u1" });
    vi.mocked(api.fetchAccount).mockRejectedValue(new Error("server error (500)"));
    const reload = vi.fn();
    vi.spyOn(window.location, "reload").mockImplementation(reload);

    render(<VaultApp />);

    expect(await screen.findByRole("alert")).toHaveTextContent("server error (500)");
    expect(screen.getByRole("button", { name: "Reload" })).toBeInTheDocument();
    expect(screen.queryByText("Loading…")).not.toBeInTheDocument();
    expect(screen.queryByText("Unlock your vault")).not.toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "Reload" }));
    expect(reload).toHaveBeenCalledTimes(1);
  });
});

describe.each(["locked", "unlocked"] as const)("logout from the %s phase", (phase) => {
  async function showPhase() {
    vi.mocked(api.me).mockResolvedValue({ userId: "u1" });
    vi.mocked(api.fetchAccount).mockResolvedValue(account());
    render(<VaultApp />);

    expect(await screen.findByText("Unlock your vault")).toBeInTheDocument();
    if (phase === "unlocked") {
      vi.mocked(vaultCrypto.deriveMasterKey).mockResolvedValue(new Uint8Array([7]));
      fireEvent.change(screen.getByLabelText("Master password"), {
        target: { value: "master password" },
      });
      fireEvent.change(screen.getByLabelText("Secret key (SK1-…)"), {
        target: { value: "SK1-test" },
      });
      fireEvent.click(screen.getByRole("button", { name: "Unlock" }));
      expect(await screen.findByText("vault-view")).toBeInTheDocument();
    }
  }

  it("keeps the current view while logout is pending, then shows login", async () => {
    const request = deferred<void>();
    vi.mocked(api.logout).mockReturnValue(request.promise);
    await showPhase();

    fireEvent.click(screen.getByRole("button", { name: "Log out" }));

    expect(api.logout).toHaveBeenCalledTimes(1);
    expect(screen.queryByRole("button", { name: "Log in with GitHub" })).not.toBeInTheDocument();
    expect(
      screen.getByText(phase === "locked" ? "Unlock your vault" : "vault-view"),
    ).toBeInTheDocument();

    await act(async () => {
      request.resolve();
    });

    expect(screen.getByRole("button", { name: "Log in with GitHub" })).toBeInTheDocument();
    expect(screen.queryByText("Unlock your vault")).not.toBeInTheDocument();
    expect(screen.queryByText("vault-view")).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Log out" })).not.toBeInTheDocument();
  });

  it("shows the logout error and Reload when the request fails", async () => {
    const request = deferred<void>();
    vi.mocked(api.logout).mockReturnValue(request.promise);
    await showPhase();

    fireEvent.click(screen.getByRole("button", { name: "Log out" }));

    expect(api.logout).toHaveBeenCalledTimes(1);
    expect(screen.queryByRole("button", { name: "Log in with GitHub" })).not.toBeInTheDocument();
    expect(
      screen.getByText(phase === "locked" ? "Unlock your vault" : "vault-view"),
    ).toBeInTheDocument();

    await act(async () => {
      request.reject(new Error("logout failed (500)"));
    });

    expect(screen.getByRole("alert")).toHaveTextContent("logout failed (500)");
    expect(screen.getByRole("button", { name: "Reload" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Log in with GitHub" })).not.toBeInTheDocument();
    expect(screen.queryByText("Unlock your vault")).not.toBeInTheDocument();
    expect(screen.queryByText("vault-view")).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Log out" })).not.toBeInTheDocument();
  });
});
