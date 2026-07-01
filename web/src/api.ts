// Share-link fetch. Same-origin relative path (a Vite dev proxy points `/shares` at the API in
// dev; production is same-origin), so the CSP stays `connect-src 'self'`.
//
// The GET burns a view server-side, so callers must fetch exactly once, on an explicit user action.

import { bytesToStandardB64, standardB64ToBytes } from "./base64";
import type { SecretEntry } from "./vault";

// Authed requests send the httpOnly session cookie. Same-origin (dev proxy) keeps CSP tight.
const CREDS: RequestInit = { credentials: "include" };

async function authedJson<T>(path: string): Promise<T> {
  const resp = await fetch(path, CREDS);
  if (!resp.ok) {
    throw new Error(`server error (${resp.status})`);
  }
  return (await resp.json()) as T;
}

/// The current session's user, or `null` if not logged in.
export async function me(): Promise<{ userId: string } | null> {
  const resp = await fetch("/auth/me", CREDS);
  if (resp.status === 401) {
    return null;
  }
  if (!resp.ok) {
    throw new Error(`server error (${resp.status})`);
  }
  const body = (await resp.json()) as { user_id: string };
  return { userId: body.user_id };
}

export async function logout(): Promise<void> {
  const resp = await fetch("/auth/logout", { method: "POST", ...CREDS });
  if (!resp.ok) {
    // The session cookie is httpOnly, so only the server can clear it; report failure rather than
    // letting callers assume the session is gone.
    throw new Error(`server error (${resp.status})`);
  }
}

/// The account KDF salt (needed to derive the master key), or `null` if the account isn't set up.
export async function fetchAccountSalt(): Promise<Uint8Array | null> {
  const resp = await fetch("/account", CREDS);
  if (resp.status === 404) {
    return null;
  }
  if (!resp.ok) {
    throw new Error(`server error (${resp.status})`);
  }
  const body = (await resp.json()) as { kdf_params: string };
  const kdf = JSON.parse(new TextDecoder().decode(standardB64ToBytes(body.kdf_params))) as {
    salt: number[];
  };
  return new Uint8Array(kdf.salt);
}

export interface Project {
  id: string;
  encName: Uint8Array;
}

export async function fetchProjects(): Promise<Project[]> {
  const rows = await authedJson<{ id: string; enc_name: string }[]>("/projects");
  return rows.map((r) => ({ id: r.id, encName: standardB64ToBytes(r.enc_name) }));
}

export interface Environment {
  id: string;
  encName: Uint8Array;
  encVaultKey: Uint8Array;
}

export async function fetchEnvironments(projectId: string): Promise<Environment[]> {
  const rows = await authedJson<{ id: string; enc_name: string; enc_vault_key: string }[]>(
    `/projects/${encodeURIComponent(projectId)}/environments`,
  );
  return rows.map((r) => ({
    id: r.id,
    encName: standardB64ToBytes(r.enc_name),
    encVaultKey: standardB64ToBytes(r.enc_vault_key),
  }));
}

export async function fetchSecrets(envId: string): Promise<SecretEntry[]> {
  const snap = await authedJson<{
    secrets: {
      id: string;
      enc_name: string;
      enc_value: string;
      enc_data_key: string;
      version: number;
      deleted: boolean;
    }[];
  }>(`/environments/${encodeURIComponent(envId)}/secrets`);
  return snap.secrets.map((s) => ({
    id: s.id,
    encName: standardB64ToBytes(s.enc_name),
    encValue: standardB64ToBytes(s.enc_value),
    encDataKey: standardB64ToBytes(s.enc_data_key),
    version: s.version,
    deleted: s.deleted,
  }));
}

/// Create a share link (session required); returns the public token.
export async function createShare(encBlob: Uint8Array, maxViews: number): Promise<string> {
  const resp = await fetch("/shares", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ enc_blob: bytesToStandardB64(encBlob), max_views: maxViews }),
    ...CREDS,
  });
  if (!resp.ok) {
    throw new Error(`server error (${resp.status})`);
  }
  const body = (await resp.json()) as { token: string };
  return body.token;
}

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
