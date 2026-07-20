import { AuthCallback } from "./AuthCallback";
import { Landing } from "./Landing";
import { RecipientPage } from "./RecipientPage";
import { VaultApp } from "./VaultApp";

// Minimal path routing (no router dependency):
//   /app            → the vault app (login → unlock → view secrets)
//   /s/:token       → the share recipient page (no account)
//   /auth/callback  → the post-OAuth landing (SPA; the API endpoints are proxied elsewhere)
//   / and the rest  → the landing page (the anonymous marketing surface)
function route():
  | { name: "landing" }
  | { name: "recipient"; token: string }
  | { name: "callback" }
  | { name: "vault" } {
  const path = window.location.pathname;
  if (path === "/app" || path === "/app/") {
    return { name: "vault" };
  }
  const share = /^\/s\/([^/]+)$/.exec(path);
  if (share !== null) {
    return { name: "recipient", token: decodeURIComponent(share[1]) };
  }
  if (path === "/auth/callback") {
    return { name: "callback" };
  }
  return { name: "landing" };
}

export function App() {
  const current = route();
  switch (current.name) {
    case "landing":
      return <Landing />;
    case "recipient":
      return <RecipientPage token={current.token} />;
    case "callback":
      return <AuthCallback />;
    case "vault":
      return <VaultApp />;
  }
}
