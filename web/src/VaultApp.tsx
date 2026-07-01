import { useEffect, useState } from "react";

import { fetchAccountSalt, logout, me } from "./api";
import { deriveMasterKey } from "./vault";
import { VaultView } from "./VaultView";

type Phase =
  | { kind: "checking" }
  | { kind: "error"; message: string }
  | { kind: "loggedOut" }
  | { kind: "locked"; salt: Uint8Array }
  | { kind: "unlocked"; master: Uint8Array };

// Begin the OAuth flow: the server sets an httpOnly cookie and redirects back to /auth/callback.
function startLogin() {
  const state = crypto.randomUUID();
  sessionStorage.setItem("sotto_oauth_state", state);
  const redirect = `${window.location.origin}/auth/callback`;
  window.location.assign(
    `/auth/github/login?redirect_uri=${encodeURIComponent(redirect)}&state=${state}`,
  );
}

export function VaultApp() {
  const [phase, setPhase] = useState<Phase>({ kind: "checking" });

  useEffect(() => {
    void (async () => {
      try {
        const user = await me();
        if (user === null) {
          setPhase({ kind: "loggedOut" });
          return;
        }
        const salt = await fetchAccountSalt();
        if (salt === null) {
          setPhase({
            kind: "error",
            message: "No account found — set up Sotto with the CLI first.",
          });
          return;
        }
        setPhase({ kind: "locked", salt });
      } catch (e) {
        setPhase({ kind: "error", message: e instanceof Error ? e.message : String(e) });
      }
    })();
  }, []);

  function doLogout() {
    void logout().finally(() => setPhase({ kind: "loggedOut" }));
  }

  switch (phase.kind) {
    case "checking":
      return (
        <main>
          <p>Loading…</p>
        </main>
      );
    case "error":
      return (
        <main>
          <h1>Sotto</h1>
          <p role="alert">{phase.message}</p>
        </main>
      );
    case "loggedOut":
      return (
        <main>
          <h1>Sotto</h1>
          <p>Log in to view your secrets in the browser.</p>
          <button onClick={startLogin}>Log in with GitHub</button>
        </main>
      );
    case "locked":
      return (
        <UnlockForm
          salt={phase.salt}
          onUnlock={(master) => setPhase({ kind: "unlocked", master })}
          onLogout={doLogout}
        />
      );
    case "unlocked":
      return <VaultView master={phase.master} onLogout={doLogout} />;
  }
}

function UnlockForm({
  salt,
  onUnlock,
  onLogout,
}: {
  salt: Uint8Array;
  onUnlock: (master: Uint8Array) => void;
  onLogout: () => void;
}) {
  const [password, setPassword] = useState("");
  const [secretKey, setSecretKey] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function unlock() {
    setBusy(true);
    setError(null);
    try {
      onUnlock(await deriveMasterKey(password, secretKey, salt));
    } catch {
      setError("Couldn't unlock — check your master password and secret key.");
      setBusy(false);
    }
  }

  return (
    <main>
      <h1>Unlock your vault</h1>
      <p>Your master password and secret key stay in this browser — the server never sees them.</p>
      <label>
        Master password
        <input type="password" value={password} onChange={(e) => setPassword(e.target.value)} />
      </label>
      <label>
        Secret key (SK1-…)
        <input type="password" value={secretKey} onChange={(e) => setSecretKey(e.target.value)} />
      </label>
      <button onClick={() => void unlock()} disabled={busy}>
        {busy ? "Deriving key…" : "Unlock"}
      </button>
      <button onClick={onLogout}>Log out</button>
      {error !== null && <p role="alert">{error}</p>}
    </main>
  );
}
