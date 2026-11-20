//! Owns rclone version qualification, typed argument construction, process
//! lifetime, and output parsing for FolderSync.

use std::{env, path::PathBuf};

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
