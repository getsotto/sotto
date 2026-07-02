import { useEffect, useState } from "react";

import {
  createGrant,
  createShare,
  fetchEnvironments,
  fetchGrantHolders,
  fetchHistory,
  fetchMachineTokens,
  fetchMembers,
  fetchMyGrant,
  fetchOrgs,
  fetchProjects,
  fetchSecrets,
  fetchSnapshot,
  grantOrgKey,
  postRotate,
  type Environment,
  type Member,
  type Project,
} from "./api";
import { bytesToUrlSafeB64 } from "./base64";
import { TeamPanel } from "./TeamPanel";
import {
  decryptEnvName,
  decryptProjectName,
  decryptSecretName,
  decryptSecretValue,
  openEnvGrant,
  openOrgKey,
  rewrapDataKey,
  sealForShare,
  sealGrantTo,
  type SecretEntry,
} from "./vault";

interface NamedProject {
  project: Project;
  name: string;
}
interface NamedEnv {
  env: Environment;
  name: string;
}
interface NamedSecret {
  entry: SecretEntry;
  name: string;
}
interface OpenEnv {
  envId: string;
  vaultKey: Uint8Array;
  secrets: NamedSecret[];
}
interface Revealed {
  name: string;
  value: string;
  link: string | null;
}

function message(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

// Fall back to the id if a metadata name can't be decrypted (e.g. an unnamed record).
function nameOr(id: string, decrypt: () => string): string {
  try {
    return decrypt();
  } catch {
    return id;
  }
}

export function VaultView({
  master,
  encPrivateKeys,
  onLogout,
}: {
  master: Uint8Array;
  encPrivateKeys: Uint8Array;
  onLogout: () => void;
}) {
  const [projects, setProjects] = useState<NamedProject[] | null>(null);
  const [activeProject, setActiveProject] = useState<NamedProject | null>(null);
  const [envs, setEnvs] = useState<NamedEnv[] | null>(null);
  const [openEnv, setOpenEnv] = useState<OpenEnv | null>(null);
  const [revealed, setRevealed] = useState<Revealed | null>(null);
  // Org members of the active project (loaded when an org env is opened), for the share picker.
  const [members, setMembers] = useState<Member[] | null>(null);
  const [shareTo, setShareTo] = useState("");
  // Opened org keys by org id — org project/env names decrypt under these.
  const [orgKeys, setOrgKeys] = useState<Map<string, Uint8Array>>(new Map());
  // The caller's role per org id (rotation is admin/owner-only).
  const [orgRoles, setOrgRoles] = useState<Map<string, string>>(new Map());
  const [notice, setNotice] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  /// The name-decryption key for a project: its org key when we hold one, else the master key.
  function nameKeyFor(orgId: string | null): Uint8Array {
    return (orgId !== null ? orgKeys.get(orgId) : undefined) ?? master;
  }

  useEffect(() => {
    void (async () => {
      try {
        // Open every org key we hold first, so org project names decrypt on first paint.
        const keys = new Map<string, Uint8Array>();
        const roles = new Map<string, string>();
        for (const org of await fetchOrgs()) {
          roles.set(org.id, org.role);
          if (org.encOrgKey !== null) {
            try {
              keys.set(org.id, openOrgKey(master, encPrivateKeys, org.encOrgKey));
            } catch {
              // A copy sealed to keys we no longer hold: fall back to master-key naming.
            }
          }
        }
        setOrgKeys(keys);
        setOrgRoles(roles);

        const rows = await fetchProjects();
        setProjects(
          rows.map((project) => {
            const key = (project.orgId !== null ? keys.get(project.orgId) : undefined) ?? master;
            return {
              project,
              name: nameOr(project.id, () => decryptProjectName(key, project.id, project.encName)),
            };
          }),
        );
      } catch (e) {
        setError(message(e));
      }
    })();
  }, [master, encPrivateKeys]);

  async function selectProject(np: NamedProject) {
    setError(null);
    setNotice(null);
    setActiveProject(np);
    setEnvs(null);
    setOpenEnv(null);
    setRevealed(null);
    setMembers(null);
    setShareTo(""); // drop a stale member pick so the next env's Share button starts disabled
    try {
      const key = nameKeyFor(np.project.orgId);
      const rows = await fetchEnvironments(np.project.id);
      setEnvs(
        rows.map((env) => ({ env, name: nameOr(env.id, () => decryptEnvName(key, env.id, env.encName)) })),
      );
    } catch (e) {
      setError(message(e));
    }
  }

  async function selectEnv(ne: NamedEnv) {
    setError(null);
    setNotice(null);
    setOpenEnv(null);
    setRevealed(null);
    setMembers(null);
    setShareTo(""); // the new env reloads its own members; don't carry a stale pick across
    try {
      // Open via our OWN grant, not the env's inline key — on a shared env the inline key is the
      // creator's grant, which our keypair can't open.
      const grant = await fetchMyGrant(ne.env.id);
      if (grant === null) {
        setError("you have no key for this environment — ask an admin to share it with you");
        return;
      }
      const vaultKey = openEnvGrant(master, encPrivateKeys, grant);
      const secrets = (await fetchSecrets(ne.env.id))
        .filter((entry) => !entry.deleted)
        .map((entry) => ({
          entry,
          name: nameOr(entry.id, () => decryptSecretName(vaultKey, ne.env.id, entry)),
        }))
        .sort((a, b) => a.name.localeCompare(b.name));
      setOpenEnv({ envId: ne.env.id, vaultKey, secrets });
      // Org project: load the member list so the env can be shared from here.
      const orgId = activeProject?.project.orgId;
      if (orgId) {
        setMembers(await fetchMembers(orgId));
      }
    } catch (e) {
      setError(message(e));
    }
  }

  /// Share the open environment with an org member: seal its vault key to their public key.
  async function shareWithMember() {
    if (openEnv === null || members === null) {
      return;
    }
    setError(null);
    setNotice(null);
    const member = members.find((m) => m.userId === shareTo);
    if (member === undefined) {
      setError("pick a member to share with");
      return;
    }
    if (member.publicKey === null) {
      setError("that member has no account keys yet — they must finish setup first");
      return;
    }
    try {
      const sealed = sealGrantTo(member.publicKey, openEnv.vaultKey);
      await createGrant(openEnv.envId, member.userId, sealed);
      // Whoever can decrypt the environment should read its display names too: upsert their org-key
      // copy alongside the env grant (best-effort — needs our own copy, and is admin/owner-only
      // server-side). A failure must not undo or mask the env share that already succeeded.
      const orgId = activeProject?.project.orgId ?? null;
      const orgKey = orgId !== null ? orgKeys.get(orgId) : undefined;
      if (orgId !== null && orgKey !== undefined) {
        try {
          await grantOrgKey(orgId, member.userId, sealGrantTo(member.publicKey, orgKey));
        } catch {
          // Best-effort only; the member may just see the org id for names.
        }
      }
      setNotice(`shared this environment with ${member.userId}`);
    } catch (e) {
      setError(message(e));
    }
  }

  /// Rotate the open environment's vault key (admin/owner): rewrap every current + history data
  /// key, re-seal grants for the current holders and machine tokens, then reload under the new key.
  async function rotateEnv() {
    if (openEnv === null) {
      return;
    }
    setError(null);
    setNotice(null);
    try {
      // Rotation re-seals a grant for every current holder, so we need the member roster (their
      // public keys). It normally loads when the env opens; fetch it here if it isn't ready yet,
      // instead of silently doing nothing, and surface any failure as an error.
      let roster = members;
      if (roster === null) {
        const orgId = activeProject?.project.orgId;
        if (!orgId) {
          throw new Error("this environment is not in an organization; nothing to rotate");
        }
        roster = await fetchMembers(orgId);
        setMembers(roster);
      }
      const newKey = crypto.getRandomValues(new Uint8Array(32));
      const snap = await fetchSnapshot(openEnv.envId);
      const dataKeys = snap.secrets.map((s) => ({
        secretId: s.id,
        encDataKey: rewrapDataKey(openEnv.vaultKey, newKey, openEnv.envId, s.id, s.version, s.encDataKey),
      }));
      const historyKeys = (await fetchHistory(openEnv.envId)).map((h) => ({
        secretId: h.secretId,
        version: h.version,
        encDataKey: rewrapDataKey(openEnv.vaultKey, newKey, openEnv.envId, h.secretId, h.version, h.encDataKey),
      }));
      const grants = [];
      for (const holder of await fetchGrantHolders(openEnv.envId)) {
        const m = roster.find((mm) => mm.userId === holder);
        if (m === undefined || m.publicKey === null) {
          throw new Error(`cannot re-grant ${holder}: no public key on file`);
        }
        grants.push({ userId: holder, encVaultKey: sealGrantTo(m.publicKey, newKey) });
      }
      const machineGrants = (await fetchMachineTokens(openEnv.envId)).map((t) => ({
        tokenId: t.tokenId,
        encVaultKey: sealGrantTo(t.publicKey, newKey),
      }));
      await postRotate(openEnv.envId, {
        baseRevision: snap.revision,
        grants,
        dataKeys,
        machineGrants,
        historyKeys,
      });
      setNotice("environment key rotated");
      // Reload the environment under the new key (fetches the re-sealed grant).
      const current = envs?.find((e) => e.env.id === openEnv.envId);
      if (current !== undefined) {
        await selectEnv(current);
      }
    } catch (e) {
      setError(message(e));
    }
  }

  function reveal(ns: NamedSecret) {
    if (openEnv === null) {
      return;
    }
    setError(null);
    try {
      const value = decryptSecretValue(openEnv.vaultKey, openEnv.envId, ns.entry);
      setRevealed({ name: ns.name, value, link: null });
    } catch (e) {
      setError(message(e));
    }
  }

  async function share(current: Revealed) {
    try {
      const { encBlob, fragmentKey } = sealForShare(current.value);
      const token = await createShare(encBlob, 1);
      const link = `${window.location.origin}/s/${token}#${bytesToUrlSafeB64(fragmentKey)}`;
      setRevealed({ ...current, link });
    } catch (e) {
      setError(message(e));
    }
  }

  return (
    <main>
      <h1>Your vault</h1>
      <button onClick={onLogout}>Log out</button>
      {error !== null && <p role="alert">{error}</p>}
      {notice !== null && <p>{notice}</p>}

      <section>
        <h2>Projects</h2>
        {projects === null ? (
          <p>Loading…</p>
        ) : (
          <ul>
            {projects.map((p) => (
              <li key={p.project.id}>
                <button onClick={() => void selectProject(p)}>{p.name}</button>
              </li>
            ))}
          </ul>
        )}
      </section>

      {envs !== null && (
        <section>
          <h2>Environments</h2>
          <ul>
            {envs.map((e) => (
              <li key={e.env.id}>
                <button onClick={() => void selectEnv(e)}>{e.name}</button>
              </li>
            ))}
          </ul>
        </section>
      )}

      {openEnv !== null && (
        <section>
          <h2>Secrets</h2>
          {openEnv.secrets.length === 0 ? (
            <p>No secrets in this environment.</p>
          ) : (
            <ul>
              {openEnv.secrets.map((s) => (
                <li key={s.entry.id}>
                  <button onClick={() => reveal(s)}>{s.name}</button>
                </li>
              ))}
            </ul>
          )}
          {members !== null && members.length > 0 && (
            <form
              onSubmit={(e) => {
                e.preventDefault();
                void shareWithMember();
              }}
            >
              <label>
                Share this environment with
                <select value={shareTo} onChange={(e) => setShareTo(e.target.value)}>
                  <option value="">— pick a member —</option>
                  {members.map((m) => (
                    <option key={m.userId} value={m.userId}>
                      {m.userId} ({m.role})
                    </option>
                  ))}
                </select>
              </label>
              <button type="submit" disabled={shareTo === ""}>
                Share
              </button>
            </form>
          )}
          {(() => {
            const orgId = activeProject?.project.orgId ?? null;
            const canRotate =
              orgId !== null && ["owner", "admin"].includes(orgRoles.get(orgId) ?? "");
            return canRotate ? (
              <p>
                <button onClick={() => void rotateEnv()}>Rotate environment key</button>
              </p>
            ) : null;
          })()}
        </section>
      )}

      {revealed !== null && (
        <section>
          <h2>{revealed.name}</h2>
          <textarea readOnly value={revealed.value} rows={3} spellCheck={false} />
          {revealed.link === null ? (
            <button onClick={() => void share(revealed)}>Create one-time share link</button>
          ) : (
            <>
              <p>Share link (burns after one view):</p>
              <textarea readOnly value={revealed.link} rows={2} spellCheck={false} />
            </>
          )}
        </section>
      )}

      <TeamPanel master={master} encPrivateKeys={encPrivateKeys} />
    </main>
  );
}
