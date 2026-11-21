//! App-owned encrypted rclone configuration management, explicit single-remote
//! import, and per-account config mutation serialization (§7, §8, §19).

use std::{
    collections::{BTreeMap, HashMap},
    env,
    ffi::OsString,
    fmt,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

#[cfg(unix)]
use std::os::unix::{
    fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    io::AsRawFd,
};

use thiserror::Error;

use crate::{
    log_parser::strip_ansi_codes,
    process::{ProcessError, ProcessRunner, ProcessSpec, SpawnOptions, minimal_isolated_env},
    qualification::{
        EngineIdentity, QualificationError, QualificationManifest,
        verify_binary_integrity_with_manifest,
    },
    request::{PasswordCommand, RequestError, validate_remote_name_str},
};

/// Header written by rclone at the top of an encrypted configuration file.
pub const RCLONE_ENCRYPT_HEADER: &str = "RCLONE_ENCRYPT_V0:";

static SETUP_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Non-secret summary of a remote stored in an app-owned rclone config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteConfigSummary {
    pub remote_name: String,
    pub remote_type: String,
    pub root_folder_id: Option<String>,
    pub shared_with_me: bool,
    /// Boolean presence check for a saved OAuth token (never exposes the token).
    pub has_token: bool,
}

/// Specification for creating a Google Drive remote inside an app-owned
/// encrypted rclone configuration using the user's own OAuth client (§7, §8).
#[derive(Clone, PartialEq, Eq)]
pub struct DriveRemoteConfigSpec {
    pub remote_name: String,
    pub client_id: String,
    pub client_secret: String,
    pub scope: String,
    pub root_folder_id: String,
}

impl fmt::Debug for DriveRemoteConfigSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DriveRemoteConfigSpec")
            .field("remote_name", &self.remote_name)
            .field("client_id", &self.client_id)
            .field("client_secret", &"[REDACTED]")
            .field("scope", &self.scope)
            .field("root_folder_id", &self.root_folder_id)
            .finish()
    }
}

/// Specification for explicitly importing ONE named remote from a user-chosen
/// rclone config file into an app-owned encrypted config (§8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportRemoteSpec {
    pub source_config_path: PathBuf,
    pub source_remote_name: String,
    pub target_remote_name: String,
    pub target_config_path: PathBuf,
    pub lock_path: PathBuf,
    pub password_command: PasswordCommand,
    /// For Google Drive imports, overrides `root_folder_id` in the copied
    /// section to the requested shared parent folder ID (and `shared_with_me`
    /// is always removed).
    pub override_root_folder_id: Option<String>,
    /// Must be `true` to allow importing from any of rclone's default config
    /// locations (`<rclone-exe-dir>/rclone.conf`, `$XDG_CONFIG_HOME/rclone/rclone.conf`,
    /// `$HOME/.config/rclone/rclone.conf`, or `$HOME/.rclone.conf`).
    pub allow_global_default_source: bool,
}

/// Summary returned after explicitly importing one remote into an encrypted config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedRemoteInfo {
    pub source_remote_name: String,
    pub target_remote_name: String,
    pub remote_type: String,
    pub root_folder_id: Option<String>,
    pub target_config_path: PathBuf,
}

/// Errors returned while creating, importing, locking, or verifying an
/// app-owned encrypted rclone config.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("engine integrity check failed: {0}")]
    Qualification(#[from] QualificationError),
    #[error("invalid request parameter: {0}")]
    Request(#[from] RequestError),
    #[error("invalid field `{field}`: {reason}")]
    InvalidInput {
        field: &'static str,
        reason: &'static str,
    },
    #[error("refused to read user's global default rclone config `{path}` without explicit opt-in")]
    GlobalDefaultConfigRefused { path: PathBuf },
    #[error(
        "source config `{path}` is already encrypted and cannot be parsed for single-section import"
    )]
    EncryptedSourceUnsupported { path: PathBuf },
    #[error("remote section `[{remote}]` was not found in `{path}`")]
    RemoteNotFound { path: PathBuf, remote: String },
    #[error("duplicate remote section `[{remote}]` found in `{path}`")]
    DuplicateRemoteSection { path: PathBuf, remote: String },
    #[error("remote section `[{remote}]` is missing required `type` option")]
    MissingRemoteType { remote: String },
    #[error("target config `{path}` already exists; refusing to overwrite")]
    ConfigAlreadyExists { path: PathBuf },
    #[error("refused to follow symbolic link at `{path}`")]
    SymlinkRefused { path: PathBuf },
    #[error("config `{path}` is not encrypted (missing {RCLONE_ENCRYPT_HEADER} marker)")]
    NotEncrypted { path: PathBuf },
    #[error("authentication required to unlock or verify encrypted config: {reason}")]
    AuthenticationRequired { reason: String },
    #[error(
        "rclone config command `{command}` failed (code={code:?}, signal={signal:?}): {detail}"
    )]
    RcloneCommandFailed {
        command: &'static str,
        code: Option<i32>,
        signal: Option<i32>,
        detail: String,
    },
    #[error("process execution failed: {0}")]
    Process(#[from] ProcessError),
    #[error("filesystem error at `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Returns `true` if `content` contains rclone's encrypted configuration marker.
pub fn is_encrypted_config_content(content: &str) -> bool {
    content
        .lines()
        .any(|line| line.trim() == RCLONE_ENCRYPT_HEADER)
}

/// Returns `true` if captured stderr/stdout indicates a credential-store lock,
/// password-command failure, decryption failure, or OAuth auth failure.
pub(crate) fn stderr_indicates_auth_failure(text: &str) -> bool {
    let plain = strip_ansi_codes(text);
    let lower = plain.to_ascii_lowercase();
    lower.contains("--password-command")
        || lower.contains("password command failed")
        || lower.contains("unable to decrypt configuration")
        || lower.contains("authentication_required")
        || lower.contains("wrong password")
        || lower.contains("oauth2: cannot fetch token")
        || lower.contains("oauth2: token expired")
        || lower.contains("couldn't fetch token")
        || lower.contains("failed to refresh token")
        || lower.contains("token has been expired or revoked")
        || lower.contains("invalid_grant")
        || lower.contains("401 unauthorized")
        || lower.contains("invalid credentials")
}

fn contains_control_breaks(value: &str) -> bool {
    value.chars().any(|c| matches!(c, '\0' | '\r' | '\n'))
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::Prefix(p) => out.push(p.as_os_str()),
            Component::RootDir => out.push(comp.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(p) => out.push(p),
        }
    }
    out
}

fn resolve_canonical_or_lexical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| lexical_normalize(path))
}

/// Checks whether `candidate` matches any of rclone v1.76.0's default global
/// configuration locations (`<rclone-exe-dir>/rclone.conf`,
/// `$XDG_CONFIG_HOME/rclone/rclone.conf`, `$HOME/.config/rclone/rclone.conf`,
/// or `$HOME/.rclone.conf`), comparing both lexical and canonicalized paths.
pub fn is_global_default_rclone_config(
    candidate: &Path,
    home_override: Option<&Path>,
    xdg_config_home_override: Option<&Path>,
    rclone_exe_dir_override: Option<&Path>,
) -> bool {
    let cand_lex = lexical_normalize(candidate);
    let cand_can = resolve_canonical_or_lexical(candidate);

    let mut defaults: Vec<PathBuf> = Vec::new();
    let home = home_override.map(Path::to_path_buf).or_else(|| {
        env::var_os("HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    });
    let xdg = xdg_config_home_override.map(Path::to_path_buf).or_else(|| {
        env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    });

    if let Some(exe_dir) = rclone_exe_dir_override {
        defaults.push(exe_dir.join("rclone.conf"));
    }
    if let Some(xdg_dir) = xdg {
        defaults.push(xdg_dir.join("rclone").join("rclone.conf"));
    }
    if let Some(home_dir) = home {
        defaults.push(home_dir.join(".config").join("rclone").join("rclone.conf"));
        defaults.push(home_dir.join(".rclone.conf"));
    }

    defaults.into_iter().any(|default_path| {
        let def_lex = lexical_normalize(&default_path);
        let def_can = resolve_canonical_or_lexical(&default_path);
        cand_lex == def_lex || cand_can == def_can
    })
}

/// Ensures `dir` exists with private `0700` permissions on Unix and is not a symlink.
fn ensure_private_dir_no_symlink(dir: &Path) -> Result<(), ConfigError> {
    if !dir.is_absolute() {
        return Err(ConfigError::InvalidInput {
            field: "directory",
            reason: "must be an absolute path",
        });
    }
    if let Ok(meta) = fs::symlink_metadata(dir) {
        if meta.file_type().is_symlink() {
            return Err(ConfigError::SymlinkRefused {
                path: dir.to_path_buf(),
            });
        }
        if !meta.is_dir() {
            return Err(ConfigError::Io {
                path: dir.to_path_buf(),
                source: std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "expected a directory",
                ),
            });
        }
    } else {
        let create_res = {
            #[cfg(unix)]
            {
                fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(dir)
            }
            #[cfg(not(unix))]
            {
                fs::create_dir_all(dir)
            }
        };
        match create_res {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                let meta = fs::symlink_metadata(dir).map_err(|source| ConfigError::Io {
                    path: dir.to_path_buf(),
                    source,
                })?;
                if meta.file_type().is_symlink() {
                    return Err(ConfigError::SymlinkRefused {
                        path: dir.to_path_buf(),
                    });
                }
                if !meta.is_dir() {
                    return Err(ConfigError::Io {
                        path: dir.to_path_buf(),
                        source: std::io::Error::new(
                            std::io::ErrorKind::AlreadyExists,
                            "expected a directory",
                        ),
                    });
                }
            }
            Err(source) => {
                return Err(ConfigError::Io {
                    path: dir.to_path_buf(),
                    source,
                });
            }
        }
    }

    #[cfg(unix)]
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(|source| {
        ConfigError::Io {
            path: dir.to_path_buf(),
            source,
        }
    })?;
    Ok(())
}

/// Refuses symlinks on `path` and enforces `0600` file permissions on Unix.
pub fn enforce_private_config_permissions(path: &Path) -> Result<(), ConfigError> {
    let meta = fs::symlink_metadata(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if meta.file_type().is_symlink() {
        return Err(ConfigError::SymlinkRefused {
            path: path.to_path_buf(),
        });
    }
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|source| {
        ConfigError::Io {
            path: path.to_path_buf(),
            source,
        }
    })?;
    Ok(())
}

fn in_process_lock_table() -> &'static Mutex<HashMap<PathBuf, Arc<Mutex<()>>>> {
    static TABLE: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Exclusive per-account configuration lock serializing all config mutations
/// (existence checks, credential registration, config creation, import,
/// encryption, OAuth authorization, and refresh-capable listings) across both
/// threads and processes (§8).
pub struct AccountConfigLock {
    file: File,
    lock_path: PathBuf,
    // Kept in-struct so the in-process mutex stays held until after `flock` unlocks.
    _in_process_mutex: Arc<Mutex<()>>,
    _in_process_guard: Option<std::sync::MutexGuard<'static, ()>>,
}

impl fmt::Debug for AccountConfigLock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccountConfigLock")
            .field("lock_path", &self.lock_path)
            .finish()
    }
}

impl AccountConfigLock {
    /// Acquires the blocking exclusive lock on `lock_path` (`accounts/<id>/config.lock`).
    pub fn acquire(lock_path: &Path) -> Result<Self, ConfigError> {
        if !lock_path.is_absolute() {
            return Err(ConfigError::InvalidInput {
                field: "lock_path",
                reason: "must be an absolute path",
            });
        }
        let parent = lock_path.parent().ok_or(ConfigError::InvalidInput {
            field: "lock_path",
            reason: "must have a parent directory",
        })?;
        ensure_private_dir_no_symlink(parent)?;

        if let Ok(meta) = fs::symlink_metadata(lock_path)
            && meta.file_type().is_symlink()
        {
            return Err(ConfigError::SymlinkRefused {
                path: lock_path.to_path_buf(),
            });
        }

        let key = fs::canonicalize(parent)
            .unwrap_or_else(|_| lexical_normalize(parent))
            .join(lock_path.file_name().unwrap_or_default());

        let per_path_mutex = {
            let mut map = in_process_lock_table().lock().unwrap();
            Arc::clone(map.entry(key).or_insert_with(|| Arc::new(Mutex::new(()))))
        };

        // Acquire in-process lock first so threads serialize deterministically.
        // SAFETY: `_in_process_mutex` holds an `Arc` keeping the `Mutex` alive
        // at least as long as `_in_process_guard`, and `Drop` explicitly drops
        // `_in_process_guard` after releasing `flock` and BEFORE `_in_process_mutex`.
        let guard: std::sync::MutexGuard<'static, ()> = unsafe {
            let ptr: *const Mutex<()> = Arc::as_ptr(&per_path_mutex);
            (*ptr).lock().unwrap_or_else(|e| e.into_inner())
        };

        let mut opts = OpenOptions::new();
        opts.create(true).read(true).write(true);
        #[cfg(unix)]
        {
            opts.mode(0o600);
            opts.custom_flags(libc::O_NOFOLLOW);
        }
        let file = opts.open(lock_path).map_err(|source| {
            #[cfg(unix)]
            if source.raw_os_error() == Some(libc::ELOOP) {
                return ConfigError::SymlinkRefused {
                    path: lock_path.to_path_buf(),
                };
            }
            ConfigError::Io {
                path: lock_path.to_path_buf(),
                source,
            }
        })?;
        enforce_private_config_permissions(lock_path)?;

        #[cfg(unix)]
        {
            let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            if ret != 0 {
                return Err(ConfigError::Io {
                    path: lock_path.to_path_buf(),
                    source: std::io::Error::last_os_error(),
                });
            }
        }

        Ok(Self {
            file,
            lock_path: lock_path.to_path_buf(),
            _in_process_mutex: per_path_mutex,
            _in_process_guard: Some(guard),
        })
    }

    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }
}

impl Drop for AccountConfigLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
        }
        // Explicitly release the in-process guard before `_in_process_mutex` drops.
        let _ = self._in_process_guard.take();
    }
}

/// Parsed single remote section from an unencrypted rclone INI config.
#[derive(Clone, PartialEq, Eq)]
pub struct ParsedIniSection {
    pub remote_name: String,
    pub entries: Vec<(String, String)>,
}

impl fmt::Debug for ParsedIniSection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ParsedIniSection")
            .field("remote_name", &self.remote_name)
            .field("option_count", &self.entries.len())
            .finish()
    }
}

impl ParsedIniSection {
    pub fn option(&self, key: &str) -> Option<&str> {
        self.entries
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    pub fn remote_type(&self) -> Option<&str> {
        self.option("type")
    }
}

/// Extracts ONLY the named `[<remote_name>]` section from unencrypted rclone
/// INI text. Refuses encrypted files, missing sections, duplicate section
/// headers, and sections missing `type`.
pub fn extract_single_remote_section(
    source_path: &Path,
    content: &str,
    remote_name: &str,
) -> Result<ParsedIniSection, ConfigError> {
    validate_remote_name_str(remote_name)?;
    if is_encrypted_config_content(content) {
        return Err(ConfigError::EncryptedSourceUnsupported {
            path: source_path.to_path_buf(),
        });
    }

    let mut current_section: Option<String> = None;
    let mut found_count = 0usize;
    let mut entries: Vec<(String, String)> = Vec::new();

    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') && line.len() >= 2 {
            let section = line[1..line.len() - 1].trim().to_string();
            if section == remote_name {
                found_count += 1;
                if found_count > 1 {
                    return Err(ConfigError::DuplicateRemoteSection {
                        path: source_path.to_path_buf(),
                        remote: remote_name.to_string(),
                    });
                }
            }
            current_section = Some(section);
            continue;
        }
        if current_section.as_deref() == Some(remote_name)
            && let Some((k, v)) = raw_line.split_once('=')
        {
            let key = k.trim();
            let val = v.trim();
            if !key.is_empty() {
                entries.push((key.to_string(), val.to_string()));
            }
        }
    }

    if found_count == 0 {
        return Err(ConfigError::RemoteNotFound {
            path: source_path.to_path_buf(),
            remote: remote_name.to_string(),
        });
    }

    let section = ParsedIniSection {
        remote_name: remote_name.to_string(),
        entries,
    };
    if section
        .remote_type()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .is_none()
    {
        return Err(ConfigError::MissingRemoteType {
            remote: remote_name.to_string(),
        });
    }
    Ok(section)
}

fn render_single_section_ini(remote_name: &str, entries: &[(String, String)]) -> String {
    let mut out = String::new();
    out.push('[');
    out.push_str(remote_name);
    out.push_str("]\n");
    for (k, v) in entries {
        out.push_str(k);
        out.push_str(" = ");
        out.push_str(v);
        out.push('\n');
    }
    out
}

fn is_leftover_temp_or_backup_name(name: &str) -> bool {
    if name == "rclone.conf" || name == "config.lock" {
        return false;
    }
    name.starts_with(".rclone.conf.setup.")
        || name.starts_with(".rclone.conf.tmp.")
        || name.starts_with("rclone.conf.")
        || name.starts_with("rclone-config")
        || name.ends_with(".bak")
        || name.ends_with(".tmp")
        || name.ends_with('~')
}

/// Scrubs and removes any leftover setup temp files (`.rclone.conf.setup.*`) and
/// rclone backup/temp files (`rclone.conf.*`, `*.bak`, `rclone-config*`, etc.)
/// inside `account_dir`, never touching `rclone.conf`, `config.lock`, or symlinks.
pub fn cleanup_account_temp_and_backup_files(account_dir: &Path) -> Result<usize, ConfigError> {
    let meta = match fs::symlink_metadata(account_dir) {
        Ok(m) => m,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(source) => {
            return Err(ConfigError::Io {
                path: account_dir.to_path_buf(),
                source,
            });
        }
    };
    if meta.file_type().is_symlink() {
        return Err(ConfigError::SymlinkRefused {
            path: account_dir.to_path_buf(),
        });
    }
    if !meta.is_dir() {
        return Ok(0);
    }

    let entries = fs::read_dir(account_dir).map_err(|source| ConfigError::Io {
        path: account_dir.to_path_buf(),
        source,
    })?;
    let mut removed = 0usize;
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !is_leftover_temp_or_backup_name(name) {
            continue;
        }
        if let Ok(emeta) = fs::symlink_metadata(&path)
            && !emeta.file_type().is_symlink()
            && emeta.is_file()
        {
            scrub_and_remove_file(&path);
            removed += 1;
        }
    }
    Ok(removed)
}

fn create_exclusive_setup_temp(account_dir: &Path, content: &[u8]) -> Result<PathBuf, ConfigError> {
    ensure_private_dir_no_symlink(account_dir)?;
    for _ in 0..64 {
        let seq = SETUP_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp_path = account_dir.join(format!(".rclone.conf.setup.{}.{seq}", std::process::id()));
        let mut opts = OpenOptions::new();
        opts.create_new(true).write(true);
        #[cfg(unix)]
        {
            opts.mode(0o600);
            opts.custom_flags(libc::O_NOFOLLOW);
        }
        match opts.open(&tmp_path) {
            Ok(mut file) => {
                let write_res = (|| -> Result<(), ConfigError> {
                    file.write_all(content).map_err(|source| ConfigError::Io {
                        path: tmp_path.clone(),
                        source,
                    })?;
                    file.sync_all().map_err(|source| ConfigError::Io {
                        path: tmp_path.clone(),
                        source,
                    })?;
                    enforce_private_config_permissions(&tmp_path)?;
                    Ok(())
                })();
                if let Err(err) = write_res {
                    scrub_and_remove_file(&tmp_path);
                    return Err(err);
                }
                return Ok(tmp_path);
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(ConfigError::Io {
                    path: tmp_path,
                    source,
                });
            }
        }
    }
    Err(ConfigError::Io {
        path: account_dir.to_path_buf(),
        source: std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "exhausted temporary setup config filenames",
        ),
    })
}

fn scrub_and_remove_file(path: &Path) {
    if let Ok(meta) = fs::symlink_metadata(path)
        && !meta.file_type().is_symlink()
        && meta.is_file()
    {
        let len = meta.len() as usize;
        if len > 0
            && let Ok(mut f) = OpenOptions::new().write(true).open(path)
        {
            let _ = f.write_all(&vec![0u8; len.min(64 * 1024)]);
            let _ = f.sync_all();
        }
        let _ = fs::remove_file(path);
    }
}

fn run_verified_rclone_config_cmd(
    engine: &EngineIdentity,
    manifest: &QualificationManifest,
    runner: &dyn ProcessRunner,
    command_label: &'static str,
    args: Vec<OsString>,
    account_dir: &Path,
) -> Result<crate::process::RunOutput, ConfigError> {
    verify_binary_integrity_with_manifest(engine, manifest)?;
    let spec = ProcessSpec {
        program: engine.path().to_path_buf(),
        args,
        current_dir: None,
        env: minimal_isolated_env(Some(account_dir)),
        expected_binary_sha256: Some(engine.sha256().to_string()),
    };
    let handle = runner.spawn(&spec, SpawnOptions::default())?;
    let run_out = handle.wait()?;
    if !run_out.exit_status.is_success() {
        let stderr = run_out.output.stderr_text();
        let stdout = run_out.output.stdout_text();
        let combined = if stderr.is_empty() { stdout } else { stderr };
        if stderr_indicates_auth_failure(&combined) {
            return Err(ConfigError::AuthenticationRequired {
                reason: combined.trim().to_string(),
            });
        }
        return Err(ConfigError::RcloneCommandFailed {
            command: command_label,
            code: run_out.exit_status.code,
            signal: run_out.exit_status.signal,
            detail: combined.trim().to_string(),
        });
    }
    Ok(run_out)
}

/// Builds the config in an exclusive `0600` temp file inside the account dir,
/// encrypts it in place via pinned rclone `config encryption set`, verifies
/// `RCLONE_ENCRYPT_V0:` and `config encryption check`, and ONLY THEN atomically
/// publishes it to `config_path`. Always cleans up setup temp and engine backup
/// files on both failure and success.
fn create_and_publish_encrypted_config_locked(
    engine: &EngineIdentity,
    manifest: &QualificationManifest,
    runner: &dyn ProcessRunner,
    config_path: &Path,
    _lock: &AccountConfigLock,
    password_command: &PasswordCommand,
    ini_bytes: &[u8],
) -> Result<(), ConfigError> {
    if !config_path.is_absolute() {
        return Err(ConfigError::InvalidInput {
            field: "config_path",
            reason: "must be an absolute path",
        });
    }
    let account_dir = config_path.parent().ok_or(ConfigError::InvalidInput {
        field: "config_path",
        reason: "must have a parent directory",
    })?;
    ensure_private_dir_no_symlink(account_dir)?;

    if let Ok(meta) = fs::symlink_metadata(config_path) {
        if meta.file_type().is_symlink() {
            return Err(ConfigError::SymlinkRefused {
                path: config_path.to_path_buf(),
            });
        }
        return Err(ConfigError::ConfigAlreadyExists {
            path: config_path.to_path_buf(),
        });
    }

    let tmp_path = create_exclusive_setup_temp(account_dir, ini_bytes)?;
    let enc_args = vec![
        OsString::from("config"),
        OsString::from("encryption"),
        OsString::from("set"),
        OsString::from("--config"),
        tmp_path.as_os_str().to_os_string(),
        OsString::from("--password-command"),
        OsString::from(password_command.to_rclone_space_sep_list()),
        OsString::from("--ask-password=false"),
    ];

    let setup_res = (|| -> Result<(), ConfigError> {
        run_verified_rclone_config_cmd(
            engine,
            manifest,
            runner,
            "config encryption set",
            enc_args,
            account_dir,
        )?;

        enforce_private_config_permissions(&tmp_path)?;
        let on_disk = fs::read_to_string(&tmp_path).map_err(|source| ConfigError::Io {
            path: tmp_path.clone(),
            source,
        })?;
        if !is_encrypted_config_content(&on_disk) {
            return Err(ConfigError::NotEncrypted {
                path: tmp_path.clone(),
            });
        }

        verify_encrypted_config_with_manifest(
            engine,
            manifest,
            runner,
            &tmp_path,
            password_command,
        )?;

        // Refuse to overwrite if final config path appeared or became a symlink.
        if let Ok(meta) = fs::symlink_metadata(config_path) {
            if meta.file_type().is_symlink() {
                return Err(ConfigError::SymlinkRefused {
                    path: config_path.to_path_buf(),
                });
            }
            return Err(ConfigError::ConfigAlreadyExists {
                path: config_path.to_path_buf(),
            });
        }

        // Publish exclusively via hard_link + unlink (never silently overwrites).
        match fs::hard_link(&tmp_path, config_path) {
            Ok(()) => {
                let _ = fs::remove_file(&tmp_path);
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(ConfigError::ConfigAlreadyExists {
                    path: config_path.to_path_buf(),
                });
            }
            Err(_) => {
                fs::rename(&tmp_path, config_path).map_err(|source| ConfigError::Io {
                    path: config_path.to_path_buf(),
                    source,
                })?;
            }
        }
        enforce_private_config_permissions(config_path)?;
        Ok(())
    })();

    if setup_res.is_err() {
        scrub_and_remove_file(&tmp_path);
    }
    let _ = cleanup_account_temp_and_backup_files(account_dir);
    setup_res
}

/// Initializes a new empty app-owned encrypted rclone config (`0600`) through
/// pinned rclone's `config encryption set` + `config encryption check`.
pub fn init_encrypted_config(
    engine: &EngineIdentity,
    runner: &dyn ProcessRunner,
    config_path: &Path,
    lock_path: &Path,
    password_command: &PasswordCommand,
) -> Result<(), ConfigError> {
    let lock = AccountConfigLock::acquire(lock_path)?;
    init_encrypted_config_locked(engine, runner, config_path, &lock, password_command)
}

/// Caller-locked variant of [`init_encrypted_config`].
pub fn init_encrypted_config_locked(
    engine: &EngineIdentity,
    runner: &dyn ProcessRunner,
    config_path: &Path,
    lock: &AccountConfigLock,
    password_command: &PasswordCommand,
) -> Result<(), ConfigError> {
    let manifest = QualificationManifest::embedded()?;
    create_and_publish_encrypted_config_locked(
        engine,
        &manifest,
        runner,
        config_path,
        lock,
        password_command,
        b"",
    )
}

/// Verifies an existing app-owned rclone config is `0600`, not a symlink,
/// carries the `RCLONE_ENCRYPT_V0:` header, and passes `rclone config encryption check`.
pub fn verify_encrypted_config(
    engine: &EngineIdentity,
    runner: &dyn ProcessRunner,
    config_path: &Path,
    password_command: &PasswordCommand,
) -> Result<(), ConfigError> {
    let manifest = QualificationManifest::embedded()?;
    verify_encrypted_config_with_manifest(engine, &manifest, runner, config_path, password_command)
}

pub(crate) fn verify_encrypted_config_with_manifest(
    engine: &EngineIdentity,
    manifest: &QualificationManifest,
    runner: &dyn ProcessRunner,
    config_path: &Path,
    password_command: &PasswordCommand,
) -> Result<(), ConfigError> {
    enforce_private_config_permissions(config_path)?;
    let on_disk = fs::read_to_string(config_path).map_err(|source| ConfigError::Io {
        path: config_path.to_path_buf(),
        source,
    })?;
    if !is_encrypted_config_content(&on_disk) {
        return Err(ConfigError::NotEncrypted {
            path: config_path.to_path_buf(),
        });
    }
    let account_dir = config_path.parent().ok_or(ConfigError::InvalidInput {
        field: "config_path",
        reason: "must have a parent directory",
    })?;
    let args = vec![
        OsString::from("config"),
        OsString::from("encryption"),
        OsString::from("check"),
        OsString::from("--config"),
        config_path.as_os_str().to_os_string(),
        OsString::from("--password-command"),
        OsString::from(password_command.to_rclone_space_sep_list()),
        OsString::from("--ask-password=false"),
    ];
    run_verified_rclone_config_cmd(
        engine,
        manifest,
        runner,
        "config encryption check",
        args,
        account_dir,
    )?;
    Ok(())
}

/// Creates a new app-owned encrypted rclone config for a Google Drive account
/// rooted at `spec.root_folder_id` with the user's own OAuth `client_id` and
/// `client_secret` (§7, §8).
///
/// Neither `client_secret` nor the config passphrase is ever placed in argv or logs.
pub fn create_drive_remote_config(
    engine: &EngineIdentity,
    runner: &dyn ProcessRunner,
    config_path: &Path,
    lock_path: &Path,
    password_command: &PasswordCommand,
    spec: &DriveRemoteConfigSpec,
) -> Result<(), ConfigError> {
    let lock = AccountConfigLock::acquire(lock_path)?;
    create_drive_remote_config_locked(engine, runner, config_path, &lock, password_command, spec)
}

/// Caller-locked variant of [`create_drive_remote_config`] so callers can hold
/// one [`AccountConfigLock`] across existence checks, credential registration,
/// config publication, and OAuth authorization.
pub fn create_drive_remote_config_locked(
    engine: &EngineIdentity,
    runner: &dyn ProcessRunner,
    config_path: &Path,
    lock: &AccountConfigLock,
    password_command: &PasswordCommand,
    spec: &DriveRemoteConfigSpec,
) -> Result<(), ConfigError> {
    let manifest = QualificationManifest::embedded()?;
    create_drive_remote_config_with_manifest_locked(
        engine,
        &manifest,
        runner,
        config_path,
        lock,
        password_command,
        spec,
    )
}

pub(crate) fn create_drive_remote_config_with_manifest_locked(
    engine: &EngineIdentity,
    manifest: &QualificationManifest,
    runner: &dyn ProcessRunner,
    config_path: &Path,
    lock: &AccountConfigLock,
    password_command: &PasswordCommand,
    spec: &DriveRemoteConfigSpec,
) -> Result<(), ConfigError> {
    validate_remote_name_str(&spec.remote_name)?;
    if spec.client_id.trim().is_empty() || contains_control_breaks(&spec.client_id) {
        return Err(ConfigError::InvalidInput {
            field: "client_id",
            reason: "must be non-empty and contain no control characters",
        });
    }
    if spec.client_secret.trim().is_empty() || contains_control_breaks(&spec.client_secret) {
        return Err(ConfigError::InvalidInput {
            field: "client_secret",
            reason: "must be non-empty and contain no control characters",
        });
    }
    if spec.root_folder_id.trim().is_empty()
        || contains_control_breaks(&spec.root_folder_id)
        || spec.root_folder_id.chars().any(char::is_whitespace)
    {
        return Err(ConfigError::InvalidInput {
            field: "root_folder_id",
            reason: "must be a non-empty Drive folder ID without whitespace or control characters",
        });
    }
    let scope = if spec.scope.trim().is_empty() {
        "drive"
    } else {
        spec.scope.trim()
    };
    if contains_control_breaks(scope) {
        return Err(ConfigError::InvalidInput {
            field: "scope",
            reason: "must not contain control characters",
        });
    }

    // Explicitly do NOT set `shared_with_me` (§7: root directly at the shared parent ID).
    let entries = vec![
        ("type".to_string(), "drive".to_string()),
        ("client_id".to_string(), spec.client_id.trim().to_string()),
        (
            "client_secret".to_string(),
            spec.client_secret.trim().to_string(),
        ),
        ("scope".to_string(), scope.to_string()),
        (
            "root_folder_id".to_string(),
            spec.root_folder_id.trim().to_string(),
        ),
    ];
    let ini = render_single_section_ini(&spec.remote_name, &entries);
    create_and_publish_encrypted_config_locked(
        engine,
        manifest,
        runner,
        config_path,
        lock,
        password_command,
        ini.as_bytes(),
    )
}

/// Explicitly imports ONE named remote section from a user-chosen rclone config
/// into a newly created encrypted app-owned config without ever mutating the source.
pub fn import_single_remote_into_encrypted_config(
    engine: &EngineIdentity,
    runner: &dyn ProcessRunner,
    spec: &ImportRemoteSpec,
) -> Result<ImportedRemoteInfo, ConfigError> {
    let lock = AccountConfigLock::acquire(&spec.lock_path)?;
    import_single_remote_into_encrypted_config_locked(engine, runner, spec, &lock)
}

/// Caller-locked variant of [`import_single_remote_into_encrypted_config`].
pub fn import_single_remote_into_encrypted_config_locked(
    engine: &EngineIdentity,
    runner: &dyn ProcessRunner,
    spec: &ImportRemoteSpec,
    lock: &AccountConfigLock,
) -> Result<ImportedRemoteInfo, ConfigError> {
    let manifest = QualificationManifest::embedded()?;
    import_single_remote_internal_locked(engine, &manifest, runner, spec, lock, None, None, None)
}

/// Variant accepting explicit `HOME` / `XDG_CONFIG_HOME` / executable-directory
/// overrides for deterministic default-config path refusal tests without mutating env.
pub fn import_single_remote_with_home_override(
    engine: &EngineIdentity,
    runner: &dyn ProcessRunner,
    spec: &ImportRemoteSpec,
    home_override: Option<&Path>,
    xdg_config_home_override: Option<&Path>,
    rclone_exe_dir_override: Option<&Path>,
) -> Result<ImportedRemoteInfo, ConfigError> {
    let manifest = QualificationManifest::embedded()?;
    let lock = AccountConfigLock::acquire(&spec.lock_path)?;
    import_single_remote_internal_locked(
        engine,
        &manifest,
        runner,
        spec,
        &lock,
        home_override,
        xdg_config_home_override,
        rclone_exe_dir_override,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn import_single_remote_internal_locked(
    engine: &EngineIdentity,
    manifest: &QualificationManifest,
    runner: &dyn ProcessRunner,
    spec: &ImportRemoteSpec,
    lock: &AccountConfigLock,
    home_override: Option<&Path>,
    xdg_config_home_override: Option<&Path>,
    rclone_exe_dir_override: Option<&Path>,
) -> Result<ImportedRemoteInfo, ConfigError> {
    validate_remote_name_str(&spec.source_remote_name)?;
    validate_remote_name_str(&spec.target_remote_name)?;

    if !spec.source_config_path.is_absolute() {
        return Err(ConfigError::InvalidInput {
            field: "source_config_path",
            reason: "must be an absolute path",
        });
    }
    let exe_dir = rclone_exe_dir_override.or_else(|| engine.path().parent());
    if !spec.allow_global_default_source
        && is_global_default_rclone_config(
            &spec.source_config_path,
            home_override,
            xdg_config_home_override,
            exe_dir,
        )
    {
        return Err(ConfigError::GlobalDefaultConfigRefused {
            path: spec.source_config_path.clone(),
        });
    }

    // Read-only access to the source file; never opened for writing.
    let source_text =
        fs::read_to_string(&spec.source_config_path).map_err(|source| ConfigError::Io {
            path: spec.source_config_path.clone(),
            source,
        })?;
    let mut parsed = extract_single_remote_section(
        &spec.source_config_path,
        &source_text,
        &spec.source_remote_name,
    )?;
    let remote_type = parsed
        .remote_type()
        .expect("validated non-empty type")
        .to_string();

    let effective_root_folder_id = if remote_type.eq_ignore_ascii_case("drive") {
        // §7: Never retain `shared_with_me`; always bind Drive remotes directly
        // to the actual shared parent folder ID.
        parsed
            .entries
            .retain(|(k, _)| !k.eq_ignore_ascii_case("shared_with_me"));

        let chosen_root = spec
            .override_root_folder_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or_else(|| {
                parsed
                    .option("root_folder_id")
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
            })
            .map(ToOwned::to_owned)
            .ok_or(ConfigError::InvalidInput {
                field: "root_folder_id",
                reason: "Drive import requires a shared parent root_folder_id (either in the source remote or via --root-folder-id)",
            })?;

        if contains_control_breaks(&chosen_root) || chosen_root.chars().any(char::is_whitespace) {
            return Err(ConfigError::InvalidInput {
                field: "root_folder_id",
                reason: "must be a non-empty Drive folder ID without whitespace or control characters",
            });
        }

        parsed
            .entries
            .retain(|(k, _)| !k.eq_ignore_ascii_case("root_folder_id"));
        parsed
            .entries
            .push(("root_folder_id".to_string(), chosen_root.clone()));
        Some(chosen_root)
    } else {
        spec.override_root_folder_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
            .or_else(|| {
                parsed
                    .option("root_folder_id")
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(ToOwned::to_owned)
            })
    };

    let single_ini = render_single_section_ini(&spec.target_remote_name, &parsed.entries);
    create_and_publish_encrypted_config_locked(
        engine,
        manifest,
        runner,
        &spec.target_config_path,
        lock,
        &spec.password_command,
        single_ini.as_bytes(),
    )?;

    Ok(ImportedRemoteInfo {
        source_remote_name: spec.source_remote_name.clone(),
        target_remote_name: spec.target_remote_name.clone(),
        remote_type,
        root_folder_id: effective_root_folder_id,
        target_config_path: spec.target_config_path.clone(),
    })
}

fn json_value_has_oauth_token(val: Option<&serde_json::Value>) -> bool {
    let Some(raw) = val.and_then(|v| v.as_str()).map(str::trim) else {
        return false;
    };
    if raw.is_empty() {
        return false;
    }
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(raw) else {
        return false;
    };
    let Some(obj) = parsed.as_object() else {
        return false;
    };
    let has_non_empty = |k: &str| {
        obj.get(k)
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.trim().is_empty())
    };
    has_non_empty("access_token") || has_non_empty("refresh_token")
}

/// Reads non-secret metadata (`remote_name`, `type`, `root_folder_id`,
/// `shared_with_me`, `has_token`) for all remotes inside an encrypted app-owned
/// config via `rclone config dump`. Never returns or logs secrets or tokens.
pub fn list_encrypted_config_remotes(
    engine: &EngineIdentity,
    runner: &dyn ProcessRunner,
    config_path: &Path,
    password_command: &PasswordCommand,
) -> Result<Vec<RemoteConfigSummary>, ConfigError> {
    let manifest = QualificationManifest::embedded()?;
    list_encrypted_config_remotes_with_manifest(
        engine,
        &manifest,
        runner,
        config_path,
        password_command,
    )
}

pub(crate) fn list_encrypted_config_remotes_with_manifest(
    engine: &EngineIdentity,
    manifest: &QualificationManifest,
    runner: &dyn ProcessRunner,
    config_path: &Path,
    password_command: &PasswordCommand,
) -> Result<Vec<RemoteConfigSummary>, ConfigError> {
    verify_encrypted_config_with_manifest(engine, manifest, runner, config_path, password_command)?;
    let account_dir = config_path.parent().ok_or(ConfigError::InvalidInput {
        field: "config_path",
        reason: "must have a parent directory",
    })?;
    let args = vec![
        OsString::from("config"),
        OsString::from("dump"),
        OsString::from("--config"),
        config_path.as_os_str().to_os_string(),
        OsString::from("--password-command"),
        OsString::from(password_command.to_rclone_space_sep_list()),
        OsString::from("--ask-password=false"),
    ];
    let run_out =
        run_verified_rclone_config_cmd(engine, manifest, runner, "config dump", args, account_dir)?;
    let dumped: BTreeMap<String, BTreeMap<String, serde_json::Value>> =
        serde_json::from_str(&run_out.output.stdout_text()).map_err(|err| {
            ConfigError::RcloneCommandFailed {
                command: "config dump",
                code: run_out.exit_status.code,
                signal: run_out.exit_status.signal,
                detail: format!("failed to parse config dump JSON: {err}"),
            }
        })?;

    Ok(dumped
        .into_iter()
        .map(|(remote_name, opts)| {
            let remote_type = opts
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let root_folder_id = opts
                .get("root_folder_id")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToOwned::to_owned);
            let shared_with_me = opts
                .get("shared_with_me")
                .and_then(|v| v.as_str())
                .is_some_and(|v| v.eq_ignore_ascii_case("true"));
            let has_token = json_value_has_oauth_token(opts.get("token"));
            RemoteConfigSummary {
                remote_name,
                remote_type,
                root_folder_id,
                shared_with_me,
                has_token,
            }
        })
        .collect())
}

/// Checks (boolean presence only; never logs or returns the token) whether
/// `remote_name` inside `config_path` has a saved OAuth token.
pub fn encrypted_remote_has_token(
    engine: &EngineIdentity,
    runner: &dyn ProcessRunner,
    config_path: &Path,
    remote_name: &str,
    password_command: &PasswordCommand,
) -> Result<bool, ConfigError> {
    let manifest = QualificationManifest::embedded()?;
    encrypted_remote_has_token_with_manifest(
        engine,
        &manifest,
        runner,
        config_path,
        remote_name,
        password_command,
    )
}

pub(crate) fn encrypted_remote_has_token_with_manifest(
    engine: &EngineIdentity,
    manifest: &QualificationManifest,
    runner: &dyn ProcessRunner,
    config_path: &Path,
    remote_name: &str,
    password_command: &PasswordCommand,
) -> Result<bool, ConfigError> {
    let remotes = list_encrypted_config_remotes_with_manifest(
        engine,
        manifest,
        runner,
        config_path,
        password_command,
    )?;
    Ok(remotes
        .into_iter()
        .find(|r| r.remote_name == remote_name)
        .is_some_and(|r| r.has_token))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::{
        ExitStatusInfo, FakeProcessRunner, FakeRunScript, OutputLine, StreamKind,
    };
    use crate::qualification::{host_platform_key, qualify_binary_with_runner, sha256_file};

    fn unique_tmp(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "foldersync-cfg-unit-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn extract_single_section_copies_only_named_remote_and_rejects_encrypted_or_duplicates() {
        let src = Path::new("/tmp/src.conf");
        let ini = "\
# comment
[alpha]
type = local
nounc = true

[beta]
type = drive
client_secret = top-secret
";
        let section = extract_single_remote_section(src, ini, "alpha").unwrap();
        assert_eq!(section.remote_name, "alpha");
        assert_eq!(section.remote_type(), Some("local"));
        assert_eq!(section.option("nounc"), Some("true"));
        assert!(section.option("client_secret").is_none());

        assert!(matches!(
            extract_single_remote_section(src, ini, "missing"),
            Err(ConfigError::RemoteNotFound { .. })
        ));

        let dup = "[a]\ntype = local\n[a]\ntype = local\n";
        assert!(matches!(
            extract_single_remote_section(src, dup, "a"),
            Err(ConfigError::DuplicateRemoteSection { .. })
        ));

        let enc = "# Encrypted rclone configuration File\n\nRCLONE_ENCRYPT_V0:\nabcdef==\n";
        assert!(matches!(
            extract_single_remote_section(src, enc, "a"),
            Err(ConfigError::EncryptedSourceUnsupported { .. })
        ));
    }

    #[test]
    fn global_default_config_detection_catches_exe_dir_xdg_dotconfig_and_legacy_dotfile() {
        let tmp = unique_tmp("global-detect");
        let home = tmp.join("home");
        let xdg = tmp.join("xdg");
        let exe_dir = tmp.join("portable-rclone-bin");
        fs::create_dir_all(home.join(".config/rclone")).unwrap();
        fs::create_dir_all(xdg.join("rclone")).unwrap();
        fs::create_dir_all(&exe_dir).unwrap();

        let p1 = home.join(".config/rclone/rclone.conf");
        let p2 = xdg.join("rclone/rclone.conf");
        let p3 = home.join(".rclone.conf");
        let p4 = exe_dir.join("rclone.conf");
        let p_other = tmp.join("custom/my.conf");

        assert!(is_global_default_rclone_config(
            &p1,
            Some(&home),
            None,
            Some(&exe_dir)
        ));
        assert!(is_global_default_rclone_config(
            &p2,
            Some(&home),
            Some(&xdg),
            Some(&exe_dir)
        ));
        assert!(is_global_default_rclone_config(
            &p3,
            Some(&home),
            None,
            Some(&exe_dir)
        ));
        assert!(is_global_default_rclone_config(
            &p4,
            Some(&home),
            Some(&xdg),
            Some(&exe_dir)
        ));
        assert!(!is_global_default_rclone_config(
            &p_other,
            Some(&home),
            Some(&xdg),
            Some(&exe_dir)
        ));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[cfg(unix)]
    #[test]
    fn failed_encryption_never_publishes_final_config_and_cleans_temp_and_backup_files() {
        let tmp = unique_tmp("enc-fail-scrub");
        let fake_bin = tmp.join("rclone");
        fs::write(&fake_bin, b"fake rclone binary").unwrap();
        fs::set_permissions(&fake_bin, fs::Permissions::from_mode(0o755)).unwrap();

        let digest = sha256_file(&fake_bin).unwrap();
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
        let engine = qualify_binary_with_runner(&fake_bin, &manifest, &qual_runner).unwrap();

        let acct_dir = tmp.join("accounts/a1");
        fs::create_dir_all(&acct_dir).unwrap();
        // Simulate a stale plaintext backup file from a prior interrupted engine save.
        let simulated_bak = acct_dir.join("rclone.conf.123456789");
        fs::write(&simulated_bak, b"[household]\nclient_secret = stale\n").unwrap();

        let runner = FakeProcessRunner::new();
        runner.push_run(FakeRunScript {
            lines: vec![OutputLine {
                stream: StreamKind::Stderr,
                text: "ERROR : Using --password-command returned: exit status 3".to_string(),
                truncated: false,
            }],
            exit_status: ExitStatusInfo {
                code: Some(1),
                signal: None,
            },
            ..Default::default()
        });

        let cfg = acct_dir.join("rclone.conf");
        let lock_path = acct_dir.join("config.lock");
        let lock = AccountConfigLock::acquire(&lock_path).unwrap();
        let pw = PasswordCommand::new(
            tmp.join("bin/sync-cli"),
            vec![
                "credential-helper".to_string(),
                "--account".to_string(),
                "a1".to_string(),
            ],
        )
        .unwrap();
        let spec = DriveRemoteConfigSpec {
            remote_name: "household".to_string(),
            client_id: "cid".to_string(),
            client_secret: "secret-never-in-argv".to_string(),
            scope: "drive".to_string(),
            root_folder_id: "root-id-1".to_string(),
        };

        let err = create_drive_remote_config_with_manifest_locked(
            &engine, &manifest, &runner, &cfg, &lock, &pw, &spec,
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::AuthenticationRequired { .. }));
        assert!(
            !cfg.exists(),
            "final rclone.conf must never be published when encryption fails"
        );
        assert!(
            !simulated_bak.exists(),
            "rclone backup file must be removed on failure cleanup"
        );
        // No `.rclone.conf.setup.*` leftover should remain in account dir.
        let remaining: Vec<String> = fs::read_dir(&acct_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(remaining, vec!["config.lock".to_string()]);

        let spawned = runner.spawned_specs();
        assert_eq!(spawned.len(), 1);
        let argv: Vec<String> = spawned[0]
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        // Verify rclone operated on the temp file, NOT final rclone.conf.
        let cfg_arg_pos = argv.iter().position(|a| a == "--config").unwrap();
        assert!(
            argv[cfg_arg_pos + 1].contains(".rclone.conf.setup."),
            "encryption must run against exclusive setup temp file, got {}",
            argv[cfg_arg_pos + 1]
        );
        assert!(!argv.iter().any(|a| a.contains("secret-never-in-argv")));
        assert!(format!("{spec:?}").contains("[REDACTED]"));
        assert!(!format!("{spec:?}").contains("secret-never-in-argv"));

        let _ = fs::remove_dir_all(&tmp);
    }
}
