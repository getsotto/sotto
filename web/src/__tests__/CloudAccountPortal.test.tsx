import "@testing-library/jest-dom/vitest";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import * as api from "../api";
import { CloudAccountPanel } from "../CloudAccountPanel";

vi.mock("../api", async (importOriginal) => ({
  ...await importOriginal<typeof import("../api")>(),
  cancelPersonalBilling: vi.fn(),
  createPersonalCheckout: vi.fn(),
  createPersonalPortal: vi.fn(),
  fetchCloudExportChunk: vi.fn(),
  fetchCloudNotices: vi.fn(),
  fetchCloudContactStatus: vi.fn(),
  fetchEligibility: vi.fn(),
  fetchOrgs: vi.fn(),
  fetchPayerTransferCapabilities: vi.fn(),
  fetchPersonalLifecycle: vi.fn(),
  fetchPersonalQuote: vi.fn(),
  fetchPersonalOperation: vi.fn(),
  me: vi.fn(),
  requestPersonalRefund: vi.fn(),
  startCloudExport: vi.fn(),
}));

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((res, rej) => { resolve = res; reject = rej; });
  return { promise, resolve, reject };
}

const lifecycle: api.PersonalLifecycle = {
  state: "active", paidThroughDate: null, cancelAtPeriodEnd: false, portalUrl: null,
  foundingPriceEndEpoch: null, standardRenewalAmountPence: null,
  foundingTransitionStatus: null, foundingTransitionReason: null,
};

describe("CloudAccountPanel billing portal", () => {
  beforeEach(() => {
    vi.resetAllMocks();
    sessionStorage.clear();
    vi.spyOn(window.location, "assign").mockImplementation(() => {});
    vi.mocked(api.fetchEligibility).mockResolvedValue({
      state: "paid", accountInitialized: true, billingAvailable: true,
      paidThroughEpoch: null, recoveryUntilEpoch: null, exportUntilEpoch: null,
      actions: { setup: false, billing: false, export: false, revoke: true },
      payer: "personal", deploymentMode: "cloud",
    });
    vi.mocked(api.fetchCloudNotices).mockResolvedValue([]);
    vi.mocked(api.fetchCloudContactStatus).mockRejectedValue(new api.UnsupportedCloudContactError());
    vi.mocked(api.fetchPayerTransferCapabilities).mockResolvedValue({ available: false, setupAvailable: false });
    vi.mocked(api.me).mockResolvedValue(null);
    vi.mocked(api.fetchOrgs).mockResolvedValue([]);
    vi.mocked(api.fetchPersonalLifecycle).mockResolvedValue(lifecycle);
  });

  afterEach(() => {
    cleanup();
    sessionStorage.clear();
    vi.restoreAllMocks();
  });

  it("allows retry after failure and handles an unavailable portal without navigating", async () => {
    const first = deferred<api.PersonalLifecycle>();
    const retry = deferred<api.PersonalLifecycle>();
    vi.mocked(api.createPersonalPortal)
      .mockReturnValueOnce(first.promise)
      .mockReturnValueOnce(retry.promise);
    render(<CloudAccountPanel />);
    const button = await screen.findByRole("button", { name: "Open billing portal" });

    fireEvent.click(button);
    expect(button).toBeDisabled();
    fireEvent.click(button);
    expect(api.createPersonalPortal).toHaveBeenCalledTimes(1);
    expect(window.location.assign).not.toHaveBeenCalled();

    await act(async () => { first.reject(new Error("synthetic portal failure")); });
    expect(screen.getByRole("alert")).toHaveTextContent("synthetic portal failure");
    expect(button).toBeEnabled();
    expect(window.location.assign).not.toHaveBeenCalled();

    fireEvent.click(button);
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(button).toBeDisabled();
    fireEvent.click(button);
    expect(api.createPersonalPortal).toHaveBeenCalledTimes(2);
    await act(async () => { retry.resolve(lifecycle); });

    expect(screen.getByText("The billing portal is not available yet.")).toBeInTheDocument();
    expect(button).toBeEnabled();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(window.location.assign).not.toHaveBeenCalled();
    expect(api.createPersonalCheckout).not.toHaveBeenCalled();
    expect(api.cancelPersonalBilling).not.toHaveBeenCalled();
    expect(api.requestPersonalRefund).not.toHaveBeenCalled();
  });

  it("navigates once to the portal URL returned by the server", async () => {
    const pending = deferred<api.PersonalLifecycle>();
    const portalUrl = "https://example.com/billing/synthetic-session";
    vi.mocked(api.createPersonalPortal).mockReturnValueOnce(pending.promise);
    render(<CloudAccountPanel />);
    const button = await screen.findByRole("button", { name: "Open billing portal" });
    fireEvent.click(button);
    expect(window.location.assign).not.toHaveBeenCalled();
    await act(async () => { pending.resolve({ ...lifecycle, portalUrl }); });

    expect(api.createPersonalPortal).toHaveBeenCalledExactlyOnceWith();
    expect(window.location.assign).toHaveBeenCalledExactlyOnceWith(portalUrl);
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(button).toBeEnabled();
    expect(api.createPersonalCheckout).not.toHaveBeenCalled();
    expect(api.cancelPersonalBilling).not.toHaveBeenCalled();
    expect(api.requestPersonalRefund).not.toHaveBeenCalled();
  });
});
