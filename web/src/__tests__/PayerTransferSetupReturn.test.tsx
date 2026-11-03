import "@testing-library/jest-dom/vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import { StrictMode } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import * as api from "../api";
import { PayerTransferInbox } from "../PayerTransferInbox";

vi.mock("../api", () => ({
  acceptPayerTransfer: vi.fn(),
  createPayerTransferPaymentSetup: vi.fn(),
  fetchPayerTransfer: vi.fn(),
  fetchPayerTransferCapabilities: vi.fn(),
  fetchPayerTransfers: vi.fn(),
  me: vi.fn(),
  withdrawPayerTransfer: vi.fn(),
}));

function transfer(overrides: Partial<api.PayerTransfer> = {}): api.PayerTransfer {
  return {
    transferId: "transfer-1",
    beneficiaryId: "beneficiary-1",
    participantRole: "counterparty",
    sourceKind: "personal",
    sourceOrganizationId: null,
    destinationKind: "sponsor",
    destinationOrganizationId: "org-b",
    offer: "standard_monthly",
    amountPence: 299,
    currency: "gbp",
    interval: "month",
    effectiveFrom: 2_000_000_500,
    quoteExpiresAtEpoch: 2_000_000_000,
    consentExpiresAtEpoch: 2_000_604_800,
    personalPaymentSetupNeeded: false,
    state: "awaiting_consent",
    resultCode: null,
    ...overrides,
  };
}

const NOW_MS = 1_900_000_000_000;
const PATH = "/cloud";
const UNRELATED = "?view=transfers&tag=a";
const RETURN_SUFFIX = "&tag=b#history";
const SETUP_NOTICE =
  "Payment method setup returned. Refreshing the transfer; provider confirmation is still required.";
const CANCELLED_NOTICE = "Payment method setup was cancelled. The transfer has not completed.";
const PENDING_COPY = "Accepted. The billing change is being prepared; it is not complete yet.";

function visit(search: string) {
  window.history.replaceState(null, "", PATH + search);
}

function renderInbox() {
  return render(<StrictMode><PayerTransferInbox /></StrictMode>);
}

beforeEach(() => {
  vi.resetAllMocks();
  vi.spyOn(Date, "now").mockReturnValue(NOW_MS);
  vi.mocked(api.fetchPayerTransferCapabilities).mockResolvedValue({ available: true, setupAvailable: true });
  vi.mocked(api.me).mockResolvedValue({ userId: "user-1" });
  // The list still shows the invitation; the targeted fetch reports what setup changed.
  vi.mocked(api.fetchPayerTransfers).mockResolvedValue({ transfers: [transfer()], nextCursor: null });
  vi.mocked(api.fetchPayerTransfer).mockResolvedValue(transfer({ state: "pending", destinationKind: "personal" }));
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  window.history.replaceState(null, "", "/");
});

function expectNoActions() {
  expect(api.acceptPayerTransfer).not.toHaveBeenCalled();
  expect(api.withdrawPayerTransfer).not.toHaveBeenCalled();
  expect(api.createPayerTransferPaymentSetup).not.toHaveBeenCalled();
}

describe.each([
  ["transfer_setup", SETUP_NOTICE],
  ["transfer_setup_cancelled", CANCELLED_NOTICE],
])("returning from payment setup with billing=%s", (outcome, notice) => {
  it("shows the notice, renders the refreshed transfer and consumes only the return parameters", async () => {
    visit(`${UNRELATED}&billing=${outcome}&transfer_id=transfer-1${RETURN_SUFFIX}`);
    const view = renderInbox();

    expect(await screen.findByText(notice)).toBeInTheDocument();
    expect(await screen.findByText(PENDING_COPY)).toBeInTheDocument();
    // StrictMode replays effects; the return must still be handled once.
    expect(api.fetchPayerTransfer).toHaveBeenCalledExactlyOnceWith("transfer-1");
    expect(screen.queryByText(/Payer change completed/)).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Accept transfer request" })).not.toBeInTheDocument();
    expect(window.location.pathname).toBe(PATH);
    expect(window.location.search).toBe("?view=transfers&tag=a&tag=b");
    expect(window.location.hash).toBe("#history");
    expectNoActions();

    view.unmount();
    vi.mocked(api.fetchPayerTransfer).mockClear();
    renderInbox();
    expect(await screen.findByText("Incoming request")).toBeInTheDocument();
    expect(screen.queryByText(notice)).not.toBeInTheDocument();
    expect(api.fetchPayerTransfer).not.toHaveBeenCalled();
    expectNoActions();
  });
});

it.each([
  ["absent", `${UNRELATED}&transfer_id=transfer-1${RETURN_SUFFIX}`],
  ["unknown", `${UNRELATED}&billing=something_else&transfer_id=transfer-1${RETURN_SUFFIX}`],
])("ignores an %s billing outcome without fetching or rewriting the URL", async (_, search) => {
  visit(search);
  renderInbox();

  expect(await screen.findByText("Incoming request")).toBeInTheDocument();
  await waitFor(() => expect(screen.queryByText("Loading transfer requests…")).not.toBeInTheDocument());
  expect(screen.queryByText(SETUP_NOTICE)).not.toBeInTheDocument();
  expect(screen.queryByText(CANCELLED_NOTICE)).not.toBeInTheDocument();
  expect(api.fetchPayerTransfer).not.toHaveBeenCalled();
  expect(window.location.pathname + window.location.search + window.location.hash).toBe(PATH + search);
  expectNoActions();
});
