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
  fetchPersonalLifecycle: vi.fn(),
  fetchPersonalQuote: vi.fn(),
  me: vi.fn(),
  quotePayerTransfer: vi.fn(),
  fetchPersonalOperation: vi.fn(),
  requestPersonalRefund: vi.fn(),
  withdrawPayerTransfer: vi.fn(),
  startCloudExport: vi.fn(),
}));

afterEach(() => {
  cleanup();
  sessionStorage.clear();
});

const base = {
  accountInitialized: true,
  billingAvailable: true,
  paidThroughEpoch: null,
  recoveryUntilEpoch: null,
  exportUntilEpoch: null,
  actions: { setup: false, billing: false, export: false, revoke: false },
  payer: null,
  deploymentMode: "cloud" as const,
};

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((res, rej) => { resolve = res; reject = rej; });
  return { promise, resolve, reject };
}

function quote(offer: api.PersonalBillingTerm): api.PersonalQuote {
  return {
    offer: offer === "monthly" ? "standard_monthly" : "standard_annual", amountPence: offer === "monthly" ? 499 : 4999, currency: "gbp",
    interval: offer === "monthly" ? "month" : "year", taxTreatment: "shown_at_checkout",
    quoteVersion: 1, quoteExpiresAtEpoch: 2_000_000_000, founding: false,
    foundingRemainingPlaces: null, foundingTerm: null, nextRenewalAmountPence: 499,
  };
}

describe("CloudAccountPanel", () => {
  beforeEach(() => vi.resetAllMocks());

  beforeEach(() => {
    vi.mocked(api.fetchPayerTransferCapabilities).mockResolvedValue({ available: false, setupAvailable: false });
    vi.mocked(api.fetchPayerTransfers).mockResolvedValue({ transfers: [], nextCursor: null });
    vi.mocked(api.me).mockResolvedValue(null);
    vi.mocked(api.fetchOrgs).mockResolvedValue([]);
    vi.mocked(api.fetchMembers).mockResolvedValue([]);
    vi.mocked(api.fetchCloudNotices).mockResolvedValue([]);
  vi.mocked(api.fetchCloudContactStatus).mockResolvedValue({ email: null, pendingExpiresAtEpoch: null, enrolmentAvailable: false });
    vi.mocked(api.fetchPersonalLifecycle).mockResolvedValue({
      state: "active", paidThroughDate: null, cancelAtPeriodEnd: false, portalUrl: null,
      foundingPriceEndEpoch: null, standardRenewalAmountPence: null, foundingTransitionStatus: null, foundingTransitionReason: null,
    });
  });


  afterEach(() => vi.restoreAllMocks());

  const PENDING_NOTICE = "Checkout is pending provider confirmation. Reload this page shortly.";

  async function renderFreeAccountWithQuote() {
    vi.mocked(api.fetchEligibility).mockResolvedValue({ ...base, state: "free", actions: { ...base.actions, billing: true } });
    vi.mocked(api.fetchPersonalQuote).mockResolvedValue(quote("monthly"));
    const assign = vi.spyOn(window.location, "assign").mockImplementation(() => {});
    render(<CloudAccountPanel />);
    const button = await screen.findByRole("button", { name: "Continue to secure checkout" });
    expect(api.createPersonalCheckout).not.toHaveBeenCalled();
    return { assign, button };
  }

  function expectCheckoutRequest(call: number) {
    expect(api.createPersonalCheckout).toHaveBeenNthCalledWith(call, expect.objectContaining({
      offer: "standard_monthly", quoteVersion: 1, quoteExpiresAtEpoch: 2_000_000_000, returnUrl: window.location.origin,
    }));
  }

  async function failFirstCheckout(button: HTMLElement, assign: ReturnType<typeof vi.spyOn>) {
    const first = deferred<api.PersonalCheckoutResult>();
    vi.mocked(api.createPersonalCheckout).mockReturnValueOnce(first.promise);

    fireEvent.click(button);
    expectCheckoutRequest(1);
    const opening = screen.getByRole("button", { name: "Opening checkout…" });
    expect(opening).toBeDisabled();
    expect(screen.getByRole("combobox", { name: "Term" })).toBeDisabled();
    fireEvent.click(opening);
    expect(api.createPersonalCheckout).toHaveBeenCalledTimes(1);

    await act(async () => { first.reject(new Error("synthetic checkout failure")); });
    expect(screen.getByRole("alert")).toHaveTextContent("synthetic checkout failure");
    expect(screen.getByRole("button", { name: "Continue to secure checkout" })).toBeEnabled();
    expect(screen.getByRole("combobox", { name: "Term" })).toBeEnabled();
    expect(sessionStorage.getItem("sotto_personal_operation_id")).toBeNull();
    expect(assign).not.toHaveBeenCalled();
    expect(api.createPersonalCheckout).toHaveBeenCalledTimes(1);
  }

  async function retryCheckout(result: api.PersonalCheckoutResult) {
    const retry = deferred<api.PersonalCheckoutResult>();
    vi.mocked(api.createPersonalCheckout).mockReturnValueOnce(retry.promise);
    fireEvent.click(screen.getByRole("button", { name: "Continue to secure checkout" }));
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Opening checkout…" })).toBeDisabled();
    expect(screen.getByRole("combobox", { name: "Term" })).toBeDisabled();
    expect(api.createPersonalCheckout).toHaveBeenCalledTimes(2);
    expectCheckoutRequest(2);
    await act(async () => { retry.resolve(result); });
  }

  it("recovers from a failed personal checkout and opens the returned checkout URL on retry", async () => {
    const { assign, button } = await renderFreeAccountWithQuote();
    await failFirstCheckout(button, assign);
    // Record what was stored at the moment of navigation; an expect() thrown inside the spy would be
    // swallowed by the component's catch and reported as an unrelated alert.
    let storedAtNavigation: string | null = "not navigated";
    assign.mockImplementation(() => {
      storedAtNavigation = sessionStorage.getItem("sotto_personal_operation_id");
    });

    await retryCheckout({ operationId: "operation-1", state: "checkout_pending", checkoutUrl: "https://checkout.example/session-1" });
    expect(assign).toHaveBeenCalledExactlyOnceWith("https://checkout.example/session-1");
    expect(storedAtNavigation).toBe("operation-1");
    expect(sessionStorage.getItem("sotto_personal_operation_id")).toBe("operation-1");
    expect(screen.queryByText(PENDING_NOTICE)).not.toBeInTheDocument();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });

  it("recovers from a failed personal checkout and shows the pending notice when no URL comes back", async () => {
    const { assign, button } = await renderFreeAccountWithQuote();
    await failFirstCheckout(button, assign);

    await retryCheckout({ operationId: "operation-2", state: "checkout_pending", checkoutUrl: null });
    expect(await screen.findByText(PENDING_NOTICE)).toBeInTheDocument();
    expect(sessionStorage.getItem("sotto_personal_operation_id")).toBe("operation-2");
    expect(assign).not.toHaveBeenCalled();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    await waitFor(() => expect(screen.getByRole("combobox", { name: "Term" })).toBeEnabled());
  });
});
