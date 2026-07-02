// In-browser vault crypto — the same operations the CLI runs, via the WASM core.
//
// The master key is derived from the password + pasted secret key + account salt (Argon2id in
// WASM), held only in memory. It recovers the account keypair, which opens a per-environment
// vault-key grant, which decrypts each secret's name/value. Sealing a secret for a share link
// produces the ciphertext + the fragment key.

import {
  aead_open,
  format_decode_key,
  kdf_derive_master_key,
  loadWasm,
  share_seal,
  vault_decrypt_name,
  vault_decrypt_value,
  vault_grant_key,
  vault_open_grant,
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

// Project/environment names are encrypted under the master key with this AAD. NOTE: the format is
// mirrored from the CLI/server (`cli/remote/sync.rs`); moving that metadata-name crypto into
// `sotto-core` (like the vault crypto) is a documented follow-up so there is a single source.
export function decryptProjectName(master: Uint8Array, projectId: string, encName: Uint8Array): string {
  return DEC.decode(aead_open(master, encName, TEXT.encode(`sotto/v1/project-name|id=${projectId}`)));
}

export function decryptEnvName(master: Uint8Array, envId: string, encName: Uint8Array): string {
  return DEC.decode(aead_open(master, encName, TEXT.encode(`sotto/v1/env-name|id=${envId}`)));
}

/// Decrypt an org's name — like project names, sealed under the *creator's* master key, so for a
/// non-creator this throws and callers fall back to the org id (per-org keys are a follow-up).
export function decryptOrgName(master: Uint8Array, orgId: string, encName: Uint8Array): string {
  return DEC.decode(aead_open(master, encName, TEXT.encode(`sotto/v1/org-name|id=${orgId}`)));
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
