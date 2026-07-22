import { useState } from "react";

import { fetchShare, ShareUnavailable } from "./api";
import { urlSafeB64ToBytes } from "./base64";
import { Shell } from "./Shell";
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
      <Shell>
        <h1>Shared secret</h1>
        <p className="muted">
          This secret has now been viewed - copy it, it may not be available again.
        </p>
        <textarea
          className="secret-value"
          readOnly
          value={state.secret}
          rows={4}
          spellCheck={false}
        />
      </Shell>
    );
  }

  return (
    <Shell>
      <h1>You&rsquo;ve received a secret</h1>
      <p className="muted">
        It&rsquo;s end-to-end encrypted and may be one-time. Reveal it only when you&rsquo;re ready -
        opening it may consume the link.
      </p>
      <form
        className="stack"
        onSubmit={(e) => {
          e.preventDefault();
          void reveal();
        }}
      >
        <label>
          Passphrase (only if the sender set one)
          <input
            type="password"
            value={passphrase}
            onChange={(e) => setPassphrase(e.target.value)}
          />
        </label>
        <button className="primary" type="submit" disabled={state.kind === "revealing"}>
          {state.kind === "revealing" ? "Revealing…" : "Reveal secret"}
        </button>
      </form>
      {state.kind === "error" && <p role="alert">{state.message}</p>}
    </Shell>
  );
}
