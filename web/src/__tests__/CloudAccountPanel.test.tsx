import "@testing-library/jest-dom/vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import * as api from "../api";
import type { EligibilityView } from "../api";
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

describe("CloudAccountPanel", () => {
  beforeEach(() => vi.resetAllMocks());

  beforeEach(() => vi.mocked(api.fetchCloudNotices).mockResolvedValue([]));

  it("does not offer checkout while eligibility evidence is unavailable", async () => {
    vi.mocked(api.fetchEligibility).mockResolvedValue({ ...base, state: "unavailable" });

    render(<CloudAccountPanel />);

    expect(await screen.findByText(/temporarily unavailable/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /checkout/i })).not.toBeInTheDocument();
    expect(api.fetchPersonalQuote).not.toHaveBeenCalled();
  });

  it("offers a retry when the initial eligibility request fails", async () => {
    vi.mocked(api.fetchEligibility).mockRejectedValueOnce(new Error("synthetic status failure"));

    render(<CloudAccountPanel />);

    expect(await screen.findByRole("alert")).toHaveTextContent("synthetic status failure");
    expect(screen.getByRole("button", { name: "Retry account status" })).toBeInTheDocument();
    expect(api.fetchPersonalQuote).not.toHaveBeenCalled();
  });

  it("clears the error while retrying, prevents overlap, and shows eligible controls on success", async () => {
    let resolveRetry!: (eligibility: EligibilityView) => void;
    vi.mocked(api.fetchEligibility)
      .mockRejectedValueOnce(new Error("synthetic status failure"))
      .mockImplementationOnce(() => new Promise((resolve) => { resolveRetry = resolve; }));
    vi.mocked(api.fetchPersonalQuote).mockResolvedValue({
      offer: "monthly", amountPence: 299, currency: "gbp", interval: "month", taxTreatment: "shown_at_checkout",
      quoteVersion: 1, quoteExpiresAtEpoch: 2_000_000_000, founding: false,
      foundingRemainingPlaces: null, foundingTerm: null, nextRenewalAmountPence: 499,
    });

    render(<CloudAccountPanel />);

    expect(await screen.findByRole("alert")).toHaveTextContent("synthetic status failure");
    fireEvent.click(screen.getByRole("button", { name: "Retry account status" }));

    expect(await screen.findByRole("status")).toHaveTextContent("Checking account status again");
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    const retrying = screen.getByRole("button", { name: "Retrying…" });
    expect(retrying).toBeDisabled();
    expect(api.fetchEligibility).toHaveBeenCalledTimes(2);
    fireEvent.click(retrying);
    expect(api.fetchEligibility).toHaveBeenCalledTimes(2);

    resolveRetry({ ...base, state: "free", actions: { ...base.actions, billing: true } });

    expect(await screen.findByRole("button", { name: /secure checkout/i })).toBeInTheDocument();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(api.fetchEligibility).toHaveBeenCalledTimes(2);
  });

  it("restores the retry control after a retry also fails", async () => {
    vi.mocked(api.fetchEligibility)
      .mockRejectedValueOnce(new Error("first status failure"))
      .mockRejectedValueOnce(new Error("second status failure"));

    render(<CloudAccountPanel />);

    expect(await screen.findByRole("alert")).toHaveTextContent("first status failure");
    fireEvent.click(screen.getByRole("button", { name: "Retry account status" }));

    expect(await screen.findByRole("alert")).toHaveTextContent("second status failure");
    expect(screen.getByRole("button", { name: "Retry account status" })).toBeEnabled();
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
