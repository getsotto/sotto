import "@testing-library/jest-dom/vitest";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
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

function quote(offer: api.PersonalQuote["offer"]): api.PersonalQuote {
  return {
    offer, amountPence: offer === "monthly" ? 499 : 4999, currency: "gbp",
    interval: offer === "monthly" ? "month" : "year", taxTreatment: "shown_at_checkout",
    quoteVersion: 1, quoteExpiresAtEpoch: 2_000_000_000, founding: false,
    foundingRemainingPlaces: null, foundingTerm: null, nextRenewalAmountPence: 499,
  };
}

describe("CloudAccountPanel", () => {
  beforeEach(() => vi.resetAllMocks());

  beforeEach(() => vi.mocked(api.fetchCloudNotices).mockResolvedValue([]));

  it("retries a failed selected-term quote through repeated failure and successful recovery", async () => {
    vi.mocked(api.fetchEligibility).mockResolvedValue({
      ...base, state: "free", actions: { ...base.actions, billing: true },
    });
    const pending = deferred<api.PersonalQuote>();
    vi.mocked(api.fetchPersonalQuote)
      .mockRejectedValueOnce(new Error("synthetic monthly failure"))
      .mockRejectedValueOnce(new Error("synthetic annual failure"))
      .mockReturnValueOnce(pending.promise)
      .mockResolvedValueOnce(quote("annual"));
    render(<CloudAccountPanel />);

    expect(await screen.findByRole("alert")).toHaveTextContent("synthetic monthly failure");
    expect(screen.getByRole("button", { name: "Retry billing quote" })).toBeEnabled();
    fireEvent.change(screen.getByRole("combobox", { name: "Term" }), { target: { value: "annual" } });
    expect(await screen.findByRole("alert")).toHaveTextContent("synthetic annual failure");
    fireEvent.click(screen.getByRole("button", { name: "Retry billing quote" }));

    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(screen.getByRole("status")).toHaveTextContent("Loading billing quote");
    expect(screen.queryByRole("button", { name: "Retry billing quote" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /checkout/i })).not.toBeInTheDocument();
    expect(api.fetchPersonalQuote).toHaveBeenCalledTimes(3);
    expect(api.fetchPersonalQuote).toHaveBeenLastCalledWith("annual");
    expect(api.createPersonalCheckout).not.toHaveBeenCalled();
    await act(async () => { pending.reject(new Error("synthetic retry failure")); });
    expect(screen.getByRole("alert")).toHaveTextContent("synthetic retry failure");
    expect(screen.getByRole("button", { name: "Retry billing quote" })).toBeEnabled();

    fireEvent.click(screen.getByRole("button", { name: "Retry billing quote" }));
    expect(await screen.findByText(/£49\.99 per year/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Continue to secure checkout" })).toBeEnabled();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(screen.queryByRole("status")).not.toBeInTheDocument();
    expect(api.fetchPersonalQuote).toHaveBeenCalledTimes(4);
    expect(api.fetchPersonalQuote).toHaveBeenLastCalledWith("annual");
    expect(api.createPersonalCheckout).not.toHaveBeenCalled();
  });

  it.each(["success", "failure"] as const)(
    "ignores a stale quote %s after changing the selected term",
    async (outcome) => {
      vi.mocked(api.fetchEligibility).mockResolvedValue({
        ...base, state: "free", actions: { ...base.actions, billing: true },
      });
      const monthly = deferred<api.PersonalQuote>();
      const annual = deferred<api.PersonalQuote>();
      vi.mocked(api.fetchPersonalQuote)
        .mockReturnValueOnce(monthly.promise)
        .mockReturnValueOnce(annual.promise);
      render(<CloudAccountPanel />);
      const term = await screen.findByRole("combobox", { name: "Term" });
      fireEvent.change(term, { target: { value: "annual" } });
      await act(async () => { annual.resolve(quote("annual")); });
      expect(screen.getByText(/£49\.99 per year/)).toBeInTheDocument();
      await act(async () => {
        if (outcome === "success") monthly.resolve(quote("monthly"));
        else monthly.reject(new Error("stale monthly failure"));
      });
      expect(term).toHaveValue("annual");
      expect(screen.getByText(/£49\.99 per year/)).toBeInTheDocument();
      expect(screen.queryByText(/£4\.99 per month/)).not.toBeInTheDocument();
      expect(screen.queryByRole("alert")).not.toBeInTheDocument();
      expect(screen.getByRole("button", { name: "Continue to secure checkout" })).toBeEnabled();
      expect(api.fetchPersonalQuote).toHaveBeenCalledTimes(2);
      expect(api.createPersonalCheckout).not.toHaveBeenCalled();
    },
  );

  it("does not offer checkout while eligibility evidence is unavailable", async () => {
    vi.mocked(api.fetchEligibility).mockResolvedValue({ ...base, state: "unavailable" });

    render(<CloudAccountPanel />);

    expect(await screen.findByText(/temporarily unavailable/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /checkout/i })).not.toBeInTheDocument();
    expect(api.fetchPersonalQuote).not.toHaveBeenCalled();
  });

  it("shows the server quote before offering a personal checkout", async () => {
    vi.mocked(api.fetchEligibility).mockResolvedValue({ ...base, state: "free", actions: { ...base.actions, billing: true } });
    vi.mocked(api.fetchPersonalQuote).mockResolvedValue({
      offer: "monthly", amountPence: 299, currency: "gbp", interval: "month", taxTreatment: "shown_at_checkout",
      quoteVersion: 1, quoteExpiresAtEpoch: 2_000_000_000, founding: true,
      foundingRemainingPlaces: 12, foundingTerm: "through the founding monthly term", nextRenewalAmountPence: 499,
    });

    render(<CloudAccountPanel />);

    expect(await screen.findByText(/£2\.99 per month/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /secure checkout/i })).toBeInTheDocument();
    expect(screen.getByText(/12 places remain/)).toBeInTheDocument();
  });

  it("keeps a pending checkout recoverable", async () => {
    vi.mocked(api.fetchEligibility).mockResolvedValue({ ...base, state: "pending_initial_payment" });
    vi.mocked(api.fetchPersonalOperation).mockResolvedValue({
      operationId: "operation-1", offer: "monthly", state: "pending", checkoutUrl: null,
    });
    sessionStorage.setItem("sotto_personal_operation_id", "operation-1");

    render(<CloudAccountPanel />);

    expect(await screen.findByRole("heading", { name: "Payment confirmation pending" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Refresh payment status" })).toBeInTheDocument();
  });

  it("shows durable account notices without unlocking the vault", async () => {
    vi.mocked(api.fetchEligibility).mockResolvedValue({ ...base, state: "export_only", actions: { ...base.actions, export: true } });
    vi.mocked(api.fetchCloudNotices).mockResolvedValue([{
      noticeId: "notice-1", kind: "export_deadline", channel: "in_app",
      content: { title: "Export window", detail: "Download your encrypted export.", effectiveAtEpoch: null, deadlineEpoch: 2_000_000_000, amountPence: null },
      dueAtEpoch: 1_900_000_000, status: "pending", lastErrorCode: null, deliveredAtEpoch: null, createdAtEpoch: 1_900_000_000,
    }]);

    render(<CloudAccountPanel />);

    expect(await screen.findByRole("heading", { name: "Account notices" })).toBeInTheDocument();
    expect(screen.getByText("Download your encrypted export.")).toBeInTheDocument();
  });
});
