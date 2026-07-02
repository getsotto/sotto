import { useEffect, useState } from "react";

import { fetchMembers, fetchOrgs, inviteMember, type Member, type Org } from "./api";
import { decryptOrgName } from "./vault";

interface NamedOrg {
  org: Org;
  name: string;
}

function message(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/// The team section: the caller's organizations, each expandable to its member list, with
/// invite-by-email for admins/owners. Org names decrypt only for their creator (they're sealed
/// under the creator's master key); everyone else sees the org id.
export function TeamPanel({ master }: { master: Uint8Array }) {
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
        setOrgs(
          rows.map((org) => {
            let name = org.id;
            try {
              name = decryptOrgName(master, org.id, org.encName);
            } catch {
              // Not the creator: the name is sealed under someone else's master key.
            }
            return { org, name };
          }),
        );
      } catch (e) {
        setError(message(e));
      }
    })();
  }, [master]);

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
      const userId = await inviteMember(no.org.id, email.trim());
      setNotice(`invited ${email.trim()} (${userId})`);
      setEmail("");
      setMembers(await fetchMembers(no.org.id));
    } catch (e) {
      setError(message(e));
    }
  }

  if (orgs === null || orgs.length === 0) {
    return null; // no team section for users without orgs
  }
  const canInvite = openOrg !== null && ["owner", "admin"].includes(openOrg.org.role);

  return (
    <section>
      <h2>Organizations</h2>
      {error !== null && <p role="alert">{error}</p>}
      {notice !== null && <p>{notice}</p>}
      <ul>
        {orgs.map((o) => (
          <li key={o.org.id}>
            <button onClick={() => void selectOrg(o)}>{o.name}</button> ({o.org.role})
          </li>
        ))}
      </ul>

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
