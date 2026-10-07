import { useEffect, useRef, useState } from "react";

import {
  createCheckout,
  createPortal,
  fetchAudit,
  fetchEntitlements,
  fetchMembers,
  fetchOrgs,
  fetchSponsoredSeats,
  fetchSponsoredQuote,
  createSponsoredCheckout,
  grantOrgKey,
  inviteMember,
  type AuditEvent,
  type Entitlements,
  type Member,
  type Org,
} from "./api";
import { decryptOrgName, openOrgKey, sealGrantTo } from "./vault";
import { OrganisationDeletionPanel } from "./OrganisationDeletionPanel";

interface NamedOrg {
  org: Org;
  name: string;
}

function message(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/// The `?billing=success|cancelled` parameter Stripe Checkout returns with. Pure - StrictMode
/// double-invokes state initialisers in development, so consuming the parameter here would eat
/// it before the surviving render; the URL cleanup lives in an effect instead.
function parseBillingOutcome(): "success" | "cancelled" | null {
  const outcome = new URLSearchParams(window.location.search).get("billing");
  return outcome === "success" || outcome === "cancelled" ? outcome : null;
}

/// Remove the consumed `billing` parameter so a reload doesn't repeat the banner, preserving any
/// other query parameters and the fragment. Idempotent - safe under StrictMode's double effects.
function clearBillingParam() {
  const params = new URLSearchParams(window.location.search);
  if (!params.has("billing")) {
    return;
  }
  params.delete("billing");
  const query = params.toString();
  window.history.replaceState(
    null,
    "",
    window.location.pathname + (query !== "" ? `?${query}` : "") + window.location.hash,
  );
}

/// Best-effort org-name decryption via the caller's sealed org-key copy; the org id otherwise.
function orgDisplayName(master: Uint8Array, encPrivateKeys: Uint8Array, org: Org): string {
  if (org.encOrgKey === null) {
    return org.id; // not granted the org key (yet): show the id
  }
  try {
    const key = openOrgKey(master, encPrivateKeys, org.encOrgKey);
    return decryptOrgName(key, org.id, org.encName);
  } catch {
    return org.id; // a pre-org-key name, or a copy sealed to keys we no longer hold
  }
}

/// The team section: the caller's organisations, each expandable to its member list, with
/// invite-by-email for admins/owners. Org names decrypt through the org key every member holds;
/// an ungranted member sees the org id. Inviting also seals the org key to the invitee.
export function TeamPanel({
  master,
  encPrivateKeys,
}: {
  master: Uint8Array;
  encPrivateKeys: Uint8Array;
}) {
  const [orgs, setOrgs] = useState<NamedOrg[] | null>(null);
  const [orgsLoading, setOrgsLoading] = useState(true);
  const [orgsError, setOrgsError] = useState<string | null>(null);
  const [openOrg, setOpenOrg] = useState<NamedOrg | null>(null);
  const [members, setMembers] = useState<Member[] | null>(null);
  const [membersLoading, setMembersLoading] = useState(false);
  const [audit, setAudit] = useState<AuditEvent[] | null>(null);
  const [plan, setPlan] = useState<Entitlements | null>(null);
  const [email, setEmail] = useState("");
  const [notice, setNotice] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [billingBusy, setBillingBusy] = useState(false);
  const [inviteBusy, setInviteBusy] = useState(false);
  const [billingOutcome] = useState(parseBillingOutcome);
  const [deletionActive, setDeletionActive] = useState(false);
  const [sponsoredSeats, setSponsoredSeats] = useState<import("./api").SponsoredSeat[]>([]);
  const [sponsoredAvailable, setSponsoredAvailable] = useState<boolean | null>(null);
  const [sponsoredBeneficiary, setSponsoredBeneficiary] = useState("");
  const [sponsoredAction, setSponsoredAction] = useState<"add" | "remove" | "replace">("add");
  const [sponsoredReplacement, setSponsoredReplacement] = useState("");
  const [sponsoredUntil, setSponsoredUntil] = useState("");
  const [sponsoredBusy, setSponsoredBusy] = useState(false);
  const orgLoadGeneration = useRef(0);
  const billingGeneration = useRef(0);
  const inviteInFlight = useRef(false);

  useEffect(() => {
    if (billingOutcome !== null) {
      clearBillingParam();
    }
  }, [billingOutcome]);

  async function loadOrgs() {
    setOrgsLoading(true);
    setOrgsError(null);
    try {
      const rows = await fetchOrgs();
      setOrgs(rows.map((org) => ({ org, name: orgDisplayName(master, encPrivateKeys, org) })));
    } catch (e) {
      setOrgsError(message(e));
    } finally {
      setOrgsLoading(false);
    }
  }

  useEffect(() => {
    void loadOrgs();
  }, [master, encPrivateKeys]);

  async function selectOrg(no: NamedOrg) {
    const generation = ++orgLoadGeneration.current;
    const isCurrent = () => generation === orgLoadGeneration.current;
    ++billingGeneration.current;
    setBillingBusy(false);
    setError(null);
    setNotice(null);
    setOpenOrg(no);
    setMembers(null);
    setMembersLoading(true);
    setAudit(null);
    setPlan(null);
    setDeletionActive(false);
    setSponsoredSeats([]);
    setSponsoredAvailable(null);
    try {
      const nextMembers = await fetchMembers(no.org.id);
      if (!isCurrent()) return;
      setMembers(nextMembers);
      setMembersLoading(false);
      const entitlements = await fetchEntitlements(no.org.id);
      if (!isCurrent()) return;
      setPlan(entitlements);
      if (["owner", "admin"].includes(no.org.role)) {
        // Sponsored billing is opt-in and may be absent on older servers. It must not hide the
        // established membership surface when that capability is unavailable.
        try {
          const nextSeats = await fetchSponsoredSeats(no.org.id);
          if (!isCurrent()) return;
          setSponsoredSeats(nextSeats);
          setSponsoredAvailable(true);
        } catch {
          if (!isCurrent()) return;
          setSponsoredSeats([]);
          setSponsoredAvailable(false);
        }
      }
      // The audit log is admin/owner-only AND a Team feature; skip the fetch when gated.
      if (
        ["owner", "admin"].includes(no.org.role) &&
        entitlements.effectiveTier === "team"
      ) {
        const nextAudit = await fetchAudit(no.org.id);
        if (!isCurrent()) return;
        setAudit(nextAudit);
      }
    } catch (e) {
      if (isCurrent()) {
        setError(message(e));
        setMembersLoading(false);
      }
    }
  }

  async function addSponsoredSeat() {
    if (openOrg === null || sponsoredBeneficiary.trim() === "") return;
    setSponsoredBusy(true);
    setError(null);
    try {
      const beneficiaryId = sponsoredBeneficiary.trim();
      const quote = await fetchSponsoredQuote(openOrg.org.id, {
        action: sponsoredAction,
        offer: "monthly",
        beneficiaryIds: [beneficiaryId],
      });
      const effectiveFrom = Math.min(
        Math.floor(Date.now() / 1000) + 30,
        quote.quoteExpiresAtEpoch - 1,
      );
      const result = await createSponsoredCheckout(openOrg.org.id, {
        action: sponsoredAction,
        offer: "monthly",
        beneficiaryId,
        replacementBeneficiaryId: sponsoredAction === "replace" ? sponsoredReplacement.trim() : undefined,
        quoteVersion: quote.quoteVersion,
        quoteExpiresAtEpoch: quote.quoteExpiresAtEpoch,
        effectiveFrom,
        effectiveUntil: sponsoredUntil === "" ? undefined : Math.floor(new Date(`${sponsoredUntil}T23:59:59Z`).getTime() / 1000),
        idempotencyKey: crypto.randomUUID(),
        returnUrl: window.location.origin,
      });
      if (result.checkoutUrl !== null) window.location.assign(result.checkoutUrl);
      else setNotice("Seat checkout is pending provider confirmation. Reload shortly.");
    } catch (e) {
      setError(message(e));
    } finally {
      setSponsoredBusy(false);
    }
  }

  async function invite(no: NamedOrg) {
    if (inviteInFlight.current) return;
    const submittedEmail = email.trim();
    if (submittedEmail === "") return;
    const generation = orgLoadGeneration.current;
    const isCurrent = () => generation === orgLoadGeneration.current;
    setError(null);
    setNotice(null);
    inviteInFlight.current = true;
    setInviteBusy(true);
    try {
      const invited = await inviteMember(no.org.id, submittedEmail);
      // Grant the invitee the org key so display names decrypt for them (best-effort: needs their
      // public key on file and our own org-key copy). A failure here - e.g. our copy is sealed to an
      // old keypair after a reset, or the server rejects the grant - must not fail the invite, which
      // has already succeeded; the invitee just sees the org id until someone re-grants the key.
      if (invited.publicKey !== null && no.org.encOrgKey !== null) {
        try {
          const orgKey = openOrgKey(master, encPrivateKeys, no.org.encOrgKey);
          await grantOrgKey(no.org.id, invited.userId, sealGrantTo(invited.publicKey, orgKey));
        } catch {
          // Best-effort only.
        }
      }
      if (!isCurrent()) return;
      setNotice(`invited ${submittedEmail} (${invited.userId})`);
      setEmail("");
      const nextMembers = await fetchMembers(no.org.id);
      if (isCurrent()) setMembers(nextMembers);
    } catch (e) {
      if (isCurrent()) setError(message(e));
    } finally {
      inviteInFlight.current = false;
      setInviteBusy(false);
    }
  }

  /// Hand the browser to a Stripe-hosted page. `busy` stays set on success: the page is about to
  /// navigate away, and re-enabling would invite a double click while it does.
  async function goToStripe(fetchUrl: (orgId: string) => Promise<string>, orgId: string) {
    const generation = ++billingGeneration.current;
    const isCurrent = () => generation === billingGeneration.current;
    setError(null);
    setNotice(null);
    setBillingBusy(true);
    try {
      const url = await fetchUrl(orgId);
      if (!isCurrent()) return;
      window.location.assign(url);
    } catch (e) {
      if (!isCurrent()) return;
      setError(message(e));
      setBillingBusy(false);
    }
  }

  // A known-empty org list means no team section. While still loading, or after a load error, keep
  // rendering so the error (or a loading state) stays visible instead of the panel vanishing.
  if (orgs !== null && orgs.length === 0) {
    return null;
  }
  // Admin/owner: the server's bar for both membership management and billing.
  const canManage =
    openOrg !== null && ["owner", "admin"].includes(openOrg.org.role) && !deletionActive;

  return (
    <section>
      <h2>Organisations</h2>
      {billingOutcome === "success" && (
        <p className="notice">
          Payment received. Your Team plan activates as soon as Stripe confirms, usually within
          seconds.
        </p>
      )}
      {billingOutcome === "cancelled" && (
        <p className="muted">Checkout cancelled. Nothing was charged.</p>
      )}
      {error !== null && <p role="alert">{error}</p>}
      {orgsError !== null && (
        <p>
          <span role="alert">{orgsError}</span>{" "}
          <button disabled={orgsLoading} onClick={() => void loadOrgs()}>
            {orgsLoading ? "Retrying…" : "Retry organisations"}
          </button>
        </p>
      )}
      <div role="status" aria-live="polite" aria-atomic="true">
        {notice !== null && <p className="notice">{notice}</p>}
      </div>
      {orgsLoading && orgs === null && <p className="muted">Loading…</p>}
      {orgs !== null && (
        <ul className="items">
          {orgs.map((o) => (
            <li key={o.org.id}>
              <button
                onClick={() => void selectOrg(o)}
                aria-current={openOrg?.org.id === o.org.id ? "true" : undefined}
              >
                {o.name}
                <span className="meta">{o.org.role}</span>
              </button>
            </li>
          ))}
        </ul>
      )}

      {openOrg !== null && (
        <>
          {plan !== null && (
            <p>
              Plan: <strong>{plan.effectiveTier}</strong>
              {plan.tier !== plan.effectiveTier && plan.trialEndsAt !== null
                ? ` (trial ends ${plan.trialEndsAt})`
                : ""}
              {plan.limits !== null
                ? ` - up to ${plan.limits.maxMembers} members, ${plan.limits.maxOrgProjects} project(s)`
                : ""}
            </p>
          )}
          {plan !== null &&
            canManage &&
            plan.billingEnabled &&
            (plan.tier === "team" || plan.purchasesEnabled) && (
            <p>
              {plan.tier !== "team" ? (
                <button
                  className="primary"
                  disabled={billingBusy}
                  onClick={() => void goToStripe(createCheckout, openOrg.org.id)}
                >
                  {billingBusy ? "Opening checkout…" : "Upgrade to Team"}
                </button>
              ) : (
                <button
                  disabled={billingBusy}
                  onClick={() => void goToStripe(createPortal, openOrg.org.id)}
                >
                  {billingBusy ? "Opening portal…" : "Manage billing"}
                </button>
              )}
            </p>
          )}
          {/* Deletion status is owner-only; admins keep this read surface, while the server rejects
              their organisation writes with 409 during an active deletion. */}
          {openOrg.org.role === "owner" && (
            <OrganisationDeletionPanel
              orgId={openOrg.org.id}
              orgName={openOrg.name}
              onActiveChange={setDeletionActive}
            />
          )}
          <h3>Members of {openOrg.name}</h3>
          {membersLoading && (
            <p className="muted">Loading…</p>
          )}
          {members !== null && (
            <ul className="items">
              {members.map((m) => (
                <li key={m.userId}>
                  {m.userId}
                  <span className="meta">
                    {m.role}
                    {m.publicKey === null ? " · no keys yet" : ""}
                  </span>
                </li>
              ))}
            </ul>
          )}
          {canManage && sponsoredAvailable === true && (
            <section aria-labelledby="sponsored-heading">
              <h3 id="sponsored-heading">Sponsored Cloud seats</h3>
              <p className="muted">Seat changes are quoted before checkout and take effect only after the provider confirms payment.</p>
              {sponsoredSeats.length === 0 ? <p className="muted">No sponsored seats are active.</p> : (
                <ul className="items">
                  {sponsoredSeats.map((seat) => <li key={seat.seatId}>{seat.beneficiaryId}<span className="meta">{seat.state} · {seat.offer}{seat.effectiveUntil === null ? " · ongoing" : ` · ends ${new Date(seat.effectiveUntil * 1000).toLocaleDateString("en-GB")}`}</span></li>)}
                </ul>
              )}
              <form className="row" onSubmit={(e) => { e.preventDefault(); void addSponsoredSeat(); }}>
                <label>Change <select value={sponsoredAction} onChange={(e) => setSponsoredAction(e.target.value as "add" | "remove" | "replace")} disabled={sponsoredBusy}><option value="add">Add seat</option><option value="remove">Remove seat</option><option value="replace">Replace seat</option></select></label>
                <label>Beneficiary user id<input value={sponsoredBeneficiary} onChange={(e) => setSponsoredBeneficiary(e.target.value)} disabled={sponsoredBusy} /></label>
                {sponsoredAction === "replace" && <label>Replacement user id<input value={sponsoredReplacement} onChange={(e) => setSponsoredReplacement(e.target.value)} disabled={sponsoredBusy} /></label>}
                {sponsoredAction !== "add" && <label>Effective until<input type="date" value={sponsoredUntil} onChange={(e) => setSponsoredUntil(e.target.value)} disabled={sponsoredBusy} required /></label>}
                <button type="submit" disabled={sponsoredBusy || sponsoredBeneficiary.trim() === "" || (sponsoredAction === "replace" && sponsoredReplacement.trim() === "")}>{sponsoredBusy ? "Preparing…" : sponsoredAction === "add" ? "Add seat" : sponsoredAction === "remove" ? "Schedule removal" : "Schedule replacement"}</button>
              </form>
            </section>
          )}
          {canManage && (
            <form
              className="row"
              onSubmit={(e) => {
                e.preventDefault();
                void invite(openOrg);
              }}
            >
              <label>
                Invite by email
                <input
                  type="email"
                  value={email}
                  onChange={(e) => setEmail(e.target.value)}
                  placeholder="teammate@example.com"
                  disabled={inviteBusy}
                />
              </label>
              <button type="submit" disabled={inviteBusy || email.trim() === ""}>
                {inviteBusy ? "Inviting…" : "Invite"}
              </button>
            </form>
          )}
          {audit !== null && (
            <>
              <h3>Audit log</h3>
              {audit.length === 0 ? (
                <p className="muted">No events yet.</p>
              ) : (
                <ul>
                  {audit.map((ev) => (
                    <li key={ev.id}>
                      <code>{ev.at}</code> {ev.action} - {ev.actor}
                      {ev.target !== null ? ` → ${ev.target}` : ""}
                      {ev.envId !== null ? ` (env ${ev.envId})` : ""}
                      {ev.detail !== null ? ` - ${ev.detail}` : ""}
                    </li>
                  ))}
                </ul>
              )}
            </>
          )}
        </>
      )}
    </section>
  );
}
