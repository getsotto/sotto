import { useEffect, useState } from "react";

import {
  acceptPayerTransfer,
  createPayerTransferPaymentSetup,
  fetchPayerTransfer,
  fetchPayerTransferCapabilities,
  fetchPayerTransfers,
  me,
  withdrawPayerTransfer,
  type PayerTransfer,
} from "./api";

function message(error: unknown): string {
  if (!(error instanceof Error)) return "The transfer could not be updated. Refresh and try again.";
  const status = /server error \((\d+)\)/.exec(error.message)?.[1];
  if (status === "403") return "Your authority for this transfer has changed. Ask the current payer to start a new request.";
  if (status === "404") return "This transfer is no longer available. Refresh the list for its latest state.";
  if (status === "409") return "The transfer terms have changed or the invitation has expired. Request a fresh transfer.";
  return "The transfer service is temporarily unavailable. Refresh and try again.";
}

function date(epoch: number): string {
  return new Date(epoch * 1000).toLocaleDateString("en-GB");
}

function pounds(pence: number): string {
  return new Intl.NumberFormat("en-GB", { style: "currency", currency: "GBP" }).format(pence / 100);
}

function payer(kind: string, organizationId: string | null): string {
  return kind === "personal" ? "personal billing" : `organisation ${organizationId ?? "billing"}`;
}

function stateCopy(state: string, expiredInvitation = false): string {
  switch (state) {
    case "awaiting_consent": return expiredInvitation
      ? "This invitation expired at the renewal boundary. Ask the initiator to start a new request."
      : "Waiting for the other payer to respond.";
    case "pending": return "Accepted. The billing change is being prepared; it is not complete yet.";
    case "destination_prepared": return "The change is being prepared for the renewal date.";
    case "destination_paid":
    case "source_adjustment_pending": return "Payment is recorded and the payer handover is being verified.";
    case "source_restoration_pending": return "The previous payer is being restored after a payment issue.";
    case "completed": return "Payer change completed after provider verification.";
    case "failed": return "This request ended without changing the payer.";
    case "expired": return "This invitation expired without changing the payer.";
    case "unknown": return "This transfer needs support review. It will not continue automatically.";
    default: return "Transfer status is being checked.";
  }
}

function withTransfer(transfers: PayerTransfer[], updated: PayerTransfer): PayerTransfer[] {
  const existing = transfers.some((transfer) => transfer.transferId === updated.transferId);
  const next = existing
    ? transfers.map((transfer) => transfer.transferId === updated.transferId ? updated : transfer)
    : [...transfers, updated];
  return next.sort((left, right) => right.transferId.localeCompare(left.transferId));
}

// setTimeout cannot wait longer than 2^31 - 1 ms; a longer wait re-arms when the timer fires.
const MAX_TIMER_DELAY_MS = 2_147_483_647;

export function PayerTransferInbox() {
  const [available, setAvailable] = useState(false);
  const [setupAvailable, setSetupAvailable] = useState(false);
  const [userId, setUserId] = useState<string | null>(null);
  const [transfers, setTransfers] = useState<PayerTransfer[]>([]);
  // The list has been received at least once, so an empty one really means no requests.
  const [listLoaded, setListLoaded] = useState(false);
  const [nextCursor, setNextCursor] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [loadingMore, setLoadingMore] = useState(false);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [busyAction, setBusyAction] = useState<"accept" | "withdraw" | "setup" | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [expiryTick, setExpiryTick] = useState(0);

  async function refresh() {
    setLoading(true);
    setError(null);
    try {
      const page = await fetchPayerTransfers();
      setTransfers(page.transfers);
      setNextCursor(page.nextCursor);
      setListLoaded(true);
    } catch (e) {
      setError(message(e));
    } finally {
      setLoading(false);
    }
  }

  async function loadMore() {
    if (nextCursor === null || loadingMore) return;
    setLoadingMore(true);
    setError(null);
    try {
      const page = await fetchPayerTransfers(nextCursor);
      setTransfers((current) => {
        const known = new Set(current.map((transfer) => transfer.transferId));
        return [...current, ...page.transfers.filter((transfer) => !known.has(transfer.transferId))];
      });
      setNextCursor(page.nextCursor);
    } catch (e) {
      setError(message(e));
    } finally {
      setLoadingMore(false);
    }
  }

  useEffect(() => {
    let current = true;
    void Promise.all([fetchPayerTransferCapabilities(), me()])
      .then(([capabilities, account]) => {
        if (!current) return;
        setAvailable(capabilities.available);
        setSetupAvailable(capabilities.setupAvailable);
        setUserId(account?.userId ?? null);
        if (capabilities.available) void refresh();
        else setLoading(false);
      })
      .catch(() => {
        // Older servers and temporary discovery failures keep the existing account page usable.
        if (current) setLoading(false);
      });
    return () => { current = false; };
  }, []);

  useEffect(() => {
    const onTransferUpdated = () => { if (available) void refresh(); };
    window.addEventListener("sotto:payer-transfer-updated", onTransferUpdated);
    return () => window.removeEventListener("sotto:payer-transfer-updated", onTransferUpdated);
  }, [available]);

  useEffect(() => {
    // Re-render when the nearest open invitation reaches its deadline, so expiry shows without a refresh.
    const nowMs = Date.now();
    const deadlines = transfers
      .filter((transfer) => transfer.state === "awaiting_consent")
      .map((transfer) => Math.min(transfer.consentExpiresAtEpoch, transfer.effectiveFrom) * 1000)
      .filter((deadlineMs) => deadlineMs > nowMs);
    if (deadlines.length === 0) return;
    const delay = Math.min(Math.min(...deadlines) - nowMs, MAX_TIMER_DELAY_MS);
    const timer = window.setTimeout(() => setExpiryTick((tick) => tick + 1), delay);
    return () => window.clearTimeout(timer);
  }, [transfers, expiryTick]);

  useEffect(() => {
    if (loading) return;
    const params = new URLSearchParams(window.location.search);
    const returnId = params.get("transfer_id");
    const outcome = params.get("billing");
    if (outcome !== "transfer_setup" && outcome !== "transfer_setup_cancelled") return;
    if (returnId !== null) {
      setNotice(outcome === "transfer_setup"
        ? "Payment method setup returned. Refreshing the transfer; provider confirmation is still required."
        : "Payment method setup was cancelled. The transfer has not completed.");
      if (available) {
        void fetchPayerTransfer(returnId)
          .then((transfer) => setTransfers((current) => withTransfer(current, transfer)))
          .catch(() => {});
      }
    }
    params.delete("billing");
    params.delete("transfer_id");
    const query = params.toString();
    window.history.replaceState(null, "", window.location.pathname + (query ? `?${query}` : "") + window.location.hash);
  }, [available, loading]);

  async function act(transfer: PayerTransfer, action: "accept" | "withdraw" | "setup") {
    setBusyId(transfer.transferId);
    setBusyAction(action);
    setError(null);
    setNotice(null);
    try {
      let updated: PayerTransfer;
      if (action === "accept") updated = await acceptPayerTransfer(transfer.transferId);
      else if (action === "withdraw") updated = await withdrawPayerTransfer(transfer.transferId);
      else {
        const setup = await createPayerTransferPaymentSetup(transfer.transferId);
        if (setup.checkoutUrl !== null) {
          window.location.assign(setup.checkoutUrl);
          return;
        }
        setNotice("Payment method setup is pending. Refresh the transfer shortly.");
        updated = await fetchPayerTransfer(transfer.transferId);
      }
      setTransfers((current) => withTransfer(current, updated));
    } catch (e) {
      setError(message(e));
      try {
        const updated = await fetchPayerTransfer(transfer.transferId);
        setTransfers((current) => withTransfer(current, updated));
      } catch {
        // Keep the last known state when the transfer itself cannot be refreshed.
      }
    } finally {
      setBusyId(null);
      setBusyAction(null);
    }
  }

  if (!available) return null;

  return <section aria-labelledby="transfer-inbox-heading">
    <h2 id="transfer-inbox-heading">Payer transfers</h2>
    <p className="muted">Transfers take effect at renewal. The current payer remains responsible through the paid term; no mid-term credit is issued.</p>
    {error !== null && <p role="alert">{error}</p>}
    {notice !== null && <p className="notice" role="status">{notice}</p>}
    <button disabled={loading || loadingMore || busyId !== null} onClick={() => void refresh()}>{loading ? "Refreshing transfers…" : "Refresh transfers"}</button>
    {loading && <p className="muted" role="status">Loading transfer requests…</p>}
    {!loading && listLoaded && transfers.length === 0 && <p className="muted">There are no payer transfer requests for this account.</p>}
    <ul className="items">
      {transfers.map((transfer) => {
        const incoming = transfer.participantRole === "counterparty";
        const invitationExpired = transfer.state === "awaiting_consent"
          && Math.floor(Date.now() / 1000) >= Math.min(transfer.consentExpiresAtEpoch, transfer.effectiveFrom);
        const actionable = transfer.state === "awaiting_consent" && !invitationExpired;
        const setupRequired = transfer.state === "pending"
          && transfer.destinationKind === "personal"
          && transfer.beneficiaryId === userId
          && transfer.personalPaymentSetupNeeded
          && setupAvailable;
        return <li key={transfer.transferId}>
          <strong>{incoming ? "Incoming request" : "Outgoing request"}</strong>
          <span className="meta">{payer(transfer.sourceKind, transfer.sourceOrganizationId)} → {payer(transfer.destinationKind, transfer.destinationOrganizationId)}</span>
          <p>{pounds(transfer.amountPence)} per {transfer.interval}; renewal {date(transfer.effectiveFrom)}.</p>
          {actionable && <p className="muted">Respond by {date(Math.min(transfer.consentExpiresAtEpoch, transfer.effectiveFrom))}.</p>}
          <p className="muted">{stateCopy(transfer.state, invitationExpired)}</p>
          {transfer.state === "unknown" && <p className="muted">Contact Sotto support before making another billing change.</p>}
          {actionable && incoming && transfer.destinationKind === "personal" && !setupAvailable
            && <p className="muted">Personal payment setup is not available on this server yet. The request has not been accepted.</p>}
          {actionable && incoming && <>
            {(transfer.destinationKind !== "personal" || setupAvailable) && <button disabled={busyId !== null} onClick={() => void act(transfer, "accept")}>{busyId === transfer.transferId && busyAction === "accept" ? "Accepting…" : "Accept transfer request"}</button>}{" "}
            <button disabled={busyId !== null} onClick={() => void act(transfer, "withdraw")}>{busyId === transfer.transferId && busyAction === "withdraw" ? "Declining…" : "Decline request"}</button>
          </>}
          {actionable && !incoming && <button disabled={busyId !== null} onClick={() => void act(transfer, "withdraw")}>Withdraw request</button>}
          {setupRequired && <button disabled={busyId !== null} onClick={() => void act(transfer, "setup")}>Set up payment method</button>}
        </li>;
      })}
    </ul>
    {nextCursor !== null && <button disabled={loading || loadingMore || busyId !== null} onClick={() => void loadMore()}>{loadingMore ? "Loading older transfers…" : "Load older transfers"}</button>}
  </section>;
}
