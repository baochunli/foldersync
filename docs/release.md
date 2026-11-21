# Release, Packaging, Signing, and Upgrades

FolderSync packages a self-contained macOS `.app` bundle containing the NSH 0.6.1 desktop application, a verified pinned `rclone` binary (`1.76.0`), the embedded Swift Finder Sync extension (`FolderSyncFinder.appex`), and verbatim third-party license notices.

> **Current packaging boundary (demo UI):** At this commit, `packaging/build-app.sh` packages the **in-memory demo UI** (`apps/sync-ui` `DemoSource`) alongside the verified `Contents/Helpers/rclone` binary and `Contents/PlugIns/FolderSyncFinder.appex`. The packaged desktop host does **not** yet wire `sync-manager` for live Finder `status.json` / `actions/*.json` handoff, and `rclone-adapter` does **not** yet discover or verify the post-signing `Contents/Helpers/rclone` helper inside the bundle (it currently resolves `.tools/rclone/1.76.0/rclone` or `FOLDERSYNC_RCLONE` against the unsigned upstream digest). Manager-backed Finder handoff and bundled-helper discovery land after `connect-t4` and `drive-t4`; all operational owner checks below are **gated on that integration**.
>
> **Verification status:** Ad hoc local packaging (`packaging/build-app.sh`) and deep strict signature verification (`codesign --verify --deep --strict`) are verified locally on macOS `arm64`. Developer ID signing, Apple notarization, stapling, Gatekeeper assessment on a second Mac, and production upgrade/rollback pilots require an Apple Developer account and are owner-only release steps.

## Packaging (`packaging/build-app.sh`)

Run from the repository root after fetching the pinned `rclone` binary:

```bash
scripts/fetch-rclone.sh
packaging/build-app.sh
```

Output is physically validated inside `<repo>/target/` with zero symlinked components before any directory creation, copy, or removal (default `target/package/FolderSync.app`). The script never installs to `/Applications` and never registers with `pluginkit`.

### Bundle Layout

`packaging/build-app.sh` invokes `~/.local/bin/nsh package --target macos --id dev.foldersync.app` on `apps/sync-ui`, then embeds and verifies:

| Path inside `FolderSync.app` | Purpose |
| --- | --- |
| `Contents/MacOS/FolderSync` | NSH 0.6.1 pure-Rust macOS executable (`CFBundleIdentifier = dev.foldersync.app`; currently demo UI until `connect-t4`/`drive-t4` land) |
| `Contents/Helpers/rclone` | Pinned `rclone` `1.76.0` executable copied from `.tools/rclone/1.76.0/rclone` after SHA-256 verification against `packaging/qualification-manifest.toml` |
| `Contents/Resources/rclone-manifest.json` | Sealed metadata recording rclone version, platform, fixed relative path (`Contents/Helpers/rclone`), verified upstream manifest SHA-256, and post-signing bundled SHA-256 |
| `Contents/PlugIns/FolderSyncFinder.appex` | Sandboxed macOS Finder Sync extension (`CFBundleIdentifier = dev.foldersync.app.finder-extension`, `FolderSyncHostBundleIdentifier = dev.foldersync.app`) |
| `Contents/Resources/Notices/` | `NOTICE.txt`, `FOLDERSYNC-LICENSE.txt` (MIT), verbatim `NSH-LICENSE-APACHE-2.0.txt` (NSH `v0.6.1`), verbatim `RCLONE-LICENSE-MIT.txt` (rclone `v1.76.0`), and `PAPER-MONO-OFL.txt` (SIL OFL 1.1). Exact upstream URLs and SHA-256 digests are recorded in [`packaging/notices/SOURCES.md`](../packaging/notices/SOURCES.md). |

> **Bundled rclone digest note:** Before copying, `packaging/build-app.sh` verifies `.tools/rclone/1.76.0/rclone` against the exact upstream `binary_sha256` in `packaging/qualification-manifest.toml`. Signing `Contents/Helpers/rclone` writes a macOS code signature into the Mach-O binary, which changes its byte SHA-256 relative to the upstream archive. Both `upstream_manifest_sha256` and `bundled_sha256` are recorded in `Contents/Resources/rclone-manifest.json` before sealing the outer `.app` bundle. Runtime bundled-helper discovery and verification against the signed helper will be wired in `connect-t4`/`drive-t4`.

## Ad Hoc vs. Developer ID Signing

`packaging/build-app.sh` signs strictly **inner to outer** (never `codesign --deep` during signing, which would overwrite the Finder extension's entitlements):

1. `Contents/Helpers/rclone`
2. `Contents/Resources/rclone-manifest.json` written with the signed helper digest
3. `Contents/PlugIns/FolderSyncFinder.appex` with its entitlements plist
4. Outer `FolderSync.app`

Then the script validates the staged bundle with `codesign --verify --deep --strict --verbose=2` before replacing `target/package/FolderSync.app`.

| Mode | Command | Flags | Appropriate for |
| --- | --- | --- | --- |
| **Ad hoc (default)** | `packaging/build-app.sh` | `--force --sign -` | Local development and packaging verification on the build Mac. Cannot be notarized and will not pass Gatekeeper on another Mac. |
| **Developer ID (owner release)** | `packaging/build-app.sh --sign "Developer ID Application: <Name> (<TEAM_ID>)"` | `--force --sign <identity> --options runtime --timestamp` | Household / external distribution outside the Mac App Store (plan §19). |

### Finder Extension Entitlements

- **Dev / ad hoc / initial Developer ID builds (outside the Mac App Store):** `apps/finder-extension/FinderExtension.entitlements` enables `com.apple.security.app-sandbox` and grants a narrow home-relative temporary file exception (`com.apple.security.temporary-exception.files.home-relative-path.read-write`) scoped strictly to `/Library/Application Support/FolderSync/finder/`. Inside the sandbox, the extension resolves the real user home via `getpwuid(getuid())`, validates every path component from the state root down via `fstatat(..., AT_SYMLINK_NOFOLLOW)` + `openat(..., O_DIRECTORY | O_NOFOLLOW)`, and opens `status.json` with `O_NONBLOCK | O_NOFOLLOW` requiring a regular file.
- **Isolated dev override:** Passing `--dev-shared-dir` to `packaging/build-app.sh` or `apps/finder-extension/build.sh` creates a fresh directory via `mktemp` under `<repo>/target/` and scopes a generated absolute-path temporary exception strictly to that directory (arbitrary user paths are rejected).
- **Future `<TEAM_ID>.*` App Group option:** Once Developer ID signing is qualified by the owner, the Finder handoff directory may optionally migrate to a `<TEAM_ID>.*` macOS App Group container (see `docs/decisions/0002-finder-extension.md`).

## Owner Notarization Steps (Apple Developer Account)

Credentials, Apple IDs, app-specific passwords, and Team IDs are **never** stored in this repository or passed on command lines (which exposes secrets to shell history and process lists).

1. **One-time interactive Keychain profile setup (owner's Mac):**
   ```bash
   xcrun notarytool store-credentials "foldersync-notary"
   ```
   `notarytool` prompts interactively for your Apple ID, Team ID, and app-specific password and stores them directly in your macOS Keychain under the profile name. **Never pass `--password` on the command line.**
2. **Build and sign with Developer ID:**
   ```bash
   packaging/build-app.sh \
     --sign "Developer ID Application: <Your Name> (<TEAM_ID>)"
   ```
3. **Notarize, staple, and assess Gatekeeper:**
   ```bash
   packaging/notarize.sh \
     --app target/package/FolderSync.app \
     --keychain-profile "foldersync-notary"
   ```
   `packaging/notarize.sh` takes only the Keychain profile NAME, verifies `codesign --verify --deep --strict`, refuses ad hoc bundles or bundles without Hardened Runtime, creates `target/package/FolderSync-notarize.zip` with `ditto -c -k --keepParent`, submits via `xcrun notarytool submit --keychain-profile <name> --wait`, staples and validates with `xcrun stapler`, and runs `spctl --assess --type execute --verbose=4`.
4. **Operational verification on a second Mac (gated on `connect-t4` / `drive-t4` host & helper integration):**
   - Copy the notarized `FolderSync.app` to `/Applications` on a clean Mac without developer tools.
   - Verify Gatekeeper opens the app without quarantine bypasses.
   - Once manager wiring and bundled-helper discovery land, verify filesystem access from the installed `.app` and `Contents/Helpers/rclone` to internal folders, protected folders, and an external APFS volume (plan §19), verify installed-app Keychain credentials and notifications, and enable `FolderSync Finder` in System Settings to verify live root badges and context menu actions end to end.

## Credentials and Privacy Boundaries (Plan §19)

- Keep all application state private (`0700` dirs, `0600` files) under `~/Library/Application Support/FolderSync/` on the internal system disk.
- Store the `rclone.conf` encryption passphrase in macOS Keychain and retrieve it only via an audited `--password-command` helper; never log or export credentials or personal file contents.
- Never run `rclone selfupdate` or resolve `rclone` from `PATH`.

## Upgrades and Rollback (Plan §19, §24)

Upgrade the application and bundled `rclone` engine deliberately:

1. **Stop scheduling and drain active jobs:** Pause scheduling and let any active run finish cleanly (or stop it explicitly), then quit FolderSync so the exclusive OS state lock (`manager.lock`) is released.
2. **Create a consistent local state checkpoint:** With all writers stopped, back up `~/Library/Application Support/FolderSync/` (`state.sqlite`, `accounts/`, `mappings/` including opaque `bisync/` workdirs, `runs/`, and `recovery-records/`) together as one unit. Never copy bisync state from another computer.
3. **Verify compatibility:** Confirm SQLite schema migrations (`schema_migrations` is forward-only; opening a newer DB with an older binary fails explicitly rather than downgrading silently) and rclone bisync listing compatibility for the new pinned `rclone` version.
4. **Re-run qualification:** Run `tests/qualification/run-local.sh`, CI, and (for engine or backend changes) the owner Drive qualification suite (`tests/qualification/run-drive.sh`) before distribution.
5. **Pilot before rollout:** Test the new bundle on one disposable mapping first, then one real mapping, before rolling out to other household computers.

### Rollback Procedure

Do not promise unconditional in-place downgrade across schema or rclone engine versions—an older binary may not be able to read a migrated SQLite schema or listings rewritten by a newer `rclone`.

To roll back safely:

1. Pause scheduling and quit FolderSync cleanly so no `rclone` child remains active.
2. Leave local synchronized folders, cloud contents, logs, and recovery copies intact.
3. Either restore the pre-upgrade application binary together with the consistent local state checkpoint taken before upgrading (only if no post-upgrade changes need to be reconciled into those listings), or keep the local/cloud trees intact and re-establish a fresh reviewed generation after inspecting both sides.
4. Do not blindly reactivate a retired third-party sync client (such as Sync.app or Google Drive desktop) against a folder whose state changed while managed by FolderSync.
