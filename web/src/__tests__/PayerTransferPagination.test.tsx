import "@testing-library/jest-dom/vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
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

it("does not overwrite a newer list or cursor when an older refresh succeeds later", async () => {
  const initial = deferred<Page>();
  const older = deferred<Page>();
  const newer = deferred<Page>();
  vi.mocked(api.fetchPayerTransfers)
    .mockReturnValueOnce(initial.promise)
    .mockReturnValueOnce(older.promise)
    .mockReturnValueOnce(newer.promise);

  render(<PayerTransferInbox />);
  await act(async () => {
    initial.resolve({ transfers: [], nextCursor: null });
  });
  expect(await screen.findByText(NO_REQUESTS)).toBeInTheDocument();

  // Dispatch two updates (e.g. from rapid transfers or initiation forms)
  window.dispatchEvent(new CustomEvent("sotto:payer-transfer-updated"));
  window.dispatchEvent(new CustomEvent("sotto:payer-transfer-updated"));

  expect(api.fetchPayerTransfers).toHaveBeenCalledTimes(3);

  // The newer request completes first with an incoming request
  const incoming = transfer({ transferId: "transfer-newer", participantRole: "counterparty" });
  await act(async () => {
    newer.resolve({ transfers: [incoming], nextCursor: "cursor-newer" });
  });

  expect(await screen.findByText("Incoming request")).toBeInTheDocument();
  expect(screen.queryByText(NO_REQUESTS)).not.toBeInTheDocument();

  // The older request completes later with an empty list and older cursor
  await act(async () => {
    older.resolve({ transfers: [], nextCursor: "cursor-older" });
  });

  // Verify that the newer transfer and state remain intact
  expect(screen.getByText("Incoming request")).toBeInTheDocument();
  expect(screen.queryByText(NO_REQUESTS)).not.toBeInTheDocument();
  expect(screen.queryByRole("alert")).not.toBeInTheDocument();
});

it("does not report an error or overwrite a newer list when an older refresh fails later", async () => {
  const initial = deferred<Page>();
  const older = deferred<Page>();
  const newer = deferred<Page>();
  vi.mocked(api.fetchPayerTransfers)
    .mockReturnValueOnce(initial.promise)
    .mockReturnValueOnce(older.promise)
    .mockReturnValueOnce(newer.promise);

  render(<PayerTransferInbox />);
  await act(async () => {
    initial.resolve({ transfers: [], nextCursor: null });
  });
  expect(await screen.findByText(NO_REQUESTS)).toBeInTheDocument();

  window.dispatchEvent(new CustomEvent("sotto:payer-transfer-updated"));
  window.dispatchEvent(new CustomEvent("sotto:payer-transfer-updated"));

  expect(api.fetchPayerTransfers).toHaveBeenCalledTimes(3);

  const incoming = transfer({ transferId: "transfer-newer", participantRole: "counterparty" });
  await act(async () => {
    newer.resolve({ transfers: [incoming], nextCursor: null });
  });

  expect(await screen.findByText("Incoming request")).toBeInTheDocument();

  // The older request fails later
  await act(async () => {
    older.reject(new Error("server error (503)"));
  });

  // Verify no error is displayed and newer data remains visible
  expect(screen.getByText("Incoming request")).toBeInTheDocument();
  expect(screen.queryByRole("alert")).not.toBeInTheDocument();
});

it("does not clear the loading state when an older refresh completes while a newer refresh is pending", async () => {
  const initial = deferred<Page>();
  const older = deferred<Page>();
  const newer = deferred<Page>();
  vi.mocked(api.fetchPayerTransfers)
    .mockReturnValueOnce(initial.promise)
    .mockReturnValueOnce(older.promise)
    .mockReturnValueOnce(newer.promise);

  render(<PayerTransferInbox />);
  await act(async () => {
    initial.resolve({ transfers: [], nextCursor: null });
  });
  expect(await screen.findByText(NO_REQUESTS)).toBeInTheDocument();

  window.dispatchEvent(new CustomEvent("sotto:payer-transfer-updated"));
  window.dispatchEvent(new CustomEvent("sotto:payer-transfer-updated"));

  expect(api.fetchPayerTransfers).toHaveBeenCalledTimes(3);

  // Older completes while newer is still in flight
  await act(async () => {
    older.resolve({ transfers: [], nextCursor: null });
  });

  // Loading state remains active for the newer in-flight request
  expect(screen.getByText("Loading transfer requests…")).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Refreshing transfers…" })).toBeDisabled();

  // Newer finally completes
  const incoming = transfer({ transferId: "transfer-newer" });
  await act(async () => {
    newer.resolve({ transfers: [incoming], nextCursor: null });
  });

  expect(await screen.findByText("Incoming request")).toBeInTheDocument();
  expect(screen.queryByText("Loading transfer requests…")).not.toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Refresh transfers" })).toBeEnabled();
});
