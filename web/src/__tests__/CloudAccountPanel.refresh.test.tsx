import "@testing-library/jest-dom/vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import * as api from "../api";
import { CloudAccountPanel } from "../CloudAccountPanel";

vi.mock("../api", () => ({
  confirmCloudContactVerification: vi.fn(),
  fetchCloudContactStatus: vi.fn(),
  UnsupportedCloudContactError: class UnsupportedCloudContactError extends Error {},
  revokeCloudContact: vi.fn(),
  startCloudContactVerification: vi.fn(),
  cancelPersonalBilling: vi.fn(),
  acceptPayerTransfer: vi.fn(),
  beginPayerTransfer: vi.fn(),
  createPayerTransferPaymentSetup: vi.fn(),
  createPersonalCheckout: vi.fn(),
  createPersonalPortal: vi.fn(),
  fetchCloudExportChunk: vi.fn(),
  fetchCloudNotices: vi.fn(),
  fetchPayerTransferCapabilities: vi.fn(),
  fetchPayerTransfer: vi.fn(),
  fetchPayerTransfers: vi.fn(),
  fetchMembers: vi.fn(),
  fetchOrgs: vi.fn(),
  fetchEligibility: vi.fn(),
  fetchPersonalQuote: vi.fn(),
  me: vi.fn(),
  quotePayerTransfer: vi.fn(),
  fetchPersonalOperation: vi.fn(),
  requestPersonalRefund: vi.fn(),
  withdrawPayerTransfer: vi.fn(),
  startCloudExport: vi.fn(),
}));

const storageKey = "sotto_personal_operation_id";
const eligibility: api.EligibilityView = {
  state: "pending_initial_payment",
  accountInitialized: true,
  billingAvailable: true,
  paidThroughEpoch: null,
  recoveryUntilEpoch: null,
  exportUntilEpoch: null,
  actions: { setup: false, billing: false, export: false, revoke: false },
  payer: null,
  deploymentMode: "cloud",
};
const pendingOperation: api.PersonalOperation = {
  operationId: "operation-saved",
  offer: "monthly",
  state: "pending",
  checkoutUrl: "https://checkout.example.test/saved",
};

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve, reject };
}

beforeEach(() => {
  vi.resetAllMocks();
  vi.mocked(api.fetchPayerTransferCapabilities).mockResolvedValue({ available: false, setupAvailable: false });
  vi.mocked(api.fetchPayerTransfers).mockResolvedValue({ transfers: [], nextCursor: null });
  vi.mocked(api.me).mockResolvedValue(null);
  vi.mocked(api.fetchOrgs).mockResolvedValue([]);
  vi.mocked(api.fetchMembers).mockResolvedValue([]);
  vi.mocked(api.fetchEligibility).mockResolvedValue(eligibility);
  vi.mocked(api.fetchCloudNotices).mockResolvedValue([]);
  vi.mocked(api.fetchCloudContactStatus).mockResolvedValue({ email: null, pendingExpiresAtEpoch: null, enrolmentAvailable: false });
});

afterEach(() => {
  cleanup();
  sessionStorage.clear();
});

describe("CloudAccountPanel saved operation refresh", () => {
  it("does not overlap a manual refresh with the automatic lookup", async () => {
    sessionStorage.setItem(storageKey, pendingOperation.operationId);
    const automaticLookup = deferred<api.PersonalOperation>();
    vi.mocked(api.fetchPersonalOperation).mockReturnValueOnce(automaticLookup.promise);

    render(<CloudAccountPanel />);

    const loadingButton = await screen.findByRole("button", {
      name: "Refreshing payment status…",
    });
    expect(loadingButton).toBeDisabled();
    expect(loadingButton).toHaveAttribute("aria-busy", "true");
    expect(api.fetchPersonalOperation).toHaveBeenCalledExactlyOnceWith(
      pendingOperation.operationId,
    );

    fireEvent.click(loadingButton);
    fireEvent.click(loadingButton);
    expect(api.fetchPersonalOperation).toHaveBeenCalledTimes(1);

    await act(async () => {
      automaticLookup.resolve(pendingOperation);
    });

    expect(await screen.findByRole("link", { name: "Return to checkout" }))
      .toHaveAttribute("href", pendingOperation.checkoutUrl);
    expect(await screen.findByRole("button", { name: "Refresh payment status" }))
      .toBeEnabled();
    expect(sessionStorage.getItem(storageKey)).toBe(pendingOperation.operationId);
  });

  it("blocks repeated manual clicks and allows another refresh after a pending result", async () => {
    sessionStorage.setItem(storageKey, pendingOperation.operationId);
    vi.mocked(api.fetchPersonalOperation).mockResolvedValueOnce(pendingOperation);
    render(<CloudAccountPanel />);

    const refreshButton = await screen.findByRole("button", {
      name: "Refresh payment status",
    });
    await waitFor(() => expect(api.fetchPersonalOperation).toHaveBeenCalledTimes(1));

    const manualLookup = deferred<api.PersonalOperation>();
    vi.mocked(api.fetchPersonalOperation).mockReturnValueOnce(manualLookup.promise);
    fireEvent.click(refreshButton);

    const loadingButton = await screen.findByRole("button", {
      name: "Refreshing payment status…",
    });
    expect(loadingButton).toBeDisabled();
    expect(api.fetchPersonalOperation).toHaveBeenCalledTimes(2);

    fireEvent.click(loadingButton);
    fireEvent.click(loadingButton);
    expect(api.fetchPersonalOperation).toHaveBeenCalledTimes(2);

    await act(async () => {
      manualLookup.resolve(pendingOperation);
    });
    expect(await screen.findByRole("button", { name: "Refresh payment status" }))
      .toBeEnabled();
    expect(sessionStorage.getItem(storageKey)).toBe(pendingOperation.operationId);

    vi.mocked(api.fetchPersonalOperation).mockResolvedValueOnce(pendingOperation);
    fireEvent.click(screen.getByRole("button", { name: "Refresh payment status" }));
    await waitFor(() => expect(api.fetchPersonalOperation).toHaveBeenCalledTimes(3));
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "Refresh payment status" }))
        .toBeEnabled(),
    );

    expect(vi.mocked(api.fetchPersonalOperation).mock.calls).toEqual([
      [pendingOperation.operationId],
      [pendingOperation.operationId],
      [pendingOperation.operationId],
    ]);
    expect(api.createPersonalCheckout).not.toHaveBeenCalled();
    expect(api.fetchPersonalQuote).not.toHaveBeenCalled();
  });

  it("releases the loading control and preserves stale-operation cleanup on rejection", async () => {
    sessionStorage.setItem(storageKey, pendingOperation.operationId);
    const failedLookup = deferred<api.PersonalOperation>();
    vi.mocked(api.fetchPersonalOperation).mockResolvedValueOnce(pendingOperation);
    render(<CloudAccountPanel />);

    const refreshButton = await screen.findByRole("button", {
      name: "Refresh payment status",
    });
    await waitFor(() => expect(api.fetchPersonalOperation).toHaveBeenCalledTimes(1));

    vi.mocked(api.fetchPersonalOperation).mockReturnValueOnce(failedLookup.promise);
    fireEvent.click(refreshButton);
    expect(await screen.findByRole("button", {
      name: "Refreshing payment status…",
    })).toBeDisabled();

    await act(async () => {
      failedLookup.reject(new Error("synthetic lookup failure"));
    });

    expect(await screen.findByRole("button", { name: "Refresh payment status" }))
      .toBeEnabled();
    expect(sessionStorage.getItem(storageKey)).toBeNull();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(api.fetchPersonalOperation).toHaveBeenCalledTimes(2);
    expect(api.createPersonalCheckout).not.toHaveBeenCalled();
  });
});
