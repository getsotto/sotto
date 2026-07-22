import type { ReactNode } from "react";

// The page frame every surface shares: the typographic wordmark (Sotto ships no
// pictogram - the word is the mark) and, when a session exists, Log out.
export function Shell({ onLogout, children }: { onLogout?: () => void; children: ReactNode }) {
  return (
    <main>
      <header>
        <span className="wordmark">Sotto</span>
        {onLogout !== undefined && (
          <button className="ghost sm" onClick={onLogout}>
            Log out
          </button>
        )}
      </header>
      {children}
    </main>
  );
}
