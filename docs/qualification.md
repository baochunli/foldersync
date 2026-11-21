# Qualification

FolderSync qualifies synchronization behavior against pinned tool versions recorded in [`packaging/qualification-manifest.toml`](../packaging/qualification-manifest.toml) (`rclone 1.76.0`, `NSH 0.6.1`, `Rust 1.98.1`). See [`docs/engine-profile.md`](engine-profile.md) for the exact `rclone 1.76.0` argument profile and observed engine quirks.

Local-to-local fixtures verify orchestration, state persistence, multi-client reconciliation on local filesystems, deletion guards, and interruptions. The packaged app uses the live manager, bundled rclone helper, and Finder status handoff. Real Google Drive, physical unplug, and installed-app acceptance still require the owner checks below.

## Automated Local-to-Local Qualification

### 1. Engine adapter integration (`crates/rclone-adapter/tests/local_bisync.rs`)

Verified locally on macOS `arm64` (`osx-arm64`) inside fresh temporary directories with throwaway empty `--config` files, cleared environments, external truth sets, SHA-256 verification, and second-cycle checks:

- Pinned binary SHA-256 + isolated `version` probe qualification, plus launch-boundary re-hashing refusing a binary replaced after qualification.
- Parent environment isolation (`RCLONE_FORCE=true` and `RCLONE_RESYNC=true` cannot bypass `--max-delete` or trigger resync).
- Initialization with `--resync-mode path1` and `--resync-mode path2`.
- Bidirectional add, edit, and delete propagation with `--backup-dir1` and `--backup-dir2` verified by SHA-256.
- Same-file concurrent edit conflict preservation (`--conflict-resolve none`, `--conflict-loser num`) including filenames containing ` - ` and benign filenames containing diagnostic phrases.
- Percentage deletion guard abort (`--max-delete 10`) preserving all files on the peer endpoint, and `--check-access` (`RCLONE_TEST`) failure hold.
- Graceful `SIGINT` interruption during a multi-file batch and recovery on the subsequent `--resilient --recover` cycle.
- Known §10 limitation demonstration: an in-place edit preserving both byte size and modification timestamp is not selected under `--compare size,modtime`.

### 2. Multi-client `sync-cli` qualification harness (`tests/qualification/run-local.sh`)

Run locally with the pinned `rclone` binary installed (via `scripts/fetch-rclone.sh`):

```bash
# Unit and safety-guardrail tests for the qualification harness
python3 -m unittest discover -s tests/qualification -p "test_*.py" -v

# Full local-to-local qualification suite driving sync-cli
tests/qualification/run-local.sh
```

The harness generates deterministic uniquely identifiable file contents (`--seed`), stores an external truth set outside all sync trees for every generated file, isolates `HOME` and `--state-dir` / `--finder-dir` to fresh temporary directories, runs multi-cycle verification across three independent client state directories sharing one local remote fixture, and writes structured evidence to:

- `target/qualification/local-evidence.json`
- `target/qualification/local-evidence.md`

Verified local scenarios (`sync-cli` + `sync-manager` + pinned `rclone 1.76.0` on macOS `arm64`):

| Scenario | What is exercised | Observed result |
| --- | --- | --- |
| `three_client_baseline_and_distinct_edits` | Three independent client state dirs (`client-a`, `client-b`, `client-c`) initialize (Path1 upload + two Path2 downloads), apply distinct edits, and converge over multiple cycles; overwritten v1 copies verified by SHA-256 in local and remote recovery dirs | **PASS** — all 3 clients + remote equal truth set; all v1 recovery copies verified by SHA-256 |
| `three_client_same_file_edit_conflict` | `client-a` and `client-b` edit `shared.txt` while offline, then A, B, C, A, B, C sync across multiple cycles | **PASS** — classified `SuccessWithConflicts`; both edits survive as `shared.txt.conflict1` / `shared.txt.conflict2` on all 3 clients + remote; prior v1 preserved in recovery |
| `three_client_edit_vs_delete` | `client-a` deletes `doc-a.txt` while `client-b` edits it; A syncs delete first, then B, C, A, B converge across cycles | **PASS** — B's edited version survives on all 3 clients + remote; pre-delete version verified in recovery |
| `three_client_offline_return` | `client-c` stays offline while A and B create, edit, and delete (`sub/report.bin`) across 2 cycles; C edits `doc-b.txt` offline and adds a new file, then returns for multi-cycle convergence | **PASS** — C's new file propagates, both conflicting `doc-b.txt` versions survive as conflict copies, and deleted `sub/report.bin` is not resurrected |
| `deletion_guard_abort_and_recovery` | Pause/resume intent persistence, followed by deleting 4 of 10 files (40%) with `--max-delete-percent 10` | **PASS** — `sync-cli run-once` exits non-zero, latches `RecoveryRequired`, publishes Finder `status.json` `state = "attention"`, and preserves all 10 remote files by SHA-256 |
| `interruption_at_phases` | (A) Missing `RCLONE_TEST` pre-listing; (B) 500 uniquely identifiable truth-set batch files (`128 KiB` each), requiring partial transfer progress before `SIGINT` (`1 <= n < 500`), re-sampled destination count strictly `< 500` immediately after `SIGINT` and after process exit, positive interrupted-transfer evidence in the run's `outcome.json`/`output.ndjson`, `RecoveryRequired` hold + refusal to auto-rerun, and follow-up `--resilient --recover` + clean second cycle against the preserved workdir verifying all 501/501 truth-set SHA-256 hashes (reports `NOT VERIFIED`, never `PASS`, if mid-transfer interruption is not positively established); (C) simulated mid-run crash (`running` SQLite row) reconciled on `sync-cli status --json` | **PASS** — post-signal and post-exit destination counts remained `< 500`, run log/outcome recorded the interrupted transfer, `RecoveryRequired` hold enforced, workdir preserved, and 501/501 truth-set SHA-256 hashes converged after follow-up cycles |

**Observed small-mapping nuance (plan §10):** Because `--max-delete 10` enforces a 10% limit on tracked files, a tiny collection with 9 or fewer tracked files treats even a single deletion or conflict rename (`1 / 5 = 20%`) as exceeding 10% and aborts safely into `RecoveryRequired`. The 3-client fixture seeds 17 tracked files (16 ordinary files + `RCLONE_TEST`, so 1 delete = 5.9%) to exercise ordinary single-file conflicts and deletions under the default 10% guard.

---

## Owner-Run Qualification Scenarios (Not Verified in Automated Lanes)

The following scenarios require real Google Drive credentials, two separately authenticated household accounts, physical removable storage, or an Apple Developer account. Never mark M0 or household-release milestones complete until the owner completes and archives evidence for them.

### 1. Real Google Drive Qualification (`tests/qualification/run-drive.sh`)

`tests/qualification/run-drive.sh` enforces strict safety guardrails: it refuses to run unless `--remote <name>`, `--state-dir <dir>` (must be a fresh, empty, non-symlinked directory), and `--rclone-config <disposable-config>` are explicitly passed; refuses `~/Library/Application Support/FolderSync` and `~/.config/rclone`; redacts `--password-command stderr:` payloads at the capture boundary; and uses only the SHA-256-verified pinned `rclone 1.76.0` binary.

The runner covers account connectivity and folder-ID preflight in the disposable shared parent. It records unexercised Drive sync scenarios as `NOT RUN` with overall status `INCOMPLETE` and a nonzero exit. `sync-cli` can check Drive accounts and resolve folder IDs. Drive mapping setup and sync use the desktop app. Use the [pilot guide](pilot-and-rollback.md) for those owner checks.

**Prerequisites (owner setup on a disposable shared Drive parent folder):**

1. Create a disposable shared parent folder in Google Drive (never production household data).
2. Configure a disposable app-owned rclone config file outside `~/.config/rclone` (e.g., inside a fresh `$QUAL_WORK/rclone.conf`) rooted at the actual shared parent folder ID using your own OAuth client credentials (plan §7, §8). Encrypt the config and retrieve its passphrase only via `--password-command`.
3. Optionally configure a second remote (`--remote-b <name>`) authenticated as the second household Google account pointing at the same actual shared parent folder ID.

**Execution:**

```bash
QUAL_WORK="$(mktemp -d)"

# Preview plan and verify guardrails without contacting Google Drive:
tests/qualification/run-drive.sh \
  --remote qualdrive \
  --state-dir "$QUAL_WORK/state" \
  --rclone-config "$QUAL_WORK/rclone.conf" \
  --dry-run

# Run account connectivity and folder-ID preflight:
tests/qualification/run-drive.sh \
  --remote qualdrive \
  --state-dir "$QUAL_WORK/state" \
  --rclone-config "$QUAL_WORK/rclone.conf" \
  --password-command "/path/to/credential-helper --account qual"
```

**Owner Drive checklist (plan §8, §15, §20):**

- [ ] **Shared parent & folder IDs:** Confirm `rclone lsjson --dirs-only` returns an unambiguous folder ID for `Data/<collection>` without `shared_with_me`. After setup, confirm the recovery folder at `Recovery/<folder-id>/<device-id>` is separate from the collection.
  - *Expected outcome:* Exactly one folder ID matches the recorded collection ID; sync proceeds only on exact ID match.
- [ ] **Changed Drive sharing permissions or changed root/collection folder ID:** Revoke/downgrade sharing on the disposable collection (or rename/replace the Drive folder at the same path so its object ID changes) before a run.
  - *Expected outcome:* Preflight detects the ID mismatch or permission denial before any mutating transfer, pauses the mapping in `EndpointChanged` (on folder-ID change) or `AuthenticationRequired` / attention hold (on revoked sharing), and leaves local and cloud files untouched.
- [ ] **Duplicate cloud folder or file names:** Create two identically named folders or files in the disposable Google Drive collection (Google Drive permits duplicate names in one parent).
  - *Expected outcome:* Preflight `lsjson` / engine diagnostics detect ambiguous duplicate names and stop the mapping in an attention / `EndpointChanged` hold for manual resolution rather than choosing one arbitrarily or overwriting data.
- [ ] **Cloud quota exhaustion:** Attempt an upload into an account or disposable test pool that hits Google Drive storage quota exhaustion.
  - *Expected outcome:* Upload fails cleanly with an actionable quota error; existing local and cloud files remain intact and uncorrupted, no local deletion is triggered, and no automatic `--resync` runs.
- [ ] **Two-account ownership, quota & recovery moves:** Using two separately authenticated accounts, verify list, upload, overwrite, deletion, and moving another account's overwritten/deleted file into `Recovery/<folder-id>/<device-id>/<run-id>`. Confirm which account's Google One / Drive quota is charged (plan §8).
  - *Expected outcome:* Both accounts can read, edit, and preserve overwritten/deleted copies in `Recovery/` with exact truth-set SHA-256 hashes; quota ownership is documented explicitly.
- [ ] **Three independent client states on Drive:** Run distinct edits, same-file conflict (`--conflict-resolve none --conflict-loser num`), edit/delete, edit/rename, Drive web edit, delayed upload, overlapping runs, and third-client offline return across multiple follow-up cycles; verify every truth-set SHA-256.
  - *Expected outcome:* All versions accounted for (live file, `.conflict*` copy, or `Recovery/` copy) across follow-up cycles with zero silent data loss or resurrected deletions.
- [ ] **Drive object policy:** Place a Google Doc/Sheet and a Drive shortcut in the disposable collection; confirm `--drive-skip-gdocs` and `--drive-skip-shortcuts` skip them without traversing shortcut targets or corrupting listings.
  - *Expected outcome:* Native Google documents and shortcuts are skipped cleanly without exporting or pulling shortcut target contents into local storage.
- [ ] **OAuth & encrypted config refresh:** Verify token refresh persists cleanly to the encrypted `--config` via `--password-command`, and verify reauthorization after several days and after token revocation.
  - *Expected outcome:* Refreshed tokens persist cleanly; revoked tokens transition the mapping into `AuthenticationRequired` without hanging on hidden stdin prompts or leaking secrets in logs.
- [ ] **Rate limiting & network loss:** Observe HTTP 403/429 throttling and network disconnect mid-run.
  - *Expected outcome:* Bounded backoff or safe recovery hold without data corruption or duplicate concurrent runs.
- [ ] **Scale benchmark (representative subset toward 20 TB):** Measure file count, no-change scan time, changed-file scan time, memory use, API throttling, and recovery-directory growth before enabling automatic scheduling on large collections (plan §13).

### 2. Physical External Disk & Unplug Tests (Owner Only — Plan §11, §20)

Perform on a dedicated disposable APFS external drive (e.g., `/Volumes/TestSSD/QualData` with sibling `/Volumes/TestSSD/FolderSyncRecovery`) while keeping an independent truth copy on the internal disk. Use a disposable sync pair and start with at least 100 MiB free on its sync and local recovery volumes.

Manager tests simulate missing, replacement, and renamed mounts, a reconnect
while paused, low space, and a disconnect during a running child. The real
probe smoke test reads the volume backing a temporary directory. Physical
unplug behavior requires the checks below.

- [ ] **Absent before launch:** Unmount the disk before launching FolderSync.
  - *Expected outcome:* Mapping enters `VolumeUnavailable` (`Waiting for <Volume>`), never auto-creates `/Volumes/<Name>`, and never deletes cloud files.
- [ ] **Wrong disk with identical display name:** Mount a second USB disk given the exact same volume display name (`/Volumes/TestSSD`).
  - *Expected outcome:* The mapping shows “A different disk named TestSSD is connected” and holds without syncing or mutating either side.
- [ ] **Same UUID at a renamed mount:** Rename and remount the expected disk, then try Sync now.
  - *Expected outcome:* The mapping requires endpoint review. Its stored local path and generation stay at the location reviewed at setup.
- [ ] **Reconnect preserves manual pause:** Pause the mapping, eject the disk, reconnect the verified disk.
  - *Expected outcome:* `Intent::Paused` remains `Paused` (never auto-resumes on volume mount).
- [ ] **Mid-run unplug matrix (each phase separately):** Physically disconnect the external disk during:
  1. Before listing (immediately at run start)
  2. During listing
  3. After listing (before transfers)
  4. During upload (local → Drive)
  5. During download (Drive → local)
  6. During deletion propagation
  - *Expected outcome:* The manager detects disk loss at the next poll, requests graceful child cancellation once, and preserves the `bisync` workdir, logs, and run record for review. Record the phase, disk UUID, unplug and stop times, engine version, and terminal outcome. After reconnecting, review recovery and compare every file on both endpoints and in `Recovery/` against the independent SHA-256 truth set through two clean follow-up cycles. A disconnect can occur before the next poll or while remote work is in flight. If any phase causes silent data loss or unacceptable resurrection, keep automatic sync on removable roots unsupported until resolved.

### 3. Packaged Desktop App & Finder Sync Acceptance (Owner Only, Plan §19, §20)

Perform on a Developer-ID-signed and notarized `FolderSync.app` built via `packaging/build-app.sh --sign ...` and `packaging/notarize.sh`. See the [release guide](release.md).

- [ ] **Fresh install on a second Mac:** Install to `/Applications` on a Mac without Rust/Xcode CLT.
  - *Expected outcome:* Gatekeeper accepts the notarized app without quarantine bypasses and `spctl --assess --type execute` succeeds.
- [ ] **Lifecycle & shutdown:** Exercise startup, window close-to-background (keep-alive), tray menu actions, `Command-Q`, native Quit, macOS logout/restart interruption, launch-at-login preference, and second-instance exclusion (`manager.lock`).
  - *Expected outcome:* Closing the window keeps the manager alive; Quit stops scheduling and drains or gracefully interrupts active runs; launching a second instance fails cleanly on `manager.lock`.
- [ ] **Installed-app credentials (Keychain + `--password-command`):** Run syncs from `/Applications/FolderSync.app` using an encrypted app-owned `rclone.conf` whose passphrase is stored in macOS Keychain, and also test with the Keychain locked.
  - *Expected outcome:* The installed signed app retrieves the passphrase cleanly via its credential helper without spawning hidden terminal prompts; a locked Keychain puts affected mappings into `AuthenticationRequired`; zero passphrases or tokens appear in `runs/`, logs, or exported diagnostic bundles.
- [ ] **Installed-app filesystem permissions:** Sync ordinary internal folders, macOS protected folders (Documents/Desktop), and an external APFS volume from the installed `.app` and its `Contents/Helpers/rclone` child.
  - *Expected outcome:* macOS TCC prompts resolve cleanly for the installed bundle and transfers in permitted folders match the pilot truth copies.
- [ ] **Installed-app Attention:** Trigger conflicts, a missing volume, expired sign-in, and a recovery hold with the window hidden.
  - *Expected outcome:* The menu bar shows that attention is needed. After reopening the window, the **Attention** cards explain each hold and the available next step. Desktop notifications are pending implementation.
- [ ] **Update interruption (OS or app update terminating active sync):** Terminate the installed app during an active multi-file transfer (simulating a macOS restart/update or forced upgrade stop), then relaunch FolderSync.
  - *Expected outcome:* Controlled shutdown requests graceful `SIGINT` when the OS permits; if terminated abruptly before completion, restart reconciles unfinished `runs` rows in SQLite, preserves the opaque `bisync` workdir, detects any surviving orphan child by PID + start time + executable without killing unrelated processes, and holds the affected mapping in `RecoveryRequired` until reviewed.
- [ ] **Finder Sync extension:** Enable `FolderSync Finder` in System Settings.
  - *Expected outcome:* Badges (`synced`, `syncing`, `attention`, `paused`, `disk_missing`, `stale`) appear only on mapped-folder roots. Check **Sync now**, **Pause**, **Open FolderSync**, and **Show last run** with the app running. With the app quit, verify **Open FolderSync** can open it and **Show last run** can open it when Finder can read the mapping snapshot.

### 4. Remaining Local / Cross-Platform Matrix Items

- **Remaining local end-to-end checks:** disk full and recovery-location full, unreadable directory permissions, corrupt/missing workdir and stale `.lck` recovery, Unicode normalization and case-only rename collisions, and reviewed bulk deletion workflows in small collections. Manager preflight tests cover symlink rejection and explicit exclusion.
- **Linux (`linux-amd64`):** verify `rclone-adapter` local integration, `/proc/<pid>/stat` process identity, secret-store behavior, and GTK desktop packaging on Ubuntu 24.04+.
