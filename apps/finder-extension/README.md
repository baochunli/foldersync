# FolderSync Finder Sync Extension (macOS)

Small Swift `FIFinderSync` extension that shows sync status badges on mapped-folder **roots** only (`synced`, `syncing`, `attention`, `paused`, `disk_missing`, and `stale`/unavailable) plus a contextual menu (`Sync now`, `Pause`, `Open FolderSync`, `Show last run`).

The extension owns no sync state, holds no OAuth credentials, and never launches `rclone`. It reads an atomically written JSON status snapshot and sends narrow action requests back to the host app.

See [docs/decisions/0002-finder-extension.md](../../docs/decisions/0002-finder-extension.md) for the feasibility findings, snapshot/action contracts, entitlement tradeoffs, and NSH packaging guidance.

## Build (Xcode Command Line Tools only)

No full `Xcode.app` or Developer ID certificate is required for local development:

```bash
chmod +x apps/finder-extension/build.sh
./apps/finder-extension/build.sh --test --out "$(mktemp -d)"
```

To verify against a throwaway temp directory without touching `~/Library/Application Support/FolderSync`:

```bash
WORK="$(mktemp -d)"
./apps/finder-extension/build.sh \
  --test \
  --out "$WORK/out" \
  --shared-dir "$WORK/shared" \
  --bundle-id dev.foldersync.spike
```

## Local verification and cleanup

1. Write a valid v1 `status.json` to `$WORK/shared/status.json` referencing temp mapping folders.
2. Launch `$WORK/out/FolderSyncHost.app` and register/enable the extension:
   ```bash
   pluginkit -a "$WORK/out/FolderSyncHost.app/Contents/PlugIns/FolderSyncFinder.appex"
   pluginkit -e use -i dev.foldersync.spike.finder-extension
   ```
3. Open the parent of your temp mapping folders in Finder.
4. Always clean up afterwards (never run `killall Finder`):
   ```bash
   pluginkit -e ignore -i dev.foldersync.spike.finder-extension
   pluginkit -r "$WORK/out/FolderSyncHost.app/Contents/PlugIns/FolderSyncFinder.appex"
   rm -rf "$WORK"
   ```

## Embedding into `nsh package` output

`nsh package --target macos` outputs an ad hoc signed `.app` bundle. To embed this extension later:
1. Copy `FolderSyncFinder.appex` into `<FolderSync.app>/Contents/PlugIns/FolderSyncFinder.appex`.
2. Match `CFBundleIdentifier` to `<host-bundle-id>.finder-extension` and `FolderSyncHostBundleIdentifier` to `<host-bundle-id>`.
3. Re-sign inner to outer: `.appex` (with entitlements) first, then the outer `.app` bundle.
