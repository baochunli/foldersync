//! Validated typed bisync request and argv builder.
//!
//! By construction, forbidden flags (`--force`, `--inplace`, `--copy-links`,
//! automatic resync, or free-form extra arguments) cannot be represented.

use std::{
    ffi::OsString,
    fs,
    path::{Component, Path, PathBuf},
};

use thiserror::Error;

use crate::{
    process::{
        ProcessError, ProcessRunner, ProcessSpec, RunHandle, SpawnOptions, minimal_isolated_env,
    },
    qualification::{
        EngineIdentity, QualificationError, QualificationManifest,
        verify_binary_integrity_with_manifest,
    },
};

const DEFAULT_CHECK_FILENAME: &str = "RCLONE_TEST";
const DEFAULT_MAX_DELETE_PERCENT: u8 = 10;

/// Second endpoint of a bisync run (remote collection or local test directory).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Path2 {
    /// A Google Drive remote collection (e.g. `household:Data/Research`).
    /// Emits `--drive-skip-gdocs` and `--drive-skip-shortcuts`.
    DriveRemote { remote: String, path: String },
    /// A non-Drive rclone remote path (omits Drive-specific skip flags).
    OtherRemote { remote: String, path: String },
    /// An absolute local directory, used for local-to-local qualification tests.
    LocalDir(PathBuf),
}

impl Path2 {
    fn to_os_string(&self) -> OsString {
        match self {
            Path2::DriveRemote { remote, path } | Path2::OtherRemote { remote, path } => {
                OsString::from(format!("{remote}:{path}"))
            }
            Path2::LocalDir(dir) => dir.as_os_str().to_os_string(),
        }
    }

    fn is_drive_remote(&self) -> bool {
        matches!(self, Path2::DriveRemote { .. })
    }
}

/// Backup destination for Path2 (`--backup-dir2`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackupDir2 {
    /// A remote path on the same remote as `Path2`.
    Remote { remote: String, path: String },
    /// An absolute local directory (for local-to-local tests).
    LocalDir(PathBuf),
}

impl BackupDir2 {
    fn to_os_string(&self) -> OsString {
        match self {
            BackupDir2::Remote { remote, path } => OsString::from(format!("{remote}:{path}")),
            BackupDir2::LocalDir(dir) => dir.as_os_str().to_os_string(),
        }
    }
}

/// Winner preference during an explicit initialization run (`--resync-mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitPreference {
    /// Prefer Path1 (local has data; cloud destination is newly empty).
    Path1,
    /// Prefer Path2 (cloud has data; local destination is empty).
    Path2,
}

/// Execution mode of a bisync request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
    /// Normal bidirectional sync (`--resilient --recover`). Never carries resync options.
    Normal,
    /// Explicit initialization (`--resync-mode path1|path2`). Never carries `--resilient`/`--recover`.
    Initialize(InitPreference),
}

/// Validated credential-helper argv passed to rclone `--password-command`.
///
/// ## Non-secret helper contract
/// - `program` must be an absolute path to a dedicated credential helper binary
///   (shell interpreters such as `sh`, `bash`, `zsh`, `cmd`, or `powershell` are
///   rejected so inline secrets cannot be passed as shell scripts).
/// - `args` must contain only non-secret lookup selectors (for example
///   `["--account", "a1"]`), never passwords, passphrases, or OAuth tokens.
/// - The helper prints the configuration password solely to stdout on success
///   and must not print secret material to stderr; `rclone-adapter` additionally
///   redacts `--password-command stderr:` payloads at the capture boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasswordCommand {
    program: PathBuf,
    args: Vec<String>,
}

/// Validation errors returned when constructing a [`BisyncRequest`] or [`PasswordCommand`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum RequestError {
    #[error("missing required field: {0}")]
    MissingField(&'static str),
    #[error("unqualified rclone engine version `{version}` (expected `{expected}`)")]
    UnqualifiedEngineVersion { version: String, expected: String },
    #[error("unqualified rclone engine platform `{actual}` (expected host `{expected}`)")]
    UnqualifiedEnginePlatform { actual: String, expected: String },
    #[error("unqualified rclone engine SHA-256 `{sha256}` does not match host platform pin")]
    UnqualifiedEngineHash { sha256: String },
    #[error("{field} must be an absolute path, got: {path}")]
    RelativePath { field: &'static str, path: PathBuf },
    #[error("{field} must not be empty")]
    EmptyPath { field: &'static str },
    #[error("{field} contains invalid characters: {reason}")]
    InvalidCharacters {
        field: &'static str,
        reason: &'static str,
    },
    #[error(
        "password_command.program must be a dedicated helper binary, not a shell interpreter: {0}"
    )]
    ShellPasswordCommand(PathBuf),
    #[error("could not resolve ancestor for {field} ({path}): {reason}")]
    UnresolvablePath {
        field: &'static str,
        path: PathBuf,
        reason: String,
    },
    #[error(
        "paths must not be identical or nested: {left_field} ({left}) overlaps {right_field} ({right})"
    )]
    OverlappingPaths {
        left_field: &'static str,
        left: String,
        right_field: &'static str,
        right: String,
    },
    #[error("invalid remote name `{remote}`: {reason}")]
    InvalidRemoteName {
        remote: String,
        reason: &'static str,
    },
    #[error("invalid remote path `{path}`: {reason}")]
    InvalidRemotePath { path: String, reason: &'static str },
    #[error("backup_dir2 must match path2 kind and remote ({reason})")]
    BackupDir2Mismatch { reason: &'static str },
    #[error("max_delete_percent must be within 1..=100, got {0}")]
    InvalidMaxDeletePercent(u8),
    #[error(
        "check_filename must be a single non-empty filename without path separators, got `{0}`"
    )]
    InvalidCheckFilename(String),
}

/// Errors raised when verifying and launching a [`BisyncRequest`].
#[derive(Debug, Error)]
pub enum LaunchError {
    #[error("engine integrity check failed before launch: {0}")]
    Qualification(#[from] QualificationError),
    #[error("failed to spawn bisync process: {0}")]
    Process(#[from] ProcessError),
}

fn contains_control_breaks(s: &str) -> bool {
    s.chars().any(|c| matches!(c, '\0' | '\r' | '\n'))
}

fn is_shell_interpreter(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    matches!(
        name.to_ascii_lowercase().as_str(),
        "sh" | "bash"
            | "zsh"
            | "dash"
            | "fish"
            | "ksh"
            | "csh"
            | "tcsh"
            | "cmd"
            | "cmd.exe"
            | "powershell"
            | "pwsh"
    )
}

fn format_space_sep_token(token: &str) -> String {
    if token.is_empty() || token.chars().any(|c| matches!(c, ' ' | '\t' | '"')) {
        let escaped = token.replace('"', "\"\"");
        format!("\"{escaped}\"")
    } else {
        token.to_string()
    }
}

impl PasswordCommand {
    /// Creates a validated credential-helper argv for `--password-command`.
    pub fn new(program: impl Into<PathBuf>, args: Vec<String>) -> Result<Self, RequestError> {
        let program = program.into();
        if program.as_os_str().is_empty() {
            return Err(RequestError::EmptyPath {
                field: "password_command.program",
            });
        }
        if !program.is_absolute() {
            return Err(RequestError::RelativePath {
                field: "password_command.program",
                path: program,
            });
        }
        if is_shell_interpreter(&program) {
            return Err(RequestError::ShellPasswordCommand(program));
        }
        let Some(prog_str) = program.to_str() else {
            return Err(RequestError::InvalidCharacters {
                field: "password_command.program",
                reason: "must be valid UTF-8 for rclone SpaceSepList encoding",
            });
        };
        if contains_control_breaks(prog_str) {
            return Err(RequestError::InvalidCharacters {
                field: "password_command.program",
                reason: "must not contain NUL, CR, or LF",
            });
        }
        for arg in &args {
            if contains_control_breaks(arg) {
                return Err(RequestError::InvalidCharacters {
                    field: "password_command.args",
                    reason: "must not contain NUL, CR, or LF",
                });
            }
        }
        Ok(Self { program, args })
    }

    pub fn program(&self) -> &Path {
        &self.program
    }

    pub fn args(&self) -> &[String] {
        &self.args
    }

    /// Formats the argv as an rclone `SpaceSepList` (RFC4180 CSV double-quote escaping).
    pub fn to_rclone_space_sep_list(&self) -> String {
        let prog = self.program.to_str().expect("validated UTF-8 program path");
        let mut tokens = Vec::with_capacity(1 + self.args.len());
        tokens.push(format_space_sep_token(prog));
        for arg in &self.args {
            tokens.push(format_space_sep_token(arg));
        }
        tokens.join(" ")
    }
}

/// Validated, immutable rclone bisync execution request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BisyncRequest {
    engine: EngineIdentity,
    mode: RunMode,
    path1: PathBuf,
    path2: Path2,
    config_path: PathBuf,
    password_command: Option<PasswordCommand>,
    workdir: PathBuf,
    filters_file: PathBuf,
    check_filename: String,
    max_delete_percent: u8,
    backup_dir1: PathBuf,
    backup_dir2: BackupDir2,
}

/// Validating builder for [`BisyncRequest`].
#[derive(Debug, Default, Clone)]
pub struct BisyncRequestBuilder {
    engine: Option<EngineIdentity>,
    mode: Option<RunMode>,
    path1: Option<PathBuf>,
    path2: Option<Path2>,
    config_path: Option<PathBuf>,
    password_command: Option<PasswordCommand>,
    workdir: Option<PathBuf>,
    filters_file: Option<PathBuf>,
    check_filename: Option<String>,
    max_delete_percent: Option<u8>,
    backup_dir1: Option<PathBuf>,
    backup_dir2: Option<BackupDir2>,
}

impl BisyncRequest {
    pub fn builder() -> BisyncRequestBuilder {
        BisyncRequestBuilder::default()
    }

    pub fn engine(&self) -> &EngineIdentity {
        &self.engine
    }

    pub fn mode(&self) -> RunMode {
        self.mode
    }

    pub fn path1(&self) -> &Path {
        &self.path1
    }

    pub fn path2(&self) -> &Path2 {
        &self.path2
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    pub fn password_command(&self) -> Option<&PasswordCommand> {
        self.password_command.as_ref()
    }

    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    pub fn filters_file(&self) -> &Path {
        &self.filters_file
    }

    pub fn check_filename(&self) -> &str {
        &self.check_filename
    }

    /// Percentage of tracked files allowed to be deleted before rclone aborts (`--max-delete N`).
    pub fn max_delete_percent(&self) -> u8 {
        self.max_delete_percent
    }

    pub fn backup_dir1(&self) -> &Path {
        &self.backup_dir1
    }

    pub fn backup_dir2(&self) -> &BackupDir2 {
        &self.backup_dir2
    }

    /// Converts this validated request into an argument vector (`Vec<OsString>`)
    /// starting with `"bisync"`. Never constructs a shell command string.
    pub fn to_args(&self) -> Vec<OsString> {
        let mut args = Vec::with_capacity(38);
        args.push(OsString::from("bisync"));
        args.push(self.path1.as_os_str().to_os_string());
        args.push(self.path2.to_os_string());

        args.push(OsString::from("--config"));
        args.push(self.config_path.as_os_str().to_os_string());

        if let Some(pw_cmd) = &self.password_command {
            args.push(OsString::from("--password-command"));
            args.push(OsString::from(pw_cmd.to_rclone_space_sep_list()));
        }

        args.push(OsString::from("--ask-password=false"));

        args.push(OsString::from("--workdir"));
        args.push(self.workdir.as_os_str().to_os_string());

        args.push(OsString::from("--filters-file"));
        args.push(self.filters_file.as_os_str().to_os_string());

        args.push(OsString::from("--check-access"));
        args.push(OsString::from("--check-filename"));
        args.push(OsString::from(&self.check_filename));

        args.push(OsString::from("--max-delete"));
        args.push(OsString::from(self.max_delete_percent.to_string()));

        args.push(OsString::from("--compare"));
        args.push(OsString::from("size,modtime"));

        args.push(OsString::from("--conflict-resolve"));
        args.push(OsString::from("none"));

        args.push(OsString::from("--conflict-loser"));
        args.push(OsString::from("num"));

        args.push(OsString::from("--check-sync"));
        args.push(OsString::from("true"));

        match self.mode {
            RunMode::Normal => {
                args.push(OsString::from("--resilient"));
                args.push(OsString::from("--recover"));
            }
            RunMode::Initialize(InitPreference::Path1) => {
                args.push(OsString::from("--resync-mode"));
                args.push(OsString::from("path1"));
            }
            RunMode::Initialize(InitPreference::Path2) => {
                args.push(OsString::from("--resync-mode"));
                args.push(OsString::from("path2"));
            }
        }

        args.push(OsString::from("--backup-dir1"));
        args.push(self.backup_dir1.as_os_str().to_os_string());

        args.push(OsString::from("--backup-dir2"));
        args.push(self.backup_dir2.to_os_string());

        args.push(OsString::from("--use-json-log"));
        args.push(OsString::from("--log-level"));
        args.push(OsString::from("INFO"));
        args.push(OsString::from("--stats"));
        args.push(OsString::from("2s"));

        if self.path2.is_drive_remote() {
            args.push(OsString::from("--drive-skip-gdocs"));
            args.push(OsString::from("--drive-skip-shortcuts"));
        }

        args
    }

    /// Re-verifies the qualified binary's SHA-256 on disk against `manifest`
    /// and builds an isolated [`ProcessSpec`] pinned to that digest.
    fn build_verified_spec(
        &self,
        manifest: &QualificationManifest,
    ) -> Result<ProcessSpec, QualificationError> {
        verify_binary_integrity_with_manifest(&self.engine, manifest)?;
        Ok(ProcessSpec {
            program: self.engine.path().to_path_buf(),
            args: self.to_args(),
            current_dir: None,
            env: minimal_isolated_env(Some(&self.workdir)),
            expected_binary_sha256: Some(self.engine.sha256().to_string()),
        })
    }

    #[cfg(test)]
    pub(crate) fn to_verified_process_spec_with_manifest(
        &self,
        manifest: &QualificationManifest,
    ) -> Result<ProcessSpec, QualificationError> {
        self.build_verified_spec(manifest)
    }

    /// Re-hashes the binary on disk at the launch boundary against the checked-in
    /// qualification manifest and spawns the process with a cleared, allowlisted
    /// environment (where [`crate::RealProcessRunner`] also re-hashes immediately
    /// before `exec`).
    pub fn spawn(
        &self,
        runner: &dyn ProcessRunner,
        options: SpawnOptions,
    ) -> Result<RunHandle, LaunchError> {
        let manifest = QualificationManifest::embedded()?;
        let spec = self.build_verified_spec(&manifest)?;
        Ok(runner.spawn(&spec, options)?)
    }

    /// Test-only launch boundary variant accepting a synthetic manifest.
    #[cfg(test)]
    pub(crate) fn spawn_with_manifest(
        &self,
        runner: &dyn ProcessRunner,
        options: SpawnOptions,
        manifest: &QualificationManifest,
    ) -> Result<RunHandle, LaunchError> {
        let spec = self.build_verified_spec(manifest)?;
        Ok(runner.spawn(&spec, options)?)
    }
}

fn validate_local_path(field: &'static str, path: &Path) -> Result<(), RequestError> {
    if path.as_os_str().is_empty() {
        return Err(RequestError::EmptyPath { field });
    }
    if path.to_string_lossy().contains('\0') {
        return Err(RequestError::InvalidCharacters {
            field,
            reason: "must not contain NUL bytes",
        });
    }
    if !path.is_absolute() {
        return Err(RequestError::RelativePath {
            field,
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn validate_remote_name(remote: &str) -> Result<(), RequestError> {
    if remote.is_empty() {
        return Err(RequestError::InvalidRemoteName {
            remote: remote.to_string(),
            reason: "must not be empty",
        });
    }
    if remote.starts_with('-') {
        return Err(RequestError::InvalidRemoteName {
            remote: remote.to_string(),
            reason: "must not start with '-'",
        });
    }
    if !remote
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '+' | '@'))
    {
        return Err(RequestError::InvalidRemoteName {
            remote: remote.to_string(),
            reason: "contains unsupported characters",
        });
    }
    Ok(())
}

fn validate_remote_path(path: &str) -> Result<(), RequestError> {
    if path.is_empty() {
        return Err(RequestError::InvalidRemotePath {
            path: path.to_string(),
            reason: "must not be empty",
        });
    }
    if contains_control_breaks(path) {
        return Err(RequestError::InvalidRemotePath {
            path: path.to_string(),
            reason: "must not contain NUL, CR, or LF",
        });
    }
    if path.starts_with('/') || path.ends_with('/') || path.contains('\\') {
        return Err(RequestError::InvalidRemotePath {
            path: path.to_string(),
            reason: "must be a normalized relative remote path without leading/trailing slashes",
        });
    }
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(RequestError::InvalidRemotePath {
                path: path.to_string(),
                reason: "contains empty or dot traversal segment",
            });
        }
    }
    Ok(())
}

fn validate_check_filename(name: &str) -> Result<(), RequestError> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.starts_with('-')
        || name.contains('/')
        || name.contains('\\')
        || contains_control_breaks(name)
    {
        return Err(RequestError::InvalidCheckFilename(name.to_string()));
    }
    Ok(())
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    normalized
}

/// Resolves the longest existing ancestor via `canonicalize` and appends the
/// remaining non-existing tail components. Rejects broken symlinks or paths
/// whose ancestors cannot be resolved safely.
fn resolve_with_existing_ancestor(
    field: &'static str,
    path: &Path,
) -> Result<PathBuf, RequestError> {
    let normalized = lexical_normalize(path);
    let mut current = normalized.as_path();
    let mut tail: Vec<OsString> = Vec::new();

    loop {
        match fs::symlink_metadata(current) {
            Ok(_) => {
                let mut resolved =
                    fs::canonicalize(current).map_err(|err| RequestError::UnresolvablePath {
                        field,
                        path: path.to_path_buf(),
                        reason: format!(
                            "existing ancestor `{}` cannot be canonicalized: {err}",
                            current.display()
                        ),
                    })?;
                for part in tail.into_iter().rev() {
                    resolved.push(part);
                }
                return Ok(resolved);
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let Some(file_name) = current.file_name() else {
                    return Err(RequestError::UnresolvablePath {
                        field,
                        path: path.to_path_buf(),
                        reason: "no existing ancestor found".to_string(),
                    });
                };
                tail.push(file_name.to_os_string());
                let Some(parent) = current.parent() else {
                    return Err(RequestError::UnresolvablePath {
                        field,
                        path: path.to_path_buf(),
                        reason: "no existing ancestor found".to_string(),
                    });
                };
                current = parent;
            }
            Err(err) => {
                return Err(RequestError::UnresolvablePath {
                    field,
                    path: path.to_path_buf(),
                    reason: format!("cannot inspect `{}`: {err}", current.display()),
                });
            }
        }
    }
}

fn remote_paths_overlap(a: &str, b: &str) -> bool {
    let sa: Vec<&str> = a.split('/').collect();
    let sb: Vec<&str> = b.split('/').collect();
    sa.starts_with(&sb) || sb.starts_with(&sa)
}

fn reject_local_overlap(
    left_field: &'static str,
    left: &Path,
    right_field: &'static str,
    right: &Path,
) -> Result<(), RequestError> {
    let left_lex = lexical_normalize(left);
    let right_lex = lexical_normalize(right);
    if left_lex.starts_with(&right_lex) || right_lex.starts_with(&left_lex) {
        return Err(RequestError::OverlappingPaths {
            left_field,
            left: left.display().to_string(),
            right_field,
            right: right.display().to_string(),
        });
    }

    let left_resolved = resolve_with_existing_ancestor(left_field, left)?;
    let right_resolved = resolve_with_existing_ancestor(right_field, right)?;
    if left_resolved.starts_with(&right_resolved) || right_resolved.starts_with(&left_resolved) {
        return Err(RequestError::OverlappingPaths {
            left_field,
            left: left.display().to_string(),
            right_field,
            right: right.display().to_string(),
        });
    }
    Ok(())
}

impl BisyncRequestBuilder {
    pub fn engine(mut self, engine: EngineIdentity) -> Self {
        self.engine = Some(engine);
        self
    }

    pub fn mode(mut self, mode: RunMode) -> Self {
        self.mode = Some(mode);
        self
    }

    pub fn path1(mut self, path1: impl Into<PathBuf>) -> Self {
        self.path1 = Some(path1.into());
        self
    }

    pub fn path2(mut self, path2: Path2) -> Self {
        self.path2 = Some(path2);
        self
    }

    pub fn config_path(mut self, config_path: impl Into<PathBuf>) -> Self {
        self.config_path = Some(config_path.into());
        self
    }

    pub fn password_command(mut self, cmd: PasswordCommand) -> Self {
        self.password_command = Some(cmd);
        self
    }

    pub fn workdir(mut self, workdir: impl Into<PathBuf>) -> Self {
        self.workdir = Some(workdir.into());
        self
    }

    pub fn filters_file(mut self, filters_file: impl Into<PathBuf>) -> Self {
        self.filters_file = Some(filters_file.into());
        self
    }

    pub fn check_filename(mut self, check_filename: impl Into<String>) -> Self {
        self.check_filename = Some(check_filename.into());
        self
    }

    /// Maximum percentage of tracked files that may be deleted in a run (1..=100).
    /// Emitted to rclone bisync as `--max-delete N`.
    pub fn max_delete_percent(mut self, percent: u8) -> Self {
        self.max_delete_percent = Some(percent);
        self
    }

    pub fn backup_dir1(mut self, backup_dir1: impl Into<PathBuf>) -> Self {
        self.backup_dir1 = Some(backup_dir1.into());
        self
    }

    pub fn backup_dir2(mut self, backup_dir2: BackupDir2) -> Self {
        self.backup_dir2 = Some(backup_dir2);
        self
    }

    /// Validates all fields against the embedded qualification manifest and
    /// constructs a [`BisyncRequest`].
    pub fn build(self) -> Result<BisyncRequest, RequestError> {
        let manifest =
            QualificationManifest::embedded().expect("checked-in qualification manifest is valid");
        self.build_with_manifest_internal(&manifest)
    }

    /// Test-only builder accepting a synthetic qualification manifest.
    #[cfg(test)]
    pub(crate) fn build_with_manifest(
        self,
        manifest: &QualificationManifest,
    ) -> Result<BisyncRequest, RequestError> {
        self.build_with_manifest_internal(manifest)
    }

    fn build_with_manifest_internal(
        self,
        manifest: &QualificationManifest,
    ) -> Result<BisyncRequest, RequestError> {
        let engine = self.engine.ok_or(RequestError::MissingField("engine"))?;
        let mode = self.mode.ok_or(RequestError::MissingField("mode"))?;
        let path1 = self.path1.ok_or(RequestError::MissingField("path1"))?;
        let path2 = self.path2.ok_or(RequestError::MissingField("path2"))?;
        let config_path = self
            .config_path
            .ok_or(RequestError::MissingField("config_path"))?;
        let workdir = self.workdir.ok_or(RequestError::MissingField("workdir"))?;
        let filters_file = self
            .filters_file
            .ok_or(RequestError::MissingField("filters_file"))?;
        let backup_dir1 = self
            .backup_dir1
            .ok_or(RequestError::MissingField("backup_dir1"))?;
        let backup_dir2 = self
            .backup_dir2
            .ok_or(RequestError::MissingField("backup_dir2"))?;
        let check_filename = self
            .check_filename
            .unwrap_or_else(|| DEFAULT_CHECK_FILENAME.to_string());
        let max_delete_percent = self
            .max_delete_percent
            .unwrap_or(DEFAULT_MAX_DELETE_PERCENT);

        // Enforce host-platform pinned engine qualification.
        let (host_platform, host_pin) = manifest
            .host_platform_pin()
            .expect("host platform is defined in qualification manifest");
        if engine.version() != manifest.rclone_version() {
            return Err(RequestError::UnqualifiedEngineVersion {
                version: engine.version().to_string(),
                expected: manifest.rclone_version().to_string(),
            });
        }
        if engine.platform() != host_platform {
            return Err(RequestError::UnqualifiedEnginePlatform {
                actual: engine.platform().to_string(),
                expected: host_platform.to_string(),
            });
        }
        if engine.sha256() != host_pin.binary_sha256 {
            return Err(RequestError::UnqualifiedEngineHash {
                sha256: engine.sha256().to_string(),
            });
        }
        validate_local_path("engine.path", engine.path())?;

        // Validate mandatory local paths.
        validate_local_path("path1", &path1)?;
        validate_local_path("config_path", &config_path)?;
        validate_local_path("workdir", &workdir)?;
        validate_local_path("filters_file", &filters_file)?;
        validate_local_path("backup_dir1", &backup_dir1)?;
        validate_check_filename(&check_filename)?;

        if !(1..=100).contains(&max_delete_percent) {
            return Err(RequestError::InvalidMaxDeletePercent(max_delete_percent));
        }

        // Reject local overlap between path1, backup_dir1, and workdir.
        reject_local_overlap("path1", &path1, "backup_dir1", &backup_dir1)?;
        reject_local_overlap("path1", &path1, "workdir", &workdir)?;
        reject_local_overlap("backup_dir1", &backup_dir1, "workdir", &workdir)?;

        // Validate path2 and backup_dir2 compatibility and non-overlap.
        match (&path2, &backup_dir2) {
            (
                Path2::DriveRemote { remote, path } | Path2::OtherRemote { remote, path },
                BackupDir2::Remote {
                    remote: b_remote,
                    path: b_path,
                },
            ) => {
                validate_remote_name(remote)?;
                validate_remote_path(path)?;
                validate_remote_name(b_remote)?;
                validate_remote_path(b_path)?;
                if remote != b_remote {
                    return Err(RequestError::BackupDir2Mismatch {
                        reason: "remote backup_dir2 must use the same remote as path2",
                    });
                }
                if remote_paths_overlap(path, b_path) {
                    return Err(RequestError::OverlappingPaths {
                        left_field: "path2",
                        left: format!("{remote}:{path}"),
                        right_field: "backup_dir2",
                        right: format!("{b_remote}:{b_path}"),
                    });
                }
            }
            (Path2::LocalDir(p2), BackupDir2::LocalDir(b2)) => {
                validate_local_path("path2", p2)?;
                validate_local_path("backup_dir2", b2)?;
                reject_local_overlap("path1", &path1, "path2", p2)?;
                reject_local_overlap("path2", p2, "backup_dir1", &backup_dir1)?;
                reject_local_overlap("path1", &path1, "backup_dir2", b2)?;
                reject_local_overlap("path2", p2, "backup_dir2", b2)?;
                reject_local_overlap("path2", p2, "workdir", &workdir)?;
                reject_local_overlap("backup_dir1", &backup_dir1, "backup_dir2", b2)?;
                reject_local_overlap("backup_dir2", b2, "workdir", &workdir)?;
            }
            (Path2::LocalDir(_), BackupDir2::Remote { .. }) => {
                return Err(RequestError::BackupDir2Mismatch {
                    reason: "local path2 requires a local backup_dir2",
                });
            }
            (Path2::DriveRemote { .. } | Path2::OtherRemote { .. }, BackupDir2::LocalDir(_)) => {
                return Err(RequestError::BackupDir2Mismatch {
                    reason: "remote path2 requires a remote backup_dir2 on the same remote",
                });
            }
        }

        Ok(BisyncRequest {
            engine,
            mode,
            path1,
            path2,
            config_path,
            password_command: self.password_command,
            workdir,
            filters_file,
            check_filename,
            max_delete_percent,
            backup_dir1,
            backup_dir2,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        process::{FakeProcessRunner, FakeRunScript, OutputLine, StreamKind},
        qualification::{host_platform_key, qualify_binary_with_runner, sha256_file},
    };
    use std::sync::atomic::{AtomicU64, Ordering};

    static REQ_TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestFixtureDir {
        root: PathBuf,
    }

    impl TestFixtureDir {
        fn new(label: &str) -> Self {
            let seq = REQ_TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "foldersync-req-test-{label}-{}-{seq}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn path(&self, rel: &str) -> PathBuf {
            self.root.join(rel)
        }
    }

    impl Drop for TestFixtureDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn test_engine(root: &TestFixtureDir) -> EngineIdentity {
        let manifest = QualificationManifest::embedded().unwrap();
        let (platform, pin) = manifest.host_platform_pin().unwrap();
        EngineIdentity::for_unit_test(
            crate::PINNED_RCLONE_VERSION,
            pin.binary_sha256.clone(),
            root.path("bin/rclone"),
            platform,
        )
    }

    fn base_drive_builder(root: &TestFixtureDir) -> BisyncRequestBuilder {
        BisyncRequest::builder()
            .engine(test_engine(root))
            .mode(RunMode::Normal)
            .path1(root.path("data/Research"))
            .path2(Path2::DriveRemote {
                remote: "household".to_string(),
                path: "Data/Research".to_string(),
            })
            .config_path(root.path("state/accounts/a1/rclone.conf"))
            .password_command(
                PasswordCommand::new(
                    root.path("bin/cred-helper"),
                    vec!["--account".to_string(), "a1".to_string()],
                )
                .unwrap(),
            )
            .workdir(root.path("state/mappings/m1/generation-1/bisync"))
            .filters_file(root.path("state/mappings/m1/generation-1/filters.txt"))
            .check_filename("RCLONE_TEST")
            .max_delete_percent(10)
            .backup_dir1(root.path("recovery/m1/run-1"))
            .backup_dir2(BackupDir2::Remote {
                remote: "household".to_string(),
                path: "Recovery/research/dev-1/run-1".to_string(),
            })
    }

    fn args_as_strings(req: &BisyncRequest) -> Vec<String> {
        req.to_args()
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn builds_exact_normal_drive_argv_without_forbidden_or_resync_flags() {
        let root = TestFixtureDir::new("normal-drive");
        let req = base_drive_builder(&root).build().unwrap();
        let args = args_as_strings(&req);

        let p1 = root.path("data/Research").display().to_string();
        let cfg = root
            .path("state/accounts/a1/rclone.conf")
            .display()
            .to_string();
        let helper = format!("{} --account a1", root.path("bin/cred-helper").display());
        let wd = root
            .path("state/mappings/m1/generation-1/bisync")
            .display()
            .to_string();
        let filters = root
            .path("state/mappings/m1/generation-1/filters.txt")
            .display()
            .to_string();
        let b1 = root.path("recovery/m1/run-1").display().to_string();

        assert_eq!(args[0], "bisync");
        assert_eq!(args[1], p1);
        assert_eq!(args[2], "household:Data/Research");
        assert!(args.windows(2).any(|w| w == ["--config", cfg.as_str()]));
        assert!(
            args.windows(2)
                .any(|w| w == ["--password-command", helper.as_str()])
        );
        assert!(args.contains(&"--ask-password=false".to_string()));
        assert!(args.windows(2).any(|w| w == ["--workdir", wd.as_str()]));
        assert!(
            args.windows(2)
                .any(|w| w == ["--filters-file", filters.as_str()])
        );
        assert!(args.contains(&"--check-access".to_string()));
        assert!(
            args.windows(2)
                .any(|w| w == ["--check-filename", "RCLONE_TEST"])
        );
        assert!(args.windows(2).any(|w| w == ["--max-delete", "10"]));
        assert!(args.windows(2).any(|w| w == ["--compare", "size,modtime"]));
        assert!(args.windows(2).any(|w| w == ["--conflict-resolve", "none"]));
        assert!(args.windows(2).any(|w| w == ["--conflict-loser", "num"]));
        assert!(args.windows(2).any(|w| w == ["--check-sync", "true"]));
        assert!(args.contains(&"--resilient".to_string()));
        assert!(args.contains(&"--recover".to_string()));
        assert!(args.windows(2).any(|w| w == ["--backup-dir1", b1.as_str()]));
        assert!(
            args.windows(2)
                .any(|w| { w == ["--backup-dir2", "household:Recovery/research/dev-1/run-1",] })
        );
        assert!(args.contains(&"--use-json-log".to_string()));
        assert!(args.windows(2).any(|w| w == ["--log-level", "INFO"]));
        assert!(args.windows(2).any(|w| w == ["--stats", "2s"]));
        assert!(args.contains(&"--drive-skip-gdocs".to_string()));
        assert!(args.contains(&"--drive-skip-shortcuts".to_string()));

        // Forbidden and initialization-only options must never appear in normal mode.
        for forbidden in [
            "--resync",
            "--resync-mode",
            "--force",
            "--inplace",
            "--copy-links",
            "--download-hash",
            "--track-renames",
        ] {
            assert!(
                !args.iter().any(|a| a == forbidden),
                "unexpected flag {forbidden}"
            );
        }
    }

    #[test]
    fn initialization_mode_carries_resync_mode_and_omits_resilient_recover() {
        let root = TestFixtureDir::new("init-mode");
        for (pref, expected_mode) in [
            (InitPreference::Path1, "path1"),
            (InitPreference::Path2, "path2"),
        ] {
            let req = base_drive_builder(&root)
                .mode(RunMode::Initialize(pref))
                .build()
                .unwrap();
            let args = args_as_strings(&req);
            assert!(
                args.windows(2)
                    .any(|w| w == ["--resync-mode", expected_mode])
            );
            assert!(!args.contains(&"--resync".to_string()));
            assert!(!args.contains(&"--resilient".to_string()));
            assert!(!args.contains(&"--recover".to_string()));
        }
    }

    #[test]
    fn omits_drive_skip_options_for_local_and_non_drive_remotes() {
        let root = TestFixtureDir::new("non-drive");
        let local_req = BisyncRequest::builder()
            .engine(test_engine(&root))
            .mode(RunMode::Normal)
            .path1(root.path("sync-a"))
            .path2(Path2::LocalDir(root.path("sync-b")))
            .config_path(root.path("rclone.conf"))
            .workdir(root.path("workdir"))
            .filters_file(root.path("filters.txt"))
            .backup_dir1(root.path("b1"))
            .backup_dir2(BackupDir2::LocalDir(root.path("b2")))
            .build()
            .unwrap();
        let args = args_as_strings(&local_req);
        assert!(!args.contains(&"--drive-skip-gdocs".to_string()));
        assert!(!args.contains(&"--drive-skip-shortcuts".to_string()));
        assert!(!args.contains(&"--password-command".to_string()));
        assert!(args.contains(&"--ask-password=false".to_string()));

        let other_remote_req = base_drive_builder(&root)
            .path2(Path2::OtherRemote {
                remote: "household".to_string(),
                path: "Data/Research".to_string(),
            })
            .build()
            .unwrap();
        let other_args = args_as_strings(&other_remote_req);
        assert!(!other_args.contains(&"--drive-skip-gdocs".to_string()));
        assert!(!other_args.contains(&"--drive-skip-shortcuts".to_string()));
    }

    #[test]
    fn rejects_relative_paths_and_empty_filters_path() {
        let root = TestFixtureDir::new("relative");
        let err = base_drive_builder(&root)
            .path1("relative/path")
            .build()
            .unwrap_err();
        assert!(matches!(
            err,
            RequestError::RelativePath { field: "path1", .. }
        ));

        let err = base_drive_builder(&root)
            .filters_file("")
            .build()
            .unwrap_err();
        assert!(matches!(
            err,
            RequestError::EmptyPath {
                field: "filters_file"
            }
        ));

        let err = base_drive_builder(&root)
            .filters_file("filters.txt")
            .build()
            .unwrap_err();
        assert!(matches!(
            err,
            RequestError::RelativePath {
                field: "filters_file",
                ..
            }
        ));

        let err = base_drive_builder(&root)
            .workdir("rel/workdir")
            .build()
            .unwrap_err();
        assert!(matches!(
            err,
            RequestError::RelativePath {
                field: "workdir",
                ..
            }
        ));
    }

    #[test]
    fn rejects_identical_or_nested_paths() {
        let root = TestFixtureDir::new("overlaps");
        let base_local = || {
            BisyncRequest::builder()
                .engine(test_engine(&root))
                .mode(RunMode::Normal)
                .config_path(root.path("rclone.conf"))
                .workdir(root.path("workdir"))
                .filters_file(root.path("filters.txt"))
                .backup_dir1(root.path("b1"))
                .backup_dir2(BackupDir2::LocalDir(root.path("b2")))
        };

        // path1 == path2
        assert!(matches!(
            base_local()
                .path1(root.path("tree"))
                .path2(Path2::LocalDir(root.path("tree")))
                .build()
                .unwrap_err(),
            RequestError::OverlappingPaths { .. }
        ));

        // path2 nested inside path1
        assert!(matches!(
            base_local()
                .path1(root.path("tree"))
                .path2(Path2::LocalDir(root.path("tree/sub")))
                .build()
                .unwrap_err(),
            RequestError::OverlappingPaths { .. }
        ));

        // path1 nested inside path2 via ..
        assert!(matches!(
            base_local()
                .path1(root.path("tree/a/../sub"))
                .path2(Path2::LocalDir(root.path("tree")))
                .build()
                .unwrap_err(),
            RequestError::OverlappingPaths { .. }
        ));

        // backup_dir1 nested inside path1
        assert!(matches!(
            base_local()
                .path1(root.path("tree"))
                .path2(Path2::LocalDir(root.path("other")))
                .backup_dir1(root.path("tree/recovery"))
                .build()
                .unwrap_err(),
            RequestError::OverlappingPaths { .. }
        ));

        // workdir nested inside path1
        assert!(matches!(
            base_local()
                .path1(root.path("tree"))
                .path2(Path2::LocalDir(root.path("other")))
                .workdir(root.path("tree/.bisync"))
                .build()
                .unwrap_err(),
            RequestError::OverlappingPaths { .. }
        ));

        // remote backup_dir2 nested inside remote path2
        assert!(matches!(
            base_drive_builder(&root)
                .backup_dir2(BackupDir2::Remote {
                    remote: "household".to_string(),
                    path: "Data/Research/Recovery".to_string(),
                })
                .build()
                .unwrap_err(),
            RequestError::OverlappingPaths { .. }
        ));

        // sibling names with common string prefix must NOT be rejected
        assert!(
            base_local()
                .path1(root.path("tree"))
                .path2(Path2::LocalDir(root.path("tree2")))
                .build()
                .is_ok()
        );
    }

    #[test]
    fn rejects_max_delete_outside_1_to_100() {
        let root = TestFixtureDir::new("max-delete");
        assert_eq!(
            base_drive_builder(&root)
                .max_delete_percent(0)
                .build()
                .unwrap_err(),
            RequestError::InvalidMaxDeletePercent(0)
        );
        assert_eq!(
            base_drive_builder(&root)
                .max_delete_percent(101)
                .build()
                .unwrap_err(),
            RequestError::InvalidMaxDeletePercent(101)
        );
        assert!(
            base_drive_builder(&root)
                .max_delete_percent(1)
                .build()
                .is_ok()
        );
        assert!(
            base_drive_builder(&root)
                .max_delete_percent(100)
                .build()
                .is_ok()
        );
    }

    #[test]
    fn rejects_unqualified_engine_version_platform_or_hash() {
        let root = TestFixtureDir::new("unqualified-engine");
        let manifest = QualificationManifest::embedded().unwrap();
        let (host_platform, host_pin) = manifest.host_platform_pin().unwrap();

        let bad_ver = EngineIdentity::for_unit_test(
            "1.75.0",
            host_pin.binary_sha256.clone(),
            root.path("rclone"),
            host_platform,
        );
        assert!(matches!(
            base_drive_builder(&root)
                .engine(bad_ver)
                .build()
                .unwrap_err(),
            RequestError::UnqualifiedEngineVersion { .. }
        ));

        let bad_hash = EngineIdentity::for_unit_test(
            crate::PINNED_RCLONE_VERSION,
            "0".repeat(64),
            root.path("rclone"),
            host_platform,
        );
        assert!(matches!(
            base_drive_builder(&root)
                .engine(bad_hash)
                .build()
                .unwrap_err(),
            RequestError::UnqualifiedEngineHash { .. }
        ));

        // Digest copied from another platform must be rejected on this host.
        let other_platform = if host_platform == "osx-arm64" {
            "linux-amd64"
        } else {
            "osx-arm64"
        };
        let other_pin = manifest.platform_pin(other_platform).unwrap();
        let wrong_platform = EngineIdentity::for_unit_test(
            crate::PINNED_RCLONE_VERSION,
            other_pin.binary_sha256.clone(),
            root.path("rclone"),
            other_platform,
        );
        assert!(matches!(
            base_drive_builder(&root)
                .engine(wrong_platform)
                .build()
                .unwrap_err(),
            RequestError::UnqualifiedEnginePlatform { .. }
        ));
    }

    #[test]
    fn formats_password_command_with_csv_space_sep_escaping_and_rejects_shells() {
        let root = TestFixtureDir::new("pw-cmd");
        let cmd = PasswordCommand::new(
            root.path("My App/cred helper"),
            vec!["--profile".to_string(), "a \"quoted\" val".to_string()],
        )
        .unwrap();
        let expected_prog = root.path("My App/cred helper").display().to_string();
        assert_eq!(
            cmd.to_rclone_space_sep_list(),
            format!("\"{expected_prog}\" --profile \"a \"\"quoted\"\" val\"")
        );

        assert!(matches!(
            PasswordCommand::new("/bin/sh", vec!["-c".to_string(), "echo pw".to_string()]),
            Err(RequestError::ShellPasswordCommand(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_absent_backup_dir_overlapping_live_tree_via_symlink_alias() {
        use std::os::unix::fs::symlink;
        let root = TestFixtureDir::new("symlink-overlap");
        let tree = root.path("tree");
        let alias = root.path("alias");
        let other = root.path("other");
        let workdir = root.path("workdir");
        let b2 = root.path("b2");
        let cfg = root.path("rclone.conf");
        let filters = root.path("filters.txt");
        fs::create_dir_all(&tree).unwrap();
        fs::create_dir_all(&other).unwrap();
        fs::create_dir_all(&workdir).unwrap();
        fs::write(&cfg, b"").unwrap();
        fs::write(&filters, b"").unwrap();
        symlink(&tree, &alias).unwrap();

        // alias/run-1 does NOT exist yet, but its existing ancestor `alias` resolves to `tree`.
        let err = BisyncRequest::builder()
            .engine(test_engine(&root))
            .mode(RunMode::Normal)
            .path1(&tree)
            .path2(Path2::LocalDir(other.clone()))
            .config_path(&cfg)
            .workdir(&workdir)
            .filters_file(&filters)
            .backup_dir1(alias.join("run-1"))
            .backup_dir2(BackupDir2::LocalDir(b2.clone()))
            .build()
            .unwrap_err();
        assert!(matches!(err, RequestError::OverlappingPaths { .. }));

        // Absent path2 under symlink alias also overlaps path1.
        let err2 = BisyncRequest::builder()
            .engine(test_engine(&root))
            .mode(RunMode::Normal)
            .path1(&tree)
            .path2(Path2::LocalDir(alias.join("sub")))
            .config_path(&cfg)
            .workdir(&workdir)
            .filters_file(&filters)
            .backup_dir1(root.path("b1"))
            .backup_dir2(BackupDir2::LocalDir(b2))
            .build()
            .unwrap_err();
        assert!(matches!(err2, RequestError::OverlappingPaths { .. }));

        // Dangling symlink ancestor must be rejected as unresolvable.
        let dangling = root.path("dangling");
        symlink(root.path("does-not-exist-target"), &dangling).unwrap();
        let err3 = BisyncRequest::builder()
            .engine(test_engine(&root))
            .mode(RunMode::Normal)
            .path1(&tree)
            .path2(Path2::LocalDir(other))
            .config_path(&cfg)
            .workdir(&workdir)
            .filters_file(&filters)
            .backup_dir1(dangling.join("run-1"))
            .backup_dir2(BackupDir2::LocalDir(root.path("b2-ok")))
            .build()
            .unwrap_err();
        assert!(matches!(err3, RequestError::UnresolvablePath { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn launch_boundary_rehashes_binary_and_refuses_replacement_after_qualification() {
        use std::os::unix::fs::PermissionsExt;
        let root = TestFixtureDir::new("launch-rehash");
        let bin = root.path("rclone");
        fs::write(&bin, b"qualified binary bytes").unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();

        let digest = sha256_file(&bin).unwrap();
        let platform = host_platform_key().unwrap();
        let manifest = QualificationManifest::from_toml_str(&format!(
            "[rclone]\nversion = \"1.76.0\"\n[rclone.{platform}]\nurl = \"https://example.invalid/rclone.zip\"\narchive_sha256 = \"{digest}\"\nbinary_sha256 = \"{digest}\"\n"
        ))
        .unwrap();

        let qual_runner = FakeProcessRunner::new();
        qual_runner.push_run(FakeRunScript {
            lines: vec![OutputLine {
                stream: StreamKind::Stdout,
                text: "rclone v1.76.0".to_string(),
                truncated: false,
            }],
            ..Default::default()
        });
        let engine = qualify_binary_with_runner(&bin, &manifest, &qual_runner).unwrap();

        let req = BisyncRequest::builder()
            .engine(engine)
            .mode(RunMode::Normal)
            .path1(root.path("p1"))
            .path2(Path2::LocalDir(root.path("p2")))
            .config_path(root.path("rclone.conf"))
            .workdir(root.path("workdir"))
            .filters_file(root.path("filters.txt"))
            .backup_dir1(root.path("b1"))
            .backup_dir2(BackupDir2::LocalDir(root.path("b2")))
            .build_with_manifest(&manifest)
            .unwrap();

        // Replace binary on disk AFTER qualification and request creation.
        fs::write(&bin, b"tampered after qualification").unwrap();
        let launch_runner = FakeProcessRunner::new();
        let err = req
            .spawn_with_manifest(&launch_runner, SpawnOptions::default(), &manifest)
            .unwrap_err();
        assert!(matches!(
            err,
            LaunchError::Qualification(QualificationError::Sha256Mismatch { .. })
        ));
        assert!(
            launch_runner.spawned_specs().is_empty(),
            "replaced binary must never be launched"
        );
    }

    #[cfg(unix)]
    #[test]
    fn real_runner_refuses_binary_replaced_after_verified_spec_creation() {
        use std::os::unix::fs::PermissionsExt;
        let root = TestFixtureDir::new("replace-after-spec");
        let bin = root.path("rclone");
        // Copy a harmless real executable (`current_exe`) so if spawn fails to re-hash it would actually execute.
        fs::copy(std::env::current_exe().unwrap(), &bin).unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();

        let digest = sha256_file(&bin).unwrap();
        let platform = host_platform_key().unwrap();
        let manifest = QualificationManifest::from_toml_str(&format!(
            "[rclone]\nversion = \"1.76.0\"\n[rclone.{platform}]\nurl = \"https://example.invalid/rclone.zip\"\narchive_sha256 = \"{digest}\"\nbinary_sha256 = \"{digest}\"\n"
        ))
        .unwrap();

        let qual_runner = FakeProcessRunner::new();
        qual_runner.push_run(FakeRunScript {
            lines: vec![OutputLine {
                stream: StreamKind::Stdout,
                text: "rclone v1.76.0".to_string(),
                truncated: false,
            }],
            ..Default::default()
        });
        let engine = qualify_binary_with_runner(&bin, &manifest, &qual_runner).unwrap();
        let req = BisyncRequest::builder()
            .engine(engine)
            .mode(RunMode::Normal)
            .path1(root.path("p1"))
            .path2(Path2::LocalDir(root.path("p2")))
            .config_path(root.path("rclone.conf"))
            .workdir(root.path("workdir"))
            .filters_file(root.path("filters.txt"))
            .backup_dir1(root.path("b1"))
            .backup_dir2(BackupDir2::LocalDir(root.path("b2")))
            .build_with_manifest(&manifest)
            .unwrap();

        // Create a verified ProcessSpec while the binary is still intact.
        let spec = req
            .to_verified_process_spec_with_manifest(&manifest)
            .unwrap();

        // Replace binary on disk AFTER spec creation, BEFORE RealProcessRunner::spawn executes.
        fs::write(&bin, b"tampered binary bytes after spec creation").unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();

        let err = crate::process::RealProcessRunner
            .spawn(&spec, SpawnOptions::default())
            .unwrap_err();
        assert!(
            matches!(err, ProcessError::BinaryIntegrityMismatch { .. }),
            "must refuse replaced binary with BinaryIntegrityMismatch before exec, got {err:?}"
        );
    }
}
