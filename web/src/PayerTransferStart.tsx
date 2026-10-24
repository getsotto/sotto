import { useEffect, useRef, useState } from "react";

import {
  beginPayerTransfer,
  fetchMembers,
  fetchOrgs,
  fetchPayerTransferCapabilities,
  me,
  quotePayerTransfer,
  type Member,
  type Org,
  type PayerTransferQuote,
  type SponsoredSeat,
} from "./api";

function message(error: unknown): string {
  if (!(error instanceof Error)) return "The transfer quote could not be loaded.";
  const status = /server error \((\d+)\)/.exec(error.message)?.[1];
  if (status === "403") return "You do not have authority for this payer change.";
  if (status === "404") return "Payer transfers are not available on this server.";
  if (status === "409") return "The billing terms changed. Refresh the page and request a new quote.";
  return "The transfer service is temporarily unavailable. Try again shortly.";
}

function date(epoch: number): string {
  return new Date(epoch * 1000).toLocaleDateString("en-GB");
}

function money(pence: number): string {
  return new Intl.NumberFormat("en-GB", { style: "currency", currency: "GBP" }).format(pence / 100);
}

function beginNotice(state: string): string {
  if (state === "awaiting_consent") {
    return "Transfer request sent. The other payer must accept it; no billing change has happened yet.";
  }
  if (state === "unknown") {
    return "This transfer needs support review. No payer change is treated as complete.";
  }
  if (state === "completed") {
    return "This transfer is already complete after provider verification.";
  }
  if (state === "failed") {
    return "This transfer request has ended without changing the payer.";
  }
  return "Transfer started. Billing will change only after the provider verifies the renewal handover.";
}

function CounterpartySelect({
  members,
  value,
  onChange,
  exclude,
  disabled = false,
}: {
  members: Member[];
  value: string;
  onChange: (value: string) => void;
  exclude: string;
  disabled?: boolean;
}) {
  const currentIsAdmin = members.some((member) => member.userId === exclude && ["owner", "admin"].includes(member.role));
  const admins = members.filter((member) => member.userId !== exclude && ["owner", "admin"].includes(member.role));
  return <label>Other payer administrator
    <select value={value} onChange={(event) => onChange(event.target.value)} disabled={disabled} required>
      <option value="">Choose an organisation owner or admin</option>
      {currentIsAdmin && <option value={exclude}>I also administer this organisation</option>}
      {admins.map((member) => <option key={member.userId} value={member.userId}>{member.userId} · {member.role}</option>)}
    </select>
  </label>;
}

function loadMessage(error: unknown): string {
  if (!(error instanceof Error)) return "Your organisations could not be loaded. Try again shortly.";
  const status = /server error \((\d+)\)/.exec(error.message)?.[1];
  if (status === "403") return "You do not have authority to list organisations for this account.";
  if (status === "404") return "Organisation lookup is not available on this server.";
  return "Your organisations could not be loaded. Try again shortly.";
}

export function PersonalTransferStart() {
  const [available, setAvailable] = useState(false);
  const [userId, setUserId] = useState("");
  const [orgs, setOrgs] = useState<Org[]>([]);
  const [destinationOrgId, setDestinationOrgId] = useState("");
  const [members, setMembers] = useState<Member[]>([]);
  const [counterpartyId, setCounterpartyId] = useState("");
  const [quote, setQuote] = useState<PayerTransferQuote | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [membersError, setMembersError] = useState<string | null>(null);
  const [membersLoading, setMembersLoading] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<string | null>(null);
  const idempotencyKey = useRef<string | null>(null);
  const membersInFlight = useRef(false);
  const formBusy = busy || membersLoading;
  const loadInFlight = useRef(false);
  const mounted = useRef(true);

  async function loadInitial() {
    if (loadInFlight.current) return;
    loadInFlight.current = true;
    setLoading(true);
    try {
      const [capabilities, account] = await Promise.all([fetchPayerTransferCapabilities(), me()]);
      if (!mounted.current) return;
      const nextAvailable = capabilities.available;
      const nextUserId = account?.userId ?? "";
      setAvailable(nextAvailable);
      setUserId(nextUserId);
      if (!nextAvailable || nextUserId === "") {
        setOrgs([]);
        setLoadError(null);
        return;
      }
      // A personal beneficiary can ask their existing organisation to take over billing even
      // when they are an ordinary member; the organisation administrator is the counterparty.
      try {
        const organisations = await fetchOrgs();
        if (!mounted.current) return;
        setOrgs(organisations);
        setLoadError(null);
      } catch (e) {
        if (!mounted.current) return;
        setOrgs([]);
        setLoadError(loadMessage(e));
      }
    } catch {
      // Capability or account discovery failed: keep the section hidden like unsupported servers.
      if (mounted.current) {
        setAvailable(false);
        setUserId("");
        setOrgs([]);
        setLoadError(null);
      }
    } finally {
      if (mounted.current) setLoading(false);
      loadInFlight.current = false;
    }
  }

  useEffect(() => {
    mounted.current = true;
    void loadInitial();
    return () => { mounted.current = false; };
  }, []);

  async function chooseOrganisation(orgId: string) {
    if (membersInFlight.current) return;
    setDestinationOrgId(orgId);
    setCounterpartyId("");
    setMembers([]);
    setQuote(null);
    setError(null);
    setMembersError(null);
    if (orgId === "") return;
    await loadMembers(orgId);
  }

  async function loadMembers(orgId: string) {
    if (orgId === "" || membersInFlight.current) return;
    membersInFlight.current = true;
    setMembersLoading(true);
    setMembersError(null);
    try {
      setMembers(await fetchMembers(orgId));
    } catch (e) {
      setMembersError(message(e));
    } finally {
      membersInFlight.current = false;
      setMembersLoading(false);
    }
  }

  async function requestQuote() {
    if (!userId || !destinationOrgId || !counterpartyId) return;
    setBusy(true); setError(null); setNotice(null);
    try {
      setQuote(await quotePayerTransfer({
        beneficiaryId: userId,
        counterpartyUserId: counterpartyId,
        destinationKind: "sponsor",
        destinationOrganizationId: destinationOrgId,
      }));
      idempotencyKey.current = crypto.randomUUID();
    } catch (e) {
      setError(message(e));
      if (e instanceof Error && e.message.includes("(409)")) {
        setQuote(null);
        idempotencyKey.current = null;
      }
    }
    finally { setBusy(false); }
  }

  async function createInvitation() {
    if (quote === null) return;
    setBusy(true); setError(null); setNotice(null);
    try {
      const result = await beginPayerTransfer({ quote, idempotencyKey: idempotencyKey.current ?? crypto.randomUUID() });
      setQuote(null);
      setNotice(beginNotice(result.state));
      window.dispatchEvent(new Event("sotto:payer-transfer-updated"));
    } catch (e) {
      setError(message(e));
      if (e instanceof Error && e.message.includes("(409)")) {
        setQuote(null);
        idempotencyKey.current = null;
      }
    }
    finally { setBusy(false); }
  }

  if (!available || userId === "") return null;
  return <section aria-labelledby="personal-transfer-heading">
    <h2 id="personal-transfer-heading">Change who pays</h2>
    <p className="muted">Move billing to an organisation at your next renewal. The current payer remains responsible through the paid term, with no mid-term credit.</p>
    {loadError !== null ? <>
      <p role="alert">{loadError}</p>
      <button type="button" disabled={loading} aria-busy={loading ? "true" : undefined} onClick={() => void loadInitial()}>{loading ? "Retrying…" : "Retry"}</button>
      {loading && <p className="muted" role="status">Loading organisations…</p>}
    </> : loading ? <p className="muted" role="status">Loading organisations…</p> : <>
      <label>Organisation
        <select value={destinationOrgId} onChange={(event) => void chooseOrganisation(event.target.value)} disabled={formBusy}>
          <option value="">Choose an organisation you belong to</option>
          {orgs.map((org) => <option key={org.id} value={org.id}>{org.id} · {org.role}</option>)}
        </select>
      </label>
      {destinationOrgId !== "" && <CounterpartySelect members={members} value={counterpartyId} onChange={(value) => { setCounterpartyId(value); setQuote(null); }} exclude={userId} disabled={formBusy} />}
      {membersError !== null && <p role="alert">{membersError}</p>}
      {(membersError !== null || membersLoading) && <button disabled={formBusy} onClick={() => void loadMembers(destinationOrgId)}>
        {membersLoading ? "Loading administrators…" : "Retry loading administrators"}
      </button>}
      {error !== null && <p role="alert">{error}</p>}
      {notice !== null && <p className="notice" role="status">{notice}</p>}
      {quote === null
        ? <button disabled={formBusy || destinationOrgId === "" || counterpartyId === ""} onClick={() => void requestQuote()}>{busy ? "Loading transfer quote…" : "Review payer transfer"}</button>
        : <div className="notice" role="status">
          <p>At renewal on {date(quote.effectiveFrom)}, {quote.destinationKind === "sponsor" ? "the organisation" : "you"} would pay {money(quote.amountPence)} per {quote.interval}. No mid-term credit is due.</p>
          {quote.founding && <p className="muted">The server has included the founding price applicable at this renewal.</p>}
          <button disabled={formBusy} onClick={() => void createInvitation()}>{busy ? "Sending request…" : "Send request for acceptance"}</button>{" "}
          <button disabled={formBusy} onClick={() => { setQuote(null); idempotencyKey.current = null; }}>Cancel</button>
        </div>}
    </>}
  </section>;
}

export function SponsoredTransferStart({
  sourceOrganizationId,
  seats,
  organisations,
}: {
  sourceOrganizationId: string;
  seats: SponsoredSeat[];
  organisations: Org[];
}) {
  const [available, setAvailable] = useState(false);
  const [setupAvailable, setSetupAvailable] = useState(false);
  const [actorId, setActorId] = useState("");
  const [beneficiaryId, setBeneficiaryId] = useState("");
  const [destinationKind, setDestinationKind] = useState<"personal" | "sponsor">("personal");
  const [destinationOrgId, setDestinationOrgId] = useState("");
  const [members, setMembers] = useState<Member[]>([]);
  const [counterpartyId, setCounterpartyId] = useState("");
  const [quote, setQuote] = useState<PayerTransferQuote | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const idempotencyKey = useRef<string | null>(null);
  const activeSeats = seats.filter((seat) => seat.state === "active");

  useEffect(() => {
    let current = true;
    void Promise.all([fetchPayerTransferCapabilities(), me()])
      .then(([capabilities, account]) => {
        if (!current) return;
        setAvailable(capabilities.available);
        setSetupAvailable(capabilities.setupAvailable);
        if (!capabilities.setupAvailable) setDestinationKind("sponsor");
        setActorId(account?.userId ?? "");
      })
      .catch(() => { if (current) setAvailable(false); });
    return () => { current = false; };
  }, []);

  async function chooseDestinationKind(kind: "personal" | "sponsor") {
    setDestinationKind(kind);
    setQuote(null); setError(null); setMembers([]); setCounterpartyId("");
    if (kind === "personal") {
      setDestinationOrgId("");
      setCounterpartyId(beneficiaryId);
    } else {
      setCounterpartyId("");
    }
  }

  async function chooseDestinationOrg(orgId: string) {
    setDestinationOrgId(orgId); setCounterpartyId(""); setQuote(null); setMembers([]); setError(null);
    if (orgId === "") return;
    setBusy(true);
    try { setMembers(await fetchMembers(orgId)); }
    catch (e) { setError(message(e)); }
    finally { setBusy(false); }
  }

  async function requestQuote() {
    if (!beneficiaryId || !counterpartyId
      || (destinationKind === "personal" && !setupAvailable)
      || (destinationKind === "sponsor" && !destinationOrgId)) return;
    setBusy(true); setError(null); setNotice(null);
    try {
      setQuote(await quotePayerTransfer({
        beneficiaryId,
        counterpartyUserId: counterpartyId,
        destinationKind,
        destinationOrganizationId: destinationKind === "sponsor" ? destinationOrgId : undefined,
      }));
      idempotencyKey.current = crypto.randomUUID();
    } catch (e) {
      setError(message(e));
      if (e instanceof Error && e.message.includes("(409)")) {
        setQuote(null);
        idempotencyKey.current = null;
      }
    }
    finally { setBusy(false); }
  }

  async function createInvitation() {
    if (quote === null) return;
    setBusy(true); setError(null); setNotice(null);
    try {
      const result = await beginPayerTransfer({ quote, idempotencyKey: idempotencyKey.current ?? crypto.randomUUID() });
      setQuote(null);
      setNotice(beginNotice(result.state));
      window.dispatchEvent(new Event("sotto:payer-transfer-updated"));
    } catch (e) {
      setError(message(e));
      if (e instanceof Error && e.message.includes("(409)")) {
        setQuote(null);
        idempotencyKey.current = null;
      }
    }
    finally { setBusy(false); }
  }

  if (!available || actorId === "" || activeSeats.length === 0) return null;
  return <section aria-labelledby="sponsored-transfer-heading">
    <h3 id="sponsored-transfer-heading">Change a sponsored payer</h3>
    <p className="muted">Payer changes take effect at the next renewal. The current payer remains responsible through the paid term; no mid-term credit is issued.</p>
    <label>Sponsored member
      <select value={beneficiaryId} disabled={busy} onChange={(event) => {
        const next = event.target.value;
        setBeneficiaryId(next); setQuote(null); setError(null);
        if (destinationKind === "personal") setCounterpartyId(next);
      }}>
        <option value="">Choose an active sponsored member</option>
        {activeSeats.map((seat) => <option key={seat.seatId} value={seat.beneficiaryId}>{seat.beneficiaryId}</option>)}
      </select>
    </label>
    <label>New payer
      <select value={destinationKind} disabled={busy} onChange={(event) => void chooseDestinationKind(event.target.value as "personal" | "sponsor")}>
        <option value="personal" disabled={!setupAvailable}>Member pays personally{setupAvailable ? "" : " (not available yet)"}</option>
        <option value="sponsor">Another organisation sponsors</option>
      </select>
    </label>
    {destinationKind === "sponsor" && <>
      <label>Destination organisation
        <select value={destinationOrgId} disabled={busy} onChange={(event) => void chooseDestinationOrg(event.target.value)}>
          <option value="">Choose an organisation you belong to</option>
          {organisations.filter((org) => org.id !== sourceOrganizationId).map((org) => <option key={org.id} value={org.id}>{org.id}</option>)}
        </select>
      </label>
      {destinationOrgId !== "" && <CounterpartySelect members={members} value={counterpartyId} onChange={(value) => { setCounterpartyId(value); setQuote(null); }} exclude={actorId} disabled={busy} />}
    </>}
    {error !== null && <p role="alert">{error}</p>}
    {notice !== null && <p className="notice" role="status">{notice}</p>}
    {quote === null
      ? <button disabled={busy || beneficiaryId === "" || counterpartyId === "" || (destinationKind === "personal" && !setupAvailable) || (destinationKind === "sponsor" && destinationOrgId === "")} onClick={() => void requestQuote()}>{busy ? "Loading transfer quote…" : "Review payer transfer"}</button>
      : <div className="notice" role="status">
        <p>At renewal on {date(quote.effectiveFrom)}, {quote.destinationKind === "sponsor" ? "the destination organisation" : "the member"} would pay {money(quote.amountPence)} per {quote.interval}. No mid-term credit is due.</p>
        {quote.founding && <p className="muted">The server has included the founding price applicable at this renewal.</p>}
        <button disabled={busy} onClick={() => void createInvitation()}>{busy ? "Sending request…" : "Send request for acceptance"}</button>{" "}
        <button disabled={busy} onClick={() => { setQuote(null); idempotencyKey.current = null; }}>Cancel</button>
      </div>}
  </section>;
}
