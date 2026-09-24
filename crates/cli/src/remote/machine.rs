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
/// Warn in the job log once a token has fewer than this many whole days left. Two weeks spans a
/// holiday and a sprint, so whoever owns the pipeline sees it at least once before it breaks.
const EXPIRY_WARNING_DAYS: i64 = 14;

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
    let mut secret: [u8; 32] = secret_bytes
        .as_slice()
        .try_into()
        .map_err(|_| Error::Input("invalid machine key in SOTTO_TOKEN".into()))?;
    secret_bytes.zeroize();
    // `keypair_from_secret` copies the secret into the returned keypair (zeroized on drop); clear
    // this stack-local copy too, so no stray duplicate of the machine key is left behind.
    let keypair = wrap::keypair_from_secret(&secret);
    secret.zeroize();
    Ok(MachineToken {
        api_token: api_token.to_string(),
        keypair,
    })
}

// --- the machine-facing wire (its own bearer, so not part of the session-authed SyncApi) --------

#[derive(Deserialize)]
struct GrantResponse {
    env_id: String,
    enc_vault_key: String,
    // Absent from servers that predate token expiry, whose tokens never expire.
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    expires_at: Option<String>,
    #[serde(default)]
    expires_in_days: Option<i64>,
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

/// What a machine fetch yields: the decrypted secrets, and a warning to print if the token is
/// close to expiry.
pub struct MachineFetch {
    /// Sorted `(name, value)` pairs.
    pub entries: Vec<(String, Vec<u8>)>,
    pub expiry_warning: Option<String>,
}

/// The one-line warning for a token with `days_left` whole days to go, or `None` while it still
/// has at least `EXPIRY_WARNING_DAYS`. Days come from the server, never this machine's clock.
///
/// `name` and `expires_at` come from the server too, which the zero-knowledge model does not
/// trust, and this line lands in a CI log. Escaping keeps it one line: a raw newline would let the
/// server start a line the runner obeys as a workflow command, and an escape sequence could
/// rewrite output already shown.
pub fn expiry_warning(name: &str, expires_at: &str, days_left: i64) -> Option<String> {
    (days_left < EXPIRY_WARNING_DAYS).then(|| {
        let (name, expires_at) = (name.escape_debug(), expires_at.escape_debug());
        format!(
            "warning: machine token `{name}` expires {expires_at} ({days_left}d left); \
             issue a replacement with `sotto token create`"
        )
    })
}

/// Fetch the machine's grant + env snapshot and decrypt every live secret in memory. The vault
/// key and plaintexts never touch disk.
pub fn fetch_entries(server: &str, token: &MachineToken) -> Result<MachineFetch> {
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
                "SOTTO_TOKEN was rejected (revoked, expired, or invalid)".into(),
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
    let expiry_warning = match (&grant.name, &grant.expires_at, grant.expires_in_days) {
        (Some(name), Some(at), Some(days)) => expiry_warning(name, at, days),
        _ => None,
    };
    Ok(MachineFetch {
        entries,
        expiry_warning,
    })
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
    fn expiry_warning_starts_two_weeks_out() {
        let at = "2026-12-22T10:00:00Z";
        assert_eq!(expiry_warning("ci", at, 14), None);
        assert_eq!(
            expiry_warning("ci", at, 13).as_deref(),
            Some(
                "warning: machine token `ci` expires 2026-12-22T10:00:00Z (13d left); \
                 issue a replacement with `sotto token create`"
            )
        );
        assert!(expiry_warning("ci", at, 0).is_some());
    }

    #[test]
    fn expiry_warning_cannot_forge_ci_log_lines() {
        // Both strings come from the server, which the zero-knowledge model does not trust. A
        // newline would start a line the CI runner reads as its own (`::error::` and friends), and
        // an escape sequence could rewrite what is already on screen.
        let warning = expiry_warning(
            "ci\n::error::forged\u{1b}[2K",
            "2026-12-22T10:00:00Z\r\n::add-mask::x",
            3,
        )
        .expect("warned");
        assert!(!warning.chars().any(char::is_control), "{warning:?}");
        assert!(
            warning.contains(r"ci\n::error::forged\u{1b}[2K"),
            "{warning}"
        );
        assert!(
            warning.contains(r"2026-12-22T10:00:00Z\r\n::add-mask::x"),
            "{warning}"
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
