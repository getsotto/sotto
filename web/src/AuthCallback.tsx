import { useEffect, useState } from "react";

import { startLogin } from "./login";
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

  // The state we stored is consumed above and the callback is fail-closed, so a retry has to start
  // a brand new flow rather than reuse what is left in the URL or session storage.
  function retry() {
    window.history.replaceState(null, "", "/auth/callback");
    startLogin();
  }

  return (
    <Shell>
      {error !== null ? (
        <>
          <p role="alert">{error}</p>
          <button className="primary" type="button" onClick={retry}>
            Try again
          </button>
        </>
      ) : (
        <p className="muted">Signing you in…</p>
      )}
    </Shell>
  );
}
