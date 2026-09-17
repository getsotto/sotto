import { AuthCallback } from "./AuthCallback";
import { Landing } from "./Landing";
import { RecipientPage } from "./RecipientPage";
import { SeoPage } from "./seo/SeoPage";
import { guideBySlug, type SeoPageData } from "./seo/pages";
import { Shell } from "./Shell";
import { VaultApp } from "./VaultApp";

// Minimal path routing (no router dependency):
//   /app            → the vault app (login → unlock → view secrets)
//   /s/:token       → the share recipient page (no account)
//   /auth/callback  → the post-OAuth landing (SPA; the API endpoints are proxied elsewhere)
//   /<guide-slug>   → an indexable guide page (see web/src/seo/pages.ts)
//   / and the rest  → the landing page (the anonymous marketing surface)
function route():
  | { name: "landing" }
  | { name: "recipient"; token: string }
  | { name: "invalid-share" }
  | { name: "callback" }
  | { name: "guide"; page: SeoPageData }
  | { name: "vault" } {
  const path = window.location.pathname;
  if (path === "/app" || path === "/app/") {
    return { name: "vault" };
  }
  const share = /^\/s\/([^/]+)$/.exec(path);
  if (share !== null) {
    let token: string;
    try {
      token = decodeURIComponent(share[1]);
    } catch {
      // The segment is user-controlled; a malformed escape is a broken link,
      // not a reason to crash the router before the page can say so.
      return { name: "invalid-share" };
    }
    return { name: "recipient", token };
  }
  if (path === "/auth/callback") {
    return { name: "callback" };
  }
  // The edge also serves the prerendered files at their direct address
  // (/<slug>.html); strip the suffix so scripting visitors get the guide
  // React renders at the clean path, not the landing fallback.
  const guide = guideBySlug(path.replace(/^\/|\/$/g, "").replace(/\.html$/, ""));
  if (guide !== undefined) {
    return { name: "guide", page: guide };
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
    case "invalid-share":
      return (
        <Shell>
          <h1>This link is invalid</h1>
          <p role="alert" className="muted">
            The share link is malformed, so it can&rsquo;t be opened. Ask the sender for a new
            link.
          </p>
        </Shell>
      );
    case "callback":
      return <AuthCallback />;
    case "guide":
      return <SeoPage page={current.page} />;
    case "vault":
      return <VaultApp />;
  }
}
