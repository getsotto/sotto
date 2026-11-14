import type { ReactNode } from "react";

// The page frame every surface shares: the typographic wordmark (Sotto ships no
// pictogram - the word is the mark) and, when a session exists, Log out.
export function Shell({
  onLogout,
  logoutPending = false,
  children,
}: {
  onLogout?: () => void;
  logoutPending?: boolean;
  children: ReactNode;
}) {
  return (
    <main>
      <header>
        <span className="wordmark">Sotto</span>
        {onLogout !== undefined && (
          <button className="ghost sm" onClick={onLogout} disabled={logoutPending}>
            {logoutPending ? "Logging out…" : "Log out"}
          </button>
        )}
      </header>
      {children}
    </main>
  );
}
