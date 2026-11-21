# ADR 0002: macOS Finder Sync Extension Feasibility & Architecture

- **Date:** 2026-11-20
- **Status:** Accepted (feasibility spike verified; ready for Group 3/4 manager and NSH packaging integration)
- **Scope:** v1 macOS Finder integration (mapped-folder **root** badges + narrow context menu)

## Context

FolderSync v1 includes Finder status badges on **mapped-folder roots only** (`synced`, `syncing`, `attention`, `paused`, `disk_missing`, plus `stale`/unavailable) and a narrow contextual menu (`Sync now`, `Pause`, `Open FolderSync`, `Show last run`). Per plan §17 and the v1 scope rulings:

- No per-file status index in v1.
- The extension owns no sync state, holds no OAuth credentials, and never launches `rclone`.
- Development machines may have only **Xcode Command Line Tools** (no full `Xcode.app`) and **no code-signing identities** (ad hoc signing only).

This spike tests what works end-to-end with CLT-only tooling and ad hoc signing on macOS, records the exact probe commands and observed results, identifies boundaries requiring a Developer ID or full Xcode, and defines the v1 snapshot/action contracts.

## Test Environment

- **OS:** macOS 27.2.1 (`arm64`)
- **Toolchain:** Xcode Command Line Tools only (`swiftc` 6.4.2, `codesign`, `plutil`, `pluginkit`, `xcrun`); no `Xcode.app` / `xcodebuild`
- **Signing:** Ad hoc (`codesign --force --sign -`) with entitlements plist; no signing certificates installed and no Keychain access

## Answers to Feasibility Questions (With Empirical Evidence)

### 1. Can a Swift Finder Sync extension be built with CLT only, ad hoc signed, embedded, and registered locally?

**Yes.** Verified end-to-end via `apps/finder-extension/build.sh`.

- **Compilation (no Xcode):**
  ```bash
  swiftc -parse-as-library -application-extension \
    -target arm64-apple-macos14.0 -sdk "$(xcrun --show-sdk-path)" \
    -framework Cocoa -framework FinderSync -framework Foundation \
    -Xlinker -e -Xlinker _NSExtensionMain \
    -o FolderSyncHost.app/Contents/PlugIns/FolderSyncFinder.appex/Contents/MacOS/FolderSyncFinder \
    Sources/SnapshotModel.swift Sources/BadgeRenderer.swift Sources/FinderSyncExtension.swift
  ```
- **No Asset Catalog required:** Badge images are rendered programmatically with macOS SF Symbols (`NSImage(systemSymbolName:accessibilityDescription:)`) and a CoreGraphics fallback, avoiding `actool` (which requires full `Xcode.app`).
- **Bundle assembly & inner-to-outer ad hoc signing:**
  ```bash
  codesign --force --sign - --entitlements FinderExtension.generated.entitlements \
    FolderSyncHost.app/Contents/PlugIns/FolderSyncFinder.appex
  codesign --force --sign - FolderSyncHost.app
  codesign --verify --deep --strict FolderSyncHost.app
  ```
- **Local registration & execution:** `pluginkit -a <appex>` and `pluginkit -e use -i <bundle-id>` succeed. When Finder opens a watched directory's parent, `pkd`/RunningBoard launches the ad hoc signed `.appex` (`amfid` logs informational `Code=-423` for ad hoc signing, and AppleSystemPolicy permits local execution).

### 2. Where can a sandboxed extension read the status snapshot with ad hoc signing?

Finder Sync extensions must run inside the macOS App Sandbox (`com.apple.security.app-sandbox = true`). Two entitlement candidates were tested live inside an ad hoc signed Finder Sync probe (`ProbeHost` / `ProbeExt`, bundle ID `dev.foldersync.probe.ext`) registered via `pluginkit`:

**Exact probe commands (run inside a `mktemp -d` directory, cleaned up via `pluginkit -e ignore` / `pluginkit -r`):**

```bash
# Variant A (App Group): ent-a.plist contains com.apple.security.app-sandbox=true
# and com.apple.security.application-groups=["group.dev.foldersync.probe"]
codesign --force --sign - --entitlements "$PROBE_DIR/ent-a.plist" "$APPEX_PATH"
codesign --force --sign - "$APP_PATH"
pluginkit -a "$APPEX_PATH" && pluginkit -e use -i dev.foldersync.probe.ext
open "$WATCH_PARENT" && open "$ROOT_A"
/usr/bin/log show --start "$START_TIME" --info --style compact \
  --predicate 'subsystem == "dev.foldersync.probe" OR (process == "sandboxd" AND eventMessage CONTAINS[c] "ProbeExt")'

# Variant B (Temporary file exceptions): ent-b.plist contains com.apple.security.app-sandbox=true,
# com.apple.security.temporary-exception.files.absolute-path.read-only=["$SNAP_DIR/", "/private$SNAP_DIR/"],
# and com.apple.security.temporary-exception.files.home-relative-path.read-only=["/$REL_FROM_HOME/"]
# (followed by a second probe with absolute-path.read-write for command file writes)
codesign --force --sign - --entitlements "$PROBE_DIR/ent-b.plist" "$APPEX_PATH"
codesign --force --sign - "$APP_PATH"
pluginkit -a "$APPEX_PATH" && pluginkit -e use -i dev.foldersync.probe.ext
open "$WATCH_PARENT" && open "$ROOT_A"
```

**Observed results (macOS 27.2.1, ad hoc signed):**

| Candidate | Entitlement | Observed Result | Unified Log Evidence |
| --- | --- | --- | --- |
| **App Group container (`group.*`)** | `com.apple.security.application-groups = ["group.dev.foldersync.probe"]` | **Failed** | `containerURL(forSecurityApplicationGroupIdentifier:)` returned `~/Library/Group Containers/group.dev.foldersync.probe`, but writing `probe.txt` failed with permission error (`sandboxd`: `kTCCServiceSystemPolicyAppDataDetailed denied by TCC for ProbeExt`). Without a Team ID or provisioning profile, macOS App Data Protection blocks access to `group.*` containers. |
| **Temporary file exception (read-only / read-write)** | `com.apple.security.temporary-exception.files.absolute-path.*` and `home-relative-path.read-only` | **Succeeded** | Extension logged `PROBE_ABS_READ_OK len=212`, `PROBE_HOMEREL_READ_OK len=12`, and in the read-write probe `P2_CMD_WRITE_OK` with zero TCC denials. |

**Decision:**
- For **local development and this spike**, `build.sh` scopes the generated entitlements strictly to a disposable `--shared-dir` (defaulting to `<out>/spike-shared`) via `com.apple.security.temporary-exception.files.absolute-path.read-write`, and both the extension and throwaway mock host default to that same disposable path. Real `~/Library/Application Support/FolderSync/` is never touched by the spike.
- For **future v1 production packaging** (outside the Mac App Store per plan §19), use `com.apple.security.temporary-exception.files.home-relative-path.read-write` scoped narrowly to `/Library/Application Support/FolderSync/finder/`, or migrate to a `<TEAM_ID>.dev.foldersync.shared` macOS App Group once a Developer ID certificate is available (see documented expectations in the capability matrix below).

### 3. Can root-level badges be driven live by an atomically written snapshot, with staleness handling?

**Yes.** Verified live with the shipped `apps/finder-extension/build.sh` bundle (`dev.foldersync.spike.finder-extension`) in a `mktemp -d` directory:

**Exact verification commands:**

```bash
VERIFY_DIR="$(mktemp -d)"
./apps/finder-extension/build.sh --test --out "$VERIFY_DIR/out" --shared-dir "$VERIFY_DIR/shared" --bundle-id dev.foldersync.spike
# Wrote atomic status.json with Research=synced, Family=syncing, Docs=attention, Archive=paused, UnmountedDisk/Photos=disk_missing
"$VERIFY_DIR/out/FolderSyncHost.app/Contents/MacOS/FolderSyncHost" &
pluginkit -a "$VERIFY_DIR/out/FolderSyncHost.app/Contents/PlugIns/FolderSyncFinder.appex"
pluginkit -e use -i dev.foldersync.spike.finder-extension
open "$VERIFY_DIR/watched"
open "$VERIFY_DIR/watched/Research"
# Atomically updated Research -> attention, then ran:
"$VERIFY_DIR/out/FolderSyncHost.app/Contents/MacOS/FolderSyncHost" --notify-snapshot
# Atomically updated generated_at -> 2020-01-01T00:00:00Z, then ran:
"$VERIFY_DIR/out/FolderSyncHost.app/Contents/MacOS/FolderSyncHost" --notify-snapshot
/usr/bin/log show --start "$START_TIME" --info --style compact \
  --predicate 'subsystem == "dev.foldersync.app.finder-extension" OR subsystem == "dev.foldersync.app"'
```

**Observed results:**

1. Opening `<tmp>/watched` caused Finder to invoke `requestBadgeIdentifier(for:)` for all four existing mapped roots (`Archive`, `Docs`, `Family`, `Research`) and set their distinct badges (`paused`, `attention`, `syncing`, `synced`).
2. Opening `<tmp>/watched/Research` triggered `beginObservingDirectory(at:)` and requested badges for child `notes.txt`; the extension returned `""` (empty string) for all non-root descendants, enforcing the v1 roots-only rule.
3. Atomically replacing `status.json` (`Research`: `synced` → `attention`) and posting `dev.foldersync.finder.snapshot-updated` updated all observed root badges immediately (`Updated badge root=.../Research badge=attention`).
4. Replacing `generated_at` with an expired timestamp (`2020-01-01T00:00:00Z`) and notifying switched all four observed roots to `stale`.
5. `NSRunningApplication.runningApplications(withBundleIdentifier:)` succeeded inside the sandboxed extension (`count=1` when running), enabling host-liveness staleness detection alongside timestamp expiry.
6. Failed snapshot reads or decodes (deleted, corrupt, or unsupported-version `status.json`) are carried into both badge and menu evaluation as `.stale` while retaining `lastKnownSnapshot` solely to show the `stale` badge on previously known roots (verified in `SnapshotModelTests` valid-to-invalid transition test).

**Implementation lessons learned during verification:**
- **Canonical path normalization is mandatory:** Finder passes resolved paths (`/private/var/...`), whereas callers often write symlinked paths (`/var/...`). Both snapshot roots and Finder callback URLs must be canonicalized via `standardizedFileURL.resolvingSymlinksInPath()`.
- **Do not pre-badge unrequested URLs:** Calling `setBadgeIdentifier(_:for:)` during `init()` before Finder calls `requestBadgeIdentifier(for:)` suppresses Finder's initial badge callbacks (documented in `FinderSync.h`). Push updates only to URLs previously seen in `requestBadgeIdentifier(for:)`.
- **Do not probe arbitrary user folders inside the sandbox:** Pass snapshot mapping roots directly to `FIFinderSyncController.default().directoryURLs` rather than gating them on `FileManager.fileExists(atPath:)` inside the extension sandbox.

### 4. How should narrow actions be sent back to the host app?

Two sandbox-compatible mechanisms were evaluated in a standalone live probe (`ProbeExt2` / `ProbeHost2`, bundle ID `dev.foldersync.probe2.ext`, where `ProbeExt2.init()` executed both IPC paths against a running `ProbeHost2` launched via `open -g -a "$APP_PATH"` with `CFBundleURLTypes` registering `foldersync-probe2://`):

**Exact IPC probe commands:**

```bash
# Built ProbeHost2.app (with CFBundleURLSchemes=["foldersync-probe2"]) and ProbeExt2.appex
# (with temporary-exception.files.absolute-path.read-write for $SNAP_DIR/)
codesign --force --sign - --entitlements "$PROBE_DIR/ent2.plist" "$APPEX_PATH"
codesign --force --sign - "$APP_PATH"
open -g -a "$APP_PATH"
pluginkit -a "$APPEX_PATH" && pluginkit -e use -i dev.foldersync.probe2.ext
open "$WATCH_PARENT"
/usr/bin/log show --start "$START_TIME" --info --style compact --predicate 'subsystem == "dev.foldersync.probe2"'
```

**Observed probe results (from `ProbeExt2` / `ProbeHost2`, NOT interactive clicks on the shipped Finder menu):**

| Candidate | Observed Result (ad hoc temp bundle) | Unified Log Evidence & Tradeoffs |
| --- | --- | --- |
| **Atomic command file + Darwin notification** (`CFNotificationCenterGetDarwinNotifyCenter`) | **Succeeded (1 ms latency)** | `18:40:11.639 P2_CMD_WRITE_OK` → `18:40:11.639 P2_DARWIN_POSTED` → `18:40:11.640 P2_HOST_DARWIN_CMD payload={"action":"sync_now","mapping_id":"research"}`. Carries zero credentials and avoids bringing the host app to the foreground for background actions (`Sync now`, `Pause`). Expected (not yet verified) to behave the same in installed `/Applications` bundles. |
| **Custom URL scheme** (`NSWorkspace.shared.open(url)`) | **Failed in temp-dir probe** | `P2_URL_HANDLER path=NIL` and `P2_URL_OPEN_ERR err=The file couldn’t be opened. domain=NSCocoaErrorDomain code=256`. LaunchServices did not register the URL scheme for the ad hoc bundle inside `$TMPDIR`. |

**Decision & distinction between spike mock host and future v1 manager:**
- Use **atomic action JSON files + Darwin notification** (`dev.foldersync.finder.action-requested`) for all four context menu actions (`sync_now`, `pause`, `open_app`, `show_last_run`).
- The shipped extension enforces the action contract at three layers: `menu.autoenablesItems = false` disables mapping-only items (`Sync now`, `Pause`, `Show last run`) whenever no mapping owns the target (such as toolbar clicks outside watched folders); the sender (`dispatchAction` / `writeActionRequest`) rejects invalid requests; and the receiver validates via `decodeValidActionRequest`.
- **Mock vs. v1 manager:** The bundled `FolderSyncHost.app` in `apps/finder-extension/Sources/HostAppMain.swift` is a **throwaway mock host** that only validates, logs, and deletes queued action files. In v1 (Group 3/4), the Rust manager will own draining `actions/*.json` and dispatching validated commands into the manager command channel.

## Status Snapshot & Action Contracts (v1)

- **Spike default shared directory:** `<out>/spike-shared` (via `build.sh`, or `--shared-dir <tmp>`).
- **Planned v1 production shared directory:** `~/Library/Application Support/FolderSync/finder/` (never touched by this spike).

### Snapshot File: `status.json` (Host → Extension)

Written atomically (`status.json.tmp` → rename to `status.json`), followed by posting Darwin notification `dev.foldersync.finder.snapshot-updated`:

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
- **Staleness rule:** If `status.json` is missing, unreadable, corrupt, has an unsupported version, is older than `stale_after_seconds`, or the host app bundle ID is not running, all known mapping roots display the `stale` badge (`FolderSync unavailable`) and the context menu shows plain-text stale status.
- **Accessibility:** Every badge registers a distinct symbol shape, color, and text label; the context menu always begins with a disabled plain-text status line (`Status: ...`).

### Action Files: `actions/action-<uuid>.json` (Extension → Host)

Written atomically by the extension, followed by posting Darwin notification `dev.foldersync.finder.action-requested`:

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
- `mapping_id`: Required (non-empty string) for `sync_now`, `pause`, and `show_last_run`; nullable only for `open_app`. No paths, rclone flags, or credentials ever cross this boundary.
- **Consumption:** The spike's mock host validates, logs, and deletes each file; the future v1 Rust manager will validate, dispatch the manager command, and delete each file.

## What Requires Full Xcode vs. Developer ID / Provisioning Profile

Cells marked **Expected (unverified)** reflect Apple's macOS signing/app-group documentation and NSH's packaging guide; they have not been executed on this machine because no signing certificates or full `Xcode.app` are installed.

| Capability | CLT + Ad Hoc (`codesign -s -`) | CLT + Developer ID (Apple Developer Account) | Full `Xcode.app` |
| --- | --- | --- | --- |
| Compile Swift `FIFinderSync` `.appex` | **Verified** | Expected (unverified; same `swiftc` step) | Optional |
| Badge rendering (SF Symbols / CoreGraphics) | **Verified** (code + registration; not visually inspected) | Expected (unverified) | Optional (only needed for `.xcassets` / `actool`) |
| Standalone Swift unit tests (`SnapshotModelTests`) | **Verified** (54/54 assertions) | Expected (unverified) | Optional (only needed for `XCTest` / `xcodebuild test`) |
| Local dev registration (`pluginkit -a` / `-e use`) | **Verified** | Expected (unverified) | Not required |
| Shared snapshot via temporary file exception | **Verified** (temp dir) | Expected (unverified; permitted outside Mac App Store per Apple docs) | Not required |
| `<TEAM_ID>.*` macOS App Group (no profile) | **Verified blocked** (no Team ID) | Expected (unverified; Apple documents `<TEAM_ID>.*` for Developer ID macOS apps) | Not required |
| `group.*` App Group or Mac App Store build | **Verified blocked** by TCC (`kTCCServiceSystemPolicyAppData`) | Expected to require provisioning profile (unverified) | Simplifies profile management |
| Multi-Mac distribution without Gatekeeper blocks | Expected blocked (ad hoc fails Gatekeeper; unverified on second Mac) | Expected via `codesign --options runtime` + `xcrun notarytool` + `xcrun stapler` (unverified; owner release task) | Not required |

**Summary:** Full `Xcode.app` is **not required** for local development and is not expected to be required for Developer ID release builds (`codesign`, `xcrun notarytool`, and `xcrun stapler` ship with CLT). A **Developer ID Application certificate** and Apple notarization are expected to be required before distributing the app + embedded extension to other household Macs (plan §19; pending qualification with owner credentials).

## Recommendation for v1 Packaging (NSH Integration)

Keep the extension as a small CLT-built Swift target under `apps/finder-extension/` and integrate it as a post-packaging step on `nsh package --target macos`:

1. Build the Rust desktop app via `nsh package --target macos --id dev.foldersync.app` to produce `FolderSync.app`.
2. Build `FolderSyncFinder.appex` from `apps/finder-extension/` matching `FolderSync.app`'s bundle ID (`dev.foldersync.app.finder-extension`) and production entitlements (`home-relative-path.read-write` for `/Library/Application Support/FolderSync/finder/`, or `<TEAM_ID>.*` App Group once Developer ID signing is qualified).
3. Embed `FolderSyncFinder.appex` at `FolderSync.app/Contents/PlugIns/FolderSyncFinder.appex` (alongside the pinned `rclone` binary per plan §19).
4. Sign strictly **inner to outer**:
   1. Embedded `rclone` binary
   2. `Contents/PlugIns/FolderSyncFinder.appex` (with entitlements, plus `--options runtime` for Developer ID)
   3. Outer `FolderSync.app` (with `--options runtime` for Developer ID; avoid `--deep` overriding inner entitlements)
5. For household releases: notarize with `xcrun notarytool`, staple with `xcrun stapler`, and qualify with `codesign --verify --deep --strict`, `spctl --assess`, and a second Mac.
6. Inside the host app UI, expose `FIFinderSyncController.isExtensionEnabled` and `FIFinderSyncController.showExtensionManagementInterface()` so household users can enable the extension in System Settings without Terminal or `pluginkit`.

## What Was NOT Verified in This Spike

- **Visual rendering & interactive Finder context menu clicks:** `screencapture -x` failed with `could not create image from display` in this terminal session (no Screen Recording permission). Badge assignment and live updates were verified via Finder callbacks and unified logs (`requestBadgeIdentifier(for:)` and `setBadgeIdentifier(_:for:)`), not visually on screen. Interactive right-click menu rendering, toolbar menu enablement, and clicking menu items inside Finder were not automated (IPC delivery was verified via the standalone `ProbeExt2`/`ProbeHost2` probe, and action-contract enforcement was verified in `SnapshotModelTests`).
- **Host relaunch on closed app:** The `NSWorkspace.shared.openApplication(at:)` fallback for `Open FolderSync` / `Show last run` when the host app is stopped was not exercised interactively.
- **Installed-bundle behavior & NSH `.app` embedding:** All live runs used throwaway bundles in `$TMPDIR`; embedding into actual `nsh package --target macos` output and running from `/Applications` remain to be verified in Group 4 packaging.
- **Developer ID signing, notarization, `<TEAM_ID>.*` App Groups, and multi-Mac Gatekeeper checks:** No signing identities exist on this machine and the Keychain was not touched; all release-signing rows in the table above are documented expectations pending owner qualification.
