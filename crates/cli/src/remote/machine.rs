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
/// Longest lifetime `sotto token create --expires-in-days` accepts, in days. Mirrors the server's
/// limit so an out-of-range value is rejected while parsing arguments, before any local setup;
/// the server still validates the lifetime authoritatively.
pub const MAX_LIFETIME_DAYS: u32 = 365;

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
    // Absent from servers that predate the combined snapshot; `/machine/grant` has it there.
    #[serde(default)]
    grant: Option<GrantResponse>,
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
///
/// One request against a current server, whose snapshot carries the grant. An older server's
/// snapshot does not, so the grant is fetched separately there.
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
        if resp.status() == reqwest::StatusCode::PAYMENT_REQUIRED {
            let body = resp.text().unwrap_or_default();
            return Err(Error::Input(if body.trim().is_empty() {
                "machine access requires an eligible Cloud account".into()
            } else {
                body
            }));
        }
        if !resp.status().is_success() {
            return Err(Error::Server(format!(
                "machine API error: {}",
                resp.status()
            )));
        }
        Ok(resp)
    };

    let mut snapshot: SnapshotResponse = get("/machine/secrets")?
        .json()
        .map_err(|e| Error::Server(e.to_string()))?;
    let grant: GrantResponse = match snapshot.grant.take() {
        Some(grant) => grant,
        None => get("/machine/grant")?
            .json()
            .map_err(|e| Error::Server(e.to_string()))?,
    };

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
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::mpsc::{channel, Receiver};

    use super::super::api::b64encode;
    use super::*;

    /// A machine token plus the wire JSON a server would hold for it: one environment with a
    /// single secret, `API_KEY=hunter2`.
    struct Fixture {
        token: MachineToken,
        grant_json: String,
        secrets_json: String,
    }

    fn fixture() -> Fixture {
        let secret = [0x42u8; 32];
        let keypair = wrap::keypair_from_secret(&secret);
        let vault_key = vault::generate_vault_key();
        let grant = vault::grant_vault_key(&keypair.public, &vault_key).unwrap();
        let enc = vault::encrypt_secret(&vault_key, "env-1", "sec-1", 1, b"API_KEY", b"hunter2");
        let grant_json = format!(
            r#"{{"env_id":"env-1","enc_vault_key":"{}","name":"ci","expires_at":"2027-01-01T00:00:00Z","expires_in_days":3}}"#,
            b64encode(&grant)
        );
        let secrets_json = format!(
            r#"[{{"id":"sec-1","enc_name":"{}","enc_value":"{}","enc_data_key":"{}","version":1,"deleted":false}}]"#,
            b64encode(&enc.enc_name),
            b64encode(&enc.enc_value),
            b64encode(&enc.enc_data_key)
        );
        Fixture {
            token: parse_token(&assemble_token("smt_abc", &secret)).unwrap(),
            grant_json,
            secrets_json,
        }
    }

    /// Serve `bodies` as JSON, one per request and in order, then stop listening. Returns the base
    /// URL and a channel yielding each request's head (request line + headers, lowercased), so a
    /// test can assert exactly which requests were made.
    fn serve(bodies: Vec<String>) -> (String, Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let (heads, received) = channel();
        std::thread::spawn(move || {
            for body in bodies {
                let (mut stream, _) = listener.accept().expect("accept");
                let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
                let mut head = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).expect("read request") <= 2 {
                        break;
                    }
                    head.push_str(&line.to_lowercase());
                }
                heads.send(head).expect("report request");
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).expect("write");
            }
        });
        (format!("http://{addr}"), received)
    }

    #[test]
    fn a_current_server_is_read_in_one_request() {
        let f = fixture();
        let (base, heads) = serve(vec![format!(
            r#"{{"revision":1,"secrets":{},"grant":{}}}"#,
            f.secrets_json, f.grant_json
        )]);
        let fetched = fetch_entries(&base, &f.token).expect("fetch");
        assert_eq!(
            fetched.entries,
            vec![("API_KEY".to_string(), b"hunter2".to_vec())]
        );
        // The grant's expiry fields survive the move into the snapshot.
        assert!(fetched.expiry_warning.expect("warned").contains("3d left"));
        let requests: Vec<String> = heads.iter().collect();
        assert_eq!(requests.len(), 1, "{requests:?}");
        assert!(requests[0].starts_with("get /machine/secrets "));
        assert!(requests[0].contains("authorization: bearer smt_abc"));
    }

    #[test]
    fn an_older_server_without_the_combined_grant_still_works() {
        let f = fixture();
        let (base, heads) = serve(vec![
            format!(r#"{{"revision":1,"secrets":{}}}"#, f.secrets_json),
            f.grant_json.clone(),
        ]);
        let fetched = fetch_entries(&base, &f.token).expect("fetch");
        assert_eq!(
            fetched.entries,
            vec![("API_KEY".to_string(), b"hunter2".to_vec())]
        );
        let requests: Vec<String> = heads.iter().collect();
        assert_eq!(requests.len(), 2, "{requests:?}");
        assert!(requests[0].starts_with("get /machine/secrets "));
        assert!(requests[1].starts_with("get /machine/grant "));
    }

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
