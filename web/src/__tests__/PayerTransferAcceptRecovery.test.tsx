import "@testing-library/jest-dom/vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";

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

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}


const NOW_MS = 1_900_000_000_000;

beforeEach(() => {
  vi.resetAllMocks();
  vi.spyOn(Date, "now").mockReturnValue(NOW_MS);
  vi.mocked(api.fetchPayerTransferCapabilities).mockResolvedValue({ available: true, setupAvailable: true });
  vi.mocked(api.me).mockResolvedValue({ userId: "user-1" });
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

const UNAVAILABLE = "The transfer service is temporarily unavailable";

async function startAcceptance() {
  const acceptance = deferred<api.PayerTransfer>();
  const recovery = deferred<api.PayerTransfer>();
  vi.mocked(api.fetchPayerTransfers).mockResolvedValue({ transfers: [transfer()], nextCursor: null });
  vi.mocked(api.acceptPayerTransfer).mockReturnValueOnce(acceptance.promise);
  vi.mocked(api.fetchPayerTransfer).mockReturnValueOnce(recovery.promise);

  render(<PayerTransferInbox />);
  fireEvent.click(await screen.findByRole("button", { name: "Accept transfer request" }));
  expect(api.acceptPayerTransfer).toHaveBeenCalledWith("transfer-1");

  const accepting = screen.getByRole("button", { name: "Accepting…" });
  const decline = screen.getByRole("button", { name: "Decline request" });
  expect(accepting).toBeDisabled();
  expect(decline).toBeDisabled();
  fireEvent.click(accepting);
  fireEvent.click(decline);
  expect(api.acceptPayerTransfer).toHaveBeenCalledTimes(1);

  acceptance.reject(new Error("server error (503)"));
  expect(await screen.findByRole("alert")).toHaveTextContent(UNAVAILABLE);
  expect(api.fetchPayerTransfer).toHaveBeenCalledWith("transfer-1");
  // Controls stay locked until the recovery fetch says what the invitation looks like now.
  expect(screen.getByRole("button", { name: "Accepting…" })).toBeDisabled();
  expect(screen.getByRole("button", { name: "Decline request" })).toBeDisabled();
  expect(api.acceptPayerTransfer).toHaveBeenCalledTimes(1);
  return recovery;
}

function expectNoOtherActions() {
  expect(api.withdrawPayerTransfer).not.toHaveBeenCalled();
  expect(api.createPayerTransferPaymentSetup).not.toHaveBeenCalled();
}

it("unlocks a still-open invitation after a failed acceptance and accepts it on an explicit retry", async () => {
  const recovery = await startAcceptance();
  vi.mocked(api.acceptPayerTransfer).mockResolvedValueOnce(transfer({ state: "pending" }));

  recovery.resolve(transfer());
  const accept = await screen.findByRole("button", { name: "Accept transfer request" });
  expect(accept).toBeEnabled();
  expect(screen.getByRole("button", { name: "Decline request" })).toBeEnabled();
  expect(api.acceptPayerTransfer).toHaveBeenCalledTimes(1);

  fireEvent.click(accept);
  expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  expect(api.acceptPayerTransfer).toHaveBeenCalledTimes(2);
  expect(api.acceptPayerTransfer).toHaveBeenLastCalledWith("transfer-1");
  expect(await screen.findByText(/Accepted\. The billing change is being prepared; it is not complete yet\./)).toBeInTheDocument();
  expect(screen.queryByText(/Payer change completed/)).not.toBeInTheDocument();
  expectNoOtherActions();
});

it("removes the actions when the recovery fetch shows the invitation expired", async () => {
  const recovery = await startAcceptance();

  recovery.resolve(transfer({ state: "expired" }));
  expect(await screen.findByText("This invitation expired without changing the payer.")).toBeInTheDocument();
  expect(screen.queryByRole("button", { name: /Accept|Decline/ })).not.toBeInTheDocument();
  expect(screen.getByRole("alert")).toHaveTextContent(UNAVAILABLE);
  expect(api.acceptPayerTransfer).toHaveBeenCalledTimes(1);
  expectNoOtherActions();
});

it("keeps the last known invitation and the original error when the recovery fetch fails too", async () => {
  const recovery = await startAcceptance();

  recovery.reject(new Error("server error (404)"));
  const accept = await screen.findByRole("button", { name: "Accept transfer request" });
  await waitFor(() => expect(accept).toBeEnabled());
  expect(screen.getByRole("button", { name: "Decline request" })).toBeEnabled();
  expect(screen.getByText("Waiting for the other payer to respond.")).toBeInTheDocument();
  expect(screen.getAllByRole("listitem")).toHaveLength(1);
  expect(screen.getByRole("alert")).toHaveTextContent(UNAVAILABLE);
  expect(api.acceptPayerTransfer).toHaveBeenCalledTimes(1);
  expectNoOtherActions();
});
