import { useEffect, useState } from "react";

import {
  cancelPersonalBilling,
  createPersonalCheckout,
  createPersonalPortal,
  fetchCloudExportChunk,
  fetchCloudNotices,
  fetchEligibility,
  fetchPersonalOperation,
  fetchPersonalQuote,
  requestPersonalRefund,
  startCloudExport,
  type EligibilityView,
  type CloudNotice,
  type PersonalLifecycle,
  type PersonalOperation,
  type PersonalQuote,
  type RefundRequest,
} from "./api";

function message(error: unknown): string { return error instanceof Error ? error.message : String(error); }
function pounds(pence: number): string { return new Intl.NumberFormat("en-GB", { style: "currency", currency: "GBP" }).format(pence / 100); }
function date(epoch: number | null): string | null { return epoch === null ? null : new Date(epoch * 1000).toLocaleDateString("en-GB"); }

export function CloudAccountPanel() {
  const [eligibility, setEligibility] = useState<EligibilityView | null>(null);
  const [offer, setOffer] = useState<PersonalQuote["offer"]>("monthly");
  const [quote, setQuote] = useState<PersonalQuote | null>(null);
  const [quoteError, setQuoteError] = useState<string | null>(null);
  const [quoteLoading, setQuoteLoading] = useState(false);
  const [quoteAttempt, setQuoteAttempt] = useState(0);
  const [lifecycle, setLifecycle] = useState<PersonalLifecycle | null>(null);
  const [refund, setRefund] = useState<RefundRequest | null>(null);
  const [operation, setOperation] = useState<PersonalOperation | null>(null);
  const [notices, setNotices] = useState<CloudNotice[]>([]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  async function load() {
    setError(null);
    try {
      const next = await fetchEligibility();
      setEligibility(next);
    } catch (e) { setError(message(e)); }
  }
  useEffect(() => { void load(); }, []);
  useEffect(() => {
    if (eligibility === null) return;
    void fetchCloudNotices()
      .then(setNotices)
      .catch(() => {
        // Eligibility and vault recovery must remain usable when the notice endpoint is briefly
        // unavailable. The next account visit will fetch the current durable list again.
      });
  }, [eligibility]);
  useEffect(() => {
    if (eligibility?.actions.billing !== true) return;
    let current = true;
    setQuote(null);
    setQuoteError(null);
    setQuoteLoading(true);
    void fetchPersonalQuote(offer)
      .then((next) => { if (current) setQuote(next); })
      .catch((e) => { if (current) setQuoteError(message(e)); })
      .finally(() => { if (current) setQuoteLoading(false); });
    return () => { current = false; };
  }, [offer, eligibility?.actions.billing, quoteAttempt]);

  async function refreshOperation() {
    const operationId = sessionStorage.getItem("sotto_personal_operation_id");
    if (operationId === null) return;
    try {
      const next = await fetchPersonalOperation(operationId);
      setOperation(next);
      if (next.state !== "pending") sessionStorage.removeItem("sotto_personal_operation_id");
    } catch {
      // A stale browser session must not turn into a billing error. Eligibility remains the
      // source of truth when the operation has already been reconciled or expired.
      sessionStorage.removeItem("sotto_personal_operation_id");
    }
  }

  useEffect(() => {
    if (eligibility?.state === "pending_initial_payment") void refreshOperation();
  }, [eligibility?.state]);

  async function checkout() {
    if (quote === null || quoteLoading || quote.offer !== offer) return;
    setBusy(true); setError(null); setNotice(null);
    try {
      // The server accepts only its configured web origin as the return-url identity. Stripe's
      // actual success/cancel paths are derived server-side, so query strings must not be added
      // here.
      const result = await createPersonalCheckout({ offer: quote.offer, idempotencyKey: crypto.randomUUID(), quoteVersion: quote.quoteVersion, quoteExpiresAtEpoch: quote.quoteExpiresAtEpoch, returnUrl: window.location.origin });
      sessionStorage.setItem("sotto_personal_operation_id", result.operationId);
      setOperation({ operationId: result.operationId, offer: quote.offer, state: result.state, checkoutUrl: result.checkoutUrl });
      if (result.checkoutUrl !== null) window.location.assign(result.checkoutUrl);
      else setNotice("Checkout is pending provider confirmation. Reload this page shortly.");
    } catch (e) { setError(message(e)); }
    finally { setBusy(false); }
  }
  async function portal() {
    setBusy(true); setError(null);
    try { const next = await createPersonalPortal(); if (next.portalUrl !== null) window.location.assign(next.portalUrl); else setNotice("The billing portal is not available yet."); }
    catch (e) { setError(message(e)); }
    finally { setBusy(false); }
  }
  async function cancel() {
    setBusy(true); setError(null);
    try { setLifecycle(await cancelPersonalBilling(crypto.randomUUID())); setNotice("Cancellation requested. Your paid period remains available until it ends."); }
    catch (e) { setError(message(e)); }
    finally { setBusy(false); }
  }
  async function askRefund() {
    setBusy(true); setError(null);
    try { setRefund(await requestPersonalRefund({ reason: "billing_error", fullRefund: true, idempotencyKey: crypto.randomUUID() })); setNotice("Refund request submitted for review."); }
    catch (e) { setError(message(e)); }
    finally { setBusy(false); }
  }
  async function exportAccount() {
    setBusy(true); setError(null); setNotice(null);
    try {
      const manifest = await startCloudExport();
      const chunks: unknown[] = [];
      for (let index = 0; index < manifest.totalChunks; index += 1) chunks.push(await fetchCloudExportChunk(manifest.exportId, index));
      const blob = new Blob([JSON.stringify({ manifest, chunks })], { type: "application/json" });
      const url = URL.createObjectURL(blob); const anchor = document.createElement("a"); anchor.href = url; anchor.download = `sotto-cloud-export-${manifest.exportId}.json`; anchor.click(); URL.revokeObjectURL(url);
      setNotice("Encrypted export downloaded. Keep it with your recovery materials.");
    } catch (e) { setError(message(e)); }
    finally { setBusy(false); }
  }

  if (eligibility === null) return <><h1>Cloud account</h1>{error !== null ? <p role="alert">{error}</p> : <p className="muted">Loading account status…</p>}</>;
  const paidThrough = date(eligibility.paidThroughEpoch); const recoveryUntil = date(eligibility.recoveryUntilEpoch); const exportUntil = date(eligibility.exportUntilEpoch);
  return <>
    <h1>Cloud account</h1>
    <p className="muted">Billing and recovery controls are available here without unlocking your vault. Sotto Cloud stores only encrypted vault material.</p>
    {error !== null && <p role="alert">{error}</p>}{notice !== null && <p className="notice" role="status">{notice}</p>}
    {notices.length > 0 && <section aria-labelledby="notices-heading"><h2 id="notices-heading">Account notices</h2><ul>{notices.map((item) => <li key={item.noticeId}><strong>{item.content.title}</strong><p>{item.content.detail}</p>{item.content.deadlineEpoch !== null && <p className="muted">Deadline: {date(item.content.deadlineEpoch)}</p>}{item.status === "failed" && <p role="alert">This notice could not be delivered{item.lastErrorCode === null ? "." : ` (${item.lastErrorCode}).`}</p>}</li>)}</ul></section>}
    <section aria-labelledby="status-heading"><h2 id="status-heading">Hosted access</h2><p><strong>{eligibility.state.replaceAll("_", " ")}</strong>{paidThrough !== null ? ` · paid through ${paidThrough}` : ""}</p>{recoveryUntil !== null && <p className="muted">Recovery is available until {recoveryUntil}.</p>}{eligibility.payer === null && eligibility.state === "paid" && <p className="muted">Your hosted access is sponsored or provided by another billing record. There is no personal upgrade to buy.</p>}{eligibility.state === "unavailable" && <p>Billing evidence is temporarily unavailable. Checkout is hidden until the account can be checked safely.</p>}</section>
    {eligibility.actions.billing && <section aria-labelledby="billing-heading">
      <h2 id="billing-heading">Choose hosted billing</h2>
      <label>Term <select value={offer} disabled={busy} onChange={(e) => setOffer(e.target.value as PersonalQuote["offer"])}><option value="monthly">Monthly</option><option value="annual">Annual</option></select></label>
      {quoteLoading && <p className="muted" role="status">Loading billing quote…</p>}
      {quoteError !== null && <><p role="alert">{quoteError}</p><button disabled={busy || quoteLoading} onClick={() => setQuoteAttempt((attempt) => attempt + 1)}>Retry billing quote</button></>}
      {quote !== null && quote.offer === offer && <>
        <p>{pounds(quote.amountPence)} per {quote.interval}. Tax is shown at checkout.</p>
        {quote.founding && <p className="muted">Founding price: {quote.foundingRemainingPlaces ?? 0} places remain; {quote.foundingTerm}. Renews at {pounds(quote.nextRenewalAmountPence)}.</p>}
        <button className="primary" disabled={busy || quoteLoading} onClick={() => void checkout()}>{busy ? "Opening checkout…" : "Continue to secure checkout"}</button>
      </>}
    </section>}
    {eligibility.state === "pending_initial_payment" && <section aria-labelledby="pending-heading"><h2 id="pending-heading">Payment confirmation pending</h2><p>Your checkout is waiting for the verified payment webhook. Hosted access stays unchanged until it arrives.</p>{operation?.checkoutUrl !== null && operation?.checkoutUrl !== undefined && <p><a href={operation.checkoutUrl}>Return to checkout</a></p>}<button disabled={busy} onClick={() => void refreshOperation()}>Refresh payment status</button></section>}
    {(eligibility.state === "paid" || eligibility.state === "renewal_recovery") && <section aria-labelledby="manage-heading"><h2 id="manage-heading">Manage billing</h2><button disabled={busy} onClick={() => void portal()}>Open billing portal</button>{" "}{eligibility.actions.revoke && <button disabled={busy} onClick={() => void cancel()}>Cancel at the end of the paid period</button>}<button disabled={busy} onClick={() => void askRefund()}>Request a refund</button>{lifecycle?.cancelAtPeriodEnd && <p className="muted">Cancellation is scheduled; access remains available until {lifecycle.paidThroughDate ?? "the paid period ends"}.</p>}{refund !== null && <p className="muted">Refund request: {refund.state}. The paid term is preserved by default.</p>}</section>}
    {eligibility.actions.export && <section aria-labelledby="export-heading"><h2 id="export-heading">Recover your encrypted account</h2><p>{exportUntil === null ? "Your export window is open." : `Export before ${exportUntil}.`}</p>{eligibility.accountInitialized ? <button disabled={busy} onClick={() => void exportAccount()}>Download encrypted export</button> : <p>Set up your account before exporting.</p>}</section>}
    <p><a href="/app">Back to vault</a></p>
  </>;
}
