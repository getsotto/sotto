//! Project-shared, secret-free declarations for named agent tasks.
//!
//! This module only parses and validates declarations. It never resolves secrets or starts a
//! process; later approval and execution layers consume the validated task representation.

use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::error::{Error, Result};

pub const TASKS_FILE: &str = "sotto.tasks.toml";
pub const TASKS_VERSION: u32 = 1;

const MAX_TASKS: usize = 128;
const MAX_ENVIRONMENTS_PER_TASK: usize = 64;
const MAX_ARGUMENTS: usize = 128;
const MAX_TEXT_LENGTH: usize = 4096;
const MAX_TASK_NAME_LENGTH: usize = 64;
const MAX_DESCRIPTION_LENGTH: usize = 256;
pub const DEFAULT_TASK_TIMEOUT_SECONDS: u64 = 600;
pub const MAX_TASK_TIMEOUT_SECONDS: u64 = 3600;

/// A validated manifest stored at the Sotto project root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskManifest {
    version: u32,
    tasks: Vec<TaskDeclaration>,
}

/// One fixed executable and argument vector that a developer may later approve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskDeclaration {
    name: String,
    description: String,
    executable: String,
    args: Vec<String>,
    environments: Vec<String>,
    working_directory: Option<String>,
    timeout_seconds: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTaskManifest {
    version: u32,
    #[serde(rename = "task")]
    tasks: Vec<RawTaskDeclaration>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTaskDeclaration {
    name: String,
    description: String,
    executable: String,
    #[serde(default)]
    args: Vec<String>,
    environments: Vec<String>,
    #[serde(default)]
    working_directory: Option<String>,
    #[serde(default)]
    timeout_seconds: Option<u64>,
}

impl From<RawTaskManifest> for TaskManifest {
    fn from(raw: RawTaskManifest) -> Self {
        Self {
            version: raw.version,
            tasks: raw.tasks.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<RawTaskDeclaration> for TaskDeclaration {
    fn from(raw: RawTaskDeclaration) -> Self {
        Self {
            name: raw.name,
            description: raw.description,
            executable: raw.executable,
            args: raw.args,
            environments: raw.environments,
            working_directory: raw.working_directory,
            timeout_seconds: raw.timeout_seconds,
        }
    }
}

impl TaskManifest {
    /// Find the nearest Sotto project, then load its task manifest from that project root.
    pub fn discover(start: &Path) -> Result<(PathBuf, Self)> {
        let (_, project_root) = Config::discover(start)?;
        let manifest = Self::load_from(&project_root)?;
        Ok((project_root, manifest))
    }

    pub fn version(&self) -> u32 {
        self.version
    }

    pub fn tasks(&self) -> &[TaskDeclaration] {
        &self.tasks
    }

    /// Load and validate `sotto.tasks.toml` from a known Sotto project root.
    pub fn load_from(project_root: &Path) -> Result<Self> {
        let path = project_root.join(TASKS_FILE);
        let text = std::fs::read_to_string(&path).map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => Error::NotFound(format!(
                "{} (create this file to declare agent tasks)",
                path.display()
            )),
            _ => Error::Io(format!("reading {}: {error}", path.display())),
        })?;

        let manifest = parse_manifest(&path, &text)?;
        manifest.validate().map_err(|error| match error {
            Error::Config(message) => Error::Config(format!("{}: {message}", path.display())),
            other => other,
        })?;
        Ok(manifest)
    }

    /// Parse and validate TOML, including the file path in configuration errors.
    pub fn parse(path: &Path, text: &str) -> Result<Self> {
        let manifest = parse_manifest(path, text)?;
        manifest.validate().map_err(|error| match error {
            Error::Config(message) => Error::Config(format!("{}: {message}", path.display())),
            other => other,
        })?;
        Ok(manifest)
    }

    fn validate(&self) -> Result<()> {
        if self.version != TASKS_VERSION {
            return Err(Error::Config(format!(
                "unsupported task file version {}; this CLI supports version {TASKS_VERSION}",
                self.version
            )));
        }
        if self.tasks.is_empty() {
            return Err(Error::Config("at least one task must be declared".into()));
        }
        if self.tasks.len() > MAX_TASKS {
            return Err(Error::Config(format!(
                "task file may declare at most {MAX_TASKS} tasks"
            )));
        }

        let mut task_names = HashSet::with_capacity(self.tasks.len());
        for (index, task) in self.tasks.iter().enumerate() {
            validate_task(task, index)?;
            if !task_names.insert(task.name.as_str()) {
                return Err(task_error(index, "name", "duplicates an earlier task name"));
            }
        }
        Ok(())
    }

    /// Fingerprint one named task together with the manifest schema version.
    pub fn fingerprint_for(&self, name: &str) -> Result<String> {
        let task = self
            .tasks
            .iter()
            .find(|task| task.name == name)
            .ok_or_else(|| Error::NotFound(format!("agent task `{name}`")))?;
        let canonical = serde_json::to_vec(&VersionedTask {
            version: self.version,
            task,
        })
        .map_err(|error| {
            Error::Config(format!("could not canonicalise task declaration: {error}"))
        })?;
        Ok(format!("{:x}", Sha256::digest(canonical)))
    }
}

impl TaskDeclaration {
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn description(&self) -> &str {
        &self.description
    }
    pub fn executable(&self) -> &str {
        &self.executable
    }
    pub fn args(&self) -> &[String] {
        &self.args
    }
    pub fn environments(&self) -> &[String] {
        &self.environments
    }
    pub fn timeout_seconds(&self) -> Option<u64> {
        self.timeout_seconds
    }
    pub fn working_directory_declaration(&self) -> Option<&str> {
        self.working_directory.as_deref()
    }

    /// Resolve a declared executable path against the project root, or mark a bare name for
    /// lookup on `PATH` when the task is eventually launched.
    pub fn executable_target(&self, project_root: &Path) -> ExecutableTarget {
        let executable = Path::new(&self.executable);
        let is_bare_name = !executable.is_absolute()
            && executable.components().count() == 1
            && executable.file_name().is_some();

        if is_bare_name {
            ExecutableTarget::SearchPath(self.executable.clone())
        } else if executable.is_absolute() {
            ExecutableTarget::Path(executable.to_path_buf())
        } else {
            ExecutableTarget::Path(project_root.join(executable))
        }
    }

    /// Resolve the declared working directory against the project root without shell or
    /// environment-variable expansion. The default is the project root.
    pub fn working_directory(&self, project_root: &Path) -> PathBuf {
        match &self.working_directory {
            Some(directory) => {
                let path = Path::new(directory);
                if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    project_root.join(path)
                }
            }
            None => project_root.to_path_buf(),
        }
    }

    /// Resolve the declared executable to the concrete file that would be launched.
    pub fn resolve_executable(&self, project_root: &Path) -> Result<PathBuf> {
        match self.executable_target(project_root) {
            ExecutableTarget::Path(path) => canonical_executable(&path),
            ExecutableTarget::SearchPath(name) => {
                let search_path = env::var_os("PATH").ok_or_else(|| {
                    Error::NotFound(format!(
                        "task executable {name:?} cannot be resolved without PATH"
                    ))
                })?;
                self.resolve_executable_with_path(project_root, &name, &search_path)
            }
        }
    }

    fn resolve_executable_with_path(
        &self,
        project_root: &Path,
        name: &str,
        search_path: &std::ffi::OsStr,
    ) -> Result<PathBuf> {
        let working_directory = self.working_directory(project_root);
        for directory in env::split_paths(search_path) {
            let directory = if directory.as_os_str().is_empty() {
                working_directory.clone()
            } else if directory.is_absolute() {
                directory
            } else {
                working_directory.join(directory)
            };
            for candidate in executable_candidates(&directory, name) {
                if let Ok(resolved) = canonical_executable(&candidate) {
                    return Ok(resolved);
                }
            }
        }
        Err(Error::NotFound(format!(
            "task executable {name:?} could not be resolved on PATH"
        )))
    }
}

fn canonical_executable(path: &Path) -> Result<PathBuf> {
    let resolved = fs::canonicalize(path)
        .map_err(|_| Error::NotFound(format!("task executable {:?} was not found", path)))?;
    let metadata = fs::metadata(&resolved)
        .map_err(|_| Error::NotFound(format!("task executable {:?} was not found", path)))?;
    if !metadata.is_file() || !is_executable(&metadata) {
        return Err(Error::NotFound(format!(
            "task executable {:?} is not an executable file",
            path
        )));
    }
    Ok(resolved)
}

#[cfg(unix)]
fn is_executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_: &fs::Metadata) -> bool {
    true
}

fn executable_candidates(directory: &Path, name: &str) -> Vec<PathBuf> {
    let base = directory.join(name);
    #[cfg(windows)]
    {
        if base.extension().is_some() {
            return vec![base];
        }
        let extensions = env::var_os("PATHEXT").unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".into());
        return std::env::split_paths(&extensions)
            .map(|extension| {
                let suffix = extension.to_string_lossy();
                PathBuf::from(format!("{}{}", base.display(), suffix))
            })
            .collect();
    }
    #[cfg(not(windows))]
    vec![base]
}

fn parse_manifest(path: &Path, text: &str) -> Result<TaskManifest> {
    let raw: RawTaskManifest = toml::from_str(text).map_err(|error| {
        let offset = error.span().map_or(0, |span| span.start.min(text.len()));
        let mut boundary = offset;
        while !text.is_char_boundary(boundary) {
            boundary -= 1;
        }
        let prefix = &text[..boundary];
        let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
        let column = prefix
            .rsplit('\n')
            .next()
            .unwrap_or_default()
            .chars()
            .count()
            + 1;
        Error::Config(format!(
            "{}:{line}:{column}: invalid task TOML syntax or value",
            path.display()
        ))
    })?;
    Ok(raw.into())
}

#[derive(Serialize)]
struct VersionedTask<'a> {
    version: u32,
    task: &'a TaskDeclaration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutableTarget {
    /// Resolve this executable name using the process `PATH` when launching.
    SearchPath(String),
    /// Launch this path directly.
    Path(PathBuf),
}

fn validate_task(task: &TaskDeclaration, index: usize) -> Result<()> {
    if task.name.is_empty()
        || task.name.len() > MAX_TASK_NAME_LENGTH
        || !task.name.is_ascii()
        || !task.name.as_bytes()[0].is_ascii_lowercase()
            && !task.name.as_bytes()[0].is_ascii_digit()
        || !task
            .name
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
        || !task.name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_'
        })
    {
        return Err(task_error(
            index,
            "name",
            "must be 1-64 lowercase ASCII letters, digits, hyphens or underscores, starting and ending with a letter or digit",
        ));
    }
    if task.description.trim().is_empty()
        || task.description.chars().count() > MAX_DESCRIPTION_LENGTH
        || task.description.chars().any(char::is_control)
    {
        return Err(task_error(
            index,
            "description",
            "must be non-empty, at most 256 characters and contain no control characters",
        ));
    }
    validate_text_field(&task.executable, index, "executable", false)?;
    if task.args.len() > MAX_ARGUMENTS {
        return Err(task_error(
            index,
            "args",
            "may contain at most 128 arguments",
        ));
    }
    for argument in &task.args {
        validate_text_field(argument, index, "args", true)?;
    }
    if task.environments.is_empty() {
        return Err(task_error(
            index,
            "environments",
            "must list at least one Sotto environment",
        ));
    }
    if task.environments.len() > MAX_ENVIRONMENTS_PER_TASK {
        return Err(task_error(
            index,
            "environments",
            "may list at most 64 environments",
        ));
    }
    let mut environments = HashSet::with_capacity(task.environments.len());
    for environment in &task.environments {
        if environment.is_empty()
            || environment.len() > MAX_TEXT_LENGTH
            || environment.contains('\0')
        {
            return Err(task_error(
                index,
                "environments",
                "each environment name must be non-empty, at most 4096 bytes and contain no NUL character",
            ));
        }
        if !environments.insert(environment.as_str()) {
            return Err(task_error(
                index,
                "environments",
                "contains a duplicate environment name",
            ));
        }
    }
    if let Some(directory) = &task.working_directory {
        validate_text_field(directory, index, "working_directory", false)?;
    }
    if task.timeout_seconds == Some(0) {
        return Err(task_error(
            index,
            "timeout_seconds",
            "must be greater than zero when specified",
        ));
    }
    if task
        .timeout_seconds
        .is_some_and(|seconds| seconds > MAX_TASK_TIMEOUT_SECONDS)
    {
        return Err(task_error(
            index,
            "timeout_seconds",
            "must not exceed 3600 seconds",
        ));
    }
    Ok(())
}

fn validate_text_field(value: &str, index: usize, field: &str, allow_empty: bool) -> Result<()> {
    if (!allow_empty && value.trim().is_empty())
        || value.len() > MAX_TEXT_LENGTH
        || value.contains('\0')
    {
        let requirement = if allow_empty {
            "must be at most 4096 bytes and contain no NUL character"
        } else {
            "must be non-empty, at most 4096 bytes and contain no NUL character"
        };
        return Err(task_error(index, field, requirement));
    }
    Ok(())
}

fn task_error(index: usize, field: &str, message: &str) -> Error {
    Error::Config(format!("task {} field `{field}` {message}", index + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
version = 1

[[task]]
name = "test"
description = "Run the test suite"
executable = "cargo"
args = ["test"]
environments = ["dev"]
"#;

    #[test]
    fn parses_a_valid_task_manifest() {
        let manifest = TaskManifest::parse(Path::new("sotto.tasks.toml"), VALID).unwrap();

        assert_eq!(manifest.version, TASKS_VERSION);
        assert_eq!(manifest.tasks.len(), 1);
        assert_eq!(manifest.tasks[0].name, "test");
        assert_eq!(manifest.tasks[0].executable, "cargo");
        assert_eq!(manifest.tasks[0].args, ["test"]);
        assert_eq!(manifest.tasks[0].environments, ["dev"]);
    }

    #[test]
    fn rejects_unsupported_versions_and_unknown_fields() {
        let unsupported = VALID.replace("version = 1", "version = 2");
        assert!(TaskManifest::parse(Path::new("tasks.toml"), &unsupported).is_err());

        let unknown_field = VALID.replace("description =", "unexpected = true\ndescription =");
        assert!(TaskManifest::parse(Path::new("tasks.toml"), &unknown_field).is_err());
    }

    #[test]
    fn rejects_duplicate_tasks_and_duplicate_environments() {
        let duplicate_task = format!(
            "{VALID}\n\n[[task]]\nname = \"test\"\ndescription = \"Another test\"\nexecutable = \"cargo\"\nargs = []\nenvironments = [\"stage\"]\n"
        );
        assert!(TaskManifest::parse(Path::new("tasks.toml"), &duplicate_task).is_err());

        let duplicate_environment = VALID.replace("[\"dev\"]", "[\"dev\", \"dev\"]");
        assert!(TaskManifest::parse(Path::new("tasks.toml"), &duplicate_environment).is_err());
    }

    #[test]
    fn rejects_invalid_names_empty_environments_and_zero_timeout() {
        for invalid in [
            VALID.replace("name = \"test\"", "name = \"Bad Name\""),
            VALID.replace("environments = [\"dev\"]", "environments = []"),
            VALID.replace("environments = [\"dev\"]", "environments = [\"\"]"),
            VALID.replace(
                "environments = [\"dev\"]",
                "environments = [\"dev\"]\ntimeout_seconds = 0",
            ),
        ] {
            assert!(TaskManifest::parse(Path::new("tasks.toml"), &invalid).is_err());
        }
    }

    #[test]
    fn preserves_existing_environment_names_without_imposing_a_new_grammar() {
        for environment in ["prod/eu", "Production US", "..", "Upper", "-prod", "prod_"] {
            let declaration = VALID.replace("[\"dev\"]", &format!("[\"{environment}\"]"));
            let manifest = TaskManifest::parse(Path::new("tasks.toml"), &declaration).unwrap();
            assert_eq!(manifest.tasks()[0].environments(), [environment]);
        }
    }

    #[test]
    fn parse_errors_report_location_without_echoing_source_text() {
        for sensitive in [
            "version = 1\n\n[[task]]\nname = \"test\"\ndescription = \"do_not_echo_this_secret\n",
            "version = \"do_not_echo_this_secret\"\n",
        ] {
            let error = TaskManifest::parse(Path::new("tasks.toml"), sensitive).unwrap_err();
            let message = error.to_string();

            assert!(message.contains("tasks.toml:"), "{message}");
            assert!(!message.contains("do_not_echo_this_secret"), "{message}");
        }
    }

    #[test]
    fn task_fingerprint_ignores_toml_formatting_but_tracks_execution_changes() {
        let original = TaskManifest::parse(Path::new("tasks.toml"), VALID).unwrap();
        let reformatted = TaskManifest::parse(
            Path::new("other.toml"),
            &VALID.replace("args = [\"test\"]", "args=[\"test\"] # formatting only"),
        )
        .unwrap();
        assert_eq!(
            original.fingerprint_for("test").unwrap(),
            reformatted.fingerprint_for("test").unwrap()
        );

        let changed = TaskManifest::parse(
            Path::new("tasks.toml"),
            &VALID.replace("args = [\"test\"]", "args = [\"test\", \"--all\"]"),
        )
        .unwrap();
        assert_ne!(
            original.fingerprint_for("test").unwrap(),
            changed.fingerprint_for("test").unwrap()
        );
    }

    #[test]
    fn resolves_bare_executables_and_project_relative_paths_consistently() {
        let manifest = TaskManifest::parse(
            Path::new("tasks.toml"),
            &VALID.replace(
                "args = [\"test\"]",
                "args = [\"test\"]\nworking_directory = \"packages/api\"",
            ),
        )
        .unwrap();
        let root = Path::new("/work/project");
        let task = &manifest.tasks[0];

        assert_eq!(
            task.executable_target(root),
            ExecutableTarget::SearchPath("cargo".into())
        );
        assert_eq!(task.working_directory(root), root.join("packages/api"));

        let relative_executable = TaskManifest::parse(
            Path::new("tasks.toml"),
            &VALID.replace("executable = \"cargo\"", "executable = \"scripts/test.sh\""),
        )
        .unwrap();
        assert_eq!(
            relative_executable.tasks[0].executable_target(root),
            ExecutableTarget::Path(root.join("scripts/test.sh"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolves_a_bare_executable_to_the_concrete_path_on_path() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let project_root = temp.path().join("project");
        let binaries = temp.path().join("bin");
        std::fs::create_dir_all(&project_root).unwrap();
        std::fs::create_dir_all(&binaries).unwrap();
        let executable = binaries.join("fixture-tool");
        std::fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let manifest = TaskManifest::parse(
            Path::new("sotto.tasks.toml"),
            &VALID.replace("cargo", "fixture-tool"),
        )
        .unwrap();
        let task = &manifest.tasks()[0];

        let resolved = task
            .resolve_executable_with_path(&project_root, "fixture-tool", binaries.as_os_str())
            .unwrap();

        assert_eq!(resolved, std::fs::canonicalize(executable).unwrap());
    }

    #[test]
    fn discovers_task_file_at_the_nearest_project_root() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let nested = project.join("src/nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(
            project.join(crate::config::CONFIG_FILE),
            "project_id = \"11111111-1111-1111-1111-111111111111\"\nproject = \"example\"\nenvironment = \"dev\"\n",
        )
        .unwrap();
        std::fs::write(project.join(TASKS_FILE), VALID).unwrap();

        let (root, manifest) = TaskManifest::discover(&nested).unwrap();
        assert_eq!(root, project);
        assert_eq!(manifest.tasks[0].name, "test");
    }

    #[test]
    fn rejects_task_timeouts_above_the_execution_limit() {
        let too_long = VALID.replace(
            "environments = [\"dev\"]",
            "environments = [\"dev\"]\ntimeout_seconds = 3601",
        );
        assert!(TaskManifest::parse(Path::new("tasks.toml"), &too_long).is_err());
    }
}
