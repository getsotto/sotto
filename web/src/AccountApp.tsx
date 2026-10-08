import { useEffect, useState } from "react";

import { logout, me } from "./api";
import { CloudAccountPanel } from "./CloudAccountPanel";
import { startLogin } from "./login";
import { Shell } from "./Shell";

type Phase = "checking" | "loggedOut" | "signedIn" | "error";

export function AccountApp() {
  const [phase, setPhase] = useState<Phase>("checking");
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    void me()
      .then((user) => setPhase(user === null ? "loggedOut" : "signedIn"))
      .catch((e) => {
        setError(e instanceof Error ? e.message : String(e));
        setPhase("error");
      });
  }, []);

  async function doLogout() {
    setError(null);
    try {
      await logout();
      setPhase("loggedOut");
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }

  if (phase === "checking") {
    return <Shell><p className="muted">Loading your Cloud account…</p></Shell>;
  }
  if (phase === "error") {
    return <Shell><p role="alert">{error}</p><button className="primary" onClick={() => window.location.reload()}>Reload</button></Shell>;
  }
  if (phase === "loggedOut") {
    return <Shell><h1>Manage your Sotto Cloud account</h1><p className="muted">Sign in to view billing, recovery and export controls. Unlocking your vault is not required.</p><button className="primary" onClick={() => startLogin("/cloud")}>Log in with GitHub</button></Shell>;
  }
  return <Shell onLogout={() => void doLogout()}>{error !== null && <p role="alert">{error}</p>}<CloudAccountPanel /></Shell>;
}
