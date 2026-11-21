# ADR 0002: macOS Finder Sync Extension Feasibility & Architecture

- **Date:** 2026-11-20
- **Status:** Accepted (feasibility spike verified; ready for Group 3/4 manager and NSH packaging integration)
- **Scope:** v1 macOS Finder integration (mapped-folder **root** badges + narrow context menu)

## Context

FolderSync v1 includes Finder status badges on **mapped-folder roots only** (`synced`, `syncing`, `attention`, `paused`, `disk_missing`, plus `stale`/unavailable) and a narrow contextual menu (`Sync now`, `Pause`, `Open FolderSync`, `Show last run`). Per plan §17 and the v1 scope rulings:

- No per-file status index in v1.
- The extension owns no sync state, holds no OAuth credentials, and never launches `rclone`.
- Development machines may have only **Xcode Command Line Tools** (no full `Xcode.app`) and **no code-signing identities** (ad hoc signing only).

This spike tests what works end-to-end with CLT-only tooling and ad hoc signing on macOS, identifies boundaries requiring a Developer ID or full Xcode, and defines the v1 snapshot/action contracts.

## Test Environment

- **OS:** macOS 27.2.1 (`arm64`)
- **Toolchain:** Xcode Command Line Tools only (`swiftc` 6.4.2, `codesign`, `plutil`, `pluginkit`, `xcrun`); no `Xcode.app` / `xcodebuild`
- **Signing:** Ad hoc (`codesign --force --sign -`) with entitlements plist; no signing certificates installed and no Keychain access

## Answers to Feasibility Questions (With Empirical Evidence)

### 1. Can a Swift Finder Sync extension be built with CLT only, ad hoc signed, embedded, and registered locally?

**Yes.** Verified end-to-end via `apps/finder-extension/build.sh` (see `apps/finder-extension/README.md` for full runnable commands).

- **Compilation (no Xcode):** `swiftc` compiles a `FIFinderSync` subclass directly into an app extension Mach-O binary using `-parse-as-library -application-extension -target arm64-apple-macos14.0 -Xlinker -e -Xlinker _NSExtensionMain` linked against `Cocoa`, `FinderSync`, and `Foundation`.
- **No Asset Catalog required:** Badge images are rendered programmatically with macOS SF Symbols (`NSImage(systemSymbolName:accessibilityDescription:)`) and a CoreGraphics fallback, avoiding `actool` (which requires full `Xcode.app`).
- **Bundle assembly & inner-to-outer ad hoc signing:** `build.sh` embeds `Contents/PlugIns/FolderSyncFinder.appex` inside a throwaway mock host bundle (`FolderSyncHost.app`, bundle ID `dev.foldersync.spike.host`), signs inner `.appex` with entitlements then outer `.app` with `codesign --force --sign -`, and verifies with `codesign --verify --deep --strict`.
- **Local registration & execution:** `pluginkit -a <appex>` and `pluginkit -e use -i <bundle-id>` succeed. When Finder opens a watched directory's parent, `pkd`/RunningBoard launches the ad hoc signed `.appex` (`amfid` logs informational `Code=-423` for ad hoc signing, and AppleSystemPolicy permits local execution).

### 2. Where can a sandboxed extension read the status snapshot with ad hoc signing?

Finder Sync extensions must run inside the macOS App Sandbox (`com.apple.security.app-sandbox = true`). Before writing the shipped spike bundle, we ran **exploratory throwaway probes** (temporary Swift `FIFinderSync` extensions compiled with `swiftc`, ad hoc signed with `codesign --force --sign -`, registered via `pluginkit`, triggered by opening temp directories in Finder, inspected via `/usr/bin/log show`, and cleaned up; those throwaway probe sources were not committed).

**Observed exploratory probe results (macOS 27.2.1, ad hoc signed):**

| Candidate | Entitlement Tested (Exploratory Probe) | Observed Result | Unified Log Evidence |
| --- | --- | --- | --- |
| **App Group container (`group.*`)** | `com.apple.security.application-groups = ["group.dev.foldersync.probe"]` | **Failed** | `containerURL(forSecurityApplicationGroupIdentifier:)` returned `~/Library/Group Containers/group.dev.foldersync.probe`, but writing `probe.txt` failed with permission error (`sandboxd`: `kTCCServiceSystemPolicyAppDataDetailed denied by TCC for ProbeExt`). Without a Team ID or provisioning profile, macOS App Data Protection blocks access to `group.*` containers. |
| **Temporary file exception (read-only / read-write)** | `com.apple.security.temporary-exception.files.absolute-path.*` (temp dir) and `home-relative-path.read-only` (throwaway temp subdir) | **Succeeded** | Extension logged `PROBE_ABS_READ_OK len=212`, `PROBE_HOMEREL_READ_OK len=12`, and in a follow-up read-write exploratory probe `P2_CMD_WRITE_OK` with zero TCC denials. |

**Decision:**
- For **this spike and local development**, `build.sh` scopes generated entitlements strictly to a disposable `--shared-dir` (defaulting to `<out>/spike-shared`) via `com.apple.security.temporary-exception.files.absolute-path.read-write`, and both the extension and throwaway mock host (`dev.foldersync.spike.host`) default to that same disposable path. The spike never touches any real FolderSync state directory.
- For **future v1 production packaging** (outside the Mac App Store per plan §19), either scope a narrow `com.apple.security.temporary-exception.files.home-relative-path.read-write` entitlement to the app's own Finder handoff subdirectory, or migrate to a `<TEAM_ID>.*` macOS App Group once a Developer ID certificate is available (see documented expectations in the capability matrix below).

### 3. Can root-level badges be driven live by an atomically written snapshot, with staleness handling?

**Yes.** Verified live with the shipped `apps/finder-extension/build.sh` bundle in a `mktemp -d` directory using the exact build, fixture, atomic `status.json` replacement, `--notify-snapshot`, and cleanup commands documented in `apps/finder-extension/README.md`.

**Observed live Finder results (unified log `subsystem == "dev.foldersync.app.finder-extension"`):**

1. Opening `<tmp>/watched` caused Finder to invoke `requestBadgeIdentifier(for:)` for all four existing mapped roots (`Archive`, `Docs`, `Family`, `Research`) and set their distinct badges (`paused`, `attention`, `syncing`, `synced`).
2. Opening `<tmp>/watched/Research` triggered `beginObservingDirectory(at:)` and requested badges for child `notes.txt`; the extension returned `""` (empty string) for all non-root descendants, enforcing the v1 roots-only rule.
3. Atomically replacing `status.json` (`Research`: `synced` → `attention`) and running `FolderSyncHost --notify-snapshot` updated all observed root badges immediately (`Updated badge root=.../Research badge=attention`).
4. Replacing `generated_at` with an expired timestamp (`2020-01-01T00:00:00Z`) and running `--notify-snapshot` switched all four observed roots to `stale`.
5. `NSRunningApplication.runningApplications(withBundleIdentifier:)` succeeded inside the sandboxed extension (`count=1` when running), enabling host-liveness staleness detection alongside timestamp expiry.

**Unit-tested model guarantees (not live-verified in Finder):**
- Deleting `status.json` or replacing it with corrupt/unsupported-version JSON while the host app is running and the prior snapshot is still within TTL evaluates as `.stale(..., readSucceeded: false)`. The cached snapshot is retained strictly for stale root badges; `menuMapping` and `directoryRootURLs` return `nil`, so the menu disables mapping actions (`Sync now`, `Pause`, `Show last run`) and exposes only `Open FolderSync` with no mapping ID. Verified in `apps/finder-extension/Tests/SnapshotModelTests.swift` (section `3b`), not exercised interactively in Finder.

**Implementation lessons learned during live verification:**
- **Canonical path normalization is mandatory:** Finder passes resolved paths (`/private/var/...`), whereas callers often write symlinked paths (`/var/...`). Both snapshot roots and Finder callback URLs must be canonicalized via `standardizedFileURL.resolvingSymlinksInPath()`.
- **Do not pre-badge unrequested URLs:** Calling `setBadgeIdentifier(_:for:)` during `init()` before Finder calls `requestBadgeIdentifier(for:)` suppresses Finder's initial badge callbacks (documented in `FinderSync.h`). Push updates only to URLs previously seen in `requestBadgeIdentifier(for:)`.
- **Do not probe arbitrary user folders inside the sandbox:** Pass snapshot mapping roots directly to `FIFinderSyncController.default().directoryURLs` rather than gating them on `FileManager.fileExists(atPath:)` inside the extension sandbox.

### 4. How should narrow actions be sent back to the host app?

Two sandbox-compatible mechanisms were evaluated in an **exploratory throwaway probe** (`ProbeExt2` / `ProbeHost2` in a `mktemp -d` directory, where `ProbeExt2.init()` exercised both IPC mechanisms against a running `ProbeHost2` launched with `open -g -a` and `CFBundleURLTypes` registering `foldersync-probe2://`; those throwaway probe sources were not committed, and the shipped Finder context menu was not clicked interactively):

**Observed exploratory probe results (`ProbeExt2` / `ProbeHost2`, NOT interactive clicks on the shipped Finder menu):**

| Candidate | Observed Result (ad hoc temp bundle) | Unified Log Evidence & Tradeoffs |
| --- | --- | --- |
| **Atomic command file + Darwin notification** (`CFNotificationCenterGetDarwinNotifyCenter`) | **Succeeded (1 ms latency)** | `18:40:11.639 P2_CMD_WRITE_OK` → `18:40:11.639 P2_DARWIN_POSTED` → `18:40:11.640 P2_HOST_DARWIN_CMD payload={"action":"sync_now","mapping_id":"research"}`. Carries zero credentials and avoids bringing the host app to the foreground for background actions (`Sync now`, `Pause`). Expected (not yet verified) to behave the same in installed `/Applications` bundles. |
| **Custom URL scheme** (`NSWorkspace.shared.open(url)`) | **Failed in temp-dir probe** | `P2_URL_HANDLER path=NIL` and `P2_URL_OPEN_ERR err=The file couldn’t be opened. domain=NSCocoaErrorDomain code=256`. LaunchServices did not register the URL scheme for the ad hoc bundle inside `$TMPDIR`. |

**Decision & distinction between spike mock host and future v1 manager:**
- Use **atomic action JSON files + Darwin notification** (`dev.foldersync.finder.action-requested`) for all four context menu actions (`sync_now`, `pause`, `open_app`, `show_last_run`).
- The shipped extension enforces the action contract at construction, menu, sender, and receiver layers:
  - `menu.autoenablesItems = false` disables mapping-only items (`Sync now`, `Pause`, `Show last run`) whenever no successful-read mapping owns the target (such as toolbar clicks outside watched folders or after a failed snapshot read).
  - `ActionRequest`'s failable initializer and `Decodable` implementation make it impossible to construct or decode a mapping-required action without a non-empty `mapping_id`.
  - Both `writeActionRequest` and `decodeValidActionRequest` enforce the same rule.
- **Mock vs. v1 manager:** The bundled `FolderSyncHost.app` (`dev.foldersync.spike.host`) in `apps/finder-extension/Sources/HostAppMain.swift` is a **throwaway mock host** that only validates, logs, and deletes queued action files. In v1 (Group 3/4), the Rust manager will own draining `actions/*.json` and dispatching validated commands into the manager command channel.

## Status Snapshot & Action Contracts (v1)

- **Spike shared directory:** Disposable directory `<out>/spike-shared` (default in `build.sh`) or `--shared-dir <tmp>`.
- **Future v1 production handoff directory:** Chosen during Group 4 packaging (either a Developer ID `<TEAM_ID>.*` App Group container or a dedicated Finder handoff subdirectory under the app's macOS application-support state).

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
- **Staleness rule:** If `status.json` is missing, unreadable, corrupt, has an unsupported version, is older than `stale_after_seconds`, or the host app bundle ID is not running, all known mapping roots display the `stale` badge (`FolderSync unavailable`) and the context menu shows plain-text stale status. After a failed read/decode, menu mappings and watch-list updates are cleared (only `Open FolderSync` remains enabled, with no mapping ID).
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
- `mapping_id`: Required (non-empty string) for `sync_now`, `pause`, and `show_last_run`; nullable only for `open_app`. Enforced at `ActionRequest` construction, JSON decoding, writing, and receiving. No paths, rclone flags, or credentials ever cross this boundary.
- **Consumption:** The spike's mock host (`dev.foldersync.spike.host`) validates, logs, and deletes each file; the future v1 Rust manager will validate, dispatch the manager command, and delete each file.

## What Requires Full Xcode vs. Developer ID / Provisioning Profile

Cells marked **Expected (unverified)** reflect Apple's macOS signing/app-group documentation and NSH's packaging guide; they have not been executed on this machine because no signing certificates or full `Xcode.app` are installed.

| Capability | CLT + Ad Hoc (`codesign -s -`) | CLT + Developer ID (Apple Developer Account) | Full `Xcode.app` |
| --- | --- | --- | --- |
| Compile Swift `FIFinderSync` `.appex` | **Verified** | Expected (unverified; same `swiftc` step) | Optional |
| Badge rendering (SF Symbols / CoreGraphics) | **Verified** (code + registration; not visually inspected) | Expected (unverified) | Optional (only needed for `.xcassets` / `actool`) |
| Standalone Swift unit tests (`SnapshotModelTests`) | **Verified** (63/63 assertions) | Expected (unverified) | Optional (only needed for `XCTest` / `xcodebuild test`) |
| Local dev registration (`pluginkit -a` / `-e use`) | **Verified** | Expected (unverified) | Not required |
| Shared snapshot via temporary file exception | **Verified** (temp dir) | Expected (unverified; permitted outside Mac App Store per Apple docs) | Not required |
| `<TEAM_ID>.*` macOS App Group (no profile) | **Verified blocked** (no Team ID) | Expected (unverified; Apple documents `<TEAM_ID>.*` for Developer ID macOS apps) | Not required |
| `group.*` App Group or Mac App Store build | **Verified blocked** by TCC (`kTCCServiceSystemPolicyAppData`) | Expected to require provisioning profile (unverified) | Simplifies profile management |
| Multi-Mac distribution without Gatekeeper blocks | Expected blocked (ad hoc fails Gatekeeper; unverified on second Mac) | Expected via `codesign --options runtime` + `xcrun notarytool` + `xcrun stapler` (unverified; owner release task) | Not required |

**Summary:** Full `Xcode.app` is **not required** for local development and is not expected to be required for Developer ID release builds (`codesign`, `xcrun notarytool`, and `xcrun stapler` ship with CLT). A **Developer ID Application certificate** and Apple notarization are expected to be required before distributing the app + embedded extension to other household Macs (plan §19; pending qualification with owner credentials).

## Recommendation for v1 Packaging (NSH Integration)

Keep the extension as a small CLT-built Swift target under `apps/finder-extension/` and integrate it as a post-packaging step on `nsh package --target macos`:

1. Build the Rust desktop app via `nsh package --target macos --id dev.foldersync.app` to produce `FolderSync.app` (distinct from this spike's mock host ID `dev.foldersync.spike.host`).
2. Build `FolderSyncFinder.appex` from `apps/finder-extension/` matching `FolderSync.app`'s bundle ID (`dev.foldersync.app.finder-extension`) and v1 production entitlements (`<TEAM_ID>.*` App Group once Developer ID signing is qualified, or a narrow home-relative temporary file exception scoped to the app's Finder handoff directory).
3. Embed `FolderSyncFinder.appex` at `FolderSync.app/Contents/PlugIns/FolderSyncFinder.appex` (alongside the pinned `rclone` binary per plan §19).
4. Sign strictly **inner to outer**:
   1. Embedded `rclone` binary
   2. `Contents/PlugIns/FolderSyncFinder.appex` (with entitlements, plus `--options runtime` for Developer ID)
   3. Outer `FolderSync.app` (with `--options runtime` for Developer ID; avoid `--deep` overriding inner entitlements)
5. For household releases: notarize with `xcrun notarytool`, staple with `xcrun stapler`, and qualify with `codesign --verify --deep --strict`, `spctl --assess`, and a second Mac.
6. Inside the host app UI, expose `FIFinderSyncController.isExtensionEnabled` and `FIFinderSyncController.showExtensionManagementInterface()` so household users can enable the extension in System Settings without Terminal or `pluginkit`.

## What Was NOT Verified in This Spike

- **Visual rendering, interactive Finder context menu clicks, and live failed-read transitions in Finder:** `screencapture -x` failed with `could not create image from display` in this terminal session (no Screen Recording permission). Badge assignment and live updates were verified via Finder callbacks and unified logs (`requestBadgeIdentifier(for:)` and `setBadgeIdentifier(_:for:)`), not visually on screen. Interactive right-click menu rendering, toolbar menu enablement, clicking menu items inside Finder, and deleting/corrupting `status.json` live in Finder were not automated (IPC delivery was verified via the exploratory `ProbeExt2`/`ProbeHost2` probe, and failed-read/action-contract rules were verified in `SnapshotModelTests`).
- **Host relaunch on closed app:** The `NSWorkspace.shared.openApplication(at:)` fallback for `Open FolderSync` / `Show last run` when the host app is stopped was not exercised interactively.
- **Installed-bundle behavior & NSH `.app` embedding:** All live runs used throwaway bundles in `$TMPDIR`; embedding into actual `nsh package --target macos` output and running from `/Applications` remain to be verified in Group 4 packaging.
- **Developer ID signing, notarization, `<TEAM_ID>.*` App Groups, and multi-Mac Gatekeeper checks:** No signing identities exist on this machine and the Keychain was not touched; all release-signing rows in the table above are documented expectations pending owner qualification.
