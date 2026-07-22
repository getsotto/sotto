//! Metadata *display-name* encryption - org, project, and environment names.
//!
//! Names are the one piece of user-readable metadata the server stores; they are sealed with the
//! same AEAD as everything else, with AAD binding each name to its record id so ciphertexts can't
//! be swapped between records. This module is the single source for that scheme: the CLI (native)
//! and the web client (WASM bindings) both call it, so the AAD strings live only here.
//!
//! **Which key?** Personal projects/environments encrypt names under the owner's master key.
//! Org-owned resources (and org names themselves) encrypt under the **org key** - a symmetric key
//! sealed grant-style to every member (see `vault::grant_vault_key`) - so each member can read
//! them. Decryption is fallback-based, not versioned: callers try the org key, then the master
//! key, then fall back to displaying the record id; the AEAD's authentication decides which key
//! (if any) is right.

use crate::aead;
use crate::error::Error;

/// Symmetric key length (org key or master key).
pub const KEY_LEN: usize = 32;

fn org_aad(id: &str) -> String {
    format!("sotto/v1/org-name|id={id}")
}
fn project_aad(id: &str) -> String {
    format!("sotto/v1/project-name|id={id}")
}
fn env_aad(id: &str) -> String {
    format!("sotto/v1/env-name|id={id}")
}

/// Encrypt an organisation's name under the org key.
pub fn encrypt_org_name(key: &[u8; KEY_LEN], org_id: &str, name: &[u8]) -> Vec<u8> {
    aead::seal(key, name, org_aad(org_id).as_bytes())
}

/// Decrypt an organisation's name.
pub fn decrypt_org_name(
    key: &[u8; KEY_LEN],
    org_id: &str,
    ciphertext: &[u8],
) -> Result<Vec<u8>, Error> {
    aead::open(key, ciphertext, org_aad(org_id).as_bytes())
}

/// Encrypt a project's name (master key for personal projects, org key for org projects).
pub fn encrypt_project_name(key: &[u8; KEY_LEN], project_id: &str, name: &[u8]) -> Vec<u8> {
    aead::seal(key, name, project_aad(project_id).as_bytes())
}

/// Decrypt a project's name.
pub fn decrypt_project_name(
    key: &[u8; KEY_LEN],
    project_id: &str,
    ciphertext: &[u8],
) -> Result<Vec<u8>, Error> {
    aead::open(key, ciphertext, project_aad(project_id).as_bytes())
}

/// Encrypt an environment's name (master key for personal projects, org key for org projects).
pub fn encrypt_env_name(key: &[u8; KEY_LEN], env_id: &str, name: &[u8]) -> Vec<u8> {
    aead::seal(key, name, env_aad(env_id).as_bytes())
}

/// Decrypt an environment's name.
pub fn decrypt_env_name(
    key: &[u8; KEY_LEN],
    env_id: &str,
    ciphertext: &[u8],
) -> Result<Vec<u8>, Error> {
    aead::open(key, ciphertext, env_aad(env_id).as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::random;

    #[test]
    fn names_round_trip_and_bind_their_record() {
        let key: [u8; KEY_LEN] = random::bytes();
        let ct = encrypt_project_name(&key, "p1", b"acme-api");
        assert_eq!(decrypt_project_name(&key, "p1", &ct).unwrap(), b"acme-api");
        // Bound to the record id and the record *kind*: neither moves.
        assert!(decrypt_project_name(&key, "p2", &ct).is_err());
        assert!(decrypt_env_name(&key, "p1", &ct).is_err());
        assert!(decrypt_org_name(&key, "p1", &ct).is_err());
    }

    #[test]
    fn env_and_org_names_round_trip() {
        let key: [u8; KEY_LEN] = random::bytes();
        let env = encrypt_env_name(&key, "e1", b"prod");
        assert_eq!(decrypt_env_name(&key, "e1", &env).unwrap(), b"prod");
        let org = encrypt_org_name(&key, "o1", b"acme");
        assert_eq!(decrypt_org_name(&key, "o1", &org).unwrap(), b"acme");
        // The wrong key fails cleanly (this is what drives org-key→master-key fallback).
        let other: [u8; KEY_LEN] = random::bytes();
        assert!(decrypt_org_name(&other, "o1", &org).is_err());
    }
}
