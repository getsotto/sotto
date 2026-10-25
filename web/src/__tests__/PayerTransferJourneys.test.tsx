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

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve, reject };
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


  it("shows Declining while a decline request is in progress", async () => {
    let resolveDecline!: (value: api.PayerTransfer) => void;

    vi.mocked(api.withdrawPayerTransfer).mockReturnValue(
      new Promise((resolve) => {
        resolveDecline = resolve;
      }),
    );
    vi.mocked(api.fetchPayerTransfers).mockResolvedValue({
      transfers: [transfer()],
      nextCursor: null,
    });
    render(<PayerTransferInbox />);
    fireEvent.click(
      await screen.findByRole("button", { name: "Decline request" }),
    );
    expect(api.withdrawPayerTransfer).toHaveBeenCalledWith("transfer-1");
    expect(
      screen.getByRole("button", { name: "Declining…" }),
    ).toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "Accepting…" }),
    ).not.toBeInTheDocument();
    await act(async () => {
      resolveDecline(
        transfer({
          state: "failed",
          resultCode: "consent_withdrawn",
        }),
      );
    });
    await waitFor(() => {
      expect(
        screen.queryByRole("button", { name: "Declining…" }),
      ).not.toBeInTheDocument();
    });
  });

  
  it("clears the decline label after a failed request and successful recovery refresh", async () => {
    let rejectDecline!: (error: Error) => void;

    vi.mocked(api.withdrawPayerTransfer).mockReturnValue(
      new Promise((_, reject) => {
        rejectDecline = reject;
      }),
    );
    vi.mocked(api.fetchPayerTransfers).mockResolvedValue({
      transfers: [transfer()],
      nextCursor: null,
    });
    vi.mocked(api.fetchPayerTransfer).mockResolvedValue(
      transfer({ state: "awaiting_consent" }),
    );
    render(<PayerTransferInbox />);
    fireEvent.click(
      await screen.findByRole("button", { name: "Decline request" }),
    );
    expect(
      screen.getByRole("button", { name: "Declining…" }),
    ).toBeInTheDocument();
    await act(async () => {
      rejectDecline(new Error("server error (503)"));
    });
    await waitFor(() => {
      expect(api.fetchPayerTransfer).toHaveBeenCalledWith("transfer-1");
      expect(screen.getByRole("button", { name: "Accept transfer request" })).toBeInTheDocument();
      expect(screen.getByRole("button", { name: "Decline request" })).toBeInTheDocument();
      expect(screen.queryByRole("button", { name: "Declining…" })).not.toBeInTheDocument();
      expect(screen.queryByRole("button", { name: "Accepting…" })).not.toBeInTheDocument();
    });
    expect(api.acceptPayerTransfer).not.toHaveBeenCalled();
  });


  it("shows Accepting while an acceptance request is in progress", async () => {
    let resolveAccept!: (value: api.PayerTransfer) => void;

    vi.mocked(api.acceptPayerTransfer).mockReturnValue(
      new Promise((resolve) => {
        resolveAccept = resolve;
      }),
    );
    vi.mocked(api.fetchPayerTransfers).mockResolvedValue({
      transfers: [transfer()],
      nextCursor: null,
    });
    render(<PayerTransferInbox />);
    fireEvent.click(
      await screen.findByRole("button", { name: "Accept transfer request" }),
    );
    expect(api.acceptPayerTransfer).toHaveBeenCalledWith("transfer-1");
    expect(
      screen.getByRole("button", { name: "Accepting…" }),
    ).toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "Declining…" }),
    ).not.toBeInTheDocument();
    await act(async () => {
      resolveAccept(transfer({ state: "pending" }));
    });
    await waitFor(() => {
      expect(
        screen.queryByRole("button", { name: "Accepting…" }),
      ).not.toBeInTheDocument();
    });
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
      expect(screen.getByText(/expired at the renewal boundary/i)).toBeInTheDocument();
      expect(api.fetchPayerTransfers).toHaveBeenCalledTimes(1);
    } finally {
      vi.useRealTimers();
    }
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

  it("shows a recoverable error when the initial organisation lookup fails", async () => {
    vi.mocked(api.fetchOrgs).mockRejectedValue(new Error("Synthetic network error"));
    render(<PersonalTransferStart />);
    expect(await screen.findByRole("alert")).toHaveTextContent(/organisations could not be loaded/i);
    expect(screen.getByRole("button", { name: "Retry" })).toBeEnabled();
    expect(screen.queryByLabelText("Organisation")).not.toBeInTheDocument();
    expect(api.quotePayerTransfer).not.toHaveBeenCalled();
    expect(api.beginPayerTransfer).not.toHaveBeenCalled();
  });

  it("reloads initial data in place after a successful retry", async () => {
    const firstOrgs = deferred<api.Org[]>();
    vi.mocked(api.fetchOrgs).mockReturnValueOnce(firstOrgs.promise);
    render(<PersonalTransferStart />);
    await waitFor(() => expect(api.fetchPayerTransferCapabilities).toHaveBeenCalled());
    await act(async () => { firstOrgs.reject(new Error("Synthetic network error")); });
    expect(await screen.findByRole("alert")).toBeInTheDocument();

    const retryOrgs = deferred<api.Org[]>();
    vi.mocked(api.fetchOrgs).mockReturnValueOnce(retryOrgs.promise);
    fireEvent.click(screen.getByRole("button", { name: "Retry" }));
    const retrying = await screen.findByRole("button", { name: "Retrying…" });
    expect(retrying).toBeDisabled();
    expect(screen.getByRole("status")).toHaveTextContent(/Loading organisations/i);
    expect(api.quotePayerTransfer).not.toHaveBeenCalled();
    expect(api.beginPayerTransfer).not.toHaveBeenCalled();

    fireEvent.click(retrying);
    expect(api.fetchOrgs).toHaveBeenCalledTimes(2);

    await act(async () => { retryOrgs.resolve([org("org-b", "member")]); });
    expect(await screen.findByLabelText("Organisation")).toBeInTheDocument();
    expect(screen.getByRole("option", { name: /org-b/ })).toBeInTheDocument();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(api.quotePayerTransfer).not.toHaveBeenCalled();
    expect(api.beginPayerTransfer).not.toHaveBeenCalled();
  });

  it("keeps the personal transfer section recoverable after a repeated organisation failure", async () => {
    vi.mocked(api.fetchOrgs)
      .mockRejectedValueOnce(new Error("Synthetic network error"))
      .mockRejectedValueOnce(new Error("Synthetic network error"));
    render(<PersonalTransferStart />);
    expect(await screen.findByRole("alert")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Retry" }));
    expect(await screen.findByRole("alert")).toHaveTextContent(/organisations could not be loaded/i);
    expect(screen.getByRole("button", { name: "Retry" })).toBeEnabled();
    expect(screen.queryByLabelText("Organisation")).not.toBeInTheDocument();
  });

  it("still hides personal transfer start when the server reports transfers unavailable", async () => {
    vi.mocked(api.fetchPayerTransferCapabilities).mockResolvedValue({ available: false, setupAvailable: false });
    const view = render(<PersonalTransferStart />);
    await waitFor(() => expect(api.fetchPayerTransferCapabilities).toHaveBeenCalled());
    expect(view.container).toBeEmptyDOMElement();
    expect(api.fetchOrgs).not.toHaveBeenCalled();
    expect(screen.queryByRole("button", { name: "Retry" })).not.toBeInTheDocument();
  });

  it("retries administrator loading for the selected organisation without starting a transfer", async () => {
    vi.mocked(api.fetchMembers).mockRejectedValueOnce(new Error("server error (503)"));
    render(<PersonalTransferStart />);

    const organisation = await screen.findByLabelText("Organisation");
    fireEvent.change(organisation, { target: { value: "org-b" } });
    expect(await screen.findByRole("alert")).toHaveTextContent("The transfer service is temporarily unavailable. Try again shortly.");
    expect(organisation).toHaveValue("org-b");
    expect(screen.getByLabelText("Other payer administrator")).toHaveValue("");
    expect(screen.getByRole("button", { name: "Review payer transfer" })).toBeDisabled();
    expect(api.fetchMembers).toHaveBeenCalledExactlyOnceWith("org-b");
    expect(api.quotePayerTransfer).not.toHaveBeenCalled();
    expect(api.beginPayerTransfer).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole("button", { name: "Retry loading administrators" }));

    expect(await screen.findByRole("option", { name: "admin-2 · owner" })).toBeInTheDocument();
    expect(api.fetchMembers).toHaveBeenCalledTimes(2);
    expect(api.fetchMembers).toHaveBeenLastCalledWith("org-b");
    expect(organisation).toHaveValue("org-b");
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Retry loading administrators" })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Review payer transfer" })).toBeDisabled();
    fireEvent.change(screen.getByLabelText("Other payer administrator"), { target: { value: "admin-2" } });
    expect(screen.getByRole("button", { name: "Review payer transfer" })).toBeEnabled();
    expect(api.quotePayerTransfer).not.toHaveBeenCalled();
    expect(api.beginPayerTransfer).not.toHaveBeenCalled();
  });

  it("keeps administrator retry single-flight and clears the old error while loading", async () => {
    const first = deferred<api.Member[]>();
    const retry = deferred<api.Member[]>();
    vi.mocked(api.fetchMembers)
      .mockReturnValueOnce(first.promise)
      .mockReturnValueOnce(retry.promise);
    render(<PersonalTransferStart />);
    const organisation = await screen.findByLabelText("Organisation");
    fireEvent.change(organisation, { target: { value: "org-b" } });
    expect(screen.getByRole("button", { name: "Loading administrators…" })).toBeDisabled();
    expect(organisation).toBeDisabled();
    expect(screen.getByLabelText("Other payer administrator")).toBeDisabled();
    expect(screen.getByRole("button", { name: "Review payer transfer" })).toBeDisabled();

    await act(async () => first.reject(new Error("server error (503)")));
    const retryButton = screen.getByRole("button", { name: "Retry loading administrators" });
    expect(retryButton).toBeEnabled();
    act(() => {
      fireEvent.click(retryButton);
      fireEvent.click(retryButton);
    });

    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    const loading = screen.getByRole("button", { name: "Loading administrators…" });
    expect(loading).toBeDisabled();
    expect(organisation).toBeDisabled();
    expect(organisation).toHaveValue("org-b");
    expect(screen.getByLabelText("Other payer administrator")).toBeDisabled();
    expect(screen.getByRole("button", { name: "Review payer transfer" })).toBeDisabled();
    fireEvent.click(loading);
    expect(api.fetchMembers).toHaveBeenCalledTimes(2);
    expect(api.fetchMembers).toHaveBeenLastCalledWith("org-b");

    await act(async () => retry.resolve([member("admin-2", "owner")]));
    expect(screen.getByRole("option", { name: "admin-2 · owner" })).toBeInTheDocument();
    expect(organisation).toBeEnabled();
    expect(screen.getByLabelText("Other payer administrator")).toBeEnabled();
    expect(screen.queryByRole("button", { name: "Loading administrators…" })).not.toBeInTheDocument();
    expect(api.quotePayerTransfer).not.toHaveBeenCalled();
    expect(api.beginPayerTransfer).not.toHaveBeenCalled();
  });

  it("restores retry after another administrator failure and can recover on the next attempt", async () => {
    const first = deferred<api.Member[]>();
    const retry = deferred<api.Member[]>();
    vi.mocked(api.fetchMembers)
      .mockReturnValueOnce(first.promise)
      .mockReturnValueOnce(retry.promise);
    render(<PersonalTransferStart />);
    const organisation = await screen.findByLabelText("Organisation");
    fireEvent.change(organisation, { target: { value: "org-b" } });
    await act(async () => first.reject(new Error("server error (503)")));
    fireEvent.click(screen.getByRole("button", { name: "Retry loading administrators" }));
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();

    await act(async () => retry.reject(new Error("server error (403)")));
    expect(screen.getByRole("alert")).toHaveTextContent("You do not have authority for this payer change.");
    expect(screen.getByRole("button", { name: "Retry loading administrators" })).toBeEnabled();
    expect(organisation).toHaveValue("org-b");
    expect(organisation).toBeEnabled();
    expect(screen.getByRole("button", { name: "Review payer transfer" })).toBeDisabled();

    fireEvent.click(screen.getByRole("button", { name: "Retry loading administrators" }));
    expect(await screen.findByRole("option", { name: "admin-2 · owner" })).toBeInTheDocument();
    expect(api.fetchMembers).toHaveBeenCalledTimes(3);
    expect(vi.mocked(api.fetchMembers).mock.calls).toEqual([["org-b"], ["org-b"], ["org-b"]]);
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Retry loading administrators" })).not.toBeInTheDocument();
    expect(api.quotePayerTransfer).not.toHaveBeenCalled();
    expect(api.beginPayerTransfer).not.toHaveBeenCalled();
  });

  it.each(["initial load", "retry"] as const)("treats an empty administrator %s as successful", async (phase) => {
    const request = deferred<api.Member[]>();
    if (phase === "retry") {
      vi.mocked(api.fetchMembers).mockRejectedValueOnce(new Error("server error (503)"));
    }
    vi.mocked(api.fetchMembers).mockReturnValueOnce(request.promise);
    render(<PersonalTransferStart />);
    const organisation = await screen.findByLabelText("Organisation");
    fireEvent.change(organisation, { target: { value: "org-b" } });
    if (phase === "retry") {
      fireEvent.click(await screen.findByRole("button", { name: "Retry loading administrators" }));
    }
    await act(async () => request.resolve([]));

    expect(organisation).toHaveValue("org-b");
    expect(organisation).toBeEnabled();
    expect(screen.getByLabelText("Other payer administrator")).toHaveDisplayValue("Choose an organisation owner or admin");
    expect(screen.queryByRole("option", { name: "admin-2 · owner" })).not.toBeInTheDocument();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Retry loading administrators" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Loading administrators…" })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Review payer transfer" })).toBeDisabled();
    expect(api.fetchMembers).toHaveBeenCalledTimes(phase === "retry" ? 2 : 1);
    expect(api.quotePayerTransfer).not.toHaveBeenCalled();
    expect(api.beginPayerTransfer).not.toHaveBeenCalled();
  });

  it.each(["org-a", ""])("clears failed administrator recovery when selecting %j", async (nextOrganisation) => {
    const next = deferred<api.Member[]>();
    vi.mocked(api.fetchMembers)
      .mockRejectedValueOnce(new Error("server error (503)"))
      .mockReturnValueOnce(next.promise);
    render(<PersonalTransferStart />);
    const organisation = await screen.findByLabelText("Organisation");
    fireEvent.change(organisation, { target: { value: "org-b" } });
    expect(await screen.findByRole("button", { name: "Retry loading administrators" })).toBeEnabled();

    fireEvent.change(organisation, { target: { value: nextOrganisation } });
    expect(organisation).toHaveValue(nextOrganisation);
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Retry loading administrators" })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Review payer transfer" })).toBeDisabled();
    if (nextOrganisation === "") {
      expect(api.fetchMembers).toHaveBeenCalledTimes(1);
      expect(screen.queryByLabelText("Other payer administrator")).not.toBeInTheDocument();
      expect(screen.queryByRole("button", { name: "Loading administrators…" })).not.toBeInTheDocument();
    } else {
      expect(api.fetchMembers).toHaveBeenCalledTimes(2);
      expect(api.fetchMembers).toHaveBeenLastCalledWith("org-a");
      expect(screen.getByRole("button", { name: "Loading administrators…" })).toBeDisabled();
      await act(async () => next.resolve([member("admin-a", "admin")]));
      expect(screen.getByRole("option", { name: "admin-a · admin" })).toBeInTheDocument();
      expect(screen.queryByRole("alert")).not.toBeInTheDocument();
      expect(organisation).toBeEnabled();
    }
    expect(api.quotePayerTransfer).not.toHaveBeenCalled();
    expect(api.beginPayerTransfer).not.toHaveBeenCalled();
  });

  it.each(["quote", "submission"] as const)("does not offer administrator retry after a %s error", async (failure) => {
    const operation = failure === "quote" ? api.quotePayerTransfer : api.beginPayerTransfer;
    vi.mocked(operation).mockRejectedValueOnce(new Error("server error (503)"));
    render(<PersonalTransferStart />);
    const organisation = await screen.findByLabelText("Organisation");
    fireEvent.change(organisation, { target: { value: "org-b" } });
    await screen.findByRole("option", { name: "admin-2 · owner" });
    fireEvent.change(screen.getByLabelText("Other payer administrator"), { target: { value: "admin-2" } });
    fireEvent.click(screen.getByRole("button", { name: "Review payer transfer" }));
    if (failure === "submission") {
      await screen.findByText(/At renewal on/);
      fireEvent.click(screen.getByRole("button", { name: "Send request for acceptance" }));
    }

    expect(await screen.findByRole("alert")).toHaveTextContent("The transfer service is temporarily unavailable. Try again shortly.");
    expect(screen.queryByRole("button", { name: "Retry loading administrators" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Loading administrators…" })).not.toBeInTheDocument();
    expect(organisation).toHaveValue("org-b");
    expect(screen.getByLabelText("Other payer administrator")).toHaveValue("admin-2");
    expect(api.fetchMembers).toHaveBeenCalledExactlyOnceWith("org-b");
    expect(screen.getByRole("button", { name: failure === "quote" ? "Review payer transfer" : "Send request for acceptance" })).toBeEnabled();
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