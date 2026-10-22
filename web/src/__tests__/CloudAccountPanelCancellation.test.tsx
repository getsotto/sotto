import { act } from "react";
import "@testing-library/jest-dom/vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import * as api from "../api";
import type { EligibilityView, PersonalLifecycle } from "../api";
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

const paid: EligibilityView = {
  state: "paid",
  accountInitialized: true,
  billingAvailable: true,
  paidThroughEpoch: 1_893_499_200,
  recoveryUntilEpoch: null,
  exportUntilEpoch: null,
  actions: { setup: false, billing: false, export: false, revoke: true },
  payer: "personal",
  deploymentMode: "cloud",
};

const activeLifecycle: PersonalLifecycle = {
  state: "active", paidThroughDate: "2030-01-01", cancelAtPeriodEnd: false, portalUrl: null,
  foundingPriceEndEpoch: null, standardRenewalAmountPence: null,
  foundingTransitionStatus: null, foundingTransitionReason: null,
};

afterEach(cleanup);

describe("CloudAccountPanel cancellation", () => {
  beforeEach(() => {
    vi.resetAllMocks();
    vi.mocked(api.fetchCloudContactStatus).mockResolvedValue({
      email: null, pendingExpiresAtEpoch: null, enrolmentAvailable: false,
    });
    vi.mocked(api.fetchCloudNotices).mockResolvedValue([]);
    vi.mocked(api.fetchPersonalLifecycle).mockResolvedValue(activeLifecycle);
    vi.mocked(api.fetchPayerTransferCapabilities).mockResolvedValue({ available: false, setupAvailable: false });
    vi.mocked(api.me).mockResolvedValue(null);
    vi.mocked(api.fetchOrgs).mockResolvedValue([]);
  });

  it("blocks overlapping billing actions, recovers from a failed cancellation, and preserves the paid term on retry", async () => {
    let rejectFirst!: (reason?: unknown) => void;
    let resolveRetry!: (value: PersonalLifecycle) => void;
    vi.mocked(api.fetchEligibility).mockResolvedValue(paid);
    vi.mocked(api.cancelPersonalBilling)
      .mockImplementationOnce(() => new Promise((_, reject) => { rejectFirst = reject; }))
      .mockImplementationOnce(() => new Promise((resolve) => { resolveRetry = resolve; }));

    render(<CloudAccountPanel />);

    const cancel = await screen.findByRole("button", { name: "Cancel at the end of the paid period" });
    const portal = screen.getByRole("button", { name: "Open billing portal" });
    const refund = screen.getByRole("button", { name: "Request a refund" });
    expect(screen.getByText(/paid through \d{2}\/\d{2}\/2030/)).toBeInTheDocument();

    fireEvent.click(cancel);
    expect(cancel).toBeDisabled();
    expect(portal).toBeDisabled();
    expect(refund).toBeDisabled();
    fireEvent.click(cancel);
    fireEvent.click(portal);
    fireEvent.click(refund);
    expect(api.cancelPersonalBilling).toHaveBeenCalledTimes(1);

    await act(async () => { rejectFirst(new Error("server error (503)")); });
    expect(screen.getByRole("alert")).toHaveTextContent("server error (503)");
    expect(cancel).toBeEnabled();
    expect(portal).toBeEnabled();
    expect(refund).toBeEnabled();
    expect(screen.getByText(/paid through \d{2}\/\d{2}\/2030/)).toBeInTheDocument();
    expect(screen.queryByText("Cancellation requested. Your paid period remains available until it ends.")).not.toBeInTheDocument();

    fireEvent.click(cancel);
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(cancel).toBeDisabled();
    expect(api.cancelPersonalBilling).toHaveBeenCalledTimes(2);

    await act(async () => {
      resolveRetry({ ...activeLifecycle, state: "paid", cancelAtPeriodEnd: true });
    });
    expect(screen.getByRole("status")).toHaveTextContent("Cancellation requested. Your paid period remains available until it ends.");
    expect(screen.getByText("Cancellation is scheduled; access remains available until 2030-01-01.")).toBeInTheDocument();
    expect(screen.getByText(/paid through \d{2}\/\d{2}\/2030/)).toBeInTheDocument();
    expect(cancel).toBeEnabled();
    expect(api.createPersonalCheckout).not.toHaveBeenCalled();
    expect(api.createPersonalPortal).not.toHaveBeenCalled();
    expect(api.requestPersonalRefund).not.toHaveBeenCalled();
  });

  it("hides cancellation when the server does not permit revocation", async () => {
    vi.mocked(api.fetchEligibility).mockResolvedValue({ ...paid, actions: { ...paid.actions, revoke: false } });

    render(<CloudAccountPanel />);

    expect(await screen.findByRole("heading", { name: "Manage billing" })).toBeInTheDocument();
    expect(screen.getByText(/paid through \d{2}\/\d{2}\/2030/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Cancel at the end of the paid period" })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Open billing portal" })).toBeEnabled();
    expect(screen.getByRole("button", { name: "Request a refund" })).toBeEnabled();
    expect(api.cancelPersonalBilling).not.toHaveBeenCalled();
  });
});
