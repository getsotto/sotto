import { useEffect, useState } from "react";

import { fetchAccount, logout, me } from "./api";
import { Shell } from "./Shell";
import { deriveMasterKey } from "./vault";
import { VaultView } from "./VaultView";

type Phase =
  | { kind: "checking" }
  | { kind: "error"; message: string }
  | { kind: "loggedOut" }
  | { kind: "locked"; salt: Uint8Array; encPrivateKeys: Uint8Array }
  | { kind: "unlocked"; master: Uint8Array; encPrivateKeys: Uint8Array };

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
        const account = await fetchAccount();
        if (account === null) {
          setPhase({
            kind: "error",
            message: "No account found — set up Sotto with the CLI first.",
          });
          return;
        }
        setPhase({ kind: "locked", salt: account.salt, encPrivateKeys: account.encPrivateKeys });
      } catch (e) {
        setPhase({ kind: "error", message: e instanceof Error ? e.message : String(e) });
      }
    })();
  }, []);

  function doLogout() {
    void (async () => {
      try {
        await logout();
        setPhase({ kind: "loggedOut" });
      } catch (e) {
        // The server couldn't clear the session — don't pretend we're logged out.
        setPhase({ kind: "error", message: e instanceof Error ? e.message : String(e) });
      }
    })();
  }

  switch (phase.kind) {
    case "checking":
      return (
        <Shell>
          <p className="muted">Loading…</p>
        </Shell>
      );
    case "error":
      return (
        <Shell>
          <p role="alert">{phase.message}</p>
        </Shell>
      );
    case "loggedOut":
      return (
        <Shell>
          <h1>Log in to view your secrets in the browser.</h1>
          <p className="muted">
            The web client runs the same crypto core as the CLI, via WebAssembly. Your keys never
            leave this browser.
          </p>
          <button className="primary" onClick={startLogin}>
            Log in with GitHub
          </button>
        </Shell>
      );
    case "locked":
      return (
        <UnlockForm
          salt={phase.salt}
          onUnlock={(master) =>
            setPhase({ kind: "unlocked", master, encPrivateKeys: phase.encPrivateKeys })
          }
          onLogout={doLogout}
        />
      );
    case "unlocked":
      return (
        <VaultView
          master={phase.master}
          encPrivateKeys={phase.encPrivateKeys}
          onLogout={doLogout}
        />
      );
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
    <Shell onLogout={onLogout}>
      <h1>Unlock your vault</h1>
      <p className="muted">
        Your master password and secret key stay in this browser — the server never sees them.
      </p>
      <form
        className="stack"
        onSubmit={(e) => {
          e.preventDefault();
          void unlock();
        }}
      >
        <label>
          Master password
          <input type="password" value={password} onChange={(e) => setPassword(e.target.value)} />
        </label>
        <label>
          Secret key (SK1-…)
          <input type="password" value={secretKey} onChange={(e) => setSecretKey(e.target.value)} />
        </label>
        <button className="primary" type="submit" disabled={busy}>
          {busy ? "Deriving key…" : "Unlock"}
        </button>
      </form>
      {error !== null && <p role="alert">{error}</p>}
    </Shell>
  );
}
