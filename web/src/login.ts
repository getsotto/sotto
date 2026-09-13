// Begin the OAuth flow: the server sets an httpOnly cookie and redirects back to /auth/callback.
// Lives in its own module so the callback page can start a fresh flow after a failed one, instead
// of sending the user back to discover the login button themselves.
export function startLogin() {
  const state = crypto.randomUUID();
  sessionStorage.setItem("sotto_oauth_state", state);
  const redirect = `${window.location.origin}/auth/callback`;
  window.location.assign(
    `/auth/github/login?redirect_uri=${encodeURIComponent(redirect)}&state=${state}`,
  );
}
