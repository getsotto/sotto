//! Identity setup and the unlock/lock session.
//!
//! - [`init`] creates the local identity: a fresh 128-bit secret key (stored in the keychain), a
//!   KDF salt + encrypted verifier, and the account key material (X25519 keypair + recovery key,
//!   stored in the [`Store`]); it returns the [`EmergencyKit`] (secret key + recovery key).
//! - [`unlock`] re-derives the master key from the password + secret key, checks it against the
//!   verifier, and caches it.
//! - [`current_master_key`] returns the cached master key while the session is valid; [`lock`]
//!   clears it.
//!
//! The master key is cached (with an expiry) in the keychain so we don't re-run 256 MiB Argon2
//! on every command. Passwords are passed in as bytes - prompting is the command layer's job.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sotto_core::vault as core_vault;
use sotto_core::{aead, format, kdf, random, wrap};
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::error::{Error, Result};
use crate::keychain::Keychain;
use crate::store::{AccountKeys, Identity, Store};

/// Secret-key length, in bytes (128-bit).
const SECRET_KEY_BYTES: usize = 16;
/// Recovery-key length, in bytes (256-bit).
const RECOVERY_KEY_BYTES: usize = 32;
/// AAD binding the master key wrapped under the recovery key.
const RECOVERY_AAD: &[u8] = b"sotto/v1/recovery";
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
    /// The secret key (second factor with the password), formatted as `SK1-…`.
    pub secret_key: String,
    /// The recovery key (independent factor that unwraps the master key), formatted as `RK1-…`.
    pub recovery_key: String,
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
    reinit(store, keychain, password, ttl)
}

/// Create a **fresh** identity, overwriting any existing one - the account-reset path for a user
/// who lost their Emergency Kit. Everything sealed to the old keys (local or remote) becomes
/// permanently unreadable; callers must warn loudly before invoking this.
pub fn reinit(
    store: &Store,
    keychain: &dyn Keychain,
    password: &[u8],
    ttl: Duration,
) -> Result<EmergencyKit> {
    let mut secret_key = random::bytes::<SECRET_KEY_BYTES>();
    let salt: [u8; kdf::SALT_LEN] = random::bytes();
    let master_key = derive(password, &secret_key, &salt)?;

    let enc_verifier = aead::seal(master_key.as_bytes(), VERIFIER_PLAINTEXT, VERIFIER_AAD);

    // Account key material, generated at signup so a new device can reconstruct the account:
    // an X25519 keypair (sharing) and an independent recovery key. The private key is sealed under
    // the master key; the master key is sealed under the recovery key. The recovery key wraps the
    // master key (it does not weaken the password+secret-key factor) and is a recovery-only factor.
    let keypair = wrap::generate_keypair();
    let mut recovery_key = random::bytes::<RECOVERY_KEY_BYTES>();
    let account_keys = AccountKeys {
        public_key: keypair.public.to_vec(),
        enc_private_keys: core_vault::wrap_private_key(master_key.as_bytes(), &keypair.secret),
        recovery_blob: wrap::wrap_key(&recovery_key, master_key.as_bytes(), RECOVERY_AAD),
    };

    // Persist the secret key before marking the store initialised. If we wrote the identity row
    // first and the keychain write then failed, the store would look initialised with no
    // recoverable secret key (future `init` is rejected, `unlock` can't re-derive). Writing the
    // key first lets us roll it back if the store write fails, so a failed init leaves no
    // half-initialised state.
    keychain.set(KC_SECRET_KEY, &secret_key)?;
    if let Err(e) = store.put_identity_with_keys(
        &Identity {
            kdf_salt: salt.to_vec(),
            enc_verifier,
        },
        &account_keys,
    ) {
        let _ = keychain.delete(KC_SECRET_KEY);
        return Err(e);
    }
    cache_session(keychain, &master_key, ttl)?;

    let kit = EmergencyKit {
        secret_key: format::encode_key("SK", 1, &secret_key),
        recovery_key: format::encode_key("RK", 1, &recovery_key),
    };
    secret_key.zeroize();
    recovery_key.zeroize();
    Ok(kit)
}

/// Reconstruct the local identity on a new device from downloaded account material.
///
/// Derives the master key from the password + pasted secret key + the account's salt, verifies it
/// by unwrapping the account's private keys, then persists the identity, account keys, and secret
/// key, and starts a session. Errors if an identity already exists.
#[allow(clippy::too_many_arguments)]
pub fn restore(
    store: &Store,
    keychain: &dyn Keychain,
    password: &[u8],
    secret_key: &[u8],
    salt: &[u8; kdf::SALT_LEN],
    account_keys: &AccountKeys,
    ttl: Duration,
) -> Result<()> {
    if store.get_identity()?.is_some() {
        return Err(Error::AlreadyInitialized);
    }
    let master_key = derive(password, secret_key, salt)?;

    // Verify password + secret key: the account keypair must recover from the private keys.
    core_vault::open_account_keypair(master_key.as_bytes(), &account_keys.enc_private_keys)
        .map_err(|_| Error::Crypto)?;

    let enc_verifier = aead::seal(master_key.as_bytes(), VERIFIER_PLAINTEXT, VERIFIER_AAD);
    keychain.set(KC_SECRET_KEY, secret_key)?;
    if let Err(e) = store.put_identity_with_keys(
        &Identity {
            kdf_salt: salt.to_vec(),
            enc_verifier,
        },
        account_keys,
    ) {
        let _ = keychain.delete(KC_SECRET_KEY);
        return Err(e);
    }
    cache_session(keychain, &master_key, ttl)
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

/// Recover the account keypair (X25519) from the store's key material under the master key. The
/// vault layer uses it to open environment key grants.
pub fn account_keypair(store: &Store, master: &MasterKey) -> Result<wrap::Keypair> {
    let keys = store.get_account_keys()?.ok_or(Error::NoIdentity)?;
    core_vault::open_account_keypair(master.as_bytes(), &keys.enc_private_keys).map_err(Into::into)
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
    // Clamp rather than `as i64`: a huge TTL would otherwise wrap to a negative offset and make
    // the session read as already-expired.
    let ttl_ms = i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX);
    let expiry = now_ms().saturating_add(ttl_ms);
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
        assert!(kit.recovery_key.starts_with("RK1-"));
        assert!(current_master_key(&kc).unwrap().is_some());
    }

    #[test]
    fn init_generates_consistent_account_key_material() {
        let (store, kc) = setup();
        let kit = init(&store, &kc, b"pw", TTL).unwrap();
        let master = current_master_key(&kc).unwrap().unwrap();
        let keys = store.get_account_keys().unwrap().unwrap();

        // The account keypair recovers from enc_private_keys, and its public matches.
        let keypair = account_keypair(&store, &master).unwrap();
        assert_eq!(keypair.public.to_vec(), keys.public_key);

        // recovery_blob unwraps (under the recovery key from the kit) to the master key.
        let recovery_key: [u8; RECOVERY_KEY_BYTES] = format::decode_key("RK", 1, &kit.recovery_key)
            .unwrap()
            .try_into()
            .unwrap();
        let recovered = wrap::unwrap_key(&recovery_key, &keys.recovery_blob, RECOVERY_AAD).unwrap();
        assert_eq!(&recovered, master.as_bytes());
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

    /// Reconstruct A's identity material for a new device: secret key + salt + account keys.
    fn restore_inputs(
        store: &Store,
        kit: &EmergencyKit,
    ) -> ([u8; kdf::SALT_LEN], AccountKeys, Vec<u8>) {
        let identity = store.get_identity().unwrap().unwrap();
        let keys = store.get_account_keys().unwrap().unwrap();
        let salt: [u8; kdf::SALT_LEN] = identity.kdf_salt.as_slice().try_into().unwrap();
        let secret_key = format::decode_key("SK", 1, &kit.secret_key).unwrap();
        (salt, keys, secret_key)
    }

    #[test]
    fn restore_reconstructs_the_same_master_key() {
        let (store_a, kc_a) = setup();
        let kit = init(&store_a, &kc_a, b"pw", TTL).unwrap();
        let master_a = current_master_key(&kc_a).unwrap().unwrap();
        let (salt, keys, secret_key) = restore_inputs(&store_a, &kit);

        let (store_b, kc_b) = setup();
        restore(&store_b, &kc_b, b"pw", &secret_key, &salt, &keys, TTL).unwrap();

        let master_b = current_master_key(&kc_b).unwrap().unwrap();
        assert_eq!(master_a.as_bytes(), master_b.as_bytes());
        assert!(store_b.get_identity().unwrap().is_some());
        assert_eq!(store_b.get_account_keys().unwrap().unwrap(), keys);
    }

    #[test]
    fn restore_with_wrong_password_fails() {
        let (store_a, kc_a) = setup();
        let kit = init(&store_a, &kc_a, b"right", TTL).unwrap();
        let (salt, keys, secret_key) = restore_inputs(&store_a, &kit);

        let (store_b, kc_b) = setup();
        assert!(matches!(
            restore(&store_b, &kc_b, b"wrong", &secret_key, &salt, &keys, TTL),
            Err(Error::Crypto)
        ));
        assert!(store_b.get_identity().unwrap().is_none());
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
