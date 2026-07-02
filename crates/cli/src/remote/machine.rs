//! Machine (CI / service) mode: everything `SOTTO_TOKEN` can do.
//!
//! A machine token string is `<smt_api-token>.<MT1-…>`: the server-issued API token joined with
//! the machine's X25519 private key in the checksummed key format. The API part authenticates the
//! tiny read-only `/machine/*` surface; the key part opens the machine's vault-key grant. The
//! server only ever sees the API part (as a hash), so a machine run stays zero-knowledge.
//!
//! Machine mode needs no local store, keychain, config, or password: the token names its
//! environment, and all decryption happens in memory.

use serde::Deserialize;
use zeroize::{Zeroize, Zeroizing};

use sotto_core::{format, vault, wrap};

use crate::error::{Error, Result};

use super::api::b64decode;

/// Prefix + version of the machine-key half of the token string.
const KEY_PREFIX: &str = "MT";
const KEY_VERSION: u8 = 1;

/// A parsed machine token: the API bearer + the machine keypair recovered from its private key.
pub struct MachineToken {
    pub api_token: String,
    pub keypair: wrap::Keypair,
}

/// Assemble the `SOTTO_TOKEN` string handed to CI: API token + encoded machine private key.
pub fn assemble_token(api_token: &str, machine_secret: &[u8; 32]) -> String {
    format!(
        "{api_token}.{}",
        format::encode_key(KEY_PREFIX, KEY_VERSION, machine_secret)
    )
}

/// Parse a `SOTTO_TOKEN` string. Fails on a malformed shape, a bad checksum, or a wrong key length.
pub fn parse_token(token: &str) -> Result<MachineToken> {
    let (api_token, key_part) = token
        .trim()
        .split_once('.')
        .ok_or_else(|| Error::Input("malformed SOTTO_TOKEN (expected <token>.<MT1-…>)".into()))?;
    let mut secret_bytes = format::decode_key(KEY_PREFIX, KEY_VERSION, key_part)
        .map_err(|_| Error::Input("invalid machine key in SOTTO_TOKEN".into()))?;
    let secret: [u8; 32] = secret_bytes
        .as_slice()
        .try_into()
        .map_err(|_| Error::Input("invalid machine key in SOTTO_TOKEN".into()))?;
    secret_bytes.zeroize();
    Ok(MachineToken {
        api_token: api_token.to_string(),
        keypair: wrap::keypair_from_secret(&secret),
    })
}

// --- the machine-facing wire (its own bearer, so not part of the session-authed SyncApi) --------

#[derive(Deserialize)]
struct GrantResponse {
    env_id: String,
    enc_vault_key: String,
}

#[derive(Deserialize)]
struct SecretEntry {
    id: String,
    enc_name: String,
    enc_value: String,
    enc_data_key: String,
    version: i64,
    deleted: bool,
}

#[derive(Deserialize)]
struct SnapshotResponse {
    secrets: Vec<SecretEntry>,
}

/// Fetch the machine's grant + env snapshot and decrypt every live secret in memory, returning
/// sorted `(name, value)` pairs. The vault key and plaintexts never touch disk.
pub fn fetch_entries(server: &str, token: &MachineToken) -> Result<Vec<(String, Vec<u8>)>> {
    let http = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| Error::Network(e.to_string()))?;
    let get = |path: &str| -> Result<reqwest::blocking::Response> {
        let resp = http
            .get(format!("{server}{path}"))
            .bearer_auth(&token.api_token)
            .send()
            .map_err(|e| Error::Network(e.to_string()))?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(Error::Input(
                "SOTTO_TOKEN was rejected (revoked or invalid)".into(),
            ));
        }
        if !resp.status().is_success() {
            return Err(Error::Server(format!(
                "machine API error: {}",
                resp.status()
            )));
        }
        Ok(resp)
    };

    let grant: GrantResponse = get("/machine/grant")?
        .json()
        .map_err(|e| Error::Server(e.to_string()))?;
    let snapshot: SnapshotResponse = get("/machine/secrets")?
        .json()
        .map_err(|e| Error::Server(e.to_string()))?;

    let vault_key = Zeroizing::new(vault::open_vault_key(
        &token.keypair,
        &b64decode(&grant.enc_vault_key)?,
    )?);

    let mut entries = Vec::new();
    for s in snapshot.secrets.iter().filter(|s| !s.deleted) {
        let (name, value) = vault::decrypt_secret(
            &vault_key,
            &grant.env_id,
            &s.id,
            s.version,
            &b64decode(&s.enc_name)?,
            &b64decode(&s.enc_value)?,
            &b64decode(&s.enc_data_key)?,
        )?;
        let name = String::from_utf8(name).map_err(|_| Error::Crypto)?;
        entries.push((name, value));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_string_round_trips() {
        let secret = [0x5Au8; 32];
        let token = assemble_token("smt_abc123", &secret);
        let parsed = parse_token(&token).unwrap();
        assert_eq!(parsed.api_token, "smt_abc123");
        assert_eq!(
            parsed.keypair.public,
            wrap::keypair_from_secret(&secret).public
        );
    }

    #[test]
    fn malformed_tokens_are_rejected() {
        assert!(parse_token("no-separator").is_err());
        assert!(parse_token("smt_abc.not-a-key").is_err());
        // A corrupted key part fails its checksum.
        let good = assemble_token("smt_abc", &[1u8; 32]);
        let mut corrupted = good.clone();
        corrupted.pop();
        assert!(parse_token(&corrupted).is_err());
    }
}
