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

export interface Account {
  /// KDF salt, needed to derive the master key.
  salt: Uint8Array;
  /// The account's X25519 private keys, sealed under the master key. Opening a vault-key grant
  /// needs the recovered private key, so the browser fetches this ciphertext alongside the salt.
  encPrivateKeys: Uint8Array;
}

/// The account's KDF salt + master-sealed private keys, or `null` if the account isn't set up.
export async function fetchAccount(): Promise<Account | null> {
  const resp = await fetch("/account", CREDS);
  if (resp.status === 404) {
    return null;
  }
  if (!resp.ok) {
    throw new Error(`server error (${resp.status})`);
  }
  const body = (await resp.json()) as { kdf_params: string; enc_private_keys: string };
  const kdf = JSON.parse(new TextDecoder().decode(standardB64ToBytes(body.kdf_params))) as {
    salt: number[];
  };
  return {
    salt: new Uint8Array(kdf.salt),
    encPrivateKeys: standardB64ToBytes(body.enc_private_keys),
  };
}

export interface Project {
  id: string;
  encName: Uint8Array;
  /// Owning organization, or `null` for a personal project (team actions apply only when set).
  orgId: string | null;
}

export async function fetchProjects(): Promise<Project[]> {
  const rows = await authedJson<{ id: string; enc_name: string; org_id: string | null }[]>(
    "/projects",
  );
  return rows.map((r) => ({
    id: r.id,
    encName: standardB64ToBytes(r.enc_name),
    orgId: r.org_id,
  }));
}

export interface Environment {
  id: string;
  encName: Uint8Array;
  /// The caller's OWN vault-key grant, or `null` if they hold none for this environment.
  encVaultKey: Uint8Array | null;
}

export async function fetchEnvironments(projectId: string): Promise<Environment[]> {
  const rows = await authedJson<
    { id: string; enc_name: string; enc_vault_key: string | null }[]
  >(`/projects/${encodeURIComponent(projectId)}/environments`);
  return rows.map((r) => ({
    id: r.id,
    encName: standardB64ToBytes(r.enc_name),
    encVaultKey: r.enc_vault_key ? standardB64ToBytes(r.enc_vault_key) : null,
  }));
}

/// The caller's own vault-key grant for an environment, or `null` if they have none (access
/// without a key: the org lets them see ciphertext, but nobody granted them the vault key).
export async function fetchMyGrant(envId: string): Promise<Uint8Array | null> {
  const resp = await fetch(`/environments/${encodeURIComponent(envId)}/grant`, CREDS);
  if (resp.status === 404) {
    return null;
  }
  if (!resp.ok) {
    throw new Error(`server error (${resp.status})`);
  }
  const body = (await resp.json()) as { enc_vault_key: string };
  return standardB64ToBytes(body.enc_vault_key);
}

export interface Org {
  id: string;
  encName: Uint8Array;
  /// The caller's own role in this org.
  role: string;
  /// The org key sealed to the caller, or `null` if not granted (names fall back to ids).
  encOrgKey: Uint8Array | null;
}

export async function fetchOrgs(): Promise<Org[]> {
  const rows = await authedJson<
    { id: string; enc_name: string; role: string; enc_org_key: string | null }[]
  >("/orgs");
  return rows.map((r) => ({
    id: r.id,
    encName: standardB64ToBytes(r.enc_name),
    role: r.role,
    encOrgKey: r.enc_org_key ? standardB64ToBytes(r.enc_org_key) : null,
  }));
}

/// Store (or replace) a member's sealed copy of the org key.
export async function grantOrgKey(
  orgId: string,
  userId: string,
  encOrgKey: Uint8Array,
): Promise<void> {
  const resp = await fetch(
    `/orgs/${encodeURIComponent(orgId)}/members/${encodeURIComponent(userId)}/org-key`,
    {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ enc_org_key: bytesToStandardB64(encOrgKey) }),
      ...CREDS,
    },
  );
  if (!resp.ok) {
    throw new Error(`server error (${resp.status})`);
  }
}

export interface Member {
  userId: string;
  role: string;
  /// The member's public key (base64 kept raw for sealing), or `null` if not set up yet.
  publicKey: Uint8Array | null;
}

export async function fetchMembers(orgId: string): Promise<Member[]> {
  const rows = await authedJson<{ user_id: string; role: string; public_key: string | null }[]>(
    `/orgs/${encodeURIComponent(orgId)}/members`,
  );
  return rows.map((r) => ({
    userId: r.user_id,
    role: r.role,
    publicKey: r.public_key ? standardB64ToBytes(r.public_key) : null,
  }));
}

export interface InvitedMember {
  userId: string;
  /// Their public key (for sealing the org key to them), or `null` if they haven't set up yet.
  publicKey: Uint8Array | null;
}

/// Invite an existing Sotto user into an org by email.
export async function inviteMember(orgId: string, email: string): Promise<InvitedMember> {
  const resp = await fetch(`/orgs/${encodeURIComponent(orgId)}/invites`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ email }),
    ...CREDS,
  });
  if (resp.status === 404) {
    throw new Error("no Sotto user with that email — they must sign up first");
  }
  if (resp.status === 409) {
    throw new Error("that user is already a member");
  }
  if (!resp.ok) {
    throw new Error(`server error (${resp.status})`);
  }
  const body = (await resp.json()) as { user_id: string; public_key: string | null };
  return {
    userId: body.user_id,
    publicKey: body.public_key ? standardB64ToBytes(body.public_key) : null,
  };
}

/// Store a member's vault-key grant for an environment (sharing).
export async function createGrant(
  envId: string,
  userId: string,
  encVaultKey: Uint8Array,
): Promise<void> {
  const resp = await fetch(`/environments/${encodeURIComponent(envId)}/grants`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ user_id: userId, enc_vault_key: bytesToStandardB64(encVaultKey) }),
    ...CREDS,
  });
  if (resp.status === 403) {
    throw new Error("only an admin or owner can share this environment");
  }
  if (!resp.ok) {
    throw new Error(`server error (${resp.status})`);
  }
}

export interface Snapshot {
  revision: number;
  secrets: SecretEntry[];
}

/// The full snapshot including its revision (rotation writes against this as `base_revision`).
export async function fetchSnapshot(envId: string): Promise<Snapshot> {
  const snap = await authedJson<{
    revision: number;
    secrets: {
      id: string;
      enc_name: string;
      enc_value: string;
      enc_data_key: string;
      version: number;
      deleted: boolean;
    }[];
  }>(`/environments/${encodeURIComponent(envId)}/secrets`);
  return {
    revision: snap.revision,
    secrets: snap.secrets.map((s) => ({
      id: s.id,
      encName: standardB64ToBytes(s.enc_name),
      encValue: standardB64ToBytes(s.enc_value),
      encDataKey: standardB64ToBytes(s.enc_data_key),
      version: s.version,
      deleted: s.deleted,
    })),
  };
}

export async function fetchSecrets(envId: string): Promise<SecretEntry[]> {
  return (await fetchSnapshot(envId)).secrets;
}

export interface HistoryRow {
  secretId: string;
  version: number;
  encDataKey: Uint8Array;
}

/// Every retained history version's (secret, version, data key) — rotation must rewrap them all.
export async function fetchHistory(envId: string): Promise<HistoryRow[]> {
  const rows = await authedJson<{ secret_id: string; version: number; enc_data_key: string }[]>(
    `/environments/${encodeURIComponent(envId)}/history`,
  );
  return rows.map((r) => ({
    secretId: r.secret_id,
    version: r.version,
    encDataKey: standardB64ToBytes(r.enc_data_key),
  }));
}

/// The user ids currently granted an environment (rotation re-grants exactly these).
export async function fetchGrantHolders(envId: string): Promise<string[]> {
  const rows = await authedJson<{ user_id: string }[]>(
    `/environments/${encodeURIComponent(envId)}/grants`,
  );
  return rows.map((r) => r.user_id);
}

export interface MachineTokenInfo {
  tokenId: string;
  name: string;
  publicKey: Uint8Array;
}

/// The environment's active machine tokens (rotation re-seals the new key to each).
export async function fetchMachineTokens(envId: string): Promise<MachineTokenInfo[]> {
  const rows = await authedJson<{ token_id: string; name: string; public_key: string }[]>(
    `/environments/${encodeURIComponent(envId)}/tokens`,
  );
  return rows.map((r) => ({
    tokenId: r.token_id,
    name: r.name,
    publicKey: standardB64ToBytes(r.public_key),
  }));
}

export interface RotatePayload {
  baseRevision: number;
  grants: { userId: string; encVaultKey: Uint8Array }[];
  dataKeys: { secretId: string; encDataKey: Uint8Array }[];
  machineGrants: { tokenId: string; encVaultKey: Uint8Array }[];
  historyKeys: { secretId: string; version: number; encDataKey: Uint8Array }[];
}

/// Apply a key rotation (rewrapped keys + the replacement grant set) at a base revision.
export async function postRotate(envId: string, payload: RotatePayload): Promise<void> {
  const resp = await fetch(`/environments/${encodeURIComponent(envId)}/rotate`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      base_revision: payload.baseRevision,
      grants: payload.grants.map((g) => ({
        user_id: g.userId,
        enc_vault_key: bytesToStandardB64(g.encVaultKey),
      })),
      data_keys: payload.dataKeys.map((d) => ({
        secret_id: d.secretId,
        enc_data_key: bytesToStandardB64(d.encDataKey),
      })),
      machine_grants: payload.machineGrants.map((m) => ({
        token_id: m.tokenId,
        enc_vault_key: bytesToStandardB64(m.encVaultKey),
      })),
      history_keys: payload.historyKeys.map((h) => ({
        secret_id: h.secretId,
        version: h.version,
        enc_data_key: bytesToStandardB64(h.encDataKey),
      })),
    }),
    ...CREDS,
  });
  if (resp.status === 412) {
    throw new Error("the environment changed while rotating — try again");
  }
  if (resp.status === 403) {
    throw new Error("only an admin or owner can rotate this environment");
  }
  if (!resp.ok) {
    throw new Error(`server error (${resp.status})`);
  }
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
