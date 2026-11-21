# Qualification

FolderSync qualifies synchronization behavior against pinned tool versions recorded in [`packaging/qualification-manifest.toml`](../packaging/qualification-manifest.toml) (`rclone 1.76.0`, `NSH 0.6.1`, `Rust 1.98.1`). See [`docs/engine-profile.md`](engine-profile.md) for the exact `rclone 1.76.0` argument profile and observed engine quirks.

> **Qualification boundary:** Local-to-local fixtures verify wrapper orchestration, state persistence, multi-client reconciliation semantics on local filesystems, deletion guards, and phase interruptions. **Local tests never satisfy real Google Drive or removable-volume milestones.** Every real Google Drive, two-account, physical external-disk unplug, and notarized-app scenario below is an **owner-run** qualification task.

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

The harness generates deterministic uniquely identifiable file contents (`--seed`), stores an external truth set outside all sync trees, isolates `HOME` and `--state-dir` / `--finder-dir` to fresh temporary directories, runs multi-cycle verification across three independent client state directories sharing one local remote fixture, and writes structured evidence to:

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
| `interruption_at_phases` | (A) Missing `RCLONE_TEST` pre-listing; (B) `SIGINT` sent to live `rclone bisync` child during a 1200-file batch; (C) simulated mid-run crash (`running` SQLite row) reconciled on `sync-cli status --json` | **PASS** — pre-listing aborts with `EndpointChanged` and zero remote mutation; mid-transfer `SIGINT` and crash reconciliation latch `RecoveryRequired` with opaque bisync workdir and `outcome.json` preserved |

**Observed small-mapping nuance (plan §10):** Because `--max-delete 10` enforces a 10% limit on tracked files, a tiny collection with 9 or fewer tracked files treats even a single deletion or conflict rename (`1 / 5 = 20%`) as exceeding 10% and aborts safely into `RecoveryRequired`. The 3-client fixture seeds 17 tracked files (16 ordinary files + `RCLONE_TEST`, so 1 delete = 5.9%) to exercise ordinary single-file conflicts and deletions under the default 10% guard.

---

## Owner-Run Qualification Scenarios (Not Verified in Automated Lanes)

The following scenarios require real Google Drive credentials, two separately authenticated household accounts, physical removable storage, or an Apple Developer account. Never mark M0 or household-release milestones complete until the owner completes and archives evidence for them.

### 1. Real Google Drive Qualification (`tests/qualification/run-drive.sh`)

`tests/qualification/run-drive.sh` enforces strict safety guardrails: it refuses to run unless `--remote <name>`, `--state-dir <dir>`, and `--rclone-config <disposable-config>` are explicitly passed, refuses `~/Library/Application Support/FolderSync` and `~/.config/rclone`, and uses only the SHA-256-verified pinned `rclone 1.76.0` binary.

**Prerequisites (owner setup on a disposable shared Drive parent folder):**

1. Create a disposable shared parent folder in Google Drive (never production household data).
2. Configure a disposable app-owned rclone config file outside `~/.config/rclone` (e.g., inside a fresh `$QUAL_WORK/rclone.conf`) rooted at the actual shared parent folder ID using your own OAuth client credentials (plan §7, §8). Encrypt the config and test retrieval via `--password-command`.
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

# Run against the disposable Google Drive remote (writes target/qualification/drive-evidence.{json,md}):
tests/qualification/run-drive.sh \
  --remote qualdrive \
  --state-dir "$QUAL_WORK/state" \
  --rclone-config "$QUAL_WORK/rclone.conf" \
  --password-command "/path/to/credential-helper --account qual"
```

**Owner Drive checklist (plan §8, §15, §20):**

- [ ] **Shared parent & folder IDs:** Confirm `rclone lsjson --dirs-only` returns unambiguous folder IDs for `Data/<collection>` and `Recovery/<collection>` without `shared_with_me`.
- [ ] **Two-account ownership, quota & recovery moves:** Using two separately authenticated accounts (`--remote` and `--remote-b`), verify list, upload, overwrite, deletion, and moving another account's overwritten/deleted file into `Recovery/<collection>/<device>/<run>`. Confirm which account's Google One / Drive quota is charged (plan §8).
- [ ] **Three independent client states on Drive:** Run distinct edits, same-file conflict (`--conflict-resolve none --conflict-loser num`), edit/delete, edit/rename, Drive web edit, delayed upload, overlapping runs, and third-client offline return across multiple follow-up cycles; verify every truth-set SHA-256.
- [ ] **Drive object policy:** Place a Google Doc/Sheet and a Drive shortcut in the disposable collection; confirm `--drive-skip-gdocs` and `--drive-skip-shortcuts` skip them without traversing shortcut targets or corrupting listings.
- [ ] **OAuth & encrypted config refresh:** Verify token refresh persists cleanly to the encrypted `--config` via `--password-command`, and verify reauthorization after several days and after token revocation (`AuthenticationRequired` hold).
- [ ] **Rate limiting & network loss:** Observe HTTP 403/429 throttling and network disconnect mid-run; confirm bounded backoff / recovery holds without data loss.
- [ ] **Scale benchmark (representative subset toward 20 TB):** Measure file count, no-change scan time, changed-file scan time, memory use, API throttling, and recovery-directory growth before enabling automatic scheduling on large collections (plan §13).

### 2. Physical External Disk & Unplug Tests (Owner Only — Plan §11, §20)

Perform on a dedicated disposable APFS external drive (e.g., `/Volumes/TestSSD/QualData` with sibling `/Volumes/TestSSD/FolderSyncRecovery`) while keeping an independent truth copy on the internal disk:

- [ ] **Absent before launch:** Unmount the disk before launching FolderSync; confirm mapping enters `VolumeUnavailable` (`Waiting for <Volume>`), never auto-creates `/Volumes/<Name>`, and never deletes cloud files.
- [ ] **Wrong disk with identical display name:** Mount a second USB disk given the exact same volume display name (`/Volumes/TestSSD`); confirm persistent volume UUID binding rejects it and holds safely.
- [ ] **Reconnect preserves manual pause:** Pause the mapping, eject the disk, reconnect the verified disk; confirm `Intent::Paused` remains `Paused` (never auto-resumes).
- [ ] **Mid-run unplug matrix (each phase separately):** Physically disconnect the external disk during:
  1. Before listing (immediately at run start)
  2. During listing
  3. After listing (before transfers)
  4. During upload (local → Drive)
  5. During download (Drive → local)
  6. During deletion propagation
- [ ] **Post-unplug audit:** Reconnect the disk, inspect `state.sqlite`, run logs, opaque `bisync` workdir, and compare every file on both endpoints and in `Recovery/` against external truth-set SHA-256 hashes across two clean follow-up cycles. Per plan §11 and §15, if any phase causes silent data loss or unacceptable resurrection, keep automatic sync on removable roots unsupported until resolved.

### 3. Packaged Desktop App & Finder Sync Acceptance (Owner Only — Plan §19, §20)

Perform on a Developer-ID-signed and notarized `FolderSync.app` built via `packaging/build-app.sh --sign ...` and `packaging/notarize.sh` (see [`docs/release.md`](release.md)):

- [ ] **Fresh install on a second Mac:** Install to `/Applications` on a Mac without Rust/Xcode CLT; verify Gatekeeper accepts the app and `spctl --assess` passes.
- [ ] **Lifecycle & shutdown:** Verify startup, window close-to-background (keep-alive), tray menu actions, `Command-Q`, native Quit, macOS logout/restart interruption, launch-at-login preference, and second-instance exclusion (`manager.lock`).
- [ ] **Installed permissions:** Verify filesystem access from the installed `.app` and `Contents/Helpers/rclone` to ordinary local directories, protected macOS folders, and external APFS volumes.
- [ ] **Finder Sync extension:** Enable `FolderSync Finder` in System Settings; verify root-only badges (`synced`, `syncing`, `attention`, `paused`, `disk_missing`, `stale`), zero badges on child files/subfolders, and interactive context menu clicks (`Sync now`, `Pause`, `Open FolderSync`, `Show last run`) including host relaunch when the app is closed.

### 4. Remaining Local / Cross-Platform Matrix Items

- **Local edge cases still TODO:** disk full and recovery-location full, unreadable directory permissions, corrupt/missing workdir and stale `.lck` recovery, Unicode normalization and case-only rename collisions, symlink rejection/exclusion policy, and very small mappings (<=9 files) / reviewed bulk deletion workflows.
- **Linux (`linux-amd64`):** verify `rclone-adapter` local integration, `/proc/<pid>/stat` process identity, secret-store behavior, and GTK desktop packaging on Ubuntu 24.04+.
