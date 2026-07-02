// In-browser vault crypto — the same operations the CLI runs, via the WASM core.
//
// The master key is derived from the password + pasted secret key + account salt (Argon2id in
// WASM), held only in memory. It recovers the account keypair, which opens a per-environment
// vault-key grant, which decrypts each secret's name/value. Sealing a secret for a share link
// produces the ciphertext + the fragment key.

import {
  format_decode_key,
  kdf_derive_master_key,
  loadWasm,
  name_decrypt_env,
  name_decrypt_org,
  name_decrypt_project,
  share_seal,
  vault_decrypt_name,
  vault_decrypt_value,
  vault_grant_key,
  vault_open_grant,
  vault_rewrap_data_key,
} from "./wasm";

const TEXT = new TextEncoder();
const DEC = new TextDecoder();

export interface SecretEntry {
  id: string;
  encName: Uint8Array;
  encValue: Uint8Array;
  encDataKey: Uint8Array;
  version: number;
  deleted: boolean;
}

/// Derive the master key from the password, a pasted `SK1-…` secret key, and the account salt.
export async function deriveMasterKey(
  password: string,
  secretKey: string,
  salt: Uint8Array,
): Promise<Uint8Array> {
  await loadWasm();
  const secretKeyBytes = format_decode_key("SK", 1, secretKey.trim());
  return kdf_derive_master_key(TEXT.encode(password), secretKeyBytes, salt);
}

/// Open an environment's vault-key grant: recover the account keypair from the master-sealed
/// private keys, then unseal the grant. The private key never leaves WASM.
export function openEnvGrant(
  master: Uint8Array,
  encPrivateKeys: Uint8Array,
  grant: Uint8Array,
): Uint8Array {
  return vault_open_grant(master, encPrivateKeys, grant);
}

export function decryptSecretName(vaultKey: Uint8Array, envId: string, s: SecretEntry): string {
  return DEC.decode(vault_decrypt_name(vaultKey, envId, s.id, s.version, s.encName, s.encDataKey));
}

export function decryptSecretValue(vaultKey: Uint8Array, envId: string, s: SecretEntry): string {
  return DEC.decode(vault_decrypt_value(vaultKey, envId, s.id, s.version, s.encValue, s.encDataKey));
}

// Display names decrypt through the single-source scheme in `sotto_core::names` (WASM bindings).
// `key` is the org key for org resources, or the master key for personal ones; callers try the
// org key first and fall back to the master, then to showing the record id.
export function decryptProjectName(key: Uint8Array, projectId: string, encName: Uint8Array): string {
  return DEC.decode(name_decrypt_project(key, projectId, encName));
}

export function decryptEnvName(key: Uint8Array, envId: string, encName: Uint8Array): string {
  return DEC.decode(name_decrypt_env(key, envId, encName));
}

export function decryptOrgName(key: Uint8Array, orgId: string, encName: Uint8Array): string {
  return DEC.decode(name_decrypt_org(key, orgId, encName));
}

/// Rewrap one data key from the old vault key to the new one (rotation). Ciphertext untouched.
export function rewrapDataKey(
  oldVaultKey: Uint8Array,
  newVaultKey: Uint8Array,
  envId: string,
  secretId: string,
  version: number,
  encDataKey: Uint8Array,
): Uint8Array {
  return vault_rewrap_data_key(oldVaultKey, newVaultKey, envId, secretId, version, encDataKey);
}

/// Open the caller's sealed org-key copy (same sealed-box grant scheme as vault keys).
export function openOrgKey(
  master: Uint8Array,
  encPrivateKeys: Uint8Array,
  encOrgKey: Uint8Array,
): Uint8Array {
  return vault_open_grant(master, encPrivateKeys, encOrgKey);
}

/// Seal an environment's vault key to a member's public key — the grant uploaded when sharing.
export function sealGrantTo(memberPublicKey: Uint8Array, vaultKey: Uint8Array): Uint8Array {
  return vault_grant_key(memberPublicKey, vaultKey);
}

/// Seal a secret value for a share link. Returns the ciphertext to upload + the fragment key that
/// goes in the URL and never reaches the server.
export function sealForShare(value: string): { encBlob: Uint8Array; fragmentKey: Uint8Array } {
  const fragmentKey = crypto.getRandomValues(new Uint8Array(32));
  const encBlob = share_seal(fragmentKey, TEXT.encode(value));
  return { encBlob, fragmentKey };
}
