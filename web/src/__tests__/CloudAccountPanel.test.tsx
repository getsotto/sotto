import "@testing-library/jest-dom/vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import * as api from "../api";
import { CloudAccountPanel } from "../CloudAccountPanel";

vi.mock("../api", () => ({
  cancelPersonalBilling: vi.fn(),
  createPersonalCheckout: vi.fn(),
  createPersonalPortal: vi.fn(),
  fetchCloudExportChunk: vi.fn(),
  fetchEligibility: vi.fn(),
  fetchPersonalQuote: vi.fn(),
  requestPersonalRefund: vi.fn(),
  startCloudExport: vi.fn(),
}));

afterEach(cleanup);

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
});
