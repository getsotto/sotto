import { useEffect, useState } from "react";

import {
  fetchAudit,
  fetchEntitlements,
  fetchMembers,
  fetchOrgs,
  grantOrgKey,
  inviteMember,
  type AuditEvent,
  type Entitlements,
  type Member,
  type Org,
} from "./api";
import { decryptOrgName, openOrgKey, sealGrantTo } from "./vault";

interface NamedOrg {
  org: Org;
  name: string;
}

function message(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
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

/// The team section: the caller's organizations, each expandable to its member list, with
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
  const [openOrg, setOpenOrg] = useState<NamedOrg | null>(null);
  const [members, setMembers] = useState<Member[] | null>(null);
  const [audit, setAudit] = useState<AuditEvent[] | null>(null);
  const [plan, setPlan] = useState<Entitlements | null>(null);
  const [email, setEmail] = useState("");
  const [notice, setNotice] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    void (async () => {
      try {
        const rows = await fetchOrgs();
        setOrgs(rows.map((org) => ({ org, name: orgDisplayName(master, encPrivateKeys, org) })));
      } catch (e) {
        setError(message(e));
      }
    })();
  }, [master, encPrivateKeys]);

  async function selectOrg(no: NamedOrg) {
    setError(null);
    setNotice(null);
    setOpenOrg(no);
    setMembers(null);
    setAudit(null);
    setPlan(null);
    try {
      setMembers(await fetchMembers(no.org.id));
      const entitlements = await fetchEntitlements(no.org.id);
      setPlan(entitlements);
      // The audit log is admin/owner-only AND a Team feature; skip the fetch when gated.
      if (
        ["owner", "admin"].includes(no.org.role) &&
        entitlements.effectiveTier === "team"
      ) {
        setAudit(await fetchAudit(no.org.id));
      }
    } catch (e) {
      setError(message(e));
    }
  }

  async function invite(no: NamedOrg) {
    setError(null);
    setNotice(null);
    try {
      const invited = await inviteMember(no.org.id, email.trim());
      // Grant the invitee the org key so display names decrypt for them (best-effort: needs their
      // public key on file and our own org-key copy). A failure here — e.g. our copy is sealed to an
      // old keypair after a reset, or the server rejects the grant — must not fail the invite, which
      // has already succeeded; the invitee just sees the org id until someone re-grants the key.
      if (invited.publicKey !== null && no.org.encOrgKey !== null) {
        try {
          const orgKey = openOrgKey(master, encPrivateKeys, no.org.encOrgKey);
          await grantOrgKey(no.org.id, invited.userId, sealGrantTo(invited.publicKey, orgKey));
        } catch {
          // Best-effort only.
        }
      }
      setNotice(`invited ${email.trim()} (${invited.userId})`);
      setEmail("");
      setMembers(await fetchMembers(no.org.id));
    } catch (e) {
      setError(message(e));
    }
  }

  // A known-empty org list means no team section. While still loading, or after a load error, keep
  // rendering so the error (or a loading state) stays visible instead of the panel vanishing.
  if (orgs !== null && orgs.length === 0) {
    return null;
  }
  const canInvite = openOrg !== null && ["owner", "admin"].includes(openOrg.org.role);

  return (
    <section>
      <h2>Organizations</h2>
      {error !== null && <p role="alert">{error}</p>}
      {notice !== null && <p className="notice">{notice}</p>}
      {orgs === null && error === null && <p className="muted">Loading…</p>}
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
                ? ` — up to ${plan.limits.maxMembers} members, ${plan.limits.maxOrgProjects} project(s)`
                : ""}
            </p>
          )}
          <h3>Members of {openOrg.name}</h3>
          {members === null ? (
            <p className="muted">Loading…</p>
          ) : (
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
          {canInvite && (
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
                />
              </label>
              <button type="submit" disabled={email.trim() === ""}>
                Invite
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
                      <code>{ev.at}</code> {ev.action} — {ev.actor}
                      {ev.target !== null ? ` → ${ev.target}` : ""}
                      {ev.envId !== null ? ` (env ${ev.envId})` : ""}
                      {ev.detail !== null ? ` — ${ev.detail}` : ""}
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
