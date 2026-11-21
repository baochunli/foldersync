# State and recovery

The app owns private local mapping, configuration generation, run, and recovery
records on the internal system disk. Each mapping generation keeps its own
opaque rclone bisync work directory and frozen filter file. Preserve that work
directory after an interrupted run and keep recovery copies separate from live
sync trees.

After a crash or disconnect, revalidate endpoints and volume identity before
continuing. A missing or damaged work directory, a deletion-guard abort, an
interrupted run, or a change to a mapping's identity requires an explicit
recovery decision. Initialization and resync are deliberate actions.

## Implemented state dimensions (`sync-core`)

`sync_core::state::MappingState` models readiness, execution, and user intent
as three separate dimensions:

| Dimension | Implemented states |
| --- | --- |
| `Readiness` | `Unconfigured`, `NeedsInitialization`, `Ready`, `VolumeUnavailable`, `AuthenticationRequired`, `EndpointChanged`, `RecoveryRequired` |
| `Execution` | `Idle`, `Queued`, `Running`, `Stopping`, `Interrupted` |
| `Intent` | `Enabled`, `Paused` |

## User actions

| Action | Implemented transition |
| --- | --- |
| `CloseWindow` | No mapping state change; manager continues running. |
| `Pause` | Sets `Intent::Paused`; cancels a `Queued` run to `Idle` but lets a `Running` or `Stopping` run finish. |
| `Resume` | Sets `Intent::Enabled`; transitions `Idle` to `Queued` (`ScheduleRun`) only when `Readiness::Ready` (while automatic scheduling is disabled in Group 3, `sync-manager` keeps `Execution::Idle`). |
| `RunNow` | Transitions `Idle` to `Queued` (`ScheduleRun`) only when `Ready` and `Enabled`. |
| `StopRun` | Moves `Running` to `Stopping` (`RequestGracefulStop`) or `Queued` to `Idle`. |
| `ForceStop` | Moves `Running` or `Stopping` to `Execution::Interrupted` and `Readiness::RecoveryRequired` (`ForceTerminateChild`). |
| `QuitRequested` | Stops scheduling (`StopScheduling`); moves `Running` to `Stopping` (`StopSchedulingAndRequestGracefulStop`) and drops `Queued` back to `Idle` without altering `Intent`. |

## Observed outcomes and recovery invariants

| Outcome (`RunOutcome`) | Implemented transition |
| --- | --- |
| `Success`, `SuccessWithConflicts`, `RetryableFailure` | `Execution::Idle`; preserves current `Readiness` and `Intent`. |
| `AuthenticationFailure` | `Execution::Idle`; activates the `AuthenticationRequired` hold while preserving other unresolved holds. |
| `VolumeMismatch` | `Execution::Idle`; activates only the `VolumeUnavailable` hold (reconnecting the verified known volume clears it without extra endpoint approval). |
| `EndpointMismatch` | `Execution::Idle`; always activates the `EndpointChanged` hold (even while `VolumeUnavailable` is active); only `EndpointValidated` clears it. |
| `InvalidConfiguration`, `CriticalFailure`, `UnknownFailure`, `CorruptState` | `Execution::Idle`; latches `Readiness::RecoveryRequired`. |
| `Interrupted` | `Execution::Interrupted`; latches `Readiness::RecoveryRequired`. |

Key enforced invariants:

- Unresolved readiness gates (`NeedsInitialization`, `AuthenticationRequired`, `EndpointChanged`, `VolumeUnavailable`, `RecoveryRequired`) remain active independently until each gate's own check passes.
- Reconnecting a volume (`ObservedEvent::VolumeAvailable`) never clears `Intent::Paused`.
- No transition clears `Intent::Paused` except `UserAction::Resume`.
- Once `Readiness::RecoveryRequired` is set, no user action or other event clears it except `ObservedEvent::RecoveryCompleted`.
- `status_text` reports explicit household status (`"Waiting for <volume>"`, `"Scanning <name>"`, `"Last completed at HH:MM"`, `"Completed with conflicts"`, `"Recovery required"`) and never claims `"Everything is up to date"`.

## Durable state layout and exclusive locking (`sync-manager`)

`sync-manager` resolves the state directory through platform APIs (macOS
`~/Library/Application Support/FolderSync`, Linux XDG state/data directory),
always overridable via constructor configuration (`ManagerConfig::state_dir`)
or `FOLDERSYNC_STATE_DIR` (tests and CLI fixtures use temporary directories and
never touch the real application directory).

- **Private permissions:** Directories under the state directory use `0700`;
  SQLite database, lock file, account configs, frozen filters, run artifacts,
  and Finder snapshots use `0600`.
- **Single-instance OS lock:** `StateLock::acquire` takes a non-blocking
  exclusive `flock` on `<state_dir>/manager.lock` before opening SQLite or
  creating subdirectories. A second GUI or `sync-cli` instance fails
  immediately with an actionable `StateLocked` error.
- **On-disk layout:**
  - `manager.lock` — exclusive OS `flock` file.
  - `state.sqlite` — bundled SQLite database (WAL mode, `synchronous = FULL`,
    foreign keys enabled) with forward-only `schema_migrations` and the §6
    tables: `devices`, `accounts`, `mappings`, `mapping_generations`, `runs`,
    and `recovery_cases` (no per-file sync table). Opening a database with a
    newer schema version fails rather than downgrading silently.
  - `accounts/<id>/rclone.conf` — app-owned per-account configuration.
  - `mappings/<id>/generation-<N>/filters.txt` — frozen exclusion filters.
  - `mappings/<id>/generation-<N>/bisync/` — opaque rclone bisync workdir.
  - `runs/<run-id>/` — `argv.json` (sanitized argv + engine identity),
    `output.ndjson` (captured rclone output), and `outcome.json`.
  - `finder/status.json` (or configured `--finder-dir`) — atomically written
    (temp + `sync_all` + rename) v1 Finder status snapshot matching ADR 0002.

## Run records, crash recovery, and orphan reconciliation

1. **Pre-spawn persistence:** Before spawning rclone, `sync-manager` inserts a
   `runs` row (`status = "starting"`, `last_verified_phase = "pre_spawn"`)
   containing the run ID, generation ID, workdir path, and unique per-run
   `backup_dir1`/`backup_dir2` recovery paths.
2. **Post-spawn child identity:** Immediately after spawn, `sync-manager`
   updates the run row with `child_pid`, OS process start time
   (`proc_pidinfo(PROC_PIDTBSDINFO)` on macOS, `/proc/<pid>/stat` on Linux),
   executable path (`proc_pidpath` / `/proc/<pid>/exe`), and verified engine
   identity.
3. **Startup reconciliation:** On every `SyncManager::open`, unfinished `runs`
   rows (`starting` or `running`) are reconciled against live OS processes:
   - **Missing PID (crash between spawn and PID record):** Ownership is
     ambiguous (`reconciled_ambiguous`). The mapping is latched to
     `Execution::Interrupted` + `Readiness::RecoveryRequired` with an open
     `orphan_ambiguous` recovery case.
   - **PID alive with matching start time and executable:** Recorded as a live
     orphan (`reconciled_orphan_running`, `orphan_running` case with
     `orphan_pid`), blocking any second job. FolderSync never kills processes
     by name.
   - **PID alive with different start time or executable (reused PID):** The
     reused PID is ignored and never signaled (`reconciled_pid_reused`); the
     mapping enters `RecoveryRequired` with `orphan_pid = None`.
   - **PID gone:** Run is marked interrupted (`reconciled_exited`), preserving
     the workdir and opening a recovery case.
4. **Durable holds and intent:** `Pause`, `Resume`, `ForceStop`, and run
   outcomes (including deletion-guard aborts, recorded as
   `deletion_guard_aborted` recovery cases) are persisted to SQLite before
   acknowledging commands or publishing snapshots.
5. **Single global mutating job:** `SyncManager` runs at most one mutating
   rclone job globally on its dedicated worker thread, publishing immutable
   `StatusSnapshot` updates and a bounded event stream without assuming any UI
   async runtime.

## Initialization and manual Run once

- **Initialization (`Initialize` / `sync-cli init <mapping>`):** Validates the
  draft mapping through `sync-core`, inspects both endpoints without following
  symlinks, validates filenames via `sync_core::validate_names`, refuses
  unrecognized `RCLONE_TEST` files without overwriting them, and classifies
  each side as empty or populated:
  - Local has data, remote empty → upload initialization (`--resync-mode path1`).
  - Remote has data, local empty → download initialization (`--resync-mode path2`).
  - Both sides populated → refused with an actionable §9 message advising a new
    empty destination or external consolidation.
  - Creates or verifies matching `AccessMarker` (`RCLONE_TEST`) files, freezes
    the `ConfigGeneration` and its SHA-256 fingerprint, runs rclone through
    `rclone-adapter`, and marks the mapping `Ready` on success while keeping
    automatic scheduling disabled (scheduling lands in Group 4).
- **Run once (`RunNow` / `sync-cli run-once <mapping>`):** Verifies readiness,
  intent, and local root presence (never auto-creating missing roots), launches
  a validated normal bisync run with per-run backup directories, feeds the
  classified `rclone-adapter` outcome into `sync-core`, persists state, and
  updates both `StatusSnapshot` and Finder `status.json`.

## CLI companion (`sync-cli`)

`sync-cli` uses the same `SyncManager` and exclusive state lock:

- `sync-cli status [--state-dir <dir>] [--json]`
- `sync-cli add-mapping --id <id> --local-dir <dir> --remote-dir <dir> --recovery-dir <dir> [--remote-recovery-dir <dir>] [--state-dir <dir>]`
- `sync-cli init <mapping> [--state-dir <dir>]`
- `sync-cli run-once <mapping> [--state-dir <dir>]`
- `sync-cli pause <mapping> [--state-dir <dir>]`
- `sync-cli resume <mapping> [--state-dir <dir>]`

## Remaining verification TODO

Real Google Drive initialization, encrypted credential helpers, external-volume
mount/unmount probing, and automatic scheduling remain to be wired and
qualified in Group 4+.
