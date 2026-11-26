# Release, Packaging, Signing, and Upgrades

FolderSync packages a self-contained macOS `.app` bundle containing the NSH 0.6.1 desktop application, a verified pinned `rclone` binary (`1.76.0`), the embedded Swift Finder Sync extension (`FolderSyncFinder.appex`), and verbatim third-party license notices.

> The desktop app uses the live manager by default and runs sync and account operations with the verified bundled rclone helper. The desktop executable also serves as the account credential helper. The manager publishes the status used by the embedded Finder extension.
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
| `Contents/MacOS/FolderSync` | NSH 0.6.1 pure-Rust macOS executable (`CFBundleIdentifier = dev.foldersync.app`) |
| `Contents/Helpers/rclone` | Pinned `rclone` `1.76.0` executable copied from `.tools/rclone/1.76.0/rclone` after SHA-256 verification against `packaging/qualification-manifest.toml` |
| `Contents/Resources/rclone-manifest.json` | Sealed metadata recording rclone version, platform, fixed relative path (`Contents/Helpers/rclone`), upstream archive SHA-256, upstream executable SHA-256, and post-signing helper SHA-256 |
| `Contents/PlugIns/FolderSyncFinder.appex` | Sandboxed macOS Finder Sync extension (`CFBundleIdentifier = dev.foldersync.app.finder-extension`, `FolderSyncHostBundleIdentifier = dev.foldersync.app`) |
| `Contents/Resources/Notices/` | `NOTICE.txt`, `FOLDERSYNC-LICENSE.txt` (MIT), verbatim `NSH-LICENSE-APACHE-2.0.txt` (NSH `v0.6.1`), verbatim `RCLONE-LICENSE-MIT.txt` (rclone `v1.76.0`), and `PAPER-MONO-OFL.txt` (SIL OFL 1.1). Exact upstream URLs and SHA-256 digests are recorded in [`packaging/notices/SOURCES.md`](../packaging/notices/SOURCES.md). |

The build checks the upstream executable SHA-256 before copying and checks the
staged copy before signing. Signing changes the helper's bytes. The build records
`upstream_archive_sha256`, `upstream_manifest_sha256` (the upstream executable
pin), and `bundled_sha256` (the signed helper) before sealing the outer app.
Startup qualification verifies the seal and uses `bundled_sha256` for the
packaged helper. [Engine profile](engine-profile.md)

## Ad Hoc vs. Developer ID Signing

`packaging/build-app.sh` signs strictly **inner to outer** (never `codesign --deep` during signing, which would overwrite the Finder extension's entitlements):

1. `Contents/Helpers/rclone` with `--options runtime` in both signing modes
2. `Contents/Resources/rclone-manifest.json` written with the signed helper digest
3. `Contents/PlugIns/FolderSyncFinder.appex` with its entitlements plist
4. Outer `FolderSync.app`

Then the script validates the staged bundle with `codesign --verify --deep --strict --verbose=2` before replacing `target/package/FolderSync.app`.

| Mode | Command | Flags | Appropriate for |
| --- | --- | --- | --- |
| **Ad hoc (default)** | `packaging/build-app.sh` | `--force --sign -` | Local development and packaging verification on the build Mac. Cannot be notarized and will not pass Gatekeeper on another Mac. |
| **Developer ID (owner release)** | `packaging/build-app.sh --sign "Developer ID Application: <Name> (<TEAM_ID>)"` | `--force --sign <identity> --options runtime --timestamp` | Household / external distribution outside the Mac App Store (plan §19). |

### Local verification of the packaged helper

The helper check uses the built app as a fixture and runs only the isolated
`rclone version` probe. Run from the repository root after fetching pinned rclone.

```bash
(
set -e
NSH_BIN="$HOME/.local/bin/nsh"
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
mkdir "$scratch/home" "$scratch/state"
export HOME="$scratch/home" FOLDERSYNC_STATE_DIR="$scratch/state"
packaging/build-app.sh --nsh-bin "$NSH_BIN"
codesign --verify --deep --strict target/package/FolderSync.app
cargo test --locked -p rclone-adapter --lib qualifies_ad_hoc_packaged_helper -- --ignored
)
```

### Finder Extension Entitlements

- **Dev / ad hoc / initial Developer ID builds (outside the Mac App Store):** `apps/finder-extension/FinderExtension.entitlements` enables `com.apple.security.app-sandbox` and grants a narrow home-relative temporary file exception (`com.apple.security.temporary-exception.files.home-relative-path.read-write`) scoped strictly to `/Library/Application Support/FolderSync/finder/`. Inside the sandbox, the extension resolves the real user home via `getpwuid(getuid())`, validates every path component from the state root down via `fstatat(..., AT_SYMLINK_NOFOLLOW)` + `openat(..., O_DIRECTORY | O_NOFOLLOW)`, and opens `status.json` with `O_NONBLOCK | O_NOFOLLOW` requiring a regular file.
- **Isolated dev override:** Passing `--dev-shared-dir` to `packaging/build-app.sh` or `apps/finder-extension/build.sh` creates a fresh private state directory under `<repo>/target/`. The generated absolute-path exception grants access to its `finder/` child. Use the printed `State dir` as `FOLDERSYNC_STATE_DIR` so the manager writes `finder/status.json` directly into the signed handoff.
- **Future `<TEAM_ID>.*` App Group option:** Once Developer ID signing is qualified by the owner, the Finder handoff directory may optionally migrate to a `<TEAM_ID>.*` macOS App Group container (see `docs/decisions/0002-finder-extension.md`).

### Scripted packaged-app and Finder qualification

From the repository root on the build Mac, with Python 3.9+, the pinned Rust
toolchain, NSH 0.6.1, and Xcode Command Line Tools available, run:

```bash
tests/app/run-app-checks.sh
```

The script fetches and verifies pinned rclone and builds an ad hoc package with
bundle ID `dev.foldersync.pkgcheck` and `--dev-shared-dir`. It runs from a
disposable fixture under `target/` with isolated `HOME`,
`FOLDERSYNC_STATE_DIR`, and `TMPDIR`. It uses a local-to-local mapping and
disables automatic scheduling so each test run comes from its own Finder
`sync_now` request. Run from an unlocked console to qualify the Finder checks.

- Verifies the deep strict code signature and checks the bundled helper
  SHA-256 against the sealed manifest.
- Verifies the live manager publishes the fixture snapshot and owns the OS file
  lock. Launches a second packaged instance and confirms the first retains
  exclusive ownership while the second exits through native quit.
- Queues an ADR 0002 `sync_now` request and verifies its consumption, byte-for-byte
  transfer, bundled engine identity, and explicit fixture rclone config. Sends
  native quit to the tracked PID after the queued run drains and checks that
  the lock is free.
- Interrupts an observed partial multi-file transfer by stopping the test
  manager and its verified bundled helper, then terminating both fixture PIDs.
  Relaunches the package and verifies run reconciliation, the
  `RecoveryRequired` hold, and unchanged bisync listings.
- Checks the console state before interacting with Finder. On an unlocked
  console it registers the disposable extension and opens the mapped root
  and its parent in the logged-in Finder session. Opening the root checks
  directory observation. Opening the parent displays the root and requests
  its `paused` badge. The script queries the Finder extension log subsystem
  and matches snapshot decode, watched-root registration, and badge evidence
  to the run's handoff path, host ID, and extension PID. It records these
  checks as **NOT VERIFIED** if the console is locked, absent, or its status
  cannot be determined.

The script writes `target/app-qualification/app-evidence.json` and
`target/app-qualification/app-evidence.md` after cleanup. Each check includes
its status and evidence. Exit status is 0 when all selected checks pass, 1
when a check fails, and 2 when checks remain unverified without a failure.
Cleanup disables and unregisters the staged extension, waits for disposable
processes to exit, and removes the staged bundle and fixtures.

#### Owner-only signed and notarized qualification

Build and notarize a dedicated disposable bundle. The bundle ID and absolute
handoff path are part of the signature, so use the same checkout for the
build and the qualification. Each run consumes the empty handoff directory
created by the build and preserves the signed input bundle.

```bash
scripts/fetch-rclone.sh
packaging/build-app.sh --bundle-id dev.foldersync.pkgcheck --dev-shared-dir \
  --out target/package-appqa-release --sign "Developer ID Application: <Name> (<TEAM_ID>)"
packaging/notarize.sh --app target/package-appqa-release/FolderSync.app \
  --keychain-profile foldersync-notary \
  --out-zip target/package-appqa-release/FolderSync-notarize.zip
tests/app/run-app-checks.sh --release target/package-appqa-release/FolderSync.app
```

This runs the local lifecycle and Finder checks on a staged copy of the signed
bundle and adds `spctl --assess --type execute` and `xcrun stapler validate`.
The script also fetches or verifies the workspace rclone pin for fixture setup.
For fresh-machine Gatekeeper, use the final production bundle on a second Mac
or a clean macOS VM with ordinary download quarantine in place. Follow the
operational verification steps below.

#### Owner-only Keychain qualification

```bash
tests/app/run-app-checks.sh --with-keychain
```

The Keychain check is currently **NOT VERIFIED**. It requires a packaged
credential-helper probe that creates a uniquely keyed item in
`dev.foldersync.test`, reads it with the same executable under a different
`HOME`, and deletes it. The current packaged helper supports only reads from
the production service, so the script records the missing capability without
accessing the Keychain.

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
4. **Operational verification on a second Mac:**
   - Copy the notarized `FolderSync.app` to `/Applications` on a clean Mac without developer tools.
   - Verify Gatekeeper opens the app without quarantine bypasses.
   - Follow the [installed-app qualification checklist](qualification.md#3-packaged-desktop-app--finder-sync-acceptance-owner-only-plan-19-20) for filesystem access, Keychain credentials, Attention, and the `FolderSync Finder` extension.

## Credentials and Privacy Boundaries (Plan §19)

- Keep all application state private (`0700` dirs, `0600` files) under `~/Library/Application Support/FolderSync/` on the internal system disk.
- Store the `rclone.conf` encryption passphrase in macOS Keychain and retrieve it only via an audited `--password-command` helper; never log or export credentials or personal file contents.
- Never run `rclone selfupdate` or resolve `rclone` from `PATH`.

## Upgrades and Rollback (Plan §19, §24)

Upgrade the application and bundled `rclone` engine deliberately:

1. **Stop scheduling and drain active jobs:** Pause scheduling and let any active run finish cleanly (or stop it explicitly), then quit FolderSync so the exclusive OS state lock (`manager.lock`) is released.
2. **Create a consistent local state checkpoint:** With all writers stopped, back up `~/Library/Application Support/FolderSync/` (`state.sqlite`, `accounts/`, `mappings/` including opaque `bisync/` workdirs, `runs/`, and `recovery-records/`) together as one unit. Never copy bisync state from another computer.
3. **Verify compatibility:** Confirm SQLite schema migrations (`schema_migrations` is forward-only; opening a newer DB with an older binary fails explicitly rather than downgrading silently) and rclone bisync listing compatibility for the new pinned `rclone` version.
4. **Re-run qualification:** Run `tests/qualification/run-local.sh` and CI. For engine or backend changes, complete the [owner Drive qualification checks](qualification.md#1-real-google-drive-qualification-testsqualificationrun-drivesh) before distribution.
5. **Pilot before rollout:** Test the new bundle on one disposable mapping first, then one real mapping, before rolling out to other household computers.

### Rollback Procedure

Do not promise unconditional in-place downgrade across schema or rclone engine versions—an older binary may not be able to read a migrated SQLite schema or listings rewritten by a newer `rclone`.

To roll back safely:

1. Pause scheduling and quit FolderSync cleanly so no `rclone` child remains active.
2. Leave local synchronized folders, cloud contents, logs, and recovery copies intact.
3. Either restore the pre-upgrade application binary together with the consistent local state checkpoint taken before upgrading (only if no post-upgrade changes need to be reconciled into those listings), or keep the local/cloud trees intact and re-establish a fresh reviewed generation after inspecting both sides.
4. Do not blindly reactivate a retired third-party sync client (such as Sync.app or Google Drive desktop) against a folder whose state changed while managed by FolderSync.
