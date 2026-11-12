import { useCallback, useEffect, useRef, useState } from "react";

import { logout, me } from "./api";
import { CloudAccountPanel } from "./CloudAccountPanel";
import { startLogin } from "./login";
import { Shell } from "./Shell";

type Phase = "checking" | "loggedOut" | "signedIn" | "error";

export function AccountApp() {
  const [phase, setPhase] = useState<Phase>("checking");
  const [error, setError] = useState<string | null>(null);
  const [loggingOut, setLoggingOut] = useState(false);
  const requestId = useRef(0);

  const checkSession = useCallback(() => {
    const id = ++requestId.current;
    setError(null);
    setPhase("checking");
    void me()
      .then((user) => {
        if (id !== requestId.current) return;
        setPhase(user === null ? "loggedOut" : "signedIn");
      })
      .catch((e) => {
        if (id !== requestId.current) return;
        setError(e instanceof Error ? e.message : String(e));
        setPhase("error");
      });
  }, []);

  useEffect(() => {
    checkSession();
    return () => {
      requestId.current += 1;
    };
  }, [checkSession]);

  async function doLogout() {
    if (loggingOut) return;
    setLoggingOut(true);
    setError(null);
    try {
      await logout();
      setPhase("loggedOut");
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoggingOut(false);
    }
  }

  if (phase === "checking") {
    return <Shell><p className="muted">Loading your Cloud account…</p></Shell>;
  }
  if (phase === "error") {
    return (
      <Shell>
        <p role="alert">{error}</p>
        <button className="primary" onClick={checkSession}>Retry</button>
      </Shell>
    );
  }
  if (phase === "loggedOut") {
    return (
      <Shell>
        <h1>Manage your Sotto Cloud account</h1>
        <p className="muted">Sign in to view billing, recovery and export controls. Unlocking your vault is not required.</p>
        <button className="primary" onClick={() => startLogin("/cloud")}>Log in with GitHub</button>
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
