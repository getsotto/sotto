import { useEffect, useState } from "react";

import { loadWasm, scheme_version } from "./wasm";

export function App() {
  const [version, setVersion] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    loadWasm()
      .then(() => setVersion(scheme_version()))
      .catch((e: unknown) => setError(String(e)));
  }, []);

  return (
    <main>
      <h1>Sotto</h1>
      {error !== null ? (
        <p>Failed to load the crypto core: {error}</p>
      ) : version !== null ? (
        <p>Crypto core loaded — scheme v{version}.</p>
      ) : (
        <p>Loading crypto core…</p>
      )}
    </main>
  );
}
