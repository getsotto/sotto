import { useEffect, useState } from "react";

import { fetchMembers, fetchOrgs, grantOrgKey, inviteMember, type Member, type Org } from "./api";
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
    try {
      setMembers(await fetchMembers(no.org.id));
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
      // public key on file and our own org-key copy).
      if (invited.publicKey !== null && no.org.encOrgKey !== null) {
        const orgKey = openOrgKey(master, encPrivateKeys, no.org.encOrgKey);
        await grantOrgKey(no.org.id, invited.userId, sealGrantTo(invited.publicKey, orgKey));
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
      {notice !== null && <p>{notice}</p>}
      {orgs === null && error === null && <p>Loading…</p>}
      {orgs !== null && (
        <ul>
          {orgs.map((o) => (
            <li key={o.org.id}>
              <button onClick={() => void selectOrg(o)}>{o.name}</button> ({o.org.role})
            </li>
          ))}
        </ul>
      )}

      {openOrg !== null && (
        <>
          <h3>Members of {openOrg.name}</h3>
          {members === null ? (
            <p>Loading…</p>
          ) : (
            <ul>
              {members.map((m) => (
                <li key={m.userId}>
                  {m.userId} ({m.role}){m.publicKey === null ? " — no keys yet" : ""}
                </li>
              ))}
            </ul>
          )}
          {canInvite && (
            <form
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
        </>
      )}
    </section>
  );
}
