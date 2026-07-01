import { AuthCallback } from "./AuthCallback";
import { RecipientPage } from "./RecipientPage";
import { VaultApp } from "./VaultApp";

// Minimal path routing (no router dependency):
//   /s/:token       → the share recipient page (no account)
//   /auth/callback  → the post-OAuth landing (SPA; the API endpoints are proxied elsewhere)
//   everything else → the vault app (login → unlock → view secrets)
function route():
  | { name: "recipient"; token: string }
  | { name: "callback" }
  | { name: "vault" } {
  const path = window.location.pathname;
  const share = /^\/s\/([^/]+)$/.exec(path);
  if (share !== null) {
    return { name: "recipient", token: decodeURIComponent(share[1]) };
  }
  if (path === "/auth/callback") {
    return { name: "callback" };
  }
  return { name: "vault" };
}

export function App() {
  const current = route();
  switch (current.name) {
    case "recipient":
      return <RecipientPage token={current.token} />;
    case "callback":
      return <AuthCallback />;
    case "vault":
      return <VaultApp />;
  }
}
