import { useState } from "react";

import { fetchShare, ShareUnavailable } from "./api";
import { urlSafeB64ToBytes } from "./base64";
import { loadWasm, share_open, share_passphrase_key } from "./wasm";

type State =
  | { kind: "idle" }
  | { kind: "revealing" }
  | { kind: "revealed"; secret: string }
  | { kind: "error"; message: string };

export function RecipientPage({ token }: { token: string }) {
  const [state, setState] = useState<State>({ kind: "idle" });
  const [passphrase, setPassphrase] = useState("");

  // Runs once per click. The fetch burns a view, so it must not run on mount or a prefetch.
  async function reveal() {
    setState({ kind: "revealing" });
    try {
      const fragment = window.location.hash.slice(1);
      if (fragment === "") {
        throw new Error("this link is missing its decryption key");
      }
      const fragmentKey = urlSafeB64ToBytes(fragment);

      await loadWasm();
      const share = await fetchShare(token); // burns the view

      let aeadKey: Uint8Array;
      if (share.passphraseSalt !== null) {
        if (passphrase === "") {
          throw new Error("this link requires a passphrase");
        }
        aeadKey = share_passphrase_key(
          fragmentKey,
          new TextEncoder().encode(passphrase),
          share.passphraseSalt,
        );
      } else {
        aeadKey = fragmentKey;
      }

      const plaintext = share_open(aeadKey, share.encBlob);
      setState({ kind: "revealed", secret: new TextDecoder().decode(plaintext) });
    } catch (e) {
      const message =
        e instanceof ShareUnavailable
          ? e.message
          : `Couldn't reveal the secret: ${e instanceof Error ? e.message : String(e)}`;
      setState({ kind: "error", message });
    }
  }

  if (state.kind === "revealed") {
    return (
      <main>
        <h1>Shared secret</h1>
        <p>This secret has now been viewed — copy it, it may not be available again.</p>
        <textarea readOnly value={state.secret} rows={4} spellCheck={false} />
      </main>
    );
  }

  return (
    <main>
      <h1>You&rsquo;ve received a secret</h1>
      <p>
        It&rsquo;s end-to-end encrypted and may be one-time. Reveal it only when you&rsquo;re ready —
        opening it may consume the link.
      </p>
      <label>
        Passphrase (only if the sender set one)
        <input
          type="password"
          value={passphrase}
          onChange={(e) => setPassphrase(e.target.value)}
        />
      </label>
      <button onClick={() => void reveal()} disabled={state.kind === "revealing"}>
        {state.kind === "revealing" ? "Revealing…" : "Reveal secret"}
      </button>
      {state.kind === "error" && <p role="alert">{state.message}</p>}
    </main>
  );
}
