import { useEffect, useState } from "react";

import {
  createGrant,
  createShare,
  fetchEnvironments,
  fetchMembers,
  fetchMyGrant,
  fetchOrgs,
  fetchProjects,
  fetchSecrets,
  grantOrgKey,
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
        for (const org of await fetchOrgs()) {
          if (org.encOrgKey !== null) {
            try {
              keys.set(org.id, openOrgKey(master, encPrivateKeys, org.encOrgKey));
            } catch {
              // A copy sealed to keys we no longer hold: fall back to master-key naming.
            }
          }
        }
        setOrgKeys(keys);

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
      // Whoever can decrypt the environment should read its display names too: upsert their
      // org-key copy alongside the env grant (best-effort — needs our own copy).
      const orgId = activeProject?.project.orgId ?? null;
      const orgKey = orgId !== null ? orgKeys.get(orgId) : undefined;
      if (orgId !== null && orgKey !== undefined) {
        await grantOrgKey(orgId, member.userId, sealGrantTo(member.publicKey, orgKey));
      }
      setNotice(`shared this environment with ${member.userId}`);
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
