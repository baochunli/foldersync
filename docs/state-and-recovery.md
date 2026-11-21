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
or `FOLDERSYNC_STATE_DIR`, normalizing overrides to canonical absolute paths
before creating any directory or marker. Tests and the fixture `sync-cli`
always require an explicit non-empty `--state-dir` and never open or create
the real application directory.

- **Safe path identifiers, containment & symlink refusal:** Mapping, account,
  device, and run IDs are validated as single path components
  (`[A-Za-z0-9][A-Za-z0-9_-]*`) and every derived directory or file path under
  the state root (`accounts`, `mappings`, `filters.txt`, `bisync`, `runs`,
  `argv.json`, `output.ndjson`, `outcome.json`) is verified to remain strictly
  inside its root with no symbolic link component (`symlink_metadata` /
  `O_NOFOLLOW`) before any directory creation, chmod, or file write.
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
  - `runs/<run-id>/` — `argv.json` (sanitized argv + engine identity, written
    only after child identity is committed), `output.ndjson` (valid NDJSON
    streamed live to disk during the run and atomically finalized from the
    adapter's lossless retained output buffer up to its bounds, wrapping plain
    or truncated lines as JSON objects), and `outcome.json` (written before
    each terminal commit on completion, pre-spawn abort, launch failure, child
    identity failure, and startup reconciliation, with startup repair rewriting
    any missing `outcome.json` for committed terminal rows).
  - `finder/status.json` (or configured `--finder-dir`) — atomically written
    (temp + `sync_all` + rename) v1 Finder status snapshot matching ADR 0002,
    refusing symlinked handoff directories or symlinked parent paths. Reflects
    global scheduling pause (`paused` / `"Scheduling paused"`) and refreshes
    `generated_at` on a periodic heartbeat well under the 300-second TTL while
    running. Idle worker wakeups preserve the next refresh deadline. After
    every successful `status.json` write, `sync-manager` invokes
    an injectable `FinderNotifier` (`NoopFinderNotifier` by default in
    `ManagerConfig::with_state_dir` so tests never post live notifications;
    `DarwinFinderNotifier` in the real interactive desktop app; tests inject
    `RecordingFinderNotifier`). Finder publication failures are logged as
    non-fatal warnings and never suppress stop signals or undo committed state.
  - `finder/actions/*.json` — ADR 0002 v1 Finder action files (`sync_now`,
    `pause`, `open_app`, `show_last_run`) drained by the manager/UI on a
    background thread. Symlinks are refused (`symlink_metadata` / `O_NOFOLLOW`),
    temporary/hidden files are ignored, unknown JSON fields and invalid IDs or
    timestamps are rejected, and each file is atomically claimed via `rename`
    to a hidden `.processing-*` name before dispatch (unclaimable files are
    rejected without dispatch) and deleted only after dispatch completes.

## Desktop UI wiring, recovery inspection, and controlled quit (`apps/sync-ui`)

- **Real manager by default (`ManagerSource`):** `apps/sync-ui` defaults to
  `ManagerSource` for interactive app runs, owning `SyncManager` strictly on a
  dedicated background worker thread (`foldersync-ui-manager`). Immutable
  `StatusSnapshot` updates flow through a coalescing latest-snapshot slot
  (never replaying older snapshots or queuing commands behind log output) and
  are marshaled onto the NSH main thread via `run_on_main_thread`. Headless
  test opens without an explicit `FOLDERSYNC_STATE_DIR` and runs with
  `FOLDERSYNC_DEMO=1` or `--demo` use the in-memory `DemoSource`.
- **Off-mutex & background preflight:** Initialization layout/inventory checks
  and engine qualification run off the manager state mutex on the background
  worker thread (`StartInitialize`) with per-attempt IDs so a cancelled attempt
  never writes or hijacks a later Start. Snapshot rebuilds use bounded
  per-mapping latest-run queries (`LIMIT 1`), and the exclusive OS `flock`
  stays held until after the worker thread joins and SQLite closes.
- **Run-specific inspection & safe local recovery copies:** Each historical run
  opens its own run-specific log (`output.ndjson`) and lists local recovery
  copies preserved on disk (`backup_dir1` and local-fixture `backup_dir2`;
  remote Google Drive recovery copy enumeration is not implemented in this
  build). Copying a recovery file (`Copy to inspection folder…`) checks path
  types with `lstat` before canonicalizing, refuses symlinked recovery roots
  and inspection directories, writes with `create_new` into a user-chosen
  separate folder, and refuses any destination overlapping a live synced
  mapping root.
- **Unreviewed conflict persistence:** `mappings.unreviewed_conflict_run_id`
  (SQLite schema v2, backfilling existing `SuccessWithConflicts` mappings on
  upgrade) tracks unreviewed conflicts independently of `Pause`, `Resume`, or
  restart; acknowledging conflicts clears only the review notice while
  preserving `RunOutcome::SuccessWithConflicts` in run history.
- **Main-thread desktop actions & injectable `SystemActions`:** In the real
  desktop app, Reveal in Finder, Open URL (restricted to `https://` and
  `x-apple.systempreferences:`), and Launch at Login are sent over a channel
  to execute strictly on the NSH main thread and report their actual outcomes
  back to the worker; tests and demo mode inject `RecordingSystemActions`.
- **Single controlled-stop Quit:** Tray Quit, ⌘Q, and `on_quit_request` share
  one handler that prompts when a run is active, displays an honest `"Stopping…"`
  state, pauses scheduling, requests graceful stop, waits up to the bounded
  timeout, persists `Interrupted` / `RecoveryRequired` (`shutdown_timeout`) if
  the child does not exit before timeout (blocking new jobs on restart while
  that child remains alive), and delivers `QuitReady` without blocking on a
  hung child thread.

## Run records, crash recovery, and orphan reconciliation

1. **Pre-spawn persistence & no-gap launch:** Before spawning rclone,
   `sync-manager` atomically dequeues, rechecks eligibility (pause, stop,
   shutdown, global orphan block, storage-blocked mappings), claims the active
   job slot, inserts a `runs` row (`status = "starting"`,
   `last_verified_phase = "pre_spawn"`) with workdir and per-run
   `backup_dir1`/`backup_dir2` recovery paths, and qualifies the rclone engine
   and `BisyncRequest`. Immediately before spawn—holding the manager state
   mutex across the final stop/force-stop/shutdown check, final root and
   `RCLONE_TEST` access-marker content verification, child spawn, and child
   identity commit—it aborts without spawning if stopped or if either root or
   marker changed.
   - *Known limitation (plan §11 residual race):* Filename-based access-marker
     checks (`RCLONE_TEST`) cannot detect an atomic same-path directory
     replacement that carries a copy of the recognized marker content, nor an
     OS-level swap racing after spawn starts; external volume UUID/identity
     probing in Group 4+ narrows this further.
2. **Post-spawn child identity:** Immediately after spawn, `sync-manager`
   commits `child_pid`, OS process start time (`proc_pidinfo(PROC_PIDTBSDINFO)`
   on macOS, `/proc/<pid>/stat` on Linux), executable path (`proc_pidpath` /
   `/proc/<pid>/exe`), and verified engine identity to SQLite **before**
   writing `argv.json`. If committing child identity fails, `sync-manager`
   immediately force-stops the child, skips `argv.json`, writes `outcome.json`,
   and records an `Interrupted` / `RecoveryRequired` outcome.
3. **Startup reconciliation:** On every `SyncManager::open`, unfinished `runs`
   rows (`starting` or `running`) are reconciled against live OS processes,
   writing `outcome.json` and committing atomically in one SQLite transaction
   (run terminal status + mapping hold + recovery case), and rewriting any
   missing `outcome.json` on previously committed terminal rows:
   - **Missing PID (crash between spawn and PID record):** Investigates
     surviving processes referencing the run's exact workdir and workdir `.lck`
     evidence, recording `orphan_ambiguous` (`reconciled_ambiguous`) and
     blocking the **global** worker until ownership is positively resolved.
   - **PID alive with matching start time and executable:** Recorded as a live
     orphan (`reconciled_orphan_running`, `orphan_running` case with
     `orphan_pid`), blocking the **global** worker from launching any second
     job on any mapping. FolderSync never kills processes by name.
   - **PID alive with different start time or conclusively different executable
     (reused PID):** The reused PID is ignored and never signaled
     (`reconciled_pid_reused`); the mapping enters `RecoveryRequired` with
     `orphan_pid = None`.
   - **Inconclusive probe or identity comparison (e.g. permission denied,
     ` (deleted)` executable suffix, or canonicalization failure):** Treated as
     possibly alive (`orphan_still_alive = None`, `orphan_ambiguous`) and
     continues to block the global worker.
   - **PID gone:** Run is marked interrupted (`reconciled_exited`), preserving
     the workdir and opening a recovery case.
4. **Durable holds and intent:** `Pause`, `Resume`, `StopRun`, `ForceStop`, and
   run outcomes (including deletion-guard aborts, recorded as
   `deletion_guard_aborted` recovery cases) commit to SQLite **before**
   mutating in-memory control flags, acknowledging commands, or publishing
   snapshots. Terminal DB commit failures propagate to `wait_until_idle`
   callers and keep the mapping blocked. Stop signals are delivered before
   Finder snapshot publication.
5. **Single global mutating job:** `SyncManager` runs at most one mutating
   rclone job globally on its dedicated worker thread, publishing immutable
   `StatusSnapshot` updates and a bounded event stream without assuming any UI
   async runtime.

## Initialization and manual Run once

- **Initialization (`Initialize` / `sync-cli init <mapping> --state-dir <dir>`):**
  Validates the complete layout (Path1, Path2, both recovery roots, state dir,
  Finder dir, and other mappings) through `sync-core` and manager layout checks
  before creating any directory or marker; inspects both endpoints without
  following symlinks (preserving literal backslashes for core name validation,
  rejecting special files such as FIFOs/sockets, and honoring explicit
  exclusions for symlinks); detects existing `RCLONE_TEST` case-insensitively
  and creates new markers strictly with `create_new` (`O_EXCL`, never
  truncating); refuses both-populated trees with the §9 message; freezes the
  `ConfigGeneration` and its SHA-256 fingerprint; runs rclone through
  `rclone-adapter`; and marks the mapping `Ready` on success while keeping
  automatic scheduling disabled (scheduling lands in Group 4).
- **Run once (`RunNow` / `sync-cli run-once <mapping> --state-dir <dir>`):**
  Re-verifies both roots against the stored generation and checks `RCLONE_TEST`
  contents on both sides right before launch (never auto-creating missing roots
  or markers), launches a validated normal bisync run with per-run backup
  directories, feeds the classified `rclone-adapter` outcome into `sync-core`,
  commits atomically, and updates both `StatusSnapshot` and Finder
  `status.json` (reporting `"Last run failed — retry manually"` rather than
  `"will retry"` while scheduling is disabled).

## CLI companion (`sync-cli`)

`sync-cli` uses the same `SyncManager` and exclusive state lock. In Group 3,
all fixture commands require a non-empty `--state-dir <dir>` (never falling
back to the real application state directory), and `init` / `run-once` exit
non-zero on any failure outcome (including retryable failures):

- `sync-cli status --state-dir <dir> [--json]`
- `sync-cli add-mapping --state-dir <dir> --id <id> --local-dir <dir> --remote-dir <dir> --recovery-dir <dir> [--remote-recovery-dir <dir>]` (defaults `--remote-recovery-dir` to a sibling `<recovery-dir>-remote` directory)
- `sync-cli init <mapping> --state-dir <dir>`
- `sync-cli run-once <mapping> --state-dir <dir>` (ties exit status to the newly started run ID and exits non-zero on any run failure or terminal DB commit error)
- `sync-cli pause <mapping> --state-dir <dir>`
- `sync-cli resume <mapping> --state-dir <dir>`

## Remaining verification TODO

Real Google Drive initialization, encrypted credential helpers, external-volume
mount/unmount probing, and automatic scheduling remain to be wired and
qualified in Group 4+.
