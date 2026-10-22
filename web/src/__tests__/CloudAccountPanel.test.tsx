import { StrictMode } from "react";
import "@testing-library/jest-dom/vitest";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import * as api from "../api";
import type { EligibilityView } from "../api";
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

  it("shows the verified founding price and interval to the payer without vault unlock", async () => {
    const effectiveAtEpoch = 1_900_000_000;
    vi.mocked(api.fetchEligibility).mockResolvedValue({
      ...base, state: "paid", payer: "personal",
    });
    vi.mocked(api.fetchCloudNotices).mockResolvedValue([{
      noticeId: "notice-price-change", kind: "price_change", channel: "in_app",
      content: {
        title: "Your founding price is changing",
        detail: "Your founding price ends on the date shown. Your next renewal will use the standard monthly price shown.",
        effectiveAtEpoch, deadlineEpoch: null, amountPence: 299, priceInterval: "month",
      },
      dueAtEpoch: effectiveAtEpoch - 60 * 24 * 60 * 60, status: "pending",
      lastErrorCode: null, deliveredAtEpoch: null, createdAtEpoch: effectiveAtEpoch,
    }]);
    render(<CloudAccountPanel />);

    expect(await screen.findByRole("heading", { name: "Account notices" })).toBeInTheDocument();
    expect(screen.getByText("Your founding price is changing")).toBeInTheDocument();
    expect(screen.getByText(`From ${new Date(effectiveAtEpoch * 1000).toLocaleDateString("en-GB")}, your price will be £2.99 per month.`)).toBeInTheDocument();
    expect(api.fetchCloudNotices).toHaveBeenCalledOnce();
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

  it("requests initial eligibility only once during StrictMode effect replay", async () => {
    vi.mocked(api.fetchEligibility).mockResolvedValue({ ...base, state: "unavailable" });

    render(
      <StrictMode>
        <CloudAccountPanel />
      </StrictMode>,
    );

    expect(await screen.findByText(/temporarily unavailable/)).toBeInTheDocument();
    expect(api.fetchEligibility).toHaveBeenCalledTimes(1);
  });

  it("does not offer checkout while eligibility evidence is unavailable", async () => {
    vi.mocked(api.fetchEligibility).mockResolvedValue({ ...base, state: "unavailable" });

    render(<CloudAccountPanel />);

    expect(await screen.findByText(/temporarily unavailable/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /checkout/i })).not.toBeInTheDocument();
    expect(api.fetchPersonalQuote).not.toHaveBeenCalled();
  });

  it.each(["failure", "empty"] as const)(
    "keeps export recovery available with an optional notices %s",
    async (outcome) => {
      vi.mocked(api.fetchEligibility).mockResolvedValue({
        ...base, state: "export_only", actions: { ...base.actions, export: true },
      });
      if (outcome === "failure") {
        vi.mocked(api.fetchCloudNotices).mockRejectedValueOnce(new Error("synthetic notice failure"));
      }
      const view = render(<CloudAccountPanel />);
      await screen.findByRole("button", { name: "Download encrypted export" });
      await act(async () => {});

      expect(api.fetchCloudNotices).toHaveBeenCalledOnce();
      expect(screen.getByRole("heading", { name: "Hosted access" })).toBeInTheDocument();
      expect(screen.getByRole("button", { name: "Download encrypted export" })).toBeEnabled();
      expect(screen.queryByRole("heading", { name: "Account notices" })).not.toBeInTheDocument();
      expect(screen.queryByRole("alert")).not.toBeInTheDocument();
      expect(api.startCloudExport).not.toHaveBeenCalled();
      expect(api.fetchCloudExportChunk).not.toHaveBeenCalled();
      expect(api.fetchPersonalQuote).not.toHaveBeenCalled();
      expect(api.createPersonalCheckout).not.toHaveBeenCalled();

      if (outcome === "failure") {
        view.unmount();
        const exportDeadline = 1_900_000_000;
        vi.mocked(api.fetchCloudNotices).mockResolvedValue([{
          noticeId: "notice-current", kind: "export_deadline", channel: "in_app",
          content: { title: "Current export notice", detail: "Keep your encrypted recovery materials.", effectiveAtEpoch: 1_800_000_000, deadlineEpoch: exportDeadline, amountPence: null },
          dueAtEpoch: 1_900_000_000, status: "pending", lastErrorCode: null,
          deliveredAtEpoch: null, createdAtEpoch: 1_900_000_000,
        }]);
        render(<CloudAccountPanel />);
        expect(await screen.findByRole("heading", { name: "Account notices" })).toBeInTheDocument();
        expect(screen.getByText("Current export notice")).toBeInTheDocument();
        expect(screen.getByText("Keep your encrypted recovery materials.")).toBeInTheDocument();
        expect(screen.getByText(`Deadline: ${new Date(exportDeadline * 1000).toLocaleDateString("en-GB")}`)).toBeInTheDocument();
        expect(api.fetchCloudNotices).toHaveBeenCalledTimes(2);
        expect(screen.getByRole("button", { name: "Download encrypted export" })).toBeEnabled();
        expect(screen.queryByRole("alert")).not.toBeInTheDocument();
        expect(api.startCloudExport).not.toHaveBeenCalled();
        expect(api.createPersonalCheckout).not.toHaveBeenCalled();
      }
    },
  );

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
      offer: "standard_monthly", amountPence: 299, currency: "gbp", interval: "month", taxTreatment: "shown_at_checkout",
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
      offer: "founding_monthly", amountPence: 199, currency: "gbp", interval: "month", taxTreatment: "shown_at_checkout",
      quoteVersion: 1, quoteExpiresAtEpoch: 2_000_000_000, founding: true,
      foundingRemainingPlaces: 12, foundingTerm: "for the first 12 monthly billing periods", nextRenewalAmountPence: 299,
    });

    render(<CloudAccountPanel />);

    expect(await screen.findByText(/£1\.99 per month/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /secure checkout/i })).toBeInTheDocument();
    expect(screen.getByText(/12 places remain/)).toBeInTheDocument();
    expect(screen.getByText(/standard price after the founding term is £2\.99/i)).toBeInTheDocument();
  });

  it("shows the founding boundary without claiming an unconfirmed schedule is active", async () => {
    vi.mocked(api.fetchEligibility).mockResolvedValue({
      ...base, state: "paid", payer: "personal",
    });
    vi.mocked(api.fetchPersonalLifecycle).mockResolvedValue({
      state: "active", paidThroughDate: "2027-01-31", cancelAtPeriodEnd: false, portalUrl: null,
      foundingPriceEndEpoch: Date.UTC(2027, 0, 31) / 1000,
      standardRenewalAmountPence: 299,
      foundingTransitionStatus: "unresolved",
      foundingTransitionReason: "provider_authentication",
    });

    render(<CloudAccountPanel />);

    expect(await screen.findByText(/Founding price applies until 31\/01\/2027/i)).toBeInTheDocument();
    expect(screen.getByText(/not yet confirmed; we are checking it/i)).toBeInTheDocument();
    expect(screen.getByText(/Stripe credentials or permissions need attention/i)).toBeInTheDocument();
  });

  it("keeps a pending checkout recoverable", async () => {
    vi.mocked(api.fetchEligibility).mockResolvedValue({ ...base, state: "pending_initial_payment" });
    vi.mocked(api.fetchPersonalOperation).mockResolvedValue({
      operationId: "operation-1", offer: "standard_monthly", state: "pending", checkoutUrl: null,
    });
    sessionStorage.setItem("sotto_personal_operation_id", "operation-1");

    render(<CloudAccountPanel />);

    expect(await screen.findByRole("heading", { name: "Payment confirmation pending" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Refresh payment status" })).toBeInTheDocument();
  });

  it("warns when a pending checkout has been parked for manual review", async () => {
    vi.mocked(api.fetchEligibility).mockResolvedValue({ ...base, state: "pending_initial_payment" });
    vi.mocked(api.fetchPersonalOperation).mockResolvedValue({
      operationId: "operation-1", offer: "standard_monthly", state: "manual_review", checkoutUrl: null,
    });
    sessionStorage.setItem("sotto_personal_operation_id", "operation-1");

    render(<CloudAccountPanel />);

    expect(await screen.findByRole("alert")).toHaveTextContent(/do not start another checkout/i);
  });

  it("shows durable account notices without unlocking the vault", async () => {
    vi.mocked(api.fetchEligibility).mockResolvedValue({ ...base, state: "renewal_recovery", payer: "personal" });
    vi.mocked(api.createPersonalPortal).mockResolvedValue({
      state: "past_due", paidThroughDate: null, cancelAtPeriodEnd: false, portalUrl: null,
      foundingPriceEndEpoch: null, standardRenewalAmountPence: null,
      foundingTransitionStatus: null, foundingTransitionReason: null,
    });
    vi.mocked(api.fetchCloudNotices).mockResolvedValue([{
      noticeId: "notice-1", kind: "failed_renewal", channel: "in_app",
      content: { title: "Action needed for your Cloud renewal", detail: "Your renewal payment was unsuccessful. Update your payment method in Cloud billing before the recovery deadline.", effectiveAtEpoch: 1_900_000_000, deadlineEpoch: 1_901_209_600, amountPence: null },
      dueAtEpoch: 1_900_000_000, status: "pending", lastErrorCode: null, deliveredAtEpoch: null, createdAtEpoch: 1_900_000_000,
    }]);

    const { unmount } = render(<CloudAccountPanel />);

    expect(await screen.findByRole("heading", { name: "Account notices" })).toBeInTheDocument();
    expect(screen.getByText("Your renewal payment was unsuccessful. Update your payment method in Cloud billing before the recovery deadline.")).toBeInTheDocument();
    expect(screen.getByText(/Deadline:/)).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Update payment method" }));
    expect(await screen.findByText("The billing portal is not available yet.")).toBeInTheDocument();
    expect(api.createPersonalPortal).toHaveBeenCalledTimes(1);

    unmount();
    vi.mocked(api.fetchCloudNotices).mockResolvedValue([{
      noticeId: "notice-2", kind: "recovery_resolved", channel: "in_app",
      content: { title: "Your Cloud renewal is up to date", detail: "Your renewal payment was received. No further action is needed.", effectiveAtEpoch: 1_900_000_100, deadlineEpoch: null, amountPence: null },
      dueAtEpoch: 1_900_000_100, status: "pending", lastErrorCode: null, deliveredAtEpoch: null, createdAtEpoch: 1_900_000_100,
    }]);
    render(<CloudAccountPanel />);
    expect(await screen.findByText("Your Cloud renewal is up to date")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Update payment method" })).not.toBeInTheDocument();
  });

  it("directs sponsored renewal actions to organisation admins instead of personal billing", async () => {
    vi.mocked(api.fetchEligibility).mockResolvedValue({ ...base, state: "renewal_recovery" });
    vi.mocked(api.fetchCloudNotices).mockResolvedValue([{
      noticeId: "sponsored-notice-1", kind: "sponsored_renewal_action", channel: "in_app",
      content: {
        title: "Organisation Cloud renewal needs attention",
        detail: "The organisation’s sponsored Cloud renewal payment was unsuccessful. Review the organisation billing panel before the recovery deadline.",
        effectiveAtEpoch: 1_900_000_000, deadlineEpoch: 1_901_209_600, amountPence: null,
      },
      dueAtEpoch: 1_900_000_000, status: "pending", lastErrorCode: null,
      deliveredAtEpoch: null, createdAtEpoch: 1_900_000_000,
    }]);

    render(<CloudAccountPanel />);

    expect(await screen.findByText(/Review the organisation billing panel/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Update payment method" })).not.toBeInTheDocument();
    expect(api.createPersonalPortal).not.toHaveBeenCalled();
  });
});
