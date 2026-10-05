//! Client-side assembly of the versioned Cloud exit export.
//!
//! The bundle is still encrypted server material: this module never asks the server for a
//! password, master key, plaintext name, or plaintext secret. It verifies the chunk envelope and
//! refuses to write a partial or scope-incomplete export.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::keychain::Keychain;
use crate::session;
use crate::store::{AccountKeys, Store, SyncSecret};

use super::api::{
    AccountBundle, ExportChunk, ExportEnvironment, ExportManifest, ExportProject, SyncApi,
};

pub const EXPORT_VERSION: i32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExportBundle {
    pub version: i32,
    pub manifest_hash: String,
    pub account: AccountBundle,
    pub projects: Vec<ExportProject>,
    pub environments: Vec<ExportEnvironment>,
}

#[derive(Serialize)]
struct ExportManifestForHash {
    version: i32,
    account: AccountBundle,
    projects: Vec<ExportProject>,
    environments: Vec<super::api::ExportEnvironmentRef>,
    omitted_environment_count: usize,
}

/// Download every bounded chunk and assemble one complete, self-contained opaque bundle.
pub fn download(api: &dyn SyncApi) -> Result<ExportBundle> {
    let manifest = api.start_export()?;
    if manifest.version != EXPORT_VERSION {
        return Err(Error::Server(format!(
            "unsupported export version {}",
            manifest.version
        )));
    }
    if !manifest.complete || manifest.omitted_environment_count != 0 {
        return Err(Error::Server(
            "export scope is incomplete; restore is refused until every authorised environment is included"
                .into(),
        ));
    }
    if manifest.total_chunks != manifest.environments.len() + 1 {
        return Err(Error::Server(
            "export manifest has an invalid chunk count".into(),
        ));
    }

    let mut account = None;
    let mut environments = BTreeMap::new();
    for index in 0..manifest.total_chunks {
        let chunk = api.export_chunk(&manifest.export_id, index)?;
        validate_chunk(&manifest, &chunk, index)?;
        if index == 0 {
            account = chunk.account;
        } else if let Some(environment) = chunk.environment {
            let Some(reference) = manifest
                .environments
                .iter()
                .find(|reference| reference.id == environment.id)
            else {
                return Err(Error::Server(
                    "export chunk returned an unknown environment".into(),
                ));
            };
            if reference.project_id != environment.project_id
                || reference.enc_name != environment.enc_name
                || reference.enc_vault_key != environment.enc_vault_key
                || reference.revision != environment.revision
            {
                return Err(Error::Server(
                    "export chunk does not match its manifest environment".into(),
                ));
            }
            environments.insert(environment.id.clone(), environment);
        } else {
            return Err(Error::Server("export chunk omitted its environment".into()));
        }
    }

    let account = account.ok_or_else(|| Error::Server("export omitted account material".into()))?;
    if environments.len() != manifest.environments.len()
        || manifest
            .environments
            .iter()
            .any(|reference| !environments.contains_key(&reference.id))
    {
        return Err(Error::Server(
            "export did not return every manifest environment".into(),
        ));
    }
    let environments = manifest
        .environments
        .iter()
        .map(|reference| {
            environments
                .get(&reference.id)
                .cloned()
                .ok_or_else(|| Error::Server("export environment disappeared".into()))
        })
        .collect::<Result<Vec<_>>>()?;
    let bundle = ExportBundle {
        version: EXPORT_VERSION,
        manifest_hash: manifest.manifest_hash,
        account,
        projects: manifest.projects,
        environments,
    };
    verify_bundle_hash(&bundle, false)?;
    Ok(bundle)
}

fn bundle_hash(bundle: &ExportBundle) -> Result<String> {
    let environments = bundle
        .environments
        .iter()
        .map(|environment| super::api::ExportEnvironmentRef {
            id: environment.id.clone(),
            project_id: environment.project_id.clone(),
            enc_name: environment.enc_name.clone(),
            enc_vault_key: environment.enc_vault_key.clone(),
            revision: environment.revision,
        })
        .collect();
    let manifest = ExportManifestForHash {
        version: bundle.version,
        account: bundle.account.clone(),
        projects: bundle.projects.clone(),
        environments,
        omitted_environment_count: 0,
    };
    let encoded = serde_json::to_vec(&manifest)
        .map_err(|e| Error::Server(format!("serialising export manifest: {e}")))?;
    Ok(super::api::b64encode(&Sha256::digest(encoded)))
}

fn verify_bundle_hash(bundle: &ExportBundle, input: bool) -> Result<()> {
    let computed = bundle_hash(bundle).map_err(|error| {
        if input {
            Error::Input(error.to_string())
        } else {
            error
        }
    })?;
    if computed != bundle.manifest_hash {
        return Err(if input {
            Error::Input("export manifest hash does not match its contents".into())
        } else {
            Error::Server("export manifest hash does not match its contents".into())
        });
    }
    Ok(())
}

fn validate_chunk(manifest: &ExportManifest, chunk: &ExportChunk, index: usize) -> Result<()> {
    if chunk.version != manifest.version
        || chunk.export_id != manifest.export_id
        || chunk.manifest_hash != manifest.manifest_hash
        || chunk.index != index
        || chunk.total_chunks != manifest.total_chunks
    {
        return Err(Error::Server(
            "export chunk does not match its manifest".into(),
        ));
    }
    if index + 1 == manifest.total_chunks && !chunk.complete {
        return Err(Error::Server("export did not declare completion".into()));
    }
    Ok(())
}

pub fn write(bundle: &ExportBundle, path: &Path) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(bundle)
        .map_err(|e| Error::Input(format!("serialising export: {e}")))?;
    std::fs::write(path, bytes).map_err(|e| Error::Io(e.to_string()))
}

pub fn read(path: &Path) -> Result<ExportBundle> {
    let bytes = std::fs::read(path).map_err(|e| Error::Io(e.to_string()))?;
    let bundle: ExportBundle = serde_json::from_slice(&bytes)
        .map_err(|e| Error::Input(format!("invalid export bundle: {e}")))?;
    if bundle.version != EXPORT_VERSION {
        return Err(Error::Input(format!(
            "unsupported export version {}",
            bundle.version
        )));
    }
    verify_bundle_hash(&bundle, true)?;
    Ok(bundle)
}

/// Restore an opaque Cloud export into a fresh local store.
///
/// The bundle contains ciphertext and wrapped keys only. Resource ids are retained so later
/// synchronisation can reconcile the restored rows with the server without creating duplicates.
pub fn restore(
    store: &Store,
    keychain: &dyn Keychain,
    bundle: &ExportBundle,
    secret_key: &[u8],
    password: &[u8],
    ttl: Duration,
) -> Result<()> {
    let params = super::api::b64decode(&bundle.account.kdf_params)
        .and_then(|bytes| crate::account::KdfParams::from_bytes(&bytes))?;
    let salt: [u8; sotto_core::kdf::SALT_LEN] = params
        .salt
        .as_slice()
        .try_into()
        .map_err(|_| Error::Crypto)?;
    let account_keys = AccountKeys {
        public_key: super::api::b64decode(&bundle.account.public_key)?,
        enc_private_keys: super::api::b64decode(&bundle.account.enc_private_keys)?,
        recovery_blob: super::api::b64decode(&bundle.account.recovery_blob)?,
    };
    let mut decoded = Vec::with_capacity(bundle.environments.len());
    for environment in &bundle.environments {
        let mut secrets = Vec::with_capacity(environment.secrets.len());
        for secret in &environment.secrets {
            secrets.push(SyncSecret {
                id: secret.id.clone(),
                enc_name: super::api::b64decode(&secret.enc_name)?,
                enc_value: super::api::b64decode(&secret.enc_value)?,
                enc_data_key: super::api::b64decode(&secret.enc_data_key)?,
                version: secret.version,
                deleted: secret.deleted,
            });
        }
        let mut history = Vec::with_capacity(environment.history.len());
        for row in &environment.history {
            history.push((
                row.secret_id.clone(),
                row.version,
                super::api::b64decode(&row.enc_name)?,
                super::api::b64decode(&row.enc_value)?,
                super::api::b64decode(&row.enc_data_key)?,
            ));
        }
        decoded.push((environment, secrets, history));
    }

    session::restore(
        store,
        keychain,
        password,
        secret_key,
        &salt,
        &account_keys,
        ttl,
    )?;

    for project in &bundle.projects {
        if store.get_project(&project.id)?.is_none() {
            store.create_project_with_id(&project.id, &project.id)?;
        }
    }
    for (environment, secrets, history) in decoded {
        if store.find_environment(&environment.id)?.is_none() {
            store.create_environment(
                &environment.id,
                &environment.project_id,
                &environment.id,
                &super::api::b64decode(&environment.enc_vault_key)?,
            )?;
        }
        for secret in &secrets {
            store.put_remote_secret(&environment.id, secret)?;
        }
        for (secret_id, version, enc_name, enc_value, enc_data_key) in history {
            store.put_remote_history(&secret_id, version, &enc_name, &enc_value, &enc_data_key)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::api::{ExportEnvironmentRef, ExportHistory, ExportSecret};

    fn manifest() -> ExportManifest {
        ExportManifest {
            version: 1,
            export_id: "e".into(),
            manifest_hash: "h".into(),
            expires_at: "later".into(),
            total_chunks: 2,
            complete: true,
            projects: vec![ExportProject {
                id: "p".into(),
                enc_name: "name".into(),
                org_id: None,
            }],
            environments: vec![ExportEnvironmentRef {
                id: "env".into(),
                project_id: "p".into(),
                enc_name: "env-name".into(),
                enc_vault_key: "grant".into(),
                revision: 2,
            }],
            omitted_environment_count: 0,
        }
    }

    #[test]
    fn rejects_a_chunk_from_another_export() {
        let m = manifest();
        let chunk = ExportChunk {
            version: 1,
            export_id: "other".into(),
            manifest_hash: "h".into(),
            index: 0,
            total_chunks: 2,
            complete: false,
            account: None,
            environment: None,
        };
        assert!(validate_chunk(&m, &chunk, 0).is_err());
    }

    #[test]
    fn bundle_round_trips_without_plaintext_fields() {
        let mut bundle = ExportBundle {
            version: 1,
            manifest_hash: String::new(),
            account: AccountBundle {
                public_key: "pk".into(),
                enc_private_keys: "keys".into(),
                kdf_params: "kdf".into(),
                recovery_blob: "recovery".into(),
            },
            projects: manifest().projects,
            environments: vec![ExportEnvironment {
                id: "env".into(),
                project_id: "p".into(),
                enc_name: "name".into(),
                enc_vault_key: "grant".into(),
                revision: 2,
                secrets: vec![ExportSecret {
                    id: "s".into(),
                    enc_name: "n".into(),
                    enc_value: "v".into(),
                    enc_data_key: "k".into(),
                    version: 1,
                    deleted: false,
                }],
                history: vec![ExportHistory {
                    secret_id: "s".into(),
                    version: 1,
                    enc_name: "n".into(),
                    enc_value: "v".into(),
                    enc_data_key: "k".into(),
                }],
            }],
        };
        bundle.manifest_hash = bundle_hash(&bundle).unwrap();
        let bytes = serde_json::to_vec(&bundle).unwrap();
        let decoded: ExportBundle = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(decoded, bundle);
        verify_bundle_hash(&decoded, true).unwrap();
        assert!(!String::from_utf8(bytes).unwrap().contains("plaintext"));
    }
}
