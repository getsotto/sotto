import "@testing-library/jest-dom/vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import * as api from "../api";
import { CloudAccountPanel } from "../CloudAccountPanel";

vi.mock("../api", () => ({
  cancelPersonalBilling: vi.fn(),
  createPersonalCheckout: vi.fn(),
  createPersonalPortal: vi.fn(),
  fetchCloudExportChunk: vi.fn(),
  fetchCloudNotices: vi.fn(),
  fetchEligibility: vi.fn(),
  fetchPersonalQuote: vi.fn(),
  fetchPersonalOperation: vi.fn(),
  requestPersonalRefund: vi.fn(),
  startCloudExport: vi.fn(),
}));

const storageKey = "sotto_personal_operation_id";
const eligibility: api.EligibilityView = {
  state: "pending_initial_payment", accountInitialized: true, billingAvailable: true,
  paidThroughEpoch: null, recoveryUntilEpoch: null, exportUntilEpoch: null,
  actions: { setup: false, billing: false, export: false, revoke: false },
  payer: null, deploymentMode: "cloud",
};

beforeEach(() => {
  vi.resetAllMocks();
  vi.mocked(api.fetchEligibility).mockResolvedValue(eligibility);
  vi.mocked(api.fetchCloudNotices).mockResolvedValue([]);
});

afterEach(() => {
  cleanup();
  sessionStorage.clear();
});

describe("Cloud saved payment-operation recovery", () => {
  it("preserves a pending operation and refreshes the same identity without starting checkout", async () => {
    sessionStorage.setItem(storageKey, "operation-saved");
    const pending: api.PersonalOperation = {
      operationId: "operation-saved", offer: "monthly", state: "pending",
      checkoutUrl: "https://checkout.example.test/saved",
    };
    vi.mocked(api.fetchPersonalOperation).mockResolvedValueOnce(pending);
    render(<CloudAccountPanel />);

    expect(await screen.findByRole("link", { name: "Return to checkout" }))
      .toHaveAttribute("href", pending.checkoutUrl);
    expect(sessionStorage.getItem(storageKey)).toBe("operation-saved");
    expect(api.fetchPersonalOperation).toHaveBeenCalledExactlyOnceWith("operation-saved");

    vi.mocked(api.fetchPersonalOperation).mockResolvedValueOnce(pending);
    fireEvent.click(screen.getByRole("button", { name: "Refresh payment status" }));
    await act(async () => {});

    expect(vi.mocked(api.fetchPersonalOperation).mock.calls).toEqual([["operation-saved"], ["operation-saved"]]);
    expect(sessionStorage.getItem(storageKey)).toBe("operation-saved");
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(api.createPersonalCheckout).not.toHaveBeenCalled();
    expect(api.fetchPersonalQuote).not.toHaveBeenCalled();
  });

  it.each(["succeeded", "failed"])(
    "clears a saved %s operation without treating it as eligibility evidence",
    async (state) => {
      sessionStorage.setItem(storageKey, "operation-finished");
      vi.mocked(api.fetchPersonalOperation).mockResolvedValue({
        operationId: "operation-finished", offer: "monthly", state, checkoutUrl: null,
      });
      render(<CloudAccountPanel />);

      await waitFor(() => expect(sessionStorage.getItem(storageKey)).toBeNull());
      expect(screen.getByRole("heading", { name: "Payment confirmation pending" })).toBeInTheDocument();
      expect(screen.queryByRole("link", { name: "Return to checkout" })).not.toBeInTheDocument();
      expect(screen.queryByRole("heading", { name: "Manage billing" })).not.toBeInTheDocument();
      fireEvent.click(screen.getByRole("button", { name: "Refresh payment status" }));
      await act(async () => {});
      expect(api.fetchPersonalOperation).toHaveBeenCalledExactlyOnceWith("operation-finished");
      expect(api.createPersonalCheckout).not.toHaveBeenCalled();
    },
  );

  it("drops an unavailable saved operation without a billing error or an automatic retry", async () => {
    sessionStorage.setItem(storageKey, "operation-stale");
    vi.mocked(api.fetchPersonalOperation).mockRejectedValue(new Error("synthetic stale operation"));
    render(<CloudAccountPanel />);

    await waitFor(() => expect(sessionStorage.getItem(storageKey)).toBeNull());
    expect(screen.getByRole("heading", { name: "Payment confirmation pending" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Refresh payment status" })).toBeEnabled();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Refresh payment status" }));
    await act(async () => {});
    expect(api.fetchPersonalOperation).toHaveBeenCalledExactlyOnceWith("operation-stale");
    expect(api.createPersonalCheckout).not.toHaveBeenCalled();
  });

  it("does not request an operation when the browser has no saved identity", async () => {
    render(<CloudAccountPanel />);
    fireEvent.click(await screen.findByRole("button", { name: "Refresh payment status" }));
    await act(async () => {});

    expect(api.fetchPersonalOperation).not.toHaveBeenCalled();
    expect(api.createPersonalCheckout).not.toHaveBeenCalled();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });
});
