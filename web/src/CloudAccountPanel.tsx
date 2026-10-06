import { useEffect, useState } from "react";

import {
  cancelPersonalBilling,
  createPersonalCheckout,
  createPersonalPortal,
  fetchCloudExportChunk,
  fetchEligibility,
  fetchPersonalQuote,
  requestPersonalRefund,
  startCloudExport,
  type EligibilityView,
  type PersonalLifecycle,
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
  const [lifecycle, setLifecycle] = useState<PersonalLifecycle | null>(null);
  const [refund, setRefund] = useState<RefundRequest | null>(null);
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
    if (eligibility?.actions.billing !== true) return;
    void fetchPersonalQuote(offer).then(setQuote).catch((e) => setError(message(e)));
  }, [offer, eligibility?.actions.billing]);

  async function checkout() {
    if (quote === null) return;
    setBusy(true); setError(null); setNotice(null);
    try {
      // The server accepts only its configured web origin as the return-url identity. Stripe's
      // actual success/cancel paths are derived server-side, so query strings must not be added
      // here.
      const result = await createPersonalCheckout({ offer: quote.offer, idempotencyKey: crypto.randomUUID(), quoteVersion: quote.quoteVersion, quoteExpiresAtEpoch: quote.quoteExpiresAtEpoch, returnUrl: window.location.origin });
      if (result.checkoutUrl !== null) window.location.assign(result.checkoutUrl);
      else setNotice("Checkout is pending provider confirmation. Reload this page shortly.");
    } catch (e) { setError(message(e)); setBusy(false); }
  }
  async function portal() {
    setBusy(true); setError(null);
    try { const next = await createPersonalPortal(); if (next.portalUrl !== null) window.location.assign(next.portalUrl); else setNotice("The billing portal is not available yet."); }
    catch (e) { setError(message(e)); setBusy(false); }
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
    <section aria-labelledby="status-heading"><h2 id="status-heading">Hosted access</h2><p><strong>{eligibility.state.replaceAll("_", " ")}</strong>{paidThrough !== null ? ` · paid through ${paidThrough}` : ""}</p>{recoveryUntil !== null && <p className="muted">Recovery is available until {recoveryUntil}.</p>}{eligibility.payer === null && eligibility.state === "paid" && <p className="muted">Your hosted access is sponsored or provided by another billing record. There is no personal upgrade to buy.</p>}{eligibility.state === "unavailable" && <p>Billing evidence is temporarily unavailable. Checkout is hidden until the account can be checked safely.</p>}</section>
    {eligibility.actions.billing && quote !== null && <section aria-labelledby="billing-heading"><h2 id="billing-heading">Choose hosted billing</h2><label>Term <select value={offer} onChange={(e) => setOffer(e.target.value as PersonalQuote["offer"])}><option value="monthly">Monthly</option><option value="annual">Annual</option></select></label><p>{pounds(quote.amountPence)} per {quote.interval}. Tax is shown at checkout.</p>{quote.founding && <p className="muted">Founding price: {quote.foundingRemainingPlaces ?? 0} places remain; {quote.foundingTerm}. Renews at {pounds(quote.nextRenewalAmountPence)}.</p>}<button className="primary" disabled={busy} onClick={() => void checkout()}>{busy ? "Opening checkout…" : "Continue to secure checkout"}</button></section>}
    {(eligibility.state === "paid" || eligibility.state === "renewal_recovery") && <section aria-labelledby="manage-heading"><h2 id="manage-heading">Manage billing</h2><button disabled={busy} onClick={() => void portal()}>Open billing portal</button>{" "}{eligibility.actions.revoke && <button disabled={busy} onClick={() => void cancel()}>Cancel at the end of the paid period</button>}<button disabled={busy} onClick={() => void askRefund()}>Request a refund</button>{lifecycle?.cancelAtPeriodEnd && <p className="muted">Cancellation is scheduled; access remains available until {lifecycle.paidThroughDate ?? "the paid period ends"}.</p>}{refund !== null && <p className="muted">Refund request: {refund.state}. The paid term is preserved by default.</p>}</section>}
    {eligibility.actions.export && <section aria-labelledby="export-heading"><h2 id="export-heading">Recover your encrypted account</h2><p>{exportUntil === null ? "Your export window is open." : `Export before ${exportUntil}.`}</p>{eligibility.accountInitialized ? <button disabled={busy} onClick={() => void exportAccount()}>Download encrypted export</button> : <p>Set up your account before exporting.</p>}</section>}
    <p><a href="/app">Back to vault</a></p>
  </>;
}
