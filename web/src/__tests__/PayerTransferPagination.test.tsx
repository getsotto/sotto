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
    transferId: "transfer-2",
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

type Page = Awaited<ReturnType<typeof api.fetchPayerTransfers>>;

const NOW_MS = 1_900_000_000_000;
const NO_REQUESTS = "There are no payer transfer requests for this account.";

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

it("keeps loaded requests and the cursor when an older page fails, then recovers on an explicit retry", async () => {
  const newest = transfer();
  const older = transfer({ transferId: "transfer-1", participantRole: "initiator", state: "completed" });
  const firstAttempt = deferred<Page>();
  const retry = deferred<Page>();
  vi.mocked(api.fetchPayerTransfers)
    .mockResolvedValueOnce({ transfers: [newest], nextCursor: "cursor-1" })
    .mockReturnValueOnce(firstAttempt.promise)
    .mockReturnValueOnce(retry.promise);

  render(<PayerTransferInbox />);
  expect(await screen.findByText("Incoming request")).toBeInTheDocument();

  fireEvent.click(screen.getByRole("button", { name: "Load older transfers" }));
  expect(api.fetchPayerTransfers).toHaveBeenLastCalledWith("cursor-1");
  const pending = screen.getByRole("button", { name: "Loading older transfers…" });
  expect(pending).toBeDisabled();
  fireEvent.click(pending);
  expect(api.fetchPayerTransfers).toHaveBeenCalledTimes(2);
  expect(screen.getByText("Incoming request")).toBeInTheDocument();

  firstAttempt.reject(new Error("server error (503)"));
  expect(await screen.findByRole("alert")).toHaveTextContent("The transfer service is temporarily unavailable");
  expect(screen.getByText("Incoming request")).toBeInTheDocument();
  expect(screen.queryByText(NO_REQUESTS)).not.toBeInTheDocument();
  const loadMore = screen.getByRole("button", { name: "Load older transfers" });
  expect(loadMore).toBeEnabled();
  // A failure is reported, not retried behind the user's back.
  expect(api.fetchPayerTransfers).toHaveBeenCalledTimes(2);

  fireEvent.click(loadMore);
  expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Loading older transfers…" })).toBeDisabled();
  expect(api.fetchPayerTransfers).toHaveBeenCalledTimes(3);
  expect(api.fetchPayerTransfers).toHaveBeenLastCalledWith("cursor-1");

  // The server repeats the request already on screen next to the older one.
  retry.resolve({ transfers: [newest, older], nextCursor: null });
  expect(await screen.findByText("Outgoing request")).toBeInTheDocument();
  expect(screen.getAllByText("Incoming request")).toHaveLength(1);
  expect(screen.getAllByRole("listitem")).toHaveLength(2);
  await waitFor(() => expect(screen.queryByRole("button", { name: /older transfers/ })).not.toBeInTheDocument());
  expect(screen.queryByRole("alert")).not.toBeInTheDocument();

  expect(api.acceptPayerTransfer).not.toHaveBeenCalled();
  expect(api.withdrawPayerTransfer).not.toHaveBeenCalled();
  expect(api.createPayerTransferPaymentSetup).not.toHaveBeenCalled();
});
