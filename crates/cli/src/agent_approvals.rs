//! Per-user approvals for project-declared agent tasks.
//!
//! Approval records contain no secret values. SQLite transactions make concurrent CLI updates
//! atomic, while the dedicated directory and database are owner-only on Unix.

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

const DIRECTORY: &str = "task-approvals";
const DATABASE: &str = "approvals.db";
const SCHEMA_VERSION: i64 = 1;
const OPEN_LOCK_RETRY_DELAYS: [Duration; 5] = [
    Duration::from_millis(50),
    Duration::from_millis(100),
    Duration::from_millis(250),
    Duration::from_millis(500),
    Duration::from_secs(1),
];

enum SchemaInitialisationError {
    Sqlite(rusqlite::Error),
    UnsupportedVersion,
}

impl From<rusqlite::Error> for SchemaInitialisationError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

/// Encode the canonical project root without losing non-UTF-8 filesystem bytes.
pub fn project_root_key(path: &Path) -> String {
    filesystem_path_key(path)
}

/// Bind a declaration fingerprint to the exact executable resolved during approval.
pub fn approval_fingerprint(declaration_fingerprint: &str, executable: &Path) -> Result<String> {
    if !valid_fingerprint(declaration_fingerprint) {
        return Err(Error::Input("invalid task declaration fingerprint".into()));
    }
    let mut digest = Sha256::new();
    digest.update(b"sotto-task-approval-v1\0");
    digest.update(declaration_fingerprint.as_bytes());
    digest.update(b"\0");
    digest.update(filesystem_path_key(executable).as_bytes());
    Ok(format!("{:x}", digest.finalize()))
}

fn filesystem_path_key(path: &Path) -> String {
    #[cfg(unix)]
    let bytes = {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    };
    #[cfg(windows)]
    let bytes = {
        use std::os::windows::ffi::OsStrExt;
        path.as_os_str()
            .encode_wide()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>()
    };
    #[cfg(not(any(unix, windows)))]
    let bytes = path.to_string_lossy().as_bytes().to_vec();

    let mut key = String::from("path:");
    for byte in bytes {
        use std::fmt::Write as _;
        write!(key, "{byte:02x}").expect("writing to a String cannot fail");
    }
    key
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Approval {
    pub task_name: String,
    pub environment: String,
    pub fingerprint: String,
}

pub struct ApprovalStore {
    connection: Connection,
}

impl ApprovalStore {
    pub fn open(data_dir: &Path) -> Result<Self> {
        let directory = private_directory(data_dir)?;
        let path = directory.join(DATABASE);
        ensure_database_file(&path)?;
        verify_private_file(&path)?;

        let mut connection = Connection::open(&path)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        match initialise_schema_with_retry(&mut connection) {
            Ok(()) => {}
            Err(SchemaInitialisationError::Sqlite(error)) => return Err(error.into()),
            Err(SchemaInitialisationError::UnsupportedVersion) => {
                return Err(Error::Store(
                    "unsupported task approval data version".into(),
                ))
            }
        }

        Ok(Self { connection })
    }

    pub fn approve(
        &self,
        project_id: &str,
        project_root: &str,
        task_name: &str,
        environment: &str,
        fingerprint: &str,
    ) -> Result<()> {
        validate_scope(project_id, project_root, task_name, environment)?;
        if !valid_fingerprint(fingerprint) {
            return Err(Error::Input("invalid task approval fingerprint".into()));
        }
        self.connection.execute(
            "INSERT INTO approvals (project_id, project_root, task_name, environment, fingerprint)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (project_id, project_root, task_name, environment)
             DO UPDATE SET fingerprint = excluded.fingerprint, created_at = unixepoch()",
            params![
                project_id,
                project_root,
                task_name,
                environment,
                fingerprint
            ],
        )?;
        Ok(())
    }

    pub fn is_approved(
        &self,
        project_id: &str,
        project_root: &str,
        task_name: &str,
        environment: &str,
        fingerprint: &str,
    ) -> Result<bool> {
        validate_scope(project_id, project_root, task_name, environment)?;
        if !valid_fingerprint(fingerprint) {
            return Err(Error::Input("invalid task approval fingerprint".into()));
        }
        let stored: Option<String> = self
            .connection
            .query_row(
                "SELECT fingerprint FROM approvals
                 WHERE project_id = ?1 AND project_root = ?2 AND task_name = ?3 AND environment = ?4",
                params![project_id, project_root, task_name, environment],
                |row| row.get(0),
            )
            .optional()?;
        Ok(stored.as_deref() == Some(fingerprint))
    }

    pub fn list(&self, project_id: &str, project_root: &str) -> Result<Vec<Approval>> {
        if project_id.is_empty() || project_root.is_empty() {
            return Err(Error::Input(
                "invalid task approval project identity".into(),
            ));
        }
        let mut statement = self.connection.prepare(
            "SELECT task_name, environment, fingerprint FROM approvals
             WHERE project_id = ?1 AND project_root = ?2
             ORDER BY task_name, environment",
        )?;
        let rows = statement.query_map(params![project_id, project_root], |row| {
            Ok(Approval {
                task_name: row.get(0)?,
                environment: row.get(1)?,
                fingerprint: row.get(2)?,
            })
        })?;
        let rows = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        if rows.iter().any(|approval| {
            approval.task_name.is_empty()
                || approval.environment.is_empty()
                || !valid_fingerprint(&approval.fingerprint)
        }) {
            return Err(Error::Store("malformed task approval record".into()));
        }
        Ok(rows)
    }

    pub fn revoke(
        &self,
        project_id: &str,
        project_root: &str,
        task_name: &str,
        environment: &str,
    ) -> Result<bool> {
        validate_scope(project_id, project_root, task_name, environment)?;
        let deleted = self.connection.execute(
            "DELETE FROM approvals
             WHERE project_id = ?1 AND project_root = ?2 AND task_name = ?3 AND environment = ?4",
            params![project_id, project_root, task_name, environment],
        )?;
        Ok(deleted != 0)
    }
}

fn initialise_schema_with_retry(
    connection: &mut Connection,
) -> std::result::Result<(), SchemaInitialisationError> {
    for delay in OPEN_LOCK_RETRY_DELAYS {
        match initialise_schema(connection) {
            Ok(()) => return Ok(()),
            Err(SchemaInitialisationError::Sqlite(error)) if is_database_lock_error(&error) => {
                std::thread::sleep(delay);
            }
            Err(error) => return Err(error),
        }
    }

    initialise_schema(connection)
}

fn initialise_schema(
    connection: &mut Connection,
) -> std::result::Result<(), SchemaInitialisationError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version: i64 = transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
    match version {
        0 => {
            transaction.execute_batch(
                "CREATE TABLE IF NOT EXISTS approvals (
                        project_id TEXT NOT NULL CHECK (length(project_id) > 0),
                        project_root TEXT NOT NULL CHECK (length(project_root) > 0),
                        task_name TEXT NOT NULL CHECK (length(task_name) > 0),
                        environment TEXT NOT NULL CHECK (length(environment) > 0),
                        fingerprint TEXT NOT NULL CHECK (
                            length(fingerprint) = 64 AND fingerprint NOT GLOB '*[^0-9a-f]*'
                        ),
                        created_at INTEGER NOT NULL DEFAULT (unixepoch()),
                        PRIMARY KEY (project_id, project_root, task_name, environment)
                    );
                    PRAGMA user_version = 1;",
            )?;
        }
        SCHEMA_VERSION => {}
        _ => return Err(SchemaInitialisationError::UnsupportedVersion),
    }
    transaction.commit()?;
    Ok(())
}

fn is_database_lock_error(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(code, _)
            if matches!(
                code.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            )
    )
}

fn validate_scope(
    project_id: &str,
    project_root: &str,
    task_name: &str,
    environment: &str,
) -> Result<()> {
    if project_id.is_empty()
        || project_root.is_empty()
        || task_name.is_empty()
        || environment.is_empty()
    {
        return Err(Error::Input("invalid task approval identity".into()));
    }
    Ok(())
}

fn valid_fingerprint(fingerprint: &str) -> bool {
    fingerprint.len() == 64
        && fingerprint
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn private_directory(data_dir: &Path) -> Result<PathBuf> {
    fs::create_dir_all(data_dir)
        .map_err(|error| Error::Io(format!("creating {}: {error}", data_dir.display())))?;
    let directory = data_dir.join(DIRECTORY);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        match builder.create(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(Error::Io(format!(
                    "creating {}: {error}",
                    directory.display()
                )))
            }
        }
    }
    #[cfg(not(unix))]
    if !directory.exists() {
        match fs::create_dir(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(Error::Io(format!(
                    "creating {}: {error}",
                    directory.display()
                )))
            }
        }
    }

    let metadata = fs::symlink_metadata(&directory)
        .map_err(|error| Error::Io(format!("reading {}: {error}", directory.display())))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(Error::Io(format!(
            "{} must be a real directory for task approvals",
            directory.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 || mode & 0o700 != 0o700 {
            return Err(Error::Io(format!(
                "{} is accessible by other users; set its permissions to 700",
                directory.display()
            )));
        }
    }
    Ok(directory)
}

fn ensure_database_file(path: &Path) -> Result<()> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(file) => {
            file.sync_all()
                .map_err(|error| Error::Io(format!("initialising {}: {error}", path.display())))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(Error::Io(format!("creating {}: {error}", path.display()))),
    }
    Ok(())
}

fn verify_private_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| Error::Io(format!("reading {}: {error}", path.display())))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(Error::Io(format!(
            "{} must be a regular file for task approvals",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 || mode & 0o600 != 0o600 {
            return Err(Error::Io(format!(
                "{} is accessible by other users; set its permissions to 600",
                path.display()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_matches_only_the_exact_project_task_environment_and_fingerprint() {
        let temp = tempfile::tempdir().unwrap();
        let approvals = ApprovalStore::open(temp.path()).unwrap();
        let fingerprint = "a".repeat(64);
        approvals
            .approve("project-a", "/repo/a", "test", "dev", &fingerprint)
            .unwrap();
        let changed_fingerprint = "b".repeat(64);

        assert!(approvals
            .is_approved("project-a", "/repo/a", "test", "dev", &fingerprint)
            .unwrap());
        for key in [
            ("project-b", "/repo/a", "test", "dev", fingerprint.as_str()),
            ("project-a", "/repo/b", "test", "dev", fingerprint.as_str()),
            ("project-a", "/repo/a", "other", "dev", fingerprint.as_str()),
            (
                "project-a",
                "/repo/a",
                "test",
                "stage",
                fingerprint.as_str(),
            ),
            (
                "project-a",
                "/repo/a",
                "test",
                "dev",
                changed_fingerprint.as_str(),
            ),
        ] {
            assert!(!approvals
                .is_approved(key.0, key.1, key.2, key.3, key.4)
                .unwrap());
        }
    }

    #[test]
    fn empty_environment_is_rejected_by_every_mutating_or_matching_operation() {
        let temp = tempfile::tempdir().unwrap();
        let approvals = ApprovalStore::open(temp.path()).unwrap();
        let fingerprint = "a".repeat(64);

        assert!(approvals
            .approve("project-a", "/repo/a", "test", "", &fingerprint)
            .is_err());
        assert!(approvals
            .is_approved("project-a", "/repo/a", "test", "", &fingerprint)
            .is_err());
        assert!(approvals
            .revoke("project-a", "/repo/a", "test", "")
            .is_err());
    }

    #[test]
    fn approval_fingerprint_changes_when_the_resolved_executable_changes() {
        let declaration = "a".repeat(64);
        let first = approval_fingerprint(&declaration, Path::new("/usr/bin/tool-v1")).unwrap();
        let second = approval_fingerprint(&declaration, Path::new("/opt/tools/tool-v1")).unwrap();

        assert_ne!(first, second);
        assert_eq!(first.len(), 64);
    }

    #[test]
    fn approvals_can_be_listed_and_revoked_without_affecting_another_environment() {
        let temp = tempfile::tempdir().unwrap();
        let approvals = ApprovalStore::open(temp.path()).unwrap();
        let fingerprint = "c".repeat(64);
        approvals
            .approve("project-a", "/repo/a", "test", "dev", &fingerprint)
            .unwrap();
        approvals
            .approve("project-a", "/repo/a", "test", "stage", &fingerprint)
            .unwrap();

        let rows = approvals.list("project-a", "/repo/a").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].environment, "dev");
        assert!(approvals
            .revoke("project-a", "/repo/a", "test", "dev")
            .unwrap());
        assert!(!approvals
            .is_approved("project-a", "/repo/a", "test", "dev", &fingerprint)
            .unwrap());
        assert!(approvals
            .is_approved("project-a", "/repo/a", "test", "stage", &fingerprint)
            .unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn approval_files_are_owner_only_and_insecure_permissions_fail_closed() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let approvals = ApprovalStore::open(temp.path()).unwrap();
        drop(approvals);
        let directory = temp.path().join(DIRECTORY);
        let database = directory.join(DATABASE);
        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&database).unwrap().permissions().mode() & 0o777,
            0o600
        );

        fs::set_permissions(&database, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(ApprovalStore::open(temp.path()).is_err());
    }

    #[test]
    fn unreadable_approval_database_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join(DIRECTORY);
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join(DATABASE), b"not a sqlite database").unwrap();

        assert!(ApprovalStore::open(temp.path()).is_err());
    }

    #[test]
    fn concurrent_cli_writers_preserve_every_approval() {
        let worker_count = 16;
        for _ in 0..8 {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().to_path_buf();
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(worker_count));
            let workers = (0..worker_count)
                .map(|index| {
                    let path = path.clone();
                    let barrier = barrier.clone();
                    std::thread::spawn(move || {
                        barrier.wait();
                        ApprovalStore::open(&path)
                            .unwrap()
                            .approve(
                                "project-a",
                                "/repo/a",
                                &format!("task-{index}"),
                                "dev",
                                &"d".repeat(64),
                            )
                            .unwrap();
                    })
                })
                .collect::<Vec<_>>();
            for worker in workers {
                worker.join().unwrap();
            }

            assert_eq!(
                ApprovalStore::open(temp.path())
                    .unwrap()
                    .list("project-a", "/repo/a")
                    .unwrap()
                    .len(),
                worker_count
            );
        }
    }
}
