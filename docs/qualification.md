# Qualification

FolderSync qualifies synchronization behavior against pinned tool versions recorded in [`packaging/qualification-manifest.toml`](../packaging/qualification-manifest.toml) (`rclone 1.76.0`, `NSH 0.6.1`, `Rust 1.98.1`). See [`docs/engine-profile.md`](engine-profile.md) for the exact `rclone 1.76.0` argument profile and observed engine quirks.

Local-to-local fixtures verify orchestration, state persistence, multi-client reconciliation on local filesystems, deletion guards, and interruptions. The packaged app uses the live manager, bundled rclone helper, and Finder status handoff. Real Google Drive, physical unplug, and installed-app acceptance still require the owner checks below.

## Automated Local-to-Local Qualification

### 1. Engine adapter integration (`crates/rclone-adapter/tests/local_bisync.rs`)

Verified locally on macOS `arm64` (`osx-arm64`) inside fresh temporary directories with throwaway empty `--config` files, cleared environments, external truth sets, SHA-256 verification, and second-cycle checks:

- Pinned binary SHA-256 + isolated `version` probe qualification, plus a launch-boundary digest check that refuses a binary replaced after qualification.
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

The harness generates deterministic file contents (`--seed`) and saves each
version outside the sync trees. It isolates `HOME`, `FOLDERSYNC_STATE_DIR`,
`--state-dir`, and `--finder-dir` in a temporary workspace and pauses
automatic scheduling in each client state. Three independent clients share
one local remote fixture. The harness writes multi-cycle hash evidence to the
following files.

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

### 1. Real Google Drive qualification

Create a disposable Drive parent with immediate `Data` and `Recovery`
folders. Keep production files outside that parent. Copy the parent's ID
from its Drive URL. Follow [Google Drive accounts](accounts.md) to configure
a desktop OAuth client. For a two-account trial, share the disposable parent
with Account B as an editor and authorize that account for client B. A and C
always use Account A. A single-account trial uses Account A for all three
independent clients and omits cross-account recovery.

1. **Set up once from Terminal in the macOS GUI session.** Use the repository
   checkout you plan to keep for the nightly job. The qualification home must
   be a new directory with an existing parent. Setup creates it with mode
   `0700`, copies the release CLI into its `bin/`, and prompts once for the
   OAuth client secret with terminal echo disabled. Follow the three browser
   prompts in order, selecting the account named by each prompt. Setup
   stores encrypted rclone configs in the private client states, checks the
   parent, and writes `FOLDERSYNC-QUALIFICATION-DISPOSABLE` in its `Data/`.

   ```bash
   scripts/fetch-rclone.sh
   cargo build --release -p sync-cli

   QUAL_HOME="$HOME/Library/Application Support/FolderSync-Qual"
   PARENT_ID='<disposable-parent-folder-id>'
   OAUTH_CLIENT_ID='<desktop-oauth-client-id>'
   tests/qualification/drive-setup.sh --qual-home "$QUAL_HOME" \
     --parent-id "$PARENT_ID" --client-id "$OAUTH_CLIENT_ID" --account-b
   ```

   Omit `--account-b` for a single-account trial. Keychain access is tied to
   the exact installed helper executable. When changing the CLI build,
   uninstall the old nightly job, run setup again in a fresh qualification
   home, and reinstall the job using that home. Keep the previously generated
   evidence while deciding whether to retire the previous setup.

2. **Preview the next run.** The preview reads the qualification home and
   prints planned fixture names. It launches no cloud or credential tools.

   ```bash
   tests/qualification/run-drive.sh --qual-home "$QUAL_HOME" --dry-run
   ```

3. **Run once interactively.** This checks that the installed helper can read
   the login Keychain and that the owner workflow can access the disposable
   parent. Each run uses fresh `Data/FolderSyncQual-<run-id>-…` collections,
   fresh mappings and local trees, and paused global scheduling in all
   three persistent client states. The runner checks the sentinel before
   each scenario, removes the run's mappings, and moves the recorded
   collection IDs to Drive trash.

   ```bash
   tests/qualification/run-drive.sh --qual-home "$QUAL_HOME"
   ```

   To rerun the three overlap, duplicate-name, and shortcut scenarios:

   ```bash
   tests/qualification/run-drive.sh --qual-home "$QUAL_HOME" \
     --only drive_delayed_upload_overlap,drive_duplicate_names,drive_native_document_and_shortcut
   ```

   Add `--dry-run` to preview the same selection. Preflight and cleanup
   still run. The summary and evidence identify the subset run and list
   the skipped scenarios. A successful subset reports `PASS (subset)`.
   The nightly job runs the full suite for the configured accounts.

   The cleanup row in `drive-evidence.json` lists `recorded_fixture_ids`
   and the `drive_trash` result for each ID. Cleanup requires the sentinel
   and the original `Data` folder ID for each fixture, verifies its direct
   parent immediately before the trash request, and checks Drive's
   confirmation. Keep the disposable parent free of manual moves or
   edits during the run. Drive's update endpoint provides no atomic
   parent precondition. Recovery trees under `Recovery/<collection-id>`
   remain available for the owner checklist.
   [Drive trash guide](https://developers.google.com/workspace/drive/api/guides/delete#move_a_file_to_the_trash)

4. **Install the nightly job from the same GUI session.** The default is
   02:00 local time. The lock covers both scheduled and manual runs and
   prevents overlap. The nightly run retains the last 14 completed local
   evidence directories by default. The login Keychain has to be unlocked
   and the agent runs in the logged-in GUI user's Aqua session. An
   SSH-only login lacks that GUI security session and can fail with
   `errSecInteractionNotAllowed`. The job is eligible when the owner is
   logged in. Keep the checkout, pinned rclone, Python, and qualification home
   at their installed locations.

   ```bash
   tests/qualification/nightly/install-nightly.sh --qual-home "$QUAL_HOME" \
     --keep 14 --hour 2 --minute 0
   # To remove the scheduled job later
   tests/qualification/nightly/uninstall-nightly.sh
   ```

5. **Read the latest evidence.** The Markdown summary shows each scenario's
   result and observed discrepancies. The JSON includes full run IDs,
   checkpoints, expected hashes, exact byte locations, and recorded folder
   IDs. `truth/` holds fixture versions and `cloud-checks/` holds downloaded
   copies outside all sync trees. A run returns 0 for `PASS` or
   `PASS (subset)`, 3 for `FAIL` or `INCOMPLETE`, and 2 for input and
   infrastructure errors. Inspect
   `nightly.log` when the scheduled runner cannot start or publish evidence.

   ```bash
   RUN_ID="$(cat "$QUAL_HOME/evidence/latest")"
   cat "$QUAL_HOME/evidence/$RUN_ID/drive-evidence.md"
   ```

| Scenario | Evidence to review |
| --- | --- |
| Preflight | Disposable sentinel, configured parent, independent client IDs, and paused scheduling. |
| Distinct edits | Different-file edits converge. Previous bytes are recoverable. |
| Same-file conflict | Both edits persist as conflict copies and the baseline remains recoverable. |
| Edit versus delete | The competing edit survives and the pre-delete bytes remain recoverable. |
| Offline return | A stale client preserves its competing edit without resurrecting an unrelated deletion. |
| Deletion guard | Four local deletions stop the mapping and preserve the Drive inventory. |
| Interruption | A mid-transfer SIGINT leaves a partial batch, diagnostic workdir, and recovery hold. |
| Rename versus edit | The renamed baseline and the edited version both survive. |
| Drive-side edit | An upload through the connected account, outside a client, propagates and preserves the earlier bytes. |
| Delayed upload | Either edit can become the live file on Drive and both clients. The evidence records the winner and where to find the overwritten version. |
| Duplicate names | Both intentionally duplicated Drive files retain their IDs and bytes. The local projection is recorded. |
| Native document and shortcut | The Sheet export remains byte-stable, the shortcut retains its original target, and both stay out of local sync trees. |
| Same-name folder replacement | Mappings enter `EndpointChanged`. Hashes verify the original folder, the new folder, and pending local changes remain separate. |
| Cross-account recovery | Account B can read A's replaced and deleted bytes in the Drive recovery directory. |
| Cleanup | This run's mappings are removed and every recorded fixture ID is confirmed in recoverable Drive trash. |

Reconciliation scenarios verify SHA-256 through two follow-up cycles. Hold
scenarios verify inventories on repeated attempts. An unobserved overlap,
interruption, or other required check appears as `INCOMPLETE`. Client B has a
bounded wait for collections just created by A. The runner records the
elapsed time and number of listings.

When the peer's sync overwrites the delayed upload on Drive, the overwritten
version is kept in the Drive `Recovery` folder. The delayed-upload scenario
also accepts a verified recovery copy on a client. Check
`overlap.loser_location` for the actual copy and `overlap.winner` for the
version that stayed live. The baseline must also be recoverable.

**Existing disposable import mode.** The legacy entry point still accepts
`--remote`, optional `--account-b`, `--rclone-config`, and a fresh
`--state-dir`. Use an existing, private `0600` Drive export rooted at the
disposable parent. Both remotes must point to that parent. The parent needs
the same sentinel as persistent mode. Use setup to place it, or upload a
regular file named `FOLDERSYNC-QUALIFICATION-DISPOSABLE` to the parent's
`Data/`, with two lines (the filename, then the parent ID) and a final
newline. For a preview, use the legacy arguments with `--dry-run`.

**Owner spot checks and §15 release decision**

- [ ] Define the intended workload and tolerance for conflicts. Review the
  scripted results for one active editor, distinct edits, same-file changes,
  and stale-client return, including any delayed loss or resurrection.
- [ ] Open and restore one verified recovery copy. Confirm Drive ownership
  and quota attribution when two accounts participate.
- [ ] In independent disposable trials, change sharing permissions, revoke
  OAuth authorization, and observe quota exhaustion and throttling. Record
  the behavior and verify that existing data remains recoverable.
- [ ] Proceed to the household pilot if the observed behavior fits the
  intended workload. Block unattended release for workloads with repeatable
  loss or unacceptable resurrection. If arbitrary concurrent edits require
  strict losslessness, pick a different engine or explicitly expand the
  architecture. Use a live collaborative editor for shared documents.

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
