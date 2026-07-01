// Share-link fetch. Same-origin relative path (a Vite dev proxy points `/shares` at the API in
// dev; production is same-origin), so the CSP stays `connect-src 'self'`.
//
// The GET burns a view server-side, so callers must fetch exactly once, on an explicit user action.

import { standardB64ToBytes } from "./base64";

export interface Share {
  encBlob: Uint8Array;
  passphraseSalt: Uint8Array | null;
}

/// Thrown when the link is unusable (invalid, expired, revoked, or already viewed → 404).
export class ShareUnavailable extends Error {}

interface ShareResponse {
  enc_blob: string;
  passphrase_salt: string | null;
}

export async function fetchShare(token: string): Promise<Share> {
  const resp = await fetch(`/shares/${encodeURIComponent(token)}`);
  if (resp.status === 404) {
    throw new ShareUnavailable(
      "This link is invalid, expired, revoked, or has already been viewed.",
    );
  }
  if (!resp.ok) {
    throw new Error(`server error (${resp.status})`);
  }
  const body = (await resp.json()) as ShareResponse;
  return {
    encBlob: standardB64ToBytes(body.enc_blob),
    passphraseSalt: body.passphrase_salt ? standardB64ToBytes(body.passphrase_salt) : null,
  };
}
