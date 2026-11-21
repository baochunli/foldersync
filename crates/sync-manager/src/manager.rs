//! Core `SyncManager` orchestrator: durable commands, single global mutating
//! job execution, crash/orphan reconciliation, snapshots, and Finder output.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs,
    io::{self, Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use chrono::{DateTime, TimeDelta, Utc};
use rclone_adapter::{
    AuthUrlCallback, BackupDir2, InitPreference, LogEvent, Outcome, OutputLimits, OutputLine,
    Path2, ProcessControl, RunMode, SpawnOptions, StreamKind, classify_run_outcome, parse_log_line,
};
use serde::{Deserialize, Serialize};
use sync_core::{
    ACCESS_MARKER_FILENAME, AccountId, CloudCollection, ComparePolicy, ConfigGeneration,
    ConflictPolicy, DEFAULT_ACTIVE_SCHEDULE_INTERVAL, DEFAULT_MAX_DELETE_PERCENT,
    DeviceCloudCollection, DeviceId, EngineCompatibility, Execution, GenerationId, Intent,
    LastRunSummary, LocalBinding, LocalLayoutEntry, MappingId, MappingState, MaxDeletePercent,
    ObservedEvent, PathResolutionError, PathResolver, Readiness, RecoveryPolicy, RunId, RunOutcome,
    StdPathResolver, SupportedTypesPolicy, TransitionEffect, UserAction, apply_event,
    apply_user_action, available_actions, default_exclusion_patterns, status_text,
    validate_cloud_collection_identity_pair, validate_cloud_collections, validate_local_binding,
    validate_local_layout,
};
use uuid::Uuid;

use crate::{
    accounts::{
        AccountError, AccountService, AccountServiceConfig, CollectionCandidate, CollectionListing,
        DRIVE_COMMAND_TIMEOUT, DriveAccountResult, DriveAuthorization, LockedDrive,
        validate_drive_folder_id,
    },
    clock::{Clock, SystemClock},
    error::ManagerError,
    finder::{
        FinderActionFileOutcome, FinderNotifier, FinderStatusWriter, NoopFinderNotifier,
        ValidatedFinderAction, claim_finder_action_files,
    },
    init::{
        DetailedEndpointInventory, DetectedVolume, EndpointInventory, decide_init_direction,
        detect_volume_identity, detect_volume_with, establish_drive_marker,
        establish_local_pair_markers, inspect_drive_endpoint, inspect_local_endpoint,
        inspect_local_endpoint_detailed, render_filters_file, verify_drive_marker,
        verify_endpoint_access_marker,
    },
    launcher::{BisyncLauncher, LaunchPlan, LaunchStopper, RealBisyncLauncher},
    platform::{
        DiskSpaceInfo, MountedVolume, OsProcessInspector, OsVolumeProbe, ProcessInspector,
        ProcessProbeResult, StateLock, VolumeProbe, ensure_private_dir_under,
        open_private_write_file_under, reject_symlinks_under_root, resolve_absolute_path,
        resolve_state_dir, space_and_writability, validate_safe_path_id,
        write_private_file_atomic_under, write_private_file_under,
    },
    scheduler::{
        LocalChangeWatcher, MAX_RETRY_COUNT, NativeWatcher, Schedule, Schedules, backoff,
        network_addresses,
    },
    snapshot::{
        DEFAULT_EVENT_BUFFER_CAPACITY, EventBus, FolderSizeSnapshot, FolderSizeStatus,
        ManagerEvent, MappingCounters, MappingStatusSnapshot, SnapshotReceiver, StatusSnapshot,
    },
    store::{
        AccountRecord, DeviceRecord, GenerationRecord, MappingRecord, RecoveryCaseRecord,
        RunRecord, SqliteStateStore,
    },
};

const ENGINE_PROFILE_VERSION: &str = "v1";
const SCHEDULE_TICK: Duration = Duration::from_secs(1);
const VOLUME_POLL_INTERVAL: Duration = Duration::from_secs(3);
const MIN_FREE_BYTES: u64 = 100 * 1024 * 1024;

fn plain_readiness_label(readiness: Readiness) -> &'static str {
    match readiness {
        Readiness::Unconfigured => "not set up",
        Readiness::NeedsInitialization => "waiting for first sync",
        Readiness::Ready => "ready",
        Readiness::VolumeUnavailable => "waiting for drive",
        Readiness::AuthenticationRequired => "sign-in required",
        Readiness::EndpointChanged => "folder changed",
        Readiness::RecoveryRequired => "recovery required",
    }
}

fn plain_execution_label(execution: Execution) -> &'static str {
    match execution {
        Execution::Idle => "idle",
        Execution::Queued => "queued",
        Execution::Running => "running",
        Execution::Stopping => "stopping",
        Execution::Interrupted => "interrupted",
    }
}

fn plain_recovery_reason_label(reason_kind: &str) -> &'static str {
    match reason_kind {
        "orphan_running" => "earlier sync still running",
        "orphan_ambiguous" => "unresolved earlier sync process",
        "deletion_guard_aborted" => "stopped by the deletion limit",
        "interrupted" | "force_stopped" | "shutdown_timeout" | "volume_disconnected" => {
            "interrupted sync"
        }
        _ => "recovery required",
    }
}

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
    /// Darwin notification sink invoked after every Finder `status.json` write.
    pub finder_notifier: Arc<dyn FinderNotifier>,
    /// Periodic Finder `status.json` heartbeat refresh interval (well under the 300s TTL).
    pub finder_heartbeat_interval: Option<Duration>,
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
    /// Creates a config rooted at an explicit state directory with safe defaults
    /// (uses [`NoopFinderNotifier`] by default so tests never post live Darwin notifications).
    pub fn with_state_dir(state_dir: impl Into<PathBuf>) -> Self {
        Self {
            state_dir: Some(state_dir.into()),
            finder_status_dir: None,
            finder_notifier: Arc::new(NoopFinderNotifier),
            finder_heartbeat_interval: Some(Duration::from_secs(60)),
            clock: Arc::new(SystemClock),
            process_inspector: Arc::new(OsProcessInspector),
            launcher: Arc::new(RealBisyncLauncher::new(None)),
            event_buffer_capacity: DEFAULT_EVENT_BUFFER_CAPACITY,
        }
    }
}

/// Non-mutating preflight preview returned by [`SyncManager::preflight_mapping_preview`],
/// sharing the exact same layout validation, volume detection, and endpoint inspection
/// logic that initialization runs (§9, HIGH #2).
#[derive(Debug, Clone)]
pub struct PreflightPreview {
    pub detected_volume: Result<DetectedVolume, String>,
    pub layout_error: Option<String>,
    pub local_inventory: Result<DetailedEndpointInventory, String>,
    pub remote_inventory: Result<DetailedEndpointInventory, String>,
    pub direction: Result<InitPreference, String>,
}

/// Proposed exclusion changes and an advisory first-sync preview.
#[derive(Debug, Clone)]
pub struct FilterChangePreview {
    mapping_id: MappingId,
    previous_generation_id: GenerationId,
    effective_exclusions: Vec<String>,
    added_exclusions: Vec<String>,
    removed_exclusions: Vec<String>,
    preflight: PreflightPreview,
}

impl FilterChangePreview {
    pub fn mapping_id(&self) -> &MappingId {
        &self.mapping_id
    }

    pub fn previous_generation_id(&self) -> &GenerationId {
        &self.previous_generation_id
    }

    pub fn effective_exclusions(&self) -> &[String] {
        &self.effective_exclusions
    }

    pub fn added_exclusions(&self) -> &[String] {
        &self.added_exclusions
    }

    pub fn removed_exclusions(&self) -> &[String] {
        &self.removed_exclusions
    }

    pub fn preflight(&self) -> &PreflightPreview {
        &self.preflight
    }
}

fn effective_exclusions(extra: &BTreeSet<String>) -> Result<BTreeSet<String>, ManagerError> {
    let mut patterns = default_exclusion_patterns();
    for pattern in extra
        .iter()
        .map(|pattern| pattern.trim())
        .filter(|pattern| !pattern.is_empty())
    {
        if pattern.chars().any(char::is_control) {
            return Err(ManagerError::PreflightRejected {
                cause: "Exclusion patterns must be single lines without control characters."
                    .to_owned(),
                next_step: "Enter each exclusion separately.".to_owned(),
            });
        }
        patterns.insert(pattern.to_owned());
    }
    Ok(patterns)
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
    /// Builds a Drive mapping request. The manager resolves the remote name, recovery
    /// path and ancestry using the selected account before saving the mapping.
    pub fn drive(
        id: impl Into<MappingId>,
        account_id: impl Into<AccountId>,
        collection: CloudCollection,
        local_dir: impl AsRef<Path>,
        local_recovery_root: impl AsRef<Path>,
    ) -> Result<Self, ManagerError> {
        let id = id.into();
        let account_id = account_id.into();
        validate_safe_path_id("mapping_id", id.as_str())?;
        validate_safe_path_id("account_id", account_id.as_str())?;
        let detected = detect_volume_identity(local_dir.as_ref())?;
        let remote_endpoint = RemoteEndpoint::DriveRemote {
            remote: String::new(),
            path: collection.display_path.clone(),
        };
        Ok(Self {
            id,
            account_id,
            collection,
            local_binding: LocalBinding {
                volume: detected.identity,
                mount_path: detected.mount_path,
                relative_dir: detected.relative_dir,
            },
            remote_endpoint,
            local_recovery_root: resolve_absolute_path(
                "local_recovery_root",
                local_recovery_root.as_ref(),
            )?,
            cloud_recovery_path: String::new(),
            extra_exclusions: BTreeSet::new(),
            max_delete_percent: DEFAULT_MAX_DELETE_PERCENT,
            schedule_interval: DEFAULT_ACTIVE_SCHEDULE_INTERVAL,
        })
    }

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
        let id = id.into();
        let account_id = account_id.into();
        validate_safe_path_id("mapping_id", id.as_str())?;
        validate_safe_path_id("account_id", account_id.as_str())?;

        let local_dir = local_dir.into();
        let remote_dir = remote_dir.into();
        let detected = detect_volume_identity(&local_dir)?;
        let remote_meta = fs::symlink_metadata(&remote_dir).map_err(|err| ManagerError::Io {
            cause: format!(
                "Remote fixture folder `{}` does not exist or cannot be accessed: {err}.",
                remote_dir.display()
            ),
            next_step:
                "Select an existing remote destination folder (FolderSync never auto-creates missing roots)."
                    .to_string(),
        })?;
        if remote_meta.file_type().is_symlink() || !remote_meta.is_dir() {
            return Err(ManagerError::PreflightRejected {
                cause: format!(
                    "Remote fixture folder `{}` must be an ordinary directory, not a file or symbolic link.",
                    remote_dir.display()
                ),
                next_step: "Choose an ordinary directory for this mapping.".to_string(),
            });
        }
        let canonical_remote = fs::canonicalize(&remote_dir).map_err(|err| ManagerError::Io {
            cause: format!(
                "Remote fixture folder `{}` does not exist or cannot be accessed: {err}.",
                remote_dir.display()
            ),
            next_step:
                "Select an existing remote destination folder (FolderSync never auto-creates missing roots)."
                    .to_string(),
        })?;

        let local_recovery_root =
            resolve_absolute_path("local_recovery_root", &local_recovery_root.into())?;
        let remote_recovery_root =
            resolve_absolute_path("remote_recovery_root", &remote_recovery_root.into())?;

        let collection_path = collection_path.into();
        let folder_id = format!(
            "local-col-{}-{}",
            account_id.as_str(),
            collection_path.replace('/', "_")
        );
        Ok(Self {
            id,
            account_id,
            collection: CloudCollection {
                display_path: collection_path,
                folder_id,
                ancestor_ids: vec!["local-root".to_string()],
            },
            local_binding: LocalBinding {
                volume: detected.identity,
                mount_path: detected.mount_path,
                relative_dir: detected.relative_dir,
            },
            remote_endpoint: RemoteEndpoint::LocalDir(canonical_remote),
            local_recovery_root,
            cloud_recovery_path: remote_recovery_root.to_string_lossy().into_owned(),
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
    /// Persists or updates a resumable draft mapping in `Readiness::Unconfigured`.
    SaveDraftMapping(Box<AddMappingCommand>),
    /// Synchronous preflight (off-mutex) + queue initialization (used by CLI & existing tests).
    Initialize(InitializeCommand),
    /// Asynchronous initialization: returns immediately and runs inventory/engine
    /// preflight on the background worker thread.
    StartInitialize(InitializeCommand),
    RunNow {
        mapping_id: MappingId,
    },
    SyncReadyMappings,
    Pause {
        mapping_id: MappingId,
    },
    Resume {
        mapping_id: MappingId,
    },
    SetSchedulingPaused {
        paused: bool,
    },
    StopRun {
        mapping_id: MappingId,
    },
    ForceStop {
        mapping_id: MappingId,
    },
    InspectRecovery {
        mapping_id: MappingId,
    },
    AcknowledgeConflicts {
        mapping_id: MappingId,
    },
    CheckAgain {
        mapping_id: MappingId,
    },
    CompleteRecovery {
        mapping_id: MappingId,
        resume_and_run: bool,
    },
    RemoveMapping {
        mapping_id: MappingId,
    },
    DiscardDraft {
        mapping_id: MappingId,
    },
    SetScheduleInterval {
        mapping_id: MappingId,
        interval_secs: u64,
    },
    RemoveAccount {
        account_id: AccountId,
    },
}

/// Which side a recovery copy was preserved on (local backup_dir1 vs remote backup_dir2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecoveryCopySide {
    Local,
    Remote,
}

/// One preserved recovery copy discovered under a run's backup directories (§12).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryCopyEntry {
    pub side: RecoveryCopySide,
    pub relative_path: String,
    pub full_path: PathBuf,
    pub size_bytes: u64,
}

/// Outcome of a bounded controlled shutdown (§14, spec §5.9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlledShutdownOutcome {
    /// No active run was in flight, or the active run finished cleanly within the timeout.
    Clean,
    /// The bounded wait expired while an active run was still stopping; an interrupted
    /// outcome and recovery case were persisted before returning.
    TimedOutInterrupted {
        mapping_id: MappingId,
        run_id: RunId,
    },
}

/// Result of consuming one Finder action file from `<finder_dir>/actions/*.json` (ADR 0002).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinderActionDispatch {
    Executed {
        file_name: String,
        action: ValidatedFinderAction,
    },
    UiRequested {
        file_name: String,
        action: ValidatedFinderAction,
    },
    Rejected {
        file_name: String,
        reason: String,
    },
    CommandFailed {
        file_name: String,
        action: ValidatedFinderAction,
        cause: String,
    },
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
    /// True only when endpoint root & access-marker verification passed during inspection.
    pub identities_verified: bool,
    /// Human-readable result of endpoint identity verification.
    pub identity_check_detail: String,
}

/// Structured detail accompanying a durable command acceptance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandOutcome {
    MappingAdded {
        mapping_id: MappingId,
    },
    DraftSaved {
        mapping_id: MappingId,
    },
    InitializationQueued {
        mapping_id: MappingId,
        direction: InitPreference,
    },
    InitializationPreflightQueued {
        mapping_id: MappingId,
    },
    RunQueued {
        mapping_id: MappingId,
    },
    RunPrioritized {
        mapping_id: MappingId,
    },
    ReadyMappingsQueued {
        mapping_ids: Vec<MappingId>,
    },
    Paused {
        mapping_id: MappingId,
    },
    Resumed {
        mapping_id: MappingId,
    },
    SchedulingPauseUpdated {
        paused: bool,
    },
    StopRequested {
        mapping_id: MappingId,
    },
    ForceStopped {
        mapping_id: MappingId,
    },
    RecoveryInspected(Box<RecoveryReport>),
    ConflictsAcknowledged {
        mapping_id: MappingId,
    },
    CheckAgainVerified {
        mapping_id: MappingId,
    },
    RecoveryCompleted {
        mapping_id: MappingId,
        resumed: bool,
    },
    MappingRemoved {
        mapping_id: MappingId,
    },
    ScheduleIntervalUpdated {
        mapping_id: MappingId,
        interval_secs: u64,
    },
    AccountRemoved {
        account_id: AccountId,
    },
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
        resolve_absolute_path("path", path).map_err(|err| PathResolutionError {
            path: path.to_path_buf(),
            message: err.cause(),
        })
    }
}

fn paths_overlap(a: &Path, b: &Path) -> bool {
    a == b || a.starts_with(b) || b.starts_with(a)
}

/// Validates that a recovery root candidate is not an existing non-directory
/// (regular file, symlink, etc.) and that its longest existing ancestor is a
/// directory so `create_dir_all` can safely create it later.
fn validate_recovery_root_dir_candidate(field: &str, path: &Path) -> Result<(), ManagerError> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.is_dir() {
                return Err(ManagerError::PreflightRejected {
                    cause: format!(
                        "{field} `{}` exists and is not an ordinary directory.",
                        path.display()
                    ),
                    next_step:
                        "Choose a directory path for recovery copies (not an existing file or symbolic link)."
                            .to_string(),
                });
            }
            return Ok(());
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(ManagerError::Io {
                cause: format!("Could not inspect {field} `{}`: {err}.", path.display()),
                next_step: "Verify permissions on the recovery directory path.".to_string(),
            });
        }
    }

    let mut cur = path.parent();
    while let Some(anc) = cur {
        match fs::symlink_metadata(anc) {
            Ok(meta) => {
                if !meta.is_dir() {
                    return Err(ManagerError::PreflightRejected {
                        cause: format!(
                            "Parent `{}` of {field} `{}` is not a directory.",
                            anc.display(),
                            path.display()
                        ),
                        next_step: "Choose a recovery directory under an existing directory."
                            .to_string(),
                    });
                }
                return Ok(());
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                cur = anc.parent();
            }
            Err(err) => {
                return Err(ManagerError::Io {
                    cause: format!(
                        "Could not inspect ancestor `{}` of {field}: {err}.",
                        anc.display()
                    ),
                    next_step: "Verify permissions on the recovery directory path.".to_string(),
                });
            }
        }
    }
    Ok(())
}

struct LayoutCandidate<'a> {
    mapping_id: &'a MappingId,
    local_binding: &'a LocalBinding,
    remote_endpoint: RemoteEndpoint,
    local_recovery_root: PathBuf,
    cloud_recovery_path: &'a str,
}

/// Validates the COMPLETE layout across all mappings: Path1, local-fixture
/// Path2, local recovery roots, local remote-recovery roots, `state_dir`, and
/// `finder_dir`, BEFORE creating any recovery directory or writing any marker.
fn validate_complete_layout(
    existing: &[MappingRecord],
    extra: Option<&LayoutCandidate<'_>>,
    state_dir: &Path,
    finder_dir: &Path,
) -> Result<(), ManagerError> {
    let mut all_candidates: Vec<LayoutCandidate<'_>> = Vec::with_capacity(existing.len() + 1);
    for m in existing {
        let ep = RemoteEndpoint::from_db_cols(&m.remote_endpoint_kind, &m.remote_endpoint_value)?;
        all_candidates.push(LayoutCandidate {
            mapping_id: &m.id,
            local_binding: &m.local_binding,
            remote_endpoint: ep,
            local_recovery_root: m.local_recovery_root.clone(),
            cloud_recovery_path: m.cloud_recovery_path.as_str(),
        });
    }
    if let Some(cand) = extra {
        all_candidates.push(LayoutCandidate {
            mapping_id: cand.mapping_id,
            local_binding: cand.local_binding,
            remote_endpoint: cand.remote_endpoint.clone(),
            local_recovery_root: cand.local_recovery_root.clone(),
            cloud_recovery_path: cand.cloud_recovery_path,
        });
    }

    // 1. Run sync-core's local layout validation for all Path1 + local_recovery_root + state_dir.
    let layout_entries: Vec<LocalLayoutEntry> = all_candidates
        .iter()
        .map(|c| LocalLayoutEntry {
            mapping_id: c.mapping_id.clone(),
            local_binding: c.local_binding.clone(),
            local_recovery_root: c.local_recovery_root.clone(),
        })
        .collect();
    validate_local_layout(&layout_entries, state_dir, &AncestorAwarePathResolver)
        .map_err(ManagerError::ValidationList)?;

    // 2. Resolve every sync tree (Path1 + LocalDir Path2), every recovery root, state_dir, and finder_dir.
    let resolved_state = resolve_absolute_path("state_dir", state_dir)?;
    let resolved_finder = resolve_absolute_path("finder_status_dir", finder_dir)?;

    struct ResolvedEntry {
        id: String,
        path1: PathBuf,
        path2: Option<PathBuf>,
        local_rec: PathBuf,
        remote_rec: Option<PathBuf>,
    }

    let mut resolved = Vec::with_capacity(all_candidates.len());
    for c in &all_candidates {
        let path1 = resolve_absolute_path("path1", &c.local_binding.resolved_path())?;
        validate_recovery_root_dir_candidate("local_recovery_root", &c.local_recovery_root)?;
        let local_rec = resolve_absolute_path("local_recovery_root", &c.local_recovery_root)?;
        let (path2, remote_rec) = match &c.remote_endpoint {
            RemoteEndpoint::LocalDir(p2) => {
                let r_p2 = resolve_absolute_path("path2", p2)?;
                let raw_rec2 = Path::new(c.cloud_recovery_path);
                validate_recovery_root_dir_candidate("remote_recovery_root", raw_rec2)?;
                let r_rec2 = resolve_absolute_path("remote_recovery_root", raw_rec2)?;
                (Some(r_p2), Some(r_rec2))
            }
            RemoteEndpoint::DriveRemote { .. } | RemoteEndpoint::OtherRemote { .. } => (None, None),
        };
        resolved.push(ResolvedEntry {
            id: c.mapping_id.as_str().to_string(),
            path1,
            path2,
            local_rec,
            remote_rec,
        });
    }

    let reject = |cause: String, next_step: &str| {
        Err(ManagerError::PreflightRejected {
            cause,
            next_step: next_step.to_string(),
        })
    };

    // Check every mapping's sync roots & recovery roots against state_dir, finder_dir, and each other.
    for entry in &resolved {
        // Local recovery vs state_dir / finder_dir
        if paths_overlap(&entry.local_rec, &resolved_state) {
            return reject(
                format!(
                    "Local recovery directory `{}` for mapping `{}` overlaps FolderSync state directory `{}`.",
                    entry.local_rec.display(),
                    entry.id,
                    resolved_state.display()
                ),
                "Choose a local recovery directory outside FolderSync's state directory.",
            );
        }
        if paths_overlap(&entry.path1, &resolved_finder) {
            return reject(
                format!(
                    "Local folder `{}` for mapping `{}` overlaps Finder status directory `{}`.",
                    entry.path1.display(),
                    entry.id,
                    resolved_finder.display()
                ),
                "Keep synchronized folders separate from FolderSync's Finder handoff directory.",
            );
        }

        if let Some(p2) = &entry.path2 {
            if paths_overlap(&entry.path1, p2) {
                return reject(
                    format!(
                        "Local folder `{}` and remote destination `{}` overlap for mapping `{}`.",
                        entry.path1.display(),
                        p2.display(),
                        entry.id
                    ),
                    "Choose separate, non-nested directories for local and remote endpoints.",
                );
            }
            if paths_overlap(p2, &resolved_state) {
                return reject(
                    format!(
                        "Remote destination `{}` for mapping `{}` overlaps FolderSync state directory `{}`.",
                        p2.display(),
                        entry.id,
                        resolved_state.display()
                    ),
                    "Choose a remote destination outside FolderSync's private state directory.",
                );
            }
            if paths_overlap(p2, &resolved_finder) {
                return reject(
                    format!(
                        "Remote destination `{}` for mapping `{}` overlaps Finder status directory `{}`.",
                        p2.display(),
                        entry.id,
                        resolved_finder.display()
                    ),
                    "Choose a remote destination outside the Finder handoff directory.",
                );
            }
        }

        if let Some(rec2) = &entry.remote_rec {
            if paths_overlap(rec2, &resolved_state) {
                return reject(
                    format!(
                        "Remote recovery directory `{}` for mapping `{}` overlaps FolderSync state directory `{}`.",
                        rec2.display(),
                        entry.id,
                        resolved_state.display()
                    ),
                    "Choose a remote recovery directory outside FolderSync's state directory.",
                );
            }
            // Prevent backup_dir1 and backup_dir2 overlapping within the same mapping.
            if paths_overlap(&entry.local_rec, rec2) {
                return reject(
                    format!(
                        "Local recovery directory `{}` and remote recovery directory `{}` overlap for mapping `{}`.",
                        entry.local_rec.display(),
                        rec2.display(),
                        entry.id
                    ),
                    "Choose disjoint local and remote recovery directories.",
                );
            }
        }
    }

    // Cross-mapping checks: every sync root (Path1 or Path2) must be disjoint from
    // every other sync root (Path1 or Path2) and from every recovery root (local or remote).
    for (i, a) in resolved.iter().enumerate() {
        for (j, b) in resolved.iter().enumerate() {
            if i != j {
                if let (Some(p2a), Some(p2b)) = (&a.path2, &b.path2)
                    && paths_overlap(p2a, p2b)
                {
                    return reject(
                        format!(
                            "Remote destinations `{}` (`{}`) and `{}` (`{}`) overlap.",
                            p2a.display(),
                            a.id,
                            p2b.display(),
                            b.id
                        ),
                        "Choose disjoint remote directories for each mapping.",
                    );
                }
                if let Some(p2b) = &b.path2
                    && paths_overlap(&a.path1, p2b)
                {
                    return reject(
                        format!(
                            "Local folder `{}` (`{}`) overlaps remote destination `{}` (`{}`).",
                            a.path1.display(),
                            a.id,
                            p2b.display(),
                            b.id
                        ),
                        "Choose disjoint local and remote directories across all mappings.",
                    );
                }
            }
            // Recovery directories vs ALL sync roots (including same mapping).
            if let Some(p2a) = &a.path2
                && paths_overlap(p2a, &b.local_rec)
            {
                return reject(
                    format!(
                        "Local recovery directory `{}` (`{}`) overlaps remote destination `{}` (`{}`).",
                        b.local_rec.display(),
                        b.id,
                        p2a.display(),
                        a.id
                    ),
                    "Place recovery directories outside every synchronized tree.",
                );
            }
            if let Some(rec2b) = &b.remote_rec {
                if paths_overlap(&a.path1, rec2b) {
                    return reject(
                        format!(
                            "Remote recovery directory `{}` (`{}`) overlaps local folder `{}` (`{}`).",
                            rec2b.display(),
                            b.id,
                            a.path1.display(),
                            a.id
                        ),
                        "Place remote recovery directories outside every synchronized tree.",
                    );
                }
                if let Some(p2a) = &a.path2
                    && paths_overlap(p2a, rec2b)
                {
                    return reject(
                        format!(
                            "Remote recovery directory `{}` (`{}`) overlaps remote destination `{}` (`{}`).",
                            rec2b.display(),
                            b.id,
                            p2a.display(),
                            a.id
                        ),
                        "Place remote recovery directories outside every synchronized tree.",
                    );
                }
            }
        }
    }

    Ok(())
}

#[derive(Debug, Clone)]
enum QueuedJobKind {
    Prepared {
        generation_id: GenerationId,
        mode: RunMode,
    },
    InitializePreflight {
        expected_direction: Option<InitPreference>,
        attempt_id: u64,
    },
}

#[derive(Debug, Clone)]
struct QueuedJob {
    automatic: bool,
    mapping_id: MappingId,
    kind: QueuedJobKind,
}

struct ActiveRunControl {
    mapping_id: MappingId,
    run_id: RunId,
    stopper: Option<Arc<dyn LaunchStopper>>,
    stop_requested: bool,
    volume_lost: bool,
    force_stopped: bool,
    timed_out_during_shutdown: bool,
}

const FOLDER_SIZE_CACHE_TTL: TimeDelta = TimeDelta::minutes(5);
const FOLDER_SIZE_CACHE_CAPACITY: usize = 128;
const AUXILIARY_STOP_TIMEOUT: Duration = Duration::from_secs(1);

/// Handle for an asynchronous account or sizing operation. Dropping the
/// handle leaves the operation running. Call `cancel` to request a stop.
#[derive(Debug)]
pub struct OperationHandle<T> {
    control: ProcessControl,
    result: mpsc::Receiver<Result<T, ManagerError>>,
}

impl<T> OperationHandle<T> {
    pub fn cancel(&self) {
        self.control.cancel();
    }

    pub fn wait(self) -> Result<T, ManagerError> {
        self.result.recv().map_err(|_| ManagerError::Io {
            cause: "The account operation ended before reporting its result.".into(),
            next_step: "Check the current status and try again.".into(),
        })?
    }
}

fn restore_size_status(
    guard: &mut SharedState,
    key: &(AccountId, String),
    previous: Option<FolderSizeSnapshot>,
) {
    if let Some(previous) = previous {
        guard.folder_sizes.insert(key.clone(), previous);
    } else {
        guard.folder_sizes.remove(key);
    }
}

fn cancel_auxiliary_operations(guard: &SharedState) {
    if let Some(control) = &guard.sizing {
        control.cancel();
    }
    if let Some((_, control)) = &guard.authorization {
        control.cancel();
    }
}

pub(crate) fn drive_error(error: AccountError) -> ManagerError {
    match error {
        AccountError::Manager(error) => *error,
        error => ManagerError::PreflightRejected {
            cause: error.cause(),
            next_step: error.next_step(),
        },
    }
}

fn account_worker_error(error: io::Error) -> ManagerError {
    ManagerError::Io {
        cause: format!("Could not start the account worker: {error}"),
        next_step: "Check system resources and try again.".into(),
    }
}

struct SharedState {
    store: SqliteStateStore,
    volumes: BTreeMap<MappingId, BoundVolume>,
    last_space_snapshot: DateTime<Utc>,
    device: DeviceRecord,
    snapshot_seq: u64,
    latest_snapshot: Arc<StatusSnapshot>,
    job_queue: VecDeque<QueuedJob>,
    active_run: Option<ActiveRunControl>,
    preflight_mapping: Option<MappingId>,
    next_init_attempt_id: u64,
    active_init_attempt: Option<(MappingId, u64)>,
    cancelled_init_attempts: BTreeSet<u64>,
    scheduling_paused: bool,
    schedules: Schedules,
    folder_sizes: BTreeMap<(AccountId, String), FolderSizeSnapshot>,
    sizing: Option<ProcessControl>,
    authorization: Option<(AccountId, ProcessControl)>,
    worker_busy: bool,
    shutdown: bool,
    last_terminal_error: Option<(String, String)>,
    storage_blocked_mappings: BTreeSet<MappingId>,
}

struct ManagerInner {
    state_dir: PathBuf,
    accounts: AccountService,
    finder_writer: FinderStatusWriter,
    finder_heartbeat_interval: Option<Duration>,
    timed_out_shutdown: AtomicBool,
    clock: Arc<dyn Clock>,
    process_inspector: Arc<dyn ProcessInspector>,
    volume_probe: Arc<dyn VolumeProbe>,
    launcher: Arc<dyn BisyncLauncher>,
    event_bus: EventBus,
    state: Mutex<SharedState>,
    worker_cv: Condvar,
    idle_cv: Condvar,
}

/// Single-process FolderSync manager owning the state lock, SQLite store,
/// single global sync worker, snapshot stream, and Finder status writer.
///
/// Shutdown joins the volume watcher and worker before `inner`
/// closes the SQLite connection. `_lock` releases the OS
/// `flock` LAST so no second instance can open SQLite during teardown.
pub struct SyncManager {
    worker_thread: Mutex<Option<JoinHandle<()>>>,
    volume_thread: Mutex<Option<JoinHandle<()>>>,
    scheduler_thread: Mutex<Option<JoinHandle<()>>>,
    inner: Arc<ManagerInner>,
    _lock: Arc<StateLock>,
}

impl SyncManager {
    /// Acquires the exclusive OS lock, opens/migrates SQLite, reconciles any
    /// unfinished runs from a previous crash, publishes the initial snapshot
    /// (including Finder `status.json`), and starts the background worker.
    pub fn open(config: ManagerConfig) -> Result<Self, ManagerError> {
        Self::open_with_volume_probe(config, Arc::new(OsVolumeProbe))
    }

    /// Opens a manager with an injectable mounted-volume probe.
    pub fn open_with_volume_probe(
        config: ManagerConfig,
        volume_probe: Arc<dyn VolumeProbe>,
    ) -> Result<Self, ManagerError> {
        Self::open_with_watcher(config, volume_probe, Box::new(NativeWatcher::new()))
    }

    /// Opens with injected volume and local-change sources.
    pub fn open_with_watcher(
        config: ManagerConfig,
        volume_probe: Arc<dyn VolumeProbe>,
        watcher: Box<dyn LocalChangeWatcher>,
    ) -> Result<Self, ManagerError> {
        Self::open_inner(config, volume_probe, watcher, None)
    }

    /// Opens with injected account dependencies, including the process runner
    /// and credential store. The account service uses the manager's state directory.
    pub fn open_with_account_config(
        config: ManagerConfig,
        accounts: AccountServiceConfig,
    ) -> Result<Self, ManagerError> {
        Self::open_inner(
            config,
            Arc::new(OsVolumeProbe),
            Box::new(NativeWatcher::new()),
            Some(accounts),
        )
    }

    fn open_inner(
        config: ManagerConfig,
        volume_probe: Arc<dyn VolumeProbe>,
        watcher: Box<dyn LocalChangeWatcher>,
        accounts: Option<AccountServiceConfig>,
    ) -> Result<Self, ManagerError> {
        let state_dir = resolve_state_dir(config.state_dir.as_deref())?;

        let lock = Arc::new(StateLock::acquire(&state_dir)?);

        ensure_private_dir_under("accounts", &state_dir, &state_dir.join("accounts"))?;
        ensure_private_dir_under("mappings", &state_dir, &state_dir.join("mappings"))?;
        ensure_private_dir_under("runs", &state_dir, &state_dir.join("runs"))?;
        ensure_private_dir_under(
            "recovery-records",
            &state_dir,
            &state_dir.join("recovery-records"),
        )?;

        let finder_dir = match config.finder_status_dir {
            Some(dir) => {
                // Refuse symlinked handoff root or symlinked immediate parent BEFORE canonicalizing (HIGH #10).
                if let Some(parent) = dir.parent()
                    && let Ok(meta) = fs::symlink_metadata(parent)
                    && (meta.file_type().is_symlink() || !meta.is_dir())
                {
                    return Err(ManagerError::PreflightRejected {
                        cause: format!(
                            "Refused symbolic link or non-directory Finder handoff parent `{}`.",
                            parent.display()
                        ),
                        next_step: "Use an ordinary private directory for Finder status handoff."
                            .to_string(),
                    });
                }
                if let Ok(meta) = fs::symlink_metadata(&dir)
                    && (meta.file_type().is_symlink() || !meta.is_dir())
                {
                    return Err(ManagerError::PreflightRejected {
                        cause: format!(
                            "Refused symbolic link or non-directory Finder handoff directory `{}`.",
                            dir.display()
                        ),
                        next_step: "Use an ordinary private directory for Finder status handoff."
                            .to_string(),
                    });
                }
                resolve_absolute_path("finder_status_dir", &dir)?
            }
            None => ensure_private_dir_under("finder", &state_dir, &state_dir.join("finder"))?,
        };

        let finder_writer = FinderStatusWriter::with_notifier(finder_dir, config.finder_notifier);

        let mut store = SqliteStateStore::open(&state_dir.join("state.sqlite"))?;
        let now = config.clock.now_rfc3339();
        let default_device = DeviceRecord {
            id: DeviceId::new(format!("device-{}", Uuid::new_v4())),
            name: default_device_name(),
            created_at: now,
        };
        let device = store.ensure_device(&default_device)?;

        let event_bus = EventBus::new(config.event_buffer_capacity);
        reconcile_startup_state(
            &state_dir,
            &mut store,
            config.process_inspector.as_ref(),
            config.clock.as_ref(),
            &event_bus,
        )?;

        let scheduling_paused = store.scheduling_paused(&device.id)?;
        let opened_at = config.clock.now_utc();
        let schedules = restore_schedules(&store, opened_at)?;

        let initial_snapshot = Arc::new(StatusSnapshot {
            sequence: 0,
            generated_at: config.clock.now_rfc3339(),
            active_job_mapping_id: None,
            queue_order: Vec::new(),
            scheduling_paused,
            mappings: Vec::new(),
            folder_sizes: Vec::new(),
        });

        let mut account_config =
            accounts.unwrap_or_else(|| AccountServiceConfig::with_state_dir(&state_dir));
        account_config.state_dir = state_dir.clone();
        account_config.acquire_state_lock = false;
        account_config.clock = Arc::clone(&config.clock);
        let accounts = AccountService::from_config(account_config).map_err(drive_error)?;
        let inner = Arc::new(ManagerInner {
            state_dir,
            accounts,
            finder_writer,
            finder_heartbeat_interval: config.finder_heartbeat_interval,
            timed_out_shutdown: AtomicBool::new(false),
            clock: config.clock,
            process_inspector: config.process_inspector,
            volume_probe,
            launcher: config.launcher,
            event_bus,
            state: Mutex::new(SharedState {
                store,
                volumes: BTreeMap::new(),
                last_space_snapshot: opened_at,
                device,
                snapshot_seq: 0,
                latest_snapshot: initial_snapshot,
                job_queue: VecDeque::new(),
                active_run: None,
                preflight_mapping: None,
                next_init_attempt_id: 0,
                active_init_attempt: None,
                cancelled_init_attempts: BTreeSet::new(),
                scheduling_paused,
                schedules,
                folder_sizes: BTreeMap::new(),
                sizing: None,
                authorization: None,
                worker_busy: false,
                shutdown: false,
                last_terminal_error: None,
                storage_blocked_mappings: BTreeSet::new(),
            }),
            worker_cv: Condvar::new(),
            idle_cv: Condvar::new(),
        });

        refresh_volumes(&inner)?;
        {
            let mut guard = inner.state.lock().unwrap();
            if guard.snapshot_seq == 0 {
                publish_updated_snapshot(&inner, &mut guard)?;
            }
        }
        let worker_inner = Arc::clone(&inner);
        let handle = thread::Builder::new()
            .name("foldersync-worker".to_string())
            .spawn(move || worker_loop(worker_inner))
            .map_err(|err| ManagerError::Io {
                cause: format!("Failed to spawn FolderSync worker thread: {err}."),
                next_step: "Check system thread limits and retry.".to_string(),
            })?;

        let manager = Self {
            worker_thread: Mutex::new(Some(handle)),
            volume_thread: Mutex::new(None),
            scheduler_thread: Mutex::new(None),
            inner,
            _lock: lock,
        };
        let volume_inner = Arc::clone(&manager.inner);
        let volume_watcher = thread::Builder::new()
            .name("foldersync-volumes".to_owned())
            .spawn(move || volume_watch_loop(&volume_inner))
            .map_err(|err| ManagerError::Io {
                cause: format!("Failed to start the disk watcher: {err}."),
                next_step: "Check system thread limits and retry.".to_owned(),
            })?;
        *manager.volume_thread.lock().unwrap() = Some(volume_watcher);
        let scheduling_inner = Arc::clone(&manager.inner);
        let scheduler = thread::Builder::new()
            .name("foldersync-scheduler".to_owned())
            .spawn(move || scheduling_loop(&scheduling_inner, watcher))
            .map_err(|err| ManagerError::Io {
                cause: format!("Failed to start automatic scheduling: {err}."),
                next_step: "Check system thread limits and retry.".to_owned(),
            })?;
        *manager.scheduler_thread.lock().unwrap() = Some(scheduler);
        Ok(manager)
    }

    pub fn state_dir(&self) -> &Path {
        &self.inner.state_dir
    }

    pub fn finder_status_dir(&self) -> &Path {
        self.inner.finder_writer.dir()
    }

    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn notify_worker_for_test(&self) {
        let _guard = self.inner.state.lock().unwrap();
        self.inner.worker_cv.notify_all();
    }

    /// Registers a local-fixture account backed by an isolated throwaway empty
    /// `rclone.conf` inside `<state_dir>/accounts/<id>/rclone.conf` (`0600`).
    pub fn register_local_fixture_account(
        &self,
        account_id: impl Into<AccountId>,
        display_name: impl Into<String>,
    ) -> Result<AccountRecord, ManagerError> {
        self.register_local_fixture_account_with_root(account_id, display_name, None)
    }

    /// Registers a local-fixture account and persists an optional parent directory
    /// path in `remote_root_folder_id` so the folder catalog survives restarts (HIGH #3).
    pub fn register_local_fixture_account_with_root(
        &self,
        account_id: impl Into<AccountId>,
        display_name: impl Into<String>,
        parent_dir: Option<&Path>,
    ) -> Result<AccountRecord, ManagerError> {
        let account_id = account_id.into();
        validate_safe_path_id("account_id", account_id.as_str())?;

        let remote_root_folder_id = match parent_dir {
            Some(dir) => {
                let meta = fs::symlink_metadata(dir).map_err(|err| ManagerError::Io {
                    cause: format!(
                        "Account parent folder `{}` does not exist or cannot be accessed: {err}.",
                        dir.display()
                    ),
                    next_step: "Choose an existing folder containing the collections to sync."
                        .to_string(),
                })?;
                if meta.file_type().is_symlink() || !meta.is_dir() {
                    return Err(ManagerError::PreflightRejected {
                        cause: format!(
                            "Account parent folder `{}` must be an ordinary directory.",
                            dir.display()
                        ),
                        next_step: "Choose an ordinary directory for this account.".to_string(),
                    });
                }
                fs::canonicalize(dir)
                    .map_err(|err| ManagerError::Io {
                        cause: format!(
                            "Could not resolve account parent folder `{}`: {err}.",
                            dir.display()
                        ),
                        next_step: "Confirm the folder exists and is readable.".to_string(),
                    })?
                    .to_string_lossy()
                    .into_owned()
            }
            None => "local-root".to_string(),
        };

        let raw_account_dir = self
            .inner
            .state_dir
            .join("accounts")
            .join(account_id.as_str());
        let account_dir =
            ensure_private_dir_under("account_dir", &self.inner.state_dir, &raw_account_dir)?;
        let config_path = account_dir.join("rclone.conf");
        reject_symlinks_under_root("account_config", &self.inner.state_dir, &config_path)?;
        if !config_path.exists() {
            write_private_file_under("account_config", &self.inner.state_dir, &config_path, b"")?;
        }
        let now = self.inner.clock.now_rfc3339();
        let record = AccountRecord {
            id: account_id,
            display_name: display_name.into(),
            kind: "local_fixture".to_string(),
            remote_name: "local".to_string(),
            remote_root_folder_id,
            config_path,
            auth_status: "ready".to_string(),
            created_at: now.clone(),
            updated_at: now,
        };
        let guard = self.inner.state.lock().unwrap();
        guard.store.upsert_account(&record)?;
        Ok(record)
    }

    /// Runs the exact same non-mutating preflight checks as initialization
    /// (volume detection, cross-mapping layout/recovery disjointness, cloud
    /// collection disjointness, endpoint inventory, and direction decision)
    /// so the setup wizard and actual preflight always agree (HIGH #2).
    pub fn preflight_mapping_preview(
        &self,
        cmd: &AddMappingCommand,
        editing_id: Option<&MappingId>,
    ) -> Result<PreflightPreview, ManagerError> {
        let local_path = cmd.local_binding.resolved_path();
        let detected_volume = detect_volume_with(self.inner.volume_probe.as_ref(), &local_path)
            .map_err(|e| e.cause());

        let (existing, device_id) = {
            let guard = self.inner.state.lock().unwrap();
            let list: Vec<MappingRecord> = guard
                .store
                .list_mappings()?
                .into_iter()
                .filter(|m| editing_id != Some(&m.id))
                .collect();
            (list, guard.device.id.clone())
        };

        let control = ProcessControl::with_timeout(DRIVE_COMMAND_TIMEOUT);
        let drive = self.mapping_drive(cmd, &control)?;
        let mut collection = cmd.collection.clone();
        let (remote_endpoint, cloud_recovery_path) = if let Some(drive) = &drive {
            collection = drive.verify_collection(&collection).map_err(drive_error)?;
            drive_mapping_paths(drive, &collection)
        } else {
            (cmd.remote_endpoint.clone(), cmd.cloud_recovery_path.clone())
        };
        let candidate = LayoutCandidate {
            mapping_id: &cmd.id,
            local_binding: &cmd.local_binding,
            remote_endpoint: remote_endpoint.clone(),
            local_recovery_root: cmd.local_recovery_root.clone(),
            cloud_recovery_path: &cloud_recovery_path,
        };
        let mut layout_error = validate_complete_layout(
            &existing,
            Some(&candidate),
            &self.inner.state_dir,
            self.inner.finder_writer.dir(),
        )
        .err()
        .map(|e| e.cause());

        if layout_error.is_none() {
            layout_error = validate_mapping_collections(
                &existing,
                &cmd.account_id,
                &collection,
                &device_id,
                drive.is_some(),
            )
            .err()
            .map(|err| err.cause());
        }

        let exclusions = effective_exclusions(&cmd.extra_exclusions)?;

        let local_inventory =
            inspect_local_endpoint_detailed(&local_path, &collection, &exclusions)
                .map_err(|e| e.cause());

        let remote_inventory = match &remote_endpoint {
            RemoteEndpoint::LocalDir(remote_root) => {
                inspect_local_endpoint_detailed(remote_root, &collection, &exclusions)
                    .map_err(|e| e.cause())
            }
            RemoteEndpoint::DriveRemote { .. } => inspect_drive_endpoint(
                drive.as_ref().expect("Drive account resolved"),
                &collection,
                &exclusions,
            )
            .map(|(inventory, _)| inventory)
            .map_err(|err| err.cause()),
            RemoteEndpoint::OtherRemote { .. } => {
                Err("This remote does not support first sync.".into())
            }
        };

        let direction = match (&local_inventory, &remote_inventory) {
            (Ok(l), Ok(r)) => {
                let l_inv = crate::init::EndpointInventory {
                    root: l.root.clone(),
                    classification: l.classification,
                    item_count: l.all_items_count,
                    has_valid_marker: l.has_valid_marker,
                };
                let r_inv = crate::init::EndpointInventory {
                    root: r.root.clone(),
                    classification: r.classification,
                    item_count: r.all_items_count,
                    has_valid_marker: r.has_valid_marker,
                };
                decide_init_direction(&cmd.id, &l_inv, &r_inv, None).map_err(|e| e.cause())
            }
            (Err(e), _) => Err(format!("Can't check local folder: {e}")),
            (_, Err(e)) => Err(format!("Can't check destination folder: {e}")),
        };

        Ok(PreflightPreview {
            detected_volume,
            layout_error,
            local_inventory,
            remote_inventory,
            direction,
        })
    }

    /// Previews a replacement filter set without mutating the mapping or its baseline.
    /// Supply all user exclusions; FolderSync always includes its built-in exclusions.
    pub fn propose_filter_change(
        &self,
        mapping_id: &MappingId,
        extra_exclusions: BTreeSet<String>,
    ) -> Result<FilterChangePreview, ManagerError> {
        let mapping = lookup_mapping(&self.inner.state.lock().unwrap().store, mapping_id)?;
        let previous_generation_id =
            mapping
                .active_generation_id
                .clone()
                .ok_or_else(|| ManagerError::InvalidState {
                    mapping_id: mapping_id.clone(),
                    cause: "Changes to an established baseline require verified first sync."
                        .to_owned(),
                    next_step: "Review first-sync setup before saving exclusions.".to_owned(),
                })?;
        let old: BTreeSet<_> = mapping.exclusions.iter().cloned().collect();
        let new = effective_exclusions(&extra_exclusions)?;
        let cmd = AddMappingCommand {
            id: mapping.id,
            account_id: mapping.account_id,
            collection: mapping.collection,
            local_binding: mapping.local_binding,
            remote_endpoint: RemoteEndpoint::from_db_cols(
                &mapping.remote_endpoint_kind,
                &mapping.remote_endpoint_value,
            )?,
            local_recovery_root: mapping.local_recovery_root,
            cloud_recovery_path: mapping.cloud_recovery_path,
            extra_exclusions,
            max_delete_percent: mapping.max_delete_percent,
            schedule_interval: Duration::from_secs(mapping.schedule_interval_secs),
        };
        let preflight = self.preflight_mapping_preview(&cmd, Some(mapping_id))?;
        Ok(FilterChangePreview {
            mapping_id: mapping_id.clone(),
            previous_generation_id,
            effective_exclusions: new.iter().cloned().collect(),
            added_exclusions: new.difference(&old).cloned().collect(),
            removed_exclusions: old.difference(&new).cloned().collect(),
            preflight,
        })
    }

    /// Stops normal runs for the old generation and queues a fresh first-sync
    /// preflight using the reviewed direction. Prior generations remain on disk.
    pub fn apply_filter_change(
        &self,
        preview: FilterChangePreview,
    ) -> Result<CommandAcceptance, ManagerError> {
        let mut guard = self.inner.state.lock().unwrap();
        let mut mapping = lookup_mapping(&guard.store, &preview.mapping_id)?;
        if guard.shutdown
            || guard.storage_blocked_mappings.contains(&mapping.id)
            || mapping.active_generation_id.as_ref() != Some(&preview.previous_generation_id)
            || mapping.state.readiness() != Readiness::Ready
            || !matches!(mapping.state.execution, Execution::Idle | Execution::Queued)
        {
            return Err(ManagerError::InvalidState {
                mapping_id: mapping.id,
                cause: "Folder selection changed or the mapping is busy or held.".to_owned(),
                next_step: "Wait for the active run to finish or resolve the hold, then review the selection again.".to_owned(),
            });
        }
        if let Some(blocking) =
            find_blocking_global_orphan(&guard.store, self.inner.process_inspector.as_ref())?
        {
            return Err(ManagerError::ActiveJobConflict {
                active_mapping_id: blocking.mapping_id.clone(),
                cause: format!(
                    "Cannot start first sync while `{}` has an unresolved hold ({}).",
                    blocking.mapping_id,
                    plain_recovery_reason_label(&blocking.reason_kind)
                ),
                next_step:
                    "Open recovery details to resolve the earlier sync process before starting another sync."
                        .to_string(),
            });
        }
        if preview.added_exclusions.is_empty() && preview.removed_exclusions.is_empty() {
            return Err(ManagerError::PreflightRejected {
                cause: "The exclusion set has not changed.".to_owned(),
                next_step: "Change the selection before starting a new setup.".to_owned(),
            });
        }
        if let Some(cause) = preview
            .preflight
            .layout_error
            .or_else(|| preview.preflight.detected_volume.err())
        {
            return Err(ManagerError::PreflightRejected {
                cause,
                next_step: "Correct the folder setup and review the new selection again."
                    .to_owned(),
            });
        }
        let direction = preview.preflight.direction.map_err(|cause| ManagerError::PreflightRejected {
            cause,
            next_step: "Review both folders and preserve the existing copies before initializing again.".to_owned(),
        })?;

        mapping.state = MappingState::new(
            Readiness::NeedsInitialization,
            Execution::Queued,
            mapping.state.intent,
        );
        mapping.scheduling_enabled = false;
        mapping.active_generation_id = None;
        mapping.exclusions = preview.effective_exclusions;
        mapping.updated_at = self.inner.clock.now_rfc3339();
        guard.store.update_mapping_runtime(&mapping)?;
        guard.schedules.remove(&mapping.id);
        guard.job_queue.retain(|job| job.mapping_id != mapping.id);
        guard.next_init_attempt_id += 1;
        let attempt_id = guard.next_init_attempt_id;
        guard.job_queue.push_front(QueuedJob {
            automatic: false,
            mapping_id: mapping.id.clone(),
            kind: QueuedJobKind::InitializePreflight {
                expected_direction: Some(direction),
                attempt_id,
            },
        });
        self.inner.worker_cv.notify_all();
        let snapshot = publish_updated_snapshot(&self.inner, &mut guard)?;
        Ok(CommandAcceptance {
            outcome: CommandOutcome::InitializationPreflightQueued {
                mapping_id: mapping.id,
            },
            snapshot,
        })
    }

    pub fn execute(&self, cmd: ManagerCommand) -> Result<CommandAcceptance, ManagerError> {
        match cmd {
            ManagerCommand::AddMapping(add) => self.handle_add_mapping(*add, false),
            ManagerCommand::SaveDraftMapping(add) => self.handle_add_mapping(*add, true),
            ManagerCommand::Initialize(init) => self.handle_initialize(init),
            ManagerCommand::StartInitialize(init) => self.handle_start_initialize(init),
            ManagerCommand::RunNow { mapping_id } => self.handle_run_now(mapping_id),
            ManagerCommand::SyncReadyMappings => self.handle_sync_ready_mappings(),
            ManagerCommand::Pause { mapping_id } => self.handle_pause(mapping_id),
            ManagerCommand::Resume { mapping_id } => self.handle_resume(mapping_id),
            ManagerCommand::SetSchedulingPaused { paused } => {
                self.handle_set_scheduling_paused(paused)
            }
            ManagerCommand::StopRun { mapping_id } => self.handle_stop_run(mapping_id),
            ManagerCommand::ForceStop { mapping_id } => self.handle_force_stop(mapping_id),
            ManagerCommand::InspectRecovery { mapping_id } => {
                self.handle_inspect_recovery(mapping_id)
            }
            ManagerCommand::AcknowledgeConflicts { mapping_id } => {
                self.handle_acknowledge_conflicts(mapping_id)
            }
            ManagerCommand::CheckAgain { mapping_id } => self.handle_check_again(mapping_id),
            ManagerCommand::CompleteRecovery {
                mapping_id,
                resume_and_run,
            } => self.handle_complete_recovery(mapping_id, resume_and_run),
            ManagerCommand::RemoveMapping { mapping_id } => self.handle_remove_mapping(mapping_id),
            ManagerCommand::DiscardDraft { mapping_id } => self.handle_remove_mapping(mapping_id),
            ManagerCommand::SetScheduleInterval {
                mapping_id,
                interval_secs,
            } => self.handle_set_schedule_interval(mapping_id, interval_secs),
            ManagerCommand::RemoveAccount { account_id } => self.handle_remove_account(account_id),
        }
    }

    pub fn add_mapping(&self, cmd: AddMappingCommand) -> Result<CommandAcceptance, ManagerError> {
        self.execute(ManagerCommand::AddMapping(Box::new(cmd)))
    }

    pub fn save_draft_mapping(
        &self,
        cmd: AddMappingCommand,
    ) -> Result<CommandAcceptance, ManagerError> {
        self.execute(ManagerCommand::SaveDraftMapping(Box::new(cmd)))
    }

    pub fn initialize(&self, cmd: InitializeCommand) -> Result<CommandAcceptance, ManagerError> {
        self.execute(ManagerCommand::Initialize(cmd))
    }

    pub fn start_initialize(
        &self,
        cmd: InitializeCommand,
    ) -> Result<CommandAcceptance, ManagerError> {
        self.execute(ManagerCommand::StartInitialize(cmd))
    }

    pub fn run_now(
        &self,
        mapping_id: impl Into<MappingId>,
    ) -> Result<CommandAcceptance, ManagerError> {
        self.execute(ManagerCommand::RunNow {
            mapping_id: mapping_id.into(),
        })
    }

    pub fn sync_ready_mappings(&self) -> Result<CommandAcceptance, ManagerError> {
        self.execute(ManagerCommand::SyncReadyMappings)
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

    pub fn set_scheduling_paused(&self, paused: bool) -> Result<CommandAcceptance, ManagerError> {
        self.execute(ManagerCommand::SetSchedulingPaused { paused })
    }

    /// Marks eligible mappings for a fresh check after wake or network return.
    pub fn notify_environment_changed(&self) {
        let mut guard = self.inner.state.lock().unwrap();
        mark_environment_changed(&self.inner, &mut guard);
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

    pub fn acknowledge_conflicts(
        &self,
        mapping_id: impl Into<MappingId>,
    ) -> Result<CommandAcceptance, ManagerError> {
        self.execute(ManagerCommand::AcknowledgeConflicts {
            mapping_id: mapping_id.into(),
        })
    }

    pub fn check_again(
        &self,
        mapping_id: impl Into<MappingId>,
    ) -> Result<CommandAcceptance, ManagerError> {
        self.execute(ManagerCommand::CheckAgain {
            mapping_id: mapping_id.into(),
        })
    }

    pub fn complete_recovery(
        &self,
        mapping_id: impl Into<MappingId>,
        resume_and_run: bool,
    ) -> Result<CommandAcceptance, ManagerError> {
        self.execute(ManagerCommand::CompleteRecovery {
            mapping_id: mapping_id.into(),
            resume_and_run,
        })
    }

    pub fn remove_mapping(
        &self,
        mapping_id: impl Into<MappingId>,
    ) -> Result<CommandAcceptance, ManagerError> {
        self.execute(ManagerCommand::RemoveMapping {
            mapping_id: mapping_id.into(),
        })
    }

    pub fn set_schedule_interval(
        &self,
        mapping_id: impl Into<MappingId>,
        interval_secs: u64,
    ) -> Result<CommandAcceptance, ManagerError> {
        self.execute(ManagerCommand::SetScheduleInterval {
            mapping_id: mapping_id.into(),
            interval_secs,
        })
    }

    pub fn remove_account(
        &self,
        account_id: impl Into<AccountId>,
    ) -> Result<CommandAcceptance, ManagerError> {
        self.execute(ManagerCommand::RemoveAccount {
            account_id: account_id.into(),
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

    /// Subscribes to status snapshots with coalescing latest-value delivery
    /// (the newest snapshot is never dropped when a consumer falls behind).
    pub fn subscribe_snapshots(&self, bound: usize) -> SnapshotReceiver {
        self.inner.event_bus.subscribe_snapshots(bound)
    }

    /// Lists immediate children of this ready account's shared Data folder.
    pub fn list_collection_candidates(
        &self,
        account: &AccountId,
    ) -> Result<CollectionListing, ManagerError> {
        self.inner
            .accounts
            .list_collection_candidates(account)
            .map_err(drive_error)
    }

    /// Creates and resolves one immediate child of Data on this account.
    pub fn create_collection_folder(
        &self,
        account: &AccountId,
        name: &str,
    ) -> Result<CollectionCandidate, ManagerError> {
        self.inner
            .accounts
            .create_collection_folder(account, name)
            .map_err(drive_error)
    }

    /// Returns a recent cached result or starts the only active size calculation.
    pub fn request_folder_size(
        &self,
        account: &AccountId,
        folder_id: &str,
    ) -> Result<OperationHandle<FolderSizeSnapshot>, ManagerError> {
        validate_safe_path_id("account_id", account.as_str())?;
        validate_drive_folder_id(folder_id).map_err(drive_error)?;
        let control = ProcessControl::with_timeout(DRIVE_COMMAND_TIMEOUT);
        let (sender, result) = mpsc::channel();
        let mut guard = self.inner.state.lock().unwrap();
        let key = (account.clone(), folder_id.to_owned());
        let now = self.inner.clock.now_utc();
        if !guard.shutdown
            && guard
                .store
                .get_account(account)?
                .is_some_and(|account| account.auth_status == "ready")
            && let Some(cached) = guard.folder_sizes.get(&key)
            && matches!(cached.status, FolderSizeStatus::Known { .. })
            && now >= cached.updated_at
            && now - cached.updated_at < FOLDER_SIZE_CACHE_TTL
        {
            let _ = sender.send(Ok(cached.clone()));
            return Ok(OperationHandle { control, result });
        }
        if guard.shutdown || guard.sizing.is_some() {
            return Err(ManagerError::PreflightRejected {
                cause: "A folder size calculation is unavailable while another one is active or FolderSync is quitting.".into(),
                next_step: "Wait for the current operation to finish and try again.".into(),
            });
        }
        let mut status = FolderSizeSnapshot {
            account_id: account.clone(),
            folder_id: folder_id.to_owned(),
            updated_at: now,
            status: FolderSizeStatus::Calculating,
        };
        let old = guard.folder_sizes.insert(key.clone(), status.clone());
        if let Err(error) = publish_snapshot(&self.inner, &mut guard, false) {
            restore_size_status(&mut guard, &key, old);
            return Err(error);
        }
        guard.sizing = Some(control.clone());
        let worker_key = key.clone();
        let worker_control = control.clone();
        let inner = Arc::clone(&self.inner);
        let lock = Arc::clone(&self._lock);
        let spawned = thread::Builder::new()
            .name("foldersync-size".into())
            .spawn(move || {
                let _lock = lock;
                let measured = inner
                    .accounts
                    .measure_collection(&status.account_id, &status.folder_id, &worker_control)
                    .map_err(drive_error)
                    .and_then(|size| {
                        if size.sizeless != 0 {
                            return Err(ManagerError::PreflightRejected {
                                cause: "Some files did not report a size.".into(),
                                next_step: "Review the folder's contents and try again.".into(),
                            });
                        }
                        Ok(FolderSizeStatus::Known {
                            bytes: size.bytes,
                            files: size.count,
                        })
                    });
                let mut guard = inner.state.lock().unwrap();
                status.updated_at = inner.clock.now_utc();
                status.status = match &measured {
                    Ok(status) => status.clone(),
                    Err(error) => FolderSizeStatus::Failed {
                        cause: error.cause(),
                    },
                };
                guard
                    .folder_sizes
                    .insert(worker_key.clone(), status.clone());
                if guard.folder_sizes.len() > FOLDER_SIZE_CACHE_CAPACITY
                    && let Some(oldest) = guard
                        .folder_sizes
                        .iter()
                        .filter(|(key, _)| *key != &worker_key)
                        .min_by_key(|(_, entry)| entry.updated_at)
                        .map(|(key, _)| key.clone())
                {
                    guard.folder_sizes.remove(&oldest);
                }
                guard.sizing = None;
                let published = publish_snapshot(&inner, &mut guard, false);
                inner.idle_cv.notify_all();
                let _ = sender.send(published.and(measured).map(|_| status));
            });
        if let Err(error) = spawned {
            guard.sizing = None;
            restore_size_status(&mut guard, &key, old);
            publish_snapshot(&self.inner, &mut guard, false)?;
            return Err(account_worker_error(error));
        }
        Ok(OperationHandle { control, result })
    }

    /// Starts connecting or reconnecting a Drive account. The returned handle
    /// can cancel the process independently of any browser window.
    pub fn start_drive_authorization(
        &self,
        operation: DriveAuthorization,
        on_auth_url: Option<AuthUrlCallback>,
    ) -> Result<OperationHandle<DriveAccountResult>, ManagerError> {
        let id = match &operation {
            DriveAuthorization::Connect(request) => request.id.clone(),
            DriveAuthorization::Reconnect(id) => id.clone(),
        };
        validate_safe_path_id("account_id", id.as_str())?;
        let control = ProcessControl::default();
        let worker_control = control.clone();
        let (sender, result) = mpsc::channel();
        let mut guard = self.inner.state.lock().unwrap();
        if guard.shutdown || guard.authorization.is_some() {
            return Err(ManagerError::PreflightRejected {
                cause: "Drive authorization is unavailable while another sign-in is active or FolderSync is quitting.".into(),
                next_step: "Wait for the current operation or cancel sign-in before trying again.".into(),
            });
        }
        guard.authorization = Some((id, control.clone()));
        let inner = Arc::clone(&self.inner);
        let lock = Arc::clone(&self._lock);
        let spawned = thread::Builder::new()
            .name("foldersync-authorization".into())
            .spawn(move || {
                let _lock = lock;
                let outcome = match operation {
                    DriveAuthorization::Connect(mut request) => {
                        request.run_oauth_flow = true;
                        inner.accounts.add_drive_account_using(
                            request,
                            on_auth_url,
                            Some(&worker_control),
                        )
                    }
                    DriveAuthorization::Reconnect(id) => inner
                        .accounts
                        .reconnect_drive_account_using(&id, on_auth_url, Some(&worker_control)),
                };
                let mut guard = inner.state.lock().unwrap();
                let result = outcome.map_err(drive_error).and_then(|outcome| {
                    if outcome.account.auth_status == "ready" {
                        let now = inner.clock.now_rfc3339();
                        let mut updates = guard.store.list_mappings()?;
                        updates.retain_mut(|mapping| {
                            if mapping.account_id != outcome.account.id {
                                return false;
                            }
                            let restored =
                                apply_event(mapping.state, ObservedEvent::AuthenticationRestored)
                                    .state;
                            if restored == mapping.state {
                                return false;
                            }
                            mapping.state = restored;
                            mapping.updated_at.clone_from(&now);
                            true
                        });
                        if !updates.is_empty() {
                            guard.store.update_mapping_runtimes(&updates)?;
                        }
                        publish_updated_snapshot(&inner, &mut guard)?;
                    }
                    Ok(outcome)
                });
                guard.authorization = None;
                inner.idle_cv.notify_all();
                let _ = sender.send(result);
            });
        if let Err(error) = spawned {
            guard.authorization = None;
            return Err(account_worker_error(error));
        }
        Ok(OperationHandle { control, result })
    }

    pub fn list_accounts(&self) -> Result<Vec<AccountRecord>, ManagerError> {
        self.inner.state.lock().unwrap().store.list_accounts()
    }

    pub fn list_mappings_records(&self) -> Result<Vec<MappingRecord>, ManagerError> {
        self.inner.state.lock().unwrap().store.list_mappings()
    }

    pub fn get_generation(
        &self,
        generation_id: &GenerationId,
    ) -> Result<Option<GenerationRecord>, ManagerError> {
        self.inner
            .state
            .lock()
            .unwrap()
            .store
            .get_generation(generation_id)
    }

    pub fn list_runs(&self, mapping_id: &MappingId) -> Result<Vec<RunRecord>, ManagerError> {
        self.inner
            .state
            .lock()
            .unwrap()
            .store
            .list_runs_for_mapping(mapping_id)
    }

    pub fn list_recent_runs(
        &self,
        mapping_id: &MappingId,
        limit: usize,
    ) -> Result<Vec<RunRecord>, ManagerError> {
        self.inner
            .state
            .lock()
            .unwrap()
            .store
            .list_recent_runs_for_mapping(mapping_id, limit)
    }

    pub fn get_run(&self, run_id: &RunId) -> Result<Option<RunRecord>, ManagerError> {
        self.inner.state.lock().unwrap().store.get_run(run_id)
    }

    /// Reads up to `max_lines` lines from a specific run's `output.ndjson` file
    /// strictly inside `<state_dir>/runs/` (refusing symlinks).
    pub fn read_run_log(
        &self,
        run_id: &RunId,
        max_lines: usize,
    ) -> Result<Vec<String>, ManagerError> {
        let run = self
            .get_run(run_id)?
            .ok_or_else(|| ManagerError::NotFound {
                cause: format!("Run `{run_id}` was not found."),
                next_step: "Select a run from recent history.".to_string(),
            })?;
        let checked = reject_symlinks_under_root("log_path", &self.inner.state_dir, &run.log_path)?;
        if !checked.exists() {
            return Ok(Vec::new());
        }
        let content = fs::read_to_string(&checked).map_err(|err| ManagerError::Io {
            cause: format!("Could not read run log `{}`: {err}.", checked.display()),
            next_step: "Check permissions inside FolderSync's state directory.".to_string(),
        })?;
        let lines: Vec<String> = content.lines().map(ToOwned::to_owned).collect();
        let cap = max_lines.max(1);
        if lines.len() > cap {
            Ok(lines[lines.len() - cap..].to_vec())
        } else {
            Ok(lines)
        }
    }

    /// Lists preserved recovery copies for a specific run (local `backup_dir1`
    /// and local-fixture `backup_dir2`) without mutating anything (§12).
    /// Surfaces directory read / symlink errors rather than returning an empty list (HIGH #9).
    pub fn list_recovery_copies(
        &self,
        run_id: &RunId,
    ) -> Result<Vec<RecoveryCopyEntry>, ManagerError> {
        let run = self
            .get_run(run_id)?
            .ok_or_else(|| ManagerError::NotFound {
                cause: format!("Run `{run_id}` was not found."),
                next_step: "Select a run from recent history.".to_string(),
            })?;

        let mut entries = Vec::new();
        collect_recovery_files_readonly(&run.backup_dir1, RecoveryCopySide::Local, &mut entries)?;
        let remote_path = PathBuf::from(&run.backup_dir2);
        if remote_path.is_absolute() {
            collect_recovery_files_readonly(&remote_path, RecoveryCopySide::Remote, &mut entries)?;
        }
        entries.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
        Ok(entries)
    }

    /// Copies one preserved recovery copy into a separate user-chosen inspection
    /// directory (plan §12 steps 1–4). Refuses symlinks BEFORE canonicalizing,
    /// refuses any destination overlapping a synced mapping root (never restores
    /// over live data), and creates the output file exclusively (`create_new`).
    pub fn copy_recovery_file_to_inspection(
        &self,
        run_id: &RunId,
        side: RecoveryCopySide,
        relative_path: &str,
        inspection_dir: &Path,
    ) -> Result<PathBuf, ManagerError> {
        let rel = Path::new(relative_path);
        if relative_path.is_empty()
            || rel.is_absolute()
            || rel.components().any(|c| {
                matches!(
                    c,
                    std::path::Component::ParentDir
                        | std::path::Component::RootDir
                        | std::path::Component::Prefix(_)
                )
            })
        {
            return Err(ManagerError::PreflightRejected {
                cause: format!("Invalid recovery file path `{relative_path}`."),
                next_step: "Select a recovery copy listed for this run.".to_string(),
            });
        }

        // Check inspection_dir type via lstat BEFORE canonicalizing (HIGH #10).
        let raw_dest_meta =
            fs::symlink_metadata(inspection_dir).map_err(|err| ManagerError::Io {
                cause: format!(
                    "Inspection folder `{}` does not exist or cannot be accessed: {err}.",
                    inspection_dir.display()
                ),
                next_step: "Choose an existing folder outside every synced folder.".to_string(),
            })?;
        if raw_dest_meta.file_type().is_symlink() || !raw_dest_meta.is_dir() {
            return Err(ManagerError::PreflightRejected {
                cause: format!(
                    "Inspection path `{}` is a symbolic link or not an ordinary folder.",
                    inspection_dir.display()
                ),
                next_step: "Choose an ordinary folder outside every synced folder.".to_string(),
            });
        }
        let dest_dir = resolve_absolute_path("inspection_dir", inspection_dir)?;

        let (run, mappings) = {
            let guard = self.inner.state.lock().unwrap();
            let run = guard
                .store
                .get_run(run_id)?
                .ok_or_else(|| ManagerError::NotFound {
                    cause: format!("Run `{run_id}` was not found."),
                    next_step: "Select a run from recent history.".to_string(),
                })?;
            let mappings = guard.store.list_mappings()?;
            (run, mappings)
        };

        // Never restore directly over live synced data (§12)!
        for m in &mappings {
            if let Ok(local_root) =
                resolve_absolute_path("local_root", &m.local_binding.resolved_path())
                && paths_overlap(&dest_dir, &local_root)
            {
                return Err(ManagerError::PreflightRejected {
                    cause: format!(
                        "Inspection folder `{}` overlaps synced folder `{}` (`{}`). FolderSync never restores directly over live data.",
                        dest_dir.display(),
                        m.collection.display_name(),
                        local_root.display()
                    ),
                    next_step: "Choose a separate inspection folder outside all synced folders, verify the file, then copy it yourself."
                        .to_string(),
                });
            }
            if let Ok(RemoteEndpoint::LocalDir(remote_root)) =
                RemoteEndpoint::from_db_cols(&m.remote_endpoint_kind, &m.remote_endpoint_value)
                && let Ok(res_remote) = resolve_absolute_path("remote_root", &remote_root)
                && paths_overlap(&dest_dir, &res_remote)
            {
                return Err(ManagerError::PreflightRejected {
                    cause: format!(
                        "Inspection folder `{}` overlaps destination folder `{}`.",
                        dest_dir.display(),
                        res_remote.display()
                    ),
                    next_step: "Choose a separate inspection folder outside all synced folders."
                        .to_string(),
                });
            }
        }

        let owning_mapping = mappings.iter().find(|m| m.id == run.mapping_id);
        let backup_root = match side {
            RecoveryCopySide::Local => run.backup_dir1.clone(),
            RecoveryCopySide::Remote => PathBuf::from(&run.backup_dir2),
        };
        // Refuse symlinked mapping recovery root and any symlinked component down to backup_root
        // BEFORE canonicalizing (HIGH #10).
        if let Some(m) = owning_mapping {
            let rec_root = match side {
                RecoveryCopySide::Local => Some(m.local_recovery_root.clone()),
                RecoveryCopySide::Remote => {
                    let p = PathBuf::from(&m.cloud_recovery_path);
                    p.is_absolute().then_some(p)
                }
            };
            if let Some(root) = rec_root {
                reject_symlinks_under_root("recovery_root", &root, &backup_root)?;
            }
        }
        let backup_meta = fs::symlink_metadata(&backup_root).map_err(|err| ManagerError::Io {
            cause: format!(
                "Run recovery folder `{}` cannot be inspected: {err}.",
                backup_root.display()
            ),
            next_step: "Refresh the recovery list and check that the recovery drive is connected."
                .to_string(),
        })?;
        if backup_meta.file_type().is_symlink() || !backup_meta.is_dir() {
            return Err(ManagerError::PreflightRejected {
                cause: format!(
                    "Run recovery folder `{}` is a symbolic link or not an ordinary directory.",
                    backup_root.display()
                ),
                next_step: "Inspect the recovery directory before copying files.".to_string(),
            });
        }
        let source_path =
            reject_symlinks_under_root("recovery_copy", &backup_root, &backup_root.join(rel))?;
        let src_meta = fs::symlink_metadata(&source_path).map_err(|err| ManagerError::Io {
            cause: format!("Recovery copy `{relative_path}` could not be inspected: {err}."),
            next_step: "Refresh the recovery list and check that the recovery drive is connected."
                .to_string(),
        })?;
        if src_meta.file_type().is_symlink() || !src_meta.is_file() {
            return Err(ManagerError::PreflightRejected {
                cause: format!("Recovery copy `{relative_path}` is not an ordinary file."),
                next_step: "Choose an ordinary file from the recovery list.".to_string(),
            });
        }

        let base_name = source_path
            .file_name()
            .ok_or_else(|| ManagerError::PreflightRejected {
                cause: format!("Recovery copy `{relative_path}` has no filename."),
                next_step: "Choose another recovery copy.".to_string(),
            })?;
        let dest_path = dest_dir.join(base_name);
        let bytes = fs::read(&source_path).map_err(|err| ManagerError::Io {
            cause: format!("Failed to read recovery copy `{relative_path}`: {err}."),
            next_step: "Check permissions on the recovery folder.".to_string(),
        })?;
        let mut out_file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&dest_path)
            .map_err(|err| {
                if err.kind() == std::io::ErrorKind::AlreadyExists {
                    ManagerError::PreflightRejected {
                        cause: format!(
                            "A file named `{}` already exists in `{}`.",
                            base_name.to_string_lossy(),
                            dest_dir.display()
                        ),
                        next_step: "Choose an empty inspection folder so nothing is overwritten."
                            .to_string(),
                    }
                } else {
                    ManagerError::Io {
                        cause: format!(
                            "Failed to create inspection copy `{}`: {err}.",
                            dest_path.display()
                        ),
                        next_step:
                            "Check write permissions and free space in the inspection folder."
                                .to_string(),
                    }
                }
            })?;
        out_file.write_all(&bytes).map_err(|err| ManagerError::Io {
            cause: format!(
                "Failed writing inspection copy `{}`: {err}.",
                dest_path.display()
            ),
            next_step: "Check free space in the inspection folder.".to_string(),
        })?;
        out_file.sync_all().map_err(|err| ManagerError::Io {
            cause: format!(
                "Failed flushing inspection copy `{}`: {err}.",
                dest_path.display()
            ),
            next_step: "Check free space and drive health in the inspection folder.".to_string(),
        })?;
        Ok(dest_path)
    }

    /// Atomically claims pending ADR 0002 v1 Finder action files by renaming each
    /// to a hidden `.processing-*` name, dispatches manager/UI commands, and deletes
    /// each claimed file AFTER dispatch (HIGH #11).
    pub fn drain_finder_actions(&self) -> Vec<FinderActionDispatch> {
        let actions_dir = self.inner.finder_writer.actions_dir_path();
        let claimed = claim_finder_action_files(&actions_dir);
        let mut results = Vec::with_capacity(claimed.len());

        for item in claimed {
            let dispatch = match item.outcome {
                FinderActionFileOutcome::Rejected { file_name, reason } => {
                    FinderActionDispatch::Rejected { file_name, reason }
                }
                FinderActionFileOutcome::Accepted { file_name, action } => match &action {
                    ValidatedFinderAction::SyncNow { mapping_id, .. } => {
                        match self.run_now(mapping_id.clone()) {
                            Ok(_) => FinderActionDispatch::Executed { file_name, action },
                            Err(err) => FinderActionDispatch::CommandFailed {
                                file_name,
                                action,
                                cause: err.cause(),
                            },
                        }
                    }
                    ValidatedFinderAction::Pause { mapping_id, .. } => {
                        match self.pause(mapping_id.clone()) {
                            Ok(_) => FinderActionDispatch::Executed { file_name, action },
                            Err(err) => FinderActionDispatch::CommandFailed {
                                file_name,
                                action,
                                cause: err.cause(),
                            },
                        }
                    }
                    ValidatedFinderAction::OpenApp { .. }
                    | ValidatedFinderAction::ShowLastRun { .. } => {
                        FinderActionDispatch::UiRequested { file_name, action }
                    }
                },
            };
            if let Some(claimed_path) = item.claimed_path {
                let _ = fs::remove_file(claimed_path);
            }
            results.push(dispatch);
        }
        results
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
                    cause: "Timed out waiting for the active synchronization job to finish."
                        .to_string(),
                    next_step: "Check Activity or stop the active run.".to_string(),
                });
            }
            let (next_guard, wait_res) = self.inner.idle_cv.wait_timeout(guard, remaining).unwrap();
            guard = next_guard;
            if wait_res.timed_out()
                && (guard.worker_busy || guard.active_run.is_some() || !guard.job_queue.is_empty())
            {
                return Err(ManagerError::Io {
                    cause: "Timed out waiting for the active synchronization job to finish."
                        .to_string(),
                    next_step: "Check Activity or stop the active run.".to_string(),
                });
            }
        }
        if let Some((cause, next_step)) = guard.last_terminal_error.take() {
            return Err(ManagerError::Storage { cause, next_step });
        }
        Ok(Arc::clone(&guard.latest_snapshot))
    }

    /// Controlled-stop shutdown handler shared by Quit, ⌘Q, and tray Quit (§14, spec §5.9):
    /// stops scheduling, clears queued jobs, requests graceful stop on any active child
    /// (updating snapshot to honest "Stopping…"), waits up to `timeout`, and on timeout
    /// persists an `Interrupted` / `RecoveryRequired` outcome + `outcome.json` before returning.
    pub fn controlled_shutdown(
        &self,
        timeout: Duration,
    ) -> Result<ControlledShutdownOutcome, ManagerError> {
        let stopper = {
            let mut guard = self.inner.state.lock().unwrap();
            guard.shutdown = true;
            cancel_auxiliary_operations(&guard);
            guard.scheduling_paused = true;

            // Cancel queued mappings back to Idle in SQLite without altering user intent.
            let queued_ids: Vec<MappingId> =
                guard.job_queue.drain(..).map(|j| j.mapping_id).collect();
            let now = self.inner.clock.now_rfc3339();
            for mid in queued_ids {
                if let Ok(Some(mut m)) = guard.store.get_mapping(&mid)
                    && m.state.execution == Execution::Queued
                {
                    m.state.execution = Execution::Idle;
                    m.updated_at = now.clone();
                    let _ = guard.store.update_mapping_runtime(&m);
                }
            }

            let stopper = if let Some(active) = guard.active_run.as_mut()
                && !active.stop_requested
            {
                active.stop_requested = true;
                let active_mid = active.mapping_id.clone();
                let st = active.stopper.as_ref().map(Arc::clone);
                if let Ok(Some(mut m)) = guard.store.get_mapping(&active_mid)
                    && m.state.execution == Execution::Running
                {
                    m.state.execution = Execution::Stopping;
                    m.updated_at = now;
                    let _ = guard.store.update_mapping_runtime(&m);
                }
                st
            } else {
                None
            };

            let _ = publish_updated_snapshot(&self.inner, &mut guard);
            self.inner.worker_cv.notify_all();
            stopper
        };

        if let Some(stopper) = stopper {
            let _ = stopper.request_stop();
        }

        // Bounded wait for the active run and worker to finish cleanly.
        let deadline = Instant::now() + timeout;
        let timed_out_info = {
            let mut guard = self.inner.state.lock().unwrap();
            while guard.worker_busy || guard.active_run.is_some() {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let (next_guard, wait_res) =
                    self.inner.idle_cv.wait_timeout(guard, remaining).unwrap();
                guard = next_guard;
                if wait_res.timed_out() {
                    break;
                }
            }

            if let Some(active) = guard.active_run.as_mut() {
                let mid = active.mapping_id.clone();
                let rid = active.run_id.clone();
                active.timed_out_during_shutdown = true;
                self.inner.timed_out_shutdown.store(true, Ordering::SeqCst);

                // Persist an interrupted outcome + recovery case BEFORE returning; surface DB errors (HIGH #6).
                let now = self.inner.clock.now_rfc3339();
                let mut run = guard
                    .store
                    .get_run(&rid)?
                    .ok_or_else(|| ManagerError::Storage {
                        cause: format!(
                            "Could not load active run `{rid}` to persist shutdown timeout."
                        ),
                        next_step: "Inspect state.sqlite before restarting FolderSync.".to_string(),
                    })?;
                run.status = "interrupted".to_string();
                run.finished_at = Some(now.clone());
                run.outcome_kind = Some("interrupted".to_string());
                run.outcome_detail = Some(
                    "Quit timed out while waiting for the active run to stop gracefully."
                        .to_string(),
                );
                run.last_verified_phase = "shutdown_timeout".to_string();

                let updated_mapping = guard.store.get_mapping(&mid)?.map(|mut m| {
                    m.state =
                        apply_event(m.state, ObservedEvent::RunFinished(RunOutcome::Interrupted))
                            .state;
                    m.last_run_id = Some(rid.clone());
                    m.last_run_outcome = Some(RunOutcome::Interrupted);
                    m.last_run_completed_at = Some(now.clone());
                    m.last_run_summary_text =
                        Some("Interrupted during quit — recovery required".to_string());
                    m.updated_at = now.clone();
                    m
                });

                let case = RecoveryCaseRecord {
                    id: format!("rc-{}", Uuid::new_v4()),
                    mapping_id: mid.clone(),
                    run_id: Some(rid.clone()),
                    status: "open".to_string(),
                    reason_kind: "shutdown_timeout".to_string(),
                    cause: "FolderSync quit before the active sync finished stopping.".to_string(),
                    next_step:
                        "Open recovery details to confirm no earlier sync process remains before resuming."
                            .to_string(),
                    workdir_path: Some(run.workdir_path.clone()),
                    backup_dir1: Some(run.backup_dir1.clone()),
                    backup_dir2: Some(run.backup_dir2.clone()),
                    log_path: Some(run.log_path.clone()),
                    orphan_pid: run.child_pid,
                    created_at: now.clone(),
                    resolved_at: None,
                    resolution_note: None,
                };
                write_run_outcome_json(&self.inner.state_dir, &run);
                guard
                    .store
                    .commit_run_terminal(&run, updated_mapping.as_ref(), Some(&case))?;
                let _ = publish_updated_snapshot(&self.inner, &mut guard);
                self.inner
                    .event_bus
                    .publish_event(ManagerEvent::ShutdownTimedOut {
                        mapping_id: mid.clone(),
                        run_id: rid.clone(),
                        timestamp: now,
                    });
                Some((mid, rid))
            } else {
                None
            }
        };

        self.wait_for_canceled_operations(timeout.min(AUXILIARY_STOP_TIMEOUT));
        self.join_watchers();
        if let Some((mapping_id, run_id)) = timed_out_info {
            return Ok(ControlledShutdownOutcome::TimedOutInterrupted { mapping_id, run_id });
        }

        if let Some(handle) = self.worker_thread.lock().unwrap().take() {
            let _ = handle.join();
        }
        Ok(ControlledShutdownOutcome::Clean)
    }

    /// Cleanly stops scheduling, requests graceful stop on any active child,
    /// joins the worker thread, and releases the exclusive state lock.
    pub fn shutdown(self) -> Result<(), ManagerError> {
        self.shutdown_internal();
        Ok(())
    }

    fn wait_for_canceled_operations(&self, timeout: Duration) {
        let guard = self.inner.state.lock().unwrap();
        drop(
            self.inner
                .idle_cv
                .wait_timeout_while(guard, timeout, |state| {
                    state.sizing.is_some() || state.authorization.is_some()
                })
                .unwrap(),
        );
    }

    fn join_watchers(&self) {
        if let Some(handle) = self.scheduler_thread.lock().unwrap().take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.volume_thread.lock().unwrap().take() {
            let _ = handle.join();
        }
    }

    fn shutdown_internal(&self) {
        let stopper = {
            let mut guard = self.inner.state.lock().unwrap();
            guard.shutdown = true;
            cancel_auxiliary_operations(&guard);
            guard.job_queue.clear();
            let stopper = if let Some(active) = guard.active_run.as_mut()
                && !active.stop_requested
            {
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
        if !self.inner.timed_out_shutdown.load(Ordering::SeqCst) {
            self.wait_for_canceled_operations(AUXILIARY_STOP_TIMEOUT);
        }
        self.join_watchers();
        // If controlled_shutdown already timed out on a hung child, do NOT block indefinitely on join (HIGH #6).
        if !self.inner.timed_out_shutdown.load(Ordering::SeqCst)
            && let Some(handle) = self.worker_thread.lock().unwrap().take()
        {
            let _ = handle.join();
        }
    }

    fn mapping_drive<'a>(
        &'a self,
        command: &AddMappingCommand,
        control: &'a ProcessControl,
    ) -> Result<Option<LockedDrive<'a>>, ManagerError> {
        if matches!(command.remote_endpoint, RemoteEndpoint::DriveRemote { .. }) {
            return self
                .inner
                .accounts
                .locked_drive(&command.account_id, control)
                .map(Some)
                .map_err(drive_error);
        }
        Ok(None)
    }

    fn handle_add_mapping(
        &self,
        mut cmd: AddMappingCommand,
        is_draft: bool,
    ) -> Result<CommandAcceptance, ManagerError> {
        validate_safe_path_id("mapping_id", cmd.id.as_str())?;
        validate_safe_path_id("account_id", cmd.account_id.as_str())?;

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
        let local_recovery_root =
            resolve_absolute_path("local_recovery_root", &cmd.local_recovery_root)?;
        // Account lock precedes the state mutex and remains held through the mapping commit.
        let control = ProcessControl::with_timeout(DRIVE_COMMAND_TIMEOUT);
        let drive = self.mapping_drive(&cmd, &control)?;
        if let Some(drive) = &drive {
            cmd.collection = drive
                .verify_collection(&cmd.collection)
                .map_err(drive_error)?;
            (cmd.remote_endpoint, cmd.cloud_recovery_path) =
                drive_mapping_paths(drive, &cmd.collection);
        }
        let (remote_endpoint, cloud_recovery_path) = match cmd.remote_endpoint {
            RemoteEndpoint::LocalDir(remote_dir) => {
                if !remote_dir.is_dir() {
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
                let canonical_remote =
                    fs::canonicalize(&remote_dir).map_err(|err| ManagerError::Io {
                        cause: format!(
                            "Failed to canonicalize remote fixture directory `{}`: {err}.",
                            remote_dir.display()
                        ),
                        next_step: "Verify the destination folder exists and is accessible."
                            .to_string(),
                    })?;
                let resolved_remote_rec = resolve_absolute_path(
                    "remote_recovery_root",
                    Path::new(&cmd.cloud_recovery_path),
                )?;
                (
                    RemoteEndpoint::LocalDir(canonical_remote),
                    resolved_remote_rec.to_string_lossy().into_owned(),
                )
            }
            other => (other, cmd.cloud_recovery_path),
        };

        let volume = observe_binding(self.inner.volume_probe.as_ref(), &cmd.local_binding);
        volume.check(&cmd.id, &cmd.local_binding)?;
        let mut guard = self.inner.state.lock().unwrap();
        let existing_same = guard.store.get_mapping(&cmd.id)?;
        let updating_draft = match &existing_same {
            Some(m) if m.state.readiness() == Readiness::Unconfigured => true,
            Some(m)
                if !is_draft
                    && m.state.readiness() == Readiness::NeedsInitialization
                    && m.active_generation_id.is_none()
                    && m.state.execution == Execution::Idle =>
            {
                // Allow Finish Setup to save reviewed edits & exclusions before starting first sync (HIGH #1).
                true
            }
            Some(_) => {
                return Err(ManagerError::PreflightRejected {
                    cause: format!("A mapping with ID `{}` already exists.", cmd.id),
                    next_step: "Choose a different mapping ID or inspect the existing mapping."
                        .to_string(),
                });
            }
            None => false,
        };
        if guard.store.get_account(&cmd.account_id)?.is_none() {
            return Err(ManagerError::NotFound {
                cause: format!("Account `{}` is not configured.", cmd.account_id),
                next_step: "Connect or register the account before adding mappings for it."
                    .to_string(),
            });
        }

        let existing: Vec<MappingRecord> = guard
            .store
            .list_mappings()?
            .into_iter()
            .filter(|m| !updating_draft || m.id != cmd.id)
            .collect();
        let candidate = LayoutCandidate {
            mapping_id: &cmd.id,
            local_binding: &cmd.local_binding,
            remote_endpoint: remote_endpoint.clone(),
            local_recovery_root: local_recovery_root.clone(),
            cloud_recovery_path: &cloud_recovery_path,
        };
        validate_complete_layout(
            &existing,
            Some(&candidate),
            &self.inner.state_dir,
            self.inner.finder_writer.dir(),
        )?;

        validate_mapping_collections(
            &existing,
            &cmd.account_id,
            &cmd.collection,
            &guard.device.id,
            drive.is_some(),
        )?;

        let effective_exclusions = effective_exclusions(&cmd.extra_exclusions)?;

        let (remote_kind, remote_value) = remote_endpoint.to_db_cols();
        let now = self.inner.clock.now_rfc3339();
        let initial_readiness = if is_draft {
            Readiness::Unconfigured
        } else {
            Readiness::NeedsInitialization
        };
        let initial_state = MappingState::new(initial_readiness, Execution::Idle, Intent::Enabled);

        let record = MappingRecord {
            id: cmd.id.clone(),
            device_id: guard.device.id.clone(),
            account_id: cmd.account_id,
            collection: cmd.collection,
            local_binding: cmd.local_binding,
            remote_endpoint_kind: remote_kind,
            remote_endpoint_value: remote_value,
            local_recovery_root,
            cloud_recovery_path,
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
            unreviewed_conflict_run_id: None,
            created_at: existing_same
                .as_ref()
                .map(|m| m.created_at.clone())
                .unwrap_or_else(|| now.clone()),
            updated_at: now.clone(),
        };

        if updating_draft {
            guard.store.update_mapping_draft(&record)?;
        } else {
            guard.store.insert_mapping(&record)?;
        }
        guard.volumes.insert(record.id.clone(), volume);
        let snapshot = publish_updated_snapshot(&self.inner, &mut guard)?;
        self.inner
            .event_bus
            .publish_event(ManagerEvent::MappingAdded {
                mapping_id: cmd.id.clone(),
                timestamp: now,
            });

        Ok(CommandAcceptance {
            outcome: if is_draft {
                CommandOutcome::DraftSaved { mapping_id: cmd.id }
            } else {
                CommandOutcome::MappingAdded { mapping_id: cmd.id }
            },
            snapshot,
        })
    }

    fn handle_start_initialize(
        &self,
        cmd: InitializeCommand,
    ) -> Result<CommandAcceptance, ManagerError> {
        let mut guard = self.inner.state.lock().unwrap();
        if guard.storage_blocked_mappings.contains(&cmd.mapping_id) {
            return Err(ManagerError::Storage {
                cause: format!(
                    "Mapping `{}` is held because a previous database save failed.",
                    cmd.mapping_id
                ),
                next_step:
                    "Check free disk space and restart FolderSync to reconcile unfinished runs."
                        .to_string(),
            });
        }
        if let Some(blocking) =
            find_blocking_global_orphan(&guard.store, self.inner.process_inspector.as_ref())?
        {
            return Err(ManagerError::ActiveJobConflict {
                active_mapping_id: blocking.mapping_id.clone(),
                cause: format!(
                    "Cannot start first sync while `{}` has an unresolved hold ({}).",
                    blocking.mapping_id,
                    plain_recovery_reason_label(&blocking.reason_kind)
                ),
                next_step:
                    "Open recovery details to resolve the earlier sync process before starting another sync."
                        .to_string(),
            });
        }
        if let Some(active) = &guard.active_run {
            return Err(ManagerError::ActiveJobConflict {
                active_mapping_id: active.mapping_id.clone(),
                cause: format!(
                    "Another sync (`{}`) is currently running.",
                    active.mapping_id
                ),
                next_step:
                    "Wait for the active sync to finish or stop it before starting a first sync."
                        .to_string(),
            });
        }
        if let Some((in_preflight, attempt_id)) = &guard.active_init_attempt
            && !guard.cancelled_init_attempts.contains(attempt_id)
        {
            return Err(ManagerError::ActiveJobConflict {
                active_mapping_id: in_preflight.clone(),
                cause: format!("First sync setup is already checking `{in_preflight}`."),
                next_step: "Wait for that check to finish before starting another first sync."
                    .to_string(),
            });
        }
        if let Some(in_preflight) = &guard.preflight_mapping {
            return Err(ManagerError::ActiveJobConflict {
                active_mapping_id: in_preflight.clone(),
                cause: format!("First sync setup is already checking `{in_preflight}`."),
                next_step: "Wait for that check to finish before starting another first sync."
                    .to_string(),
            });
        }
        if let Some(queued) = guard.job_queue.front() {
            return Err(ManagerError::ActiveJobConflict {
                active_mapping_id: queued.mapping_id.clone(),
                cause: format!(
                    "Another sync (`{}`) is already waiting in the queue.",
                    queued.mapping_id
                ),
                next_step: "Wait for queued syncs to finish before starting a first sync."
                    .to_string(),
            });
        }

        let mut mapping =
            guard
                .store
                .get_mapping(&cmd.mapping_id)?
                .ok_or_else(|| ManagerError::NotFound {
                    cause: format!("Mapping `{}` does not exist.", cmd.mapping_id),
                    next_step: "Add the mapping before starting its first sync.".to_string(),
                })?;
        validate_safe_path_id("mapping_id", mapping.id.as_str())?;

        if mapping.state.readiness() != Readiness::NeedsInitialization
            || mapping.state.execution != Execution::Idle
        {
            return Err(ManagerError::InvalidState {
                mapping_id: mapping.id.clone(),
                cause: format!(
                    "Mapping `{}` cannot start first sync while {} ({}).",
                    mapping.id,
                    plain_readiness_label(mapping.state.readiness()),
                    plain_execution_label(mapping.state.execution)
                ),
                next_step:
                    "Only idle mappings waiting for first sync can be initialized; use Sync now or recovery for already set up mappings."
                        .to_string(),
            });
        }

        if guard.store.get_account(&mapping.account_id)?.is_none() {
            return Err(ManagerError::NotFound {
                cause: format!("Account `{}` is missing.", mapping.account_id),
                next_step: "Reconnect the account before starting first sync.".to_string(),
            });
        }

        guard.next_init_attempt_id += 1;
        let attempt_id = guard.next_init_attempt_id;
        mapping.state.execution = Execution::Queued;
        mapping.updated_at = self.inner.clock.now_rfc3339();
        guard.store.update_mapping_runtime(&mapping)?;
        guard.job_queue.push_back(QueuedJob {
            automatic: false,
            mapping_id: mapping.id.clone(),
            kind: QueuedJobKind::InitializePreflight {
                expected_direction: cmd.expected_direction,
                attempt_id,
            },
        });
        let snapshot = publish_updated_snapshot(&self.inner, &mut guard)?;
        self.inner.worker_cv.notify_all();

        Ok(CommandAcceptance {
            outcome: CommandOutcome::InitializationPreflightQueued {
                mapping_id: mapping.id,
            },
            snapshot,
        })
    }

    fn handle_initialize(&self, cmd: InitializeCommand) -> Result<CommandAcceptance, ManagerError> {
        // Phase 1 (short lock): validate state, claim preflight reservation, capture inputs, then DROP mutex!
        let snapshot_inputs = {
            let mut guard = self.inner.state.lock().unwrap();
            if guard.storage_blocked_mappings.contains(&cmd.mapping_id) {
                return Err(ManagerError::Storage {
                    cause: format!(
                        "Mapping `{}` is held because a previous database save failed.",
                        cmd.mapping_id
                    ),
                    next_step:
                        "Check free disk space and restart FolderSync to reconcile unfinished runs."
                            .to_string(),
                });
            }
            if let Some(blocking) =
                find_blocking_global_orphan(&guard.store, self.inner.process_inspector.as_ref())?
            {
                return Err(ManagerError::ActiveJobConflict {
                    active_mapping_id: blocking.mapping_id.clone(),
                    cause: format!(
                        "Cannot start first sync while `{}` has an unresolved hold ({}).",
                        blocking.mapping_id,
                        plain_recovery_reason_label(&blocking.reason_kind)
                    ),
                    next_step:
                        "Open recovery details to resolve the earlier sync process before starting another sync."
                            .to_string(),
                });
            }
            if let Some(active) = &guard.active_run {
                return Err(ManagerError::ActiveJobConflict {
                    active_mapping_id: active.mapping_id.clone(),
                    cause: format!(
                        "Another sync (`{}`) is currently running.",
                        active.mapping_id
                    ),
                    next_step:
                        "Wait for the active sync to finish or stop it before starting a first sync."
                            .to_string(),
                });
            }
            if let Some(in_preflight) = guard
                .preflight_mapping
                .as_ref()
                .or_else(|| guard.active_init_attempt.as_ref().map(|(id, _)| id))
            {
                return Err(ManagerError::ActiveJobConflict {
                    active_mapping_id: in_preflight.clone(),
                    cause: format!("First sync setup is already checking `{in_preflight}`."),
                    next_step: "Wait for that check to finish before starting another first sync."
                        .to_string(),
                });
            }
            if let Some(queued) = guard.job_queue.front() {
                return Err(ManagerError::ActiveJobConflict {
                    active_mapping_id: queued.mapping_id.clone(),
                    cause: format!(
                        "Another sync (`{}`) is already waiting in the queue.",
                        queued.mapping_id
                    ),
                    next_step: "Wait for queued syncs to finish before starting a first sync."
                        .to_string(),
                });
            }

            let mapping = guard.store.get_mapping(&cmd.mapping_id)?.ok_or_else(|| {
                ManagerError::NotFound {
                    cause: format!("Mapping `{}` does not exist.", cmd.mapping_id),
                    next_step: "Add the mapping before starting its first sync.".to_string(),
                }
            })?;
            validate_safe_path_id("mapping_id", mapping.id.as_str())?;

            if mapping.state.readiness() != Readiness::NeedsInitialization
                || mapping.state.execution != Execution::Idle
            {
                return Err(ManagerError::InvalidState {
                    mapping_id: mapping.id.clone(),
                    cause: format!(
                        "Mapping `{}` cannot start first sync while {} ({}).",
                        mapping.id,
                        plain_readiness_label(mapping.state.readiness()),
                        plain_execution_label(mapping.state.execution)
                    ),
                    next_step:
                        "Only idle mappings waiting for first sync can be initialized; use Sync now or recovery for already set up mappings."
                            .to_string(),
                });
            }

            let account = guard
                .store
                .get_account(&mapping.account_id)?
                .ok_or_else(|| ManagerError::NotFound {
                    cause: format!("Account `{}` is missing.", mapping.account_id),
                    next_step: "Reconnect the account before starting first sync.".to_string(),
                })?;
            let all_mappings = guard.store.list_mappings()?;
            let gen_num = guard.store.next_generation_number(&mapping.id)?;
            guard.preflight_mapping = Some(mapping.id.clone());
            (mapping, account, all_mappings, gen_num)
        };

        struct PreflightResetGuard<'a>(&'a ManagerInner);
        impl Drop for PreflightResetGuard<'_> {
            fn drop(&mut self) {
                if let Ok(mut g) = self.0.state.lock() {
                    g.preflight_mapping = None;
                    self.0.worker_cv.notify_all();
                }
            }
        }
        let _preflight_guard = PreflightResetGuard(&self.inner);

        // Phase 2 (NO state mutex held!): run layout checks, recursive directory inventory,
        // engine probe, recovery directory creation, and marker setup.
        let (mapping_snap, account, all_mappings, gen_num) = snapshot_inputs;
        let preflight = run_init_preflight_unlocked(
            &self.inner,
            &mapping_snap,
            &account,
            &all_mappings,
            gen_num,
            cmd.expected_direction,
            &|| false,
        )?
        .ok_or_else(|| ManagerError::InvalidState {
            mapping_id: cmd.mapping_id.clone(),
            cause: "First sync setup was cancelled.".to_string(),
            next_step: "Start first sync again when ready.".to_string(),
        })?;

        // Phase 3 (re-acquire mutex): recheck state, persist generation, queue job, publish snapshot.
        let mut guard = self.inner.state.lock().unwrap();
        if guard.shutdown {
            return Err(ManagerError::InvalidState {
                mapping_id: cmd.mapping_id,
                cause: "FolderSync is shutting down.".to_string(),
                next_step: "Reopen FolderSync to finish setup.".to_string(),
            });
        }
        let mut mapping =
            guard
                .store
                .get_mapping(&cmd.mapping_id)?
                .ok_or_else(|| ManagerError::NotFound {
                    cause: format!("Mapping `{}` no longer exists.", cmd.mapping_id),
                    next_step: "Add the mapping again to set it up.".to_string(),
                })?;
        if !same_volume_inputs(&mapping_snap, &mapping)
            || mapping_snap.exclusions != mapping.exclusions
            || mapping.state.readiness() != Readiness::NeedsInitialization
            || mapping.state.execution != Execution::Idle
        {
            return Err(ManagerError::InvalidState {
                mapping_id: mapping.id.clone(),
                cause: format!(
                    "Mapping `{}` changed state during preflight ({} / {}).",
                    mapping.id,
                    plain_readiness_label(mapping.state.readiness()),
                    plain_execution_label(mapping.state.execution)
                ),
                next_step: "Review the mapping status and retry first sync.".to_string(),
            });
        }

        let gen_id = preflight.gen_record.id.clone();
        let direction = preflight.direction;
        write_binding_marker(&self.inner.state_dir, &preflight.gen_record)?;
        guard.store.insert_generation(&preflight.gen_record)?;

        mapping.state.execution = Execution::Queued;
        mapping.updated_at = self.inner.clock.now_rfc3339();
        guard.store.update_mapping_runtime(&mapping)?;

        guard.job_queue.push_front(QueuedJob {
            automatic: false,
            mapping_id: mapping.id.clone(),
            kind: QueuedJobKind::Prepared {
                generation_id: gen_id,
                mode: RunMode::Initialize(direction),
            },
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
        let mut checked = read_mapping_check(&self.inner, &mapping_id)?;
        let mut guard = self.inner.state.lock().unwrap();
        if guard.storage_blocked_mappings.contains(&mapping_id) {
            return Err(ManagerError::Storage {
                cause: format!(
                    "Mapping `{mapping_id}` is held because a previous database save failed."
                ),
                next_step:
                    "Check free disk space and restart FolderSync to reconcile unfinished runs."
                        .to_string(),
            });
        }
        if let Some(blocking) =
            find_blocking_global_orphan(&guard.store, self.inner.process_inspector.as_ref())?
        {
            return Err(ManagerError::ActiveJobConflict {
                active_mapping_id: blocking.mapping_id.clone(),
                cause: format!(
                    "Cannot start a sync while `{}` has an unresolved hold ({}).",
                    blocking.mapping_id,
                    plain_recovery_reason_label(&blocking.reason_kind)
                ),
                next_step:
                    "Open recovery details to resolve the earlier sync process before starting another sync."
                        .to_string(),
            });
        }

        let mut mapping = reload_checked_mapping(&guard, &checked.mapping)?;
        if apply_checked_volume(&self.inner, &mut guard, &mut mapping, &checked)? {
            publish_updated_snapshot(&self.inner, &mut guard)?;
        }
        if let Some(volume) = guard.volumes.get(&mapping.id) {
            volume.check(&mapping.id, &mapping.local_binding)?;
        }
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
                    "Mapping `{}` is not ready to sync ({}).",
                    mapping.id,
                    plain_readiness_label(mapping.state.readiness())
                ),
                next_step: match mapping.state.readiness() {
                    Readiness::NeedsInitialization => {
                        "Finish first sync setup before syncing this folder.".to_string()
                    }
                    Readiness::RecoveryRequired => {
                        "Open recovery details before resuming sync.".to_string()
                    }
                    Readiness::VolumeUnavailable => {
                        "Reconnect the drive and try again.".to_string()
                    }
                    Readiness::AuthenticationRequired => {
                        "Reconnect the account and try again.".to_string()
                    }
                    Readiness::EndpointChanged => {
                        "Review the changed folder before syncing.".to_string()
                    }
                    Readiness::Unconfigured => "Finish setting up the mapping first.".to_string(),
                    Readiness::Ready => unreachable!(),
                },
            });
        }

        // Spec §6 / §13: Sync now on an already-queued mapping moves it to the FRONT of the queue!
        if mapping.state.execution == Execution::Queued {
            if let Some(pos) = guard
                .job_queue
                .iter()
                .position(|j| j.mapping_id == mapping.id)
                && let Some(mut job) = guard.job_queue.remove(pos)
            {
                job.automatic = false;
                guard.job_queue.push_front(job);
            }
            let snapshot = publish_updated_snapshot(&self.inner, &mut guard)?;
            return Ok(CommandAcceptance {
                outcome: CommandOutcome::RunPrioritized {
                    mapping_id: mapping.id,
                },
                snapshot,
            });
        }

        if mapping.state.execution != Execution::Idle {
            return Err(ManagerError::InvalidState {
                mapping_id,
                cause: format!(
                    "Mapping `{}` is already {}.",
                    mapping.id,
                    plain_execution_label(mapping.state.execution)
                ),
                next_step: "Wait for the current run to finish or stop it.".to_string(),
            });
        }

        let Some(gen_id) = mapping.active_generation_id.clone() else {
            return Err(ManagerError::InvalidState {
                mapping_id,
                cause: format!("Mapping `{}` has no active setup generation.", mapping.id),
                next_step: "Finish first sync setup to establish a baseline.".to_string(),
            });
        };

        if let Some(Err((event, err))) = checked.endpoints.take() {
            record_preflight_check_failure(&self.inner, &mut guard, &mut mapping, event, &err)?;
            publish_updated_snapshot(&self.inner, &mut guard)?;
            return Err(*err);
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

        guard.job_queue.push_front(QueuedJob {
            automatic: false,
            mapping_id: mapping.id.clone(),
            kind: QueuedJobKind::Prepared {
                generation_id: gen_id,
                mode: RunMode::Normal,
            },
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

    fn handle_sync_ready_mappings(&self) -> Result<CommandAcceptance, ManagerError> {
        let candidates = mapping_check_inputs(&self.inner, true)?;
        let checks = candidates
            .into_iter()
            .filter(|(m, _)| {
                m.state.readiness() == Readiness::Ready
                    && m.state.execution == Execution::Idle
                    && m.state.intent == Intent::Enabled
            })
            .map(|(m, g)| check_mapping_volume(&self.inner, m, g, true))
            .collect::<Vec<_>>();
        let mut guard = self.inner.state.lock().unwrap();
        if let Some(blocking) =
            find_blocking_global_orphan(&guard.store, self.inner.process_inspector.as_ref())?
        {
            return Err(ManagerError::ActiveJobConflict {
                active_mapping_id: blocking.mapping_id.clone(),
                cause: format!(
                    "Cannot start syncing while `{}` has an unresolved hold ({}).",
                    blocking.mapping_id,
                    plain_recovery_reason_label(&blocking.reason_kind)
                ),
                next_step:
                    "Open recovery details to resolve the earlier sync process before starting another sync."
                        .to_string(),
            });
        }

        let mut queued_ids = Vec::new();
        let now = self.inner.clock.now_rfc3339();

        for checked in checks {
            let Some(mut mapping) = guard.store.get_mapping(&checked.mapping.id)? else {
                continue;
            };
            if !same_volume_inputs(&checked.mapping, &mapping) {
                continue;
            }
            apply_checked_volume(&self.inner, &mut guard, &mut mapping, &checked)?;
            if guard.storage_blocked_mappings.contains(&mapping.id)
                || mapping.state.readiness() != Readiness::Ready
                || mapping.state.execution != Execution::Idle
                || mapping.state.intent != Intent::Enabled
            {
                continue;
            }
            let Some(gen_id) = mapping.active_generation_id.clone() else {
                continue;
            };
            if let Some(Err((event, err))) = checked.endpoints {
                record_preflight_check_failure(&self.inner, &mut guard, &mut mapping, event, &err)?;
                continue;
            }
            let transition = apply_user_action(mapping.state, UserAction::RunNow);
            if transition.effect != TransitionEffect::ScheduleRun {
                continue;
            }
            mapping.state = transition.state;
            mapping.updated_at = now.clone();
            guard.store.update_mapping_runtime(&mapping)?;
            guard.job_queue.push_back(QueuedJob {
                automatic: false,
                mapping_id: mapping.id.clone(),
                kind: QueuedJobKind::Prepared {
                    generation_id: gen_id,
                    mode: RunMode::Normal,
                },
            });
            queued_ids.push(mapping.id);
        }

        let snapshot = publish_updated_snapshot(&self.inner, &mut guard)?;
        if !queued_ids.is_empty() {
            self.inner.worker_cv.notify_all();
        }
        Ok(CommandAcceptance {
            outcome: CommandOutcome::ReadyMappingsQueued {
                mapping_ids: queued_ids,
            },
            snapshot,
        })
    }

    fn handle_set_scheduling_paused(
        &self,
        paused: bool,
    ) -> Result<CommandAcceptance, ManagerError> {
        let mut guard = self.inner.state.lock().unwrap();
        guard
            .store
            .set_scheduling_paused(&guard.device.id, paused)?;
        guard.scheduling_paused = paused;
        self.inner.worker_cv.notify_all();
        let snapshot = publish_updated_snapshot(&self.inner, &mut guard)?;
        Ok(CommandAcceptance {
            outcome: CommandOutcome::SchedulingPauseUpdated { paused },
            snapshot,
        })
    }

    fn handle_acknowledge_conflicts(
        &self,
        mapping_id: MappingId,
    ) -> Result<CommandAcceptance, ManagerError> {
        let mut guard = self.inner.state.lock().unwrap();
        let mut mapping =
            guard
                .store
                .get_mapping(&mapping_id)?
                .ok_or_else(|| ManagerError::NotFound {
                    cause: format!("Mapping `{mapping_id}` does not exist."),
                    next_step: "Check the mapping list and try again.".to_string(),
                })?;

        // Preserve `last_run_outcome == SuccessWithConflicts` and run history; clear ONLY the unreviewed notice (Q5).
        mapping.unreviewed_conflict_run_id = None;
        mapping.updated_at = self.inner.clock.now_rfc3339();
        guard.store.update_mapping_runtime(&mapping)?;
        let snapshot = publish_updated_snapshot(&self.inner, &mut guard)?;

        Ok(CommandAcceptance {
            outcome: CommandOutcome::ConflictsAcknowledged { mapping_id },
            snapshot,
        })
    }

    fn handle_check_again(&self, mapping_id: MappingId) -> Result<CommandAcceptance, ManagerError> {
        let mut checked = read_mapping_check(&self.inner, &mapping_id)?;
        let mut guard = self.inner.state.lock().unwrap();
        let mut mapping = reload_checked_mapping(&guard, &checked.mapping)?;
        if apply_checked_volume(&self.inner, &mut guard, &mut mapping, &checked)? {
            publish_updated_snapshot(&self.inner, &mut guard)?;
        }
        let Some(result) = checked.endpoints.take() else {
            return Err(ManagerError::InvalidState {
                mapping_id,
                cause: format!("Mapping `{}` has not completed first sync yet.", mapping.id),
                next_step: "Finish first sync setup before checking folder identity.".to_string(),
            });
        };
        result.map_err(|(_, err)| *err)?;

        // A successful Drive endpoint check includes a ready account and a
        // verified generation. Recheck readiness before clearing the hold.
        if mapping.state.readiness() == Readiness::AuthenticationRequired
            && mapping.remote_endpoint_kind == "drive_remote"
            && guard
                .store
                .get_account(&mapping.account_id)?
                .is_some_and(|account| account.auth_status == "ready")
        {
            mapping.state = apply_event(mapping.state, ObservedEvent::AuthenticationRestored).state;
        }

        match mapping.state.readiness() {
            Readiness::EndpointChanged => {
                mapping.state = apply_event(mapping.state, ObservedEvent::EndpointValidated).state;
            }
            Readiness::VolumeUnavailable => {
                mapping.state = apply_event(mapping.state, ObservedEvent::VolumeAvailable).state;
            }
            Readiness::AuthenticationRequired => {
                return Err(ManagerError::InvalidState {
                    mapping_id,
                    cause: format!(
                        "Account `{}` still requires sign-in before syncing.",
                        mapping.account_id
                    ),
                    next_step: "Reconnect the account in Accounts first.".to_string(),
                });
            }
            Readiness::RecoveryRequired => {
                return Err(ManagerError::InvalidState {
                    mapping_id,
                    cause: format!("Mapping `{}` still requires recovery review.", mapping.id),
                    next_step: "Open Start recovery… to review the run log and recovery copies."
                        .to_string(),
                });
            }
            _ => {}
        }

        mapping.updated_at = self.inner.clock.now_rfc3339();
        guard.store.update_mapping_runtime(&mapping)?;
        let snapshot = publish_updated_snapshot(&self.inner, &mut guard)?;

        Ok(CommandAcceptance {
            outcome: CommandOutcome::CheckAgainVerified { mapping_id },
            snapshot,
        })
    }

    fn handle_complete_recovery(
        &self,
        mapping_id: MappingId,
        resume_and_run: bool,
    ) -> Result<CommandAcceptance, ManagerError> {
        let mut checked = read_mapping_check(&self.inner, &mapping_id)?;
        let mut guard = self.inner.state.lock().unwrap();
        if let Some(blocking) =
            find_blocking_global_orphan(&guard.store, self.inner.process_inspector.as_ref())?
        {
            return Err(ManagerError::ActiveJobConflict {
                active_mapping_id: blocking.mapping_id.clone(),
                cause: format!(
                    "Cannot clear recovery while `{}` has an unresolved hold ({}).",
                    blocking.mapping_id,
                    plain_recovery_reason_label(&blocking.reason_kind)
                ),
                next_step:
                    "Confirm the earlier sync process has stopped or export diagnostics before resuming."
                        .to_string(),
            });
        }

        let mut mapping = reload_checked_mapping(&guard, &checked.mapping)?;
        if apply_checked_volume(&self.inner, &mut guard, &mut mapping, &checked)? {
            publish_updated_snapshot(&self.inner, &mut guard)?;
        }
        if mapping.state.readiness() != Readiness::RecoveryRequired {
            return Err(ManagerError::InvalidState {
                mapping_id,
                cause: format!(
                    "Mapping `{}` is not waiting for recovery ({}).",
                    mapping.id,
                    plain_readiness_label(mapping.state.readiness())
                ),
                next_step: "No recovery action is needed for this mapping.".to_string(),
            });
        }

        let next_readiness = if let Some(result) = checked.endpoints.take() {
            result.map_err(|(_, err)| *err)?;
            sync_core::PostRecoveryReadiness::Ready
        } else {
            sync_core::PostRecoveryReadiness::NeedsInitialization
        };

        let now = self.inner.clock.now_rfc3339();
        mapping.state = apply_event(
            mapping.state,
            ObservedEvent::RecoveryCompleted { next_readiness },
        )
        .state;

        let mut queue_gen_id = None;
        if resume_and_run && next_readiness == sync_core::PostRecoveryReadiness::Ready {
            mapping.state = apply_user_action(mapping.state, UserAction::Resume).state;
            mapping.state.execution = Execution::Idle;
            let run_t = apply_user_action(mapping.state, UserAction::RunNow);
            if run_t.effect == TransitionEffect::ScheduleRun {
                mapping.state = run_t.state;
                queue_gen_id = mapping.active_generation_id.clone();
            }
        } else {
            mapping.state = apply_user_action(mapping.state, UserAction::Pause).state;
        }
        mapping.updated_at = now.clone();

        let note = if resume_and_run {
            "User completed recovery checklist and resumed normal sync."
        } else {
            "User completed recovery checklist and kept mapping paused."
        };
        guard
            .store
            .resolve_recovery_and_update_mapping(&mapping, &now, note)?;

        if let Some(gen_id) = queue_gen_id {
            guard.job_queue.push_back(QueuedJob {
                automatic: false,
                mapping_id: mapping.id.clone(),
                kind: QueuedJobKind::Prepared {
                    generation_id: gen_id,
                    mode: RunMode::Normal,
                },
            });
            self.inner.worker_cv.notify_all();
        }

        let snapshot = publish_updated_snapshot(&self.inner, &mut guard)?;
        Ok(CommandAcceptance {
            outcome: CommandOutcome::RecoveryCompleted {
                mapping_id: mapping.id,
                resumed: resume_and_run,
            },
            snapshot,
        })
    }

    fn handle_remove_mapping(
        &self,
        mapping_id: MappingId,
    ) -> Result<CommandAcceptance, ManagerError> {
        let mut guard = self.inner.state.lock().unwrap();
        let mapping =
            guard
                .store
                .get_mapping(&mapping_id)?
                .ok_or_else(|| ManagerError::NotFound {
                    cause: format!("Mapping `{mapping_id}` does not exist."),
                    next_step: "Check the mapping list and try again.".to_string(),
                })?;

        if matches!(
            mapping.state.execution,
            Execution::Running | Execution::Stopping
        ) || guard
            .active_run
            .as_ref()
            .is_some_and(|a| a.mapping_id == mapping.id)
        {
            return Err(ManagerError::InvalidState {
                mapping_id,
                cause: format!(
                    "Mapping `{}` is currently {}.",
                    mapping.id,
                    plain_execution_label(mapping.state.execution)
                ),
                next_step:
                    "Stop the running sync and wait for it to finish before removing this mapping."
                        .to_string(),
            });
        }

        // Refuse removal while any live or ambiguous orphan evidence exists for this mapping (HIGH #7).
        for case in guard.store.list_open_recovery_cases(&mapping.id)? {
            let blocks_removal = match case.reason_kind.as_str() {
                "orphan_ambiguous" => true,
                "orphan_running" | "shutdown_timeout" => {
                    let run = case
                        .run_id
                        .as_ref()
                        .map(|rid| guard.store.get_run(rid))
                        .transpose()?
                        .flatten();
                    let alive = case.orphan_pid.and_then(|pid| {
                        probe_orphan_liveness(
                            self.inner.process_inspector.as_ref(),
                            pid,
                            run.as_ref(),
                        )
                    });
                    alive != Some(false)
                }
                _ => false,
            };
            if blocks_removal {
                return Err(ManagerError::ActiveJobConflict {
                    active_mapping_id: mapping.id.clone(),
                    cause: format!(
                        "Cannot remove mapping `{}` while an earlier sync process may still be running ({}).",
                        mapping.id,
                        plain_recovery_reason_label(&case.reason_kind)
                    ),
                    next_step:
                        "Open recovery details to confirm the earlier sync process has stopped before removing this mapping."
                            .to_string(),
                });
            }
        }

        guard.job_queue.retain(|j| j.mapping_id != mapping.id);
        guard.storage_blocked_mappings.remove(&mapping.id);
        // Deletes only DB records; local folder, cloud folder, and recovery copies stay untouched (§7).
        guard.store.delete_mapping_cascade(&mapping.id)?;
        guard.volumes.remove(&mapping.id);
        guard.schedules.remove(&mapping.id);
        let snapshot = publish_updated_snapshot(&self.inner, &mut guard)?;
        self.inner.idle_cv.notify_all();

        Ok(CommandAcceptance {
            outcome: CommandOutcome::MappingRemoved { mapping_id },
            snapshot,
        })
    }

    fn handle_set_schedule_interval(
        &self,
        mapping_id: MappingId,
        interval_secs: u64,
    ) -> Result<CommandAcceptance, ManagerError> {
        if !(60..=86_400).contains(&interval_secs) {
            return Err(ManagerError::PreflightRejected {
                cause: "Schedule interval must be between 1 minute and 24 hours.".to_string(),
                next_step:
                    "Choose one of the standard schedule intervals (5, 15, 30, or 60 minutes)."
                        .to_string(),
            });
        }
        let mut guard = self.inner.state.lock().unwrap();
        if guard.store.get_mapping(&mapping_id)?.is_none() {
            return Err(ManagerError::NotFound {
                cause: format!("Mapping `{mapping_id}` does not exist."),
                next_step: "Check the mapping list and try again.".to_string(),
            });
        }
        let now = self.inner.clock.now_rfc3339();
        guard
            .store
            .update_mapping_schedule_interval(&mapping_id, interval_secs, &now)?;
        let snapshot = publish_updated_snapshot(&self.inner, &mut guard)?;
        Ok(CommandAcceptance {
            outcome: CommandOutcome::ScheduleIntervalUpdated {
                mapping_id,
                interval_secs,
            },
            snapshot,
        })
    }

    fn handle_remove_account(
        &self,
        account_id: AccountId,
    ) -> Result<CommandAcceptance, ManagerError> {
        let account = self
            .inner
            .accounts
            .get_account(&account_id)
            .map_err(drive_error)?;
        let _account_lock = if account.is_some_and(|account| account.kind == "drive") {
            Some(
                self.inner
                    .accounts
                    .try_account_lock(&account_id)
                    .map_err(drive_error)?,
            )
        } else {
            None
        };
        let mut guard = self.inner.state.lock().unwrap();
        if guard.folder_sizes.values().any(|size| {
            size.account_id == account_id && size.status == FolderSizeStatus::Calculating
        }) {
            return Err(ManagerError::PreflightRejected {
                cause: "A folder size is being calculated for this account.".into(),
                next_step:
                    "Cancel the calculation or wait for it to finish before removing the account."
                        .into(),
            });
        }
        if guard
            .authorization
            .as_ref()
            .is_some_and(|(id, _)| id == &account_id)
        {
            return Err(ManagerError::PreflightRejected {
                cause: "This account is signing in.".into(),
                next_step: "Cancel sign-in or wait for it to finish before removing the account."
                    .into(),
            });
        }
        if guard.store.get_account(&account_id)?.is_none() {
            return Err(ManagerError::NotFound {
                cause: format!("Account `{account_id}` does not exist."),
                next_step: "Check the Accounts list and try again.".to_string(),
            });
        }
        let mappings = guard.store.list_mappings()?;
        let bound_count = mappings
            .iter()
            .filter(|m| m.account_id == account_id)
            .count();
        if bound_count > 0 {
            let word = if bound_count == 1 {
                "1 mapping".to_string()
            } else {
                format!("{bound_count} mappings")
            };
            return Err(ManagerError::InvalidState {
                mapping_id: MappingId::new(account_id.as_str()),
                cause: format!("Account `{account_id}` still has {word} on this Mac."),
                next_step: format!("Remove its {word} first before removing the account."),
            });
        }
        guard.store.delete_account(&account_id)?;
        guard.folder_sizes.retain(|(id, _), _| id != &account_id);
        let snapshot = publish_updated_snapshot(&self.inner, &mut guard)?;
        Ok(CommandAcceptance {
            outcome: CommandOutcome::AccountRemoved { account_id },
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

        // Persist BEFORE mutating in-memory queue or acknowledging!
        guard.store.update_mapping_runtime(&mapping)?;
        guard.job_queue.retain(|j| j.mapping_id != mapping.id);
        if let Some((id, attempt)) = &guard.active_init_attempt
            && id == &mapping.id
        {
            let attempt = *attempt;
            guard.cancelled_init_attempts.insert(attempt);
            guard.active_init_attempt = None;
        }
        if let Some(schedule) = guard.schedules.get_mut(&mapping.id) {
            schedule.hint_at = None;
        }
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
        if transition.effect == TransitionEffect::ScheduleRun {
            mapping.state.execution = Execution::Idle;
        }
        mapping.updated_at = self.inner.clock.now_rfc3339();

        // Persist BEFORE acknowledging!
        guard.store.update_mapping_runtime(&mapping)?;
        if let Some(schedule) = guard.schedules.get_mut(&mapping.id) {
            schedule.hint_at = Some(self.inner.clock.now_utc());
        }
        let snapshot = publish_updated_snapshot(&self.inner, &mut guard)?;

        Ok(CommandAcceptance {
            outcome: CommandOutcome::Resumed {
                mapping_id: mapping.id,
            },
            snapshot,
        })
    }

    fn handle_stop_run(&self, mapping_id: MappingId) -> Result<CommandAcceptance, ManagerError> {
        // 1. Commit stopping state & mark stop_requested BEFORE publishing Finder status.
        let stopper =
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
                            "Mapping `{}` has no active or queued run to stop ({}).",
                            mapping.id,
                            plain_execution_label(mapping.state.execution)
                        ),
                        next_step: "Only queued or running sync jobs can be stopped.".to_string(),
                    });
                }

                let transition = apply_user_action(mapping.state, UserAction::StopRun);
                mapping.state = transition.state;
                mapping.updated_at = self.inner.clock.now_rfc3339();
                guard.store.update_mapping_runtime(&mapping)?;
                // Cancel any active or queued initialization preflight attempt for this mapping (HIGH #5).
                if let Some((active_mid, attempt_id)) = guard.active_init_attempt.as_ref()
                    && active_mid == &mapping.id
                {
                    let cancelled_id = *attempt_id;
                    guard.cancelled_init_attempts.insert(cancelled_id);
                    guard.active_init_attempt = None;
                }
                let queued_attempts: Vec<u64> = guard
                    .job_queue
                    .iter()
                    .filter(|j| j.mapping_id == mapping.id)
                    .filter_map(|j| match j.kind {
                        QueuedJobKind::InitializePreflight { attempt_id, .. } => Some(attempt_id),
                        QueuedJobKind::Prepared { .. } => None,
                    })
                    .collect();
                for attempt_id in queued_attempts {
                    guard.cancelled_init_attempts.insert(attempt_id);
                }
                guard.job_queue.retain(|j| j.mapping_id != mapping.id);
                if mapping.state.execution == Execution::Idle
                    && let Some(schedule) = guard.schedules.get_mut(&mapping.id)
                {
                    schedule.finish(self.inner.clock.now_utc(), false, "");
                    schedule.hint_at = None;
                }

                if transition.effect == TransitionEffect::RequestGracefulStop {
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
                }
            };

        // 2. Deliver stop signal BEFORE Finder publication (HIGH #9).
        if let Some(stopper) = stopper {
            stopper.request_stop()?;
        }

        // 3. Publish snapshot + non-fatal Finder write.
        let snapshot = {
            let mut guard = self.inner.state.lock().unwrap();
            let snap = publish_updated_snapshot(&self.inner, &mut guard)?;
            self.inner.idle_cv.notify_all();
            snap
        };

        Ok(CommandAcceptance {
            outcome: CommandOutcome::StopRequested { mapping_id },
            snapshot,
        })
    }

    fn handle_force_stop(&self, mapping_id: MappingId) -> Result<CommandAcceptance, ManagerError> {
        // 1. Atomically commit mapping hold + recovery case in ONE SQLite transaction.
        let (stopper, opened_event) = {
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
                        "Mapping `{}` has no active job to force-stop ({}).",
                        mapping.id,
                        plain_execution_label(mapping.state.execution)
                    ),
                    next_step: "Check the mapping status before requesting a force stop."
                        .to_string(),
                });
            }

            let transition = apply_user_action(mapping.state, UserAction::ForceStop);
            mapping.state = transition.state;
            let now = self.inner.clock.now_rfc3339();
            mapping.updated_at = now.clone();

            let run_id_for_case = guard
                .active_run
                .as_ref()
                .filter(|a| a.mapping_id == mapping.id)
                .map(|a| a.run_id.clone());

            let recovery_case = if transition.effect == TransitionEffect::ForceTerminateChild {
                let gen_workdir = mapping
                    .active_generation_id
                    .as_ref()
                    .and_then(|gid| guard.store.get_generation(gid).ok().flatten())
                    .map(|g| g.workdir_path);
                Some(RecoveryCaseRecord {
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
                })
            } else {
                None
            };

            // Commit transaction BEFORE mutating in-memory flags or queue (HIGH #7).
            guard
                .store
                .commit_mapping_and_recovery(&mapping, recovery_case.as_ref())?;

            guard.job_queue.retain(|j| j.mapping_id != mapping.id);
            if mapping.state.execution == Execution::Idle
                && let Some(schedule) = guard.schedules.get_mut(&mapping.id)
            {
                schedule.finish(self.inner.clock.now_utc(), false, "");
                schedule.hint_at = None;
            }
            let stopper = if let Some(active) = guard.active_run.as_mut()
                && active.mapping_id == mapping.id
            {
                active.force_stopped = true;
                active.stop_requested = true;
                active.stopper.as_ref().map(Arc::clone)
            } else {
                None
            };

            let ev = recovery_case.map(|case| ManagerEvent::RecoveryOpened {
                mapping_id: mapping.id.clone(),
                case_id: case.id,
                reason_kind: case.reason_kind,
                cause: case.cause,
                timestamp: now,
            });
            (stopper, ev)
        };

        // 2. Deliver force-stop signal BEFORE Finder publication (HIGH #9).
        if let Some(stopper) = stopper {
            stopper.force_stop()?;
        }

        // 3. Publish events & snapshot + non-fatal Finder write AFTER commit + signal.
        if let Some(ev) = opened_event {
            self.inner.event_bus.publish_event(ev);
        }
        let snapshot = {
            let mut guard = self.inner.state.lock().unwrap();
            let snap = publish_updated_snapshot(&self.inner, &mut guard)?;
            self.inner.idle_cv.notify_all();
            snap
        };

        Ok(CommandAcceptance {
            outcome: CommandOutcome::ForceStopped { mapping_id },
            snapshot,
        })
    }

    fn handle_inspect_recovery(
        &self,
        mapping_id: MappingId,
    ) -> Result<CommandAcceptance, ManagerError> {
        let checked = read_mapping_check(&self.inner, &mapping_id)?;
        let guard = self.inner.state.lock().unwrap();
        let mapping = reload_checked_mapping(&guard, &checked.mapping)?;
        let raw_cases = guard.store.list_open_recovery_cases(&mapping.id)?;
        let recent_runs = guard.store.list_runs_for_mapping(&mapping.id)?;

        let mut open_cases = Vec::with_capacity(raw_cases.len());
        for case in raw_cases {
            let orphan_still_alive = case.orphan_pid.and_then(|pid| {
                let run_match = case
                    .run_id
                    .as_ref()
                    .and_then(|rid| recent_runs.iter().find(|r| &r.id == rid));
                probe_orphan_liveness(self.inner.process_inspector.as_ref(), pid, run_match)
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

        let (identities_verified, identity_check_detail) = match checked.endpoints {
            Some(Ok(())) => (
                true,
                "Local and destination folders match setup.".to_owned(),
            ),
            Some(Err((_, err))) => (false, err.cause()),
            None => (
                false,
                "First sync has not established folder identities yet.".to_owned(),
            ),
        };

        let report = RecoveryReport {
            mapping_id: mapping.id,
            state: mapping.state,
            open_cases,
            workdir_files,
            has_lock_file,
            recent_runs,
            identities_verified,
            identity_check_detail,
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum BoundVolume {
    Present(MountedVolume),
    Missing,
    WrongDisk(String),
    Inaccessible(String),
    Moved(MountedVolume),
    Legacy,
}

impl BoundVolume {
    fn mounted(&self) -> Option<&MountedVolume> {
        match self {
            Self::Present(volume) | Self::Moved(volume) => Some(volume),
            Self::Missing | Self::WrongDisk(_) | Self::Inaccessible(_) | Self::Legacy => None,
        }
    }

    fn message(&self, binding: &LocalBinding) -> String {
        match self {
            Self::Present(volume) => volume.display_name.clone(),
            Self::Missing => format!("Waiting for {}", binding.volume_display_name()),
            Self::WrongDisk(name) => format!("A different disk named {name} is connected"),
            Self::Inaccessible(reason) => {
                format!("Can’t access {}: {reason}", binding.volume_display_name())
            }
            Self::Moved(volume) => format!(
                "{} is mounted at a new location. Review the local folder before syncing.",
                volume.display_name
            ),
            Self::Legacy => "Review the local folder to verify its disk identity.".to_owned(),
        }
    }

    fn check(
        &self,
        id: &MappingId,
        binding: &LocalBinding,
    ) -> Result<&MountedVolume, ManagerError> {
        match self {
            Self::Present(volume) => Ok(volume),
            _ => Err(ManagerError::InvalidState {
                mapping_id: id.clone(),
                cause: self.message(binding),
                next_step: match self {
                    Self::Moved(_) => "Reconnect the disk at its original mount or review setup.",
                    Self::Legacy => "Verify the drive and set up the mapping again.",
                    Self::Inaccessible(_) => "Check disk permissions and try again.",
                    _ => "Connect the disk selected during setup.",
                }
                .to_owned(),
            }),
        }
    }
}

fn observe_binding(probe: &dyn VolumeProbe, binding: &LocalBinding) -> BoundVolume {
    let expected = binding.volume.stable_id.as_str();
    // OS-assigned device numbers recorded by earlier versions cannot identify a disk.
    if expected == "unknown"
        || expected.starts_with("vol-")
        || expected.starts_with("dev-")
        || !binding.mount_path.is_absolute()
        || binding.relative_dir.as_os_str().is_empty()
        || binding
            .relative_dir
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return BoundVolume::Legacy;
    }
    let root = binding.resolved_path();
    // A deleted folder can have a still-mounted volume. Stop at the recorded mount
    // so a parent filesystem cannot masquerade as the missing disk.
    for path in root
        .ancestors()
        .take_while(|path| path.starts_with(&binding.mount_path))
    {
        match probe.probe(path) {
            Ok(volume) if volume.identity.stable_id == expected => {
                return if volume.binding_mount(&root) == binding.mount_path {
                    BoundVolume::Present(volume)
                } else {
                    BoundVolume::Moved(volume)
                };
            }
            Ok(volume) if volume.binding_mount(&root).starts_with(&binding.mount_path) => {
                return BoundVolume::WrongDisk(volume.display_name);
            }
            Ok(_) => break,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return BoundVolume::Inaccessible(err.to_string()),
        }
    }
    match probe.find(binding) {
        Ok(Some(volume)) if volume.binding_mount(&root) != binding.mount_path => {
            BoundVolume::Moved(volume)
        }
        Err(err) if err.kind() != io::ErrorKind::NotFound => {
            BoundVolume::Inaccessible(err.to_string())
        }
        _ => BoundVolume::Missing,
    }
}

struct MappingVolumeCheck {
    mapping: MappingRecord,
    observation: BoundVolume,
    paths_valid: Option<bool>,
    endpoints: Option<Result<(), EndpointCheckError>>,
}

fn active_generation(
    store: &SqliteStateStore,
    mapping: &MappingRecord,
) -> Result<Option<GenerationRecord>, ManagerError> {
    mapping
        .active_generation_id
        .as_ref()
        .map(|id| {
            store
                .get_generation(id)?
                .ok_or_else(|| ManagerError::Storage {
                    cause: format!("Active setup generation `{id}` is missing."),
                    next_step: "Open recovery details before syncing.".to_owned(),
                })
        })
        .transpose()
}

fn mapping_check_inputs(
    inner: &ManagerInner,
    verify_all: bool,
) -> Result<Vec<(MappingRecord, Option<GenerationRecord>)>, ManagerError> {
    let guard = inner.state.lock().unwrap();
    guard
        .store
        .list_mappings()?
        .into_iter()
        .map(|mapping| {
            let generation = if verify_all || mapping.state.readiness() != Readiness::Ready {
                active_generation(&guard.store, &mapping)?
            } else {
                None
            };
            Ok((mapping, generation))
        })
        .collect()
}

fn read_mapping_check(
    inner: &ManagerInner,
    id: &MappingId,
) -> Result<MappingVolumeCheck, ManagerError> {
    let (mapping, generation) = {
        let guard = inner.state.lock().unwrap();
        let mapping = lookup_mapping(&guard.store, id)?;
        let generation = active_generation(&guard.store, &mapping)?;
        (mapping, generation)
    };
    Ok(check_mapping_volume(inner, mapping, generation, true))
}

fn lookup_mapping(store: &SqliteStateStore, id: &MappingId) -> Result<MappingRecord, ManagerError> {
    store
        .get_mapping(id)?
        .ok_or_else(|| ManagerError::NotFound {
            cause: format!("Mapping `{id}` does not exist."),
            next_step: "Check the mapping list and try again.".to_owned(),
        })
}

fn same_volume_inputs(previous: &MappingRecord, current: &MappingRecord) -> bool {
    previous.local_binding == current.local_binding
        && previous.account_id == current.account_id
        && previous.active_generation_id == current.active_generation_id
        && previous.remote_endpoint_kind == current.remote_endpoint_kind
        && previous.remote_endpoint_value == current.remote_endpoint_value
        && previous.local_recovery_root == current.local_recovery_root
        && previous.cloud_recovery_path == current.cloud_recovery_path
        && previous.collection == current.collection
}

fn reload_checked_mapping(
    guard: &SharedState,
    previous: &MappingRecord,
) -> Result<MappingRecord, ManagerError> {
    let mapping = lookup_mapping(&guard.store, &previous.id)?;
    if !same_volume_inputs(previous, &mapping) {
        return Err(ManagerError::InvalidState {
            mapping_id: previous.id.clone(),
            cause: "The mapping changed while checking its folders.".to_owned(),
            next_step: "Review the current mapping and try again.".to_owned(),
        });
    }
    Ok(mapping)
}

fn check_mapping_volume(
    inner: &ManagerInner,
    mapping: MappingRecord,
    generation: Option<GenerationRecord>,
    verify_all: bool,
) -> MappingVolumeCheck {
    let observation = observe_binding(inner.volume_probe.as_ref(), &mapping.local_binding);
    let endpoints = generation
        .as_ref()
        .filter(|_| verify_all)
        .map(|generation| {
            verify_endpoints_for_generation(inner, &mapping, generation, &observation)?;
            if mapping.remote_endpoint_kind == "drive_remote" {
                let control = ProcessControl::with_timeout(DRIVE_COMMAND_TIMEOUT);
                let drive = inner
                    .accounts
                    .locked_drive(&mapping.account_id, &control)
                    .map_err(drive_endpoint_error)?;
                verify_drive_generation(&drive, &mapping, generation)?;
            }
            Ok(())
        });
    let paths_valid = match &endpoints {
        Some(Err((Some(ObservedEvent::EndpointChanged), _))) => Some(false),
        Some(_) => Some(true),
        None => match generation.as_ref() {
            Some(generation) if matches!(observation, BoundVolume::Present(_)) => {
                Some(verify_generation_paths(&inner.state_dir, &mapping, generation).is_ok())
            }
            _ if mapping.active_generation_id.is_none() => Some(true),
            _ => None,
        },
    };
    MappingVolumeCheck {
        mapping,
        observation,
        paths_valid,
        endpoints,
    }
}

/// Applies a completed observation to the current durable state with the lock held.
fn apply_checked_volume(
    inner: &ManagerInner,
    guard: &mut SharedState,
    mapping: &mut MappingRecord,
    checked: &MappingVolumeCheck,
) -> Result<bool, ManagerError> {
    let observation = &checked.observation;
    let old_state = mapping.state;
    let mut stop = false;
    let next = match observation {
        BoundVolume::Present(_) => {
            let available = apply_event(old_state, ObservedEvent::VolumeAvailable).state;
            match checked.paths_valid {
                Some(false) if available != old_state => {
                    apply_event(available, ObservedEvent::EndpointChanged).state
                }
                Some(_) => available,
                None => old_state,
            }
        }
        BoundVolume::Legacy => apply_event(old_state, ObservedEvent::EndpointChanged).state,
        unavailable => {
            let lost = apply_event(old_state, ObservedEvent::VolumeLost);
            stop = lost.effect == TransitionEffect::RequestGracefulStop;
            if matches!(unavailable, BoundVolume::Moved(_)) {
                apply_event(lost.state, ObservedEvent::EndpointChanged).state
            } else {
                lost.state
            }
        }
    };
    let previous = guard.volumes.get(&mapping.id);
    let changed = previous.is_none_or(|previous| !same_volume_status(previous, observation));
    let is_present = matches!(observation, BoundVolume::Present(_));

    if next != old_state {
        mapping.state = next;
        mapping.updated_at = inner.clock.now_rfc3339();
        guard.store.update_mapping_runtime(mapping)?;
        if old_state.execution == Execution::Queued && next.execution == Execution::Idle {
            guard.job_queue.retain(|job| job.mapping_id != mapping.id);
            if let Some((id, attempt)) = &guard.active_init_attempt
                && id == &mapping.id
            {
                guard.cancelled_init_attempts.insert(*attempt);
                guard.active_init_attempt = None;
            }
            inner.idle_cv.notify_all();
        }
    }

    if !is_present
        && let Some(active) = guard.active_run.as_mut()
        && active.mapping_id == mapping.id
    {
        active.volume_lost = true;
        if stop && !active.stop_requested {
            active.stop_requested = true;
            if let Some(stopper) = &active.stopper
                && let Err(err) = stopper.request_stop()
            {
                eprintln!("Failed to stop the sync for a disconnected disk: {err}");
            }
        }
    }

    if previous != Some(observation) {
        guard
            .volumes
            .insert(mapping.id.clone(), observation.clone());
    }
    if old_state.readiness() == Readiness::VolumeUnavailable
        && next.readiness() == Readiness::Ready
        && let Some(schedule) = guard.schedules.get_mut(&mapping.id)
    {
        schedule.hint_at = Some(inner.clock.now_utc());
    }
    Ok(changed || next != old_state)
}

fn restore_schedules(
    store: &SqliteStateStore,
    now: DateTime<Utc>,
) -> Result<Schedules, ManagerError> {
    let mut schedules = Schedules::new();
    for mapping in store.list_mappings()? {
        let Some(generation) = mapping.active_generation_id else {
            continue;
        };
        if !mapping.scheduling_enabled {
            continue;
        }
        let recent = store.retry_runs(&generation)?;
        let finished = recent
            .first()
            .and_then(|run| run.finished_at.as_deref())
            .or(mapping.last_run_completed_at.as_deref())
            .and_then(|ts| ts.parse::<DateTime<Utc>>().ok())
            .unwrap_or(now);
        let mut schedule = Schedule::new(generation, finished);
        let streak = recent
            .iter()
            .take_while(|run| run.outcome_kind.as_deref() == Some("retryable"))
            .count();
        if let Some(run) = recent.first().filter(|_| streak > 0) {
            schedule.retry_count = (streak as u32).min(MAX_RETRY_COUNT);
            schedule.retry_at = Some(finished + backoff(schedule.retry_count, run.id.as_str()));
        }
        schedules.insert(mapping.id, schedule);
    }
    Ok(schedules)
}

fn mark_environment_changed(inner: &ManagerInner, guard: &mut SharedState) {
    let now = inner.clock.now_utc();
    for schedule in guard.schedules.values_mut() {
        schedule.hint_at = Some(now);
    }
}

fn cancel_queued_automatic(
    inner: &ManagerInner,
    guard: &mut SharedState,
    mapping_id: &MappingId,
) -> Result<(), ManagerError> {
    if let Some(mut mapping) = guard.store.get_mapping(mapping_id)?
        && mapping.state.execution == Execution::Queued
    {
        mapping.state.execution = Execution::Idle;
        mapping.updated_at = inner.clock.now_rfc3339();
        guard.store.update_mapping_runtime(&mapping)?;
    }
    publish_updated_snapshot(inner, guard)?;
    Ok(())
}

fn scheduling_loop(inner: &ManagerInner, mut watcher: Box<dyn LocalChangeWatcher>) {
    let mut last_tick = inner.clock.now_utc();
    let mut previous_addresses = network_addresses().ok();
    loop {
        let changed = watcher.take_changed_roots();
        let now = inner.clock.now_utc();
        let addresses = network_addresses().ok();
        let network_returned = addresses.as_ref().is_some_and(|current| {
            !current.is_empty()
                && previous_addresses
                    .as_ref()
                    .is_some_and(|old| old != current)
        });
        if addresses.is_some() {
            previous_addresses = addresses;
        }
        let woke = now - last_tick > TimeDelta::seconds(15);
        if now < last_tick {
            // A backward wall-clock adjustment must not delay every deadline indefinitely.
            let shift = now - last_tick;
            let mut guard = inner.state.lock().unwrap();
            for schedule in guard.schedules.values_mut() {
                schedule.last_finished += shift;
                schedule.retry_at = schedule.retry_at.map(|at| at + shift);
                schedule.hint_at = schedule.hint_at.map(|at| at + shift);
            }
        }
        last_tick = now;
        let roots = {
            let mut guard = inner.state.lock().unwrap();
            if guard.shutdown {
                break;
            }
            if woke || network_returned {
                mark_environment_changed(inner, &mut guard);
            }
            match schedule_tick(inner, &mut guard, &changed) {
                Ok(roots) => roots,
                Err(err) => {
                    eprintln!("Automatic scheduling failed: {err}");
                    BTreeSet::new()
                }
            }
        };
        watcher.set_roots(&roots);
        let guard = inner.state.lock().unwrap();
        if guard.shutdown {
            break;
        }
        let _guard = inner
            .worker_cv
            .wait_timeout_while(guard, SCHEDULE_TICK, |state| !state.shutdown)
            .unwrap()
            .0;
    }
}

fn schedule_tick(
    inner: &ManagerInner,
    guard: &mut SharedState,
    changed: &BTreeSet<PathBuf>,
) -> Result<BTreeSet<PathBuf>, ManagerError> {
    let now = inner.clock.now_utc();
    let mut roots = BTreeSet::new();
    let mut queued = false;
    let mappings = guard.store.list_mappings()?;
    let mut orphan = None;
    for mut mapping in mappings {
        if !mapping.scheduling_enabled
            || mapping.state.readiness() != Readiness::Ready
            || mapping.state.intent != Intent::Enabled
            || guard.storage_blocked_mappings.contains(&mapping.id)
        {
            continue;
        }
        let Some(generation) = mapping.active_generation_id.clone() else {
            continue;
        };
        let root = mapping.local_binding.resolved_path();
        let hint = changed.contains(&root);
        roots.insert(root);
        let schedule = guard
            .schedules
            .entry(mapping.id.clone())
            .or_insert_with(|| Schedule::new(generation.clone(), now));
        if schedule.generation != generation {
            *schedule = Schedule::new(generation.clone(), now);
        }
        if hint {
            schedule.hint_at = Some(now);
        }
        if guard.scheduling_paused
            || mapping.state.execution != Execution::Idle
            || !schedule.due(now, mapping.schedule_interval_secs)
        {
            continue;
        }
        if orphan.is_none() {
            orphan = Some(
                find_blocking_global_orphan(&guard.store, inner.process_inspector.as_ref())?
                    .is_some(),
            );
        }
        if orphan == Some(true) {
            continue;
        }
        let transition = apply_user_action(mapping.state, UserAction::RunNow);
        mapping.state = transition.state;
        mapping.updated_at = inner.clock.now_rfc3339();
        guard.store.update_mapping_runtime(&mapping)?;
        guard.job_queue.push_back(QueuedJob {
            automatic: true,
            mapping_id: mapping.id,
            kind: QueuedJobKind::Prepared {
                generation_id: generation,
                mode: RunMode::Normal,
            },
        });
        queued = true;
        // Wake the worker even if persisting a later mapping in this tick fails.
        inner.worker_cv.notify_all();
    }
    if queued {
        publish_updated_snapshot(inner, guard)?;
    }
    Ok(roots)
}

fn same_volume_status(a: &BoundVolume, b: &BoundVolume) -> bool {
    match (a, b) {
        (BoundVolume::Present(a), BoundVolume::Present(b))
        | (BoundVolume::Moved(a), BoundVolume::Moved(b)) => {
            a.identity == b.identity
                && a.mount_path == b.mount_path
                && a.display_name == b.display_name
                && a.filesystem == b.filesystem
        }
        _ => a == b,
    }
}

fn refresh_volumes(inner: &ManagerInner) -> Result<(), ManagerError> {
    let mut changed = false;
    for (mapping, generation) in mapping_check_inputs(inner, false)? {
        let checked = check_mapping_volume(inner, mapping, generation, false);
        let mut guard = inner.state.lock().unwrap();
        if guard.shutdown {
            return Ok(());
        }
        let Some(mut mapping) = guard.store.get_mapping(&checked.mapping.id)? else {
            continue;
        };
        if same_volume_inputs(&checked.mapping, &mapping) {
            changed |= apply_checked_volume(inner, &mut guard, &mut mapping, &checked)?;
        }
    }
    let mut guard = inner.state.lock().unwrap();
    let now = inner.clock.now_utc();
    if changed {
        publish_updated_snapshot(inner, &mut guard)?;
    } else if now - guard.last_space_snapshot >= TimeDelta::seconds(30)
        && guard.latest_snapshot.mappings.iter().any(|mapping| {
            mapping.volume_space
                != guard
                    .volumes
                    .get(&mapping.id)
                    .and_then(BoundVolume::mounted)
                    .map(|volume| volume.space)
        })
    {
        publish_snapshot(inner, &mut guard, false)?;
    }
    Ok(())
}

fn volume_watch_loop(inner: &ManagerInner) {
    loop {
        let guard = inner.state.lock().unwrap();
        let guard = inner
            .worker_cv
            .wait_timeout_while(guard, VOLUME_POLL_INTERVAL, |state| !state.shutdown)
            .unwrap()
            .0;
        if guard.shutdown {
            break;
        }
        drop(guard);
        if let Err(err) = refresh_volumes(inner) {
            eprintln!("Failed to refresh disk status: {err}");
        }
    }
}

fn record_preflight_check_failure(
    inner: &ManagerInner,
    guard: &mut SharedState,
    mapping: &mut MappingRecord,
    event: Option<ObservedEvent>,
    err: &ManagerError,
) -> Result<(), ManagerError> {
    if let Some(event) = event {
        mapping.state = apply_event(mapping.state, event).state;
    }
    let now = inner.clock.now_rfc3339();
    mapping.updated_at = now.clone();
    mapping.last_run_summary_text = Some(err.cause());
    guard.store.update_mapping_runtime(mapping)?;
    inner
        .event_bus
        .publish_event(ManagerEvent::PreflightFailed {
            mapping_id: mapping.id.clone(),
            cause: err.cause(),
            next_step: err.next_step(),
            timestamp: now,
        });
    Ok(())
}

type EndpointCheckError = (Option<ObservedEvent>, Box<ManagerError>);

fn drive_endpoint_error(error: AccountError) -> EndpointCheckError {
    let event = match &error {
        AccountError::AuthenticationRequired { .. } => Some(ObservedEvent::AuthenticationFailed),
        AccountError::FolderNotFound { .. }
        | AccountError::FolderIdMismatch { .. }
        | AccountError::AmbiguousDuplicate { .. } => Some(ObservedEvent::EndpointChanged),
        _ => None,
    };
    (event, Box::new(drive_error(error)))
}

fn verify_drive_generation(
    drive: &LockedDrive<'_>,
    mapping: &MappingRecord,
    generation: &GenerationRecord,
) -> Result<(), EndpointCheckError> {
    let endpoint_changed = |error| (Some(ObservedEvent::EndpointChanged), Box::new(error));
    drive_mapping_configuration_matches(drive, mapping, &generation.config.remote_root_folder_id)
        .map_err(endpoint_changed)?;
    if generation.config.path2 != mapping.remote_endpoint_value
        || generation.config.selected_folder_id != mapping.collection.folder_id
    {
        return Err(endpoint_changed(ManagerError::PreflightRejected {
            cause: "The cloud folder does not match the saved sync generation.".into(),
            next_step: "Review the Drive collection selected for this mapping.".into(),
        }));
    }
    drive_recovery_directory(drive, mapping, false, &|| false)
        .map_err(|error| (None, Box::new(error)))?;
    drive
        .verify_collection(&mapping.collection)
        .map_err(drive_endpoint_error)?;
    verify_drive_marker(drive, &mapping.collection).map_err(|error| {
        let event = matches!(error, ManagerError::AccessMarkerConflict { .. })
            .then_some(ObservedEvent::EndpointChanged);
        (event, Box::new(error))
    })?;
    Ok(())
}

fn verify_endpoints_for_generation(
    inner: &ManagerInner,
    mapping: &MappingRecord,
    generation: &GenerationRecord,
    observation: &BoundVolume,
) -> Result<(), EndpointCheckError> {
    let volume = observation
        .check(&mapping.id, &mapping.local_binding)
        .map_err(|err| {
            let event = match observation {
                BoundVolume::Moved(_) | BoundVolume::Legacy => ObservedEvent::EndpointChanged,
                _ => ObservedEvent::VolumeLost,
            };
            (Some(event), Box::new(err))
        })?;
    verify_generation_paths(&inner.state_dir, mapping, generation)
        .map_err(|err| (Some(ObservedEvent::EndpointChanged), Box::new(err)))?;
    require_free_space(&volume.display_name, volume.space.free_bytes)
        .and_then(|()| writable_directory_space(&generation.config.path1))
        .and_then(|_| {
            verify_recovery_root(inner.volume_probe.as_ref(), &mapping.local_recovery_root)
        })
        .and_then(|()| {
            if mapping.remote_endpoint_kind == "local_dir" {
                writable_directory_space(Path::new(&generation.config.path2))?;
                verify_recovery_root(
                    inner.volume_probe.as_ref(),
                    Path::new(&mapping.cloud_recovery_path),
                )?;
            }
            Ok(())
        })
        .map_err(|err| (None, Box::new(err)))
}

fn verify_generation_paths(
    state_dir: &Path,
    mapping: &MappingRecord,
    generation: &GenerationRecord,
) -> Result<(), ManagerError> {
    if mapping.local_binding != generation.config.local_binding {
        return Err(ManagerError::PreflightRejected {
            cause: "The disk binding does not match the saved sync setup.".to_owned(),
            next_step: "Review the local folder selected for this mapping.".to_owned(),
        });
    }
    let local_root = mapping.local_binding.resolved_path();
    verify_marked_root(&local_root, &generation.config.path1, &mapping.collection)?;
    verify_binding_marker(state_dir, generation)?;
    let remote = RemoteEndpoint::from_db_cols(
        &mapping.remote_endpoint_kind,
        &mapping.remote_endpoint_value,
    )?;
    if let RemoteEndpoint::LocalDir(remote_root) = remote {
        verify_marked_root(
            &remote_root,
            Path::new(&generation.config.path2),
            &mapping.collection,
        )?;
    }
    Ok(())
}

fn verify_marked_root(
    root: &Path,
    expected: &Path,
    collection: &CloudCollection,
) -> Result<(), ManagerError> {
    let valid = fs::symlink_metadata(root).is_ok_and(|meta| meta.is_dir())
        && fs::canonicalize(root).is_ok_and(|path| path == expected);
    if !valid {
        return Err(ManagerError::PreflightRejected {
            cause: format!(
                "Folder `{}` is missing or has changed since setup.",
                root.display()
            ),
            next_step: "Review the folder selected for this mapping.".to_owned(),
        });
    }
    verify_endpoint_access_marker(expected, collection)
}

fn writable_directory_space(path: &Path) -> Result<DiskSpaceInfo, ManagerError> {
    let is_directory = fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir())
        && fs::canonicalize(path).is_ok_and(|canonical| canonical == path);
    if is_directory && let Ok((space, true)) = space_and_writability(path) {
        return Ok(space);
    }
    Err(ManagerError::PreflightRejected {
        cause: format!(
            "Folder `{}` is unavailable or cannot be written to.",
            path.display()
        ),
        next_step: "Confirm the folder is mounted, accessible, and writable.".to_owned(),
    })
}

fn verify_recovery_root(probe: &dyn VolumeProbe, path: &Path) -> Result<(), ManagerError> {
    writable_directory_space(path).map_err(|err| ManagerError::PreflightRejected {
        cause: format!("Recovery destination is unavailable. {}", err.cause()),
        next_step: err.next_step(),
    })?;
    let space = probe
        .probe(path)
        .map_err(|err| ManagerError::PreflightRejected {
            cause: format!(
                "Can’t check recovery destination `{}`: {err}",
                path.display()
            ),
            next_step: "Confirm that the recovery disk is connected and accessible.".to_owned(),
        })?
        .space;
    require_free_space(
        &format!("the recovery destination at `{}`", path.display()),
        space.free_bytes,
    )
}

fn require_free_space(label: &str, free_bytes: u64) -> Result<(), ManagerError> {
    if free_bytes >= MIN_FREE_BYTES {
        return Ok(());
    }
    Err(ManagerError::PreflightRejected {
        cause: format!("FolderSync requires at least 100 MiB free on {label}."),
        next_step: "Free up disk space and try again.".to_owned(),
    })
}

fn binding_marker_path(state_dir: &Path, generation: &GenerationRecord) -> PathBuf {
    state_dir
        .join("mappings")
        .join(generation.mapping_id.as_str())
        .join(format!("generation-{}", generation.generation_number))
        .join("volume-binding")
}

fn binding_marker_contents(generation: &GenerationRecord) -> String {
    format!(
        "{}\n{}\n",
        generation.id,
        generation.config.fingerprint().to_hex()
    )
}

fn write_binding_marker(
    state_dir: &Path,
    generation: &GenerationRecord,
) -> Result<(), ManagerError> {
    let path = binding_marker_path(state_dir, generation);
    let path = reject_symlinks_under_root("volume binding marker", state_dir, &path)?;
    let write = || -> io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?;
        file.write_all(binding_marker_contents(generation).as_bytes())?;
        file.sync_all()
    };
    match write() {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            verify_binding_marker(state_dir, generation)
        }
        Err(err) => Err(ManagerError::Io {
            cause: format!("Could not create the disk binding marker: {err}."),
            next_step: "Check FolderSync's local state directory before retrying setup.".to_owned(),
        }),
    }
}

fn verify_binding_marker(
    state_dir: &Path,
    generation: &GenerationRecord,
) -> Result<(), ManagerError> {
    let path = binding_marker_path(state_dir, generation);
    let path = reject_symlinks_under_root("volume binding marker", state_dir, &path)?;
    let expected = binding_marker_contents(generation);
    if fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_file())
        && let Ok(file) = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
    {
        let mut contents = Vec::new();
        if file
            .take(expected.len() as u64 + 1)
            .read_to_end(&mut contents)
            .is_ok()
            && contents == expected.as_bytes()
        {
            return Ok(());
        }
    }
    Err(ManagerError::PreflightRejected {
        cause: "The saved disk binding marker is missing or has changed.".to_owned(),
        next_step: "Review the mapping's saved disk and setup before syncing.".to_owned(),
    })
}

/// Probes whether a recorded orphan PID is still the exact same process:
/// - `Some(true)`: positively matches PID + start time + executable
/// - `Some(false)`: positively exited (`NotFound`) or PID reused (start time or conclusively different exe)
/// - `None`: inconclusive (permission denied, missing metadata, `(deleted)` exe, or canonicalization failure)
fn probe_orphan_liveness(
    inspector: &dyn ProcessInspector,
    pid: u32,
    run: Option<&RunRecord>,
) -> Option<bool> {
    match inspector.inspect(pid) {
        ProcessProbeResult::NotFound => Some(false),
        ProcessProbeResult::Unavailable(_) => None,
        ProcessProbeResult::Live(live) => {
            let run = run?;
            let start_matches =
                run.child_start_time.is_some() && live.start_time == run.child_start_time;
            let start_differs = run.child_start_time.is_some()
                && live.start_time.is_some()
                && live.start_time != run.child_start_time;
            let exe_cmp = compare_executables(&run.child_executable, &live.executable);

            if start_matches && exe_cmp == ExeComparison::Match {
                Some(true)
            } else if start_differs || exe_cmp == ExeComparison::ClearlyDifferent {
                Some(false)
            } else {
                None
            }
        }
    }
}

/// Returns any open `orphan_ambiguous` or live/inconclusive `orphan_running` /
/// `shutdown_timeout` case across ALL mappings that must block the global worker (§3, §14, HIGH #6).
fn find_blocking_global_orphan(
    store: &SqliteStateStore,
    inspector: &dyn ProcessInspector,
) -> Result<Option<RecoveryCaseRecord>, ManagerError> {
    for case in store.list_all_open_recovery_cases()? {
        match case.reason_kind.as_str() {
            "orphan_ambiguous" => return Ok(Some(case)),
            "orphan_running" | "shutdown_timeout" => {
                let run = case
                    .run_id
                    .as_ref()
                    .map(|rid| store.get_run(rid))
                    .transpose()?
                    .flatten();
                let alive = case
                    .orphan_pid
                    .and_then(|pid| probe_orphan_liveness(inspector, pid, run.as_ref()));
                // Block when positively alive (`Some(true)`), missing PID, or inconclusive (`None`).
                if alive != Some(false) {
                    return Ok(Some(case));
                }
            }
            _ => {}
        }
    }
    Ok(None)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExeComparison {
    Match,
    ClearlyDifferent,
    Inconclusive,
}

/// Compares recorded and live executable paths without ever treating
/// uncanonicalizable paths or Linux ` (deleted)` suffixes as proof of PID reuse (HIGH #6).
fn compare_executables(recorded: &Option<PathBuf>, live: &Option<PathBuf>) -> ExeComparison {
    let (Some(rec), Some(act)) = (recorded, live) else {
        return ExeComparison::Inconclusive;
    };
    if act.to_string_lossy().ends_with(" (deleted)")
        || rec.to_string_lossy().ends_with(" (deleted)")
    {
        return ExeComparison::Inconclusive;
    }
    if rec == act {
        return ExeComparison::Match;
    }
    match (fs::canonicalize(rec), fs::canonicalize(act)) {
        (Ok(a), Ok(b)) if a == b => ExeComparison::Match,
        (Ok(_), Ok(_)) => ExeComparison::ClearlyDifferent,
        _ => ExeComparison::Inconclusive,
    }
}

fn write_run_outcome_json(state_dir: &Path, run: &RunRecord) {
    let outcome_doc = serde_json::json!({
        "run_id": run.id.as_str(),
        "status": run.status,
        "outcome_kind": run.outcome_kind,
        "outcome_detail": run.outcome_detail,
        "last_verified_phase": run.last_verified_phase,
        "exit_code": run.exit_code,
        "exit_signal": run.exit_signal,
        "conflict_paths": run.conflict_paths,
        "files_checked": run.files_checked,
        "files_transferred": run.files_transferred,
        "bytes_transferred": run.bytes_transferred,
        "finished_at": run.finished_at,
    });
    if let Ok(bytes) = serde_json::to_vec_pretty(&outcome_doc)
        && let Err(err) = write_private_file_under(
            "outcome_json",
            state_dir,
            &run.run_dir.join("outcome.json"),
            &bytes,
        )
    {
        eprintln!(
            "Warning: failed to write outcome.json for run `{}`: {err}",
            run.id
        );
    }
}

/// Formats one captured output line as a guaranteed-valid single-line JSON
/// object for `output.ndjson` (HIGH #8).
fn format_ndjson_line(line: &OutputLine, timestamp: &str) -> String {
    let trimmed = line.text.trim();
    if !line.truncated
        && trimmed.starts_with('{')
        && let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed)
        && val.is_object()
        && let Ok(compact) = serde_json::to_string(&val)
    {
        return compact;
    }

    let stream = match line.stream {
        StreamKind::Stdout => "stdout",
        StreamKind::Stderr => "stderr",
    };
    serde_json::to_string(&serde_json::json!({
        "stream": stream,
        "raw": line.text,
        "truncated": line.truncated,
        "timestamp": timestamp,
    }))
    .unwrap_or_else(|_| r#"{"raw":"<serialization-error>"}"#.to_string())
}

fn reconcile_startup_state(
    state_dir: &Path,
    store: &mut SqliteStateStore,
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
            None => {
                // Investigate surviving processes referencing this exact workdir (§14).
                let proc_note = match inspector.find_processes_referencing_workdir(&run.workdir_path)
                {
                    Some(pids) if !pids.is_empty() => format!(
                        " Surviving process(es) referencing this work directory were observed: PID(s) {:?}.",
                        pids
                    ),
                    Some(_) => {
                        " No surviving process referencing this work directory was observed, but missing PID prevents proof of ownership."
                            .to_string()
                    }
                    None => " Process-table workdir scan was unavailable.".to_string(),
                };
                (
                    "reconciled_ambiguous".to_string(),
                    "orphan_ambiguous".to_string(),
                    format!(
                        "FolderSync exited after recording run `{}` and before recording the child process PID; process ownership is ambiguous.{lock_note}{proc_note}",
                        run.id
                    ),
                    "Inspect active processes and the preserved bisync work directory before clearing recovery."
                        .to_string(),
                    None,
                )
            }
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
                    let start_matches =
                        run.child_start_time.is_some() && live.start_time == run.child_start_time;
                    let start_differs = run.child_start_time.is_some()
                        && live.start_time.is_some()
                        && live.start_time != run.child_start_time;
                    let exe_cmp = compare_executables(&run.child_executable, &live.executable);

                    if start_matches && exe_cmp == ExeComparison::Match {
                        (
                            "reconciled_orphan_running".to_string(),
                            "orphan_running".to_string(),
                            format!(
                                "Orphaned rclone process (PID {pid}) from run `{}` is still running.{lock_note}",
                                run.id
                            ),
                            "Wait for or stop the exact orphaned process before starting another sync job."
                                .to_string(),
                            Some(pid),
                        )
                    } else if start_differs || exe_cmp == ExeComparison::ClearlyDifferent {
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

        let updated_mapping = if let Some(mut mapping) = store.get_mapping(&run.mapping_id)? {
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
            reconciled_mappings.insert(mapping.id.clone());
            Some(mapping)
        } else {
            None
        };

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

        // Write outcome.json BEFORE committing terminal DB state (HIGH #8), then commit atomically (HIGH #7).
        write_run_outcome_json(state_dir, &run);
        store.commit_run_terminal(&run, updated_mapping.as_ref(), Some(&case))?;

        event_bus.publish_event(ManagerEvent::RecoveryOpened {
            mapping_id: run.mapping_id,
            case_id: case.id,
            reason_kind,
            cause,
            timestamp: now.clone(),
        });
    }

    // Repair any missing outcome.json on already-committed terminal run rows (HIGH #8).
    for run in store.list_terminal_runs()? {
        let outcome_path = run.run_dir.join("outcome.json");
        if matches!(
            fs::symlink_metadata(&outcome_path),
            Err(ref err) if err.kind() == std::io::ErrorKind::NotFound
        ) {
            write_run_outcome_json(state_dir, &run);
        }
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
                store.commit_mapping_and_recovery(&mapping, Some(&case))?;
            }
            Execution::Idle | Execution::Interrupted => {}
        }
    }

    Ok(())
}

fn build_snapshot(guard: &SharedState, clock: &dyn Clock) -> Result<StatusSnapshot, ManagerError> {
    let store = &guard.store;
    let scheduling_paused = guard.scheduling_paused;
    let mappings = store.list_mappings()?;
    let mut views = Vec::with_capacity(mappings.len());

    for m in mappings {
        let fingerprint = m
            .active_generation_id
            .as_ref()
            .map(|gid| store.get_generation(gid))
            .transpose()?
            .flatten()
            .map(|generation| generation.fingerprint);
        let open_cases = store.list_open_recovery_cases(&m.id)?;
        // Bounded O(1) lookup (`LIMIT 1`) — never reloads all historical runs on snapshot rebuild.
        let latest_run = store.get_latest_run_for_mapping(&m.id)?;
        let unreviewed_conflict_paths = match &m.unreviewed_conflict_run_id {
            Some(rid) => {
                if latest_run.as_ref().is_some_and(|r| &r.id == rid) {
                    latest_run
                        .as_ref()
                        .map(|r| r.conflict_paths.clone())
                        .unwrap_or_default()
                } else {
                    store
                        .get_run(rid)?
                        .map(|r| r.conflict_paths)
                        .unwrap_or_default()
                }
            }
            None => Vec::new(),
        };

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
        let observed_volume = guard.volumes.get(&m.id);
        let mounted = observed_volume.and_then(BoundVolume::mounted);
        let volume_name = mounted
            .map(|volume| volume.display_name.clone())
            .unwrap_or_else(|| m.local_binding.volume_display_name());
        let volume_space = mounted.map(|volume| volume.space);
        let volume_filesystem = mounted.map(|volume| volume.filesystem.clone());
        let volume_kind = m.local_binding.volume.kind;
        let volume_stable_id = m.local_binding.volume.stable_id.clone();
        let mut text = status_text(
            &m.state,
            &display_name,
            &volume_name,
            last_run_summary.as_ref(),
        );
        if !matches!(m.state.execution, Execution::Running | Execution::Stopping)
            && let Some(observed) = observed_volume
            && matches!(
                (m.state.readiness(), observed),
                (
                    Readiness::VolumeUnavailable,
                    BoundVolume::WrongDisk(_) | BoundVolume::Inaccessible(_)
                ) | (
                    Readiness::EndpointChanged,
                    BoundVolume::Moved(_) | BoundVolume::Legacy
                )
            )
        {
            text = observed.message(&m.local_binding);
            if m.state.intent == Intent::Paused {
                text.push_str(" (paused)");
            }
        }
        // Never promise automatic retry while scheduling is disabled (HIGH #11).
        if (!m.scheduling_enabled || scheduling_paused) && text == "Last run failed — will retry"
        {
            text = "Last run failed — retry manually".to_string();
        }
        let actions = available_actions(&m.state);

        let counters = MappingCounters {
            files_checked: latest_run.as_ref().map(|r| r.files_checked).unwrap_or(0),
            files_transferred: latest_run
                .as_ref()
                .map(|r| r.files_transferred)
                .unwrap_or(0),
            bytes_transferred: latest_run
                .as_ref()
                .map(|r| r.bytes_transferred)
                .unwrap_or(0),
            conflict_count: latest_run.as_ref().map(|r| r.conflict_count).unwrap_or(0),
            open_recovery_cases: open_cases.len() as u64,
        };

        let schedule = guard.schedules.get(&m.id).filter(|schedule| {
            !scheduling_paused
                && m.scheduling_enabled
                && m.state.readiness() == Readiness::Ready
                && m.state.intent == Intent::Enabled
                && m.state.execution == Execution::Idle
                && m.active_generation_id.as_ref() == Some(&schedule.generation)
                && !guard.storage_blocked_mappings.contains(&m.id)
        });
        let retry_at = schedule.and_then(|schedule| schedule.retry_at);
        let next_poll_at = schedule
            .filter(|_| retry_at.is_none())
            .map(|schedule| schedule.next_poll(m.schedule_interval_secs));

        views.push(MappingStatusSnapshot {
            id: m.id,
            display_name,
            account_id: m.account_id,
            collection_display_path: m.collection.display_path,
            collection_folder_id: m.collection.folder_id,
            local_root: m.local_binding.resolved_path(),
            volume_name,
            volume_kind,
            volume_stable_id,
            volume_space,
            volume_filesystem,
            readiness: m.state.readiness(),
            execution: m.state.execution,
            intent: m.state.intent,
            scheduling_enabled: m.scheduling_enabled,
            schedule_interval_secs: m.schedule_interval_secs,
            next_poll_at,
            retry_at,
            active_generation_id: m.active_generation_id,
            active_generation_fingerprint: fingerprint,
            last_run_id: m.last_run_id,
            last_run_outcome: m.last_run_outcome,
            last_run_completed_at: m.last_run_completed_at,
            last_run_summary: m.last_run_summary_text,
            unreviewed_conflict_run_id: m.unreviewed_conflict_run_id,
            unreviewed_conflict_paths,
            exclusions: m.exclusions,
            counters,
            available_actions: actions,
            status_text: text,
            local_recovery_root: m.local_recovery_root,
            cloud_recovery_path: m.cloud_recovery_path,
        });
    }

    Ok(StatusSnapshot {
        sequence: guard.snapshot_seq,
        generated_at: clock.now_rfc3339(),
        active_job_mapping_id: guard.active_run.as_ref().map(|run| run.mapping_id.clone()),
        queue_order: guard
            .job_queue
            .iter()
            .map(|job| job.mapping_id.clone())
            .collect(),
        scheduling_paused,
        mappings: views,
        folder_sizes: guard.folder_sizes.values().cloned().collect(),
    })
}

/// Rebuilds and publishes the immutable `StatusSnapshot`, treating Finder
/// `status.json` write errors as non-fatal logged warnings (HIGH #9).
fn publish_updated_snapshot(
    inner: &ManagerInner,
    guard: &mut SharedState,
) -> Result<Arc<StatusSnapshot>, ManagerError> {
    publish_snapshot(inner, guard, true)
}

fn publish_snapshot(
    inner: &ManagerInner,
    guard: &mut SharedState,
    update_finder: bool,
) -> Result<Arc<StatusSnapshot>, ManagerError> {
    guard.snapshot_seq += 1;
    guard.last_space_snapshot = inner.clock.now_utc();
    let snap = Arc::new(build_snapshot(guard, inner.clock.as_ref())?);
    guard.latest_snapshot = Arc::clone(&snap);
    inner.event_bus.publish_snapshot(Arc::clone(&snap));
    inner
        .event_bus
        .publish_event(ManagerEvent::SnapshotUpdated {
            sequence: snap.sequence,
            timestamp: snap.generated_at.clone(),
        });
    if update_finder && let Err(err) = inner.finder_writer.write_snapshot(&snap) {
        eprintln!("Warning: failed to publish Finder status.json: {err}");
    }
    Ok(snap)
}

fn drive_mapping_paths(
    drive: &LockedDrive<'_>,
    collection: &CloudCollection,
) -> (RemoteEndpoint, String) {
    let remote = drive.account.remote_name.clone();
    let recovery = format!("{remote}:Recovery/{}", collection.folder_id);
    (
        RemoteEndpoint::DriveRemote {
            remote,
            path: collection.display_path.clone(),
        },
        recovery,
    )
}

fn drive_mapping_configuration_matches(
    drive: &LockedDrive<'_>,
    mapping: &MappingRecord,
    frozen_root: &str,
) -> Result<(), ManagerError> {
    let (endpoint, recovery) = drive_mapping_paths(drive, &mapping.collection);
    let (kind, value) = endpoint.to_db_cols();
    if drive.account.remote_root_folder_id != frozen_root
        || mapping.remote_endpoint_kind != kind
        || mapping.remote_endpoint_value != value
        || mapping.cloud_recovery_path != recovery
        || mapping.collection.ancestor_ids.is_empty()
    {
        return Err(ManagerError::PreflightRejected {
            cause: "The saved Drive account, collection or recovery path has changed.".into(),
            next_step: "Review the account and collection selected for this mapping.".into(),
        });
    }
    Ok(())
}

fn drive_recovery_directory(
    drive: &LockedDrive<'_>,
    mapping: &MappingRecord,
    create: bool,
    is_cancelled: &dyn Fn() -> bool,
) -> Result<String, ManagerError> {
    validate_drive_folder_id(&mapping.collection.folder_id).map_err(drive_error)?;
    validate_safe_path_id("device_id", mapping.device_id.as_str())?;
    let mut parent = String::new();
    for name in [
        "Recovery",
        mapping.collection.folder_id.as_str(),
        mapping.device_id.as_str(),
    ] {
        let path = if parent.is_empty() {
            name.to_owned()
        } else {
            format!("{parent}/{name}")
        };
        let inspect = || -> Result<bool, ManagerError> {
            let entries = drive
                .request
                .list_entries(&parent, false, &drive.runner)
                .map_err(|err| drive_error(err.into()))?;
            let mut matches = entries
                .iter()
                .filter(|entry| entry.name.eq_ignore_ascii_case(name));
            match (matches.next(), matches.next()) {
                (None, _) => Ok(false),
                (Some(entry), None)
                    if entry.name == name
                        && entry.path == name
                        && entry.is_dir
                        && entry.id.as_ref().is_none_or(|id| {
                            id != &mapping.collection.folder_id
                                && !mapping.collection.ancestor_ids.contains(id)
                        }) =>
                {
                    Ok(true)
                }
                _ => Err(ManagerError::PreflightRejected {
                    cause: format!(
                        "The Drive recovery folder `{path}` is ambiguous or overlaps a synced folder."
                    ),
                    next_step: "Review the Recovery folder under the shared parent.".into(),
                }),
            }
        };
        let mut present = inspect()?;
        if !present && create && !is_cancelled() {
            drive
                .request
                .mkdir(&path, &drive.runner)
                .map_err(|err| drive_error(err.into()))?;
            present = inspect()?;
        }
        if !present {
            return Err(ManagerError::PreflightRejected {
                cause: format!("The Drive recovery folder `{path}` is unavailable."),
                next_step: "Check access to the Recovery folder under the shared parent.".into(),
            });
        }
        parent = path;
    }
    Ok(parent)
}

fn validate_mapping_collections(
    existing: &[MappingRecord],
    account_id: &AccountId,
    collection: &CloudCollection,
    device_id: &DeviceId,
    drive: bool,
) -> Result<(), ManagerError> {
    let candidate = DeviceCloudCollection {
        device_id: device_id.clone(),
        collection: collection.clone(),
    };
    let mut account_collections = vec![candidate.clone()];
    for mapping in existing {
        let assignment = DeviceCloudCollection {
            device_id: mapping.device_id.clone(),
            collection: mapping.collection.clone(),
        };
        if &mapping.account_id == account_id {
            account_collections.push(assignment);
        } else if drive && mapping.remote_endpoint_kind == "drive_remote" {
            validate_cloud_collection_identity_pair(&candidate, &assignment)?;
        }
    }
    validate_cloud_collections(&account_collections).map_err(ManagerError::ValidationList)
}

struct InitPreflightOutput {
    direction: InitPreference,
    gen_record: GenerationRecord,
}

/// Runs initialization preflight (layout validation, recursive endpoint inventory,
/// engine metadata qualification, recovery dir creation, marker setup, and frozen
/// generation artifacts) strictly WITHOUT holding `inner.state` mutex.
///
/// Checks `is_cancelled()` before every mutating side effect (recovery dirs,
/// markers, workdir, `filters.txt`) so a cancelled attempt never writes after Stop (HIGH #5).
fn run_init_preflight_unlocked(
    inner: &ManagerInner,
    mapping: &MappingRecord,
    account: &AccountRecord,
    all_mappings: &[MappingRecord],
    gen_num: i64,
    expected_direction: Option<InitPreference>,
    is_cancelled: &dyn Fn() -> bool,
) -> Result<Option<InitPreflightOutput>, ManagerError> {
    if is_cancelled() {
        return Ok(None);
    }
    let verify_local_disk = || -> Result<(), ManagerError> {
        let observed = observe_binding(inner.volume_probe.as_ref(), &mapping.local_binding);
        let volume = observed.check(&mapping.id, &mapping.local_binding)?;
        require_free_space(&volume.display_name, volume.space.free_bytes)
    };
    verify_local_disk()?;
    let canonical_local = validate_local_binding(&mapping.local_binding, &StdPathResolver)?;
    validate_complete_layout(
        all_mappings,
        None,
        &inner.state_dir,
        inner.finder_writer.dir(),
    )?;

    let other_mappings: Vec<_> = all_mappings
        .iter()
        .filter(|other| other.id != mapping.id)
        .cloned()
        .collect();
    validate_mapping_collections(
        &other_mappings,
        &mapping.account_id,
        &mapping.collection,
        &mapping.device_id,
        mapping.remote_endpoint_kind == "drive_remote",
    )?;

    let effective_filters: BTreeSet<String> = mapping.exclusions.iter().cloned().collect();

    let mut gen_num = gen_num;
    let mapping_dir = inner.state_dir.join("mappings").join(mapping.id.as_str());
    let raw_gen_dir = loop {
        let candidate = mapping_dir.join(format!("generation-{gen_num}"));
        reject_symlinks_under_root("generation", &inner.state_dir, &candidate)?;
        match fs::symlink_metadata(&candidate) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => break candidate,
            Ok(meta) if meta.is_dir() => {
                for leaf in ["bisync", "filters.txt", "volume-binding"] {
                    reject_symlinks_under_root(leaf, &inner.state_dir, &candidate.join(leaf))?;
                }
                gen_num += 1;
            }
            other => {
                return Err(ManagerError::Io {
                    cause: format!(
                        "Unable to use setup generation path `{}`: {other:?}",
                        candidate.display()
                    ),
                    next_step: "Inspect the private state directory before trying again."
                        .to_owned(),
                });
            }
        }
    };
    let gen_id = GenerationId::new(format!("{}-gen-{gen_num}", mapping.id));
    let raw_workdir_path = raw_gen_dir.join("bisync");
    let raw_filters_file_path = raw_gen_dir.join("filters.txt");
    reject_symlinks_under_root("workdir_path", &inner.state_dir, &raw_workdir_path)?;
    reject_symlinks_under_root("filters_file", &inner.state_dir, &raw_filters_file_path)?;

    let remote_ep = RemoteEndpoint::from_db_cols(
        &mapping.remote_endpoint_kind,
        &mapping.remote_endpoint_value,
    )?;
    let (direction, engine_meta) = match &remote_ep {
        RemoteEndpoint::LocalDir(remote_root) => {
            let local_inv =
                inspect_local_endpoint(&canonical_local, &mapping.collection, &effective_filters)?;
            if is_cancelled() {
                return Ok(None);
            }
            let remote_inv =
                inspect_local_endpoint(remote_root, &mapping.collection, &effective_filters)?;
            let dir =
                decide_init_direction(&mapping.id, &local_inv, &remote_inv, expected_direction)?;

            // Qualify engine metadata BEFORE mutating endpoints (creating recovery dirs/markers).
            let engine_meta = inner.launcher.engine_metadata()?;
            if is_cancelled() {
                return Ok(None);
            }

            verify_local_disk()?;
            writable_directory_space(&canonical_local)?;
            writable_directory_space(remote_root)?;
            fs::create_dir_all(&mapping.local_recovery_root).map_err(|err| ManagerError::Io {
                cause: format!(
                    "Failed to create local recovery directory `{}`: {err}.",
                    mapping.local_recovery_root.display()
                ),
                next_step: "Verify permissions on the recovery drive and retry.".to_string(),
            })?;
            let remote_recovery_dir = PathBuf::from(&mapping.cloud_recovery_path);
            fs::create_dir_all(&remote_recovery_dir).map_err(|err| ManagerError::Io {
                cause: format!(
                    "Failed to create remote recovery directory `{}`: {err}.",
                    remote_recovery_dir.display()
                ),
                next_step: "Verify permissions on the remote recovery path and retry.".to_string(),
            })?;
            if is_cancelled() {
                return Ok(None);
            }
            verify_local_disk()?;
            verify_recovery_root(inner.volume_probe.as_ref(), &mapping.local_recovery_root)?;
            verify_recovery_root(inner.volume_probe.as_ref(), &remote_recovery_dir)?;
            establish_local_pair_markers(
                &canonical_local,
                remote_root,
                &local_inv,
                &remote_inv,
                &mapping.collection,
            )?;
            (dir, engine_meta)
        }
        RemoteEndpoint::DriveRemote { .. } => {
            let control = ProcessControl::with_timeout(DRIVE_COMMAND_TIMEOUT);
            let drive = inner
                .accounts
                .locked_drive(&account.id, &control)
                .map_err(drive_error)?;
            drive_mapping_configuration_matches(&drive, mapping, &account.remote_root_folder_id)?;
            drive
                .verify_collection(&mapping.collection)
                .map_err(drive_error)?;
            let local_inv =
                inspect_local_endpoint(&canonical_local, &mapping.collection, &effective_filters)?;
            let (remote, marker) =
                inspect_drive_endpoint(&drive, &mapping.collection, &effective_filters)?;
            if let Some(cause) = remote.name_findings.first() {
                return Err(ManagerError::PreflightRejected {
                    cause: cause.clone(),
                    next_step: remote.first_name_finding_next_step.unwrap_or_default(),
                });
            }
            let remote_inv = EndpointInventory {
                root: remote.root,
                classification: remote.classification,
                item_count: remote.all_items_count,
                has_valid_marker: remote.has_valid_marker,
            };
            let direction =
                decide_init_direction(&mapping.id, &local_inv, &remote_inv, expected_direction)?;
            let engine_meta = inner.launcher.engine_metadata()?;
            if is_cancelled() {
                return Ok(None);
            }

            verify_local_disk()?;
            writable_directory_space(&canonical_local)?;
            fs::create_dir_all(&mapping.local_recovery_root).map_err(|err| ManagerError::Io {
                cause: format!("Failed to create local recovery directory: {err}"),
                next_step: "Verify permissions on the recovery drive and retry.".into(),
            })?;
            verify_recovery_root(inner.volume_probe.as_ref(), &mapping.local_recovery_root)?;
            let recovery = drive_recovery_directory(&drive, mapping, true, is_cancelled)?;
            if is_cancelled() {
                return Ok(None);
            }
            // Check path-addressed identity again before touching the endpoints.
            drive
                .verify_collection(&mapping.collection)
                .map_err(drive_error)?;
            establish_drive_marker(
                &drive,
                &mapping.collection,
                &local_inv,
                marker,
                &recovery,
                is_cancelled,
            )?;
            (direction, engine_meta)
        }
        RemoteEndpoint::OtherRemote { .. } => {
            return Err(ManagerError::PreflightRejected {
                cause: "This remote does not support first sync.".into(),
                next_step: "Choose a supported Drive account.".into(),
            });
        }
    };

    if is_cancelled() {
        return Ok(None);
    }

    ensure_private_dir_under("mapping", &inner.state_dir, &mapping_dir)?;
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&raw_gen_dir)
        .map_err(|err| ManagerError::Io {
            cause: format!(
                "Failed to reserve setup generation `{}`: {err}",
                raw_gen_dir.display()
            ),
            next_step: "Check the FolderSync state directory and try again.".to_owned(),
        })?;
    let workdir_path =
        ensure_private_dir_under("workdir_path", &inner.state_dir, &raw_workdir_path)?;
    let filters_file_path =
        reject_symlinks_under_root("filters_file", &inner.state_dir, &raw_filters_file_path)?;
    let write_filters = || -> io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&filters_file_path)?;
        file.write_all(render_filters_file(&effective_filters).as_bytes())?;
        file.sync_all()
    };
    write_filters().map_err(|err| ManagerError::Io {
        cause: format!("Failed to freeze setup filters: {err}"),
        next_step: "Check the FolderSync state directory and try again.".to_owned(),
    })?;

    let config_gen = ConfigGeneration {
        path1: canonical_local,
        path2: remote_ep.path2_string(),
        remote_root_folder_id: account.remote_root_folder_id.clone(),
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
    let now = inner.clock.now_rfc3339();
    let gen_record = GenerationRecord {
        id: gen_id,
        mapping_id: mapping.id.clone(),
        generation_number: gen_num,
        fingerprint,
        config: config_gen,
        initial_direction: Some(initial_direction),
        workdir_path,
        filters_file_path,
        created_at: now,
    };

    Ok(Some(InitPreflightOutput {
        direction,
        gen_record,
    }))
}

fn collect_recovery_files_readonly(
    root: &Path,
    side: RecoveryCopySide,
    out: &mut Vec<RecoveryCopyEntry>,
) -> Result<(), ManagerError> {
    fn walk(
        base: &Path,
        dir: &Path,
        side: RecoveryCopySide,
        out: &mut Vec<RecoveryCopyEntry>,
    ) -> Result<(), ManagerError> {
        let entries = fs::read_dir(dir).map_err(|err| ManagerError::Io {
            cause: format!("Could not read recovery folder `{}`: {err}.", dir.display()),
            next_step: "Check permissions on the recovery folder and try again.".to_string(),
        })?;
        for entry in entries {
            let entry = entry.map_err(|err| ManagerError::Io {
                cause: format!(
                    "Could not read recovery folder entry in `{}`: {err}.",
                    dir.display()
                ),
                next_step: "Check permissions on the recovery folder and try again.".to_string(),
            })?;
            let path = entry.path();
            let meta = fs::symlink_metadata(&path).map_err(|err| ManagerError::Io {
                cause: format!(
                    "Could not inspect recovery copy `{}`: {err}.",
                    path.display()
                ),
                next_step: "Check permissions on the recovery folder and try again.".to_string(),
            })?;
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                walk(base, &path, side, out)?;
            } else if meta.is_file()
                && let Ok(rel) = path.strip_prefix(base)
            {
                out.push(RecoveryCopyEntry {
                    side,
                    relative_path: rel.to_string_lossy().replace('\\', "/"),
                    full_path: path,
                    size_bytes: meta.len(),
                });
            }
        }
        Ok(())
    }

    match fs::symlink_metadata(root) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.is_dir() {
                return Err(ManagerError::PreflightRejected {
                    cause: format!(
                        "Recovery folder `{}` is a symbolic link or not an ordinary directory.",
                        root.display()
                    ),
                    next_step: "Check the recovery folder on disk before copying files."
                        .to_string(),
                });
            }
            walk(root, root, side, out)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(ManagerError::Io {
            cause: format!(
                "Could not inspect recovery folder `{}`: {err}.",
                root.display()
            ),
            next_step: "Check permissions on the recovery folder and try again.".to_string(),
        }),
    }
}

struct InitPreflightClaim {
    attempt_id: u64,
    mapping: MappingRecord,
    account: AccountRecord,
    all_mappings: Vec<MappingRecord>,
    gen_num: i64,
    expected_direction: Option<InitPreference>,
}

enum DequeueOutcome {
    Shutdown,
    Skip,
    Prepared(Box<PreparedRun>),
    InitPreflight(Box<InitPreflightClaim>),
}

fn worker_loop(inner: Arc<ManagerInner>) {
    let mut last_heartbeat = Instant::now();
    loop {
        // Atomic dequeue -> eligibility recheck -> pre-spawn DB row -> active_run claim
        // all under ONE mutex acquisition (HIGH #5).
        let outcome = {
            let mut guard = inner.state.lock().unwrap();
            while !guard.shutdown
                && (guard.job_queue.is_empty() || guard.preflight_mapping.is_some())
            {
                if let Some(heartbeat) = inner.finder_heartbeat_interval {
                    let remaining = heartbeat.saturating_sub(last_heartbeat.elapsed());
                    guard = inner.worker_cv.wait_timeout(guard, remaining).unwrap().0;
                    if !guard.shutdown
                        && (guard.job_queue.is_empty() || guard.preflight_mapping.is_some())
                        && last_heartbeat.elapsed() >= heartbeat
                    {
                        // Refresh Finder status.json generated_at periodically so idle running
                        // apps never trip the 5-minute TTL (HIGH #12).
                        let mut refreshed = (*guard.latest_snapshot).clone();
                        refreshed.generated_at = inner.clock.now_rfc3339();
                        let refreshed = Arc::new(refreshed);
                        guard.latest_snapshot = Arc::clone(&refreshed);
                        if let Err(err) = inner.finder_writer.write_snapshot(&refreshed) {
                            eprintln!("Warning: failed Finder heartbeat status.json write: {err}");
                        }
                        last_heartbeat = Instant::now();
                    }
                } else {
                    guard = inner.worker_cv.wait(guard).unwrap();
                }
            }
            if guard.shutdown {
                DequeueOutcome::Shutdown
            } else if let Some(job) = guard.job_queue.pop_front() {
                if job.automatic && guard.scheduling_paused {
                    if let Err(err) = cancel_queued_automatic(&inner, &mut guard, &job.mapping_id) {
                        record_terminal_db_error_locked(&mut guard, &job.mapping_id, err);
                    }
                    inner.idle_cv.notify_all();
                    continue;
                }
                match &job.kind {
                    QueuedJobKind::Prepared {
                        generation_id,
                        mode,
                    } => {
                        match claim_and_prepare_locked(
                            &inner,
                            &mut guard,
                            &job.mapping_id,
                            generation_id,
                            *mode,
                        ) {
                            Ok(Some(prepared)) => {
                                guard.worker_busy = true;
                                DequeueOutcome::Prepared(Box::new(prepared))
                            }
                            Ok(None) => {
                                if guard.active_run.is_none() && guard.job_queue.is_empty() {
                                    inner.idle_cv.notify_all();
                                }
                                DequeueOutcome::Skip
                            }
                            Err(err) => {
                                record_prepare_failure_locked(
                                    &inner,
                                    &mut guard,
                                    &job.mapping_id,
                                    err,
                                );
                                if guard.active_run.is_none() && guard.job_queue.is_empty() {
                                    inner.idle_cv.notify_all();
                                }
                                DequeueOutcome::Skip
                            }
                        }
                    }
                    QueuedJobKind::InitializePreflight {
                        expected_direction,
                        attempt_id,
                    } => {
                        match claim_init_preflight_locked(
                            &inner,
                            &mut guard,
                            &job.mapping_id,
                            *expected_direction,
                            *attempt_id,
                        ) {
                            Ok(Some(claim)) => {
                                guard.worker_busy = true;
                                DequeueOutcome::InitPreflight(Box::new(claim))
                            }
                            Ok(None) => {
                                if guard.active_run.is_none() && guard.job_queue.is_empty() {
                                    inner.idle_cv.notify_all();
                                }
                                DequeueOutcome::Skip
                            }
                            Err(err) => {
                                record_init_preflight_failure_locked(
                                    &inner,
                                    &mut guard,
                                    &job.mapping_id,
                                    err,
                                );
                                if guard.active_run.is_none() && guard.job_queue.is_empty() {
                                    inner.idle_cv.notify_all();
                                }
                                DequeueOutcome::Skip
                            }
                        }
                    }
                }
            } else {
                DequeueOutcome::Skip
            }
        };

        match outcome {
            DequeueOutcome::Shutdown => break,
            DequeueOutcome::Skip => continue,
            DequeueOutcome::Prepared(prepared) => {
                execute_prepared_job(&inner, *prepared);
                let mut guard = inner.state.lock().unwrap();
                guard.worker_busy = false;
                if guard.active_run.is_none() && guard.job_queue.is_empty() {
                    inner.idle_cv.notify_all();
                }
            }
            DequeueOutcome::InitPreflight(claim) => {
                let InitPreflightClaim {
                    attempt_id,
                    mapping,
                    account,
                    all_mappings,
                    gen_num,
                    expected_direction,
                } = *claim;
                let mapping_id = mapping.id.clone();
                let is_cancelled = || {
                    let g = inner.state.lock().unwrap();
                    g.shutdown
                        || g.cancelled_init_attempts.contains(&attempt_id)
                        || g.active_init_attempt.as_ref() != Some(&(mapping_id.clone(), attempt_id))
                };
                // Run inventory + engine probe completely OUTSIDE the state mutex!
                let preflight_res = run_init_preflight_unlocked(
                    &inner,
                    &mapping,
                    &account,
                    &all_mappings,
                    gen_num,
                    expected_direction,
                    &is_cancelled,
                );
                let maybe_prepared = {
                    let mut guard = inner.state.lock().unwrap();
                    let still_active_attempt = !guard.shutdown
                        && !guard.cancelled_init_attempts.remove(&attempt_id)
                        && guard.active_init_attempt.as_ref()
                            == Some(&(mapping_id.clone(), attempt_id));
                    if guard.active_init_attempt.as_ref() == Some(&(mapping_id.clone(), attempt_id))
                    {
                        guard.active_init_attempt = None;
                    }
                    if !still_active_attempt {
                        // Cancelled attempt MUST NOT write generation or hijack a newer attempt (HIGH #5).
                        None
                    } else {
                        match preflight_res {
                            Ok(Some(out)) => {
                                let gen_id = out.gen_record.id.clone();
                                let mode = RunMode::Initialize(out.direction);
                                let save_generation =
                                    write_binding_marker(&inner.state_dir, &out.gen_record)
                                        .and_then(|()| {
                                            guard.store.insert_generation(&out.gen_record)
                                        });
                                if let Err(err) = save_generation {
                                    record_init_preflight_failure_locked(
                                        &inner,
                                        &mut guard,
                                        &mapping_id,
                                        err,
                                    );
                                    None
                                } else {
                                    match claim_and_prepare_locked(
                                        &inner,
                                        &mut guard,
                                        &mapping_id,
                                        &gen_id,
                                        mode,
                                    ) {
                                        Ok(opt) => opt,
                                        Err(err) => {
                                            record_prepare_failure_locked(
                                                &inner,
                                                &mut guard,
                                                &mapping_id,
                                                err,
                                            );
                                            None
                                        }
                                    }
                                }
                            }
                            Ok(None) => None,
                            Err(err) => {
                                record_init_preflight_failure_locked(
                                    &inner,
                                    &mut guard,
                                    &mapping_id,
                                    err,
                                );
                                None
                            }
                        }
                    }
                };
                if let Some(prepared) = maybe_prepared {
                    execute_prepared_job(&inner, prepared);
                }
                let mut guard = inner.state.lock().unwrap();
                guard.worker_busy = false;
                if guard.active_run.is_none() && guard.job_queue.is_empty() {
                    inner.idle_cv.notify_all();
                }
            }
        }
    }
}

fn claim_init_preflight_locked(
    inner: &ManagerInner,
    guard: &mut SharedState,
    mapping_id: &MappingId,
    expected_direction: Option<InitPreference>,
    attempt_id: u64,
) -> Result<Option<InitPreflightClaim>, ManagerError> {
    if guard.shutdown
        || guard.storage_blocked_mappings.contains(mapping_id)
        || guard.cancelled_init_attempts.remove(&attempt_id)
    {
        return Ok(None);
    }
    let Some(mut mapping) = guard.store.get_mapping(mapping_id)? else {
        return Ok(None);
    };
    if mapping.state.execution != Execution::Queued
        || mapping.state.readiness() != Readiness::NeedsInitialization
    {
        return Ok(None);
    }
    if find_blocking_global_orphan(&guard.store, inner.process_inspector.as_ref())?.is_some() {
        mapping.state.execution = Execution::Idle;
        mapping.updated_at = inner.clock.now_rfc3339();
        guard.store.update_mapping_runtime(&mapping)?;
        let _ = publish_updated_snapshot(inner, guard);
        return Ok(None);
    }
    let account = guard
        .store
        .get_account(&mapping.account_id)?
        .ok_or_else(|| ManagerError::NotFound {
            cause: format!("Account `{}` is missing.", mapping.account_id),
            next_step: "Reconnect the account before starting first sync.".to_string(),
        })?;
    let all_mappings = guard.store.list_mappings()?;
    let gen_num = guard.store.next_generation_number(&mapping.id)?;
    guard.active_init_attempt = Some((mapping.id.clone(), attempt_id));
    Ok(Some(InitPreflightClaim {
        attempt_id,
        mapping,
        account,
        all_mappings,
        gen_num,
        expected_direction,
    }))
}

fn record_init_preflight_failure_locked(
    inner: &ManagerInner,
    guard: &mut SharedState,
    mapping_id: &MappingId,
    err: ManagerError,
) {
    let now = inner.clock.now_rfc3339();
    if let Ok(Some(mut m)) = guard.store.get_mapping(mapping_id) {
        m.state.execution = Execution::Idle;
        m.last_run_summary_text = Some(err.cause());
        m.updated_at = now.clone();
        if let Err(db_err) = guard.store.update_mapping_runtime(&m) {
            record_terminal_db_error_locked(guard, mapping_id, db_err);
            return;
        }
        let _ = publish_updated_snapshot(inner, guard);
    }
    inner
        .event_bus
        .publish_event(ManagerEvent::PreflightFailed {
            mapping_id: mapping_id.clone(),
            cause: err.cause(),
            next_step: err.next_step(),
            timestamp: now,
        });
}

struct PreparedRun {
    run: RunRecord,
    mapping: MappingRecord,
    generation: GenerationRecord,
    plan: LaunchPlan,
    mode_label: String,
    is_init: bool,
    log_file: fs::File,
}

/// Rechecks durable eligibility and claims the active run under the state
/// mutex. The worker checks folders and volume identity outside the mutex.
fn claim_and_prepare_locked(
    inner: &ManagerInner,
    guard: &mut SharedState,
    mapping_id: &MappingId,
    generation_id: &GenerationId,
    mode: RunMode,
) -> Result<Option<PreparedRun>, ManagerError> {
    if guard.shutdown || guard.storage_blocked_mappings.contains(mapping_id) {
        return Ok(None);
    }

    let Some(mut mapping) = guard.store.get_mapping(mapping_id)? else {
        return Ok(None);
    };

    // Recheck eligibility right before claiming active slot (HIGH #5).
    if mapping.state.execution != Execution::Queued {
        return Ok(None);
    }
    let is_init = matches!(mode, RunMode::Initialize(_));
    if !is_init
        && (mapping.state.intent == Intent::Paused
            || mapping.state.readiness() != Readiness::Ready
            || mapping.active_generation_id.as_ref() != Some(generation_id))
    {
        mapping.state.execution = Execution::Idle;
        mapping.updated_at = inner.clock.now_rfc3339();
        guard.store.update_mapping_runtime(&mapping)?;
        let _ = publish_updated_snapshot(inner, guard);
        return Ok(None);
    }
    if is_init && mapping.state.readiness() != Readiness::NeedsInitialization {
        mapping.state.execution = Execution::Idle;
        mapping.updated_at = inner.clock.now_rfc3339();
        guard.store.update_mapping_runtime(&mapping)?;
        let _ = publish_updated_snapshot(inner, guard);
        return Ok(None);
    }

    // Re-check global orphan block right before launch (HIGH #6).
    if find_blocking_global_orphan(&guard.store, inner.process_inspector.as_ref())?.is_some() {
        mapping.state.execution = Execution::Idle;
        mapping.updated_at = inner.clock.now_rfc3339();
        guard.store.update_mapping_runtime(&mapping)?;
        let _ = publish_updated_snapshot(inner, guard);
        return Ok(None);
    }

    let generation =
        guard
            .store
            .get_generation(generation_id)?
            .ok_or_else(|| ManagerError::NotFound {
                cause: format!("Generation `{generation_id}` is missing."),
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
    validate_safe_path_id("run_id", run_id.as_str())?;
    validate_safe_path_id("mapping_id", mapping.id.as_str())?;
    validate_safe_path_id("device_id", mapping.device_id.as_str())?;

    let raw_run_dir = inner.state_dir.join("runs").join(run_id.as_str());
    let run_dir = ensure_private_dir_under("run_dir", &inner.state_dir, &raw_run_dir)?;
    let log_path = run_dir.join("output.ndjson");
    let log_file = open_private_write_file_under("log_path", &inner.state_dir, &log_path)?;

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
            let p = Path::new(&mapping.cloud_recovery_path)
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

    let mode_label = match mode {
        RunMode::Normal => "normal".to_string(),
        RunMode::Initialize(InitPreference::Path1) => "init_path1".to_string(),
        RunMode::Initialize(InitPreference::Path2) => "init_path2".to_string(),
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

    // Persist run-start row (run id, generation, workdir, recovery paths) BEFORE spawn (§12, §14).
    guard.store.insert_run_start(&run)?;

    if is_init {
        mapping.state.execution = Execution::Running;
    } else {
        mapping.state = apply_event(mapping.state, ObservedEvent::RunStarted).state;
    }
    mapping.updated_at = started_at.clone();
    guard.store.update_mapping_runtime(&mapping)?;

    if let Some(schedule) = guard.schedules.get_mut(mapping_id) {
        schedule.hint_at = None;
    }
    guard.active_run = Some(ActiveRunControl {
        mapping_id: mapping.id.clone(),
        run_id: run_id.clone(),
        stopper: None,
        stop_requested: false,
        volume_lost: false,
        force_stopped: false,
        timed_out_during_shutdown: false,
    });

    let plan = LaunchPlan {
        run_id: run_id.as_str().to_string(),
        mode,
        path1: generation.config.path1.clone(),
        path2: remote_ep.to_path2(),
        config_path: account.config_path,
        password_command: None,
        workdir: generation.workdir_path.clone(),
        filters_file: generation.filters_file_path.clone(),
        check_filename: ACCESS_MARKER_FILENAME.to_string(),
        max_delete_percent: mapping.max_delete_percent,
        backup_dir1,
        backup_dir2,
    };

    Ok(Some(PreparedRun {
        run,
        mapping,
        generation,
        plan,
        mode_label,
        is_init,
        log_file,
    }))
}

fn record_prepare_failure_locked(
    inner: &ManagerInner,
    guard: &mut SharedState,
    mapping_id: &MappingId,
    err: ManagerError,
) {
    if let Ok(Some(mut m)) = guard.store.get_mapping(mapping_id) {
        let next = apply_event(
            m.state,
            ObservedEvent::RunFinished(RunOutcome::InvalidConfiguration),
        );
        m.state = next.state;
        let now = inner.clock.now_rfc3339();
        m.updated_at = now.clone();
        let case = RecoveryCaseRecord {
            id: format!("rc-{}", Uuid::new_v4()),
            mapping_id: m.id.clone(),
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
            created_at: now,
            resolved_at: None,
            resolution_note: None,
        };
        if let Err(db_err) = guard.store.commit_mapping_and_recovery(&m, Some(&case)) {
            eprintln!("Error committing prepare failure recovery hold: {db_err}");
            record_terminal_db_error_locked(guard, mapping_id, db_err);
            return;
        }
        let _ = publish_updated_snapshot(inner, guard);
    }
}

fn record_terminal_db_error_locked(
    guard: &mut SharedState,
    mapping_id: &MappingId,
    db_err: ManagerError,
) {
    guard.storage_blocked_mappings.insert(mapping_id.clone());
    guard.last_terminal_error = Some((db_err.cause(), db_err.next_step()));
}

fn abort_before_spawn_locked(
    inner: &ManagerInner,
    guard: &mut SharedState,
    mut run: RunRecord,
    is_force_stopped: bool,
) {
    let now = inner.clock.now_rfc3339();
    guard.active_run = None;
    run.status = "interrupted".to_string();
    run.finished_at = Some(now.clone());
    run.outcome_kind = Some(
        if is_force_stopped {
            "force_stopped"
        } else {
            "canceled_before_spawn"
        }
        .to_string(),
    );
    run.outcome_detail = Some(
        if is_force_stopped {
            "Synchronization was force-stopped before spawning child process."
        } else {
            "Synchronization was stopped before spawning child process."
        }
        .to_string(),
    );
    run.last_verified_phase = "aborted_pre_spawn".to_string();

    let updated_mapping = match guard.store.get_mapping(&run.mapping_id) {
        Ok(Some(mut mapping)) => {
            if is_force_stopped {
                mapping.state = apply_event(
                    mapping.state,
                    ObservedEvent::RunFinished(RunOutcome::Interrupted),
                )
                .state;
            } else {
                mapping.state.execution = Execution::Idle;
            }
            mapping.last_run_id = Some(run.id.clone());
            mapping.last_run_outcome = Some(RunOutcome::Interrupted);
            mapping.last_run_completed_at = Some(now.clone());
            mapping.last_run_summary_text = Some(
                if is_force_stopped {
                    "Force-stopped before launch"
                } else {
                    "Stopped before launch"
                }
                .to_string(),
            );
            mapping.updated_at = now.clone();
            Some(mapping)
        }
        Ok(None) => None,
        Err(db_err) => {
            eprintln!("Error loading mapping for pre-spawn abort commit: {db_err}");
            record_terminal_db_error_locked(guard, &run.mapping_id, db_err);
            inner.idle_cv.notify_all();
            return;
        }
    };

    write_run_outcome_json(&inner.state_dir, &run);
    if let Err(db_err) = guard
        .store
        .commit_run_terminal(&run, updated_mapping.as_ref(), None)
    {
        eprintln!(
            "Error committing pre-spawn abort for `{}`: {db_err}",
            run.id
        );
        record_terminal_db_error_locked(guard, &run.mapping_id, db_err);
        inner.idle_cv.notify_all();
        return;
    }

    if let Some(schedule) = guard.schedules.get_mut(&run.mapping_id) {
        schedule.finish(inner.clock.now_utc(), false, run.id.as_str());
    }
    let _ = publish_updated_snapshot(inner, guard);
    inner.event_bus.publish_event(ManagerEvent::RunFinished {
        mapping_id: run.mapping_id,
        run_id: run.id,
        outcome: run
            .outcome_kind
            .unwrap_or_else(|| "canceled_before_spawn".to_string()),
        timestamp: now,
    });
    inner.idle_cv.notify_all();
}

fn abort_endpoint_check_failed_locked(
    inner: &ManagerInner,
    guard: &mut SharedState,
    mut run: RunRecord,
    event: Option<ObservedEvent>,
    err: ManagerError,
    volume: BoundVolume,
) {
    let now = inner.clock.now_rfc3339();
    let (outcome_kind, run_outcome) = match event {
        Some(ObservedEvent::VolumeLost) => ("volume_mismatch", RunOutcome::VolumeMismatch),
        Some(ObservedEvent::AuthenticationFailed) => {
            ("authentication_failure", RunOutcome::AuthenticationFailure)
        }
        Some(_) => ("endpoint_mismatch", RunOutcome::EndpointMismatch),
        None => ("preflight_failed", RunOutcome::RetryableFailure),
    };
    run.status = "failed".to_string();
    run.finished_at = Some(now.clone());
    run.outcome_kind = Some(outcome_kind.to_string());
    run.outcome_detail = Some(err.cause());
    run.last_verified_phase = "pre_spawn_endpoint_check_failed".to_string();

    let updated_mapping = match guard.store.get_mapping(&run.mapping_id) {
        Ok(Some(mut mapping)) => {
            if let Some(event) = event {
                mapping.state = apply_event(mapping.state, event).state;
            }
            mapping.state.execution = Execution::Idle;
            mapping.last_run_id = Some(run.id.clone());
            mapping.last_run_outcome = Some(run_outcome);
            mapping.last_run_completed_at = Some(now.clone());
            mapping.last_run_summary_text = Some(err.cause());
            mapping.updated_at = now.clone();
            Some(mapping)
        }
        Ok(None) => None,
        Err(db_err) => {
            eprintln!("Error loading mapping for pre-spawn endpoint check failure: {db_err}");
            record_terminal_db_error_locked(guard, &run.mapping_id, db_err);
            guard.active_run = None;
            inner.idle_cv.notify_all();
            return;
        }
    };

    write_run_outcome_json(&inner.state_dir, &run);
    if let Err(db_err) = guard
        .store
        .commit_run_terminal(&run, updated_mapping.as_ref(), None)
    {
        eprintln!(
            "Error committing pre-spawn endpoint check failure for `{}`: {db_err}",
            run.id
        );
        record_terminal_db_error_locked(guard, &run.mapping_id, db_err);
        guard.active_run = None;
        inner.idle_cv.notify_all();
        return;
    }

    guard.active_run = None;
    guard.volumes.insert(run.mapping_id.clone(), volume);
    if let Some(schedule) = guard.schedules.get_mut(&run.mapping_id) {
        schedule.finish(inner.clock.now_utc(), false, run.id.as_str());
    }
    let _ = publish_updated_snapshot(inner, guard);
    inner.event_bus.publish_event(ManagerEvent::RunFinished {
        mapping_id: run.mapping_id,
        run_id: run.id,
        outcome: outcome_kind.to_string(),
        timestamp: now,
    });
    inner.idle_cv.notify_all();
}

fn finish_with_child_identity_error(
    inner: &ManagerInner,
    mut run: RunRecord,
    db_err: ManagerError,
) {
    let now = inner.clock.now_rfc3339();
    let cause = format!(
        "Failed to persist child process identity ({}); child was force-stopped immediately.",
        db_err.cause()
    );
    run.status = "interrupted".to_string();
    run.finished_at = Some(now.clone());
    run.outcome_kind = Some("interrupted".to_string());
    run.outcome_detail = Some(cause.clone());
    run.last_verified_phase = "child_identity_persist_failed".to_string();

    let mut guard = inner.state.lock().unwrap();
    guard.active_run = None;

    let updated_mapping = match guard.store.get_mapping(&run.mapping_id) {
        Ok(Some(mut mapping)) => {
            let next = apply_event(
                mapping.state,
                ObservedEvent::RunFinished(RunOutcome::Interrupted),
            );
            mapping.state = next.state;
            mapping.last_run_id = Some(run.id.clone());
            mapping.last_run_outcome = Some(RunOutcome::Interrupted);
            mapping.last_run_completed_at = Some(now.clone());
            mapping.last_run_summary_text =
                Some("Interrupted — failed to persist child identity".to_string());
            mapping.updated_at = now.clone();
            Some(mapping)
        }
        Ok(None) => None,
        Err(load_err) => {
            eprintln!("Error loading mapping after child identity failure: {load_err}");
            record_terminal_db_error_locked(&mut guard, &run.mapping_id, load_err);
            inner.idle_cv.notify_all();
            return;
        }
    };

    let case = RecoveryCaseRecord {
        id: format!("rc-{}", Uuid::new_v4()),
        mapping_id: run.mapping_id.clone(),
        run_id: Some(run.id.clone()),
        status: "open".to_string(),
        reason_kind: "child_identity_persist_failed".to_string(),
        cause: cause.clone(),
        next_step:
            "Inspect SQLite storage, active processes, and the preserved work directory before clearing recovery."
                .to_string(),
        workdir_path: Some(run.workdir_path.clone()),
        backup_dir1: Some(run.backup_dir1.clone()),
        backup_dir2: Some(run.backup_dir2.clone()),
        log_path: Some(run.log_path.clone()),
        orphan_pid: None,
        created_at: now.clone(),
        resolved_at: None,
        resolution_note: None,
    };

    write_run_outcome_json(&inner.state_dir, &run);
    if let Err(commit_err) =
        guard
            .store
            .commit_run_terminal(&run, updated_mapping.as_ref(), Some(&case))
    {
        eprintln!(
            "Error committing child identity failure for `{}`: {commit_err}",
            run.id
        );
        record_terminal_db_error_locked(&mut guard, &run.mapping_id, commit_err);
        inner.idle_cv.notify_all();
        return;
    }

    let _ = publish_updated_snapshot(inner, &mut guard);
    inner.event_bus.publish_event(ManagerEvent::RecoveryOpened {
        mapping_id: run.mapping_id.clone(),
        case_id: case.id,
        reason_kind: case.reason_kind,
        cause,
        timestamp: now.clone(),
    });
    inner.event_bus.publish_event(ManagerEvent::RunFinished {
        mapping_id: run.mapping_id,
        run_id: run.id,
        outcome: "interrupted".to_string(),
        timestamp: now,
    });
    inner.idle_cv.notify_all();
}

fn launch_cancellation(guard: &SharedState, run_id: &RunId) -> Option<bool> {
    match guard.active_run.as_ref() {
        Some(active) if &active.run_id == run_id => {
            (active.force_stopped || active.stop_requested || guard.shutdown)
                .then_some(active.force_stopped)
        }
        _ => Some(false),
    }
}

fn execute_prepared_job(inner: &Arc<ManagerInner>, mut prepared: PreparedRun) {
    let volume = observe_binding(inner.volume_probe.as_ref(), &prepared.mapping.local_binding);
    let control = ProcessControl::with_timeout(DRIVE_COMMAND_TIMEOUT);
    let open_drive = || -> Result<Option<LockedDrive<'_>>, EndpointCheckError> {
        verify_endpoints_for_generation(inner, &prepared.mapping, &prepared.generation, &volume)?;
        if prepared.mapping.remote_endpoint_kind != "drive_remote" {
            return Ok(None);
        }
        inner
            .accounts
            .locked_drive(&prepared.mapping.account_id, &control)
            .map(Some)
            .map_err(drive_endpoint_error)
    };
    // Drive may refresh its token during bisync, so keep the account lock
    // through the child exit and terminal bookkeeping.
    let drive = match open_drive() {
        Ok(drive) => drive,
        Err((event, err)) => {
            let mut guard = inner.state.lock().unwrap();
            if let Some(forced) = launch_cancellation(&guard, &prepared.run.id) {
                abort_before_spawn_locked(inner, &mut guard, prepared.run, forced);
            } else {
                abort_endpoint_check_failed_locked(
                    inner,
                    &mut guard,
                    prepared.run,
                    event,
                    *err,
                    volume,
                );
            }
            return;
        }
    };
    if let Some(drive) = &drive {
        prepared.plan.config_path = drive.account.config_path.clone();
        prepared.plan.password_command = drive.request.password_command().cloned();
    }

    // Qualify engine and construct the request outside the state mutex.
    let prepared_launch = match inner.launcher.prepare(&prepared.plan) {
        Ok(p) => p,
        Err(err) => {
            finish_with_launch_error(inner, prepared.run, err);
            return;
        }
    };

    let (line_tx, line_rx) = mpsc::sync_channel::<OutputLine>(1024);
    let forwarder_inner = Arc::clone(inner);
    let fwd_mapping = prepared.run.mapping_id.clone();
    let fwd_run = prepared.run.id.clone();
    let mut log_file = prepared.log_file;
    // Stream valid NDJSON lines to disk live during the run (HIGH #8) and emit
    // live transfer progress events from parsed rclone JSON stats (HIGH #14).
    let forwarder = thread::spawn(move || {
        let mut last_progress_emit = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .unwrap_or_else(Instant::now);
        let mut last_heartbeat = Instant::now();
        let mut max_bytes = 0u64;
        let mut max_transfers = 0u64;
        let mut max_checks = 0u64;
        while let Ok(line) = line_rx.recv() {
            let ts = forwarder_inner.clock.now_rfc3339();
            let ndjson_line = format_ndjson_line(&line, &ts);
            let _ = writeln!(log_file, "{ndjson_line}");
            let _ = log_file.flush();
            if let LogEvent::Structured(ev) = parse_log_line(&line.text)
                && let Some(stats) = ev.stats
            {
                let first_bytes = max_bytes == 0 && stats.bytes > 0;
                max_bytes = max_bytes.max(stats.bytes);
                max_transfers = max_transfers.max(stats.transfers);
                max_checks = max_checks.max(stats.checks);
                let total_bytes = (stats.total_bytes > 0).then_some(stats.total_bytes);
                if first_bytes || last_progress_emit.elapsed() >= Duration::from_millis(250) {
                    last_progress_emit = Instant::now();
                    forwarder_inner
                        .event_bus
                        .publish_event(ManagerEvent::RunProgress {
                            mapping_id: fwd_mapping.clone(),
                            run_id: fwd_run.clone(),
                            bytes_transferred: max_bytes,
                            total_bytes,
                            files_transferred: max_transfers,
                            files_checked: max_checks,
                            timestamp: ts.clone(),
                        });
                }
            }
            if let Some(hb) = forwarder_inner.finder_heartbeat_interval
                && last_heartbeat.elapsed() >= hb
            {
                last_heartbeat = Instant::now();
                let mut refreshed =
                    (*forwarder_inner.state.lock().unwrap().latest_snapshot).clone();
                refreshed.generated_at = ts.clone();
                let _ = forwarder_inner.finder_writer.write_snapshot(&refreshed);
            }
            forwarder_inner
                .event_bus
                .publish_event(ManagerEvent::RunOutputLine {
                    mapping_id: fwd_mapping.clone(),
                    run_id: fwd_run.clone(),
                    line: line.text,
                    timestamp: ts,
                });
        }
        let _ = log_file.sync_all();
    });

    // Validate filesystem inputs outside the mutex. Recheck the active run
    // and cancellation under the lock immediately before spawn and PID commit.
    let volume = observe_binding(inner.volume_probe.as_ref(), &prepared.mapping.local_binding);
    let verify_backups = || {
        reject_symlinks_under_root(
            "backup_dir1",
            &prepared.mapping.local_recovery_root,
            &prepared.run.backup_dir1,
        )?;
        if prepared.mapping.remote_endpoint_kind == "local_dir" {
            reject_symlinks_under_root(
                "backup_dir2",
                Path::new(&prepared.mapping.cloud_recovery_path),
                Path::new(&prepared.run.backup_dir2),
            )?;
        }
        Ok(())
    };
    let verification =
        verify_endpoints_for_generation(inner, &prepared.mapping, &prepared.generation, &volume)
            .and_then(|()| verify_backups().map_err(|err| (None, Box::new(err))))
            .and_then(|()| {
                if let Some(drive) = &drive {
                    verify_drive_generation(drive, &prepared.mapping, &prepared.generation)?;
                }
                Ok(())
            });
    let mut run = prepared.run;
    let launched = {
        let mut guard = inner.state.lock().unwrap();
        if let Some(is_force_stopped) = launch_cancellation(&guard, &run.id) {
            drop(prepared_launch);
            drop(line_tx);
            abort_before_spawn_locked(inner, &mut guard, run, is_force_stopped);
            drop(guard);
            let _ = forwarder.join();
            return;
        }

        if let Err((event, err)) = verification {
            drop(prepared_launch);
            drop(line_tx);
            abort_endpoint_check_failed_locked(inner, &mut guard, run, event, *err, volume);
            drop(guard);
            let _ = forwarder.join();
            return;
        }

        let launched = match prepared_launch.spawn(SpawnOptions {
            limits: OutputLimits::default(),
            line_observer: Some(line_tx),
        }) {
            Ok(l) => l,
            Err(err) => {
                drop(guard);
                let _ = forwarder.join();
                finish_with_launch_error(inner, run, err);
                return;
            }
        };

        // Test fault-injection hook simulating an abrupt manager crash right after
        // spawn BEFORE updating child_pid / start_time in SQLite (§14).
        if launched.simulate_crash_before_pid_record {
            guard.active_run = None;
            inner.idle_cv.notify_all();
            drop(guard);
            drop(launched);
            let _ = forwarder.join();
            return;
        }

        let stopper = launched.stop_handle();
        // Commit child identity to SQLite FIRST before any post-spawn file work (HIGH #8).
        if let Err(db_err) =
            guard
                .store
                .update_run_child_identity(&run.id, &launched.identity, &launched.engine)
        {
            guard.active_run = None;
            drop(guard);
            let _ = stopper.force_stop();
            let _ = launched.wait();
            let _ = forwarder.join();
            finish_with_child_identity_error(inner, run, db_err);
            return;
        }

        run.child_pid = Some(launched.identity.pid);
        run.child_start_time = launched.identity.start_time;
        run.child_executable = Some(launched.identity.executable.clone());
        run.engine_version = Some(launched.engine.version.clone());
        run.engine_sha256 = Some(launched.engine.sha256.clone());
        run.engine_platform = Some(launched.engine.platform.clone());
        run.status = "running".to_string();
        run.last_verified_phase = "spawned".to_string();

        if let Some(active) = guard.active_run.as_mut() {
            active.stopper = Some(Arc::clone(&stopper));
        }
        let _ = publish_updated_snapshot(inner, &mut guard);
        inner.event_bus.publish_event(ManagerEvent::RunStarted {
            mapping_id: run.mapping_id.clone(),
            run_id: run.id.clone(),
            mode: prepared.mode_label.clone(),
            timestamp: run.started_at.clone(),
        });
        launched
    };

    // 3. Only AFTER child identity is committed, write runs/<run-id>/argv.json (HIGH #2, #8).
    let argv_doc = serde_json::json!({
        "run_id": run.id.as_str(),
        "mode": prepared.mode_label,
        "argv": launched.sanitized_argv,
        "engine": {
            "version": launched.engine.version,
            "sha256": launched.engine.sha256,
            "platform": launched.engine.platform,
            "executable": launched.engine.executable,
        }
    });
    if let Ok(bytes) = serde_json::to_vec_pretty(&argv_doc)
        && let Err(err) = write_private_file_under(
            "argv_json",
            &inner.state_dir,
            &run.run_dir.join("argv.json"),
            &bytes,
        )
    {
        eprintln!(
            "Warning: failed to write argv.json for run `{}`: {err}",
            run.id
        );
    }

    // 4. Wait for child completion without holding the manager state lock.
    let wait_res = launched.wait();
    let _ = forwarder.join();

    match wait_res {
        Ok(run_out) => {
            finalize_completed_run(inner, run, &prepared.mapping, prepared.is_init, run_out);
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
    let already_timed_out = guard
        .active_run
        .as_ref()
        .is_some_and(|a| a.timed_out_during_shutdown);
    let already_force_stopped = guard.active_run.as_ref().is_some_and(|a| a.force_stopped);
    guard.active_run = None;
    if already_timed_out {
        // Never overwrite a persisted shutdown-timeout record (HIGH #6).
        inner.idle_cv.notify_all();
        return;
    }

    let updated_mapping = match guard.store.get_mapping(&run.mapping_id) {
        Ok(Some(mut mapping)) => {
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
            Some(mapping)
        }
        Ok(None) => None,
        Err(db_err) => {
            eprintln!("Error loading mapping for launch failure commit: {db_err}");
            record_terminal_db_error_locked(&mut guard, &run.mapping_id, db_err);
            inner.idle_cv.notify_all();
            return;
        }
    };

    let case = if !already_force_stopped {
        Some(RecoveryCaseRecord {
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
        })
    } else {
        None
    };

    // Write outcome.json BEFORE terminal commit (HIGH #8), then commit atomically (HIGH #7).
    write_run_outcome_json(&inner.state_dir, &run);
    if let Err(db_err) =
        guard
            .store
            .commit_run_terminal(&run, updated_mapping.as_ref(), case.as_ref())
    {
        eprintln!("Error committing launch failure atomically: {db_err}");
        record_terminal_db_error_locked(&mut guard, &run.mapping_id, db_err);
        inner.idle_cv.notify_all();
        return;
    }

    let _ = publish_updated_snapshot(inner, &mut guard);
    if let Some(c) = case {
        inner.event_bus.publish_event(ManagerEvent::RecoveryOpened {
            mapping_id: run.mapping_id.clone(),
            case_id: c.id,
            reason_kind: c.reason_kind,
            cause: c.cause,
            timestamp: now.clone(),
        });
    }
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
            graceful, reason, ..
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
    mapping: &MappingRecord,
    is_init: bool,
    run_out: rclone_adapter::RunOutput,
) {
    let mut files_checked = 0u64;
    let mut files_transferred = 0u64;
    let mut bytes_transferred = 0u64;

    for line in &run_out.output.lines {
        if let LogEvent::Structured(ev) = parse_log_line(&line.text)
            && let Some(stats) = ev.stats
        {
            files_checked = files_checked.max(stats.checks);
            files_transferred = files_transferred.max(stats.transfers);
            bytes_transferred = bytes_transferred.max(stats.bytes);
        }
    }

    // Rewrite disk output.ndjson atomically from the adapter's lossless retained
    // output buffer (up to its bounds), refusing any symlinks under state_dir (HIGH #2, #8).
    let log_ts = inner.clock.now_rfc3339();
    let mut final_log = String::new();
    for line in &run_out.output.lines {
        final_log.push_str(&format_ndjson_line(line, &log_ts));
        final_log.push('\n');
    }
    if run_out.output.dropped_lines > 0 || run_out.output.dropped_diagnostic_events > 0 {
        let notice = serde_json::json!({
            "stream": "manager",
            "warning": "output_buffer_truncated",
            "dropped_lines": run_out.output.dropped_lines,
            "dropped_diagnostic_events": run_out.output.dropped_diagnostic_events,
            "timestamp": log_ts,
        });
        final_log.push_str(&notice.to_string());
        final_log.push('\n');
    }
    if let Err(err) = write_private_file_atomic_under(
        "log_path",
        &inner.state_dir,
        &run.log_path,
        final_log.as_bytes(),
    ) {
        eprintln!(
            "Warning: failed to finalize output.ndjson for run `{}`: {err}",
            run.id
        );
    }

    let classified = classify_run_outcome(&run_out);
    let volume = observe_binding(inner.volume_probe.as_ref(), &mapping.local_binding);
    let mut guard = inner.state.lock().unwrap();
    let already_timed_out = guard
        .active_run
        .as_ref()
        .is_some_and(|a| a.timed_out_during_shutdown);
    let already_force_stopped = guard.active_run.as_ref().is_some_and(|a| a.force_stopped);
    let observed_loss = guard
        .active_run
        .as_ref()
        .is_some_and(|active| active.volume_lost);
    guard.active_run = None;
    if already_timed_out {
        // Never overwrite a persisted shutdown-timeout record (HIGH #6).
        inner.idle_cv.notify_all();
        return;
    }

    let volume_lost = observed_loss || !matches!(volume, BoundVolume::Present(_));
    let (core_outcome, outcome_kind, outcome_detail, conflict_paths, recovery_seed) =
        if volume_lost && !already_force_stopped {
            let cause = "The local disk disconnected while synchronization was active.".to_owned();
            (
                RunOutcome::Interrupted,
                "interrupted",
                cause.clone(),
                Vec::new(),
                Some(RecoverySeed {
                    reason_kind: "volume_disconnected",
                    cause,
                    next_step:
                        "Reconnect the disk and review the preserved sync state before resuming."
                            .to_owned(),
                }),
            )
        } else {
            map_adapter_outcome(&classified)
        };

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

    let updated_mapping = match guard.store.get_mapping(&run.mapping_id) {
        Ok(Some(mut mapping)) => {
            if !matches!(volume, BoundVolume::Present(_)) {
                mapping.state = apply_event(mapping.state, ObservedEvent::VolumeLost).state;
                if matches!(volume, BoundVolume::Moved(_) | BoundVolume::Legacy) {
                    mapping.state =
                        apply_event(mapping.state, ObservedEvent::EndpointChanged).state;
                }
            }
            if already_force_stopped {
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
                mapping.scheduling_enabled = true;
            } else {
                mapping.state =
                    apply_event(mapping.state, ObservedEvent::RunFinished(core_outcome)).state;
            }

            mapping.last_run_id = Some(run.id.clone());
            mapping.last_run_outcome = Some(core_outcome);
            mapping.last_run_completed_at = Some(now.clone());
            mapping.last_run_summary_text = Some(summary_text);
            if core_outcome == RunOutcome::SuccessWithConflicts {
                mapping.unreviewed_conflict_run_id = Some(run.id.clone());
            }
            mapping.updated_at = now.clone();
            Some(mapping)
        }
        Ok(None) => None,
        Err(db_err) => {
            eprintln!("Error loading mapping during run finalization: {db_err}");
            record_terminal_db_error_locked(&mut guard, &run.mapping_id, db_err);
            inner.idle_cv.notify_all();
            return;
        }
    };

    let case = if !already_force_stopped && let Some(seed) = recovery_seed {
        Some(RecoveryCaseRecord {
            id: format!("rc-{}", Uuid::new_v4()),
            mapping_id: run.mapping_id.clone(),
            run_id: Some(run.id.clone()),
            status: "open".to_string(),
            reason_kind: seed.reason_kind.to_string(),
            cause: seed.cause,
            next_step: seed.next_step,
            workdir_path: Some(run.workdir_path.clone()),
            backup_dir1: Some(run.backup_dir1.clone()),
            backup_dir2: Some(run.backup_dir2.clone()),
            log_path: Some(run.log_path.clone()),
            orphan_pid: None,
            created_at: now.clone(),
            resolved_at: None,
            resolution_note: None,
        })
    } else {
        None
    };

    // Write outcome.json BEFORE terminal commit (HIGH #8), then commit run +
    // mapping state + recovery case atomically in ONE SQLite transaction (HIGH #7).
    write_run_outcome_json(&inner.state_dir, &run);
    if let Err(db_err) =
        guard
            .store
            .commit_run_terminal(&run, updated_mapping.as_ref(), case.as_ref())
    {
        eprintln!("Error committing completed run `{}`: {db_err}", run.id);
        record_terminal_db_error_locked(&mut guard, &run.mapping_id, db_err);
        inner.idle_cv.notify_all();
        return;
    }

    if let Some(mapping) = &updated_mapping
        && mapping.scheduling_enabled
        && let Some(generation) = &mapping.active_generation_id
    {
        let now = inner.clock.now_utc();
        let schedule = guard
            .schedules
            .entry(mapping.id.clone())
            .or_insert_with(|| Schedule::new(generation.clone(), now));
        schedule.finish(now, outcome_kind == "retryable", run.id.as_str());
    }
    guard.volumes.insert(run.mapping_id.clone(), volume);
    let _ = publish_updated_snapshot(inner, &mut guard);
    if let Some(c) = case {
        inner.event_bus.publish_event(ManagerEvent::RecoveryOpened {
            mapping_id: run.mapping_id.clone(),
            case_id: c.id,
            reason_kind: c.reason_kind,
            cause: c.cause,
            timestamp: now.clone(),
        });
    }
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
