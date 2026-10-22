// Begin the OAuth flow: the server sets an httpOnly cookie and redirects back to /auth/callback.
// Lives in its own module so the callback page can start a fresh flow after a failed one, instead
// of sending the user back to discover the login button themselves.
export const PENDING_CLOUD_CONTACT_TOKEN_KEY = "sotto_pending_cloud_contact_token";

export function startLogin(destination = "/app") {
  const state = crypto.randomUUID();
  const safeDestination = destination === "/cloud" ? "/cloud" : "/app";
  if (safeDestination === "/cloud") {
    const contactToken = new URLSearchParams(window.location.hash.slice(1)).get("contact_token");
    if (contactToken !== null && /^[0-9a-f]{64}$/i.test(contactToken)) {
      sessionStorage.setItem(PENDING_CLOUD_CONTACT_TOKEN_KEY, contactToken);
      window.history.replaceState(null, "", `${window.location.pathname}${window.location.search}`);
    }
  } else {
    sessionStorage.removeItem(PENDING_CLOUD_CONTACT_TOKEN_KEY);
  }
  sessionStorage.setItem("sotto_oauth_state", state);
  sessionStorage.setItem("sotto_oauth_destination", safeDestination);
  const redirect = `${window.location.origin}/auth/callback`;
  window.location.assign(
    `/auth/github/login?redirect_uri=${encodeURIComponent(redirect)}&state=${state}`,
  );
}
