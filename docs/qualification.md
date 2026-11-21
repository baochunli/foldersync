# Qualification

The workspace bootstrap checks cover the non-UI crates, the macOS arm64 desktop
build, and the SHA-256 values for the three pinned rclone archives and
executables. See the [qualification manifest](../packaging/qualification-manifest.toml)
for the exact tool versions and digests, and the [engine profile](engine-profile.md)
for the implemented rclone `1.76.0` argument profile and observed quirks.

## Completed local-to-local qualification (`rclone-adapter`, rclone `1.76.0`)

Verified locally on macOS arm64 (`osx-arm64`) inside fresh temporary directories
with throwaway empty `--config` files, cleared environments, external truth
sets, SHA-256 verification, and second-cycle checks
(`crates/rclone-adapter/tests/local_bisync.rs`):

- Pinned binary SHA-256 + isolated `version` probe qualification, plus
  launch-boundary re-hashing refusing a binary replaced after qualification.
- Parent environment isolation (`RCLONE_FORCE=true` and `RCLONE_RESYNC=true`
  cannot bypass `--max-delete` or trigger resync).
- Initialization with `--resync-mode path1` and `--resync-mode path2`.
- Bidirectional add, edit, and delete propagation with `--backup-dir1` and
  `--backup-dir2` verified by SHA-256.
- Same-file concurrent edit conflict preservation (`--conflict-resolve none`,
  `--conflict-loser num`) including filenames containing ` - ` and benign
  filenames containing diagnostic phrases.
- Percentage deletion guard abort (`--max-delete 10`) preserving all files on
  the peer endpoint, and `--check-access` (`RCLONE_TEST`) failure hold.
- Graceful `SIGINT` interruption during a multi-file batch and recovery on the
  subsequent `--resilient --recover` cycle.
- Known §10 limitation demonstration: an in-place edit preserving both byte
  size and modification timestamp is not selected under `--compare size,modtime`.

## Remaining qualification TODO

- **Local failure matrix (remaining):** missing and wrong volumes (including
  external-drive unplug races at each sync phase), permission and disk-full /
  recovery-full failures, corrupt or missing workdirs and stale lock recovery,
  Unicode/case name collisions, symlink handling, and very small mappings /
  reviewed bulk deletions.
- **Linux (`linux-amd64`) execution:** verify local-to-local integration and
  `/proc/<pid>/stat` process identity on Linux CI (`ubuntu-latest`).
- **Real Google Drive (owner qualification):** actual OAuth client and scopes,
  shared-parent folder IDs, `--drive-skip-gdocs` and `--drive-skip-shortcuts`,
  same-remote `Recovery/...` moves, two separately authenticated accounts,
  encrypted `--config` + Keychain `--password-command` token refresh, rate
  limiting, overlapping multi-client runs, and offline return.
- **Packaged desktop app:** startup, close-to-background, tray/native Quit,
  Command-Q, logout/update interruption, launch at login, permissions,
  notifications, Finder Sync root badges, and second-instance exclusion.
