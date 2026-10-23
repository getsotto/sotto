import "@testing-library/jest-dom/vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import * as api from "../api";
import { PayerTransferInbox } from "../PayerTransferInbox";
import { PersonalTransferStart, SponsoredTransferStart } from "../PayerTransferStart";

vi.mock("../api", () => ({
  acceptPayerTransfer: vi.fn(),
  beginPayerTransfer: vi.fn(),
  createPayerTransferPaymentSetup: vi.fn(),
  fetchPayerTransfer: vi.fn(),
  fetchMembers: vi.fn(),
  fetchOrgs: vi.fn(),
  fetchPayerTransferCapabilities: vi.fn(),
  fetchPayerTransfers: vi.fn(),
  me: vi.fn(),
  quotePayerTransfer: vi.fn(),
  withdrawPayerTransfer: vi.fn(),
}));

const quote: api.PayerTransferQuote = {
  quoteId: "quote-1",
  quoteVersion: 1,
  quoteExpiresAtEpoch: 2_000_000_000,
  beneficiaryId: "beneficiary-1",
  counterpartyUserId: "admin-2",
  sourceKind: "personal",
  sourceOrganizationId: null,
  destinationKind: "sponsor",
  destinationOrganizationId: "org-b",
  offer: "standard_monthly",
  amountPence: 299,
  currency: "gbp",
  interval: "month",
  effectiveFrom: 2_000_000_500,
  creditPence: 0,
  founding: false,
};

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

function org(id: string, role = "member"): api.Org {
  return { id, encName: new Uint8Array(), role, encOrgKey: null };
}

function member(userId: string, role: string): api.Member {
  return { userId, role, publicKey: null };
}

beforeEach(() => {
  vi.resetAllMocks();
  vi.mocked(api.fetchPayerTransferCapabilities).mockResolvedValue({ available: true, setupAvailable: true });
  vi.mocked(api.fetchPayerTransfers).mockResolvedValue({ transfers: [], nextCursor: null });
  vi.mocked(api.fetchPayerTransfer).mockResolvedValue(transfer());
  vi.mocked(api.me).mockResolvedValue({ userId: "user-1" });
  vi.mocked(api.fetchOrgs).mockResolvedValue([org("org-a", "member"), org("org-b", "member")]);
  vi.mocked(api.fetchMembers).mockResolvedValue([member("admin-2", "owner")]);
  vi.mocked(api.quotePayerTransfer).mockResolvedValue(quote);
  vi.mocked(api.beginPayerTransfer).mockResolvedValue(transfer({ participantRole: "initiator" }));
  vi.mocked(api.acceptPayerTransfer).mockResolvedValue(transfer({ state: "pending" }));
  vi.mocked(api.withdrawPayerTransfer).mockResolvedValue(transfer({ state: "failed", resultCode: "consent_withdrawn" }));
});

afterEach(cleanup);

describe("payer transfer journeys", () => {
  it("hides the inbox and initiation when the server does not advertise the capability", async () => {
    vi.mocked(api.fetchPayerTransferCapabilities).mockResolvedValue({ available: false, setupAvailable: false });
    const view = render(<><PayerTransferInbox /><PersonalTransferStart /></>);
    await waitFor(() => expect(api.fetchPayerTransferCapabilities).toHaveBeenCalled());
    expect(view.container).toBeEmptyDOMElement();
  });

  it("lets the named counterparty accept an incoming invitation without claiming it is complete", async () => {
    vi.mocked(api.fetchPayerTransfers)
      .mockResolvedValueOnce({ transfers: [transfer()], nextCursor: null })
      .mockResolvedValue({ transfers: [transfer({ state: "pending" })], nextCursor: null });
    render(<PayerTransferInbox />);
    fireEvent.click(await screen.findByRole("button", { name: "Accept transfer request" }));
    expect(api.acceptPayerTransfer).toHaveBeenCalledWith("transfer-1");
    expect(await screen.findByText(/Accepted\. The billing change is being prepared/)).toBeInTheDocument();
  });

  it("loads older transfer pages using the server cursor", async () => {
    const older = transfer({ transferId: "transfer-older", participantRole: "initiator", state: "completed" });
    vi.mocked(api.fetchPayerTransfers)
      .mockResolvedValueOnce({ transfers: [transfer()], nextCursor: "transfer-1" })
      .mockResolvedValueOnce({ transfers: [older], nextCursor: null });
    render(<PayerTransferInbox />);
    expect(await screen.findByText("Incoming request")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Load older transfers" }));
    expect(await screen.findByText("Outgoing request")).toBeInTheDocument();
    expect(api.fetchPayerTransfers).toHaveBeenLastCalledWith("transfer-1");
    expect(screen.queryByRole("button", { name: "Load older transfers" })).not.toBeInTheDocument();
  });

  it("does not claim an empty inbox when the first list request fails", async () => {
    vi.mocked(api.fetchPayerTransfers)
      .mockRejectedValueOnce(new Error("server error (503)"))
      .mockRejectedValueOnce(new Error("server error (503)"))
      .mockResolvedValueOnce({ transfers: [], nextCursor: null })
      .mockResolvedValueOnce({ transfers: [transfer()], nextCursor: null });
    render(<PayerTransferInbox />);
    expect(await screen.findByRole("alert")).toHaveTextContent("The transfer service is temporarily unavailable");
    const refreshButton = screen.getByRole("button", { name: "Refresh transfers" });
    expect(refreshButton).toBeEnabled();
    expect(screen.queryByText("There are no payer transfer requests for this account.")).not.toBeInTheDocument();

    fireEvent.click(refreshButton);
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(await screen.findByRole("alert")).toBeInTheDocument();
    expect(screen.queryByText("There are no payer transfer requests for this account.")).not.toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "Refresh transfers" }));
    expect(await screen.findByText("There are no payer transfer requests for this account.")).toBeInTheDocument();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "Refresh transfers" }));
    expect(await screen.findByText("Incoming request")).toBeInTheDocument();
    expect(screen.queryByText("There are no payer transfer requests for this account.")).not.toBeInTheDocument();
  });

  it("keeps loaded requests visible when a later refresh fails", async () => {
    vi.mocked(api.fetchPayerTransfers)
      .mockResolvedValueOnce({ transfers: [transfer()], nextCursor: null })
      .mockRejectedValueOnce(new Error("server error (503)"));
    render(<PayerTransferInbox />);
    expect(await screen.findByText("Incoming request")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Refresh transfers" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("The transfer service is temporarily unavailable");
    expect(screen.getByText("Incoming request")).toBeInTheDocument();
    expect(screen.queryByText("There are no payer transfer requests for this account.")).not.toBeInTheDocument();
  });

  it("shows invitations as expired once their renewal boundary has passed", async () => {
    vi.mocked(api.fetchPayerTransfers).mockResolvedValue({
      transfers: [transfer({ effectiveFrom: 1 })],
      nextCursor: null,
    });
    render(<PayerTransferInbox />);
    expect(await screen.findByText(/expired at the renewal boundary/i)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Accept transfer request" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Decline request" })).not.toBeInTheDocument();
  });

  it("expires an open invitation at its deadline without a refresh or click", async () => {
    vi.useFakeTimers();
    vi.setSystemTime(2_000_000_000_000);
    try {
      vi.mocked(api.fetchPayerTransfers).mockResolvedValue({
        transfers: [
          transfer({ transferId: "soon", consentExpiresAtEpoch: 2_000_000_001, effectiveFrom: 2_000_000_100 }),
          transfer({ transferId: "later", consentExpiresAtEpoch: 2_000_000_600, effectiveFrom: 2_000_000_700 }),
        ],
        nextCursor: null,
      });
      render(<PayerTransferInbox />);
      await act(async () => { await vi.advanceTimersByTimeAsync(0); });
      expect(screen.getAllByRole("button", { name: "Accept transfer request" })).toHaveLength(2);

      await act(async () => { await vi.advanceTimersByTimeAsync(1_000); });
      expect(screen.getAllByRole("button", { name: "Accept transfer request" })).toHaveLength(1);
      // The response deadline passed 99 seconds before renewal, so the copy must not blame renewal.
      expect(screen.getByText(/expired because its response deadline passed/i)).toBeInTheDocument();
      expect(screen.queryByText(/expired at the renewal boundary/i)).not.toBeInTheDocument();
      expect(api.fetchPayerTransfers).toHaveBeenCalledTimes(1);
    } finally {
      vi.useRealTimers();
    }
  });

  it("expires an open invitation at the renewal boundary when renewal comes first", async () => {
    vi.useFakeTimers();
    vi.setSystemTime(2_000_000_000_000);
    try {
      vi.mocked(api.fetchPayerTransfers).mockResolvedValue({
        transfers: [transfer({ consentExpiresAtEpoch: 2_000_000_600, effectiveFrom: 2_000_000_001 })],
        nextCursor: null,
      });
      render(<PayerTransferInbox />);
      await act(async () => { await vi.advanceTimersByTimeAsync(0); });
      expect(screen.getByRole("button", { name: "Accept transfer request" })).toBeInTheDocument();

      await act(async () => { await vi.advanceTimersByTimeAsync(1_000); });
      expect(screen.queryByRole("button", { name: "Accept transfer request" })).not.toBeInTheDocument();
      expect(screen.getByText(/expired at the renewal boundary/i)).toBeInTheDocument();
      expect(screen.queryByText(/response deadline passed/i)).not.toBeInTheDocument();
    } finally {
      vi.useRealTimers();
    }
  });

  it("shows invitations loaded after their response deadline as expired without blaming renewal", async () => {
    vi.mocked(api.fetchPayerTransfers).mockResolvedValue({
      transfers: [transfer({ consentExpiresAtEpoch: 1, effectiveFrom: 2 })],
      nextCursor: null,
    });
    render(<PayerTransferInbox />);
    expect(await screen.findByText(/expired because its response deadline passed/i)).toBeInTheDocument();
    expect(screen.queryByText(/expired at the renewal boundary/i)).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Accept transfer request" })).not.toBeInTheDocument();
  });

  it("keeps personal payer invitations unaccepted until setup is available", async () => {
    vi.mocked(api.fetchPayerTransferCapabilities).mockResolvedValue({ available: true, setupAvailable: false });
    vi.mocked(api.fetchPayerTransfers).mockResolvedValue({ transfers: [transfer({
      destinationKind: "personal",
      destinationOrganizationId: null,
    })], nextCursor: null });
    render(<PayerTransferInbox />);
    expect(await screen.findByText(/Personal payment setup is not available/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Accept transfer request" })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Decline request" })).toBeInTheDocument();
  });

  it("lets an organisation member ask a known admin to take over personal billing", async () => {
    render(<PersonalTransferStart />);
    fireEvent.change(await screen.findByLabelText("Organisation"), { target: { value: "org-b" } });
    fireEvent.change(await screen.findByLabelText("Other payer administrator"), { target: { value: "admin-2" } });
    fireEvent.click(screen.getByRole("button", { name: "Review payer transfer" }));
    expect(await screen.findByText(/At renewal on/)).toHaveTextContent("£2.99 per month");
    fireEvent.click(screen.getByRole("button", { name: "Send request for acceptance" }));
    await waitFor(() => expect(api.quotePayerTransfer).toHaveBeenCalledWith({
      beneficiaryId: "user-1",
      counterpartyUserId: "admin-2",
      destinationKind: "sponsor",
      destinationOrganizationId: "org-b",
    }));
    await waitFor(() => expect(api.beginPayerTransfer).toHaveBeenCalledWith({ quote, idempotencyKey: expect.any(String) }));
    expect(await screen.findByText(/The other payer must accept it/)).toBeInTheDocument();
  });

  it("does not claim a second payer must accept when the caller can authorise both sides", async () => {
    vi.mocked(api.fetchOrgs).mockResolvedValue([org("org-b", "admin")]);
    vi.mocked(api.fetchMembers).mockResolvedValue([member("user-1", "owner")]);
    vi.mocked(api.quotePayerTransfer).mockResolvedValue({ ...quote, counterpartyUserId: "user-1" });
    vi.mocked(api.beginPayerTransfer).mockResolvedValue(transfer({
      participantRole: "initiator",
      state: "pending",
    }));
    render(<PersonalTransferStart />);
    fireEvent.change(await screen.findByLabelText("Organisation"), { target: { value: "org-b" } });
    fireEvent.change(await screen.findByLabelText("Other payer administrator"), { target: { value: "user-1" } });
    fireEvent.click(screen.getByRole("button", { name: "Review payer transfer" }));
    await screen.findByText(/At renewal on/);
    fireEvent.click(screen.getByRole("button", { name: "Send request for acceptance" }));
    expect(await screen.findByText(/Transfer started\. Billing will change only after/)).toBeInTheDocument();
    expect(screen.queryByText(/other payer must accept/)).not.toBeInTheDocument();
  });

  it("lets a sponsor admin invite the beneficiary to take over personally", async () => {
    render(<SponsoredTransferStart sourceOrganizationId="org-a" seats={[{
      seatId: "seat-1", beneficiaryId: "beneficiary-1", offer: "standard_monthly",
      effectiveFrom: 1, effectiveUntil: null, state: "active", foundingPriceEndEpoch: null,
      standardRenewalAmountPence: null, foundingTransitionStatus: null, foundingTransitionReason: null,
    }]} organisations={[org("org-a", "admin"), org("org-b", "admin")]} />);
    fireEvent.change(await screen.findByLabelText("Sponsored member"), { target: { value: "beneficiary-1" } });
    fireEvent.click(screen.getByRole("button", { name: "Review payer transfer" }));
    await screen.findByText(/At renewal on/);
    expect(api.quotePayerTransfer).toHaveBeenCalledWith({
      beneficiaryId: "beneficiary-1",
      counterpartyUserId: "beneficiary-1",
      destinationKind: "personal",
      destinationOrganizationId: undefined,
    });
  });

  it("does not offer a personal destination when its setup capability is disabled", async () => {
    vi.mocked(api.fetchPayerTransferCapabilities).mockResolvedValue({ available: true, setupAvailable: false });
    render(<SponsoredTransferStart sourceOrganizationId="org-a" seats={[{
      seatId: "seat-1", beneficiaryId: "beneficiary-1", offer: "standard_monthly",
      effectiveFrom: 1, effectiveUntil: null, state: "active", foundingPriceEndEpoch: null,
      standardRenewalAmountPence: null, foundingTransitionStatus: null, foundingTransitionReason: null,
    }]} organisations={[org("org-a", "admin"), org("org-b", "admin")]} />);
    const destination = await screen.findByLabelText("New payer") as HTMLSelectElement;
    expect(destination.value).toBe("sponsor");
    expect(screen.getByRole("option", { name: "Member pays personally (not available yet)" })).toBeDisabled();
  });

  it("quotes a sponsor to sponsor transfer with the destination organisation's known admin", async () => {
    render(<SponsoredTransferStart sourceOrganizationId="org-a" seats={[{
      seatId: "seat-1", beneficiaryId: "beneficiary-1", offer: "standard_monthly",
      effectiveFrom: 1, effectiveUntil: null, state: "active", foundingPriceEndEpoch: null,
      standardRenewalAmountPence: null, foundingTransitionStatus: null, foundingTransitionReason: null,
    }]} organisations={[org("org-a", "admin"), org("org-b", "admin")]} />);
    fireEvent.change(await screen.findByLabelText("Sponsored member"), { target: { value: "beneficiary-1" } });
    fireEvent.change(screen.getByLabelText("New payer"), { target: { value: "sponsor" } });
    fireEvent.change(screen.getByLabelText("Destination organisation"), { target: { value: "org-b" } });
    fireEvent.change(await screen.findByLabelText("Other payer administrator"), { target: { value: "admin-2" } });
    fireEvent.click(screen.getByRole("button", { name: "Review payer transfer" }));
    await screen.findByText(/At renewal on/);
    expect(api.quotePayerTransfer).toHaveBeenCalledWith({
      beneficiaryId: "beneficiary-1",
      counterpartyUserId: "admin-2",
      destinationKind: "sponsor",
      destinationOrganizationId: "org-b",
    });
  });

  it("shows personal setup only while the beneficiary still needs it and setup is available", async () => {
    vi.mocked(api.fetchPayerTransfers).mockResolvedValue({ transfers: [transfer({
      state: "pending",
      destinationKind: "personal",
      destinationOrganizationId: null,
      beneficiaryId: "user-1",
      personalPaymentSetupNeeded: true,
    })], nextCursor: null });
    vi.mocked(api.createPayerTransferPaymentSetup).mockResolvedValue({ checkoutUrl: null });
    render(<PayerTransferInbox />);
    fireEvent.click(await screen.findByRole("button", { name: "Set up payment method" }));
    expect(api.createPayerTransferPaymentSetup).toHaveBeenCalledWith("transfer-1");
    expect(await screen.findByText(/Payment method setup is pending/)).toBeInTheDocument();
  });

  it("keeps manual-review transfers parked without customer resume actions", async () => {
    vi.mocked(api.fetchPayerTransfers).mockResolvedValue({ transfers: [transfer({ state: "unknown" })], nextCursor: null });
    render(<PayerTransferInbox />);
    expect(await screen.findByText(/needs support review/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /resume|accept|withdraw/i })).not.toBeInTheDocument();
  });
});
