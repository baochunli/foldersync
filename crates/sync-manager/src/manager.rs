//! Core `SyncManager` orchestrator: durable commands, single global mutating
//! job execution, crash/orphan reconciliation, snapshots, and Finder output.

use std::{
    collections::{BTreeSet, VecDeque},
    fs,
    path::{Component, Path, PathBuf},
    sync::{Arc, Condvar, Mutex, mpsc},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use chrono::DateTime;
use rclone_adapter::{
    BackupDir2, InitPreference, LogEvent, Outcome, OutputLimits, OutputLine, Path2, RunMode,
    SpawnOptions, classify_run_outcome, parse_log_line,
};
use serde::{Deserialize, Serialize};
use sync_core::{
    ACCESS_MARKER_FILENAME, AccountId, CloudCollection, ComparePolicy, ConfigGeneration,
    ConflictPolicy, DEFAULT_ACTIVE_SCHEDULE_INTERVAL, DEFAULT_MAX_DELETE_PERCENT,
    DeviceCloudCollection, DeviceId, EngineCompatibility, Execution, GenerationId, Intent,
    LastRunSummary, LocalBinding, LocalLayoutEntry, MappingId, MappingState, MaxDeletePercent,
    ObservedEvent, PathResolutionError, PathResolver, Readiness, RecoveryPolicy, RunId, RunOutcome,
    StdPathResolver, SupportedTypesPolicy, TransitionEffect, UserAction, VolumeIdentity,
    VolumeKind, apply_event, apply_user_action, available_actions, default_exclusion_patterns,
    status_text, validate_cloud_collections, validate_local_binding, validate_local_layout,
};
use uuid::Uuid;

use crate::{
    clock::{Clock, SystemClock},
    error::ManagerError,
    finder::FinderStatusWriter,
    init::{
        decide_init_direction, establish_local_pair_markers, inspect_local_endpoint,
        render_filters_file,
    },
    launcher::{BisyncLauncher, LaunchPlan, LaunchStopper, RealBisyncLauncher},
    platform::{
        OsProcessInspector, ProcessInspector, ProcessProbeResult, StateLock, ensure_private_dir,
        resolve_state_dir, write_private_file,
    },
    snapshot::{
        DEFAULT_EVENT_BUFFER_CAPACITY, EventBus, ManagerEvent, MappingCounters,
        MappingStatusSnapshot, StatusSnapshot,
    },
    store::{
        AccountRecord, DeviceRecord, GenerationRecord, MappingRecord, RecoveryCaseRecord,
        RunRecord, SqliteStateStore,
    },
};

const ENGINE_PROFILE_VERSION: &str = "v1";

/// Remote endpoint paired with a mapping's local root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RemoteEndpoint {
    /// Local directory fixture used in local-to-local tests and early CLI fixtures.
    LocalDir(PathBuf),
    /// Google Drive remote collection (e.g. `household`, `Data/Research`).
    DriveRemote { remote: String, path: String },
    /// Other rclone remote.
    OtherRemote { remote: String, path: String },
}

impl RemoteEndpoint {
    fn to_db_cols(&self) -> (String, String) {
        match self {
            Self::LocalDir(p) => ("local_dir".to_string(), p.to_string_lossy().into_owned()),
            Self::DriveRemote { remote, path } => {
                ("drive_remote".to_string(), format!("{remote}:{path}"))
            }
            Self::OtherRemote { remote, path } => {
                ("other_remote".to_string(), format!("{remote}:{path}"))
            }
        }
    }

    fn from_db_cols(kind: &str, value: &str) -> Result<Self, ManagerError> {
        match kind {
            "local_dir" => Ok(Self::LocalDir(PathBuf::from(value))),
            "drive_remote" => {
                let (remote, path) =
                    value.split_once(':').ok_or_else(|| ManagerError::Storage {
                        cause: format!("Malformed drive remote endpoint `{value}` in database."),
                        next_step: "Inspect mapping configuration in state.sqlite.".to_string(),
                    })?;
                Ok(Self::DriveRemote {
                    remote: remote.to_string(),
                    path: path.to_string(),
                })
            }
            "other_remote" => {
                let (remote, path) =
                    value.split_once(':').ok_or_else(|| ManagerError::Storage {
                        cause: format!("Malformed remote endpoint `{value}` in database."),
                        next_step: "Inspect mapping configuration in state.sqlite.".to_string(),
                    })?;
                Ok(Self::OtherRemote {
                    remote: remote.to_string(),
                    path: path.to_string(),
                })
            }
            other => Err(ManagerError::Storage {
                cause: format!("Unknown remote_endpoint_kind `{other}` in database."),
                next_step: "Inspect mapping configuration in state.sqlite.".to_string(),
            }),
        }
    }

    fn to_path2(&self) -> Path2 {
        match self {
            Self::LocalDir(p) => Path2::LocalDir(p.clone()),
            Self::DriveRemote { remote, path } => Path2::DriveRemote {
                remote: remote.clone(),
                path: path.clone(),
            },
            Self::OtherRemote { remote, path } => Path2::OtherRemote {
                remote: remote.clone(),
                path: path.clone(),
            },
        }
    }

    fn path2_string(&self) -> String {
        match self {
            Self::LocalDir(p) => p.to_string_lossy().into_owned(),
            Self::DriveRemote { remote, path } | Self::OtherRemote { remote, path } => {
                format!("{remote}:{path}")
            }
        }
    }
}

/// Configuration for opening a [`SyncManager`].
pub struct ManagerConfig {
    /// Explicit state directory override (takes precedence over `FOLDERSYNC_STATE_DIR`).
    pub state_dir: Option<PathBuf>,
    /// Optional Finder `status.json` directory override (defaults to `<state_dir>/finder`).
    pub finder_status_dir: Option<PathBuf>,
    /// Clock implementation (defaults to [`SystemClock`]).
    pub clock: Arc<dyn Clock>,
    /// Live process inspector (defaults to [`OsProcessInspector`]).
    pub process_inspector: Arc<dyn ProcessInspector>,
    /// Sync launcher (defaults to [`RealBisyncLauncher`]).
    pub launcher: Arc<dyn BisyncLauncher>,
    /// Bounded event ring buffer capacity.
    pub event_buffer_capacity: usize,
}

impl ManagerConfig {
    /// Creates a config rooted at an explicit state directory with real OS/rclone defaults.
    pub fn with_state_dir(state_dir: impl Into<PathBuf>) -> Self {
        Self {
            state_dir: Some(state_dir.into()),
            finder_status_dir: None,
            clock: Arc::new(SystemClock),
            process_inspector: Arc::new(OsProcessInspector),
            launcher: Arc::new(RealBisyncLauncher::new(None)),
            event_buffer_capacity: DEFAULT_EVENT_BUFFER_CAPACITY,
        }
    }
}

/// Input for [`ManagerCommand::AddMapping`].
#[derive(Debug, Clone)]
pub struct AddMappingCommand {
    pub id: MappingId,
    pub account_id: AccountId,
    pub collection: CloudCollection,
    pub local_binding: LocalBinding,
    pub remote_endpoint: RemoteEndpoint,
    pub local_recovery_root: PathBuf,
    pub cloud_recovery_path: String,
    pub extra_exclusions: BTreeSet<String>,
    pub max_delete_percent: u8,
    pub schedule_interval: Duration,
}

impl AddMappingCommand {
    /// Builds a validated local-to-local fixture mapping command.
    pub fn local_fixture(
        id: impl Into<MappingId>,
        account_id: impl Into<AccountId>,
        collection_path: impl Into<String>,
        local_dir: impl Into<PathBuf>,
        remote_dir: impl Into<PathBuf>,
        local_recovery_root: impl Into<PathBuf>,
        remote_recovery_root: impl Into<PathBuf>,
    ) -> Result<Self, ManagerError> {
        let local_dir = local_dir.into();
        let remote_dir = remote_dir.into();
        let canonical_local = fs::canonicalize(&local_dir).map_err(|err| ManagerError::Io {
            cause: format!(
                "Local folder `{}` does not exist or cannot be accessed: {err}.",
                local_dir.display()
            ),
            next_step:
                "Select an existing local folder (FolderSync never auto-creates missing roots)."
                    .to_string(),
        })?;
        let canonical_remote = fs::canonicalize(&remote_dir).map_err(|err| ManagerError::Io {
            cause: format!(
                "Remote fixture folder `{}` does not exist or cannot be accessed: {err}.",
                remote_dir.display()
            ),
            next_step:
                "Select an existing remote destination folder (FolderSync never auto-creates missing roots)."
                    .to_string(),
        })?;
        let mount_path = canonical_local
            .parent()
            .ok_or_else(|| ManagerError::PreflightRejected {
                cause: format!(
                    "Cannot use filesystem root `{}` as a mapping folder.",
                    canonical_local.display()
                ),
                next_step: "Choose a dedicated subdirectory instead of a volume root.".to_string(),
            })?
            .to_path_buf();
        let relative_dir = PathBuf::from(canonical_local.file_name().ok_or_else(|| {
            ManagerError::PreflightRejected {
                cause: format!(
                    "Local folder `{}` has no directory name.",
                    canonical_local.display()
                ),
                next_step: "Choose a dedicated subdirectory.".to_string(),
            }
        })?);

        let collection_path = collection_path.into();
        let folder_id = format!("local-col-{}", collection_path.replace('/', "_"));
        Ok(Self {
            id: id.into(),
            account_id: account_id.into(),
            collection: CloudCollection {
                display_path: collection_path,
                folder_id,
                ancestor_ids: vec!["local-root".to_string()],
            },
            local_binding: LocalBinding {
                volume: VolumeIdentity::new(VolumeKind::Internal, "local-fixture-volume"),
                mount_path,
                relative_dir,
            },
            remote_endpoint: RemoteEndpoint::LocalDir(canonical_remote),
            local_recovery_root: local_recovery_root.into(),
            cloud_recovery_path: remote_recovery_root.into().to_string_lossy().into_owned(),
            extra_exclusions: BTreeSet::new(),
            max_delete_percent: DEFAULT_MAX_DELETE_PERCENT,
            schedule_interval: DEFAULT_ACTIVE_SCHEDULE_INTERVAL,
        })
    }
}

/// Input for [`ManagerCommand::Initialize`].
#[derive(Debug, Clone)]
pub struct InitializeCommand {
    pub mapping_id: MappingId,
    pub expected_direction: Option<InitPreference>,
}

/// Explicit manager commands (§3, §16).
#[derive(Debug, Clone)]
pub enum ManagerCommand {
    AddMapping(Box<AddMappingCommand>),
    Initialize(InitializeCommand),
    RunNow { mapping_id: MappingId },
    Pause { mapping_id: MappingId },
    Resume { mapping_id: MappingId },
    StopRun { mapping_id: MappingId },
    ForceStop { mapping_id: MappingId },
    InspectRecovery { mapping_id: MappingId },
}

/// Enriched view of a single recovery case returned by `InspectRecovery`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryCaseView {
    pub case: RecoveryCaseRecord,
    pub orphan_still_alive: Option<bool>,
}

/// Diagnostic inspection report returned by [`ManagerCommand::InspectRecovery`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryReport {
    pub mapping_id: MappingId,
    pub state: MappingState,
    pub open_cases: Vec<RecoveryCaseView>,
    pub workdir_files: Vec<String>,
    pub has_lock_file: bool,
    pub recent_runs: Vec<RunRecord>,
}

/// Structured detail accompanying a durable command acceptance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandOutcome {
    MappingAdded {
        mapping_id: MappingId,
    },
    InitializationQueued {
        mapping_id: MappingId,
        direction: InitPreference,
    },
    RunQueued {
        mapping_id: MappingId,
    },
    Paused {
        mapping_id: MappingId,
    },
    Resumed {
        mapping_id: MappingId,
    },
    StopRequested {
        mapping_id: MappingId,
    },
    ForceStopped {
        mapping_id: MappingId,
    },
    RecoveryInspected(Box<RecoveryReport>),
}

/// Durable acknowledgment returned by [`SyncManager::execute`].
#[derive(Debug, Clone)]
pub struct CommandAcceptance {
    pub outcome: CommandOutcome,
    pub snapshot: Arc<StatusSnapshot>,
}

/// Resolves existing paths directly and non-existing recovery tails via their
/// longest existing ancestor so overlap checks work before recovery dirs are created.
struct AncestorAwarePathResolver;

impl PathResolver for AncestorAwarePathResolver {
    fn canonicalize(&self, path: &Path) -> Result<PathBuf, PathResolutionError> {
        if let Ok(c) = fs::canonicalize(path) {
            return Ok(c);
        }
        let mut normalized = PathBuf::new();
        for comp in path.components() {
            match comp {
                Component::Prefix(p) => normalized.push(p.as_os_str()),
                Component::RootDir => normalized.push(comp.as_os_str()),
                Component::CurDir => {}
                Component::ParentDir => {
                    normalized.pop();
                }
                Component::Normal(part) => normalized.push(part),
            }
        }
        let mut current = normalized.as_path();
        let mut tail = Vec::new();
        loop {
            match fs::canonicalize(current) {
                Ok(mut base) => {
                    for part in tail.into_iter().rev() {
                        base.push(part);
                    }
                    return Ok(base);
                }
                Err(_) => {
                    let Some(name) = current.file_name() else {
                        return Err(PathResolutionError {
                            path: path.to_path_buf(),
                            message: "no existing ancestor directory found".to_string(),
                        });
                    };
                    tail.push(name.to_os_string());
                    let Some(parent) = current.parent() else {
                        return Err(PathResolutionError {
                            path: path.to_path_buf(),
                            message: "no existing ancestor directory found".to_string(),
                        });
                    };
                    current = parent;
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
struct QueuedJob {
    mapping_id: MappingId,
    generation_id: GenerationId,
    mode: RunMode,
}

struct ActiveRunControl {
    mapping_id: MappingId,
    run_id: RunId,
    stopper: Option<Arc<dyn LaunchStopper>>,
    stop_requested: bool,
    force_stopped: bool,
}

struct SharedState {
    store: SqliteStateStore,
    device: DeviceRecord,
    snapshot_seq: u64,
    latest_snapshot: Arc<StatusSnapshot>,
    job_queue: VecDeque<QueuedJob>,
    active_run: Option<ActiveRunControl>,
    worker_busy: bool,
    shutdown: bool,
}

struct ManagerInner {
    state_dir: PathBuf,
    finder_writer: FinderStatusWriter,
    clock: Arc<dyn Clock>,
    process_inspector: Arc<dyn ProcessInspector>,
    launcher: Arc<dyn BisyncLauncher>,
    event_bus: EventBus,
    state: Mutex<SharedState>,
    worker_cv: Condvar,
    idle_cv: Condvar,
}

/// Single-process FolderSync manager owning the state lock, SQLite store,
/// single global sync worker, snapshot stream, and Finder status writer.
pub struct SyncManager {
    _lock: StateLock,
    inner: Arc<ManagerInner>,
    worker_thread: Mutex<Option<JoinHandle<()>>>,
}

impl SyncManager {
    /// Acquires the exclusive OS lock, opens/migrates SQLite, reconciles any
    /// unfinished runs from a previous crash, publishes the initial snapshot
    /// (including Finder `status.json`), and starts the background worker.
    pub fn open(config: ManagerConfig) -> Result<Self, ManagerError> {
        let state_dir = resolve_state_dir(config.state_dir.as_deref())?;
        let lock = StateLock::acquire(&state_dir)?;

        ensure_private_dir(&state_dir.join("accounts"))?;
        ensure_private_dir(&state_dir.join("mappings"))?;
        ensure_private_dir(&state_dir.join("runs"))?;
        ensure_private_dir(&state_dir.join("recovery-records"))?;

        let finder_dir = config
            .finder_status_dir
            .unwrap_or_else(|| state_dir.join("finder"));
        ensure_private_dir(&finder_dir)?;
        let finder_writer = FinderStatusWriter::new(finder_dir);

        let store = SqliteStateStore::open(&state_dir.join("state.sqlite"))?;
        let now = config.clock.now_rfc3339();
        let default_device = DeviceRecord {
            id: DeviceId::new(format!("device-{}", Uuid::new_v4())),
            name: default_device_name(),
            created_at: now,
        };
        let device = store.ensure_device(&default_device)?;

        let event_bus = EventBus::new(config.event_buffer_capacity);
        reconcile_startup_state(
            &store,
            config.process_inspector.as_ref(),
            config.clock.as_ref(),
            &event_bus,
        )?;

        let initial_snapshot = Arc::new(build_snapshot(&store, 1, None, config.clock.as_ref())?);
        finder_writer.write_snapshot(&initial_snapshot)?;

        let inner = Arc::new(ManagerInner {
            state_dir,
            finder_writer,
            clock: config.clock,
            process_inspector: config.process_inspector,
            launcher: config.launcher,
            event_bus,
            state: Mutex::new(SharedState {
                store,
                device,
                snapshot_seq: 1,
                latest_snapshot: Arc::clone(&initial_snapshot),
                job_queue: VecDeque::new(),
                active_run: None,
                worker_busy: false,
                shutdown: false,
            }),
            worker_cv: Condvar::new(),
            idle_cv: Condvar::new(),
        });

        let worker_inner = Arc::clone(&inner);
        let handle = thread::Builder::new()
            .name("foldersync-worker".to_string())
            .spawn(move || worker_loop(worker_inner))
            .map_err(|err| ManagerError::Io {
                cause: format!("Failed to spawn FolderSync worker thread: {err}."),
                next_step: "Check system thread limits and retry.".to_string(),
            })?;

        Ok(Self {
            _lock: lock,
            inner,
            worker_thread: Mutex::new(Some(handle)),
        })
    }

    pub fn state_dir(&self) -> &Path {
        &self.inner.state_dir
    }

    pub fn finder_status_dir(&self) -> &Path {
        self.inner.finder_writer.dir()
    }

    /// Registers a local-fixture account backed by an isolated throwaway empty
    /// `rclone.conf` inside `<state_dir>/accounts/<id>/rclone.conf` (`0600`).
    pub fn register_local_fixture_account(
        &self,
        account_id: impl Into<AccountId>,
        display_name: impl Into<String>,
    ) -> Result<AccountRecord, ManagerError> {
        let account_id = account_id.into();
        if account_id.is_empty() {
            return Err(ManagerError::PreflightRejected {
                cause: "Account identifier must not be empty.".to_string(),
                next_step: "Provide a non-empty account identifier.".to_string(),
            });
        }
        let account_dir = self
            .inner
            .state_dir
            .join("accounts")
            .join(account_id.as_str());
        ensure_private_dir(&account_dir)?;
        let config_path = account_dir.join("rclone.conf");
        if !config_path.exists() {
            write_private_file(&config_path, b"")?;
        }
        let now = self.inner.clock.now_rfc3339();
        let record = AccountRecord {
            id: account_id,
            display_name: display_name.into(),
            kind: "local_fixture".to_string(),
            remote_name: "local".to_string(),
            remote_root_folder_id: "local-root".to_string(),
            config_path,
            auth_status: "ready".to_string(),
            created_at: now.clone(),
            updated_at: now,
        };
        let guard = self.inner.state.lock().unwrap();
        guard.store.upsert_account(&record)?;
        Ok(record)
    }

    pub fn execute(&self, cmd: ManagerCommand) -> Result<CommandAcceptance, ManagerError> {
        match cmd {
            ManagerCommand::AddMapping(add) => self.handle_add_mapping(*add),
            ManagerCommand::Initialize(init) => self.handle_initialize(init),
            ManagerCommand::RunNow { mapping_id } => self.handle_run_now(mapping_id),
            ManagerCommand::Pause { mapping_id } => self.handle_pause(mapping_id),
            ManagerCommand::Resume { mapping_id } => self.handle_resume(mapping_id),
            ManagerCommand::StopRun { mapping_id } => self.handle_stop_run(mapping_id),
            ManagerCommand::ForceStop { mapping_id } => self.handle_force_stop(mapping_id),
            ManagerCommand::InspectRecovery { mapping_id } => {
                self.handle_inspect_recovery(mapping_id)
            }
        }
    }

    pub fn add_mapping(&self, cmd: AddMappingCommand) -> Result<CommandAcceptance, ManagerError> {
        self.execute(ManagerCommand::AddMapping(Box::new(cmd)))
    }

    pub fn initialize(&self, cmd: InitializeCommand) -> Result<CommandAcceptance, ManagerError> {
        self.execute(ManagerCommand::Initialize(cmd))
    }

    pub fn run_now(
        &self,
        mapping_id: impl Into<MappingId>,
    ) -> Result<CommandAcceptance, ManagerError> {
        self.execute(ManagerCommand::RunNow {
            mapping_id: mapping_id.into(),
        })
    }

    pub fn pause(
        &self,
        mapping_id: impl Into<MappingId>,
    ) -> Result<CommandAcceptance, ManagerError> {
        self.execute(ManagerCommand::Pause {
            mapping_id: mapping_id.into(),
        })
    }

    pub fn resume(
        &self,
        mapping_id: impl Into<MappingId>,
    ) -> Result<CommandAcceptance, ManagerError> {
        self.execute(ManagerCommand::Resume {
            mapping_id: mapping_id.into(),
        })
    }

    pub fn stop_run(
        &self,
        mapping_id: impl Into<MappingId>,
    ) -> Result<CommandAcceptance, ManagerError> {
        self.execute(ManagerCommand::StopRun {
            mapping_id: mapping_id.into(),
        })
    }

    pub fn force_stop(
        &self,
        mapping_id: impl Into<MappingId>,
    ) -> Result<CommandAcceptance, ManagerError> {
        self.execute(ManagerCommand::ForceStop {
            mapping_id: mapping_id.into(),
        })
    }

    pub fn inspect_recovery(
        &self,
        mapping_id: impl Into<MappingId>,
    ) -> Result<RecoveryReport, ManagerError> {
        match self
            .execute(ManagerCommand::InspectRecovery {
                mapping_id: mapping_id.into(),
            })?
            .outcome
        {
            CommandOutcome::RecoveryInspected(report) => Ok(*report),
            _ => unreachable!(),
        }
    }

    pub fn snapshot(&self) -> Arc<StatusSnapshot> {
        Arc::clone(&self.inner.state.lock().unwrap().latest_snapshot)
    }

    pub fn recent_events(&self) -> Vec<ManagerEvent> {
        self.inner.event_bus.recent_events()
    }

    pub fn subscribe_events(&self, bound: usize) -> mpsc::Receiver<ManagerEvent> {
        self.inner.event_bus.subscribe_events(bound)
    }

    pub fn subscribe_snapshots(&self, bound: usize) -> mpsc::Receiver<Arc<StatusSnapshot>> {
        self.inner.event_bus.subscribe_snapshots(bound)
    }

    pub fn list_runs(&self, mapping_id: &MappingId) -> Result<Vec<RunRecord>, ManagerError> {
        self.inner
            .state
            .lock()
            .unwrap()
            .store
            .list_runs_for_mapping(mapping_id)
    }

    pub fn last_run_id(&self, mapping_id: &MappingId) -> Option<RunId> {
        self.snapshot()
            .mappings
            .iter()
            .find(|m| &m.id == mapping_id)
            .and_then(|m| m.last_run_id.clone())
    }

    /// Waits until no mutating job is queued or running, or until `timeout` elapses.
    pub fn wait_until_idle(&self, timeout: Duration) -> Result<Arc<StatusSnapshot>, ManagerError> {
        let deadline = Instant::now() + timeout;
        let mut guard = self.inner.state.lock().unwrap();
        while guard.worker_busy || guard.active_run.is_some() || !guard.job_queue.is_empty() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(ManagerError::Io {
                    cause: "Timed out waiting for active synchronization job to finish."
                        .to_string(),
                    next_step: "Inspect status or request StopRun to cancel the active job."
                        .to_string(),
                });
            }
            let (next_guard, wait_res) = self.inner.idle_cv.wait_timeout(guard, remaining).unwrap();
            guard = next_guard;
            if wait_res.timed_out()
                && (guard.worker_busy || guard.active_run.is_some() || !guard.job_queue.is_empty())
            {
                return Err(ManagerError::Io {
                    cause: "Timed out waiting for active synchronization job to finish."
                        .to_string(),
                    next_step: "Inspect status or request StopRun to cancel the active job."
                        .to_string(),
                });
            }
        }
        Ok(Arc::clone(&guard.latest_snapshot))
    }

    /// Cleanly stops scheduling, requests graceful stop on any active child,
    /// joins the worker thread, and releases the exclusive state lock.
    pub fn shutdown(self) -> Result<(), ManagerError> {
        self.shutdown_internal();
        Ok(())
    }

    fn shutdown_internal(&self) {
        let stopper = {
            let mut guard = self.inner.state.lock().unwrap();
            guard.shutdown = true;
            guard.job_queue.clear();
            let stopper = if let Some(active) = guard.active_run.as_mut() {
                active.stop_requested = true;
                active.stopper.as_ref().map(Arc::clone)
            } else {
                None
            };
            self.inner.worker_cv.notify_all();
            stopper
        };
        if let Some(stopper) = stopper {
            let _ = stopper.request_stop();
        }
        if let Some(handle) = self.worker_thread.lock().unwrap().take() {
            let _ = handle.join();
        }
    }

    fn handle_add_mapping(
        &self,
        cmd: AddMappingCommand,
    ) -> Result<CommandAcceptance, ManagerError> {
        if cmd.id.is_empty() {
            return Err(ManagerError::PreflightRejected {
                cause: "Mapping identifier must not be empty.".to_string(),
                next_step: "Provide a short, unique identifier for the mapping.".to_string(),
            });
        }
        // Validate max_delete_percent via sync-core (0..=100) and rclone-adapter (1..=100).
        let max_del = MaxDeletePercent::new(cmd.max_delete_percent)?;
        if max_del.value() == 0 {
            return Err(ManagerError::PreflightRejected {
                cause: "Deletion limit must be at least 1% of tracked files.".to_string(),
                next_step: "Choose a deletion limit between 1% and 100% (default: 10%)."
                    .to_string(),
            });
        }

        // Local root must already exist (never auto-create missing roots).
        validate_local_binding(&cmd.local_binding, &StdPathResolver)?;
        if let RemoteEndpoint::LocalDir(remote_dir) = &cmd.remote_endpoint
            && !remote_dir.is_dir()
        {
            return Err(ManagerError::Io {
                cause: format!(
                    "Remote fixture directory `{}` does not exist.",
                    remote_dir.display()
                ),
                next_step:
                    "Create or choose an existing destination directory before adding the mapping."
                        .to_string(),
            });
        }

        let mut guard = self.inner.state.lock().unwrap();
        if guard.store.get_mapping(&cmd.id)?.is_some() {
            return Err(ManagerError::PreflightRejected {
                cause: format!("A mapping with ID `{}` already exists.", cmd.id),
                next_step: "Choose a different mapping ID or inspect the existing mapping."
                    .to_string(),
            });
        }
        if guard.store.get_account(&cmd.account_id)?.is_none() {
            return Err(ManagerError::NotFound {
                cause: format!("Account `{}` is not configured.", cmd.account_id),
                next_step: "Connect or register the account before adding mappings for it."
                    .to_string(),
            });
        }

        let existing = guard.store.list_mappings()?;
        let mut layout_entries: Vec<LocalLayoutEntry> = existing
            .iter()
            .map(|m| LocalLayoutEntry {
                mapping_id: m.id.clone(),
                local_binding: m.local_binding.clone(),
                local_recovery_root: m.local_recovery_root.clone(),
            })
            .collect();
        layout_entries.push(LocalLayoutEntry {
            mapping_id: cmd.id.clone(),
            local_binding: cmd.local_binding.clone(),
            local_recovery_root: cmd.local_recovery_root.clone(),
        });
        validate_local_layout(
            &layout_entries,
            &self.inner.state_dir,
            &AncestorAwarePathResolver,
        )
        .map_err(ManagerError::ValidationList)?;

        let mut cloud_assignments: Vec<DeviceCloudCollection> = existing
            .iter()
            .map(|m| DeviceCloudCollection {
                device_id: m.device_id.clone(),
                collection: m.collection.clone(),
            })
            .collect();
        cloud_assignments.push(DeviceCloudCollection {
            device_id: guard.device.id.clone(),
            collection: cmd.collection.clone(),
        });
        validate_cloud_collections(&cloud_assignments).map_err(ManagerError::ValidationList)?;

        let mut effective_exclusions = default_exclusion_patterns();
        for pat in cmd.extra_exclusions {
            if !pat.trim().is_empty() {
                effective_exclusions.insert(pat);
            }
        }

        let (remote_kind, remote_value) = cmd.remote_endpoint.to_db_cols();
        let now = self.inner.clock.now_rfc3339();
        let initial_state = MappingState::new(
            Readiness::NeedsInitialization,
            Execution::Idle,
            Intent::Enabled,
        );

        let record = MappingRecord {
            id: cmd.id.clone(),
            device_id: guard.device.id.clone(),
            account_id: cmd.account_id,
            collection: cmd.collection,
            local_binding: cmd.local_binding,
            remote_endpoint_kind: remote_kind,
            remote_endpoint_value: remote_value,
            local_recovery_root: cmd.local_recovery_root,
            cloud_recovery_path: cmd.cloud_recovery_path,
            schedule_interval_secs: cmd.schedule_interval.as_secs(),
            scheduling_enabled: false,
            state: initial_state,
            active_generation_id: None,
            exclusions: effective_exclusions.into_iter().collect(),
            max_delete_percent: max_del.value(),
            last_run_id: None,
            last_run_outcome: None,
            last_run_completed_at: None,
            last_run_summary_text: None,
            created_at: now.clone(),
            updated_at: now.clone(),
        };

        guard.store.insert_mapping(&record)?;
        let snapshot = publish_updated_snapshot(&self.inner, &mut guard)?;
        self.inner
            .event_bus
            .publish_event(ManagerEvent::MappingAdded {
                mapping_id: cmd.id.clone(),
                timestamp: now,
            });

        Ok(CommandAcceptance {
            outcome: CommandOutcome::MappingAdded { mapping_id: cmd.id },
            snapshot,
        })
    }

    fn handle_initialize(&self, cmd: InitializeCommand) -> Result<CommandAcceptance, ManagerError> {
        let mut guard = self.inner.state.lock().unwrap();
        if let Some(active) = &guard.active_run {
            return Err(ManagerError::ActiveJobConflict {
                active_mapping_id: active.mapping_id.clone(),
                cause: format!(
                    "Another synchronization job (`{}`) is currently active.",
                    active.mapping_id
                ),
                next_step:
                    "Wait for the active job to finish or stop it before initializing another mapping."
                        .to_string(),
            });
        }
        if let Some(queued) = guard.job_queue.front() {
            return Err(ManagerError::ActiveJobConflict {
                active_mapping_id: queued.mapping_id.clone(),
                cause: format!(
                    "Another synchronization job (`{}`) is already queued.",
                    queued.mapping_id
                ),
                next_step: "Wait for queued jobs to finish before initializing.".to_string(),
            });
        }

        let mut mapping =
            guard
                .store
                .get_mapping(&cmd.mapping_id)?
                .ok_or_else(|| ManagerError::NotFound {
                    cause: format!("Mapping `{}` does not exist.", cmd.mapping_id),
                    next_step: "Add the mapping before initializing it.".to_string(),
                })?;

        if mapping.state.readiness() != Readiness::NeedsInitialization
            || mapping.state.execution != Execution::Idle
        {
            return Err(ManagerError::InvalidState {
                mapping_id: mapping.id.clone(),
                cause: format!(
                    "Mapping `{}` cannot be initialized in state {:?}/{:?}.",
                    mapping.id,
                    mapping.state.readiness(),
                    mapping.state.execution
                ),
                next_step:
                    "Only uninitialized idle mappings can run initialization; use recovery or Run now for initialized mappings."
                        .to_string(),
            });
        }

        let account = guard
            .store
            .get_account(&mapping.account_id)?
            .ok_or_else(|| ManagerError::NotFound {
                cause: format!("Account `{}` is missing.", mapping.account_id),
                next_step: "Reconnect the account before initializing.".to_string(),
            })?;

        // Re-validate local binding and full layout with sync-core.
        let canonical_local = validate_local_binding(&mapping.local_binding, &StdPathResolver)?;
        let all_mappings = guard.store.list_mappings()?;
        let layout_entries: Vec<LocalLayoutEntry> = all_mappings
            .iter()
            .map(|m| LocalLayoutEntry {
                mapping_id: m.id.clone(),
                local_binding: m.local_binding.clone(),
                local_recovery_root: m.local_recovery_root.clone(),
            })
            .collect();
        validate_local_layout(
            &layout_entries,
            &self.inner.state_dir,
            &AncestorAwarePathResolver,
        )
        .map_err(ManagerError::ValidationList)?;

        let cloud_assignments: Vec<DeviceCloudCollection> = all_mappings
            .iter()
            .map(|m| DeviceCloudCollection {
                device_id: m.device_id.clone(),
                collection: m.collection.clone(),
            })
            .collect();
        validate_cloud_collections(&cloud_assignments).map_err(ManagerError::ValidationList)?;

        // Inspect endpoints, classify both sides, refuse both-populated, establish recovery & markers (§9).
        let remote_ep = RemoteEndpoint::from_db_cols(
            &mapping.remote_endpoint_kind,
            &mapping.remote_endpoint_value,
        )?;
        let direction = match &remote_ep {
            RemoteEndpoint::LocalDir(remote_root) => {
                let local_inv = inspect_local_endpoint(&canonical_local, &mapping.collection)?;
                let remote_inv = inspect_local_endpoint(remote_root, &mapping.collection)?;
                let dir = decide_init_direction(
                    &mapping.id,
                    &local_inv,
                    &remote_inv,
                    cmd.expected_direction,
                )?;
                // Establish recovery directories only inside explicit initialization.
                fs::create_dir_all(&mapping.local_recovery_root).map_err(|err| {
                    ManagerError::Io {
                        cause: format!(
                            "Failed to create local recovery directory `{}`: {err}.",
                            mapping.local_recovery_root.display()
                        ),
                        next_step: "Verify permissions on the recovery drive and retry."
                            .to_string(),
                    }
                })?;
                let remote_recovery_dir = PathBuf::from(&mapping.cloud_recovery_path);
                fs::create_dir_all(&remote_recovery_dir).map_err(|err| ManagerError::Io {
                    cause: format!(
                        "Failed to create remote recovery directory `{}`: {err}.",
                        remote_recovery_dir.display()
                    ),
                    next_step: "Verify permissions on the remote recovery path and retry."
                        .to_string(),
                })?;
                establish_local_pair_markers(
                    &canonical_local,
                    remote_root,
                    &local_inv,
                    &remote_inv,
                    &mapping.collection,
                )?;
                dir
            }
            RemoteEndpoint::DriveRemote { .. } | RemoteEndpoint::OtherRemote { .. } => {
                return Err(ManagerError::PreflightRejected {
                    cause: "Cloud Drive initialization is not yet wired in Group 3.".to_string(),
                    next_step: "Use local-to-local fixtures until Group 4 Drive onboarding lands."
                        .to_string(),
                });
            }
        };

        // Qualify engine metadata before freezing the generation.
        let engine_meta = self.inner.launcher.engine_metadata()?;

        let gen_num = guard.store.next_generation_number(&mapping.id)?;
        let gen_id = GenerationId::new(format!("{}-gen-{gen_num}", mapping.id));
        let gen_dir = self
            .inner
            .state_dir
            .join("mappings")
            .join(mapping.id.as_str())
            .join(format!("generation-{gen_num}"));
        let workdir_path = gen_dir.join("bisync");
        let filters_file_path = gen_dir.join("filters.txt");
        ensure_private_dir(&workdir_path)?;

        let effective_filters: BTreeSet<String> = mapping.exclusions.iter().cloned().collect();
        let filters_content = render_filters_file(&effective_filters);
        write_private_file(&filters_file_path, filters_content.as_bytes())?;

        let config_gen = ConfigGeneration {
            path1: canonical_local,
            path2: remote_ep.path2_string(),
            remote_root_folder_id: account.remote_root_folder_id,
            selected_folder_id: mapping.collection.folder_id.clone(),
            local_binding: mapping.local_binding.clone(),
            effective_filters,
            compare_policy: ComparePolicy::SizeModtime,
            supported_types: SupportedTypesPolicy::default(),
            conflict_policy: ConflictPolicy::default(),
            recovery_policy: RecoveryPolicy {
                keep_recovery_copies: true,
                max_delete_percent: MaxDeletePercent::new(mapping.max_delete_percent)?,
            },
            engine_compatibility: EngineCompatibility {
                engine_name: "rclone".to_string(),
                engine_version: engine_meta.version,
                profile_version: ENGINE_PROFILE_VERSION.to_string(),
            },
        };
        let fingerprint = config_gen.fingerprint().to_hex();
        let initial_direction = match direction {
            InitPreference::Path1 => "path1".to_string(),
            InitPreference::Path2 => "path2".to_string(),
        };
        let now = self.inner.clock.now_rfc3339();
        let gen_record = GenerationRecord {
            id: gen_id.clone(),
            mapping_id: mapping.id.clone(),
            generation_number: gen_num,
            fingerprint,
            config: config_gen,
            initial_direction: Some(initial_direction),
            workdir_path,
            filters_file_path,
            created_at: now.clone(),
        };
        guard.store.insert_generation(&gen_record)?;

        mapping.state.execution = Execution::Queued;
        mapping.updated_at = now;
        guard.store.update_mapping_runtime(&mapping)?;

        guard.job_queue.push_back(QueuedJob {
            mapping_id: mapping.id.clone(),
            generation_id: gen_id,
            mode: RunMode::Initialize(direction),
        });
        let snapshot = publish_updated_snapshot(&self.inner, &mut guard)?;
        self.inner.worker_cv.notify_all();

        Ok(CommandAcceptance {
            outcome: CommandOutcome::InitializationQueued {
                mapping_id: mapping.id,
                direction,
            },
            snapshot,
        })
    }

    fn handle_run_now(&self, mapping_id: MappingId) -> Result<CommandAcceptance, ManagerError> {
        let mut guard = self.inner.state.lock().unwrap();
        let mut mapping =
            guard
                .store
                .get_mapping(&mapping_id)?
                .ok_or_else(|| ManagerError::NotFound {
                    cause: format!("Mapping `{mapping_id}` does not exist."),
                    next_step: "Check the mapping ID with `status`.".to_string(),
                })?;

        if mapping.state.intent == Intent::Paused {
            return Err(ManagerError::InvalidState {
                mapping_id,
                cause: format!("Mapping `{}` is paused.", mapping.id),
                next_step: "Resume the mapping before running a sync.".to_string(),
            });
        }
        if mapping.state.readiness() != Readiness::Ready {
            return Err(ManagerError::InvalidState {
                mapping_id,
                cause: format!(
                    "Mapping `{}` is not ready to sync (current readiness: {:?}).",
                    mapping.id,
                    mapping.state.readiness()
                ),
                next_step: match mapping.state.readiness() {
                    Readiness::NeedsInitialization => {
                        "Run explicit initialization (`init`) first.".to_string()
                    }
                    Readiness::RecoveryRequired => {
                        "Inspect recovery details (`InspectRecovery`) before resuming sync."
                            .to_string()
                    }
                    Readiness::VolumeUnavailable => {
                        "Reconnect the bound volume and retry.".to_string()
                    }
                    Readiness::AuthenticationRequired => {
                        "Reconnect the cloud account and retry.".to_string()
                    }
                    Readiness::EndpointChanged => {
                        "Review the endpoint identity change before syncing.".to_string()
                    }
                    Readiness::Unconfigured => "Finish configuring the mapping first.".to_string(),
                    Readiness::Ready => unreachable!(),
                },
            });
        }
        if mapping.state.execution != Execution::Idle {
            return Err(ManagerError::InvalidState {
                mapping_id,
                cause: format!(
                    "Mapping `{}` is already {:?}.",
                    mapping.id, mapping.state.execution
                ),
                next_step: "Wait for the current run to finish or request StopRun.".to_string(),
            });
        }

        let Some(gen_id) = mapping.active_generation_id.clone() else {
            return Err(ManagerError::InvalidState {
                mapping_id,
                cause: format!(
                    "Mapping `{}` has no active configuration generation.",
                    mapping.id
                ),
                next_step: "Run initialization to establish a baseline generation.".to_string(),
            });
        };

        // Preflight local root presence (never auto-create missing roots).
        if !mapping.local_binding.resolved_path().is_dir() {
            let next = apply_event(mapping.state, ObservedEvent::VolumeLost);
            mapping.state = next.state;
            mapping.updated_at = self.inner.clock.now_rfc3339();
            guard.store.update_mapping_runtime(&mapping)?;
            let _ = publish_updated_snapshot(&self.inner, &mut guard)?;
            return Err(ManagerError::InvalidState {
                mapping_id: mapping.id.clone(),
                cause: format!(
                    "Bound local folder `{}` is missing or its volume is unavailable.",
                    mapping.local_binding.resolved_path().display()
                ),
                next_step: "Mount the expected volume and confirm the folder exists (FolderSync never auto-creates missing roots)."
                    .to_string(),
            });
        }

        let transition = apply_user_action(mapping.state, UserAction::RunNow);
        if transition.effect != TransitionEffect::ScheduleRun {
            return Err(ManagerError::InvalidState {
                mapping_id: mapping.id.clone(),
                cause: format!("Mapping `{}` cannot start a run right now.", mapping.id),
                next_step: "Inspect mapping status and readiness before retrying.".to_string(),
            });
        }

        mapping.state = transition.state;
        mapping.updated_at = self.inner.clock.now_rfc3339();
        guard.store.update_mapping_runtime(&mapping)?;

        guard.job_queue.push_back(QueuedJob {
            mapping_id: mapping.id.clone(),
            generation_id: gen_id,
            mode: RunMode::Normal,
        });
        let snapshot = publish_updated_snapshot(&self.inner, &mut guard)?;
        self.inner.worker_cv.notify_all();

        Ok(CommandAcceptance {
            outcome: CommandOutcome::RunQueued {
                mapping_id: mapping.id,
            },
            snapshot,
        })
    }

    fn handle_pause(&self, mapping_id: MappingId) -> Result<CommandAcceptance, ManagerError> {
        let mut guard = self.inner.state.lock().unwrap();
        let mut mapping =
            guard
                .store
                .get_mapping(&mapping_id)?
                .ok_or_else(|| ManagerError::NotFound {
                    cause: format!("Mapping `{mapping_id}` does not exist."),
                    next_step: "Check the mapping ID with `status`.".to_string(),
                })?;

        let transition = apply_user_action(mapping.state, UserAction::Pause);
        mapping.state = transition.state;
        mapping.updated_at = self.inner.clock.now_rfc3339();
        // Cancel any queued (not yet running) job for this mapping; let an active run finish (§14).
        guard.job_queue.retain(|j| j.mapping_id != mapping.id);

        // Persist BEFORE acknowledging!
        guard.store.update_mapping_runtime(&mapping)?;
        let snapshot = publish_updated_snapshot(&self.inner, &mut guard)?;
        self.inner.idle_cv.notify_all();

        Ok(CommandAcceptance {
            outcome: CommandOutcome::Paused {
                mapping_id: mapping.id,
            },
            snapshot,
        })
    }

    fn handle_resume(&self, mapping_id: MappingId) -> Result<CommandAcceptance, ManagerError> {
        let mut guard = self.inner.state.lock().unwrap();
        let mut mapping =
            guard
                .store
                .get_mapping(&mapping_id)?
                .ok_or_else(|| ManagerError::NotFound {
                    cause: format!("Mapping `{mapping_id}` does not exist."),
                    next_step: "Check the mapping ID with `status`.".to_string(),
                })?;

        let transition = apply_user_action(mapping.state, UserAction::Resume);
        mapping.state = transition.state;
        // Scheduling is disabled in Group 3: resuming clears Intent::Paused without
        // automatically launching a sync run.
        if transition.effect == TransitionEffect::ScheduleRun && !mapping.scheduling_enabled {
            mapping.state.execution = Execution::Idle;
        }
        mapping.updated_at = self.inner.clock.now_rfc3339();

        // Persist BEFORE acknowledging!
        guard.store.update_mapping_runtime(&mapping)?;
        let snapshot = publish_updated_snapshot(&self.inner, &mut guard)?;

        Ok(CommandAcceptance {
            outcome: CommandOutcome::Resumed {
                mapping_id: mapping.id,
            },
            snapshot,
        })
    }

    fn handle_stop_run(&self, mapping_id: MappingId) -> Result<CommandAcceptance, ManagerError> {
        let (stopper, snapshot) =
            {
                let mut guard = self.inner.state.lock().unwrap();
                let mut mapping = guard.store.get_mapping(&mapping_id)?.ok_or_else(|| {
                    ManagerError::NotFound {
                        cause: format!("Mapping `{mapping_id}` does not exist."),
                        next_step: "Check the mapping ID with `status`.".to_string(),
                    }
                })?;

                if !matches!(
                    mapping.state.execution,
                    Execution::Running | Execution::Queued
                ) {
                    return Err(ManagerError::InvalidState {
                        mapping_id: mapping.id.clone(),
                        cause: format!(
                            "Mapping `{}` has no active or queued run to stop (execution: {:?}).",
                            mapping.id, mapping.state.execution
                        ),
                        next_step: "Only queued or running sync jobs can be stopped.".to_string(),
                    });
                }

                let transition = apply_user_action(mapping.state, UserAction::StopRun);
                mapping.state = transition.state;
                mapping.updated_at = self.inner.clock.now_rfc3339();
                guard.job_queue.retain(|j| j.mapping_id != mapping.id);
                guard.store.update_mapping_runtime(&mapping)?;

                let stopper = if transition.effect == TransitionEffect::RequestGracefulStop {
                    if let Some(active) = guard.active_run.as_mut()
                        && active.mapping_id == mapping.id
                    {
                        active.stop_requested = true;
                        active.stopper.as_ref().map(Arc::clone)
                    } else {
                        None
                    }
                } else {
                    None
                };
                let snapshot = publish_updated_snapshot(&self.inner, &mut guard)?;
                (stopper, snapshot)
            };

        if let Some(stopper) = stopper {
            stopper.request_stop()?;
        }

        Ok(CommandAcceptance {
            outcome: CommandOutcome::StopRequested { mapping_id },
            snapshot,
        })
    }

    fn handle_force_stop(&self, mapping_id: MappingId) -> Result<CommandAcceptance, ManagerError> {
        let (stopper, snapshot) = {
            let mut guard = self.inner.state.lock().unwrap();
            let mut mapping =
                guard
                    .store
                    .get_mapping(&mapping_id)?
                    .ok_or_else(|| ManagerError::NotFound {
                        cause: format!("Mapping `{mapping_id}` does not exist."),
                        next_step: "Check the mapping ID with `status`.".to_string(),
                    })?;

            if !matches!(
                mapping.state.execution,
                Execution::Running | Execution::Stopping | Execution::Queued
            ) {
                return Err(ManagerError::InvalidState {
                    mapping_id: mapping.id.clone(),
                    cause: format!(
                        "Mapping `{}` has no active job to force-stop (execution: {:?}).",
                        mapping.id, mapping.state.execution
                    ),
                    next_step: "Inspect status before requesting ForceStop.".to_string(),
                });
            }

            let transition = apply_user_action(mapping.state, UserAction::ForceStop);
            mapping.state = transition.state;
            let now = self.inner.clock.now_rfc3339();
            mapping.updated_at = now.clone();
            guard.job_queue.retain(|j| j.mapping_id != mapping.id);

            let mut stopper = None;
            let mut run_id_for_case = None;
            if let Some(active) = guard.active_run.as_mut()
                && active.mapping_id == mapping.id
            {
                active.force_stopped = true;
                run_id_for_case = Some(active.run_id.clone());
                stopper = active.stopper.as_ref().map(Arc::clone);
            }

            // Persist recovery hold and recovery case BEFORE acknowledging!
            guard.store.update_mapping_runtime(&mapping)?;
            if transition.effect == TransitionEffect::ForceTerminateChild {
                let gen_workdir = mapping
                    .active_generation_id
                    .as_ref()
                    .and_then(|gid| guard.store.get_generation(gid).ok().flatten())
                    .map(|g| g.workdir_path);
                let case = RecoveryCaseRecord {
                    id: format!("rc-{}", Uuid::new_v4()),
                    mapping_id: mapping.id.clone(),
                    run_id: run_id_for_case,
                    status: "open".to_string(),
                    reason_kind: "force_stopped".to_string(),
                    cause: "Synchronization was forcefully terminated before completion."
                        .to_string(),
                    next_step:
                        "Inspect the preserved work directory, run logs, and recovery copies before clearing recovery."
                            .to_string(),
                    workdir_path: gen_workdir,
                    backup_dir1: None,
                    backup_dir2: None,
                    log_path: None,
                    orphan_pid: None,
                    created_at: now.clone(),
                    resolved_at: None,
                    resolution_note: None,
                };
                guard.store.insert_recovery_case(&case)?;
                self.inner
                    .event_bus
                    .publish_event(ManagerEvent::RecoveryOpened {
                        mapping_id: mapping.id.clone(),
                        case_id: case.id,
                        reason_kind: case.reason_kind,
                        cause: case.cause,
                        timestamp: now,
                    });
            }

            let snapshot = publish_updated_snapshot(&self.inner, &mut guard)?;
            (stopper, snapshot)
        };

        if let Some(stopper) = stopper {
            stopper.force_stop()?;
        }

        Ok(CommandAcceptance {
            outcome: CommandOutcome::ForceStopped { mapping_id },
            snapshot,
        })
    }

    fn handle_inspect_recovery(
        &self,
        mapping_id: MappingId,
    ) -> Result<CommandAcceptance, ManagerError> {
        let guard = self.inner.state.lock().unwrap();
        let mapping =
            guard
                .store
                .get_mapping(&mapping_id)?
                .ok_or_else(|| ManagerError::NotFound {
                    cause: format!("Mapping `{mapping_id}` does not exist."),
                    next_step: "Check the mapping ID with `status`.".to_string(),
                })?;

        let raw_cases = guard.store.list_open_recovery_cases(&mapping.id)?;
        let recent_runs = guard.store.list_runs_for_mapping(&mapping.id)?;

        let mut open_cases = Vec::with_capacity(raw_cases.len());
        for case in raw_cases {
            let orphan_still_alive = case.orphan_pid.map(|pid| {
                // Re-verify recorded PID + start_time + executable from its run row!
                let run_match = case
                    .run_id
                    .as_ref()
                    .and_then(|rid| recent_runs.iter().find(|r| &r.id == rid));
                match (self.inner.process_inspector.inspect(pid), run_match) {
                    (ProcessProbeResult::Live(info), Some(run)) => {
                        run.child_start_time.is_some()
                            && info.start_time == run.child_start_time
                            && executables_match(&run.child_executable, &info.executable)
                    }
                    _ => false,
                }
            });
            open_cases.push(RecoveryCaseView {
                case,
                orphan_still_alive,
            });
        }

        let workdir_path = mapping
            .active_generation_id
            .as_ref()
            .and_then(|gid| guard.store.get_generation(gid).ok().flatten())
            .map(|g| g.workdir_path)
            .or_else(|| recent_runs.first().map(|r| r.workdir_path.clone()));

        let (workdir_files, has_lock_file) = workdir_path
            .as_deref()
            .map(inspect_workdir_readonly)
            .unwrap_or_default();

        let report = RecoveryReport {
            mapping_id: mapping.id,
            state: mapping.state,
            open_cases,
            workdir_files,
            has_lock_file,
            recent_runs,
        };
        let snapshot = Arc::clone(&guard.latest_snapshot);

        Ok(CommandAcceptance {
            outcome: CommandOutcome::RecoveryInspected(Box::new(report)),
            snapshot,
        })
    }
}

impl Drop for SyncManager {
    fn drop(&mut self) {
        self.shutdown_internal();
    }
}

fn default_device_name() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "foldersync-device".to_string())
}

fn inspect_workdir_readonly(workdir: &Path) -> (Vec<String>, bool) {
    let mut files = Vec::new();
    let mut has_lock = false;
    if let Ok(entries) = fs::read_dir(workdir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".lck") {
                has_lock = true;
            }
            files.push(name);
        }
    }
    files.sort();
    (files, has_lock)
}

fn executables_match(recorded: &Option<PathBuf>, live: &Option<PathBuf>) -> bool {
    let (Some(rec), Some(act)) = (recorded, live) else {
        return false;
    };
    if rec == act {
        return true;
    }
    match (fs::canonicalize(rec), fs::canonicalize(act)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn executables_clearly_differ(recorded: &Option<PathBuf>, live: &Option<PathBuf>) -> bool {
    let (Some(rec), Some(act)) = (recorded, live) else {
        return false;
    };
    !executables_match(&Some(rec.clone()), &Some(act.clone()))
}

fn reconcile_startup_state(
    store: &SqliteStateStore,
    inspector: &dyn ProcessInspector,
    clock: &dyn Clock,
    event_bus: &EventBus,
) -> Result<(), ManagerError> {
    let now = clock.now_rfc3339();
    let unfinished = store.list_unfinished_runs()?;
    let mut reconciled_mappings = BTreeSet::new();

    for mut run in unfinished {
        let (_, has_lock) = inspect_workdir_readonly(&run.workdir_path);
        let lock_note = if has_lock {
            " A bisync lock file (.lck) is present in the work directory."
        } else {
            ""
        };

        let (phase, reason_kind, cause, next_step, orphan_pid) = match run.child_pid {
            None => (
                "reconciled_ambiguous".to_string(),
                "orphan_ambiguous".to_string(),
                format!(
                    "FolderSync exited after recording run `{}` and before recording the child process PID; process ownership is ambiguous.{lock_note}",
                    run.id
                ),
                "Inspect active processes and the preserved bisync work directory before clearing recovery."
                    .to_string(),
                None,
            ),
            Some(pid) => match inspector.inspect(pid) {
                ProcessProbeResult::NotFound => (
                    "reconciled_exited".to_string(),
                    "interrupted".to_string(),
                    format!(
                        "Run `{}` (PID {pid}) was interrupted by an unexpected manager exit and the child process is no longer running.{lock_note}",
                        run.id
                    ),
                    "Inspect the run log and work directory, then follow the recovery workflow."
                        .to_string(),
                    None,
                ),
                ProcessProbeResult::Live(live) => {
                    let start_matches = run.child_start_time.is_some()
                        && live.start_time == run.child_start_time;
                    let start_differs = run.child_start_time.is_some()
                        && live.start_time.is_some()
                        && live.start_time != run.child_start_time;
                    let exe_matches = executables_match(&run.child_executable, &live.executable);
                    let exe_differs =
                        executables_clearly_differ(&run.child_executable, &live.executable);

                    if start_matches && exe_matches {
                        (
                            "reconciled_orphan_running".to_string(),
                            "orphan_running".to_string(),
                            format!(
                                "Orphaned rclone process (PID {pid}) from run `{}` is still running.{lock_note}",
                                run.id
                            ),
                            "Wait for or stop the exact orphaned process before starting another run on this mapping."
                                .to_string(),
                            Some(pid),
                        )
                    } else if start_differs || exe_differs {
                        (
                            "reconciled_pid_reused".to_string(),
                            "interrupted".to_string(),
                            format!(
                                "Run `{}` was interrupted; recorded PID {pid} is now held by a different process (start time or executable mismatch) and was not touched.{lock_note}",
                                run.id
                            ),
                            "Inspect the preserved work directory and run logs before clearing recovery."
                                .to_string(),
                            None,
                        )
                    } else {
                        (
                            "reconciled_ambiguous".to_string(),
                            "orphan_ambiguous".to_string(),
                            format!(
                                "Recorded PID {pid} for run `{}` is live, but process identity (start time/executable) could not be conclusively verified.{lock_note}",
                                run.id
                            ),
                            "Verify whether the process belongs to FolderSync and inspect the work directory before clearing recovery."
                                .to_string(),
                            None,
                        )
                    }
                }
                ProcessProbeResult::Unavailable(detail) => (
                    "reconciled_ambiguous".to_string(),
                    "orphan_ambiguous".to_string(),
                    format!(
                        "Could not verify ownership of recorded PID {pid} for run `{}` ({detail}).{lock_note}",
                        run.id
                    ),
                    "Inspect running processes and the preserved work directory before clearing recovery."
                        .to_string(),
                    None,
                ),
            },
        };

        run.status = "interrupted".to_string();
        run.finished_at = Some(now.clone());
        run.outcome_kind = Some("interrupted".to_string());
        run.outcome_detail = Some(cause.clone());
        run.last_verified_phase = phase;
        store.update_run_finished(&run)?;

        if let Some(mut mapping) = store.get_mapping(&run.mapping_id)? {
            let next = apply_event(
                mapping.state,
                ObservedEvent::RunFinished(RunOutcome::Interrupted),
            );
            mapping.state = next.state;
            mapping.last_run_id = Some(run.id.clone());
            mapping.last_run_outcome = Some(RunOutcome::Interrupted);
            mapping.last_run_completed_at = Some(now.clone());
            mapping.last_run_summary_text = Some("Interrupted — recovery required".to_string());
            mapping.updated_at = now.clone();
            store.update_mapping_runtime(&mapping)?;
            reconciled_mappings.insert(mapping.id.clone());
        }

        let case = RecoveryCaseRecord {
            id: format!("rc-{}", Uuid::new_v4()),
            mapping_id: run.mapping_id.clone(),
            run_id: Some(run.id.clone()),
            status: "open".to_string(),
            reason_kind: reason_kind.clone(),
            cause: cause.clone(),
            next_step,
            workdir_path: Some(run.workdir_path.clone()),
            backup_dir1: Some(run.backup_dir1.clone()),
            backup_dir2: Some(run.backup_dir2.clone()),
            log_path: Some(run.log_path.clone()),
            orphan_pid,
            created_at: now.clone(),
            resolved_at: None,
            resolution_note: None,
        };
        store.insert_recovery_case(&case)?;
        event_bus.publish_event(ManagerEvent::RecoveryOpened {
            mapping_id: run.mapping_id,
            case_id: case.id,
            reason_kind,
            cause,
            timestamp: now.clone(),
        });
    }

    // Defensive check for any mapping left in Queued/Running/Stopping without an unfinished run row.
    for mut mapping in store.list_mappings()? {
        if reconciled_mappings.contains(&mapping.id) {
            continue;
        }
        match mapping.state.execution {
            Execution::Queued => {
                mapping.state.execution = Execution::Idle;
                mapping.updated_at = now.clone();
                store.update_mapping_runtime(&mapping)?;
            }
            Execution::Running | Execution::Stopping => {
                let next = apply_event(
                    mapping.state,
                    ObservedEvent::RunFinished(RunOutcome::Interrupted),
                );
                mapping.state = next.state;
                mapping.updated_at = now.clone();
                store.update_mapping_runtime(&mapping)?;
                let case = RecoveryCaseRecord {
                    id: format!("rc-{}", Uuid::new_v4()),
                    mapping_id: mapping.id.clone(),
                    run_id: mapping.last_run_id.clone(),
                    status: "open".to_string(),
                    reason_kind: "orphan_ambiguous".to_string(),
                    cause: "Mapping was marked active at shutdown without an open run record."
                        .to_string(),
                    next_step: "Inspect the work directory and logs before clearing recovery."
                        .to_string(),
                    workdir_path: None,
                    backup_dir1: None,
                    backup_dir2: None,
                    log_path: None,
                    orphan_pid: None,
                    created_at: now.clone(),
                    resolved_at: None,
                    resolution_note: None,
                };
                store.insert_recovery_case(&case)?;
            }
            Execution::Idle | Execution::Interrupted => {}
        }
    }

    Ok(())
}

fn build_snapshot(
    store: &SqliteStateStore,
    sequence: u64,
    active_job_mapping_id: Option<MappingId>,
    clock: &dyn Clock,
) -> Result<StatusSnapshot, ManagerError> {
    let mappings = store.list_mappings()?;
    let mut views = Vec::with_capacity(mappings.len());

    for m in mappings {
        let fingerprint = m
            .active_generation_id
            .as_ref()
            .and_then(|gid| store.get_generation(gid).ok().flatten())
            .map(|g| g.fingerprint);
        let open_cases = store.list_open_recovery_cases(&m.id)?;
        let runs = store.list_runs_for_mapping(&m.id)?;
        let latest_run = runs.first();

        let last_run_summary = match (m.last_run_outcome, m.last_run_completed_at.as_deref()) {
            (Some(outcome), Some(ts)) => {
                let wall = DateTime::parse_from_rfc3339(ts)
                    .ok()
                    .map(|dt| clock.wall_time(dt.with_timezone(&chrono::Utc)))
                    .unwrap_or_else(|| clock.wall_time(clock.now_utc()));
                Some(LastRunSummary {
                    outcome,
                    completed_at: wall,
                })
            }
            _ => None,
        };

        let display_name = m.collection.display_name().to_string();
        let volume_name = m.local_binding.volume_display_name();
        let text = status_text(
            &m.state,
            &display_name,
            &volume_name,
            last_run_summary.as_ref(),
        );
        let actions = available_actions(&m.state);

        let counters = MappingCounters {
            files_checked: latest_run.map(|r| r.files_checked).unwrap_or(0),
            files_transferred: latest_run.map(|r| r.files_transferred).unwrap_or(0),
            bytes_transferred: latest_run.map(|r| r.bytes_transferred).unwrap_or(0),
            conflict_count: latest_run.map(|r| r.conflict_count).unwrap_or(0),
            open_recovery_cases: open_cases.len() as u64,
        };

        views.push(MappingStatusSnapshot {
            id: m.id,
            display_name,
            account_id: m.account_id,
            collection_display_path: m.collection.display_path,
            local_root: m.local_binding.resolved_path(),
            volume_name,
            readiness: m.state.readiness(),
            execution: m.state.execution,
            intent: m.state.intent,
            scheduling_enabled: m.scheduling_enabled,
            active_generation_id: m.active_generation_id,
            active_generation_fingerprint: fingerprint,
            last_run_id: m.last_run_id,
            last_run_outcome: m.last_run_outcome,
            last_run_completed_at: m.last_run_completed_at,
            last_run_summary: m.last_run_summary_text,
            counters,
            available_actions: actions,
            status_text: text,
            local_recovery_root: m.local_recovery_root,
            cloud_recovery_path: m.cloud_recovery_path,
        });
    }

    Ok(StatusSnapshot {
        sequence,
        generated_at: clock.now_rfc3339(),
        active_job_mapping_id,
        mappings: views,
    })
}

fn publish_updated_snapshot(
    inner: &ManagerInner,
    guard: &mut SharedState,
) -> Result<Arc<StatusSnapshot>, ManagerError> {
    guard.snapshot_seq += 1;
    let active = guard.active_run.as_ref().map(|a| a.mapping_id.clone());
    let snap = Arc::new(build_snapshot(
        &guard.store,
        guard.snapshot_seq,
        active,
        inner.clock.as_ref(),
    )?);
    inner.finder_writer.write_snapshot(&snap)?;
    guard.latest_snapshot = Arc::clone(&snap);
    inner.event_bus.publish_snapshot(Arc::clone(&snap));
    inner
        .event_bus
        .publish_event(ManagerEvent::SnapshotUpdated {
            sequence: snap.sequence,
            timestamp: snap.generated_at.clone(),
        });
    Ok(snap)
}

fn worker_loop(inner: Arc<ManagerInner>) {
    loop {
        let job = {
            let mut guard = inner.state.lock().unwrap();
            while !guard.shutdown && guard.job_queue.is_empty() {
                guard = inner.worker_cv.wait(guard).unwrap();
            }
            if guard.shutdown {
                break;
            }
            let job = guard.job_queue.pop_front();
            if job.is_some() {
                guard.worker_busy = true;
            }
            job
        };

        let Some(job) = job else {
            continue;
        };

        execute_single_job(&inner, job);

        let mut guard = inner.state.lock().unwrap();
        guard.worker_busy = false;
        if guard.active_run.is_none() && guard.job_queue.is_empty() {
            inner.idle_cv.notify_all();
        }
    }
}

struct PreparedRun {
    run: RunRecord,
    plan: LaunchPlan,
    mode_label: String,
    is_init: bool,
}

fn prepare_run_before_spawn(
    inner: &ManagerInner,
    job: &QueuedJob,
) -> Result<PreparedRun, ManagerError> {
    let mut guard = inner.state.lock().unwrap();
    let mut mapping =
        guard
            .store
            .get_mapping(&job.mapping_id)?
            .ok_or_else(|| ManagerError::NotFound {
                cause: format!("Mapping `{}` no longer exists.", job.mapping_id),
                next_step: "Refresh mapping list.".to_string(),
            })?;
    let generation = guard
        .store
        .get_generation(&job.generation_id)?
        .ok_or_else(|| ManagerError::NotFound {
            cause: format!("Generation `{}` is missing.", job.generation_id),
            next_step: "Inspect mapping configuration.".to_string(),
        })?;
    let account = guard
        .store
        .get_account(&mapping.account_id)?
        .ok_or_else(|| ManagerError::NotFound {
            cause: format!("Account `{}` is missing.", mapping.account_id),
            next_step: "Reconnect account.".to_string(),
        })?;

    let run_id = RunId::new(format!("run-{}", Uuid::new_v4()));
    let run_dir = inner.state_dir.join("runs").join(run_id.as_str());
    ensure_private_dir(&run_dir)?;
    let log_path = run_dir.join("output.ndjson");

    let remote_ep = RemoteEndpoint::from_db_cols(
        &mapping.remote_endpoint_kind,
        &mapping.remote_endpoint_value,
    )?;
    let backup_dir1 = mapping
        .local_recovery_root
        .join(mapping.id.as_str())
        .join(run_id.as_str());
    let (backup_dir2, backup_dir2_str) = match &remote_ep {
        RemoteEndpoint::LocalDir(_) => {
            let p = PathBuf::from(&mapping.cloud_recovery_path)
                .join(mapping.device_id.as_str())
                .join(run_id.as_str());
            let s = p.to_string_lossy().into_owned();
            (BackupDir2::LocalDir(p), s)
        }
        RemoteEndpoint::DriveRemote { remote, .. } | RemoteEndpoint::OtherRemote { remote, .. } => {
            let prefix = mapping
                .cloud_recovery_path
                .split_once(':')
                .map(|(_, rest)| rest)
                .unwrap_or(mapping.cloud_recovery_path.as_str());
            let path = format!("{prefix}/{}/{}", mapping.device_id, run_id);
            let s = format!("{remote}:{path}");
            (
                BackupDir2::Remote {
                    remote: remote.clone(),
                    path,
                },
                s,
            )
        }
    };

    let (mode_label, is_init) = match job.mode {
        RunMode::Normal => ("normal".to_string(), false),
        RunMode::Initialize(InitPreference::Path1) => ("init_path1".to_string(), true),
        RunMode::Initialize(InitPreference::Path2) => ("init_path2".to_string(), true),
    };

    let started_at = inner.clock.now_rfc3339();
    let run = RunRecord {
        id: run_id.clone(),
        mapping_id: mapping.id.clone(),
        generation_id: generation.id.clone(),
        mode: mode_label.clone(),
        status: "starting".to_string(),
        started_at: started_at.clone(),
        finished_at: None,
        workdir_path: generation.workdir_path.clone(),
        backup_dir1: backup_dir1.clone(),
        backup_dir2: backup_dir2_str,
        run_dir,
        log_path,
        child_pid: None,
        child_start_time: None,
        child_executable: None,
        engine_version: None,
        engine_sha256: None,
        engine_platform: None,
        exit_code: None,
        exit_signal: None,
        outcome_kind: None,
        outcome_detail: None,
        conflict_paths: Vec::new(),
        last_verified_phase: "pre_spawn".to_string(),
        files_checked: 0,
        files_transferred: 0,
        bytes_transferred: 0,
        conflict_count: 0,
    };

    // CRITICAL (§12, §14): Persist run-start row (run id, generation, workdir, recovery paths)
    // BEFORE spawning the child process!
    guard.store.insert_run_start(&run)?;

    if is_init {
        mapping.state.execution = Execution::Running;
    } else {
        mapping.state = apply_event(mapping.state, ObservedEvent::RunStarted).state;
    }
    mapping.updated_at = started_at.clone();
    guard.store.update_mapping_runtime(&mapping)?;

    guard.active_run = Some(ActiveRunControl {
        mapping_id: mapping.id.clone(),
        run_id: run_id.clone(),
        stopper: None,
        stop_requested: false,
        force_stopped: false,
    });
    let _ = publish_updated_snapshot(inner, &mut guard)?;
    inner.event_bus.publish_event(ManagerEvent::RunStarted {
        mapping_id: mapping.id.clone(),
        run_id: run_id.clone(),
        mode: mode_label.clone(),
        timestamp: started_at,
    });

    let plan = LaunchPlan {
        run_id: run_id.as_str().to_string(),
        mode: job.mode,
        path1: generation.config.path1.clone(),
        path2: remote_ep.to_path2(),
        config_path: account.config_path,
        workdir: generation.workdir_path,
        filters_file: generation.filters_file_path,
        check_filename: ACCESS_MARKER_FILENAME.to_string(),
        max_delete_percent: mapping.max_delete_percent,
        backup_dir1,
        backup_dir2,
    };

    Ok(PreparedRun {
        run,
        plan,
        mode_label,
        is_init,
    })
}

fn execute_single_job(inner: &Arc<ManagerInner>, job: QueuedJob) {
    let prepared = match prepare_run_before_spawn(inner, &job) {
        Ok(p) => p,
        Err(err) => {
            let mut guard = inner.state.lock().unwrap();
            if let Ok(Some(mut m)) = guard.store.get_mapping(&job.mapping_id) {
                let next = apply_event(
                    m.state,
                    ObservedEvent::RunFinished(RunOutcome::InvalidConfiguration),
                );
                m.state = next.state;
                m.updated_at = inner.clock.now_rfc3339();
                let _ = guard.store.update_mapping_runtime(&m);
                let case = RecoveryCaseRecord {
                    id: format!("rc-{}", Uuid::new_v4()),
                    mapping_id: m.id,
                    run_id: None,
                    status: "open".to_string(),
                    reason_kind: "invalid_config".to_string(),
                    cause: err.cause(),
                    next_step: err.next_step(),
                    workdir_path: None,
                    backup_dir1: None,
                    backup_dir2: None,
                    log_path: None,
                    orphan_pid: None,
                    created_at: inner.clock.now_rfc3339(),
                    resolved_at: None,
                    resolution_note: None,
                };
                let _ = guard.store.insert_recovery_case(&case);
                let _ = publish_updated_snapshot(inner, &mut guard);
            }
            return;
        }
    };

    let (line_tx, line_rx) = mpsc::sync_channel::<OutputLine>(512);
    let forwarder_inner = Arc::clone(inner);
    let fwd_mapping = prepared.run.mapping_id.clone();
    let fwd_run = prepared.run.id.clone();
    let forwarder = thread::spawn(move || {
        while let Ok(line) = line_rx.recv() {
            forwarder_inner
                .event_bus
                .publish_event(ManagerEvent::RunOutputLine {
                    mapping_id: fwd_mapping.clone(),
                    run_id: fwd_run.clone(),
                    line: line.text,
                    timestamp: forwarder_inner.clock.now_rfc3339(),
                });
        }
    });

    let spawn_result = inner.launcher.launch(
        &prepared.plan,
        SpawnOptions {
            limits: OutputLimits::default(),
            line_observer: Some(line_tx),
        },
    );

    let launched = match spawn_result {
        Ok(l) => l,
        Err(err) => {
            let _ = forwarder.join();
            finish_with_launch_error(inner, prepared.run, err);
            return;
        }
    };

    // Write sanitized argv and engine identity under runs/<run-id>/argv.json.
    let argv_doc = serde_json::json!({
        "run_id": prepared.run.id.as_str(),
        "mode": prepared.mode_label,
        "argv": launched.sanitized_argv,
        "engine": {
            "version": launched.engine.version,
            "sha256": launched.engine.sha256,
            "platform": launched.engine.platform,
            "executable": launched.engine.executable,
        }
    });
    if let Ok(bytes) = serde_json::to_vec_pretty(&argv_doc) {
        let _ = write_private_file(&prepared.run.run_dir.join("argv.json"), &bytes);
    }

    // Test fault-injection hook simulating an abrupt manager crash right after
    // spawn BEFORE updating child_pid / start_time in SQLite (§14).
    if launched.simulate_crash_before_pid_record {
        drop(launched);
        let _ = forwarder.join();
        let mut guard = inner.state.lock().unwrap();
        guard.active_run = None;
        inner.idle_cv.notify_all();
        return;
    }

    // Record child identity (PID + start time + executable + engine identity) right after spawn.
    let mut run = prepared.run;
    run.child_pid = Some(launched.identity.pid);
    run.child_start_time = launched.identity.start_time;
    run.child_executable = Some(launched.identity.executable.clone());
    run.engine_version = Some(launched.engine.version.clone());
    run.engine_sha256 = Some(launched.engine.sha256.clone());
    run.engine_platform = Some(launched.engine.platform.clone());
    run.status = "running".to_string();
    run.last_verified_phase = "spawned".to_string();

    let stopper = launched.stop_handle();
    let (pending_force_stop, pending_graceful_stop) = {
        let mut guard = inner.state.lock().unwrap();
        let _ =
            guard
                .store
                .update_run_child_identity(&run.id, &launched.identity, &launched.engine);
        if let Some(active) = guard.active_run.as_mut() {
            active.stopper = Some(Arc::clone(&stopper));
            (
                active.force_stopped,
                active.stop_requested || guard.shutdown,
            )
        } else {
            (false, guard.shutdown)
        }
    };
    if pending_force_stop {
        let _ = stopper.force_stop();
    } else if pending_graceful_stop {
        let _ = stopper.request_stop();
    }

    // Wait for child completion without holding the manager state lock.
    let wait_res = launched.wait();
    let _ = forwarder.join();

    match wait_res {
        Ok(run_out) => {
            finalize_completed_run(inner, run, prepared.is_init, run_out);
        }
        Err(err) => {
            finish_with_launch_error(inner, run, err);
        }
    }
}

fn finish_with_launch_error(inner: &ManagerInner, mut run: RunRecord, err: ManagerError) {
    let now = inner.clock.now_rfc3339();
    run.status = "failed".to_string();
    run.finished_at = Some(now.clone());
    run.outcome_kind = Some("launch_error".to_string());
    run.outcome_detail = Some(err.cause());
    run.last_verified_phase = "launch_failed".to_string();

    let mut guard = inner.state.lock().unwrap();
    let _ = guard.store.update_run_finished(&run);
    let already_force_stopped = guard.active_run.as_ref().is_some_and(|a| a.force_stopped);
    guard.active_run = None;

    if let Ok(Some(mut mapping)) = guard.store.get_mapping(&run.mapping_id) {
        let next = apply_event(
            mapping.state,
            ObservedEvent::RunFinished(RunOutcome::CriticalFailure),
        );
        mapping.state = next.state;
        mapping.last_run_id = Some(run.id.clone());
        mapping.last_run_outcome = Some(RunOutcome::CriticalFailure);
        mapping.last_run_completed_at = Some(now.clone());
        mapping.last_run_summary_text = Some("Failed to launch rclone".to_string());
        mapping.updated_at = now.clone();
        let _ = guard.store.update_mapping_runtime(&mapping);
    }

    if !already_force_stopped {
        let case = RecoveryCaseRecord {
            id: format!("rc-{}", Uuid::new_v4()),
            mapping_id: run.mapping_id.clone(),
            run_id: Some(run.id.clone()),
            status: "open".to_string(),
            reason_kind: "critical_error".to_string(),
            cause: err.cause(),
            next_step: err.next_step(),
            workdir_path: Some(run.workdir_path.clone()),
            backup_dir1: Some(run.backup_dir1.clone()),
            backup_dir2: Some(run.backup_dir2.clone()),
            log_path: Some(run.log_path.clone()),
            orphan_pid: None,
            created_at: now.clone(),
            resolved_at: None,
            resolution_note: None,
        };
        let _ = guard.store.insert_recovery_case(&case);
    }
    let _ = publish_updated_snapshot(inner, &mut guard);
    inner.event_bus.publish_event(ManagerEvent::RunFinished {
        mapping_id: run.mapping_id,
        run_id: run.id,
        outcome: "launch_error".to_string(),
        timestamp: now,
    });
    inner.idle_cv.notify_all();
}

struct RecoverySeed {
    reason_kind: &'static str,
    cause: String,
    next_step: String,
}

fn map_adapter_outcome(
    outcome: &Outcome,
) -> (
    RunOutcome,
    &'static str,
    String,
    Vec<String>,
    Option<RecoverySeed>,
) {
    match outcome {
        Outcome::Success => (
            RunOutcome::Success,
            "success",
            "Synchronization completed cleanly.".to_string(),
            Vec::new(),
            None,
        ),
        Outcome::SuccessWithConflicts { paths } => (
            RunOutcome::SuccessWithConflicts,
            "success_with_conflicts",
            format!("Completed with {} conflict copy/copies.", paths.len()),
            paths.clone(),
            None,
        ),
        Outcome::Retryable { reason } => (
            RunOutcome::RetryableFailure,
            "retryable",
            reason.clone(),
            Vec::new(),
            None,
        ),
        Outcome::AuthFailure { reason } => (
            RunOutcome::AuthenticationFailure,
            "auth_failure",
            reason.clone(),
            Vec::new(),
            None,
        ),
        Outcome::AccessCheckFailed { reason } => (
            RunOutcome::EndpointMismatch,
            "access_check_failed",
            reason.clone(),
            Vec::new(),
            None,
        ),
        Outcome::EndpointMismatch { reason } => (
            RunOutcome::EndpointMismatch,
            "endpoint_mismatch",
            reason.clone(),
            Vec::new(),
            None,
        ),
        Outcome::DeletionGuardAborted { reason } => (
            RunOutcome::CriticalFailure,
            "deletion_guard_aborted",
            reason.clone(),
            Vec::new(),
            Some(RecoverySeed {
                reason_kind: "deletion_guard_aborted",
                cause: format!("Deletion guard aborted sync: {reason}"),
                next_step:
                    "Review the deleted files and recovery copies; FolderSync will not override the deletion limit automatically."
                        .to_string(),
            }),
        ),
        Outcome::InvalidConfig { reason } => (
            RunOutcome::InvalidConfiguration,
            "invalid_config",
            reason.clone(),
            Vec::new(),
            Some(RecoverySeed {
                reason_kind: "invalid_config",
                cause: format!("Invalid rclone configuration or arguments: {reason}"),
                next_step: "Inspect the run log and mapping configuration before retrying."
                    .to_string(),
            }),
        ),
        Outcome::Critical {
            requires_resync,
            reason,
        } => (
            if *requires_resync {
                RunOutcome::CorruptState
            } else {
                RunOutcome::CriticalFailure
            },
            if *requires_resync {
                "corrupt_state"
            } else {
                "critical_error"
            },
            reason.clone(),
            Vec::new(),
            Some(RecoverySeed {
                reason_kind: if *requires_resync {
                    "corrupt_state"
                } else {
                    "critical_error"
                },
                cause: reason.clone(),
                next_step:
                    "Inspect the preserved work directory and run log; do not delete listings or resync blindly."
                        .to_string(),
            }),
        ),
        Outcome::Interrupted {
            graceful,
            reason,
            ..
        } => (
            RunOutcome::Interrupted,
            "interrupted",
            reason.clone(),
            Vec::new(),
            Some(RecoverySeed {
                reason_kind: "interrupted",
                cause: if *graceful {
                    format!("Sync was gracefully interrupted: {reason}")
                } else {
                    format!("Sync was abruptly interrupted: {reason}")
                },
                next_step:
                    "Inspect the preserved work directory and run log before resuming synchronization."
                        .to_string(),
            }),
        ),
        Outcome::IncompleteDiagnosis { reason, .. } => (
            RunOutcome::UnknownFailure,
            "incomplete_diagnosis",
            reason.clone(),
            Vec::new(),
            Some(RecoverySeed {
                reason_kind: "incomplete_diagnosis",
                cause: reason.clone(),
                next_step:
                    "Inspect the full run output because diagnostic capture was truncated or dropped events."
                        .to_string(),
            }),
        ),
        Outcome::Unknown { reason, .. } => (
            RunOutcome::UnknownFailure,
            "unknown_failure",
            reason.clone(),
            Vec::new(),
            Some(RecoverySeed {
                reason_kind: "unknown_failure",
                cause: reason.clone(),
                next_step: "Inspect the run log and preserved work directory before retrying."
                    .to_string(),
            }),
        ),
    }
}

fn finalize_completed_run(
    inner: &ManagerInner,
    mut run: RunRecord,
    is_init: bool,
    run_out: rclone_adapter::RunOutput,
) {
    // Persist NDJSON log output under runs/<run-id>/output.ndjson (0600).
    let mut ndjson = String::new();
    let mut files_checked = 0u64;
    let mut files_transferred = 0u64;
    let mut bytes_transferred = 0u64;

    for line in &run_out.output.lines {
        ndjson.push_str(&line.text);
        ndjson.push('\n');
        if let LogEvent::Structured(ev) = parse_log_line(&line.text)
            && let Some(stats) = ev.stats
        {
            files_checked = files_checked.max(stats.checks);
            files_transferred = files_transferred.max(stats.transfers);
            bytes_transferred = bytes_transferred.max(stats.bytes);
        }
    }
    let _ = write_private_file(&run.log_path, ndjson.as_bytes());

    let classified = classify_run_outcome(&run_out);
    let (core_outcome, outcome_kind, outcome_detail, conflict_paths, recovery_seed) =
        map_adapter_outcome(&classified);

    let now = inner.clock.now_rfc3339();
    run.status = match core_outcome {
        RunOutcome::Success | RunOutcome::SuccessWithConflicts => "completed".to_string(),
        RunOutcome::Interrupted => "interrupted".to_string(),
        _ => "failed".to_string(),
    };
    run.finished_at = Some(now.clone());
    run.exit_code = run_out.exit_status.code;
    run.exit_signal = run_out.exit_status.signal;
    run.outcome_kind = Some(outcome_kind.to_string());
    run.outcome_detail = Some(outcome_detail.clone());
    run.conflict_count = conflict_paths.len() as u64;
    run.conflict_paths = conflict_paths;
    run.last_verified_phase = "completed".to_string();
    run.files_checked = files_checked;
    run.files_transferred = files_transferred;
    run.bytes_transferred = bytes_transferred;

    let outcome_doc = serde_json::json!({
        "run_id": run.id.as_str(),
        "outcome_kind": run.outcome_kind,
        "outcome_detail": run.outcome_detail,
        "exit_code": run.exit_code,
        "exit_signal": run.exit_signal,
        "conflict_paths": run.conflict_paths,
        "files_checked": run.files_checked,
        "files_transferred": run.files_transferred,
        "bytes_transferred": run.bytes_transferred,
        "finished_at": run.finished_at,
    });
    if let Ok(bytes) = serde_json::to_vec_pretty(&outcome_doc) {
        let _ = write_private_file(&run.run_dir.join("outcome.json"), &bytes);
    }

    let summary_text = match core_outcome {
        RunOutcome::Success => format!(
            "0 conflicts · {} files checked · {} transferred",
            run.files_checked, run.files_transferred
        ),
        RunOutcome::SuccessWithConflicts => format!(
            "{} conflicts · {} files checked",
            run.conflict_count, run.files_checked
        ),
        _ => outcome_detail.clone(),
    };

    let mut guard = inner.state.lock().unwrap();
    let _ = guard.store.update_run_finished(&run);
    let already_force_stopped = guard.active_run.as_ref().is_some_and(|a| a.force_stopped);
    guard.active_run = None;

    if let Ok(Some(mut mapping)) = guard.store.get_mapping(&run.mapping_id) {
        if already_force_stopped {
            // Keep ForceStop's Interrupted + RecoveryRequired latch intact.
            let next = apply_event(
                mapping.state,
                ObservedEvent::RunFinished(RunOutcome::Interrupted),
            );
            mapping.state = next.state;
        } else if is_init && matches!(core_outcome, RunOutcome::Success) {
            mapping.active_generation_id = Some(run.generation_id.clone());
            mapping.state.execution = Execution::Idle;
            mapping.state =
                apply_event(mapping.state, ObservedEvent::InitializationCompleted).state;
            // Scheduling stays disabled in Group 3.
            mapping.scheduling_enabled = false;
        } else {
            mapping.state =
                apply_event(mapping.state, ObservedEvent::RunFinished(core_outcome)).state;
        }

        mapping.last_run_id = Some(run.id.clone());
        mapping.last_run_outcome = Some(core_outcome);
        mapping.last_run_completed_at = Some(now.clone());
        mapping.last_run_summary_text = Some(summary_text);
        mapping.updated_at = now.clone();
        let _ = guard.store.update_mapping_runtime(&mapping);
    }

    if !already_force_stopped && let Some(seed) = recovery_seed {
        let case = RecoveryCaseRecord {
            id: format!("rc-{}", Uuid::new_v4()),
            mapping_id: run.mapping_id.clone(),
            run_id: Some(run.id.clone()),
            status: "open".to_string(),
            reason_kind: seed.reason_kind.to_string(),
            cause: seed.cause.clone(),
            next_step: seed.next_step,
            workdir_path: Some(run.workdir_path.clone()),
            backup_dir1: Some(run.backup_dir1.clone()),
            backup_dir2: Some(run.backup_dir2.clone()),
            log_path: Some(run.log_path.clone()),
            orphan_pid: None,
            created_at: now.clone(),
            resolved_at: None,
            resolution_note: None,
        };
        let _ = guard.store.insert_recovery_case(&case);
        inner.event_bus.publish_event(ManagerEvent::RecoveryOpened {
            mapping_id: run.mapping_id.clone(),
            case_id: case.id,
            reason_kind: case.reason_kind,
            cause: seed.cause,
            timestamp: now.clone(),
        });
    }

    let _ = publish_updated_snapshot(inner, &mut guard);
    inner.event_bus.publish_event(ManagerEvent::RunFinished {
        mapping_id: run.mapping_id,
        run_id: run.id,
        outcome: outcome_kind.to_string(),
        timestamp: now,
    });
    inner.idle_cv.notify_all();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_endpoint_db_roundtrip() {
        let ep = RemoteEndpoint::LocalDir(PathBuf::from("/tmp/remote"));
        let (k, v) = ep.to_db_cols();
        assert_eq!(RemoteEndpoint::from_db_cols(&k, &v).unwrap(), ep);
    }
}
