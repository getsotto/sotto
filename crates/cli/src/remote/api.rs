//! The server sync API surface: request/response types and the [`SyncApi`] trait the engine and
//! team operations depend on. The reqwest implementation is [`super::http::HttpClient`]; callers
//! target the trait and are tested with a mock. Opaque ciphertext travels as base64 JSON strings.

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
    /// Owning organisation, when creating a shared project; omitted for a personal one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub org_id: Option<String>,
}

/// An organisation to create: name ciphertext + the org key sealed to the creator.
#[derive(Debug, Clone, Serialize)]
pub struct NewOrg {
    pub id: String,
    pub enc_name: String,
    pub enc_org_key: String,
}

/// An organisation the caller belongs to, with their own role and (if granted) sealed org key.
#[derive(Debug, Clone, Deserialize)]
pub struct OrgInfo {
    pub id: String,
    pub enc_name: String,
    pub role: String,
    #[serde(default)]
    pub enc_org_key: Option<String>,
}

/// The result of inviting a user: their id (now a member) and public key (for sealing grants).
#[derive(Debug, Clone, Deserialize)]
pub struct Invited {
    pub user_id: String,
    pub public_key: Option<String>,
}

/// A member of an organisation.
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

/// One recipient's new grant in a rotation (the new vault key sealed to their public key).
#[derive(Debug, Clone, Serialize)]
pub struct GrantEntry {
    pub user_id: String,
    pub enc_vault_key: String,
}

/// One secret's data key, rewrapped under the new vault key, in a rotation.
#[derive(Debug, Clone, Serialize)]
pub struct DataKeyEntry {
    pub secret_id: String,
    pub enc_data_key: String,
}

/// The new vault key re-sealed to one machine token's public key, in a rotation.
#[derive(Debug, Clone, Serialize)]
pub struct MachineGrantEntry {
    pub token_id: String,
    pub enc_vault_key: String,
}

/// One retained history version's data key, rewrapped under the new vault key, in a rotation.
#[derive(Debug, Clone, Serialize)]
pub struct HistoryKeyEntry {
    pub secret_id: String,
    pub version: i64,
    pub enc_data_key: String,
}

/// A rotation request: rewrapped data keys (current + history) + the replacement grant set (users
/// and machines), at a base revision. `machine_grants` must cover every active machine token in the
/// env and may also cover one that expired after it was listed; `history_keys` must cover exactly
/// the env's retained versions.
#[derive(Debug, Clone, Serialize)]
pub struct RotateRequest {
    pub base_revision: i64,
    pub grants: Vec<GrantEntry>,
    pub data_keys: Vec<DataKeyEntry>,
    pub machine_grants: Vec<MachineGrantEntry>,
    pub history_keys: Vec<HistoryKeyEntry>,
}

/// An org's plan: assigned + effective tier, trial end, and the limits in effect.
#[derive(Debug, Clone, Deserialize)]
pub struct Entitlements {
    pub tier: String,
    pub effective_tier: String,
    pub trial_ends_at: Option<String>,
    pub limits: Option<PlanLimits>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PlanLimits {
    pub max_members: i64,
    pub max_org_projects: i64,
}

/// One org audit event, newest-first from the server's log.
#[derive(Debug, Clone, Deserialize)]
pub struct AuditEvent {
    pub id: i64,
    pub actor: String,
    pub action: String,
    pub target: Option<String>,
    pub env_id: Option<String>,
    pub detail: Option<String>,
    /// RFC 3339 timestamp.
    pub at: String,
}

/// One retained version of one secret, as the server's history endpoint returns it.
#[derive(Debug, Clone, Deserialize)]
pub struct HistoryRow {
    pub secret_id: String,
    pub version: i64,
    pub enc_name: String,
    pub enc_value: String,
    pub enc_data_key: String,
}

/// An active machine token, as listed for admins (rotation re-seals to `public_key`).
#[derive(Debug, Clone, Deserialize)]
pub struct MachineTokenInfo {
    pub token_id: String,
    pub name: String,
    /// The machine's X25519 public key (base64).
    pub public_key: String,
    /// The user who created the token, if still known.
    pub created_by: Option<String>,
    /// The human account whose hosted eligibility is accountable for this token, if known.
    #[serde(default)]
    pub beneficiary_id: Option<String>,
    /// `verified` or `ambiguous`; absent from servers predating machine accountability.
    #[serde(default)]
    pub beneficiary_status: Option<String>,
    /// When the token stops authenticating (UTC, RFC 3339). Absent from servers that predate
    /// token expiry, whose tokens never expire.
    #[serde(default)]
    pub expires_at: Option<String>,
    /// Whole days until then, rounded down, by the server's clock.
    #[serde(default)]
    pub expires_in_days: Option<i64>,
}

impl MachineTokenInfo {
    /// `expires <timestamp> (in <n>d)` for listings, or `None` against a server that predates
    /// token expiry. The timestamp is server-sent, so it is escaped onto one line.
    pub fn expiry_label(&self) -> Option<String> {
        let at = self.expires_at.as_deref()?.escape_debug();
        Some(match self.expires_in_days {
            Some(days) => format!("expires {at} (in {days}d)"),
            None => format!("expires {at}"),
        })
    }
}

/// A machine token revoked by a member removal (names, never the raw token).
#[derive(Debug, Clone, Deserialize)]
pub struct RevokedTokenInfo {
    pub token_id: String,
    pub name: String,
    pub env_id: String,
}

/// The member-removal receipt: what the server revoked, so the team knows which shared
/// machine tokens to recreate.
#[derive(Debug, Clone, Deserialize)]
pub struct RemovalReceipt {
    pub revoked_tokens: Vec<RevokedTokenInfo>,
    pub grants_deleted: i64,
}

/// A freshly created machine token: its id + the raw API token (shown by the server exactly once).
#[derive(Debug, Clone, Deserialize)]
pub struct CreatedMachineToken {
    pub token_id: String,
    pub token: String,
    /// When the token stops authenticating; absent from servers that predate token expiry.
    #[serde(default)]
    pub expires_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RotateResponse {
    pub revision: i64,
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
    /// The caller's OWN vault-key grant, or `None` if they hold none for this environment.
    pub enc_vault_key: Option<String>,
    pub revision: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Me {
    pub user_id: String,
}

/// Public server capability metadata. Older servers may not expose this route; callers treat the
/// absence as an unknown capability rather than assuming hosted billing is available.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ServerInfo {
    pub deployment_mode: String,
    pub entitlement_model: String,
}

/// Account-level Cloud state returned by the eligibility endpoint.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct EligibilityView {
    pub model: String,
    pub state: String,
    pub deployment_mode: String,
    pub account_initialized: bool,
    pub billing_available: bool,
    pub paid_through_epoch: Option<i64>,
    pub recovery_until_epoch: Option<i64>,
    pub export_until_epoch: Option<i64>,
    pub actions: EligibilityActions,
    pub next_actions: Vec<String>,
    pub payer: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct EligibilityActions {
    pub setup: bool,
    pub billing: bool,
    pub export: bool,
    pub revoke: bool,
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

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ShareSummary {
    pub token: String,
    pub share_class: String,
    pub max_views: i32,
    pub view_count: i32,
    pub expires_at: Option<String>,
    pub revoked_at: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ShareList {
    pub active_free_count: i64,
    pub links: Vec<ShareSummary>,
}

/// Versioned Cloud exit-export manifest. It names only resources the caller can decrypt.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ExportManifest {
    pub version: i32,
    pub export_id: String,
    pub manifest_hash: String,
    pub expires_at: String,
    pub total_chunks: usize,
    pub complete: bool,
    pub projects: Vec<ExportProject>,
    pub environments: Vec<ExportEnvironmentRef>,
    #[serde(default)]
    pub not_shared_environment_ids: Vec<String>,
    pub omitted_environment_count: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ExportProject {
    pub id: String,
    pub enc_name: String,
    pub org_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ExportEnvironmentRef {
    pub id: String,
    pub project_id: String,
    pub enc_name: String,
    pub enc_vault_key: String,
    pub revision: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ExportChunk {
    pub version: i32,
    pub export_id: String,
    pub manifest_hash: String,
    pub index: usize,
    pub total_chunks: usize,
    pub complete: bool,
    pub account: Option<AccountBundle>,
    pub environment: Option<ExportEnvironment>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ExportEnvironment {
    pub id: String,
    pub project_id: String,
    pub enc_name: String,
    pub enc_vault_key: String,
    pub revision: i64,
    pub content_hash: String,
    pub secrets: Vec<ExportSecret>,
    pub history: Vec<ExportHistory>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ExportSecret {
    pub id: String,
    pub enc_name: String,
    pub enc_value: String,
    pub enc_data_key: String,
    pub version: i64,
    pub deleted: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ExportHistory {
    pub secret_id: String,
    pub version: i64,
    pub enc_name: String,
    pub enc_value: String,
    pub enc_data_key: String,
}

/// The server operations the sync engine needs, abstracted for testability.
pub trait SyncApi {
    /// Verify the session and return the authenticated user.
    fn me(&self) -> Result<Me>;
    /// Upload account crypto material (first-time account init).
    fn put_account(&self, bundle: &AccountBundle) -> Result<()>;
    /// Replace the account's crypto material with fresh keys (Emergency-Kit-lost recovery). The
    /// server also deletes the user's now-dead environment grants.
    fn reset_account(&self, bundle: &AccountBundle) -> Result<()>;
    /// Download account crypto material, or `None` if the account isn't initialised.
    fn get_account(&self) -> Result<Option<AccountBundle>>;
    /// Start a short-lived, versioned export of the caller's authorised resources.
    fn start_export(&self) -> Result<ExportManifest>;
    /// Fetch one opaque export chunk, rechecking the caller's grant and environment revision.
    fn export_chunk(&self, export_id: &str, index: usize) -> Result<ExportChunk>;
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
    /// List share metadata without ciphertext or fragment keys.
    fn list_shares(&self) -> Result<ShareList>;
    /// Revoke one of the caller's share links.
    fn revoke_share(&self, token: &str) -> Result<()>;

    // --- teams: organisations, invites, and environment vault-key grants ---

    /// Create an organisation (the caller becomes its owner).
    fn create_org(&self, org: &NewOrg) -> Result<()>;
    /// List the organisations the caller belongs to.
    fn list_orgs(&self) -> Result<Vec<OrgInfo>>;
    /// Invite an existing user (by email) into an org as a member; returns their id + public key.
    fn invite_member(&self, org_id: &str, email: &str) -> Result<Invited>;
    /// List an org's members (with their public keys, for sealing grants).
    fn list_members(&self, org_id: &str) -> Result<Vec<MemberInfo>>;
    /// Store a member's vault-key grant for an environment (sharing).
    fn create_grant(&self, env_id: &str, user_id: &str, enc_vault_key: &str) -> Result<()>;
    /// Fetch the caller's own vault-key grant for an environment, or `None` if they have none.
    fn get_grant(&self, env_id: &str) -> Result<Option<String>>;
    /// The user ids currently granted an environment (for planning a rotation's re-grants).
    fn list_grant_holders(&self, env_id: &str) -> Result<Vec<String>>;
    /// The ids of an org's environments that `user_id` holds a grant to (for removal-time rotation).
    fn member_env_grants(&self, org_id: &str, user_id: &str) -> Result<Vec<String>>;
    /// Rotate an environment's vault key (rewrapped data keys + replacement grants); new revision.
    fn rotate(&self, env_id: &str, req: &RotateRequest) -> Result<RotateResponse>;
    /// Every retained version of every secret in an environment (the complete history).
    fn list_history(&self, env_id: &str) -> Result<Vec<HistoryRow>>;
    /// An org's audit events, newest first (admin+).
    fn org_audit(&self, org_id: &str, limit: Option<i64>) -> Result<Vec<AuditEvent>>;
    /// An org's plan (tier, trial, limits), visible to any member.
    fn org_entitlements(&self, org_id: &str) -> Result<Entitlements>;
    /// Remove a member from an org (revokes their grants, tokens, and API access).
    fn remove_member(&self, org_id: &str, user_id: &str) -> Result<RemovalReceipt>;
    /// Store (or replace) a member's sealed copy of the org key (display-name access).
    fn grant_org_key(&self, org_id: &str, user_id: &str, enc_org_key: &str) -> Result<()>;
    /// Create a machine token for an environment (public key + sealed grant are client-generated).
    /// `expires_in_days` of `None` takes the server's default lifetime.
    fn create_machine_token(
        &self,
        env_id: &str,
        name: &str,
        public_key: &str,
        enc_vault_key: &str,
        expires_in_days: Option<u32>,
    ) -> Result<CreatedMachineToken>;
    /// The environment's active machine tokens (for listings and rotation re-sealing).
    fn list_machine_tokens(&self, env_id: &str) -> Result<Vec<MachineTokenInfo>>;
    /// Revoke a machine token (its API access dies immediately).
    fn revoke_machine_token(&self, env_id: &str, token_id: &str) -> Result<()>;
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

    #[test]
    fn machine_token_expiry_label_tolerates_older_servers() {
        // A server that predates expiry sends neither field; the listing must still parse.
        let old: MachineTokenInfo = serde_json::from_str(
            r#"{"token_id":"t","name":"ci","public_key":"pk","created_by":null}"#,
        )
        .unwrap();
        assert_eq!(old.expiry_label(), None);

        let new: MachineTokenInfo = serde_json::from_str(
            r#"{"token_id":"t","name":"ci","public_key":"pk","created_by":null,"expires_at":"2026-12-22T10:00:00Z","expires_in_days":90}"#,
        )
        .unwrap();
        assert_eq!(
            new.expiry_label().as_deref(),
            Some("expires 2026-12-22T10:00:00Z (in 90d)")
        );

        let forged: MachineTokenInfo = serde_json::from_str(
            r#"{"token_id":"t","name":"ci","public_key":"pk","created_by":null,"expires_at":"soon\nroot  admin  forged","expires_in_days":1}"#,
        )
        .unwrap();
        let label = forged.expiry_label().unwrap();
        assert!(!label.chars().any(char::is_control), "{label:?}");
    }

    #[test]
    fn eligibility_view_matches_the_server_contract() {
        let view: EligibilityView = serde_json::from_str(
            r#"{
                "model":"person_eligibility_v1",
                "deployment_mode":"cloud",
                "state":"export_only",
                "account_initialized":true,
                "billing_available":true,
                "paid_through_epoch":null,
                "recovery_until_epoch":1760000000,
                "export_until_epoch":1761000000,
                "actions":{"setup":false,"billing":false,"export":true,"revoke":false},
                "next_actions":["export"],
                "payer":"personal"
            }"#,
        )
        .expect("eligibility response should decode");
        assert_eq!(view.model, "person_eligibility_v1");
        assert_eq!(view.state, "export_only");
        assert_eq!(view.next_actions, vec!["export"]);
    }
}
