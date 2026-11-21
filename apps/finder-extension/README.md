# FolderSync Finder Sync Extension (macOS)

Small Swift `FIFinderSync` extension that shows sync status badges on mapped-folder **roots** only (`synced`, `syncing`, `attention`, `paused`, `disk_missing`, and `stale`/unavailable) plus a contextual menu (`Sync now`, `Pause`, `Open FolderSync`, `Show last run`).

The extension owns no sync state, holds no OAuth credentials, and never launches `rclone`. It reads an atomically written JSON status snapshot (`status.json`) and writes narrow action requests (`actions/action-<id>.json`), refusing symlinks and unsafe identifiers.

See [docs/decisions/0002-finder-extension.md](../../docs/decisions/0002-finder-extension.md) for the feasibility findings, snapshot/action contracts, entitlement tradeoffs, and NSH packaging guidance.

## Production Bundle IDs and Shared Handoff Location

- **Host bundle ID:** `dev.foldersync.app`
- **Extension bundle ID:** `dev.foldersync.app.finder-extension`
- **Darwin notifications (ADR 0002):**
  - Host → Extension: `dev.foldersync.finder.snapshot-updated`
  - Extension → Host: `dev.foldersync.finder.action-requested`
- **Shared handoff location (`<state dir>/finder`):**
  - In production dev / ad hoc / Developer ID builds outside the Mac App Store, `FinderExtension.entitlements` grants a narrow home-relative temporary file exception (`com.apple.security.temporary-exception.files.home-relative-path.read-write` for `/Library/Application Support/FolderSync/finder/`), resolved against the real user home directory (`getpwuid(getuid())`) rather than the App Sandbox container path. Every component from the state root down is validated via `fstatat(..., AT_SYMLINK_NOFOLLOW)` + `openat(..., O_DIRECTORY | O_NOFOLLOW)`, and `status.json` is opened with `O_NONBLOCK | O_NOFOLLOW` requiring a regular file (`S_IFREG`).
  - Passing `--dev-shared-dir` creates a fresh disposable directory via `mktemp` under `<repo>/target/` with a scoped absolute-path temporary exception for isolated dev testing (arbitrary paths are rejected).

## Build and Unit Tests (Xcode Command Line Tools only)

No full `Xcode.app` or Developer ID certificate is required for local development:

```bash
# Run Swift unit tests and build production FolderSyncFinder.appex into target/
./apps/finder-extension/build.sh --test
```

To embed the extension alongside the NSH desktop app and pinned `rclone` binary into `target/package/FolderSync.app`, use `packaging/build-app.sh` (see [docs/release.md](../../docs/release.md)).

## Standalone Mock-Host Verification (Target Directory Only)

To exercise the extension in isolation without `nsh package`, pass `--with-mock-host` (which builds under `target/` and creates a fresh disposable handoff dir under `target/` via `mktemp`, never touching real application state):

```bash
./apps/finder-extension/build.sh \
  --test \
  --with-mock-host \
  --out target/finder-mock \
  --bundle-id dev.foldersync.spike.host
```

Cleanup after any local `pluginkit` verification (never run `killall Finder`):

```bash
pluginkit -e ignore -i dev.foldersync.spike.host.finder-extension
pluginkit -r "target/finder-mock/FolderSyncHost.app/Contents/PlugIns/FolderSyncFinder.appex"
rm -rf target/finder-mock
```
