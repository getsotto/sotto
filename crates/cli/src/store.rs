//! Local encrypted store (SQLite).
//!
//! Holds ciphertext blobs (secret names/values/data-keys are opaque here) plus structural
//! metadata. The schema mirrors the server's so M3 sync is additive. Secrets are addressed by a
//! stable id, not by name — names are encrypted, so the vault layer resolves name→id by
//! decrypting (the store never sees plaintext).

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use uuid::Uuid;

use crate::error::{Error, Result};

const SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS identity (
    id         INTEGER PRIMARY KEY CHECK (id = 1),
    kdf_salt   BLOB NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS projects (
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS environments (
    id            TEXT PRIMARY KEY,
    project_id    TEXT NOT NULL REFERENCES projects(id),
    name          TEXT NOT NULL,
    enc_vault_key BLOB NOT NULL,
    created_at    INTEGER NOT NULL,
    UNIQUE (project_id, name)
);
CREATE TABLE IF NOT EXISTS secrets (
    id            TEXT PRIMARY KEY,
    env_id        TEXT NOT NULL REFERENCES environments(id),
    enc_name      BLOB NOT NULL,
    enc_value     BLOB NOT NULL,
    enc_data_key  BLOB NOT NULL,
    version       INTEGER NOT NULL,
    deleted_at    INTEGER,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS secret_versions (
    id           TEXT PRIMARY KEY,
    secret_id    TEXT NOT NULL REFERENCES secrets(id),
    version      INTEGER NOT NULL,
    enc_name     BLOB NOT NULL,
    enc_value    BLOB NOT NULL,
    enc_data_key BLOB NOT NULL,
    created_at   INTEGER NOT NULL,
    UNIQUE (secret_id, version)
);
";

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn new_id() -> String {
    Uuid::new_v4().to_string()
}

/// A local project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub id: String,
    pub name: String,
}

/// An environment within a project (holds the vault key wrapped under the master key).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Environment {
    pub id: String,
    pub project_id: String,
    pub name: String,
    pub enc_vault_key: Vec<u8>,
}

/// A stored secret row — all payload fields are opaque ciphertext.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretRow {
    pub id: String,
    pub env_id: String,
    pub enc_name: Vec<u8>,
    pub enc_value: Vec<u8>,
    pub enc_data_key: Vec<u8>,
    pub version: i64,
}

/// One retained version of a secret (all payload fields opaque ciphertext).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretVersion {
    pub version: i64,
    pub enc_name: Vec<u8>,
    pub enc_value: Vec<u8>,
    pub enc_data_key: Vec<u8>,
}

/// The local SQLite store.
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open (creating + migrating) the store at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        Self::init(Connection::open(path)?)
    }

    /// Open an ephemeral in-memory store (tests).
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    // --- identity (the local KDF salt; the secret key lives in the OS keychain) ---

    pub fn put_identity_salt(&self, kdf_salt: &[u8]) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO identity (id, kdf_salt, created_at) VALUES (1, ?1, ?2)",
            params![kdf_salt, now_ms()],
        )?;
        Ok(())
    }

    pub fn identity_salt(&self) -> Result<Option<Vec<u8>>> {
        self.conn
            .query_row("SELECT kdf_salt FROM identity WHERE id = 1", [], |r| {
                r.get(0)
            })
            .optional()
            .map_err(Into::into)
    }

    // --- projects ---

    pub fn create_project(&self, name: &str) -> Result<Project> {
        let id = new_id();
        self.conn.execute(
            "INSERT INTO projects (id, name, created_at) VALUES (?1, ?2, ?3)",
            params![id, name, now_ms()],
        )?;
        Ok(Project {
            id,
            name: name.to_string(),
        })
    }

    pub fn get_project(&self, id: &str) -> Result<Option<Project>> {
        self.conn
            .query_row(
                "SELECT id, name FROM projects WHERE id = ?1",
                params![id],
                |r| {
                    Ok(Project {
                        id: r.get(0)?,
                        name: r.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    // --- environments ---

    pub fn create_environment(
        &self,
        id: &str,
        project_id: &str,
        name: &str,
        enc_vault_key: &[u8],
    ) -> Result<Environment> {
        self.conn.execute(
            "INSERT INTO environments (id, project_id, name, enc_vault_key, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, project_id, name, enc_vault_key, now_ms()],
        )?;
        Ok(Environment {
            id: id.to_string(),
            project_id: project_id.to_string(),
            name: name.to_string(),
            enc_vault_key: enc_vault_key.to_vec(),
        })
    }

    pub fn get_environment(&self, project_id: &str, name: &str) -> Result<Option<Environment>> {
        self.conn
            .query_row(
                "SELECT id, project_id, name, enc_vault_key FROM environments
                 WHERE project_id = ?1 AND name = ?2",
                params![project_id, name],
                |r| {
                    Ok(Environment {
                        id: r.get(0)?,
                        project_id: r.get(1)?,
                        name: r.get(2)?,
                        enc_vault_key: r.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn list_environments(&self, project_id: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT name FROM environments WHERE project_id = ?1 ORDER BY name")?;
        let rows = stmt.query_map(params![project_id], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // --- secrets (id-based; names are opaque blobs resolved by the vault layer) ---

    /// List non-deleted secret rows for an environment.
    pub fn list_secrets(&self, env_id: &str) -> Result<Vec<SecretRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, env_id, enc_name, enc_value, enc_data_key, version
             FROM secrets WHERE env_id = ?1 AND deleted_at IS NULL",
        )?;
        let rows = stmt.query_map(params![env_id], |r| {
            Ok(SecretRow {
                id: r.get(0)?,
                env_id: r.get(1)?,
                enc_name: r.get(2)?,
                enc_value: r.get(3)?,
                enc_data_key: r.get(4)?,
                version: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Insert a new secret (version 1) with a caller-supplied id, snapshotting history.
    pub fn insert_secret(
        &self,
        id: &str,
        env_id: &str,
        enc_name: &[u8],
        enc_value: &[u8],
        enc_data_key: &[u8],
    ) -> Result<SecretRow> {
        let ts = now_ms();
        // The row insert and its history snapshot must be all-or-nothing: a failure between them
        // would leave a secret with no version history. `unchecked_transaction` works on `&self`.
        let tx = self.conn.unchecked_transaction()?;
        self.conn.execute(
            "INSERT INTO secrets
                (id, env_id, enc_name, enc_value, enc_data_key, version, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?6)",
            params![id, env_id, enc_name, enc_value, enc_data_key, ts],
        )?;
        self.snapshot(id, 1, enc_name, enc_value, enc_data_key, ts)?;
        tx.commit()?;
        Ok(SecretRow {
            id: id.to_string(),
            env_id: env_id.to_string(),
            enc_name: enc_name.to_vec(),
            enc_value: enc_value.to_vec(),
            enc_data_key: enc_data_key.to_vec(),
            version: 1,
        })
    }

    /// Update an existing secret to `new_version` (which must be the current version + 1),
    /// snapshotting history and un-deleting any tombstone.
    ///
    /// Returns [`Error::NotFound`] if the secret is absent, or [`Error::Conflict`] if
    /// `new_version` doesn't immediately follow the stored version (optimistic concurrency).
    pub fn update_secret(
        &self,
        id: &str,
        new_version: i64,
        enc_name: &[u8],
        enc_value: &[u8],
        enc_data_key: &[u8],
    ) -> Result<()> {
        // Read-check-write-snapshot as one unit, so a mid-update failure can't advance the
        // version without recording history. `unchecked_transaction` works on `&self`.
        let tx = self.conn.unchecked_transaction()?;
        let current: i64 = self
            .conn
            .query_row(
                "SELECT version FROM secrets WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(id.to_string()))?;
        if new_version != current + 1 {
            return Err(Error::Conflict(format!(
                "secret {id}: expected version {}, got {new_version}",
                current + 1
            )));
        }
        let ts = now_ms();
        // Compare-and-swap on the version column: gate the UPDATE on the version we just read so
        // a racing writer (a second process on the same file) that already bumped it updates 0
        // rows here and gets a deterministic Conflict, rather than slipping through to a
        // secret_versions UNIQUE violation (which would surface as an opaque Error::Store).
        let updated = self.conn.execute(
            "UPDATE secrets
             SET enc_name = ?2, enc_value = ?3, enc_data_key = ?4, version = ?5,
                 updated_at = ?6, deleted_at = NULL
             WHERE id = ?1 AND version = ?7",
            params![
                id,
                enc_name,
                enc_value,
                enc_data_key,
                new_version,
                ts,
                current
            ],
        )?;
        if updated == 0 {
            return Err(Error::Conflict(format!(
                "secret {id}: version changed concurrently (expected {current})"
            )));
        }
        self.snapshot(id, new_version, enc_name, enc_value, enc_data_key, ts)?;
        tx.commit()?;
        Ok(())
    }

    /// Soft-delete a secret (tombstone). History is retained.
    pub fn delete_secret(&self, id: &str) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE secrets SET deleted_at = ?2 WHERE id = ?1 AND deleted_at IS NULL",
            params![id, now_ms()],
        )?;
        if n == 0 {
            return Err(Error::NotFound(id.to_string()));
        }
        Ok(())
    }

    /// Return the retained version history of a secret, oldest first.
    pub fn secret_versions(&self, secret_id: &str) -> Result<Vec<SecretVersion>> {
        let mut stmt = self.conn.prepare(
            "SELECT version, enc_name, enc_value, enc_data_key
             FROM secret_versions WHERE secret_id = ?1 ORDER BY version",
        )?;
        let rows = stmt.query_map(params![secret_id], |r| {
            Ok(SecretVersion {
                version: r.get(0)?,
                enc_name: r.get(1)?,
                enc_value: r.get(2)?,
                enc_data_key: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn snapshot(
        &self,
        secret_id: &str,
        version: i64,
        enc_name: &[u8],
        enc_value: &[u8],
        enc_data_key: &[u8],
        ts: i64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO secret_versions
                (id, secret_id, version, enc_name, enc_value, enc_data_key, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                new_id(),
                secret_id,
                version,
                enc_name,
                enc_value,
                enc_data_key,
                ts
            ],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_salt_round_trips() {
        let s = Store::open_in_memory().unwrap();
        assert!(s.identity_salt().unwrap().is_none());
        s.put_identity_salt(b"sixteen-byte-slt").unwrap();
        assert_eq!(s.identity_salt().unwrap().unwrap(), b"sixteen-byte-slt");
    }

    #[test]
    fn environments_are_unique_per_project_and_listable() {
        let s = Store::open_in_memory().unwrap();
        let p = s.create_project("acme").unwrap();
        s.create_environment("e-dev", &p.id, "dev", b"k1").unwrap();
        s.create_environment("e-prod", &p.id, "prod", b"k2")
            .unwrap();
        assert_eq!(s.list_environments(&p.id).unwrap(), vec!["dev", "prod"]);
        assert!(s.get_environment(&p.id, "prod").unwrap().is_some());
        assert!(s.get_environment(&p.id, "missing").unwrap().is_none());
        // duplicate (project, name) violates the UNIQUE constraint
        assert!(s.create_environment("e-dup", &p.id, "dev", b"k3").is_err());
    }

    #[test]
    fn secret_lifecycle_keeps_version_history() {
        let s = Store::open_in_memory().unwrap();
        let p = s.create_project("acme").unwrap();
        let e = s.create_environment("e1", &p.id, "dev", b"vault").unwrap();

        let row = s
            .insert_secret("sec-1", &e.id, b"n1", b"v1", b"dk1")
            .unwrap();
        assert_eq!(row.version, 1);
        assert_eq!(s.list_secrets(&e.id).unwrap().len(), 1);

        s.update_secret(&row.id, 2, b"n1", b"v2", b"dk2").unwrap();
        assert_eq!(s.list_secrets(&e.id).unwrap()[0].version, 2);

        let history = s.secret_versions(&row.id).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].enc_value, b"v1");
        assert_eq!(history[1].enc_value, b"v2");

        // soft delete hides it from listing but retains history
        s.delete_secret(&row.id).unwrap();
        assert!(s.list_secrets(&e.id).unwrap().is_empty());
        assert_eq!(s.secret_versions(&row.id).unwrap().len(), 2);
    }

    #[test]
    fn update_missing_secret_is_not_found() {
        let s = Store::open_in_memory().unwrap();
        assert!(matches!(
            s.update_secret("nope", 2, b"n", b"v", b"dk"),
            Err(Error::NotFound(_))
        ));
    }

    #[test]
    fn update_with_nonconsecutive_version_conflicts() {
        let s = Store::open_in_memory().unwrap();
        let p = s.create_project("p").unwrap();
        let e = s.create_environment("e1", &p.id, "dev", b"v").unwrap();
        let row = s.insert_secret("sec-1", &e.id, b"n", b"v", b"dk").unwrap();
        assert!(matches!(
            s.update_secret(&row.id, 3, b"n", b"v2", b"dk2"),
            Err(Error::Conflict(_))
        ));
    }
}
