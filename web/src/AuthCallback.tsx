import { useEffect, useState } from "react";

import { Shell } from "./Shell";

// After OAuth, the server has set the session cookie and redirected here with `?state=`. Verify it
// matches the value we stored (CSRF), then go to the app, which detects the session via /auth/me.
export function AuthCallback() {
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const returned = new URLSearchParams(window.location.search).get("state");
    const expected = sessionStorage.getItem("sotto_oauth_state");
    sessionStorage.removeItem("sotto_oauth_state");
    if (returned === null || returned !== expected) {
      setError("Login could not be verified (state mismatch).");
      return;
    }
    window.location.replace("/app");
  }, []);

  return (
    <Shell>
      {error !== null ? <p role="alert">{error}</p> : <p className="muted">Signing you in…</p>}
    </Shell>
  );
}
