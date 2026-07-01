import { Home } from "./Home";
import { RecipientPage } from "./RecipientPage";

// Minimal path routing (no router dependency): `/s/:token` is the share recipient page.
function route(): { name: "recipient"; token: string } | { name: "home" } {
  const match = /^\/s\/([^/]+)$/.exec(window.location.pathname);
  if (match) {
    return { name: "recipient", token: decodeURIComponent(match[1]) };
  }
  return { name: "home" };
}

export function App() {
  const current = route();
  return current.name === "recipient" ? (
    <RecipientPage token={current.token} />
  ) : (
    <Home />
  );
}
