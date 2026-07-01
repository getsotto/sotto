import { useEffect, useState } from "react";

import {
  createShare,
  fetchEnvironments,
  fetchProjects,
  fetchSecrets,
  type Environment,
  type Project,
} from "./api";
import { bytesToUrlSafeB64 } from "./base64";
import {
  decryptEnvName,
  decryptProjectName,
  decryptSecretName,
  decryptSecretValue,
  sealForShare,
  unwrapVaultKey,
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

export function VaultView({ master, onLogout }: { master: Uint8Array; onLogout: () => void }) {
  const [projects, setProjects] = useState<NamedProject[] | null>(null);
  const [envs, setEnvs] = useState<NamedEnv[] | null>(null);
  const [openEnv, setOpenEnv] = useState<OpenEnv | null>(null);
  const [revealed, setRevealed] = useState<Revealed | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    void (async () => {
      try {
        const rows = await fetchProjects();
        setProjects(
          rows.map((project) => ({
            project,
            name: nameOr(project.id, () => decryptProjectName(master, project.id, project.encName)),
          })),
        );
      } catch (e) {
        setError(message(e));
      }
    })();
  }, [master]);

  async function selectProject(np: NamedProject) {
    setError(null);
    setEnvs(null);
    setOpenEnv(null);
    setRevealed(null);
    try {
      const rows = await fetchEnvironments(np.project.id);
      setEnvs(
        rows.map((env) => ({ env, name: nameOr(env.id, () => decryptEnvName(master, env.id, env.encName)) })),
      );
    } catch (e) {
      setError(message(e));
    }
  }

  async function selectEnv(ne: NamedEnv) {
    setError(null);
    setRevealed(null);
    try {
      const vaultKey = unwrapVaultKey(master, ne.env.encVaultKey, ne.env.id);
      const secrets = (await fetchSecrets(ne.env.id))
        .filter((entry) => !entry.deleted)
        .map((entry) => ({ entry, name: decryptSecretName(vaultKey, ne.env.id, entry) }))
        .sort((a, b) => a.name.localeCompare(b.name));
      setOpenEnv({ envId: ne.env.id, vaultKey, secrets });
    } catch (e) {
      setError(message(e));
    }
  }

  function reveal(ns: NamedSecret) {
    if (openEnv === null) {
      return;
    }
    const value = decryptSecretValue(openEnv.vaultKey, openEnv.envId, ns.entry);
    setRevealed({ name: ns.name, value, link: null });
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
    </main>
  );
}
