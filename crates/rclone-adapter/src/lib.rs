//! Owns rclone version qualification, typed argument construction, process
//! lifetime, output parsing, and outcome classification for FolderSync.

pub mod log_parser;
pub mod outcome;
pub mod process;
pub mod qualification;
pub mod request;

use std::{env, path::PathBuf};

pub use log_parser::{
    BisyncMarker, ConflictSide, LogEvent, LogLevel, StructuredLogEvent, TransferStats,
    parse_log_line, parse_log_stream, strip_ansi_codes,
};
pub use outcome::{
    EXIT_CODE_TABLE_VERSION, Outcome, RcloneExitCode, classify_exit_code, classify_outcome,
};
pub use process::{
    BoundedLineBuffer, CapturedOutput, ChildIdentity, ExitStatusInfo, FakeProcessRunner,
    FakeRunScript, OutputLimits, OutputLine, ProcessError, ProcessRunner, ProcessSpec,
    ProcessStartTime, RealProcessRunner, RunHandle, RunOutput, SpawnOptions, StopHandle,
    StreamKind, read_process_start_time,
};
pub use qualification::{
    EngineIdentity, PlatformPin, QualificationError, QualificationManifest, host_platform_key,
    parse_rclone_version, qualify_binary, qualify_binary_with_runner, qualify_pinned_binary,
    sha256_file,
};
pub use request::{
    BackupDir2, BisyncRequest, BisyncRequestBuilder, InitPreference, PasswordCommand, Path2,
    RequestError, RunMode,
};

/// Exact rclone release pinned in `packaging/qualification-manifest.toml`.
pub const PINNED_RCLONE_VERSION: &str = "1.76.0";

const DEFAULT_BINARY: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../.tools/rclone/1.76.0/rclone"
);

/// Resolves an explicit development override or the workspace's pinned binary
/// to an absolute file path.
pub fn pinned_binary_path() -> Option<PathBuf> {
    let path = env::var_os("FOLDERSYNC_RCLONE")
        .map_or_else(|| PathBuf::from(DEFAULT_BINARY), PathBuf::from);
    path.canonicalize().ok().filter(|path| path.is_file())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn resolves_the_pinned_binary_when_installed() {
        let expected = env::var_os("FOLDERSYNC_RCLONE")
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
