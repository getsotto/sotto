//! Identity setup and the unlock/lock session.
//!
//! - [`init`] creates the local identity: a fresh 128-bit secret key (stored in the keychain), a
//!   KDF salt + encrypted verifier (stored in the [`Store`]), and returns the [`EmergencyKit`].
//! - [`unlock`] re-derives the master key from the password + secret key, checks it against the
//!   verifier, and caches it.
//! - [`current_master_key`] returns the cached master key while the session is valid; [`lock`]
//!   clears it.
//!
//! The master key is cached (with an expiry) in the keychain so we don't re-run 256 MiB Argon2
//! on every command. Passwords are passed in as bytes — prompting is the command layer's job.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sotto_core::{aead, format, kdf, random};
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::error::{Error, Result};
use crate::keychain::Keychain;
use crate::store::{Identity, Store};

/// Secret-key length, in bytes (128-bit).
const SECRET_KEY_BYTES: usize = 16;
/// Keychain entry holding the persistent secret key.
const KC_SECRET_KEY: &str = "secret-key";
/// Keychain entry holding the cached session (master key + expiry).
const KC_SESSION: &str = "session";
/// Session layout: 32-byte master key followed by an 8-byte little-endian expiry (unix ms).
const SESSION_LEN: usize = 32 + 8;
/// Marker sealed under the master key at init and checked at unlock.
const VERIFIER_PLAINTEXT: &[u8] = b"sotto-verifier-v1";
const VERIFIER_AAD: &[u8] = b"sotto/v1/verifier";

/// A derived master key, zeroized on drop.
#[derive(ZeroizeOnDrop)]
pub struct MasterKey([u8; 32]);

impl MasterKey {
    /// The raw key bytes, for passing to the crypto core.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// What a user must keep to recover access (shown once at `init`).
#[derive(Debug, Clone)]
pub struct EmergencyKit {
    /// The secret key, formatted as `SK1-…`.
    pub secret_key: String,
}

/// Create the local identity and start an unlocked session. Errors if one already exists.
pub fn init(
    store: &Store,
    keychain: &dyn Keychain,
    password: &[u8],
    ttl: Duration,
) -> Result<EmergencyKit> {
    if store.get_identity()?.is_some() {
        return Err(Error::AlreadyInitialized);
    }

    let mut secret_key = random::bytes::<SECRET_KEY_BYTES>();
    let salt: [u8; kdf::SALT_LEN] = random::bytes();
    let master_key = derive(password, &secret_key, &salt)?;

    let enc_verifier = aead::seal(master_key.as_bytes(), VERIFIER_PLAINTEXT, VERIFIER_AAD);
    store.put_identity(&Identity {
        kdf_salt: salt.to_vec(),
        enc_verifier,
    })?;
    keychain.set(KC_SECRET_KEY, &secret_key)?;
    cache_session(keychain, &master_key, ttl)?;

    let kit = EmergencyKit {
        secret_key: format::encode_key("SK", 1, &secret_key),
    };
    secret_key.zeroize();
    Ok(kit)
}

/// Re-derive the master key from the password, verify it, and start a session.
pub fn unlock(
    store: &Store,
    keychain: &dyn Keychain,
    password: &[u8],
    ttl: Duration,
) -> Result<()> {
    let mut secret_key = keychain.get(KC_SECRET_KEY)?.ok_or(Error::NoIdentity)?;
    let identity = store.get_identity()?.ok_or(Error::NoIdentity)?;
    let salt: [u8; kdf::SALT_LEN] = identity
        .kdf_salt
        .as_slice()
        .try_into()
        .map_err(|_| Error::Crypto)?;

    let master_key = derive(password, &secret_key, &salt)?;
    secret_key.zeroize();

    // The verifier is the unlock check: a wrong password yields an authentication failure.
    aead::open(master_key.as_bytes(), &identity.enc_verifier, VERIFIER_AAD)
        .map_err(|_| Error::Crypto)?;
    cache_session(keychain, &master_key, ttl)
}

/// Clear the cached session (lock the store).
pub fn lock(keychain: &dyn Keychain) -> Result<()> {
    keychain.delete(KC_SESSION)
}

/// Return the cached master key if the session is still valid, else `None` (locked or expired).
/// A malformed or expired entry is purged.
pub fn current_master_key(keychain: &dyn Keychain) -> Result<Option<MasterKey>> {
    let Some(mut buf) = keychain.get(KC_SESSION)? else {
        return Ok(None);
    };
    if buf.len() != SESSION_LEN {
        buf.zeroize();
        keychain.delete(KC_SESSION)?;
        return Ok(None);
    }

    let expiry = i64::from_le_bytes(buf[32..SESSION_LEN].try_into().expect("8 bytes"));
    if now_ms() >= expiry {
        buf.zeroize();
        keychain.delete(KC_SESSION)?;
        return Ok(None);
    }

    let mut key = [0u8; 32];
    key.copy_from_slice(&buf[..32]);
    buf.zeroize();
    Ok(Some(MasterKey(key)))
}

fn derive(password: &[u8], secret_key: &[u8], salt: &[u8; kdf::SALT_LEN]) -> Result<MasterKey> {
    let mut derived = kdf::derive_master_key(password, secret_key, salt)?;
    let master_key = MasterKey(derived);
    derived.zeroize();
    Ok(master_key)
}

fn cache_session(keychain: &dyn Keychain, master_key: &MasterKey, ttl: Duration) -> Result<()> {
    let expiry = now_ms().saturating_add(ttl.as_millis() as i64);
    let mut buf = Vec::with_capacity(SESSION_LEN);
    buf.extend_from_slice(master_key.as_bytes());
    buf.extend_from_slice(&expiry.to_le_bytes());
    let result = keychain.set(KC_SESSION, &buf);
    buf.zeroize();
    result
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keychain::MemoryKeychain;
    use crate::store::Store;

    const TTL: Duration = Duration::from_secs(3600);

    fn setup() -> (Store, MemoryKeychain) {
        (Store::open_in_memory().unwrap(), MemoryKeychain::default())
    }

    #[test]
    fn init_returns_kit_and_leaves_session_unlocked() {
        let (store, kc) = setup();
        let kit = init(&store, &kc, b"correct horse battery", TTL).unwrap();
        assert!(kit.secret_key.starts_with("SK1-"));
        assert!(current_master_key(&kc).unwrap().is_some());
    }

    #[test]
    fn init_twice_is_rejected() {
        let (store, kc) = setup();
        init(&store, &kc, b"pw", TTL).unwrap();
        assert!(matches!(
            init(&store, &kc, b"pw", TTL),
            Err(Error::AlreadyInitialized)
        ));
    }

    #[test]
    fn unlock_reproduces_the_init_master_key() {
        let (store, kc) = setup();
        init(&store, &kc, b"pw", TTL).unwrap();
        let from_init = current_master_key(&kc).unwrap().unwrap();

        lock(&kc).unwrap();
        assert!(current_master_key(&kc).unwrap().is_none());

        unlock(&store, &kc, b"pw", TTL).unwrap();
        let from_unlock = current_master_key(&kc).unwrap().unwrap();
        assert_eq!(from_init.as_bytes(), from_unlock.as_bytes());
    }

    #[test]
    fn unlock_with_wrong_password_fails_and_stays_locked() {
        let (store, kc) = setup();
        init(&store, &kc, b"right", TTL).unwrap();
        lock(&kc).unwrap();
        assert!(matches!(
            unlock(&store, &kc, b"wrong", TTL),
            Err(Error::Crypto)
        ));
        assert!(current_master_key(&kc).unwrap().is_none());
    }

    #[test]
    fn unlock_without_identity_fails() {
        let (store, kc) = setup();
        assert!(matches!(
            unlock(&store, &kc, b"pw", TTL),
            Err(Error::NoIdentity)
        ));
    }

    #[test]
    fn expired_session_reads_as_locked() {
        let (store, kc) = setup();
        init(&store, &kc, b"pw", Duration::ZERO).unwrap();
        assert!(current_master_key(&kc).unwrap().is_none());
    }
}
