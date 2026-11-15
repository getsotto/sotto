import "@testing-library/jest-dom/vitest";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { AccountApp } from "../AccountApp";
import { me } from "../api";
import { startLogin } from "../login";

vi.mock("../api", () => ({
  me: vi.fn(),
  logout: vi.fn(),
}));

vi.mock("../login", () => ({
  startLogin: vi.fn(),
}));

vi.mock("../CloudAccountPanel", () => ({
  CloudAccountPanel: () => <div data-testid="mock-cloud-panel">Mocked Panel</div>,
}));

vi.mock("../Shell", () => ({
  Shell: ({ children }: { children: React.ReactNode }) => (
    <div data-testid="mock-shell">{children}</div>
  ),
}));

afterEach(cleanup);

function createDeferred<T>() {
  let resolveFn!: (value: T | PromiseLike<T>) => void;
  let rejectFn!: (reason?: unknown) => void;
  const promise = new Promise<T>((resolve, reject) => {
    resolveFn = resolve;
    rejectFn = reject;
  });
  return { promise, resolve: resolveFn, reject: rejectFn };
}

describe("AccountApp - Initial Session Discovery", () => {
  let reloadSpy: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    vi.clearAllMocks();
    reloadSpy = vi.fn();
    Object.defineProperty(window, "location", {
      configurable: true,
      value: { ...window.location, reload: reloadSpy },
    });
  });

  it("shows loading state and hides account panel while request is pending", () => {
    const deferred = createDeferred<{ userId: string } | null>();
    vi.mocked(me).mockReturnValue(deferred.promise);

    render(<AccountApp />);

    expect(screen.getByRole("status")).toHaveTextContent("Loading your Cloud account…");
    expect(screen.queryByTestId("mock-cloud-panel")).not.toBeInTheDocument();
  });

  it("triggers login with /cloud when resolving as an anonymous visitor", async () => {
    const deferred = createDeferred<{ userId: string } | null>();
    vi.mocked(me).mockReturnValue(deferred.promise);

    render(<AccountApp />);

    await act(async () => {
      deferred.resolve(null);
    });

    const loginBtn = await screen.findByRole("button", { name: "Log in with GitHub" });
    fireEvent.click(loginBtn);

    expect(startLogin).toHaveBeenCalledWith("/cloud");
  });

  it("mounts the CloudAccountPanel when resolving with a signed-in user", async () => {
    const deferred = createDeferred<{ userId: string } | null>();
    vi.mocked(me).mockReturnValue(deferred.promise);

    render(<AccountApp />);

    await act(async () => {
      deferred.resolve({ userId: "user123" });
    });

    expect(await screen.findByTestId("mock-cloud-panel")).toBeInTheDocument();
  });

  it("shows an alert and Retry session after a session failure without reloading", async () => {
    const deferred = createDeferred<{ userId: string } | null>();
    vi.mocked(me).mockReturnValue(deferred.promise);

    render(<AccountApp />);

    await act(async () => {
      deferred.reject(new Error("Synthetic network error"));
    });

    expect(await screen.findByRole("alert")).toHaveTextContent("Synthetic network error");
    expect(screen.getByRole("button", { name: "Retry session" })).toBeEnabled();
    expect(screen.queryByTestId("mock-cloud-panel")).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Reload" })).not.toBeInTheDocument();
    expect(reloadSpy).not.toHaveBeenCalled();
  });

  it("shows pending status and ignores a second click while a retry is in flight", async () => {
    const first = createDeferred<{ userId: string } | null>();
    const retry = createDeferred<{ userId: string } | null>();
    vi.mocked(me).mockReturnValueOnce(first.promise).mockReturnValueOnce(retry.promise);

    render(<AccountApp />);
    await act(async () => {
      first.reject(new Error("first failure"));
    });

    fireEvent.click(await screen.findByRole("button", { name: "Retry session" }));
    expect(await screen.findByRole("status")).toHaveTextContent("Loading your Cloud account…");
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Retry session" })).not.toBeInTheDocument();
    expect(screen.queryByTestId("mock-cloud-panel")).not.toBeInTheDocument();
    expect(me).toHaveBeenCalledTimes(2);
    expect(reloadSpy).not.toHaveBeenCalled();
    expect(startLogin).not.toHaveBeenCalled();
  });

  it("mounts the account panel after a successful signed-in retry", async () => {
    const first = createDeferred<{ userId: string } | null>();
    const retry = createDeferred<{ userId: string } | null>();
    vi.mocked(me).mockReturnValueOnce(first.promise).mockReturnValueOnce(retry.promise);

    render(<AccountApp />);
    await act(async () => {
      first.reject(new Error("first failure"));
    });
    fireEvent.click(await screen.findByRole("button", { name: "Retry session" }));

    await act(async () => {
      retry.resolve({ userId: "user123" });
    });

    expect(await screen.findByTestId("mock-cloud-panel")).toBeInTheDocument();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(reloadSpy).not.toHaveBeenCalled();
  });

  it("shows the signed-out view after a successful anonymous retry", async () => {
    const first = createDeferred<{ userId: string } | null>();
    const retry = createDeferred<{ userId: string } | null>();
    vi.mocked(me).mockReturnValueOnce(first.promise).mockReturnValueOnce(retry.promise);

    render(<AccountApp />);
    await act(async () => {
      first.reject(new Error("first failure"));
    });
    fireEvent.click(await screen.findByRole("button", { name: "Retry session" }));

    await act(async () => {
      retry.resolve(null);
    });

    const loginBtn = await screen.findByRole("button", { name: "Log in with GitHub" });
    expect(screen.queryByTestId("mock-cloud-panel")).not.toBeInTheDocument();
    fireEvent.click(loginBtn);
    expect(startLogin).toHaveBeenCalledExactlyOnceWith("/cloud");
    expect(reloadSpy).not.toHaveBeenCalled();
  });

  it("shows a new error after a failed retry and recovers on the next attempt", async () => {
    const first = createDeferred<{ userId: string } | null>();
    const second = createDeferred<{ userId: string } | null>();
    const third = createDeferred<{ userId: string } | null>();
    vi.mocked(me)
      .mockReturnValueOnce(first.promise)
      .mockReturnValueOnce(second.promise)
      .mockReturnValueOnce(third.promise);

    render(<AccountApp />);
    await act(async () => {
      first.reject(new Error("first failure"));
    });
    fireEvent.click(await screen.findByRole("button", { name: "Retry session" }));

    await act(async () => {
      second.reject(new Error("second failure"));
    });
    expect(await screen.findByRole("alert")).toHaveTextContent("second failure");
    expect(screen.getByRole("button", { name: "Retry session" })).toBeEnabled();
    expect(screen.queryByTestId("mock-cloud-panel")).not.toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "Retry session" }));
    await act(async () => {
      third.resolve({ userId: "user123" });
    });

    expect(await screen.findByTestId("mock-cloud-panel")).toBeInTheDocument();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(me).toHaveBeenCalledTimes(3);
    expect(reloadSpy).not.toHaveBeenCalled();
  });
});
