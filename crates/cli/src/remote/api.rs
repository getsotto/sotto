//! The server sync API surface: request/response types and the [`SyncApi`] trait the engine
//! depends on. The reqwest implementation is [`super::http::HttpClient`]; the sync engine (PR5b-ii)
//! targets the trait and is tested with a mock. Opaque ciphertext travels as base64 JSON strings.

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Encode opaque bytes for transport.
pub fn b64encode(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

/// Decode an opaque base64 field received from the server.
pub fn b64decode(value: &str) -> Result<Vec<u8>> {
    STANDARD
        .decode(value)
        .map_err(|e| Error::Server(format!("invalid base64 from server: {e}")))
}

/// The account crypto-material bundle (matches the server's `/account` shape).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountBundle {
    pub public_key: String,
    pub enc_private_keys: String,
    pub kdf_params: String,
    pub recovery_blob: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct NewProject {
    pub id: String,
    pub enc_name: String,
    /// Owning organization, when creating a shared project; omitted for a personal one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub org_id: Option<String>,
}

/// An organization to create (its name is opaque ciphertext, like a project's).
#[derive(Debug, Clone, Serialize)]
pub struct NewOrg {
    pub id: String,
    pub enc_name: String,
}

/// An organization the caller belongs to, with their own role in it.
#[derive(Debug, Clone, Deserialize)]
pub struct OrgInfo {
    pub id: String,
    pub enc_name: String,
    pub role: String,
}

/// The result of inviting a user: their id (now a member) and public key (for sealing grants).
#[derive(Debug, Clone, Deserialize)]
pub struct Invited {
    pub user_id: String,
    pub public_key: Option<String>,
}

/// A member of an organization.
#[derive(Debug, Clone, Deserialize)]
pub struct MemberInfo {
    pub user_id: String,
    pub role: String,
    pub public_key: Option<String>,
}

/// The caller's vault-key grant for an environment (base64), as returned by `GET .../grant`.
#[derive(Debug, Clone, Deserialize)]
pub struct GrantView {
    pub enc_vault_key: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct NewEnvironment {
    pub id: String,
    pub enc_name: String,
    pub enc_vault_key: String,
}

/// A single change in a batch write. `op` is `"set"` or `"delete"`; the `enc_*` fields are present
/// only for `set` (omitted from the JSON otherwise).
#[derive(Debug, Clone, Serialize)]
pub struct SecretChange {
    pub id: String,
    pub op: String,
    pub version: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enc_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enc_value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enc_data_key: Option<String>,
}

impl SecretChange {
    pub fn set(
        id: String,
        version: i64,
        enc_name: String,
        enc_value: String,
        enc_data_key: String,
    ) -> Self {
        Self {
            id,
            op: "set".into(),
            version,
            enc_name: Some(enc_name),
            enc_value: Some(enc_value),
            enc_data_key: Some(enc_data_key),
        }
    }

    pub fn delete(id: String) -> Self {
        Self {
            id,
            op: "delete".into(),
            version: 0,
            enc_name: None,
            enc_value: None,
            enc_data_key: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct BatchRequest {
    pub base_revision: i64,
    pub changes: Vec<SecretChange>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BatchResponse {
    pub revision: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SecretEntry {
    pub id: String,
    pub enc_name: String,
    pub enc_value: String,
    pub enc_data_key: String,
    pub version: i64,
    pub deleted: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Snapshot {
    pub revision: i64,
    pub secrets: Vec<SecretEntry>,
}

/// An environment as returned by the list endpoint (used to reconstruct envs on a new device).
#[derive(Debug, Clone, Deserialize)]
pub struct EnvironmentInfo {
    pub id: String,
    pub enc_name: String,
    pub enc_vault_key: String,
    pub revision: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Me {
    pub user_id: String,
}

/// A share link to create: the sealed blob + limits. `enc_blob`/`passphrase_salt` are base64.
#[derive(Debug, Clone, Serialize)]
pub struct NewShare {
    pub enc_blob: String,
    pub max_views: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl_seconds: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub passphrase_salt: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreatedShare {
    pub token: String,
    #[serde(default)]
    pub expires_at: Option<String>,
}

/// The server operations the sync engine needs, abstracted for testability.
pub trait SyncApi {
    /// Verify the session and return the authenticated user.
    fn me(&self) -> Result<Me>;
    /// Upload account crypto material (first-time account init).
    fn put_account(&self, bundle: &AccountBundle) -> Result<()>;
    /// Download account crypto material, or `None` if the account isn't initialized.
    fn get_account(&self) -> Result<Option<AccountBundle>>;
    fn create_project(&self, project: &NewProject) -> Result<()>;
    fn create_environment(&self, project_id: &str, env: &NewEnvironment) -> Result<()>;
    /// List a project's environments (for reconstructing them on a new device).
    fn list_environments(&self, project_id: &str) -> Result<Vec<EnvironmentInfo>>;
    /// Full snapshot, or `None` when `if_none_match` matches (server returns 304).
    fn snapshot(&self, env_id: &str, if_none_match: Option<i64>) -> Result<Option<Snapshot>>;
    /// Apply a batch atomically; returns the new revision.
    fn write_secrets(&self, env_id: &str, batch: &BatchRequest) -> Result<BatchResponse>;
    /// Create a share link; returns the public token.
    fn create_share(&self, share: &NewShare) -> Result<CreatedShare>;

    // --- teams: organizations, invites, and environment vault-key grants ---

    /// Create an organization (the caller becomes its owner).
    fn create_org(&self, org: &NewOrg) -> Result<()>;
    /// List the organizations the caller belongs to.
    fn list_orgs(&self) -> Result<Vec<OrgInfo>>;
    /// Invite an existing user (by email) into an org as a member; returns their id + public key.
    fn invite_member(&self, org_id: &str, email: &str) -> Result<Invited>;
    /// List an org's members (with their public keys, for sealing grants).
    fn list_members(&self, org_id: &str) -> Result<Vec<MemberInfo>>;
    /// Store a member's vault-key grant for an environment (sharing).
    fn create_grant(&self, env_id: &str, user_id: &str, enc_vault_key: &str) -> Result<()>;
    /// Fetch the caller's own vault-key grant for an environment, or `None` if they have none.
    fn get_grant(&self, env_id: &str) -> Result<Option<String>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips() {
        let bytes = b"\x00\x01\xfe\xff sealed";
        assert_eq!(b64decode(&b64encode(bytes)).unwrap(), bytes);
        assert!(b64decode("not valid base64!!").is_err());
    }

    #[test]
    fn set_change_serializes_all_fields() {
        let json = serde_json::to_string(&SecretChange::set(
            "s1".into(),
            2,
            "n".into(),
            "v".into(),
            "k".into(),
        ))
        .unwrap();
        assert!(json.contains("\"op\":\"set\""));
        assert!(json.contains("\"version\":2"));
        assert!(json.contains("\"enc_value\":\"v\""));
    }

    #[test]
    fn delete_change_omits_enc_fields() {
        let json = serde_json::to_string(&SecretChange::delete("s1".into())).unwrap();
        assert!(json.contains("\"op\":\"delete\""));
        assert!(!json.contains("enc_name"));
        assert!(!json.contains("enc_value"));
    }

    #[test]
    fn account_bundle_round_trips() {
        let bundle = AccountBundle {
            public_key: "cGs".into(),
            enc_private_keys: "ZXBr".into(),
            kdf_params: "a2Rm".into(),
            recovery_blob: "cmVj".into(),
        };
        let json = serde_json::to_string(&bundle).unwrap();
        assert_eq!(
            serde_json::from_str::<AccountBundle>(&json).unwrap(),
            bundle
        );
    }
}
