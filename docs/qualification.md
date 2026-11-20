# Qualification

The bootstrap checks cover the non-UI crates, the macOS arm64 desktop build,
and the SHA-256 values for the three pinned rclone archives and executables.
See the [qualification manifest](../packaging/qualification-manifest.toml)
for the exact tool versions and digests.

Use fresh local trees and a throwaway rclone configuration for local
integration fixtures. Keep independent copies of the expected files and
compare contents, conflicts, and recoverable versions after successive runs.

**TODO** qualify initialization, edits and deletes, interrupted runs, missing
and wrong volumes, permission and space failures, corrupt workdirs, and name
collisions. Qualify real Drive with separate accounts, overlapping clients,
and offline return. Verify the packaged desktop app on the supported systems.
