# FolderSync Finder Sync Extension (macOS)

Small Swift `FIFinderSync` extension that shows sync status badges on mapped-folder **roots** only (`synced`, `syncing`, `attention`, `paused`, `disk_missing`, and `stale`/unavailable) plus a contextual menu (`Sync now`, `Pause`, `Open FolderSync`, `Show last run`).

The extension owns no sync state, holds no OAuth credentials, and never launches `rclone`. It reads an atomically written JSON status snapshot and sends narrow action requests back to the host app.

> **Note:** The bundled `FolderSyncHost.app` built by `build.sh` is a **throwaway mock host** (`dev.foldersync.spike.host`) for this feasibility spike. It only validates incoming action JSON files, logs them, and deletes them; it does not execute sync runs. Both the extension and the mock host default to the same disposable shared directory (`<out>/spike-shared`) and never touch any real application state directory.

See [docs/decisions/0002-finder-extension.md](../../docs/decisions/0002-finder-extension.md) for the feasibility findings, snapshot/action contracts, entitlement tradeoffs, and NSH packaging guidance.

## Build and Unit Tests (Xcode Command Line Tools only)

No full `Xcode.app` or Developer ID certificate is required for local development:

```bash
chmod +x apps/finder-extension/build.sh
WORK="$(mktemp -d)"
./apps/finder-extension/build.sh \
  --test \
  --out "$WORK/out" \
  --shared-dir "$WORK/shared" \
  --bundle-id dev.foldersync.spike.host
```

## Local Verification (Temp Directory Only) and Cleanup

1. Create throwaway mapping folders and write a v1 `status.json` atomically:
   ```bash
   mkdir -p "$WORK/watched/Research" "$WORK/watched/Family" "$WORK/watched/Docs" "$WORK/watched/Archive"
   echo "sample" > "$WORK/watched/Research/notes.txt"
   NOW_ISO="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
   cat > "$WORK/shared/status.json.tmp" <<EOF
   {
     "version": 1,
     "generated_at": "${NOW_ISO}",
     "stale_after_seconds": 300,
     "mappings": [
       {"id": "research", "root_path": "${WORK}/watched/Research", "state": "synced", "label": "Synced · Just now"},
       {"id": "family", "root_path": "${WORK}/watched/Family", "state": "syncing", "label": "Syncing…"},
       {"id": "docs", "root_path": "${WORK}/watched/Docs", "state": "attention", "label": "Needs attention"},
       {"id": "archive", "root_path": "${WORK}/watched/Archive", "state": "paused", "label": "Paused"}
     ]
   }
   EOF
   mv "$WORK/shared/status.json.tmp" "$WORK/shared/status.json"
   ```
2. Launch the throwaway mock host app, then register and enable the extension:
   ```bash
   FOLDERSYNC_HOST_LOG="$WORK/host.log" \
     "$WORK/out/FolderSyncHost.app/Contents/MacOS/FolderSyncHost" &
   HOST_PID=$!

   pluginkit -a "$WORK/out/FolderSyncHost.app/Contents/PlugIns/FolderSyncFinder.appex"
   pluginkit -e use -i dev.foldersync.spike.host.finder-extension
   open "$WORK/watched"
   ```
3. Push an atomic status update and notify the extension via Darwin notification:
   ```bash
   NOW_ISO2="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
   sed "s/\"synced\"/\"attention\"/; s/${NOW_ISO}/${NOW_ISO2}/" \
     "$WORK/shared/status.json" > "$WORK/shared/status.json.tmp"
   mv "$WORK/shared/status.json.tmp" "$WORK/shared/status.json"
   "$WORK/out/FolderSyncHost.app/Contents/MacOS/FolderSyncHost" --notify-snapshot
   ```
4. Always stop the mock host (`dev.foldersync.spike.host` only) and clean up afterwards (never run `killall Finder`):
   ```bash
   kill "$HOST_PID" 2>/dev/null || osascript -e 'tell application id "dev.foldersync.spike.host" to quit' 2>/dev/null || true
   pluginkit -e ignore -i dev.foldersync.spike.host.finder-extension
   pluginkit -r "$WORK/out/FolderSyncHost.app/Contents/PlugIns/FolderSyncFinder.appex"
   rm -rf "$WORK"
   ```

## Embedding into `nsh package` Output

`nsh package --target macos` outputs an ad hoc signed `.app` bundle. To embed this extension in v1 packaging:
1. Copy `FolderSyncFinder.appex` into `<FolderSync.app>/Contents/PlugIns/FolderSyncFinder.appex`.
2. Match `CFBundleIdentifier` to `<host-bundle-id>.finder-extension` and `FolderSyncHostBundleIdentifier` to `<host-bundle-id>`.
3. Sign inner to outer: embedded binaries and `.appex` (with entitlements) first, then the outer `.app` bundle.
