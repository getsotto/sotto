import { useEffect, useRef, useState } from "react";

import { logout, me } from "./api";
import { CloudAccountPanel } from "./CloudAccountPanel";
import { startLogin } from "./login";
import { Shell } from "./Shell";

type Phase = "checking" | "loggedOut" | "signedIn" | "error";

function message(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

export function AccountApp() {
  const [phase, setPhase] = useState<Phase>("checking");
  const [error, setError] = useState<string | null>(null);
  const [loggingOut, setLoggingOut] = useState(false);
  // Session checks can settle out of order after a retry or Strict Mode remount.
  const sessionLoad = useRef(0);

  async function checkSession() {
    const load = ++sessionLoad.current;
    setError(null);
    setPhase("checking");
    try {
      const user = await me();
      if (load !== sessionLoad.current) {
        return;
      }
      setPhase(user === null ? "loggedOut" : "signedIn");
    } catch (e) {
      if (load !== sessionLoad.current) {
        return;
      }
      setError(message(e));
      setPhase("error");
    }
  }

  useEffect(() => {
    void checkSession();
    return () => {
      ++sessionLoad.current;
    };
  }, []);

  function retrySession() {
    if (phase === "checking") {
      return;
    }
    void checkSession();
  }

  async function doLogout() {
    if (loggingOut) return;
    setLoggingOut(true);
    setError(null);
    try {
      await logout();
      setPhase("loggedOut");
    } catch (e) {
      setError(message(e));
    } finally {
      setLoggingOut(false);
    }
  }

  if (phase === "checking") {
    return (
      <Shell>
        <p className="muted" role="status">
          Loading your Cloud account…
        </p>
      </Shell>
    );
  }
  if (phase === "error") {
    return (
      <Shell>
        <p role="alert">{error}</p>
        <button className="primary" type="button" onClick={retrySession}>
          Retry session
        </button>
      </Shell>
    );
  }
  if (phase === "loggedOut") {
    return (
      <Shell>
        <h1>Manage your Sotto Cloud account</h1>
        <p className="muted">
          Sign in to view billing, recovery and export controls. Unlocking your vault is not required.
        </p>
        <button className="primary" onClick={() => startLogin("/cloud")}>
          Log in with GitHub
        </button>
      </Shell>
    );
  }
  return (
    <Shell onLogout={() => void doLogout()} logoutPending={loggingOut}>
      {error !== null && <p role="alert">{error}</p>}
      <CloudAccountPanel />
    </Shell>
  );
}
