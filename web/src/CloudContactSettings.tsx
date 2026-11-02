import { useEffect, useRef, useState } from "react";

import {
  confirmCloudContactVerification,
  fetchCloudContactStatus,
  revokeCloudContact,
  startCloudContactVerification,
  UnsupportedCloudContactError,
  type CloudContactStatus,
} from "./api";

function message(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function deliveryMessage(state: NonNullable<CloudContactStatus["latestDelivery"]>["state"]): string {
  switch (state) {
    case "queued": return "The latest lifecycle email is queued.";
    case "accepted": return "The email service accepted the latest lifecycle email; delivery is not yet confirmed.";
    case "delivery_unknown": return "We could not confirm whether the latest lifecycle email was sent. Check your inbox before requesting help.";
    case "delivered": return "The email service confirmed delivery of the latest lifecycle email.";
    case "failed": return "The latest lifecycle email could not be sent.";
    case "bounced": return "The latest lifecycle email was returned by the recipient mail server. Verify a different address to resume email notices.";
    case "suppressed": return "Email delivery is paused because the address could not accept a previous message.";
  }
}

/** Contact management is available on the signed-in Cloud page without unlocking the vault. */
export function CloudContactSettings() {
  const [status, setStatus] = useState<CloudContactStatus | "unsupported" | "error" | null>(null);
  const [email, setEmail] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const requestKey = useRef<string | null>(null);

  async function refresh() {
    setError(null);
    try {
      setStatus(await fetchCloudContactStatus());
    } catch (cause) {
      if (cause instanceof UnsupportedCloudContactError) {
        setStatus("unsupported");
        return;
      }
      setStatus("error");
      setError(message(cause));
    }
  }

  useEffect(() => {
    const fragment = new URLSearchParams(window.location.hash.slice(1));
    const token = fragment.get("contact_token");
    if (token === null) {
      void refresh();
      return;
    }

    // Remove the bearer-like one-use value from the visible URL and browser history before the
    // authenticated POST. The fragment is never sent in the HTTP request or Referer header.
    window.history.replaceState(null, "", `${window.location.pathname}${window.location.search}`);
    setBusy(true);
    void confirmCloudContactVerification(token)
      .then(async (verified) => {
        // Avoid racing this fresh status with a request started before confirmation.
        await refresh();
        setNotice(verified ? "Email address verified." : "This verification link is invalid or has expired.");
      })
      .catch(async (cause) => {
        await refresh();
        if (cause instanceof UnsupportedCloudContactError) {
          setNotice(cause.message);
        } else {
          setError(message(cause));
        }
      })
      .finally(() => setBusy(false));
  }, []);

  async function start() {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      requestKey.current ??= crypto.randomUUID();
      await startCloudContactVerification(email, requestKey.current);
      requestKey.current = null;
      setNotice("If this address can be used, a verification message will arrive shortly.");
      setEmail("");
      await refresh();
    } catch (cause) {
      if (cause instanceof UnsupportedCloudContactError) setNotice(cause.message);
      else setError(message(cause));
    } finally {
      setBusy(false);
    }
  }

  async function revoke() {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      await revokeCloudContact();
      setNotice("Lifecycle email contact removed.");
      await refresh();
    } catch (cause) {
      if (cause instanceof UnsupportedCloudContactError) setNotice(cause.message);
      else setError(message(cause));
    } finally {
      setBusy(false);
    }
  }

  return (
    <section aria-labelledby="contact-heading">
      <h2 id="contact-heading">Lifecycle email</h2>
      <p>Use an email address for important Cloud account notices. It is optional and does not affect your vault access.</p>
      {error !== null && <p role="alert">{error}</p>}
      {notice !== null && <p role="status">{notice}</p>}
      {status === null ? <p className="muted" role="status">Checking contact settings…</p>
        : status === "unsupported" ? <p className="muted">Email contact settings require a newer Sotto server version.</p>
        : status === "error" ? <><p className="muted">Contact settings could not be loaded.</p><button type="button" onClick={() => void refresh()}>Retry</button></>
        : <>
        {status.email === null
          ? <p>No verified email contact is set.</p>
          : <p>Verified contact: <strong>{status.email.maskedDestination}</strong></p>}
        {status.latestDelivery != null && <p className="muted" role="status">
          {deliveryMessage(status.latestDelivery.state)}
        </p>}
        {status.pendingExpiresAtEpoch !== null && <p className="muted">A verification request is pending. You can request another link if it expires.</p>}
        {status.enrolmentAvailable ? <form
          noValidate
          onSubmit={(event) => {
            event.preventDefault();
            if (!busy && email.trim() !== "") void start();
          }}
        >
          <label htmlFor="lifecycle-email">Email address</label>{" "}
          <input
            id="lifecycle-email"
            type="email"
            autoComplete="email"
            value={email}
            maxLength={320}
            onChange={(event) => { requestKey.current = null; setEmail(event.target.value); }}
            disabled={busy}
          />{" "}
          <button type="submit" disabled={busy || email.trim() === ""}>
            {status.pendingExpiresAtEpoch === null ? "Send verification link" : "Resend verification link"}
          </button>
        </form> : <p className="muted">Email contact setup is not enabled on this instance.</p>}
        {status.email !== null && <button type="button" disabled={busy} onClick={() => void revoke()}>
          Remove verified email
        </button>}
      </>}
    </section>
  );
}
