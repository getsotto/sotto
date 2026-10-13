import "@testing-library/jest-dom/vitest";
import { cleanup, render, screen, fireEvent } from "@testing-library/react";
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
  beforeEach(() => {
    vi.clearAllMocks();
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

  it("displays alert and triggers browser reload upon activation on session failure", async () => {
    // Spy on window.location.reload
    const originalLocation = window.location;
    Object.defineProperty(window, "location", {
      configurable: true,
      value: { ...originalLocation, reload: vi.fn() },
    });

    const deferred = createDeferred<any>();
    vi.mocked(me).mockReturnValue(deferred.promise);

    render(<AccountApp />);

    // Reject the promise with a synthetic error
    deferred.reject(new Error("Synthetic network error"));

    // Verify error UI is displayed
    const alertMsg = await screen.findByRole("alert");
    expect(alertMsg).toHaveTextContent("Synthetic network error");
    expect(screen.queryByTestId("mock-cloud-panel")).not.toBeInTheDocument();

    // Verify clicking Reload triggers window.location.reload
    const reloadBtn = screen.getByRole("button", { name: "Reload" });
    fireEvent.click(reloadBtn);

    expect(window.location.reload).toHaveBeenCalledOnce();

    // Restore window.location mockup to not break other test files
    Object.defineProperty(window, "location", {
      configurable: true,
      value: originalLocation,
    });
  });
});