//! Client-side share creation: seal a secret, upload the ciphertext, and build the shareable link.
//!
//! The fragment key never leaves the client — it goes only in the URL fragment (`#…`), which the
//! browser keeps out of requests. With a passphrase, the AEAD key is derived from the fragment key
//! + passphrase, so neither the link nor the server can decrypt alone.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;

use sotto_core::{kdf, random, share as core_share};

use crate::error::Result;

use super::api::{b64encode, NewShare, SyncApi};

/// Options for a new share link.
pub struct ShareOptions {
    pub max_views: i32,
    pub ttl_seconds: Option<i64>,
    /// If set, the link is passphrase-protected (a second factor beyond the fragment key).
    pub passphrase: Option<Vec<u8>>,
}

/// Seal `value`, upload it, and return the shareable link (`<web>/s/<token>#<fragment-key>`).
pub fn create(
    api: &dyn SyncApi,
    web_base: &str,
    value: &[u8],
    opts: &ShareOptions,
) -> Result<String> {
    const MAX_VIEWS: i32 = 100;
    const MAX_TTL_SECONDS: i64 = 30 * 24 * 60 * 60;

    if !(1..=MAX_VIEWS).contains(&opts.max_views) {
        return Err(crate::error::Error::Input(format!(
            "views must be between 1 and {MAX_VIEWS}"
        )));
    }
    if let Some(ttl) = opts.ttl_seconds {
        if !(1..=MAX_TTL_SECONDS).contains(&ttl) {
            return Err(crate::error::Error::Input(format!(
                "expire must be between 1 and {MAX_TTL_SECONDS} seconds"
            )));
        }
    }

    let fragment_key = random::bytes::<{ core_share::KEY_LEN }>();
    let (aead_key, passphrase_salt) = match &opts.passphrase {
        Some(passphrase) => {
            let salt = random::bytes::<{ kdf::SALT_LEN }>();
            let key = core_share::passphrase_key(&fragment_key, passphrase, &salt)?;
            (key, Some(salt.to_vec()))
        }
        None => (fragment_key, None),
    };

    let enc_blob = core_share::seal(&aead_key, value);
    let created = api.create_share(&NewShare {
        enc_blob: b64encode(&enc_blob),
        max_views: opts.max_views,
        ttl_seconds: opts.ttl_seconds,
        passphrase_salt: passphrase_salt.as_deref().map(b64encode),
    })?;

    Ok(build_link(web_base, &created.token, &fragment_key))
}

/// `<web>/s/<token>#<url-safe base64 fragment key>`.
fn build_link(web_base: &str, token: &str, fragment_key: &[u8]) -> String {
    format!(
        "{}/s/{}#{}",
        web_base.trim_end_matches('/'),
        token,
        URL_SAFE_NO_PAD.encode(fragment_key),
    )
}

#[cfg(test)]
mod tests {
    use super::build_link;

    #[test]
    fn link_format() {
        assert_eq!(
            build_link("https://app.sotto.dev/", "tok123", &[0u8, 0, 0]),
            "https://app.sotto.dev/s/tok123#AAAA"
        );
    }
}
