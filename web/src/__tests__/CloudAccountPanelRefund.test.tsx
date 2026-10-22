import "@testing-library/jest-dom/vitest";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
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
  createPersonalCheckout: vi.fn(),
  createPersonalPortal: vi.fn(),
  fetchCloudExportChunk: vi.fn(),
  fetchCloudNotices: vi.fn(),
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

const paidPersonalAccount: api.EligibilityView = {
  state: "paid",
  accountInitialized: true,
  billingAvailable: true,
  paidThroughEpoch: 2_000_000_000,
  recoveryUntilEpoch: null,
  exportUntilEpoch: null,
  actions: { setup: false, billing: false, export: false, revoke: true },
  payer: "personal",
  deploymentMode: "cloud",
};

const requestedRefund: api.RefundRequest = {
  requestId: "refund-1",
  state: "requested",
  reason: "billing_error",
  amountPence: null,
  fullRefundRequested: true,
  preservePaidTerm: true,
  effectiveAtEpoch: null,
};

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: Error) => void;
  const promise = new Promise<T>((res, rej) => { resolve = res; reject = rej; });
  return { promise, resolve, reject };
}

afterEach(() => {
  cleanup();
  sessionStorage.clear();
});

describe("CloudAccountPanel personal refunds", () => {
  beforeEach(() => {
    vi.resetAllMocks();
    vi.mocked(api.fetchCloudContactStatus).mockResolvedValue({
      email: null, pendingExpiresAtEpoch: null, enrolmentAvailable: false,
    });
    vi.mocked(api.fetchEligibility).mockResolvedValue(paidPersonalAccount);
    vi.mocked(api.fetchCloudNotices).mockResolvedValue([]);
    vi.mocked(api.fetchPersonalLifecycle).mockResolvedValue({
      state: "active", paidThroughDate: null, cancelAtPeriodEnd: false, portalUrl: null,
      foundingPriceEndEpoch: null, standardRenewalAmountPence: null,
      foundingTransitionStatus: null, foundingTransitionReason: null,
    });
    vi.mocked(api.fetchPayerTransferCapabilities).mockResolvedValue({ available: false, setupAvailable: false });
    vi.mocked(api.me).mockResolvedValue(null);
    vi.mocked(api.fetchOrgs).mockResolvedValue([]);
  });

  it("submits one full refund request only after a click and blocks duplicates while pending", async () => {
    const pending = deferred<api.RefundRequest>();
    vi.mocked(api.requestPersonalRefund).mockReturnValue(pending.promise);

    render(<CloudAccountPanel />);

    const button = await screen.findByRole("button", { name: "Request a refund" });
    expect(screen.getByRole("heading", { name: "Manage billing" })).toBeInTheDocument();
    expect(api.requestPersonalRefund).not.toHaveBeenCalled();

    fireEvent.click(button);

    expect(api.requestPersonalRefund).toHaveBeenCalledWith({
      reason: "billing_error",
      fullRefund: true,
      idempotencyKey: expect.any(String),
    });
    expect(vi.mocked(api.requestPersonalRefund).mock.calls[0][0].idempotencyKey).toMatch(/\S/);
    expect(button).toBeDisabled();
    fireEvent.click(button);
    expect(api.requestPersonalRefund).toHaveBeenCalledTimes(1);

    await act(async () => pending.resolve(requestedRefund));

    expect(button).toBeEnabled();
    expect(screen.getByRole("status")).toHaveTextContent("Refund request submitted for review.");
    expect(screen.getByText("Refund request: requested. The paid term is preserved by default.")).toBeInTheDocument();
    expect(api.cancelPersonalBilling).not.toHaveBeenCalled();
    expect(api.createPersonalCheckout).not.toHaveBeenCalled();
    expect(api.createPersonalPortal).not.toHaveBeenCalled();
  });

  it("shows a failed request, clears the error on retry, and reports success", async () => {
    const failed = deferred<api.RefundRequest>();
    const retried = deferred<api.RefundRequest>();
    vi.mocked(api.requestPersonalRefund).mockReturnValueOnce(failed.promise).mockReturnValueOnce(retried.promise);

    render(<CloudAccountPanel />);
    const button = await screen.findByRole("button", { name: "Request a refund" });
    fireEvent.click(button);

    await act(async () => failed.reject(new Error("refund service unavailable")));

    expect(screen.getByRole("alert")).toHaveTextContent("refund service unavailable");
    expect(button).toBeEnabled();
    expect(screen.queryByRole("status")).not.toBeInTheDocument();
    expect(screen.queryByText(/Refund request: requested/)).not.toBeInTheDocument();
    expect(api.requestPersonalRefund).toHaveBeenCalledTimes(1);

    fireEvent.click(button);

    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(button).toBeDisabled();
    expect(api.requestPersonalRefund).toHaveBeenCalledTimes(2);

    await act(async () => retried.resolve(requestedRefund));

    expect(button).toBeEnabled();
    expect(screen.getByRole("status")).toHaveTextContent("Refund request submitted for review.");
    expect(screen.getByText("Refund request: requested. The paid term is preserved by default.")).toBeInTheDocument();
    expect(api.cancelPersonalBilling).not.toHaveBeenCalled();
    expect(api.createPersonalCheckout).not.toHaveBeenCalled();
    expect(api.createPersonalPortal).not.toHaveBeenCalled();
  });
});
