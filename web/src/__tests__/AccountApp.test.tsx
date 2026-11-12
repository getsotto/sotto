import "@testing-library/jest-dom/vitest";
import { cleanup, render, screen, fireEvent, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { AccountApp } from "../AccountApp";
import { me } from "../api";
import { startLogin } from "../login";

// 1. Mock the API and external dependencies exactly as imported in AccountApp.tsx
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

// Mock the Shell so it just passes children through, keeping tests focused on AccountApp logic
vi.mock("../Shell", () => ({
  Shell: ({ children }: { children: React.ReactNode }) => <div data-testid="mock-shell">{children}</div>,
}));

// Cleanup DOM after each test to prevent leaks
afterEach(cleanup);

// Utility to control the timing of the `me()` promise manually
function createDeferred<T>() {
  let resolveFn!: (value: T | PromiseLike<T>) => void;
  let rejectFn!: (reason?: any) => void;
  const promise = new Promise<T>((resolve, reject) => {
    resolveFn = resolve;
    rejectFn = reject;
  });
  return { promise, resolve: resolveFn, reject: rejectFn };
}

describe("AccountApp - Initial Session Discovery", () => {
  const originalLocation = window.location;

  beforeEach(() => {
    vi.clearAllMocks();
    Object.defineProperty(window, "location", {
      configurable: true,
      value: { ...originalLocation, reload: vi.fn() },
    });
  });

  afterEach(() => {
    Object.defineProperty(window, "location", {
      configurable: true,
      value: originalLocation,
    });
  });

  it("shows loading state and hides account panel while request is pending", () => {
    const deferred = createDeferred<any>();
    vi.mocked(me).mockReturnValue(deferred.promise);

    render(<AccountApp />);

    // Assert checking state text from the component
    expect(screen.getByText("Loading your Cloud account…")).toBeInTheDocument();
    expect(screen.queryByTestId("mock-cloud-panel")).not.toBeInTheDocument();
  });

  it("triggers login with /cloud when resolving as an anonymous visitor", async () => {
    const deferred = createDeferred<any>();
    vi.mocked(me).mockReturnValue(deferred.promise);

    render(<AccountApp />);

    // Resolve with null to simulate anonymous visitor
    deferred.resolve(null);

    // Wait for the UI to update to the loggedOut state
    const loginBtn = await screen.findByRole("button", { name: "Log in with GitHub" });
    fireEvent.click(loginBtn);

    expect(startLogin).toHaveBeenCalledWith("/cloud");
  });

  it("mounts the CloudAccountPanel when resolving with a signed-in user", async () => {
    const deferred = createDeferred<any>();
    vi.mocked(me).mockReturnValue(deferred.promise);

    render(<AccountApp />);

    // Resolve with a synthetic user object
    deferred.resolve({ id: "user123", username: "testuser" });

    // Wait for the panel to mount
    expect(await screen.findByTestId("mock-cloud-panel")).toBeInTheDocument();
  });

  it("retries session discovery after initial failure without reloading the page", async () => {
    const first = createDeferred<any>();
    const retryPending = createDeferred<any>();
    const retrySignedIn = createDeferred<any>();
    const retryAnonymous = createDeferred<any>();
    const retryFailAgain = createDeferred<any>();
    const retryRecovered = createDeferred<any>();

    vi.mocked(me)
      .mockReturnValueOnce(first.promise)
      .mockReturnValueOnce(retryPending.promise)
      .mockReturnValueOnce(retrySignedIn.promise)
      .mockReturnValueOnce(retryAnonymous.promise)
      .mockReturnValueOnce(retryFailAgain.promise)
      .mockReturnValueOnce(retryRecovered.promise);

    render(<AccountApp />);

    // Initial failure
    first.reject(new Error("Synthetic network error"));
    const alertMsg = await screen.findByRole("alert");
    expect(alertMsg).toHaveTextContent("Synthetic network error");
    expect(screen.queryByTestId("mock-cloud-panel")).not.toBeInTheDocument();
    const retryBtn = screen.getByRole("button", { name: "Retry" });

    // Pending retry: shows accessible pending status, clears previous error, no overlapping requests
    fireEvent.click(retryBtn);
    expect(screen.getByText("Loading your Cloud account…")).toBeInTheDocument();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(screen.queryByTestId("mock-cloud-panel")).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Retry" })).not.toBeInTheDocument();
    expect(me).toHaveBeenCalledTimes(2);
    // repeated activation not possible while pending (button not present)
    expect(window.location.reload).not.toHaveBeenCalled();

    // Successful retry with a user mounts the account panel
    retryPending.resolve({ id: "user123", username: "testuser" });
    expect(await screen.findByTestId("mock-cloud-panel")).toBeInTheDocument();
    expect(me).toHaveBeenCalledTimes(2);
    expect(window.location.reload).not.toHaveBeenCalled();
  });

  it("recovers to anonymous view on retry and allows login", async () => {
    const first = createDeferred<any>();
    const retry = createDeferred<any>();
    vi.mocked(me).mockReturnValueOnce(first.promise).mockReturnValueOnce(retry.promise);

    render(<AccountApp />);

    first.reject(new Error("Synthetic network error"));
    fireEvent.click(await screen.findByRole("button", { name: "Retry" }));
    retry.resolve(null);

    const loginBtn = await screen.findByRole("button", { name: "Log in with GitHub" });
    fireEvent.click(loginBtn);
    expect(startLogin).toHaveBeenCalledWith("/cloud");
    expect(window.location.reload).not.toHaveBeenCalled();
  });

  it("shows new error after repeated failure and can recover on next retry", async () => {
    const first = createDeferred<any>();
    const retryFail = createDeferred<any>();
    const retryRecover = createDeferred<any>();
    vi.mocked(me)
      .mockReturnValueOnce(first.promise)
      .mockReturnValueOnce(retryFail.promise)
      .mockReturnValueOnce(retryRecover.promise);

    render(<AccountApp />);

    first.reject(new Error("first failure"));
    fireEvent.click(await screen.findByRole("button", { name: "Retry" }));

    retryFail.reject(new Error("second failure"));
    expect(await screen.findByRole("alert")).toHaveTextContent("second failure");
    expect(screen.getByRole("button", { name: "Retry" })).toBeInTheDocument();
    expect(screen.queryByTestId("mock-cloud-panel")).not.toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "Retry" }));
    retryRecover.resolve({ id: "user123", username: "testuser" });
    expect(await screen.findByTestId("mock-cloud-panel")).toBeInTheDocument();
    expect(me).toHaveBeenCalledTimes(3);
    expect(startLogin).not.toHaveBeenCalled();
    expect(window.location.reload).not.toHaveBeenCalled();
  });
});
