# ADR 0002: macOS Finder Sync Extension Feasibility & Architecture

- **Date:** 2026-11-20
- **Status:** Accepted (feasibility spike verified; ready for Group 3/4 manager and NSH packaging integration)
- **Scope:** v1 macOS Finder integration (mapped-folder **root** badges + narrow context menu)

## Context

FolderSync v1 includes Finder status badges on **mapped-folder roots only** (`synced`, `syncing`, `attention`, `paused`, `disk_missing`, plus `stale`/unavailable) and a narrow contextual menu (`Sync now`, `Pause`, `Open FolderSync`, `Show last run`). Per plan §17 and the v1 scope rulings:

- No per-file status index in v1.
- The extension owns no sync state, holds no OAuth credentials, and never launches `rclone`.
- Development machines may have only **Xcode Command Line Tools** (no full `Xcode.app`) and **no code-signing identities** (ad hoc signing only).

This spike tests what works end-to-end with CLT-only tooling and ad hoc signing on macOS, identifies exact boundaries requiring a Developer ID or full Xcode, and defines the v1 snapshot/action contracts.

## Test Environment

- **OS:** macOS 27.2.1 (`arm64`)
- **Toolchain:** Xcode Command Line Tools only (`swiftc` 6.4.2, `codesign`, `plutil`, `pluginkit`, `xcrun`); no `Xcode.app` / `xcodebuild`
- **Signing:** Ad hoc (`codesign --force --sign -`) with entitlements plist; no signing certificates installed and no Keychain access

## Answers to Feasibility Questions (With Evidence)

### 1. Can a Swift Finder Sync extension be built with CLT only, ad hoc signed, embedded, and registered locally?

**Yes.** Verified end-to-end via `apps/finder-extension/build.sh`.

- **Compilation (no Xcode):** `swiftc` compiles a `FIFinderSync` subclass directly into an app extension Mach-O binary using `-parse-as-library -application-extension -Xlinker -e -Xlinker _NSExtensionMain` linked against `Cocoa`, `FinderSync`, and `Foundation`.
- **No Asset Catalog required:** Badge images are rendered programmatically with macOS SF Symbols (`NSImage(systemSymbolName:accessibilityDescription:)`) and a CoreGraphics fallback, avoiding `actool` (which requires full `Xcode.app`).
- **Bundle assembly & signing:** Embedding `Contents/PlugIns/FolderSyncFinder.appex` inside a throwaway `FolderSyncHost.app` and signing inner-to-outer passes `codesign --verify --deep --strict`:
  ```bash
  codesign --force --sign - --entitlements FinderExtension.entitlements FolderSyncHost.app/Contents/PlugIns/FolderSyncFinder.appex
  codesign --force --sign - FolderSyncHost.app
  codesign --verify --deep --strict FolderSyncHost.app
  ```
- **Local registration & execution:** `pluginkit -a <appex>` and `pluginkit -e use -i <bundle-id>` succeed. When Finder opens a watched directory's parent, `pkd`/RunningBoard launches the ad hoc signed `.appex` (`amfid` logs informational `Code=-423` for ad hoc signing, and AppleSystemPolicy permits local execution).

### 2. Where can a sandboxed extension read the status snapshot with ad hoc signing?

Finder Sync extensions must run inside the macOS App Sandbox (`com.apple.security.app-sandbox = true`). Two shared-location mechanisms were tested live inside the running extension:

| Candidate | Entitlement | Result (ad hoc signed, macOS 27.2.1) | Observed Evidence |
| --- | --- | --- | --- |
| **App Group container (`group.*`)** | `com.apple.security.application-groups = ["group.dev.foldersync.probe"]` | **Failed** | `containerURL(forSecurityApplicationGroupIdentifier:)` returned `~/Library/Group Containers/group.dev.foldersync.probe`, but file access failed with `EPERM` (`sandboxd`: `kTCCServiceSystemPolicyAppDataDetailed denied by TCC`). Without a Team ID or provisioning profile, macOS App Data Protection blocks access to `group.*` containers. |
| **Temporary file exception (home-relative / absolute)** | `com.apple.security.temporary-exception.files.home-relative-path.read-write` (production) or `absolute-path.read-write` (temp-dir dev) | **Succeeded** | Extension read `status.json` (`len=212`), read a home-relative JSON file (`len=12`), and atomically wrote action JSON files inside the granted directory with zero TCC denials. |

**Decision:** Use `com.apple.security.temporary-exception.files.home-relative-path.read-write` scoped narrowly to `/Library/Application Support/FolderSync/finder/` (and `--shared-dir` with `absolute-path.read-write` in `build.sh` for isolated temp-directory development). Temporary file exceptions are permitted for Developer ID apps distributed outside the Mac App Store (plan §19 explicitly excludes Mac App Store sandboxing for v1). Once a Developer ID Team ID is available, migrating to a `<TEAM_ID>.dev.foldersync.shared` macOS App Group is a small entitlement and path update.

### 3. Can root-level badges be driven live by an atomically written snapshot, with staleness handling?

**Yes.** Verified live with Finder and unified logging (`dev.foldersync.app.finder-extension`):

1. Opening the parent directory (`<tmp>/watched`) caused Finder to invoke `requestBadgeIdentifier(for:)` for all four existing mapped roots (`Archive`, `Docs`, `Family`, `Research`) and set their distinct badges (`paused`, `attention`, `syncing`, `synced`).
2. Opening `<tmp>/watched/Research` triggered `beginObservingDirectory(at:)` and requested badges for child files (`notes.txt`); the extension returned `""` (empty string) for all non-root descendants, enforcing the v1 roots-only rule.
3. Atomically replacing `status.json` (`Research`: `synced` → `attention`) and posting Darwin notification `dev.foldersync.finder.snapshot-updated` updated all observed root badges immediately (`Updated badge root=.../Research badge=attention`).
4. Replacing `generated_at` with an expired timestamp (`2020-01-01T00:00:00Z`) and notifying switched all four observed roots to `stale`.
5. `NSRunningApplication.runningApplications(withBundleIdentifier:)` succeeded inside the sandboxed extension (`count=1` when running), enabling host-liveness staleness detection alongside timestamp expiry.

**Implementation lessons learned during verification:**
- **Canonical path normalization is mandatory:** Finder passes resolved paths (`/private/var/...`), whereas callers often write symlinked paths (`/var/...`). Both snapshot roots and Finder callback URLs must be canonicalized via `standardizedFileURL.resolvingSymlinksInPath()`.
- **Do not pre-badge unrequested URLs:** Calling `setBadgeIdentifier(_:for:)` during `init()` before Finder calls `requestBadgeIdentifier(for:)` suppresses Finder's initial badge callbacks (documented in `FinderSync.h`). Only push updates to URLs previously seen in `requestBadgeIdentifier(for:)`.
- **Do not probe arbitrary user folders inside the sandbox:** Pass snapshot mapping roots directly to `FIFinderSyncController.default().directoryURLs` rather than gating them on `FileManager.fileExists(atPath:)` inside the extension sandbox.

### 4. How should narrow actions be sent back to the host app?

Two sandbox-compatible mechanisms were evaluated empirically:

| Candidate | Result (ad hoc, temp bundle) | Pros & Cons |
| --- | --- | --- |
| **Command file in shared directory + Darwin notification** (`CFNotificationCenterGetDarwinNotifyCenter`) | **Succeeded (<1 ms latency)** | Verified live: extension atomically wrote JSON into `<shared>/actions/` and posted `dev.foldersync.finder.action-requested`; host app received the notification and read the payload in 1 ms. Does not steal window focus on background actions (`Sync now`, `Pause`), works identically in temp dev bundles and installed bundles, and carries zero credentials. |
| **Custom URL scheme** (`NSWorkspace.shared.open(url)`) | **Failed in temp-dir test** | `NSWorkspace.shared.urlForApplication(toOpen:)` returned `nil` and `open(_:configuration:)` failed with `NSCocoaErrorDomain code=256` for an ad hoc host app running from `$TMPDIR` (LaunchServices does not register URL schemes reliably from temp directories). Also brings the host app to the foreground when background actions only need to trigger a manager command. |

**Decision:** Use **atomic action JSON files + Darwin notification** (`dev.foldersync.finder.action-requested`) for all four context menu actions (`sync_now`, `pause`, `open_app`, `show_last_run`). For `Open FolderSync` and `Show last run`, if the host app is not running, the extension also attempts `NSWorkspace.shared.openApplication(at:)` against its enclosing `.app` bundle (unverified interactively; see below).

## Status Snapshot & Action Contracts (v1)

Shared directory (production default): `~/Library/Application Support/FolderSync/finder/` (overridable via `build.sh --shared-dir <tmp>` for development).

### Snapshot File: `status.json` (Host → Extension)

Written atomically by the Rust manager (`status.json.tmp` → rename to `status.json`), followed by posting Darwin notification `dev.foldersync.finder.snapshot-updated`:

```json
{
  "version": 1,
  "generated_at": "2026-11-20T18:45:00Z",
  "stale_after_seconds": 300,
  "mappings": [
    {
      "id": "research",
      "root_path": "/Volumes/WorkSSD/Research",
      "state": "synced",
      "label": "Synced · Last completed 14:32",
      "last_run_summary": "0 conflicts · 12 files checked"
    }
  ]
}
```

- `version`: Must be `1`; unknown versions are rejected and treated as unavailable/stale.
- `generated_at`: RFC 3339 / ISO 8601 UTC timestamp.
- `stale_after_seconds`: Optional integer (default `300` seconds / 5 minutes).
- `mappings[].state`: One of `synced`, `syncing`, `attention`, `paused`, `disk_missing`.
- **Staleness rule:** If `status.json` is missing/invalid, older than `stale_after_seconds`, or the host app bundle ID is not running, all known mapping roots display the `stale` badge (`FolderSync unavailable`) and the context menu shows plain-text stale status.
- **Accessibility:** Every badge registers a distinct symbol shape, color, and text label; the context menu always begins with a disabled plain-text status line (`Status: ...`).

### Action Files: `actions/action-<uuid>.json` (Extension → Host)

Written atomically by the extension, followed by posting Darwin notification `dev.foldersync.finder.action-requested`. The host app drains, validates, executes via the manager command channel, and deletes each file:

```json
{
  "version": 1,
  "id": "550e8400-e29b-41d4-a716-446655440000",
  "requested_at": "2026-11-20T18:46:12Z",
  "action": "sync_now",
  "mapping_id": "research"
}
```

- `action`: Strictly one of `sync_now`, `pause`, `open_app`, `show_last_run`.
- `mapping_id`: Mapping identifier (nullable for `open_app`). No paths, rclone flags, or credentials ever cross this boundary.

## What Requires Full Xcode vs. Developer ID / Provisioning Profile

| Capability | CLT + Ad Hoc (`codesign -s -`) | CLT + Developer ID (Apple Developer Account) | Full `Xcode.app` |
| --- | --- | --- | --- |
| Compile Swift `FIFinderSync` `.appex` | Works | Works | Optional |
| Badge rendering (SF Symbols / CoreGraphics) | Works | Works | Optional (only needed for `.xcassets` / `actool`) |
| Standalone Swift unit tests (`SnapshotModelTests`) | Works | Works | Optional (only needed for `XCTest` / `xcodebuild test`) |
| Local dev registration (`pluginkit -a` / `-e use`) | Works | Works | Not required |
| Shared snapshot via temporary file exception | Works | Works (outside Mac App Store) | Not required |
| `<TEAM_ID>.*` macOS App Group (no profile) | Blocked by TCC | Works (Team ID bound to cert) | Not required |
| `group.*` App Group or Mac App Store build | Blocked by TCC | Requires provisioning profile | Simplifies profile management |
| Multi-Mac distribution without Gatekeeper blocks | **Blocked** (ad hoc fails Gatekeeper) | Works (`codesign --options runtime` + `xcrun notarytool` + `xcrun stapler`) | Not required |

**Summary:** Full `Xcode.app` is **not required** for either local development or Developer ID release builds of the Finder extension. A **Developer ID Application certificate** (and Apple notarization via CLT `xcrun notarytool`) **is required** before distributing the app + embedded extension to other household Macs.

## Recommendation for v1 Packaging (NSH Integration)

Keep the extension as a small CLT-built Swift target under `apps/finder-extension/` and integrate it as a post-packaging step on `nsh package --target macos`:

1. Build the Rust desktop app via `nsh package --target macos --id dev.foldersync.app` to produce `FolderSync.app`.
2. Build `FolderSyncFinder.appex` via `apps/finder-extension/build.sh` with `--bundle-id dev.foldersync.app` (or extract the `.appex` build step into a packaging script).
3. Embed `FolderSyncFinder.appex` at `FolderSync.app/Contents/PlugIns/FolderSyncFinder.appex` (and embed the pinned `rclone` binary per plan §19).
4. Sign strictly **inner to outer**:
   1. Embedded `rclone` binary
   2. `Contents/PlugIns/FolderSyncFinder.appex` (with `FinderExtension.entitlements`, plus `--options runtime` for Developer ID)
   3. Outer `FolderSync.app` (with `--options runtime` for Developer ID; never `--deep` without explicit inner entitlements)
5. For household releases: notarize with `xcrun notarytool`, staple with `xcrun stapler`, and verify with `codesign --verify --deep --strict` and `spctl --assess`.
6. Inside the host app UI, expose `FIFinderSyncController.isExtensionEnabled` and a button calling `FIFinderSyncController.showExtensionManagementInterface()` so household users can enable the extension in System Settings without Terminal or `pluginkit`.

## What Was NOT Verified in This Spike

- **Visual appearance in Finder & interactive context menu:** `screencapture -x` failed with `could not create image from display` in this non-interactive terminal environment (no Screen Recording permission). Badge delivery and updates were verified via Finder callbacks and unified logs (`requestBadgeIdentifier(for:)` and `setBadgeIdentifier(_:for:)`), not visually on screen. Interactive right-click menu rendering and clicking menu items in Finder were not automated.
- **Host relaunch on closed app:** The `NSWorkspace.shared.openApplication(at:)` fallback for `Open FolderSync` / `Show last run` when the host app is stopped was not exercised interactively.
- **NSH `.app` bundle embedding:** `apps/sync-ui` is being built in another lane; embedding into actual `nsh package --target macos` output remains to be verified during Group 4 packaging.
- **Developer ID signing, notarization, and `<TEAM_ID>.*` App Groups:** No signing identities exist on this machine and the Keychain was not touched. Gatekeeper behavior on a second Mac and notarized release packaging remain owner tasks (plan §19).
