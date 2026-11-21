//! Owns rclone version qualification, typed argument construction, process
//! lifetime, output parsing, and outcome classification for FolderSync.

pub mod auth;
pub mod config;
pub mod log_parser;
pub mod lsjson;
pub mod outcome;
pub mod process;
pub mod qualification;
pub mod request;

use std::{
    env,
    path::{Path, PathBuf},
};

pub use auth::{
    AuthUrlCallback, DriveAuthError, DriveAuthOutcome, DriveAuthRequest, extract_auth_url,
    run_drive_auth, run_drive_auth_locked,
};
pub use config::{
    AccountConfigLock, ConfigError, DriveRemoteConfigSpec, ImportRemoteSpec, ImportedRemoteInfo,
    ParsedIniSection, RCLONE_ENCRYPT_HEADER, RemoteConfigSummary, SETUP_TEMP_PREFIX,
    check_config_recovery_state, cleanup_account_setup_temp_files, create_drive_remote_config,
    create_drive_remote_config_locked, encrypted_remote_has_token,
    enforce_private_config_permissions, extract_single_remote_section, find_rclone_config_backups,
    import_single_remote_into_encrypted_config, import_single_remote_into_encrypted_config_locked,
    init_encrypted_config, init_encrypted_config_locked, is_encrypted_config_content,
    is_global_default_rclone_config, list_encrypted_config_remotes, verify_encrypted_config,
};
pub use log_parser::{
    BisyncMarker, ConflictSide, LogEvent, LogLevel, StructuredLogEvent, TransferStats,
    parse_log_line, parse_log_stream, strip_ansi_codes,
};
pub use lsjson::{
    DuplicateDirFinding, LsJsonEntry, LsJsonError, LsJsonRequest, ResolvedComponent,
    ResolvedRemotePath, find_duplicate_dir_names, list_dirs_with_runner, parse_lsjson_entries,
    resolve_folder_path_with_runner, verify_expected_folder_identity_with_runner,
};
pub use outcome::{
    EXIT_CODE_TABLE_VERSION, Outcome, RcloneExitCode, classify_exit_code, classify_outcome,
    classify_run_outcome,
};
pub use process::{
    BoundedLineBuffer, CapturedOutput, ChildIdentity, ExitStatusInfo, FakeProcessRunner,
    FakeRunScript, OutputLimits, OutputLine, ProcessError, ProcessRunner, ProcessSpec,
    ProcessStartTime, RealProcessRunner, RunHandle, RunOutput, SpawnOptions, StopHandle,
    StreamKind, minimal_isolated_env, read_process_start_time,
};
pub use qualification::{
    EngineIdentity, PlatformPin, QualificationError, QualificationManifest, host_platform_key,
    parse_rclone_version, qualify_binary, qualify_pinned_binary, sha256_file,
    verify_binary_integrity,
};
pub use request::{
    BackupDir2, BisyncRequest, BisyncRequestBuilder, InitPreference, LaunchError, PasswordCommand,
    Path2, RequestError, RunMode,
};

/// Exact rclone release pinned in `packaging/qualification-manifest.toml`.
pub const PINNED_RCLONE_VERSION: &str = "1.76.0";

const DEFAULT_BINARY: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../.tools/rclone/1.76.0/rclone"
);

/// Resolves the workspace's pinned binary to an absolute file path. Debug
/// builds accept `FOLDERSYNC_RCLONE` as a development override.
/// Use [`qualify_pinned_binary`] for bundle-aware startup qualification.
pub fn pinned_binary_path() -> Option<PathBuf> {
    workspace_binary_path(Path::new(DEFAULT_BINARY), cfg!(debug_assertions), || {
        env::var_os("FOLDERSYNC_RCLONE").map(PathBuf::from)
    })
}

fn workspace_binary_path(
    default: &Path,
    debug_build: bool,
    override_path: impl FnOnce() -> Option<PathBuf>,
) -> Option<PathBuf> {
    let override_path = debug_build.then(override_path).flatten();
    let path = override_path.as_deref().unwrap_or(default);
    path.canonicalize().ok().filter(|path| path.is_file())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn workspace_binary_override_is_only_used_in_debug_builds() {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"));
        let default = workspace.join("Cargo.toml");
        let override_path = workspace.join("src/lib.rs");
        assert_eq!(
            workspace_binary_path(&default, true, || Some(override_path.clone())),
            Some(override_path.canonicalize().unwrap())
        );
        assert_eq!(
            workspace_binary_path(&default, false, || panic!("read release override")),
            Some(default.canonicalize().unwrap())
        );
    }

    #[test]
    fn resolves_the_pinned_binary_when_installed() {
        let expected = if cfg!(debug_assertions) {
            env::var_os("FOLDERSYNC_RCLONE")
        } else {
            None
        }
        .map_or_else(|| PathBuf::from(DEFAULT_BINARY), PathBuf::from);
        if !expected.is_file() {
            eprintln!("Skipping: pinned rclone is absent; run scripts/fetch-rclone.sh");
            return;
        }

        let binary = pinned_binary_path().expect("pinned binary should resolve");
        assert_eq!(binary, expected.canonicalize().unwrap());
        assert!(binary.is_absolute());
        assert_ne!(binary.metadata().unwrap().permissions().mode() & 0o111, 0);
    }
}
