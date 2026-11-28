//! Real `sync-manager` adapter for the NSH desktop UI (`ManagerSource`).
//!
//! `ManagerSource` owns a [`SyncManager`] strictly on a dedicated background
//! worker thread so the NSH UI thread never locks manager state or blocks on
//! filesystem/SQLite/process I/O.
//!
//! Snapshots flow through a coalescing latest-snapshot slot ([`UiBridgeSlot`])
//! so the UI always renders the newest state without replaying stale snapshots,
//! while UI commands ride on a dedicated command channel ahead of log output
//! (HIGH #14). Platform system actions in the real desktop app are dispatched
//! over [`UiControlEvent::SystemAction`] to the NSH main thread (HIGH #8).

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use chrono::DateTime;
use rclone_adapter::{
    InitPreference, ProcessError, ProcessRunner, ProcessSpec, RealProcessRunner, RunHandle,
    SpawnOptions,
};
use sync_core::{
    ACCESS_MARKER_FILENAME, AccountId, CloudCollection, DEFAULT_EXCLUSIONS, Execution, Intent,
    LastRunSummary, MappingId, MappingState, Readiness, RunId, VolumeIdentity, VolumeKind,
    WallTime, available_actions,
};
use sync_manager::{
    AccountError, AccountRecord, AccountService, AccountServiceConfig, AddDriveAccountRequest,
    AddMappingCommand, BisyncLauncher, CollectionCandidate, ControlledShutdownOutcome,
    DarwinFinderNotifier, DriveAuthorization, FilterChangePreview, FinderActionDispatch,
    FinderNotifier, FolderSizeSnapshot, FolderSizeStatus, ImportAccountRequest, InitializeCommand,
    ManagerConfig, ManagerError, ManagerEvent, MappingStatusSnapshot, NoopFinderNotifier,
    OperationCanceller, OperationHandle, RecoveryCopySide, RunRecord, SecretString,
    SideClassification, SkippedCollection, StatusSnapshot, SyncManager, ValidatedFinderAction,
    detect_volume_identity, inspect_local_endpoint_detailed,
};

use crate::{
    contract::{
        AccountSnapshot, AccountStatus, ActiveRunSnapshot, ActivityEvent, AttentionItem,
        AttentionKind, AttentionPriority, CloudFolderCandidate, DemoPreset, FilterReview,
        FinderExtensionStatus, FolderSize, IconKind, InspectedRunSnapshot, LocationCheck,
        MappingSnapshot, PreferencesSnapshot, RecoveryCauseKind, RecoveryChecklistData,
        RecoveryCopyItem, ReviewScenario, RunPhase, RunRecordSnapshot, ScheduledCheck, ScreenRoute,
        StatusModifier, SubfolderNode, Tone, UiCommand, UiSnapshot, UnreviewedConflicts,
        VolumeSnapshot, WizardDraft, WizardStep, format_bytes, is_low_space,
    },
    system_actions::{
        LaunchAtLoginOutcome, SystemActionOutcome, SystemActionRequest, SystemActions,
        is_drive_authorization_url,
    },
};

const DEFAULT_RECENT_RUNS_FOR_UI: usize = 15;
const DEFAULT_RUN_LOG_MAX_LINES: usize = 500;
const DEFAULT_QUIT_TIMEOUT: Duration = Duration::from_secs(85);
const MAX_PENDING_MANAGER_EVENTS: usize = 512;

/// Ordered control events sent from the manager worker to the NSH main thread.
#[derive(Debug, Clone)]
pub enum UiControlEvent {
    Navigate(ScreenRoute),
    ShowWindowAndNavigate(ScreenRoute),
    /// Real macOS desktop action to execute strictly on the NSH main thread (HIGH #8).
    SystemAction {
        request_id: u64,
        request: SystemActionRequest,
    },
    QuitReady(ControlledShutdownOutcome),
    QuitDeferred,
}

struct UiBridgeInner {
    latest_snapshot: Option<Box<UiSnapshot>>,
    control_events: VecDeque<UiControlEvent>,
    closed: bool,
}

/// Coalescing UI bridge slot: stores ONLY the single latest [`UiSnapshot`]
/// alongside an ordered queue of [`UiControlEvent`]s so slow UI consumers never
/// replay older snapshots or stall behind log floods (HIGH #14).
pub struct UiBridgeSlot {
    inner: Mutex<UiBridgeInner>,
    cv: Condvar,
}

impl UiBridgeSlot {
    fn new() -> Self {
        Self {
            inner: Mutex::new(UiBridgeInner {
                latest_snapshot: None,
                control_events: VecDeque::new(),
                closed: false,
            }),
            cv: Condvar::new(),
        }
    }

    fn publish_snapshot(&self, snap: UiSnapshot) {
        let mut g = self.inner.lock().unwrap();
        g.latest_snapshot = Some(Box::new(snap));
        self.cv.notify_all();
    }

    fn push_control(&self, ev: UiControlEvent) {
        let mut g = self.inner.lock().unwrap();
        g.control_events.push_back(ev);
        self.cv.notify_all();
    }

    fn close(&self) {
        let mut g = self.inner.lock().unwrap();
        g.closed = true;
        self.cv.notify_all();
    }

    /// Blocks until either a newer snapshot or at least one control event is ready.
    /// Returns `None` once the worker has exited and no items remain.
    pub fn wait_for_batch(&self) -> Option<(Option<Box<UiSnapshot>>, Vec<UiControlEvent>)> {
        let mut g = self.inner.lock().unwrap();
        while g.latest_snapshot.is_none() && g.control_events.is_empty() && !g.closed {
            g = self.cv.wait(g).unwrap();
        }
        if g.latest_snapshot.is_none() && g.control_events.is_empty() && g.closed {
            return None;
        }
        let snap = g.latest_snapshot.take();
        let events: Vec<UiControlEvent> = g.control_events.drain(..).collect();
        Some((snap, events))
    }

    /// Waits up to `timeout` for the next control event matching `pred` (for tests).
    pub fn wait_for_control_timeout(
        &self,
        timeout: Duration,
        pred: impl Fn(&UiControlEvent) -> bool,
    ) -> Option<UiControlEvent> {
        let deadline = Instant::now() + timeout;
        let mut g = self.inner.lock().unwrap();
        loop {
            if let Some(pos) = g.control_events.iter().position(&pred) {
                return g.control_events.remove(pos);
            }
            if g.closed {
                return None;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let (next_g, wait_res) = self.cv.wait_timeout(g, remaining).unwrap();
            g = next_g;
            if wait_res.timed_out() {
                return g
                    .control_events
                    .iter()
                    .position(&pred)
                    .and_then(|pos| g.control_events.remove(pos));
            }
        }
    }
}

/// Configuration for creating a [`ManagerSource`].
pub struct ManagerSourceConfig {
    pub state_dir: Option<PathBuf>,
    pub finder_status_dir: Option<PathBuf>,
    pub launcher: Option<Arc<dyn BisyncLauncher>>,
    /// Tests can supply the service with a fake credential store and process runner.
    pub account_service: Option<Arc<AccountService>>,
    /// Overrides the manager's account dependencies for tests.
    pub manager_accounts: Option<AccountServiceConfig>,
    pub finder_notifier: Option<Arc<dyn FinderNotifier>>,
    pub finder_heartbeat_interval: Option<Duration>,
    pub shutdown_timeout: Duration,
    /// Interval for draining `<finder_dir>/actions/*.json` (None disables background polling in unit tests).
    pub finder_poll_interval: Option<Duration>,
}

impl Default for ManagerSourceConfig {
    fn default() -> Self {
        Self {
            state_dir: None,
            finder_status_dir: None,
            launcher: None,
            account_service: None,
            manager_accounts: None,
            // Real interactive desktop app uses DarwinFinderNotifier; tests explicitly pass Noop/Recording (HIGH #15).
            finder_notifier: Some(Arc::new(DarwinFinderNotifier)),
            finder_heartbeat_interval: Some(Duration::from_secs(60)),
            shutdown_timeout: DEFAULT_QUIT_TIMEOUT,
            finder_poll_interval: Some(Duration::from_millis(500)),
        }
    }
}

enum WorkerCmd {
    Command(UiCommand),
    AuthorizationReady(SecretString),
    AccountOperationFinished(Result<AccountRecord, String>),
    SystemActionCompleted {
        _request_id: u64,
        outcome: SystemActionOutcome,
    },
    DrainFinderActions,
    ControlledShutdown {
        timeout: Duration,
    },
    ShutdownNow,
}

struct WorkerWakeState {
    commands: VecDeque<WorkerCmd>,
    latest_mgr_snap: Option<Arc<StatusSnapshot>>,
    events: VecDeque<ManagerEvent>,
    closed: bool,
}

/// Priority-ordered wakeup inbox for the manager worker thread:
/// commands are ALWAYS drained before manager snapshots and events, and manager
/// snapshots coalesce in a single slot so older snapshots are never replayed (HIGH #14).
struct WorkerInbox {
    state: Mutex<WorkerWakeState>,
    cv: Condvar,
}

type WorkerBatch = (
    Vec<WorkerCmd>,
    Option<Arc<StatusSnapshot>>,
    Vec<ManagerEvent>,
);

impl WorkerInbox {
    fn new() -> Self {
        Self {
            state: Mutex::new(WorkerWakeState {
                commands: VecDeque::new(),
                latest_mgr_snap: None,
                events: VecDeque::new(),
                closed: false,
            }),
            cv: Condvar::new(),
        }
    }

    fn push_cmd(&self, cmd: WorkerCmd) {
        let mut g = self.state.lock().unwrap();
        if !g.closed {
            g.commands.push_back(cmd);
            self.cv.notify_one();
        }
    }

    fn set_latest_mgr_snap(&self, snap: Arc<StatusSnapshot>) {
        let mut g = self.state.lock().unwrap();
        if !g.closed {
            g.latest_mgr_snap = Some(snap);
            self.cv.notify_one();
        }
    }

    fn push_event(&self, ev: ManagerEvent) {
        let mut g = self.state.lock().unwrap();
        if !g.closed {
            if g.events.len() >= MAX_PENDING_MANAGER_EVENTS {
                // Drop oldest log line before dropping lifecycle events.
                if let Some(pos) = g
                    .events
                    .iter()
                    .position(|e| matches!(e, ManagerEvent::RunOutputLine { .. }))
                {
                    g.events.remove(pos);
                } else {
                    g.events.pop_front();
                }
            }
            g.events.push_back(ev);
            self.cv.notify_one();
        }
    }

    fn close(&self) {
        let mut g = self.state.lock().unwrap();
        g.closed = true;
        self.cv.notify_all();
    }

    fn take_batch(&self) -> Option<WorkerBatch> {
        let mut g = self.state.lock().unwrap();
        while g.commands.is_empty()
            && g.latest_mgr_snap.is_none()
            && g.events.is_empty()
            && !g.closed
        {
            g = self.cv.wait(g).unwrap();
        }
        if g.commands.is_empty() && g.latest_mgr_snap.is_none() && g.events.is_empty() && g.closed {
            return None;
        }
        let cmds: Vec<WorkerCmd> = g.commands.drain(..).collect();
        let snap = g.latest_mgr_snap.take();
        let events: Vec<ManagerEvent> = g.events.drain(..).collect();
        Some((cmds, snap, events))
    }
}

/// Real manager-backed data source owning `SyncManager` on a background worker thread.
pub struct ManagerSource {
    inbox: Arc<WorkerInbox>,
    ui_slot: Arc<UiBridgeSlot>,
    latest_cached: Arc<Mutex<UiSnapshot>>,
    shutdown_timeout: Duration,
    timed_out_shutdown: Arc<AtomicBool>,
    worker_handle: Mutex<Option<JoinHandle<()>>>,
}

impl ManagerSource {
    /// Spawns the background manager worker thread for the real desktop app,
    /// routing platform system actions over [`UiControlEvent::SystemAction`]
    /// to execute strictly on the NSH main thread (HIGH #8).
    pub fn spawn_for_app(config: ManagerSourceConfig) -> Self {
        Self::spawn_internal(config, None)
    }

    /// Spawns the background manager worker thread with an explicit injectable
    /// [`SystemActions`] implementation (used by tests with `RecordingSystemActions`).
    pub fn spawn(config: ManagerSourceConfig, system_actions: Arc<dyn SystemActions>) -> Self {
        Self::spawn_internal(config, Some(system_actions))
    }

    fn spawn_internal(
        config: ManagerSourceConfig,
        system_actions: Option<Arc<dyn SystemActions>>,
    ) -> Self {
        let inbox = Arc::new(WorkerInbox::new());
        let ui_slot = Arc::new(UiBridgeSlot::new());
        let initial_snapshot = empty_loading_snapshot();
        let latest_cached = Arc::new(Mutex::new(initial_snapshot));
        let timed_out_shutdown = Arc::new(AtomicBool::new(false));

        let inbox_for_worker = Arc::clone(&inbox);
        let ui_slot_for_worker = Arc::clone(&ui_slot);
        let cached_for_worker = Arc::clone(&latest_cached);
        let timed_out_for_worker = Arc::clone(&timed_out_shutdown);
        let shutdown_timeout = config.shutdown_timeout;

        let handle = thread::Builder::new()
            .name("foldersync-ui-manager".to_string())
            .spawn(move || {
                manager_worker_main(
                    config,
                    system_actions,
                    inbox_for_worker,
                    ui_slot_for_worker,
                    cached_for_worker,
                    timed_out_for_worker,
                );
            })
            .expect("failed to spawn foldersync-ui-manager worker thread");

        Self {
            inbox,
            ui_slot,
            latest_cached,
            shutdown_timeout,
            timed_out_shutdown,
            worker_handle: Mutex::new(Some(handle)),
        }
    }

    /// Returns a cloned snapshot immediately without touching manager locks.
    pub fn snapshot(&self) -> UiSnapshot {
        self.latest_cached.lock().unwrap().clone()
    }

    /// Returns the coalescing UI bridge slot (consumed off the NSH thread via `blocking`).
    pub fn ui_slot(&self) -> Arc<UiBridgeSlot> {
        Arc::clone(&self.ui_slot)
    }

    /// Non-blocking dispatch of a [`UiCommand`] to the manager worker thread.
    pub fn dispatch(&self, cmd: UiCommand) {
        self.inbox.push_cmd(WorkerCmd::Command(cmd));
    }

    /// Reports the actual outcome of a main-thread system action back to the worker (HIGH #8).
    pub fn complete_system_action(&self, request_id: u64, outcome: SystemActionOutcome) {
        self.inbox.push_cmd(WorkerCmd::SystemActionCompleted {
            _request_id: request_id,
            outcome,
        });
    }

    /// Triggers the single controlled-stop quit flow on the manager worker thread.
    pub fn request_controlled_shutdown(&self) {
        self.inbox.push_cmd(WorkerCmd::ControlledShutdown {
            timeout: self.shutdown_timeout,
        });
    }

    /// Triggers Finder action file drain immediately (useful in deterministic tests).
    pub fn poll_finder_actions(&self) {
        self.inbox.push_cmd(WorkerCmd::DrainFinderActions);
    }

    /// Gracefully stops the worker thread and closes `SyncManager`.
    pub fn shutdown(&self) {
        self.inbox.push_cmd(WorkerCmd::ShutdownNow);
        if !self.timed_out_shutdown.load(Ordering::SeqCst)
            && let Some(handle) = self.worker_handle.lock().unwrap().take()
        {
            let _ = handle.join();
        }
    }
}

impl Drop for ManagerSource {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct LiveRunProgress {
    mapping_id: MappingId,
    run_id: RunId,
    started_at: Instant,
    bytes_transferred: u64,
    total_bytes: Option<u64>,
}

enum AccountOperation {
    Authorize(DriveAuthorization),
    Import(ImportAccountRequest),
    Check(AccountId),
}

struct PendingFolderSize {
    account: AccountId,
    folder_id: String,
    handle: OperationHandle<FolderSizeSnapshot>,
    canceled: bool,
}

struct WorkerState {
    mgr: Option<Arc<SyncManager>>,
    account_service: Option<Arc<AccountService>>,
    account_worker: Option<JoinHandle<()>>,
    account_operation_id: Option<AccountId>,
    reconnecting_account: Option<AccountId>,
    authorization_canceller: Option<OperationCanceller>,
    authorization_url: Option<SecretString>,
    unverified_accounts: BTreeSet<AccountId>,
    inbox: Arc<WorkerInbox>,
    system_actions: Option<Arc<dyn SystemActions>>,
    next_sys_action_id: u64,
    local_account_parents: BTreeMap<AccountId, PathBuf>,
    recent_runs_cache: BTreeMap<MappingId, Vec<RunRecordSnapshot>>,
    recent_runs_dirty: bool,
    live_progress: Option<LiveRunProgress>,
    first_sync_started_at: Option<Instant>,
    filter_preview: Option<FilterChangePreview>,
    wizard: WizardDraft,
    cloud_catalog: Vec<CloudFolderCandidate>,
    skipped_cloud_folders: Vec<SkippedCollection>,
    cloud_catalog_error: Option<String>,
    refresh_catalog_after_size: bool,
    pending_folder_size: Option<PendingFolderSize>,
    requested_folder_size: Option<String>,
    inspected_run: Option<InspectedRunSnapshot>,
    recovery_checklist: Option<RecoveryChecklistData>,
    preferences: PreferencesSnapshot,
    recent_events: Vec<ActivityEvent>,
    next_event_id: u64,
    raw_log: Vec<String>,
    status_notice: Option<String>,
    action_error: Option<String>,
    device_id: String,
}

fn manager_worker_main(
    config: ManagerSourceConfig,
    system_actions: Option<Arc<dyn SystemActions>>,
    inbox: Arc<WorkerInbox>,
    ui_slot: Arc<UiBridgeSlot>,
    latest_cached: Arc<Mutex<UiSnapshot>>,
    timed_out_shutdown: Arc<AtomicBool>,
) {
    let mut mgr_cfg = match config.state_dir {
        Some(dir) => ManagerConfig::with_state_dir(dir),
        None => ManagerConfig {
            state_dir: None,
            finder_status_dir: None,
            finder_notifier: Arc::new(NoopFinderNotifier),
            finder_heartbeat_interval: Some(Duration::from_secs(60)),
            clock: Arc::new(sync_manager::SystemClock),
            process_inspector: Arc::new(sync_manager::OsProcessInspector),
            launcher: Arc::new(sync_manager::RealBisyncLauncher::new(None)),
            event_buffer_capacity: sync_manager::snapshot::DEFAULT_EVENT_BUFFER_CAPACITY,
        },
    };
    mgr_cfg.finder_status_dir = config.finder_status_dir;
    mgr_cfg.finder_heartbeat_interval = config.finder_heartbeat_interval;
    if let Some(launcher) = config.launcher {
        mgr_cfg.launcher = launcher;
    }
    if let Some(notifier) = config.finder_notifier {
        mgr_cfg.finder_notifier = notifier;
    }

    let mut state = WorkerState {
        mgr: None,
        account_service: config.account_service,
        account_worker: None,
        account_operation_id: None,
        reconnecting_account: None,
        authorization_canceller: None,
        authorization_url: None,
        unverified_accounts: BTreeSet::new(),
        inbox: inbox.clone(),
        system_actions,
        next_sys_action_id: 1,
        local_account_parents: BTreeMap::new(),
        recent_runs_cache: BTreeMap::new(),
        recent_runs_dirty: true,
        live_progress: None,
        first_sync_started_at: None,
        filter_preview: None,
        wizard: default_real_wizard(),
        cloud_catalog: Vec::new(),
        skipped_cloud_folders: Vec::new(),
        cloud_catalog_error: None,
        refresh_catalog_after_size: false,
        pending_folder_size: None,
        requested_folder_size: None,
        inspected_run: None,
        recovery_checklist: None,
        preferences: default_real_preferences(),
        recent_events: Vec::new(),
        next_event_id: 1,
        raw_log: Vec::new(),
        status_notice: None,
        action_error: None,
        device_id: "this-mac".to_owned(),
    };

    let mut snap_fwd_handle = None;
    let mut ev_fwd_handle = None;
    let mut finder_poll_handle = None;
    let stop_flag = Arc::new(AtomicBool::new(false));

    let account_config = config.manager_accounts.unwrap_or_else(|| {
        let mut config = AccountServiceConfig::with_state_dir("");
        config.process_runner = Arc::new(DriveAccountProcessRunner);
        config
    });
    match SyncManager::open_with_account_config(mgr_cfg, account_config) {
        Ok(mgr) => {
            // Restore persisted local-fixture account parent paths across restarts (HIGH #3).
            if let Ok(accounts) = mgr.list_accounts() {
                for acct in &accounts {
                    let candidate = PathBuf::from(&acct.remote_root_folder_id);
                    if acct.kind == "local_fixture" && candidate.is_absolute() && candidate.is_dir()
                    {
                        state
                            .local_account_parents
                            .insert(acct.id.clone(), candidate);
                    }
                }
                if let Some(first) = accounts.first() {
                    state.wizard.selected_account = first.id.clone();
                    state.wizard.step = if accounts.len() > 1 {
                        WizardStep::Account
                    } else {
                        WizardStep::Folder
                    };
                }
            }

            let snap_rx = mgr.subscribe_snapshots(1);
            let ev_rx = mgr.subscribe_events(256);
            let inbox_snap = Arc::clone(&inbox);
            snap_fwd_handle = Some(thread::spawn(move || {
                while let Ok(snap) = snap_rx.recv() {
                    inbox_snap.set_latest_mgr_snap(snap);
                }
            }));
            let inbox_ev = Arc::clone(&inbox);
            ev_fwd_handle = Some(thread::spawn(move || {
                while let Ok(ev) = ev_rx.recv() {
                    inbox_ev.push_event(ev);
                }
            }));
            if let Some(interval) = config.finder_poll_interval {
                let inbox_poll = Arc::clone(&inbox);
                let flag = Arc::clone(&stop_flag);
                finder_poll_handle = Some(thread::spawn(move || {
                    while !flag.load(Ordering::Relaxed) {
                        thread::sleep(interval);
                        if flag.load(Ordering::Relaxed) {
                            break;
                        }
                        inbox_poll.push_cmd(WorkerCmd::DrainFinderActions);
                    }
                }));
            }
            state.mgr = Some(Arc::new(mgr));
            if state
                .local_account_parents
                .contains_key(&state.wizard.selected_account)
            {
                refresh_real_catalog(&mut state);
            }
            emit_snapshot(&mut state, None, &ui_slot, &latest_cached);
        }
        Err(err) => {
            state.action_error = Some(format!("{} {}", err.cause(), err.next_step()));
            emit_snapshot(&mut state, None, &ui_slot, &latest_cached);
        }
    }

    let mut skip_helper_joins = false;
    'worker: while let Some((cmds, latest_mgr_snap, events)) = inbox.take_batch() {
        let mut need_emit = false;
        let mut explicit_snap: Option<Arc<StatusSnapshot>> = None;

        // 1. ALWAYS process commands FIRST (Stop/Quit/UI actions never wait behind logs, HIGH #14).
        for cmd in cmds {
            match cmd {
                WorkerCmd::Command(ui_cmd) => {
                    let check_local_path = matches!(&ui_cmd, UiCommand::WizardSetLocalPath(_));
                    let (nav, sys_req) = handle_ui_command(&mut state, ui_cmd);
                    if check_local_path {
                        if !state.wizard.local_path.is_empty() {
                            // Publish the path without waiting on any of the inventory probes.
                            let pending = {
                                let mut cached = latest_cached.lock().unwrap();
                                cached.wizard = state.wizard.clone();
                                cached.clone()
                            };
                            ui_slot.publish_snapshot(pending);
                        }
                        recompute_real_wizard_inventory(&mut state);
                    }
                    need_emit = true;
                    if let Some(route) = nav {
                        ui_slot.push_control(UiControlEvent::Navigate(route));
                    }
                    if let Some((request_id, request)) = sys_req {
                        ui_slot.push_control(UiControlEvent::SystemAction {
                            request_id,
                            request,
                        });
                    }
                }
                WorkerCmd::AuthorizationReady(url) => {
                    if state.authorization_canceller.is_none() {
                        continue;
                    }
                    state.authorization_url = Some(url.clone());
                    if let Some((request_id, request)) = dispatch_system_action(
                        &mut state,
                        SystemActionRequest::OpenDriveAuthorization(url),
                    ) {
                        ui_slot.push_control(UiControlEvent::SystemAction {
                            request_id,
                            request,
                        });
                    }
                    need_emit = true;
                }
                WorkerCmd::AccountOperationFinished(result) => {
                    if let Some(worker) = state.account_worker.take() {
                        let _ = worker.join();
                    }
                    let operation_id = state.account_operation_id.take();
                    let was_authorizing = state.reconnecting_account.take().is_some();
                    state.authorization_canceller = None;
                    state.authorization_url = None;
                    match result {
                        Ok(account) => {
                            state.unverified_accounts.remove(&account.id);
                            if state.wizard.step == WizardStep::Account
                                && !state
                                    .mgr
                                    .as_ref()
                                    .and_then(|mgr| mgr.list_accounts().ok())
                                    .is_some_and(|accounts| {
                                        accounts.iter().any(|a| {
                                            a.id == state.wizard.selected_account
                                                && a.auth_status == "ready"
                                        })
                                    })
                            {
                                state.wizard.selected_account = account.id.clone();
                            }
                            state.status_notice = Some(if account.kind == "drive" {
                                format!("Access checked for {}.", account.id)
                            } else {
                                format!("Imported the configuration for {}.", account.id)
                            });
                            if !was_authorizing && let Some(mgr) = &state.mgr {
                                for mapping in &mgr.snapshot().mappings {
                                    if mapping.account_id == account.id
                                        && mapping.readiness == Readiness::AuthenticationRequired
                                        && let Err(err) = mgr.check_again(mapping.id.clone())
                                    {
                                        state.action_error =
                                            Some(format!("{} {}", err.cause(), err.next_step()));
                                    }
                                }
                            }
                        }
                        Err(err) => {
                            if let Some(id) = operation_id {
                                state.unverified_accounts.insert(id);
                            }
                            state.action_error = Some(err);
                        }
                    }
                    need_emit = true;
                }
                WorkerCmd::SystemActionCompleted {
                    _request_id: _,
                    outcome,
                } => {
                    apply_system_action_outcome(&mut state, outcome);
                    need_emit = true;
                }
                WorkerCmd::DrainFinderActions => {
                    let mut ui_actions = Vec::new();
                    let mut had_finder_activity = false;
                    if let Some(mgr) = &state.mgr {
                        let dispatches = mgr.drain_finder_actions();
                        for d in dispatches {
                            had_finder_activity = true;
                            match d {
                                FinderActionDispatch::Executed { action, .. } => {
                                    let msg = match action {
                                        ValidatedFinderAction::SyncNow { mapping_id, .. } => {
                                            format!("Finder requested Sync now for {mapping_id}")
                                        }
                                        ValidatedFinderAction::Pause { mapping_id, .. } => {
                                            format!("Finder requested Pause for {mapping_id}")
                                        }
                                        _ => "Finder action executed".to_owned(),
                                    };
                                    push_ui_event(&mut state, msg, None);
                                }
                                FinderActionDispatch::UiRequested { action, .. } => {
                                    ui_actions.push(action);
                                }
                                FinderActionDispatch::CommandFailed { action, cause, .. } => {
                                    let label = plain_finder_action_label(&action);
                                    let msg = format!("{label} from Finder couldn't run: {cause}");
                                    state.action_error = Some(msg.clone());
                                    push_ui_event(&mut state, msg, None);
                                }
                                FinderActionDispatch::Rejected { reason, .. } => {
                                    let msg = format!(
                                        "FolderSync ignored an invalid Finder request: {reason}"
                                    );
                                    state.action_error = Some(msg.clone());
                                    push_ui_event(&mut state, msg, None);
                                }
                            }
                        }
                    }
                    if had_finder_activity {
                        need_emit = true;
                    }
                    for act in ui_actions {
                        match act {
                            ValidatedFinderAction::OpenApp { mapping_id, .. } => {
                                let route = match mapping_id {
                                    Some(id) => ScreenRoute::MappingDetail(id),
                                    None => ScreenRoute::Overview,
                                };
                                ui_slot.push_control(UiControlEvent::ShowWindowAndNavigate(route));
                            }
                            ValidatedFinderAction::ShowLastRun { mapping_id, .. } => {
                                if let Some(mgr) = &state.mgr
                                    && let Some(run_id) = mgr.last_run_id(&mapping_id)
                                {
                                    load_inspected_run(&mut state, &mapping_id, &run_id);
                                    emit_snapshot(&mut state, None, &ui_slot, &latest_cached);
                                    ui_slot.push_control(UiControlEvent::ShowWindowAndNavigate(
                                        ScreenRoute::RunInspection { mapping_id, run_id },
                                    ));
                                } else {
                                    ui_slot.push_control(UiControlEvent::ShowWindowAndNavigate(
                                        ScreenRoute::MappingDetail(mapping_id),
                                    ));
                                }
                            }
                            _ => {}
                        }
                    }
                }
                WorkerCmd::ControlledShutdown { timeout } => {
                    if state.account_worker.is_some() && state.reconnecting_account.is_none() {
                        state.action_error = Some(
                            "Finish the account operation before quitting FolderSync.".to_owned(),
                        );
                        ui_slot.push_control(UiControlEvent::QuitDeferred);
                        need_emit = true;
                        continue;
                    }
                    if let Some(mgr) = state.mgr.take() {
                        match mgr.controlled_shutdown(timeout) {
                            Ok(outcome) => {
                                stop_flag.store(true, Ordering::Relaxed);
                                let timed_out = matches!(
                                    outcome,
                                    ControlledShutdownOutcome::TimedOutInterrupted { .. }
                                );
                                // The manager already requested cancellation and gave sign-in a
                                // bounded cleanup window. A slow credential backend must not hold Quit.
                                let authorization_still_finishing =
                                    state.reconnecting_account.is_some()
                                        && state
                                            .account_worker
                                            .as_ref()
                                            .is_some_and(|worker| !worker.is_finished());
                                if authorization_still_finishing {
                                    drop(state.account_worker.take());
                                }
                                if timed_out || authorization_still_finishing {
                                    timed_out_shutdown.store(true, Ordering::SeqCst);
                                    skip_helper_joins = true;
                                }
                                // Send QuitReady BEFORE dropping SyncManager / joining threads (HIGH #6).
                                ui_slot.push_control(UiControlEvent::QuitReady(outcome));
                                drop(mgr);
                                break 'worker;
                            }
                            Err(err) => {
                                // Surface timeout persistence error; do NOT send QuitReady (HIGH #6).
                                state.mgr = Some(mgr);
                                state.action_error =
                                    Some(format!("{} {}", err.cause(), err.next_step()));
                                need_emit = true;
                            }
                        }
                    } else {
                        stop_flag.store(true, Ordering::Relaxed);
                        ui_slot.push_control(UiControlEvent::QuitReady(
                            ControlledShutdownOutcome::Clean,
                        ));
                        break 'worker;
                    }
                }
                WorkerCmd::ShutdownNow => {
                    stop_flag.store(true, Ordering::Relaxed);
                    if state.reconnecting_account.is_some()
                        && let Some(mgr) = &state.mgr
                    {
                        let _ = mgr.controlled_shutdown(config.shutdown_timeout);
                    }
                    if let Some(worker) = state.account_worker.take() {
                        let _ = worker.join();
                    }
                    if let Some(mgr) = state.mgr.take().and_then(Arc::into_inner) {
                        let _ = mgr.shutdown();
                    }
                    break 'worker;
                }
            }
        }

        // 2. Process manager events (without rebuilding snapshot on every log line, HIGH #14).
        for ev in events {
            if handle_manager_event(&mut state, ev) {
                need_emit = true;
            }
        }

        // 3. Apply the single newest manager snapshot (coalesced, never stale).
        if let Some(snap) = latest_mgr_snap {
            update_folder_size(&mut state);
            if let Some(ref mid) = state.wizard.first_sync_mapping_id
                && let Some(m) = snap.mappings.iter().find(|m| &m.id == mid)
            {
                if m.readiness == Readiness::Ready && m.execution == Execution::Idle {
                    if state.wizard.first_sync_preflight_done {
                        state.wizard.first_sync_completed = true;
                        state.wizard.first_sync_stopping = false;
                        state.wizard.first_sync_percent = 100;
                    }
                    if let Some(started) = state.first_sync_started_at {
                        state.wizard.first_sync_elapsed_secs = started.elapsed().as_secs();
                    }
                } else if m.stopped_first_sync.is_some() {
                    state.wizard.first_sync_stopping = false;
                } else if state.wizard.first_sync_stopping
                    && m.execution == Execution::Idle
                    && m.readiness == Readiness::NeedsInitialization
                {
                    show_stopped_first_sync(&mut state.wizard);
                }
            }
            explicit_snap = Some(snap);
            need_emit = true;
        }

        if need_emit {
            emit_snapshot(
                &mut state,
                explicit_snap.as_deref(),
                &ui_slot,
                &latest_cached,
            );
        }
    }

    stop_flag.store(true, Ordering::Relaxed);
    inbox.close();
    ui_slot.close();
    drop(state.mgr.take());
    if let Some(worker) = state.account_worker.take() {
        let _ = worker.join();
    }
    if !skip_helper_joins {
        if let Some(h) = snap_fwd_handle {
            let _ = h.join();
        }
        if let Some(h) = ev_fwd_handle {
            let _ = h.join();
        }
        if let Some(h) = finder_poll_handle {
            let _ = h.join();
        }
    }
}

fn plain_finder_action_label(action: &ValidatedFinderAction) -> &'static str {
    match action {
        ValidatedFinderAction::SyncNow { .. } => "Sync now",
        ValidatedFinderAction::Pause { .. } => "Pause",
        ValidatedFinderAction::OpenApp { .. } => "Open FolderSync",
        ValidatedFinderAction::ShowLastRun { .. } => "Show last run",
    }
}

/// Handles one manager event and returns `true` iff a UI snapshot rebuild is needed.
/// `RunOutputLine` appends to `raw_log` WITHOUT triggering a full snapshot rebuild (HIGH #14).
fn handle_manager_event(state: &mut WorkerState, ev: ManagerEvent) -> bool {
    match ev {
        ManagerEvent::SnapshotUpdated { .. } => false,
        ManagerEvent::MappingAdded { mapping_id, .. } => {
            state.recent_runs_dirty = true;
            push_ui_event(
                state,
                format!("Added mapping `{mapping_id}`."),
                Some(("Open".to_owned(), ScreenRoute::MappingDetail(mapping_id))),
            );
            true
        }
        ManagerEvent::RunStarted {
            mapping_id,
            run_id,
            mode,
            ..
        } => {
            state.raw_log.clear();
            state.recent_runs_dirty = true;
            state.live_progress = Some(LiveRunProgress {
                mapping_id: mapping_id.clone(),
                run_id: run_id.clone(),
                started_at: Instant::now(),
                bytes_transferred: 0,
                total_bytes: None,
            });
            if state.wizard.first_sync_mapping_id.as_ref() == Some(&mapping_id) {
                state.wizard.first_sync_preflight_done = true;
                state.wizard.first_sync_markers_ready = true;
                state.first_sync_started_at = Some(Instant::now());
            }
            push_ui_event(
                state,
                format!("Started {mode} run for `{mapping_id}`."),
                Some((
                    "Show run".to_owned(),
                    ScreenRoute::RunInspection { mapping_id, run_id },
                )),
            );
            true
        }
        ManagerEvent::RunProgress {
            mapping_id,
            run_id,
            bytes_transferred,
            total_bytes,
            files_transferred,
            files_already_present,
            first_sync_total_files,
            ..
        } => {
            match state.live_progress.as_mut() {
                Some(p) if p.mapping_id == mapping_id && p.run_id == run_id => {
                    p.bytes_transferred = p.bytes_transferred.max(bytes_transferred);
                    if total_bytes.is_some() {
                        p.total_bytes = total_bytes;
                    }
                }
                _ => {
                    state.live_progress = Some(LiveRunProgress {
                        mapping_id: mapping_id.clone(),
                        run_id,
                        started_at: Instant::now(),
                        bytes_transferred,
                        total_bytes,
                    });
                }
            }
            if state.wizard.first_sync_mapping_id.as_ref() == Some(&mapping_id)
                && let Some(started) = state.first_sync_started_at
            {
                state.wizard.first_sync_elapsed_secs = started.elapsed().as_secs();
                if let Some(total) = first_sync_total_files {
                    let done = files_already_present
                        .saturating_add(files_transferred)
                        .min(total);
                    let prior = state
                        .wizard
                        .first_sync_file_progress
                        .map_or(0, |(done, _)| done);
                    state.wizard.first_sync_file_progress = Some((prior.max(done), total));
                }
            }
            true
        }
        ManagerEvent::RunOutputLine { line, .. } => {
            state.raw_log.push(line);
            if state.raw_log.len() > DEFAULT_RUN_LOG_MAX_LINES {
                let excess = state.raw_log.len() - DEFAULT_RUN_LOG_MAX_LINES;
                state.raw_log.drain(0..excess);
            }
            // Do NOT trigger a full snapshot rebuild or DB query on every log line (HIGH #14).
            false
        }
        ManagerEvent::RunFinished {
            mapping_id,
            run_id,
            outcome,
            ..
        } => {
            state.recent_runs_dirty = true;
            if state
                .live_progress
                .as_ref()
                .is_some_and(|p| p.mapping_id == mapping_id)
            {
                state.live_progress = None;
            }
            if state.wizard.first_sync_mapping_id.as_ref() == Some(&mapping_id) {
                if let Some(started) = state.first_sync_started_at {
                    state.wizard.first_sync_elapsed_secs = started.elapsed().as_secs();
                }
                if outcome == "success" {
                    state.wizard.first_sync_completed = true;
                    state.wizard.first_sync_stopping = false;
                    state.wizard.first_sync_percent = 100;
                } else if matches!(
                    outcome.as_str(),
                    "first_sync_stopped" | "canceled_before_spawn"
                ) {
                    state.wizard.first_sync_stopping = false;
                } else if outcome != "preflight_failed" {
                    // Surface terminal engine failures in the wizard (HIGH #4).
                    state.wizard.first_sync_stopping = false;
                    state.wizard.first_sync_error = Some(format!(
                        "First sync stopped before completing ({outcome}). Review the run log before trying again."
                    ));
                }
            }
            push_ui_event(
                state,
                format!("Finished run for `{mapping_id}` ({outcome})."),
                Some((
                    "Show run".to_owned(),
                    ScreenRoute::RunInspection { mapping_id, run_id },
                )),
            );
            true
        }
        ManagerEvent::RecoveryOpened {
            mapping_id, cause, ..
        } => {
            state.recent_runs_dirty = true;
            if state.wizard.first_sync_mapping_id.as_ref() == Some(&mapping_id)
                && !state.wizard.first_sync_stopping
                && state.wizard.first_sync_error.is_none()
            {
                state.wizard.first_sync_error = Some(cause.clone());
            }
            push_ui_event(
                state,
                format!("Recovery needed for `{mapping_id}`: {cause}"),
                Some((
                    "Start recovery…".to_owned(),
                    ScreenRoute::Recovery(mapping_id),
                )),
            );
            true
        }
        ManagerEvent::PreflightFailed {
            mapping_id,
            cause,
            next_step,
            ..
        } => {
            let full = format!("{cause} {next_step}");
            if state.wizard.first_sync_mapping_id.as_ref() == Some(&mapping_id) {
                state.wizard.first_sync_error = Some(full.clone());
                state.wizard.first_sync_stopping = false;
            }
            state.action_error = Some(full);
            push_ui_event(state, format!("First sync check failed: {cause}"), None);
            true
        }
        ManagerEvent::ShutdownTimedOut { mapping_id, .. } => {
            state.recent_runs_dirty = true;
            push_ui_event(
                state,
                format!(
                    "Quit timed out while stopping `{mapping_id}`; recovery required on restart."
                ),
                None,
            );
            true
        }
    }
}

fn dispatch_system_action(
    state: &mut WorkerState,
    request: SystemActionRequest,
) -> Option<(u64, SystemActionRequest)> {
    if let Some(sys) = state.system_actions.clone() {
        let outcome = match request {
            SystemActionRequest::RevealPath(ref path) => {
                SystemActionOutcome::RevealPath(sys.reveal_path(path))
            }
            SystemActionRequest::OpenDriveAuthorization(ref url) => {
                SystemActionOutcome::OpenUrl(if is_drive_authorization_url(url.expose()) {
                    sys.open_drive_authorization(url.expose())
                } else {
                    Err("Could not open the Google Drive sign-in page.".to_owned())
                })
            }
            SystemActionRequest::OpenUrl(ref url) => {
                SystemActionOutcome::OpenUrl(sys.open_url(url))
            }
            SystemActionRequest::SetLaunchAtLogin(enabled) => {
                SystemActionOutcome::SetLaunchAtLogin(sys.set_launch_at_login(enabled))
            }
            SystemActionRequest::Notify {
                ref id,
                ref title,
                ref body,
            } => SystemActionOutcome::Notify(sys.notify(id, title, body)),
        };
        apply_system_action_outcome(state, outcome);
        None
    } else {
        let req_id = state.next_sys_action_id;
        state.next_sys_action_id += 1;
        Some((req_id, request))
    }
}

fn apply_system_action_outcome(state: &mut WorkerState, outcome: SystemActionOutcome) {
    match outcome {
        SystemActionOutcome::RevealPath(Ok(())) | SystemActionOutcome::OpenUrl(Ok(())) => {}
        SystemActionOutcome::RevealPath(Err(err)) | SystemActionOutcome::OpenUrl(Err(err)) => {
            state.action_error = Some(err);
        }
        SystemActionOutcome::SetLaunchAtLogin(Ok(LaunchAtLoginOutcome::Enabled)) => {
            state.preferences.open_at_login = true;
            state.preferences.open_at_login_pending = false;
        }
        SystemActionOutcome::SetLaunchAtLogin(Ok(LaunchAtLoginOutcome::PendingApproval)) => {
            state.preferences.open_at_login = true;
            state.preferences.open_at_login_pending = true;
        }
        SystemActionOutcome::SetLaunchAtLogin(Ok(LaunchAtLoginOutcome::Disabled)) => {
            state.preferences.open_at_login = false;
            state.preferences.open_at_login_pending = false;
        }
        SystemActionOutcome::SetLaunchAtLogin(Ok(LaunchAtLoginOutcome::Unavailable)) => {
            state.preferences.open_at_login = false;
            state.preferences.open_at_login_pending = false;
            state.status_notice =
                Some("Open at login is available only in the packaged FolderSync app.".to_owned());
        }
        SystemActionOutcome::SetLaunchAtLogin(Err(err)) => {
            state.action_error = Some(err);
        }
        SystemActionOutcome::Notify(Ok(_)) => {}
        SystemActionOutcome::Notify(Err(err)) => {
            state.action_error = Some(err);
        }
    }
}

fn handle_ui_command(
    state: &mut WorkerState,
    cmd: UiCommand,
) -> (Option<ScreenRoute>, Option<(u64, SystemActionRequest)>) {
    if !matches!(cmd, UiCommand::WizardLeave) {
        state.status_notice = None;
        state.action_error = None;
    }

    match cmd {
        UiCommand::DismissError => {
            state.action_error = None;
            (None, None)
        }
        UiCommand::PollFinderActions => (None, None),
        UiCommand::SyncNow(id) => {
            if let Some(mgr) = &state.mgr {
                record_mgr_result(state, mgr.run_now(id));
            }
            (None, None)
        }
        UiCommand::SyncReadyMappings => {
            if let Some(mgr) = &state.mgr {
                record_mgr_result(state, mgr.sync_ready_mappings());
            }
            (None, None)
        }
        UiCommand::PauseMapping(id) => {
            if let Some(mgr) = &state.mgr {
                record_mgr_result(state, mgr.pause(id));
            }
            (None, None)
        }
        UiCommand::ResumeMapping(id) => {
            if let Some(mgr) = &state.mgr {
                record_mgr_result(state, mgr.resume(id));
            }
            (None, None)
        }
        UiCommand::ResumeFirstSync(id) => {
            let Some(mgr) = &state.mgr else {
                return (None, None);
            };
            match mgr.resume_first_sync(id.clone()) {
                Ok(acceptance) => {
                    if let Some(mapping) = acceptance.snapshot.mappings.iter().find(|m| m.id == id)
                    {
                        let direction = mapping
                            .last_run_id
                            .as_ref()
                            .and_then(|rid| mgr.get_run(rid).ok().flatten())
                            .map(|run| run.mode);
                        state.wizard = default_real_wizard();
                        state.wizard.editing_mapping_id = Some(id.clone());
                        state.wizard.selected_account = mapping.account_id.clone();
                        state.wizard.selected_folder = mapping.display_name.clone();
                        state.wizard.volume_name = mapping.volume_name.clone();
                        state.wizard.review_scenario = match direction.as_deref() {
                            Some("init_path2") => ReviewScenario::CloudToLocal,
                            _ => ReviewScenario::LocalToCloud,
                        };
                        begin_first_sync(state, id);
                    }
                    return (Some(ScreenRoute::AddMapping), None);
                }
                Err(err) => {
                    state.action_error = Some(format!("{} {}", err.cause(), err.next_step()))
                }
            }
            (None, None)
        }
        UiCommand::StopRun(id) => {
            if let Some(mgr) = &state.mgr {
                record_mgr_result(state, mgr.stop_run(id));
            }
            (None, None)
        }
        UiCommand::ForceStopRun(id) => {
            if let Some(mgr) = &state.mgr {
                record_mgr_result(state, mgr.force_stop(id));
            }
            (None, None)
        }
        UiCommand::SetSchedulingPaused(paused) => {
            if let Some(mgr) = &state.mgr {
                record_mgr_result(state, mgr.set_scheduling_paused(paused));
            }
            (None, None)
        }
        UiCommand::MarkConflictsReviewed(id) => {
            if let Some(mgr) = &state.mgr {
                record_mgr_result(state, mgr.acknowledge_conflicts(id));
            }
            (None, None)
        }
        UiCommand::CheckAgain(id) => {
            if let Some(mgr) = &state.mgr {
                record_mgr_result(state, mgr.check_again(id));
            }
            (None, None)
        }
        UiCommand::InspectRecovery(id) => {
            load_recovery_checklist(state, &id);
            (None, None)
        }
        UiCommand::InspectRun { mapping_id, run_id } => {
            load_inspected_run(state, &mapping_id, &run_id);
            (None, None)
        }
        UiCommand::CopyRecoveryCopyToInspection {
            run_id,
            is_remote,
            relative_path,
            dest_dir,
        } => {
            let side = if is_remote {
                RecoveryCopySide::Remote
            } else {
                RecoveryCopySide::Local
            };
            if let Some(mgr) = &state.mgr {
                match mgr.copy_recovery_file_to_inspection(
                    &run_id,
                    side,
                    &relative_path,
                    Path::new(&dest_dir),
                ) {
                    Ok(copied_path) => {
                        let note = format!(
                            "Copied `{relative_path}` to `{}` for inspection. Review it there before moving it into a synced folder yourself.",
                            copied_path.display()
                        );
                        if let Some(insp) = &mut state.inspected_run
                            && insp.run_id == run_id
                        {
                            insp.copy_notice = Some(note.clone());
                        }
                        state.status_notice = Some(note.clone());
                        push_ui_event(state, note, None);
                    }
                    Err(err) => {
                        state.action_error = Some(format!("{} {}", err.cause(), err.next_step()));
                    }
                }
            }
            (None, None)
        }
        UiCommand::CompleteRecovery {
            mapping_id,
            resume_and_run,
        } => {
            if let Some(mgr) = &state.mgr {
                match mgr.complete_recovery(mapping_id.clone(), resume_and_run) {
                    Ok(_) => {
                        state.recovery_checklist = None;
                        state.recent_runs_dirty = true;
                        return (Some(ScreenRoute::Overview), None);
                    }
                    Err(err) => {
                        state.action_error = Some(format!("{} {}", err.cause(), err.next_step()));
                    }
                }
            }
            (None, None)
        }
        UiCommand::RemoveMapping(id) | UiCommand::DiscardDraft(id) => {
            if let Some(mgr) = &state.mgr {
                match mgr.remove_mapping(id) {
                    Ok(_) => state.recent_runs_dirty = true,
                    Err(err) => {
                        state.action_error = Some(format!("{} {}", err.cause(), err.next_step()));
                    }
                }
            }
            (None, None)
        }
        UiCommand::ResumeDraft(id) => {
            if load_mapping_into_wizard(state, &id, WizardStep::Location) {
                return (Some(ScreenRoute::AddMapping), None);
            }
            (None, None)
        }
        UiCommand::FinishSetup(id) => {
            if load_mapping_into_wizard(state, &id, WizardStep::Review) {
                return (Some(ScreenRoute::AddMapping), None);
            }
            (None, None)
        }
        UiCommand::ChangeSubfolders(id) => {
            let ready = state.mgr.as_ref().is_some_and(|mgr| {
                mgr.snapshot().mappings.iter().any(|m| {
                    m.id == id
                        && m.active_generation_id.is_some()
                        && m.readiness == Readiness::Ready
                        && matches!(m.execution, Execution::Idle | Execution::Queued)
                })
            });
            if ready && load_mapping_into_wizard(state, &id, WizardStep::Subfolders) {
                (Some(ScreenRoute::AddMapping), None)
            } else {
                state.action_error = Some(
                    "Wait for this mapping to be ready before changing its subfolders.".to_owned(),
                );
                (None, None)
            }
        }
        UiCommand::PreviewFilterChange => {
            if state.wizard.changing_subfolders
                && state.wizard.step == WizardStep::Subfolders
                && let Some(id) = state.wizard.editing_mapping_id.clone()
                && let Some(mgr) = &state.mgr
            {
                invalidate_wizard_confirmation(&mut state.wizard);
                match mgr.propose_filter_change(&id, state.wizard.extra_exclusions()) {
                    Ok(preview) => {
                        state.wizard.filter_review = Some(project_filter_review(
                            &preview,
                            state.wizard.volume_free_bytes,
                        ));
                        state.filter_preview = Some(preview);
                        state.wizard.step = WizardStep::Review;
                    }
                    Err(err) => {
                        state.action_error = Some(format!("{} {}", err.cause(), err.next_step()))
                    }
                }
            }
            (None, None)
        }
        UiCommand::ApplyFilterChange => {
            if state.wizard.changing_subfolders
                && state.wizard.step == WizardStep::Review
                && state.wizard.no_other_sync_confirmed
                && state
                    .wizard
                    .filter_review
                    .as_ref()
                    .is_some_and(FilterReview::can_apply)
                && let Some(mgr) = &state.mgr
                && let Some(preview) = state.filter_preview.take()
            {
                let id = preview.mapping_id().clone();
                let preflight = preview.preflight();
                let local = preflight.local_inventory.as_ref().ok();
                let remote = preflight.remote_inventory.as_ref().ok();
                state.wizard.review_scenario = match (remote, local) {
                    (Some(r), _) if r.classification == SideClassification::HasData => {
                        ReviewScenario::CloudToLocal
                    }
                    (_, Some(l)) if l.classification == SideClassification::HasData => {
                        ReviewScenario::LocalToCloud
                    }
                    _ => ReviewScenario::BothEmpty,
                };
                match mgr.apply_filter_change(preview) {
                    Ok(_) => begin_first_sync(state, id),
                    Err(err) => {
                        invalidate_wizard_confirmation(&mut state.wizard);
                        state.wizard.step = WizardStep::Subfolders;
                        state.action_error = Some(format!("{} {}", err.cause(), err.next_step()));
                    }
                }
            }
            (None, None)
        }
        UiCommand::SetScheduleInterval {
            mapping_id,
            minutes,
        } => {
            if let Some(mgr) = &state.mgr {
                record_mgr_result(
                    state,
                    mgr.set_schedule_interval(mapping_id, u64::from(minutes) * 60),
                );
            }
            (None, None)
        }
        UiCommand::ConnectDemoAccount(_) => (None, None),
        UiCommand::CancelReconnect => {
            if let Some(canceller) = state.authorization_canceller.take() {
                state.authorization_url = None;
                canceller.cancel();
            }
            (None, None)
        }
        UiCommand::ConnectDrive {
            account_id,
            display_name,
            root_folder_id,
            client_id,
            client_secret,
        } => {
            start_account_operation(
                state,
                AccountOperation::Authorize(DriveAuthorization::Connect(AddDriveAccountRequest {
                    remote_name: account_id.to_string(),
                    id: account_id,
                    display_name,
                    root_folder_id,
                    client_id,
                    client_secret,
                    scope: "drive".to_owned(),
                    run_oauth_flow: true,
                })),
            );
            (None, None)
        }
        UiCommand::ImportDrive {
            account_id,
            display_name,
            source_config_path,
            source_remote_name,
            root_folder_id,
        } => {
            start_account_operation(
                state,
                AccountOperation::Import(ImportAccountRequest {
                    id: account_id,
                    display_name,
                    target_remote_name: source_remote_name.clone(),
                    source_remote_name,
                    source_config_path,
                    root_folder_id,
                    allow_global_default_source: false,
                }),
            );
            (None, None)
        }
        UiCommand::ReopenDriveAuthorization => {
            let req = state.authorization_url.clone().and_then(|url| {
                dispatch_system_action(state, SystemActionRequest::OpenDriveAuthorization(url))
            });
            (None, req)
        }
        UiCommand::AddLocalFixtureAccount {
            id,
            display_name,
            parent_dir,
        } => {
            if state.account_worker.is_some() {
                state.action_error = Some("Wait for the account operation to finish.".to_owned());
                return (None, None);
            }
            if let Some(mgr) = &state.mgr {
                let acct_id = AccountId::new(id.trim());
                let trimmed_parent = parent_dir.trim();
                let parent_opt = (!trimmed_parent.is_empty()).then(|| Path::new(trimmed_parent));
                match mgr.register_local_fixture_account_with_root(
                    acct_id.clone(),
                    display_name,
                    parent_opt,
                ) {
                    Ok(rec) => {
                        let stored_parent = PathBuf::from(&rec.remote_root_folder_id);
                        if stored_parent.is_absolute() && stored_parent.is_dir() {
                            state
                                .local_account_parents
                                .insert(acct_id.clone(), stored_parent);
                        }
                        state.wizard.selected_account = acct_id;
                        refresh_real_catalog(state);
                    }
                    Err(err) => {
                        state.action_error = Some(format!("{} {}", err.cause(), err.next_step()));
                    }
                }
            }
            (None, None)
        }
        UiCommand::ReconnectAccount(id) => {
            start_account_operation(
                state,
                AccountOperation::Authorize(DriveAuthorization::Reconnect(id)),
            );
            (None, None)
        }
        UiCommand::CheckAccountAccess(id) => {
            start_account_operation(state, AccountOperation::Check(id));
            (None, None)
        }
        UiCommand::RemoveAccount(account_id) => {
            if state.account_worker.is_some() {
                state.action_error = Some("Wait for the account operation to finish.".to_owned());
                return (None, None);
            }
            if let Some(mgr) = &state.mgr {
                match mgr.remove_account(account_id.clone()) {
                    Ok(_) => {
                        state.local_account_parents.remove(&account_id);
                    }
                    Err(err) => {
                        state.action_error = Some(format!("{} {}", err.cause(), err.next_step()));
                    }
                }
            }
            (None, None)
        }
        UiCommand::SetOpenAtLogin(enabled) => {
            let req = dispatch_system_action(state, SystemActionRequest::SetLaunchAtLogin(enabled));
            (None, req)
        }
        UiCommand::SetNotifications(_) => {
            state.status_notice =
                Some("Desktop notifications are not connected yet in this build.".to_owned());
            (None, None)
        }
        UiCommand::SetBandwidthLimit(_) => {
            state.status_notice = Some(
                "Bandwidth limits are not connected to sync runs in this build yet.".to_owned(),
            );
            (None, None)
        }
        UiCommand::WizardReset => {
            cancel_folder_size(state);
            state.filter_preview = None;
            state.wizard = default_real_wizard();
            state.first_sync_started_at = None;
            if let Some(mgr) = &state.mgr
                && let Ok(accounts) = mgr.list_accounts()
            {
                let mut usable = accounts.iter().filter(|a| {
                    a.auth_status == "ready"
                        && matches!(a.kind.as_str(), "drive" | "local_fixture")
                        && !state.unverified_accounts.contains(&a.id)
                });
                if let Some(first) = usable.next() {
                    state.wizard.selected_account = first.id.clone();
                    if usable.next().is_none() {
                        state.wizard.step = WizardStep::Folder;
                    }
                }
            }
            clear_cloud_catalog(state);
            if state.wizard.step == WizardStep::Folder
                || state
                    .local_account_parents
                    .contains_key(&state.wizard.selected_account)
            {
                refresh_real_catalog(state);
            }
            recompute_real_wizard_inventory(state);
            (None, None)
        }
        UiCommand::WizardSetStep(step) => {
            if state.wizard.changing_subfolders {
                if step == WizardStep::Subfolders && state.wizard.step == WizardStep::Review {
                    state.filter_preview = None;
                    invalidate_wizard_confirmation(&mut state.wizard);
                    state.wizard.step = step;
                }
                return (None, None);
            }
            if state.wizard.can_visit_step(step)
                && (step < WizardStep::Folder
                    || state
                        .mgr
                        .as_ref()
                        .and_then(|mgr| mgr.list_accounts().ok())
                        .is_some_and(|accounts| {
                            accounts.iter().any(|account| {
                                account.id == state.wizard.selected_account
                                    && account.auth_status == "ready"
                                    && !state.unverified_accounts.contains(&account.id)
                            })
                        }))
                && (step < WizardStep::Location || state.wizard.has_cloud_folder())
            {
                let entering_folder = step == WizardStep::Folder && state.wizard.step != step;
                if step != WizardStep::Folder {
                    cancel_folder_size(state);
                }
                state.wizard.step = step;
                if entering_folder && state.cloud_catalog.is_empty() {
                    refresh_real_catalog(state);
                } else if step >= WizardStep::Location {
                    recompute_real_wizard_inventory(state);
                    if !drive_size_holds_wizard_account(state)
                        && let Err(err) = persist_real_draft_if_ready(state, false)
                    {
                        state.action_error = Some(err);
                    }
                }
            }
            (None, None)
        }
        UiCommand::WizardSelectAccount(acct) => {
            if state.wizard.selected_account != acct {
                cancel_folder_size(state);
                clear_cloud_catalog(state);
                state.wizard.selected_folder.clear();
                state.wizard.remote_folder_path = None;
                state.wizard.drive_collection = None;
                state.wizard.subfolders.clear();
            }
            state.wizard.selected_account = acct;
            if state
                .local_account_parents
                .contains_key(&state.wizard.selected_account)
            {
                refresh_real_catalog(state);
            }
            invalidate_wizard_confirmation(&mut state.wizard);
            (None, None)
        }
        UiCommand::WizardSelectFolder(folder) => {
            if let Some(candidate) = state
                .cloud_catalog
                .iter()
                .find(|item| item.name == folder)
                .cloned()
                && candidate.on_this_mac.is_none()
            {
                select_cloud_folder(state, candidate);
            }
            (None, None)
        }
        UiCommand::WizardCreateNewFolder(name) => {
            if state.wizard.step == WizardStep::Folder
                && state.pending_folder_size.is_none()
                && let Some(mgr) = &state.mgr
            {
                match mgr.create_collection_folder(&state.wizard.selected_account, name.trim()) {
                    Ok(candidate) => {
                        let item = project_drive_candidate(state, candidate);
                        state.cloud_catalog.push(item.clone());
                        state.cloud_catalog.sort_by(|a, b| a.name.cmp(&b.name));
                        select_cloud_folder(state, item);
                    }
                    Err(err) => {
                        state.action_error = Some(format!("{} {}", err.cause(), err.next_step()))
                    }
                }
            }
            (None, None)
        }
        UiCommand::WizardRefreshFolders => {
            refresh_real_catalog(state);
            (None, None)
        }
        UiCommand::WizardCancelFolderSize | UiCommand::WizardLeave => {
            cancel_folder_size(state);
            (None, None)
        }
        UiCommand::WizardSetRemotePath(path) => {
            if state
                .mgr
                .as_ref()
                .and_then(|mgr| mgr.list_accounts().ok())
                .is_some_and(|accounts| {
                    accounts
                        .iter()
                        .any(|a| a.id == state.wizard.selected_account && a.kind != "local_fixture")
                })
            {
                state.action_error =
                    Some("Choose a local test folder account to use a test folder.".to_owned());
                return (None, None);
            }
            let trimmed = path.trim();
            let candidate = PathBuf::from(trimmed);
            if let Some(parent) = state
                .local_account_parents
                .get(&state.wizard.selected_account)
            {
                let valid_child = fs::canonicalize(&candidate)
                    .ok()
                    .and_then(|c| {
                        let cp = fs::canonicalize(parent).ok()?;
                        (c.parent() == Some(cp.as_path())).then_some(c)
                    })
                    .is_some();
                if !valid_child {
                    state.action_error = Some(format!(
                        "Choose a direct subfolder inside `{}` so collections never overlap.",
                        parent.display()
                    ));
                    return (None, None);
                }
            }
            if let Some(name) = candidate.file_name().and_then(|n| n.to_str()) {
                state.wizard.selected_folder = name.to_owned();
            }
            state.wizard.remote_folder_path = Some(trimmed.to_owned());
            invalidate_wizard_confirmation(&mut state.wizard);
            recompute_real_wizard_inventory(state);
            (None, None)
        }
        UiCommand::WizardSetLocalPath(path) => {
            let p = PathBuf::from(path.trim());
            state.wizard.local_path = path.trim().to_owned();
            state.wizard.location_check_pending = !state.wizard.local_path.is_empty();
            state.wizard.location_checks.clear();
            state.wizard.volume_free_bytes = None;
            if state.wizard.local_recovery_path.is_empty()
                && let Some(parent) = p.parent()
                && let Some(name) = p.file_name().and_then(|n| n.to_str())
            {
                state.wizard.local_recovery_path = parent
                    .join("FolderSyncRecovery")
                    .join(name)
                    .to_string_lossy()
                    .into_owned();
                if state.wizard.drive_collection.is_none() {
                    state.wizard.cloud_recovery_path = parent
                        .join("FolderSyncRecovery-Remote")
                        .join(name)
                        .to_string_lossy()
                        .into_owned();
                }
            }
            invalidate_wizard_confirmation(&mut state.wizard);
            (None, None)
        }
        UiCommand::DemoPreviewLocationCheck(_) => (None, None),
        UiCommand::WizardToggleSubfolder(sub_id) => {
            if sub_id == "root" {
                let next = !state
                    .wizard
                    .subfolders
                    .first()
                    .map(|n| n.included)
                    .unwrap_or(true);
                for node in &mut state.wizard.subfolders {
                    node.included = next;
                }
            } else if let Some(node) = state.wizard.subfolders.iter_mut().find(|n| n.id == sub_id) {
                node.included = !node.included;
            }
            wizard_exclusions_changed(state);
            (None, None)
        }
        UiCommand::WizardToggleDefaultExclusions => {
            state.wizard.show_default_exclusions = !state.wizard.show_default_exclusions;
            (None, None)
        }
        UiCommand::WizardAddPattern(pattern) => {
            if state.wizard.add_custom_pattern(&pattern) {
                wizard_exclusions_changed(state);
            }
            (None, None)
        }
        UiCommand::WizardRemovePattern(pattern) => {
            if state.wizard.remove_custom_pattern(&pattern) {
                wizard_exclusions_changed(state);
            }
            (None, None)
        }
        UiCommand::WizardSetLocalRecoveryPath(path) => {
            state.wizard.local_recovery_path = path.trim().to_owned();
            invalidate_wizard_confirmation(&mut state.wizard);
            recompute_real_wizard_inventory(state);
            (None, None)
        }
        UiCommand::WizardSetReviewScenario(_) | UiCommand::DemoFailFolderSize => (None, None),
        UiCommand::WizardSetNoOtherSyncConfirmed(confirmed) => {
            state.wizard.no_other_sync_confirmed = confirmed;
            state.wizard.reviewed_fingerprint = if confirmed {
                Some(wizard_review_fingerprint(&state.wizard))
            } else {
                None
            };
            (None, None)
        }
        UiCommand::WizardSaveDraft => {
            if state.wizard.changing_subfolders {
                return (None, None);
            }
            if let Err(err) = persist_real_draft_if_ready(state, true) {
                state.action_error = Some(err);
            }
            (None, None)
        }
        UiCommand::WizardStartFirstSync => {
            if state.wizard.changing_subfolders {
                return (None, None);
            }
            let prior_fp = state.wizard.reviewed_fingerprint.clone();
            recompute_real_wizard_inventory(state);
            let current_fp = wizard_review_fingerprint(&state.wizard);
            if prior_fp.as_deref() != Some(current_fp.as_str()) {
                // Inventory or direction changed since confirmation -> invalidate confirmation (HIGH #1).
                invalidate_wizard_confirmation(&mut state.wizard);
                state.action_error = Some(
                    "Folder contents or settings changed since you confirmed. Review the updated plan and confirm again."
                        .to_owned(),
                );
                return (None, None);
            }
            if state.wizard.download_exceeds_free_space() {
                state.action_error = Some("This selection exceeds available space. Leave out more subfolders or choose a volume with more free space.".to_owned());
                return (None, None);
            }
            if !state.wizard.can_start_first_sync() {
                state.action_error = Some(
                    "Finish the required checks and confirmation before starting the first sync."
                        .to_owned(),
                );
                return (None, None);
            }
            let add_cmd = match build_add_mapping_from_wizard(state) {
                Ok(cmd) => cmd,
                Err(err) => {
                    state.action_error = Some(err);
                    return (None, None);
                }
            };
            let mapping_id = add_cmd.id.clone();
            let expected_direction = match state.wizard.review_scenario {
                ReviewScenario::CloudToLocal => Some(InitPreference::Path2),
                ReviewScenario::LocalToCloud | ReviewScenario::BothEmpty => {
                    Some(InitPreference::Path1)
                }
                ReviewScenario::BothPopulated => {
                    state.action_error = Some(
                        "Both folders already contain files. Choose an empty local folder first."
                            .to_owned(),
                    );
                    return (None, None);
                }
            };

            if let Some(mgr) = &state.mgr {
                // ALWAYS persist all reviewed edits & exclusions before starting initialization (HIGH #1).
                if let Err(err) = mgr.add_mapping(add_cmd) {
                    state.action_error = Some(format!("{} {}", err.cause(), err.next_step()));
                    return (None, None);
                }
                match mgr.start_initialize(InitializeCommand {
                    mapping_id: mapping_id.clone(),
                    expected_direction,
                }) {
                    Ok(_) => begin_first_sync(state, mapping_id),
                    Err(err) => {
                        state.action_error = Some(format!("{} {}", err.cause(), err.next_step()));
                    }
                }
            }
            (None, None)
        }
        UiCommand::WizardStopFirstSync => {
            if let Some(mid) = state.wizard.first_sync_mapping_id.clone()
                && let Some(mgr) = &state.mgr
            {
                match mgr.stop_run(mid.clone()) {
                    Ok(acceptance) => {
                        let already_idle = acceptance
                            .snapshot
                            .mappings
                            .iter()
                            .find(|m| m.id == mid)
                            .is_some_and(|m| m.execution == Execution::Idle);
                        if already_idle {
                            show_stopped_first_sync(&mut state.wizard);
                        } else {
                            // Wait for the stop result before jumping back to Review (HIGH #4).
                            state.wizard.first_sync_stopping = true;
                        }
                    }
                    Err(err) => {
                        state.action_error = Some(format!("{} {}", err.cause(), err.next_step()));
                    }
                }
            }
            (None, None)
        }
        UiCommand::WizardCompleteDemoFirstSync => (None, None),
        UiCommand::RevealPath(path) => {
            let req =
                dispatch_system_action(state, SystemActionRequest::RevealPath(PathBuf::from(path)));
            (None, req)
        }
        UiCommand::OpenCloudUrl(url) => {
            let req = dispatch_system_action(state, SystemActionRequest::OpenUrl(url));
            (None, req)
        }
        UiCommand::OpenExtensionSettings => {
            let req = dispatch_system_action(
                state,
                SystemActionRequest::OpenUrl(
                    "x-apple.systempreferences:com.apple.ExtensionsPreferences".to_owned(),
                ),
            );
            (None, req)
        }
        UiCommand::ExportDiagnostics(dest_path) => {
            let report = build_redacted_diagnostics_text(state);
            let target = PathBuf::from(dest_path.trim());
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)
                .and_then(|mut f| {
                    use std::io::Write;
                    f.write_all(report.as_bytes())?;
                    f.sync_all()
                }) {
                Ok(()) => {
                    let msg = format!(
                        "Exported redacted diagnostics to {} (no file contents and no sign-in data).",
                        target.display()
                    );
                    state.status_notice = Some(msg.clone());
                    push_ui_event(state, msg, None);
                }
                Err(err) => {
                    state.action_error = Some(format!(
                        "Could not write diagnostics file `{}`: {err}. Choose a new filename in a writable folder.",
                        target.display()
                    ));
                }
            }
            (None, None)
        }
        UiCommand::DemoSetActivityPhase(_)
        | UiCommand::DemoSetPreset(_)
        | UiCommand::DemoStepMappingState(_)
        | UiCommand::DemoTickElapsed
        | UiCommand::DemoAction(_)
        | UiCommand::RequestControlledQuit => (None, None),
    }
}

fn start_account_operation(state: &mut WorkerState, operation: AccountOperation) {
    let Some(mgr) = state.mgr.clone() else { return };
    if state.account_worker.is_some() {
        state.action_error = Some("Wait for the current account operation to finish.".to_owned());
        return;
    }
    let id = match &operation {
        AccountOperation::Authorize(DriveAuthorization::Connect(request)) => request.id.clone(),
        AccountOperation::Authorize(DriveAuthorization::Reconnect(id))
        | AccountOperation::Check(id) => id.clone(),
        AccountOperation::Import(request) => request.id.clone(),
    };
    let inbox = state.inbox.clone();
    if let AccountOperation::Authorize(authorization) = operation {
        let callback_inbox = inbox.clone();
        let callback: rclone_adapter::AuthUrlCallback = Box::new(move |url| {
            if is_drive_authorization_url(url) {
                callback_inbox.push_cmd(WorkerCmd::AuthorizationReady(SecretString::new(url)));
            }
        });
        match mgr.start_drive_authorization(authorization, Some(callback)) {
            Ok(handle) => {
                state.account_operation_id = Some(id.clone());
                state.reconnecting_account = Some(id);
                state.authorization_canceller = Some(handle.canceller());
                state.account_worker = Some(thread::spawn(move || {
                    let result = handle
                        .wait()
                        .map(|outcome| outcome.account)
                        .map_err(|err| err.cause());
                    inbox.push_cmd(WorkerCmd::AccountOperationFinished(result));
                }));
            }
            Err(err) => state.action_error = Some(format!("{} {}", err.cause(), err.next_step())),
        }
        return;
    }

    state.account_operation_id = Some(id);
    let injected_service = state.account_service.clone();
    state.account_worker = Some(thread::spawn(move || {
        // Hold the manager lock while AccountService uses the same database.
        let service = injected_service.map(Ok).unwrap_or_else(|| {
            AccountService::for_locked_manager(AccountServiceConfig::with_state_dir(
                mgr.state_dir(),
            ))
            .map(Arc::new)
        });
        let result = service
            .and_then(|service| match operation {
                AccountOperation::Import(request) => service.import_account(request),
                AccountOperation::Check(id) => service.check_account(&id).map(|r| r.account),
                AccountOperation::Authorize(_) => unreachable!(),
            })
            .map_err(|err| account_error_text(&err).to_owned());
        inbox.push_cmd(WorkerCmd::AccountOperationFinished(result));
    }));
}

/// Match rclone's environment key convention for hyphenated remote names.
struct DriveAccountProcessRunner;

impl ProcessRunner for DriveAccountProcessRunner {
    fn spawn(&self, spec: &ProcessSpec, options: SpawnOptions) -> Result<RunHandle, ProcessError> {
        if let Some((key, _)) = spec.env.iter().find(|(key, _)| {
            key.to_str().is_some_and(|name| {
                name.starts_with("RCLONE_CONFIG_")
                    && name.ends_with("_CONFIG_AUTH_NO_BROWSER")
                    && name.contains('-')
            })
        }) {
            let mut spec = spec.clone();
            let value = spec.env.remove(key).expect("key came from the environment");
            spec.env
                .insert(key.to_string_lossy().replace('-', "_").into(), value);
            return RealProcessRunner.spawn(&spec, options);
        }
        RealProcessRunner.spawn(spec, options)
    }
}

fn account_error_text(err: &AccountError) -> &'static str {
    match err {
        AccountError::AuthenticationRequired { .. } => {
            "FolderSync couldn’t verify access. If Keychain is locked, unlock it and choose Check access. If your sign-in expired, choose Reconnect."
        }
        AccountError::GlobalDefaultConfigRefused { .. } => {
            "Choose a separate exported configuration file."
        }
        AccountError::RemoteNotFound { .. } => {
            "The selected configuration doesn’t contain that remote. Check its name and try again."
        }
        AccountError::EncryptedSourceUnsupported { .. } => {
            "Choose an unencrypted export of the remote or connect Google Drive in your browser."
        }
        AccountError::AlreadyExists { .. } => {
            "That account ID is already in use. Choose a different ID or reconnect the account."
        }
        AccountError::InvalidInput { .. } => {
            "Check the account details, shared folder ID, and OAuth client settings, then try again."
        }
        AccountError::RecoveryNeeded { .. } => {
            "FolderSync found an earlier configuration backup. Review the account setup before trying again."
        }
        _ => {
            "FolderSync couldn’t verify access. Check the connection, account setup, and sync engine, then try again."
        }
    }
}

fn project_filter_review(preview: &FilterChangePreview, free_bytes: Option<u64>) -> FilterReview {
    let preflight = preview.preflight();
    let mut checks = vec![
        LocationCheck {
            ok: preflight.layout_error.is_none(),
            message: preflight
                .layout_error
                .clone()
                .unwrap_or_else(|| "Folder and recovery locations checked.".to_owned()),
        },
        LocationCheck {
            ok: preflight.detected_volume.is_ok(),
            message: match &preflight.detected_volume {
                Ok(volume) => format!("Volume checked · {}", volume.display_name),
                Err(reason) => reason.clone(),
            },
        },
    ];
    for (side, inventory) in [
        ("Local folder", &preflight.local_inventory),
        ("Cloud folder", &preflight.remote_inventory),
    ] {
        match inventory {
            Ok(items) => {
                checks.push(LocationCheck {
                    ok: true,
                    message: format!(
                        "{side} · {} included files · {}",
                        items.file_count,
                        format_bytes(items.total_bytes)
                    ),
                });
                for problem in items
                    .marker_conflict
                    .iter()
                    .chain(items.name_findings.iter())
                {
                    checks.push(LocationCheck {
                        ok: false,
                        message: problem.clone(),
                    });
                }
                if !items.symlinks.is_empty() || !items.special_files.is_empty() {
                    checks.push(LocationCheck {
                        ok: false,
                        message: format!("{side} contains unsupported files."),
                    });
                }
            }
            Err(reason) => checks.push(LocationCheck {
                ok: false,
                message: format!("{side} · {reason}"),
            }),
        }
    }
    checks.push(LocationCheck {
        ok: preflight.direction.is_ok(),
        message: match &preflight.direction {
            Ok(InitPreference::Path2) => "First sync downloads from the cloud.".to_owned(),
            Ok(InitPreference::Path1)
                if preflight
                    .local_inventory
                    .as_ref()
                    .is_ok_and(|i| i.file_count > 0) =>
            {
                "First sync uploads to the cloud.".to_owned()
            }
            Ok(InitPreference::Path1) => "First sync sets up access markers.".to_owned(),
            Err(reason) => reason.clone(),
        },
    });
    if matches!(preflight.direction, Ok(InitPreference::Path2))
        && let (Some(free), Ok(cloud)) = (free_bytes, &preflight.remote_inventory)
    {
        checks.push(LocationCheck {
            ok: cloud.total_bytes <= free,
            message: format!(
                "{} to download · {} free on the target volume.",
                format_bytes(cloud.total_bytes),
                format_bytes(free)
            ),
        });
    }
    FilterReview {
        added: preview.added_exclusions().to_vec(),
        removed: preview.removed_exclusions().to_vec(),
        checks,
    }
}

fn begin_first_sync(state: &mut WorkerState, mapping_id: MappingId) {
    state.wizard.editing_mapping_id = Some(mapping_id.clone());
    state.wizard.first_sync_mapping_id = Some(mapping_id);
    state.wizard.first_sync_preflight_done = false;
    state.wizard.first_sync_markers_ready = false;
    state.wizard.first_sync_stopping = false;
    state.wizard.first_sync_completed = false;
    state.wizard.first_sync_error = None;
    state.wizard.first_sync_percent = 0;
    state.wizard.first_sync_file_progress = None;
    state.wizard.first_sync_elapsed_secs = 0;
    state.first_sync_started_at = Some(Instant::now());
    state.wizard.step = WizardStep::FirstSync;
}

fn show_stopped_first_sync(wizard: &mut WizardDraft) {
    wizard.first_sync_stopping = false;
    if wizard.changing_subfolders {
        wizard.first_sync_error =
            Some("First sync stopped. Open the mapping to review its setup.".to_owned());
    } else {
        wizard.step = WizardStep::Review;
    }
}

fn wizard_exclusions_changed(state: &mut WorkerState) {
    invalidate_wizard_confirmation(&mut state.wizard);
    if state.wizard.changing_subfolders {
        state.filter_preview = None;
    } else {
        recompute_real_wizard_inventory(state);
    }
}

fn invalidate_wizard_confirmation(wizard: &mut WizardDraft) {
    wizard.filter_review = None;
    wizard.no_other_sync_confirmed = false;
    wizard.reviewed_fingerprint = None;
}

fn wizard_review_fingerprint(wizard: &WizardDraft) -> String {
    let excluded_subs: Vec<&str> = wizard
        .subfolders
        .iter()
        .filter(|n| n.depth == 1 && !n.included)
        .map(|n| n.name.as_str())
        .collect();
    format!(
        "{}|{}|{}|{}|{}|{}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
        wizard.selected_account,
        wizard.selected_folder,
        wizard.remote_folder_path.as_deref().unwrap_or(""),
        wizard.local_path,
        wizard.local_recovery_path,
        wizard.cloud_recovery_path,
        wizard.drive_collection,
        excluded_subs,
        wizard.custom_patterns,
        wizard.review_scenario,
        wizard.local_file_count,
        wizard.cloud_file_count,
        wizard.local_total_bytes,
        wizard.cloud_total_bytes,
    )
}

#[cfg(test)]
#[test]
fn wizard_review_fingerprint_changes_with_drive_ancestry() {
    let mut wizard = default_real_wizard();
    wizard.drive_collection = Some(CloudCollection {
        folder_id: "research-id".to_owned(),
        display_path: "Data/Research".to_owned(),
        ancestor_ids: vec!["parent-id".to_owned(), "data-id".to_owned()],
    });
    let fingerprint = wizard_review_fingerprint(&wizard);
    wizard.drive_collection.as_mut().unwrap().ancestor_ids[1] = "replacement-data-id".to_owned();
    assert_ne!(wizard_review_fingerprint(&wizard), fingerprint);
}

fn record_mgr_result<T>(state: &mut WorkerState, res: Result<T, ManagerError>) {
    if let Err(err) = res {
        state.action_error = Some(format!("{} {}", err.cause(), err.next_step()));
    }
}

fn load_inspected_run(state: &mut WorkerState, mapping_id: &MappingId, run_id: &RunId) {
    let Some(mgr) = &state.mgr else {
        return;
    };
    let snap = mgr.snapshot();
    let mapping_name = snap
        .mappings
        .iter()
        .find(|m| &m.id == mapping_id)
        .map(|m| m.display_name.clone())
        .unwrap_or_else(|| mapping_id.as_str().to_owned());

    let run = match mgr.get_run(run_id) {
        Ok(Some(r)) => r,
        Ok(None) => {
            state.action_error = Some(format!("Run `{run_id}` could not be found."));
            return;
        }
        Err(err) => {
            state.action_error = Some(format!("{} {}", err.cause(), err.next_step()));
            return;
        }
    };

    // Surface log read & recovery-list failures as explicit errors rather than empty results (HIGH #9).
    let mut read_errors = Vec::new();
    let log_lines = match mgr.read_run_log(run_id, DEFAULT_RUN_LOG_MAX_LINES) {
        Ok(lines) => lines,
        Err(err) => {
            read_errors.push(format!("Could not read run log: {}", err.cause()));
            Vec::new()
        }
    };
    let copies: Vec<RecoveryCopyItem> = match mgr.list_recovery_copies(run_id) {
        Ok(list) => list
            .into_iter()
            .map(|c| RecoveryCopyItem {
                side_label: match c.side {
                    RecoveryCopySide::Local => "On this Mac".to_owned(),
                    RecoveryCopySide::Remote => "Local destination recovery".to_owned(),
                },
                is_remote: c.side == RecoveryCopySide::Remote,
                relative_path: c.relative_path,
                size_label: format_bytes(c.size_bytes),
            })
            .collect(),
        Err(err) => {
            read_errors.push(format!("Could not read recovery copies: {}", err.cause()));
            Vec::new()
        }
    };
    if !read_errors.is_empty() {
        state.action_error = Some(read_errors.join(" "));
    }

    let log_exists = run.log_path.is_file();
    state.inspected_run = Some(InspectedRunSnapshot {
        run_id: run.id.clone(),
        mapping_id: mapping_id.clone(),
        mapping_name,
        started_label: format_short_timestamp(&run.started_at),
        result_label: run
            .outcome_kind
            .clone()
            .unwrap_or_else(|| run.status.clone()),
        log_lines,
        log_path: Some(run.log_path.to_string_lossy().into_owned()),
        log_exists,
        recovery_copies: copies,
        copy_notice: None,
    });
}

fn load_recovery_checklist(state: &mut WorkerState, mapping_id: &MappingId) {
    let Some(mgr) = &state.mgr else {
        return;
    };
    match mgr.inspect_recovery(mapping_id.clone()) {
        Ok(report) => {
            let has_ambiguous_case = report.open_cases.iter().any(|c| {
                c.case.reason_kind == "orphan_ambiguous"
                    || (matches!(
                        c.case.reason_kind.as_str(),
                        "orphan_running" | "shutdown_timeout"
                    ) && c.orphan_still_alive != Some(false))
            });
            let orphan_alive = if report.open_cases.is_empty() {
                Some(false)
            } else if report
                .open_cases
                .iter()
                .any(|c| c.orphan_still_alive == Some(true))
            {
                Some(true)
            } else if report.open_cases.iter().any(|c| {
                c.case.reason_kind == "orphan_ambiguous"
                    || (matches!(
                        c.case.reason_kind.as_str(),
                        "orphan_running" | "shutdown_timeout"
                    ) && c.orphan_still_alive.is_none())
                    || (c.case.orphan_pid.is_some() && c.orphan_still_alive.is_none())
            }) {
                None
            } else {
                Some(false)
            };
            let first_case = report.open_cases.first();
            let cause = first_case
                .map(|c| c.case.cause.clone())
                .unwrap_or_else(|| "Recovery review requested.".to_owned());
            let next_step = first_case
                .map(|c| c.case.next_step.clone())
                .unwrap_or_else(|| "Inspect the run log and recovery copies.".to_owned());
            let latest_run_id = first_case
                .and_then(|c| c.case.run_id.clone())
                .or_else(|| report.recent_runs.first().map(|r| r.id.clone()));

            let recovery_copies = match latest_run_id.as_ref() {
                Some(rid) => match mgr.list_recovery_copies(rid) {
                    Ok(list) => list
                        .into_iter()
                        .map(|c| RecoveryCopyItem {
                            side_label: match c.side {
                                RecoveryCopySide::Local => "On this Mac".to_owned(),
                                RecoveryCopySide::Remote => "Local destination recovery".to_owned(),
                            },
                            is_remote: c.side == RecoveryCopySide::Remote,
                            relative_path: c.relative_path,
                            size_label: format_bytes(c.size_bytes),
                        })
                        .collect(),
                    Err(err) => {
                        state.action_error =
                            Some(format!("Could not read recovery copies: {}", err.cause()));
                        Vec::new()
                    }
                },
                None => Vec::new(),
            };

            state.recovery_checklist = Some(RecoveryChecklistData {
                mapping_id: mapping_id.clone(),
                inspected: true,
                is_paused: report.state.intent == Intent::Paused,
                orphan_alive,
                has_ambiguous_case,
                has_lock_file: report.has_lock_file,
                workdir_files: report.workdir_files,
                cause,
                next_step,
                latest_run_id,
                recovery_copies,
                identities_verified: report.identities_verified,
                identity_check_detail: report.identity_check_detail,
            });
        }
        Err(err) => {
            state.action_error = Some(format!("{} {}", err.cause(), err.next_step()));
        }
    }
}

fn load_mapping_into_wizard(
    state: &mut WorkerState,
    mapping_id: &MappingId,
    requested_step: WizardStep,
) -> bool {
    let Some(mgr) = &state.mgr else {
        return false;
    };
    let Ok(records) = mgr.list_mappings_records() else {
        return false;
    };
    let Some(rec) = records.into_iter().find(|m| &m.id == mapping_id) else {
        return false;
    };

    // Reset wizard cleanly before populating from `rec` so prior draft exclusions
    // never leak into a resumed draft or Finish Setup session (HIGH #1).
    let mut wizard = default_real_wizard();
    wizard.editing_mapping_id = Some(rec.id.clone());
    wizard.selected_account = rec.account_id.clone();
    wizard.selected_folder = rec.collection.display_name().to_owned();
    wizard.local_path = rec
        .local_binding
        .resolved_path()
        .to_string_lossy()
        .into_owned();
    wizard.remote_folder_path = if rec.remote_endpoint_kind == "local_dir" {
        Some(rec.remote_endpoint_value.clone())
    } else {
        None
    };
    wizard.local_recovery_path = rec.local_recovery_root.to_string_lossy().into_owned();
    wizard.cloud_recovery_path = rec.cloud_recovery_path.clone();

    let default_pats: BTreeSet<&str> = DEFAULT_EXCLUSIONS.iter().map(|d| d.pattern).collect();
    let mut restored_excluded_subs = BTreeSet::new();
    for excl in &rec.exclusions {
        if default_pats.contains(excl.as_str()) {
            continue;
        }
        if excl.starts_with('/')
            && excl.ends_with("/**")
            && !excl
                .trim_end_matches("/**")
                .contains(['*', '?', '[', ']', '{', '}'])
        {
            let sub_name = excl
                .trim_start_matches('/')
                .trim_end_matches("/**")
                .to_owned();
            if !sub_name.is_empty() && !sub_name.contains('/') {
                restored_excluded_subs.insert(sub_name);
                continue;
            }
        }
        wizard.custom_patterns.push(excl.clone());
    }
    // Seed placeholder unchecked depth-1 nodes so `recompute_real_wizard_inventory` preserves them.
    for name in restored_excluded_subs {
        wizard.subfolders.push(SubfolderNode {
            id: name.clone(),
            name,
            depth: 1,
            has_children: false,
            size_gb: 0.0,
            size_label: "Unknown".to_owned(),
            included: false,
        });
    }

    if rec.remote_endpoint_kind == "drive_remote" {
        wizard.drive_collection = Some(rec.collection.clone());
    }
    let snapshot = mgr.snapshot();
    cancel_folder_size(state);
    clear_cloud_catalog(state);
    state.wizard = wizard;
    state.filter_preview = None;
    state.first_sync_started_at = None;
    if requested_step == WizardStep::Subfolders {
        state.wizard.changing_subfolders = true;
        state.wizard.step = WizardStep::Subfolders;
        if let Some(mapping) = snapshot.mappings.iter().find(|m| &m.id == mapping_id) {
            state.wizard.volume_name.clone_from(&mapping.volume_name);
            state.wizard.volume_free_bytes = mapping.volume_space.map(|space| space.free_bytes);
        }
        if let Some(path) = state.wizard.remote_folder_path.as_deref()
            && let Ok(inventory) = inspect_local_endpoint_detailed(
                Path::new(path),
                &rec.collection,
                &sync_core::default_exclusion_patterns(),
            )
        {
            set_subfolder_inventory(&mut state.wizard, &inventory);
        }
    } else {
        recompute_real_wizard_inventory(state);
        state.wizard.step = if state.wizard.can_visit_step(requested_step) {
            requested_step
        } else {
            WizardStep::Location
        };
    }
    true
}

fn persist_real_draft_if_ready(state: &mut WorkerState, explicit_save: bool) -> Result<(), String> {
    let cmd = match build_add_mapping_from_wizard(state) {
        Ok(cmd) => cmd,
        Err(err) => {
            return if explicit_save { Err(err) } else { Ok(()) };
        }
    };
    let Some(mgr) = &state.mgr else {
        return Err("Manager is not running.".to_owned());
    };
    let can_save = mgr
        .snapshot()
        .mappings
        .iter()
        .find(|m| m.id == cmd.id)
        .is_none_or(|m| m.readiness == Readiness::Unconfigured);
    if !can_save {
        return if explicit_save {
            Err(format!(
                "Mapping `{}` is already configured and cannot be overwritten as a draft.",
                cmd.id
            ))
        } else {
            Ok(())
        };
    }
    match mgr.save_draft_mapping(cmd.clone()) {
        Ok(_) => {
            state.wizard.editing_mapping_id = Some(cmd.id);
            Ok(())
        }
        Err(err) => Err(format!("{} {}", err.cause(), err.next_step())),
    }
}

fn allocate_unique_mapping_id(state: &WorkerState, folder: &str) -> MappingId {
    let base = sanitize_id(folder);
    let existing: BTreeSet<String> = state
        .mgr
        .as_ref()
        .map(|m| {
            m.snapshot()
                .mappings
                .iter()
                .map(|x| x.id.as_str().to_owned())
                .collect()
        })
        .unwrap_or_default();
    if !existing.contains(&base) {
        return MappingId::new(base);
    }
    for suffix in 2u32..=9999 {
        let cand = format!("{base}-{suffix}");
        if !existing.contains(&cand) {
            return MappingId::new(cand);
        }
    }
    MappingId::new(format!("{base}-{}", chrono::Utc::now().timestamp_millis()))
}

fn build_add_mapping_from_wizard(state: &WorkerState) -> Result<AddMappingCommand, String> {
    let folder = state.wizard.selected_folder.trim();
    let local_str = state.wizard.local_path.trim();
    let remote_str = state
        .wizard
        .remote_folder_path
        .as_deref()
        .map(str::trim)
        .unwrap_or("");
    if folder.is_empty() || local_str.is_empty() || !state.wizard.has_cloud_folder() {
        return Err(
            "Choose a folder in Data, a local folder on this Mac, and a recovery folder first."
                .to_owned(),
        );
    }
    let mapping_id = state
        .wizard
        .editing_mapping_id
        .clone()
        .unwrap_or_else(|| allocate_unique_mapping_id(state, folder));
    let local_rec = if state.wizard.local_recovery_path.trim().is_empty() {
        PathBuf::from(local_str)
            .parent()
            .ok_or_else(|| "Choose a local folder inside a mounted volume.".to_owned())?
            .join("FolderSyncRecovery")
            .join(mapping_id.as_str())
    } else {
        PathBuf::from(state.wizard.local_recovery_path.trim())
    };
    if let Some(collection) = &state.wizard.drive_collection {
        let mut cmd = AddMappingCommand::drive(
            mapping_id,
            state.wizard.selected_account.clone(),
            collection.clone(),
            PathBuf::from(local_str),
            local_rec,
        )
        .map_err(|err| format!("{} {}", err.cause(), err.next_step()))?;
        cmd.extra_exclusions = state.wizard.extra_exclusions();
        return Ok(cmd);
    }
    let remote_rec = if state.wizard.cloud_recovery_path.trim().is_empty() {
        PathBuf::from(remote_str)
            .parent()
            .ok_or_else(|| "Choose a valid destination folder.".to_owned())?
            .join("FolderSyncRecovery-Remote")
            .join(mapping_id.as_str())
    } else {
        PathBuf::from(state.wizard.cloud_recovery_path.trim())
    };
    let mut cmd = AddMappingCommand::local_fixture(
        mapping_id,
        state.wizard.selected_account.clone(),
        format!("Data/{folder}"),
        PathBuf::from(local_str),
        PathBuf::from(remote_str),
        local_rec,
        remote_rec,
    )
    .map_err(|err| format!("{} {}", err.cause(), err.next_step()))?;

    cmd.extra_exclusions = state.wizard.extra_exclusions();
    Ok(cmd)
}

fn clear_cloud_catalog(state: &mut WorkerState) {
    state.cloud_catalog.clear();
    state.skipped_cloud_folders.clear();
    state.cloud_catalog_error = None;
    state.refresh_catalog_after_size = false;
}

fn refresh_real_catalog(state: &mut WorkerState) {
    clear_cloud_catalog(state);
    let Some(parent) = state
        .local_account_parents
        .get(&state.wizard.selected_account)
        .cloned()
    else {
        if drive_size_holds_wizard_account(state) {
            state.refresh_catalog_after_size = true;
        } else if let Some(mgr) = &state.mgr {
            match mgr.list_collection_candidates(&state.wizard.selected_account) {
                Ok(listing) => {
                    state.cloud_catalog = listing
                        .candidates
                        .into_iter()
                        .map(|candidate| project_drive_candidate(state, candidate))
                        .collect();
                    state.skipped_cloud_folders = listing.skipped;
                }
                Err(err) => {
                    state.cloud_catalog_error =
                        Some(format!("{} {}", err.cause(), err.next_step()));
                }
            }
        }
        return;
    };
    let existing_mappings = state
        .mgr
        .as_ref()
        .map(|m| m.snapshot().mappings.clone())
        .unwrap_or_default();

    let Ok(entries) = fs::read_dir(&parent) else {
        state.cloud_catalog_error = Some("Can’t read the local test account folder.".to_owned());
        return;
    };
    let mut items = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() || !meta.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let dummy_col = CloudCollection {
            display_path: format!("Data/{name}"),
            folder_id: format!("local-col-Data_{name}"),
            ancestor_ids: vec!["local-root".to_owned()],
        };
        let inv = inspect_local_endpoint_detailed(&path, &dummy_col, &BTreeSet::new()).ok();
        let used_by_other = path.join(ACCESS_MARKER_FILENAME).is_file();
        let on_this_mac = existing_mappings
            .iter()
            .find(|m| {
                m.account_id == state.wizard.selected_account
                    && Some(&m.id) != state.wizard.editing_mapping_id.as_ref()
                    && m.display_name.eq_ignore_ascii_case(&name)
            })
            .map(|m| m.volume_name.clone());
        items.push(CloudFolderCandidate {
            name,
            collection: None,
            size: inv.map_or_else(
                || FolderSize::Failed {
                    cause: "Can’t check this test folder.".to_owned(),
                },
                |inv| FolderSize::Known {
                    bytes: inv.total_bytes,
                    files: Some(inv.file_count as u64),
                    measured_at: None,
                },
            ),
            on_this_mac,
            used_by_other_computer: used_by_other,
        });
    }
    items.sort_by(|a, b| a.name.cmp(&b.name));
    state.cloud_catalog = items;
}

fn project_drive_candidate(
    state: &WorkerState,
    candidate: CollectionCandidate,
) -> CloudFolderCandidate {
    let on_this_mac = state.mgr.as_ref().and_then(|mgr| {
        mgr.snapshot()
            .mappings
            .iter()
            .find(|mapping| {
                mapping.collection_folder_id == candidate.collection.folder_id
                    && Some(&mapping.id) != state.wizard.editing_mapping_id.as_ref()
            })
            .map(|mapping| format!("On {}", mapping.volume_name))
    });
    CloudFolderCandidate {
        name: candidate.name,
        collection: Some(candidate.collection),
        size: FolderSize::Unmeasured,
        on_this_mac,
        used_by_other_computer: false,
    }
}

fn select_cloud_folder(state: &mut WorkerState, candidate: CloudFolderCandidate) {
    let same_folder = state.wizard.selected_folder == candidate.name;
    if same_folder && candidate.size == FolderSize::Calculating {
        return;
    }
    cancel_folder_size(state);
    if !same_folder {
        state.wizard.subfolders.clear();
    }
    state.wizard.selected_folder = candidate.name;
    state.wizard.drive_collection = candidate.collection;
    state.wizard.remote_folder_path = state
        .local_account_parents
        .get(&state.wizard.selected_account)
        .map(|parent| {
            parent
                .join(&state.wizard.selected_folder)
                .to_string_lossy()
                .into_owned()
        });
    if let Some(collection) = &state.wizard.drive_collection {
        let remote = state
            .mgr
            .as_ref()
            .and_then(|mgr| mgr.list_accounts().ok())
            .and_then(|accounts| {
                accounts
                    .into_iter()
                    .find(|acct| acct.id == state.wizard.selected_account)
            })
            .map_or_else(
                || state.wizard.selected_account.to_string(),
                |acct| acct.remote_name,
            );
        state.wizard.cloud_recovery_path = format!("{remote}:Recovery/{}", collection.folder_id);
        state.requested_folder_size = Some(collection.folder_id.clone());
        if let Some(item) = state
            .cloud_catalog
            .iter_mut()
            .find(|item| item.name == state.wizard.selected_folder)
        {
            item.size = FolderSize::Calculating;
        }
        update_folder_size(state);
    }
    invalidate_wizard_confirmation(&mut state.wizard);
}

fn cancel_folder_size(state: &mut WorkerState) {
    state.requested_folder_size = None;
    if let Some(pending) = &mut state.pending_folder_size
        && !pending.canceled
    {
        pending.handle.cancel();
        pending.canceled = true;
    }
    for item in &mut state.cloud_catalog {
        if item.size == FolderSize::Calculating {
            item.size = FolderSize::Unmeasured;
        }
    }
}

fn drive_size_holds_wizard_account(state: &WorkerState) -> bool {
    state
        .pending_folder_size
        .as_ref()
        .is_some_and(|pending| pending.account == state.wizard.selected_account)
}

fn update_folder_size(state: &mut WorkerState) {
    let Some(mgr) = state.mgr.clone() else { return };
    if let Some(pending) = &state.pending_folder_size {
        let snapshot = mgr.snapshot();
        let Some(result) = snapshot.folder_sizes.iter().find(|size| {
            size.account_id == pending.account
                && size.folder_id == pending.folder_id
                && size.status != FolderSizeStatus::Calculating
        }) else {
            return;
        };
        let pending = state.pending_folder_size.take().expect("size was pending");
        if !pending.canceled && pending.account == state.wizard.selected_account {
            set_folder_size(state, &pending.folder_id, project_folder_size(result));
        }
        if state.refresh_catalog_after_size {
            if state.wizard.step == WizardStep::Folder {
                refresh_real_catalog(state);
            } else {
                state.refresh_catalog_after_size = false;
            }
        }
        if state.wizard.step == WizardStep::Location
            && pending.account == state.wizard.selected_account
        {
            recompute_real_wizard_inventory(state);
            if let Err(err) = persist_real_draft_if_ready(state, false) {
                state.action_error = Some(err);
            }
        }
    }

    let Some(folder_id) = state.requested_folder_size.take() else {
        return;
    };
    let account = state.wizard.selected_account.clone();
    match mgr.request_folder_size(&account, &folder_id) {
        Ok(handle) => {
            let snapshot = mgr.snapshot();
            if let Some(result) = snapshot.folder_sizes.iter().find(|size| {
                size.account_id == account
                    && size.folder_id == folder_id
                    && size.status != FolderSizeStatus::Calculating
            }) {
                set_folder_size(state, &folder_id, project_folder_size(result));
            } else {
                state.pending_folder_size = Some(PendingFolderSize {
                    account,
                    folder_id,
                    handle,
                    canceled: false,
                });
            }
        }
        Err(err) => set_folder_size(state, &folder_id, FolderSize::Failed { cause: err.cause() }),
    }
}

fn set_folder_size(state: &mut WorkerState, id: &str, size: FolderSize) {
    if let Some(item) = state.cloud_catalog.iter_mut().find(|item| {
        item.collection
            .as_ref()
            .is_some_and(|collection| collection.folder_id == id)
    }) {
        item.size = size;
    }
}

fn project_folder_size(result: &FolderSizeSnapshot) -> FolderSize {
    match &result.status {
        FolderSizeStatus::Calculating => FolderSize::Calculating,
        FolderSizeStatus::Known { bytes, files } => FolderSize::Known {
            bytes: *bytes,
            files: Some(*files),
            measured_at: Some(
                result
                    .updated_at
                    .with_timezone(&chrono::Local)
                    .format("%H:%M")
                    .to_string(),
            ),
        },
        FolderSizeStatus::Failed { cause } => FolderSize::Failed {
            cause: cause.clone(),
        },
    }
}

/// Runs the manager's single preflight preview function so wizard checks and
/// actual initialization preflight always agree (HIGH #2).
fn set_subfolder_inventory(
    wizard: &mut WizardDraft,
    source: &sync_manager::DetailedEndpointInventory,
) {
    let omitted = wizard.excluded_names();
    let rooted_total = source.total_bytes.saturating_add(
        source
            .subfolders
            .iter()
            .filter(|sub| omitted.contains(&sub.name) && !wizard.changing_subfolders)
            .map(|sub| sub.total_bytes)
            .fold(0, u64::saturating_add),
    );
    let mut nodes = vec![SubfolderNode {
        id: "root".to_owned(),
        name: wizard.selected_folder.clone(),
        depth: 0,
        has_children: !source.subfolders.is_empty(),
        included: true,
        size_gb: rooted_total as f64 / 1_000_000_000.0,
        size_label: format_bytes(rooted_total),
    }];
    nodes.extend(source.subfolders.iter().map(|sub| SubfolderNode {
        id: sub.relative_path.clone(),
        name: sub.name.clone(),
        depth: 1,
        has_children: sub.has_children,
        included: !omitted.contains(&sub.name),
        size_gb: sub.total_bytes as f64 / 1_000_000_000.0,
        size_label: format_bytes(sub.total_bytes),
    }));
    nodes.extend(
        wizard
            .subfolders
            .iter()
            .filter(|node| {
                node.depth == 1
                    && !node.included
                    && !source.subfolders.iter().any(|sub| sub.name == node.name)
            })
            .cloned(),
    );
    wizard.subfolders = nodes;
}

fn recompute_real_wizard_inventory(state: &mut WorkerState) {
    let mut loc_checks = Vec::new();
    let mut rec_checks = Vec::new();
    let trimmed_local = state.wizard.local_path.trim().to_owned();
    let local_path = PathBuf::from(&trimmed_local);

    if !trimmed_local.is_empty()
        && state.wizard.drive_collection.is_some()
        && drive_size_holds_wizard_account(state)
    {
        // The canceled measurement still owns the account. Its completion resumes this check.
        state.wizard.location_check_pending = true;
        return;
    }
    state.wizard.location_check_pending = false;

    let volume_space = if trimmed_local.is_empty() {
        None
    } else {
        match detect_volume_identity(&local_path) {
            Ok(det) => {
                state.wizard.volume_name = det.display_name;
                state.wizard.volume_kind = det.identity.kind;
                state.wizard.volume_fs = det.filesystem;
                Some(det.space)
            }
            Err(err) => {
                state.wizard.volume_name = "unknown".to_owned();
                state.wizard.volume_fs = "unknown".to_owned();
                loc_checks.push(LocationCheck {
                    ok: false,
                    message: err.cause(),
                });
                None
            }
        }
    };
    state.wizard.volume_free_bytes = volume_space.map(|space| space.free_bytes);
    if let Some(space) = volume_space {
        state.wizard.volume_free_gb = space.free_bytes / 1_000_000_000;
        state.wizard.volume_total_gb = (space.total_bytes / 1_000_000_000).max(1);
    }

    let mut local_inv_res = None;
    let mut remote_inv_res = None;
    let mut layout_err = None;

    if let Some(mgr) = &state.mgr
        && let Ok(cmd) = build_add_mapping_from_wizard(state)
    {
        let editing_id = state.wizard.editing_mapping_id.as_ref();
        match mgr.preflight_mapping_preview(&cmd, editing_id) {
            Ok(preview) => {
                if let Ok(det) = preview.detected_volume {
                    state.wizard.volume_name = det.display_name;
                    state.wizard.volume_kind = det.identity.kind;
                    state.wizard.volume_fs = det.filesystem;
                }
                layout_err = preview.layout_error;
                local_inv_res = Some(preview.local_inventory);
                remote_inv_res = Some(preview.remote_inventory);
            }
            Err(err) => {
                loc_checks.push(LocationCheck {
                    ok: false,
                    message: format!("Can't check setup: {}", err.cause()),
                });
            }
        }
    } else if !trimmed_local.is_empty() && loc_checks.is_empty() {
        // Destination not selected yet — still inspect local folder honestly (never swallow errors).
        let collection = CloudCollection {
            display_path: format!("Data/{}", state.wizard.selected_folder),
            folder_id: format!("local-col-Data_{}", state.wizard.selected_folder),
            ancestor_ids: vec!["local-root".to_owned()],
        };
        let mut exclusions = sync_core::default_exclusion_patterns();
        for pat in &state.wizard.custom_patterns {
            if !pat.trim().is_empty() {
                exclusions.insert(pat.trim().to_owned());
            }
        }
        local_inv_res = Some(
            inspect_local_endpoint_detailed(&local_path, &collection, &exclusions)
                .map_err(|e| e.cause()),
        );
    }

    // Never treat an unreadable folder as empty (HIGH #2).
    let local_ok = match &local_inv_res {
        Some(Ok(inv)) => {
            loc_checks.push(LocationCheck {
                ok: true,
                message: if inv.classification == SideClassification::HasData {
                    "Local folder exists and contains files.".to_owned()
                } else {
                    "Local folder exists and is empty.".to_owned()
                },
            });
            if let Some(mc) = &inv.marker_conflict {
                loc_checks.push(LocationCheck {
                    ok: false,
                    message: mc.clone(),
                });
            }
            if !inv.symlinks.is_empty() {
                loc_checks.push(LocationCheck {
                    ok: false,
                    message: format!(
                        "Local folder contains unsupported symbolic link(s): {}.",
                        inv.symlinks.join(", ")
                    ),
                });
            }
            if !inv.special_files.is_empty() {
                loc_checks.push(LocationCheck {
                    ok: false,
                    message: format!(
                        "Local folder contains unsupported special file(s): {}.",
                        inv.special_files.join(", ")
                    ),
                });
            }
            if let Some(first_name) = inv.name_findings.first() {
                loc_checks.push(LocationCheck {
                    ok: false,
                    message: first_name.clone(),
                });
            }
            Some(inv)
        }
        Some(Err(err)) => {
            loc_checks.push(LocationCheck {
                ok: false,
                message: format!("Can't check local folder: {err}"),
            });
            None
        }
        None => None,
    };

    let remote_ok = match &remote_inv_res {
        Some(Ok(inv)) => {
            if let Some(mc) = &inv.marker_conflict {
                loc_checks.push(LocationCheck {
                    ok: false,
                    message: mc.clone(),
                });
            }
            if !inv.symlinks.is_empty() {
                loc_checks.push(LocationCheck {
                    ok: false,
                    message: format!(
                        "Destination folder contains unsupported symbolic link(s): {}.",
                        inv.symlinks.join(", ")
                    ),
                });
            }
            if !inv.special_files.is_empty() {
                loc_checks.push(LocationCheck {
                    ok: false,
                    message: format!(
                        "Destination folder contains unsupported special file(s): {}.",
                        inv.special_files.join(", ")
                    ),
                });
            }
            if let Some(first_name) = inv.name_findings.first() {
                loc_checks.push(LocationCheck {
                    ok: false,
                    message: first_name.clone(),
                });
            }
            Some(inv)
        }
        Some(Err(err)) => {
            loc_checks.push(LocationCheck {
                ok: false,
                message: format!("Can't check destination folder: {err}"),
            });
            None
        }
        None => {
            if !trimmed_local.is_empty() && !state.wizard.has_cloud_folder() {
                loc_checks.push(LocationCheck {
                    ok: false,
                    message: "Choose a folder from the account before continuing.".to_owned(),
                });
            }
            None
        }
    };

    // Measure the available choices independently of the selection.
    if let Some(src_inv) = remote_ok
        .filter(|i| i.classification == SideClassification::HasData)
        .or(local_ok)
    {
        set_subfolder_inventory(&mut state.wizard, src_inv);
    }

    state.wizard.cloud_file_count = remote_ok.map(|i| i.file_count);
    state.wizard.cloud_total_bytes = remote_ok.map(|i| i.total_bytes);
    state.wizard.local_file_count = local_ok.map(|i| i.file_count);
    state.wizard.local_total_bytes = local_ok.map(|i| i.total_bytes);
    state.wizard.marker_reused = remote_ok.is_some_and(|i| i.has_valid_marker);

    let mut symlinks = Vec::new();
    let mut name_issues = Vec::new();
    for inv in [remote_ok, local_ok].into_iter().flatten() {
        symlinks.extend(inv.symlinks.iter().cloned());
        name_issues.extend(inv.name_findings.iter().cloned());
    }
    state.wizard.found_symlinks = symlinks;
    state.wizard.found_name_issues = name_issues;

    // Derive ReviewScenario strictly from verified inventories (never from unread sides).
    let local_has = local_ok.is_some_and(|i| i.classification == SideClassification::HasData);
    let remote_has = remote_ok.is_some_and(|i| i.classification == SideClassification::HasData);
    state.wizard.review_scenario = match (remote_has, local_has) {
        (true, true) => ReviewScenario::BothPopulated,
        (true, false) => ReviewScenario::CloudToLocal,
        (false, true) => ReviewScenario::LocalToCloud,
        (false, false) => ReviewScenario::BothEmpty,
    };

    // Layout & recovery disjointness from manager's single validate_complete_layout (HIGH #2).
    let rec_local = state.wizard.local_recovery_path.trim().to_owned();
    let layout_is_recovery = layout_err
        .as_deref()
        .is_some_and(|msg| msg.to_ascii_lowercase().contains("recovery"));

    if let Some(ref err) = layout_err {
        if layout_is_recovery {
            rec_checks.push(LocationCheck {
                ok: false,
                message: err.clone(),
            });
            loc_checks.push(LocationCheck {
                ok: true,
                message: "No overlap with other synced folders.".to_owned(),
            });
        } else {
            loc_checks.push(LocationCheck {
                ok: false,
                message: err.clone(),
            });
        }
    } else if local_ok.is_some() && remote_ok.is_some() {
        loc_checks.push(LocationCheck {
            ok: true,
            message: "No overlap with other mappings.".to_owned(),
        });
    }

    if let Some(space) = volume_space {
        loc_checks.push(LocationCheck {
            ok: true,
            message: format!(
                "{} free on the target volume.",
                format_bytes(space.free_bytes)
            ),
        });
    }

    if rec_local.is_empty() {
        rec_checks.push(LocationCheck {
            ok: false,
            message: "Choose a local recovery folder outside every synced folder.".to_owned(),
        });
    } else if rec_checks.is_empty() {
        if layout_err.is_none() && local_ok.is_some() && remote_ok.is_some() {
            rec_checks.push(LocationCheck {
                ok: true,
                message: "Recovery folder is outside every synced folder.".to_owned(),
            });
        } else if !trimmed_local.is_empty()
            && (Path::new(&rec_local).starts_with(&local_path)
                || local_path.starts_with(Path::new(&rec_local)))
        {
            rec_checks.push(LocationCheck {
                ok: false,
                message: "Local recovery folder cannot overlap the synced folder.".to_owned(),
            });
        } else if layout_err.is_some() {
            rec_checks.push(LocationCheck {
                ok: false,
                message: "Fix folder overlap issues before checking recovery location.".to_owned(),
            });
        } else {
            rec_checks.push(LocationCheck {
                ok: true,
                message: "Recovery folder path set (checked against all mappings once folders are chosen)."
                    .to_owned(),
            });
        }
    }

    state.wizard.location_checks = loc_checks;
    state.wizard.recovery_checks = rec_checks;
}

fn emit_snapshot(
    state: &mut WorkerState,
    explicit_snap: Option<&StatusSnapshot>,
    ui_slot: &Arc<UiBridgeSlot>,
    latest_cached: &Arc<Mutex<UiSnapshot>>,
) {
    let ui_snap = project_ui_snapshot(state, explicit_snap);
    *latest_cached.lock().unwrap() = ui_snap.clone();
    ui_slot.publish_snapshot(ui_snap);
}

fn project_ui_snapshot(
    state: &mut WorkerState,
    explicit_snap: Option<&StatusSnapshot>,
) -> UiSnapshot {
    let Some(mgr) = &state.mgr else {
        let mut s = empty_loading_snapshot();
        s.action_error = state.action_error.clone();
        s.status_notice = state.status_notice.clone();
        return s;
    };

    let mgr_snap = match explicit_snap {
        Some(s) => s.clone(),
        None => (*mgr.snapshot()).clone(),
    };
    let accounts_raw = mgr.list_accounts().unwrap_or_default();

    // Refresh cached recent runs ONLY when dirty (never on every log line, HIGH #14).
    if state.recent_runs_dirty {
        state.recent_runs_cache.clear();
        for m in &mgr_snap.mappings {
            let runs = mgr
                .list_recent_runs(&m.id, DEFAULT_RECENT_RUNS_FOR_UI)
                .unwrap_or_default()
                .into_iter()
                .map(project_run_record)
                .collect();
            state.recent_runs_cache.insert(m.id.clone(), runs);
        }
        state.recent_runs_dirty = false;
    }

    let mut accounts: Vec<AccountSnapshot> = accounts_raw
        .iter()
        .map(|a| project_account(a, &mgr_snap.mappings, state))
        .collect();
    accounts.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));

    let mut volumes_map: BTreeMap<VolumeIdentity, VolumeSnapshot> = BTreeMap::new();
    let mut mappings = Vec::with_capacity(mgr_snap.mappings.len());
    let mut active_run = None;

    for m in &mgr_snap.mappings {
        let vol_id = VolumeIdentity::new(m.volume_kind, m.volume_stable_id.clone());
        let candidate = build_volume_snapshot(&vol_id, m);
        volumes_map
            .entry(vol_id.clone())
            .and_modify(|volume| {
                if (!volume.connected && candidate.connected)
                    || (volume.total_gb == 0 && candidate.total_gb > 0)
                {
                    *volume = candidate.clone();
                }
            })
            .or_insert(candidate);

        let recent_runs = state
            .recent_runs_cache
            .get(&m.id)
            .cloned()
            .unwrap_or_default();

        let (gen_num, engine_opts) = m
            .active_generation_id
            .as_ref()
            .and_then(|gid| mgr.get_generation(gid).ok().flatten())
            .map(|g| {
                let opts = vec![
                    format!("--compare {:?}", g.config.compare_policy),
                    format!(
                        "--max-delete {}",
                        g.config.recovery_policy.max_delete_percent.value()
                    ),
                    "--conflict-resolve none".to_owned(),
                    "--conflict-loser num".to_owned(),
                    "--check-access".to_owned(),
                    format!("--check-filename {ACCESS_MARKER_FILENAME}"),
                    "--drive-skip-gdocs --drive-skip-shortcuts".to_owned(),
                ];
                (g.generation_number as u32, opts)
            })
            .unwrap_or_else(|| (0, vec!["Not initialized yet.".to_owned()]));

        let mut mapping_state = MappingState::new(m.readiness, m.execution, m.intent);
        mapping_state.execution = m.execution;
        mapping_state.intent = m.intent;

        let last_run = match (m.last_run_outcome, m.last_run_completed_at.as_deref()) {
            (Some(outcome), Some(ts)) => {
                let wall = DateTime::parse_from_rfc3339(ts)
                    .ok()
                    .and_then(|dt| {
                        let utc = dt.with_timezone(&chrono::Local);
                        WallTime::new(
                            chrono::Timelike::hour(&utc) as u8,
                            chrono::Timelike::minute(&utc) as u8,
                        )
                    })
                    .unwrap_or_else(|| WallTime::new(0, 0).expect("00:00 is valid"));
                Some(LastRunSummary {
                    outcome,
                    completed_at: wall,
                })
            }
            _ => None,
        };

        let unreviewed_conflicts =
            m.unreviewed_conflict_run_id
                .clone()
                .map(|rid| UnreviewedConflicts {
                    run_id: rid,
                    paths: m.unreviewed_conflict_paths.clone(),
                });

        let modifier = if m.execution == Execution::Queued {
            let ahead = mgr_snap
                .queue_order
                .iter()
                .position(|id| id == &m.id)
                .and_then(|index| index.checked_sub(1))
                .map(|index| &mgr_snap.queue_order[index])
                .or(mgr_snap.active_job_mapping_id.as_ref());
            ahead
                .and_then(|id| mgr_snap.mappings.iter().find(|entry| &entry.id == id))
                .map_or(StatusModifier::None, |entry| StatusModifier::QueuedBehind {
                    active_name: entry.display_name.clone(),
                })
        } else if let Some(stopped) = &m.stopped_first_sync {
            StatusModifier::StoppedFirstSync(stopped.clone())
        } else if m.readiness == Readiness::RecoveryRequired {
            StatusModifier::RecoveryCause {
                kind: RecoveryCauseKind::Interrupted,
                detail: m
                    .last_run_summary
                    .clone()
                    .unwrap_or_else(|| "Recovery required".to_owned()),
                recovery_copies_count: 0,
                ambiguous_ownership: false,
            }
        } else if m.readiness == Readiness::Ready
            && m.execution == Execution::Idle
            && m.last_run_id.is_none()
        {
            StatusModifier::NeverCompleted
        } else {
            StatusModifier::None
        };

        if mgr_snap.active_job_mapping_id.as_ref() == Some(&m.id)
            && matches!(m.execution, Execution::Running | Execution::Stopping)
        {
            // Activity shows Transferring from live stats when present, otherwise Scanning (HIGH #14).
            let live = state
                .live_progress
                .as_ref()
                .filter(|p| p.mapping_id == m.id);
            let elapsed_secs = live.map(|p| p.started_at.elapsed().as_secs()).unwrap_or(0);
            let phase = if m.execution == Execution::Stopping {
                RunPhase::Stopping { elapsed_secs }
            } else if let Some(p) = live
                && p.bytes_transferred > 0
            {
                if let Some(total) = p.total_bytes
                    && total > 0
                {
                    let pct = ((p.bytes_transferred.saturating_mul(100)) / total).min(100) as u8;
                    RunPhase::TransferringKnown {
                        transferred_label: format_bytes(p.bytes_transferred),
                        total_label: format_bytes(total),
                        percent: pct,
                        elapsed_secs,
                    }
                } else {
                    RunPhase::TransferringUnknown {
                        transferred_label: format_bytes(p.bytes_transferred),
                        elapsed_secs,
                    }
                }
            } else {
                RunPhase::Scanning { elapsed_secs }
            };
            let run_id = live
                .map(|p| p.run_id.clone())
                .or_else(|| m.last_run_id.clone())
                .unwrap_or_else(|| RunId::new("run-active"));
            active_run = Some(ActiveRunSnapshot {
                run_id,
                mapping_id: m.id.clone(),
                mapping_name: m.display_name.clone(),
                volume_name: m.volume_name.clone(),
                started_label: "just now".to_owned(),
                trigger_label: String::new(),
                phase,
            });
        }

        let acct_email = accounts_raw
            .iter()
            .find(|a| a.id == m.account_id)
            .map(|a| a.display_name.clone())
            .unwrap_or_else(|| m.account_id.as_str().to_owned());

        let excluded_subfolders: Vec<(String, String)> = m
            .exclusions
            .iter()
            .filter(|e| e.starts_with('/') && e.ends_with("/**"))
            .map(|e| {
                let name = e.trim_start_matches('/').trim_end_matches("/**").to_owned();
                (name, "excluded".to_owned())
            })
            .collect();
        let default_pats: BTreeSet<&str> = DEFAULT_EXCLUSIONS.iter().map(|d| d.pattern).collect();
        let custom_exclusions: Vec<String> = m
            .exclusions
            .iter()
            .filter(|e| {
                !default_pats.contains(e.as_str()) && !(e.starts_with('/') && e.ends_with("/**"))
            })
            .cloned()
            .collect();

        mappings.push(MappingSnapshot {
            id: m.id.clone(),
            display_name: m.display_name.clone(),
            account_id: m.account_id.clone(),
            account_email: acct_email,
            cloud_path: format!("{}:{}", m.account_id, m.collection_display_path),
            cloud_folder_id: m.collection_folder_id.clone(),
            local_path: m.local_root.to_string_lossy().into_owned(),
            volume_id: vol_id,
            volume_name: m.volume_name.clone(),
            state: mapping_state,
            last_run,
            last_completed_label: m
                .last_run_completed_at
                .as_deref()
                .map(format_short_timestamp)
                .unwrap_or_else(|| "Never".to_owned()),
            modifier,
            unreviewed_conflicts,
            available_actions: available_actions(&mapping_state),
            scheduling_enabled: m.scheduling_enabled,
            volume_issue: (m.readiness == Readiness::VolumeUnavailable)
                .then(|| m.status_text.trim_end_matches(" (paused)").to_owned()),
            schedule_minutes: (m.schedule_interval_secs / 60).max(1) as u32,
            next_check_label: match m.execution {
                Execution::Queued => "Queued",
                Execution::Running | Execution::Stopping => "Run in progress",
                _ => "Time not available",
            }
            .to_owned(),
            next_check: m.retry_at.or(m.next_poll_at).map(|at| ScheduledCheck {
                at,
                retry: m.retry_at.is_some(),
            }),
            included_subfolders: 0,
            total_subfolders: excluded_subfolders.len(),
            included_size_label: "measured on sync".to_owned(),
            total_size_label: "local".to_owned(),
            excluded_subfolders,
            custom_exclusions,
            local_recovery_path: m.local_recovery_root.to_string_lossy().into_owned(),
            local_recovery_size_label: "Kept per run".to_owned(),
            cloud_recovery_path: m.cloud_recovery_path.clone(),
            found_symlinks: Vec::new(),
            recent_runs,
            generation: gen_num,
            fingerprint: m
                .active_generation_fingerprint
                .clone()
                .unwrap_or_else(|| "Not initialized".to_owned()),
            engine_options: engine_opts,
            is_variant_showcase: false,
        });
    }

    let volumes: Vec<VolumeSnapshot> = volumes_map.into_values().collect();
    let attention_items = build_real_attention_items(&mappings, &volumes);

    UiSnapshot {
        is_demo_mode: false,
        device_id: state.device_id.clone(),
        scheduling_paused: mgr_snap.scheduling_paused,
        demo_preset: DemoPreset::HouseholdOnly,
        accounts,
        volumes,
        mappings,
        active_run,
        queue_order: mgr_snap.queue_order,
        active_job_mapping_id: mgr_snap.active_job_mapping_id,
        attention_items,
        recent_events: state.recent_events.clone(),
        raw_log: state.raw_log.clone(),
        inspected_run: state.inspected_run.clone(),
        recovery_checklist: state.recovery_checklist.clone(),
        cloud_catalog: state.cloud_catalog.clone(),
        skipped_cloud_folders: state.skipped_cloud_folders.clone(),
        cloud_catalog_error: state.cloud_catalog_error.clone(),
        folder_size_pending: state.pending_folder_size.is_some(),
        wizard: state.wizard.clone(),
        preferences: state.preferences.clone(),
        reconnecting_account: state.reconnecting_account.clone(),
        account_operation_pending: state.account_worker.is_some(),
        authorization_canceling: state.reconnecting_account.is_some()
            && state.authorization_canceller.is_none(),
        can_reopen_authorization: state.authorization_url.is_some(),
        status_notice: state.status_notice.clone(),
        action_error: state.action_error.clone(),
    }
}

fn project_account(
    acct: &AccountRecord,
    mappings: &[MappingStatusSnapshot],
    state: &WorkerState,
) -> AccountSnapshot {
    let mapping_count = mappings.iter().filter(|m| m.account_id == acct.id).count();
    let sign_in_held = mappings
        .iter()
        .any(|m| m.account_id == acct.id && m.readiness == Readiness::AuthenticationRequired);
    let is_drive = acct.kind == "drive";
    let status = if !is_drive && acct.kind != "local_fixture" {
        AccountStatus::Unverified
    } else if state.reconnecting_account.as_ref() == Some(&acct.id) {
        AccountStatus::Reconnecting
    } else {
        match acct.auth_status.as_str() {
            "keychain_locked" => AccountStatus::KeychainLocked,
            "authentication_required" => AccountStatus::SignInRequired,
            "ready" if state.unverified_accounts.contains(&acct.id) => AccountStatus::Unverified,
            "ready" if !sign_in_held => AccountStatus::Connected,
            "ready" => AccountStatus::SignInRequired,
            _ => AccountStatus::Unverified,
        }
    };
    let access_summary = if acct.kind == "local_fixture" {
        "Local test folder account.".to_owned()
    } else if !is_drive {
        "Configuration imported. Folder access is unavailable for this account type.".to_owned()
    } else {
        match status {
            AccountStatus::Connected if is_drive => format!(
                "Shared folder listing verified · {}",
                format_short_timestamp(&acct.updated_at)
            ),
            AccountStatus::Connected => "Account configuration is available.".to_owned(),
            AccountStatus::SignInRequired => "Reconnect to check access again.".to_owned(),
            AccountStatus::KeychainLocked => "Unlock Keychain, then check access again.".to_owned(),
            AccountStatus::Reconnecting => "Waiting for browser sign-in.".to_owned(),
            AccountStatus::Unverified => {
                "Folder access has not been verified. Check access or reconnect.".to_owned()
            }
        }
    };
    AccountSnapshot {
        id: acct.id.clone(),
        email: acct.display_name.clone(),
        shared_parent_label: acct.remote_root_folder_id.clone(),
        status,
        access_summary,
        mapping_count,
        is_drive,
    }
}

fn build_volume_snapshot(
    vol_id: &VolumeIdentity,
    mapping: &MappingStatusSnapshot,
) -> VolumeSnapshot {
    let connected =
        mapping.volume_space.is_some() || mapping.readiness != Readiness::VolumeUnavailable;
    let filesystem = mapping
        .volume_filesystem
        .clone()
        .unwrap_or_else(|| "Unknown".to_owned());
    let (total_gb, free_gb, free_label, total_label, used_percent, low_space) =
        if let Some(space) = mapping.volume_space {
            let t_gb = (space.total_bytes / 1_000_000_000).max(1);
            let f_gb = space.free_bytes / 1_000_000_000;
            let used = space.total_bytes.saturating_sub(space.free_bytes);
            let pct = (u128::from(used) * 100 / u128::from(space.total_bytes.max(1))) as u8;
            let low = is_low_space(space.free_bytes, pct);
            (
                t_gb,
                f_gb,
                format_bytes(space.free_bytes),
                format_bytes(space.total_bytes),
                pct,
                low,
            )
        } else {
            (0, 0, "Unknown".to_owned(), "Unknown".to_owned(), 0, false)
        };

    VolumeSnapshot {
        identity: vol_id.clone(),
        display_name: mapping.volume_name.clone(),
        filesystem,
        connected,
        total_gb,
        free_gb,
        free_bytes: mapping.volume_space.map(|space| space.free_bytes),
        free_label,
        total_label,
        used_percent,
        low_space,
        last_seen_label: None,
    }
}

fn project_run_record(r: RunRecord) -> RunRecordSnapshot {
    let (result_label, result_tone, result_icon) = match r.outcome_kind.as_deref() {
        Some("success") => ("Completed".to_owned(), Tone::Ok, IconKind::Check),
        Some("success_with_conflicts") => (
            "Completed with conflicts".to_owned(),
            Tone::Attn,
            IconKind::Bang,
        ),
        Some("first_sync_stopped") => (
            "First sync stopped".to_owned(),
            Tone::Neutral,
            IconKind::Stop,
        ),
        Some("interrupted" | "force_stopped" | "canceled_before_spawn") => {
            ("Interrupted".to_owned(), Tone::Err, IconKind::Octagon)
        }
        Some("deletion_guard_aborted") => (
            "Stopped by deletion limit".to_owned(),
            Tone::Err,
            IconKind::Octagon,
        ),
        _ if r.status == "running" || r.status == "starting" => {
            ("Running".to_owned(), Tone::Run, IconKind::Sync)
        }
        _ => ("Needs review".to_owned(), Tone::Err, IconKind::Alert),
    };
    let changes_label = if r.conflict_count > 0 {
        format!(
            "{} conflicts · {} checked · {} transferred",
            r.conflict_count, r.files_checked, r.files_transferred
        )
    } else {
        format!(
            "{} checked · {} transferred",
            r.files_checked, r.files_transferred
        )
    };
    RunRecordSnapshot {
        run_id: r.id,
        started_label: format_short_timestamp(&r.started_at),
        duration_label: match r.finished_at.as_deref() {
            Some(fin) => format_duration_between(&r.started_at, fin),
            None => "in progress".to_owned(),
        },
        result_label,
        result_tone,
        result_icon,
        changes_label,
    }
}

fn build_real_attention_items(
    mappings: &[MappingSnapshot],
    volumes: &[VolumeSnapshot],
) -> Vec<AttentionItem> {
    let mut items = Vec::new();
    for m in mappings {
        if matches!(&m.modifier, StatusModifier::StoppedFirstSync(stopped) if stopped.cause.is_none())
            && m.state.readiness() == Readiness::RecoveryRequired
        {
            continue;
        }
        match m.state.readiness() {
            Readiness::AuthenticationRequired => {
                items.push(AttentionItem {
                    id: format!("attn-auth-{}", m.id),
                    priority: AttentionPriority::SignIn,
                    mapping_id: Some(m.id.clone()),
                    mapping_name: Some(m.display_name.clone()),
                    pill_label: "Sign-in required".to_owned(),
                    pill_tone: Tone::Err,
                    icon: IconKind::Key,
                    is_error_accent: true,
                    title: format!("Sign-in required · {}", m.display_name),
                    cause: format!("Sign-in for {} is required.", m.account_id),
                    what_did: format!("Held {}. Nothing was changed or deleted.", m.display_name),
                    next_step: "Reconnect the account in Accounts.".to_owned(),
                    kind: AttentionKind::Authentication {
                        account_id: m.account_id.clone(),
                        keychain_locked: false,
                    },
                });
            }
            Readiness::RecoveryRequired => {
                items.push(AttentionItem {
                    id: format!("attn-rec-{}", m.id),
                    priority: AttentionPriority::Recovery,
                    mapping_id: Some(m.id.clone()),
                    mapping_name: Some(m.display_name.clone()),
                    pill_label: "Recovery required".to_owned(),
                    pill_tone: Tone::Err,
                    icon: IconKind::Octagon,
                    is_error_accent: true,
                    title: format!("{} · recovery required", m.display_name),
                    cause: match &m.modifier {
                        StatusModifier::StoppedFirstSync(stopped) => stopped.cause.clone(),
                        _ => None,
                    }
                    .unwrap_or_else(|| {
                        "An earlier run was interrupted or hit a safety stop.".to_owned()
                    }),
                    what_did:
                        "Kept the sync state and recovery copies intact and held this mapping."
                            .to_owned(),
                    next_step: "Open Start recovery… to inspect the run log and recovery copies."
                        .to_owned(),
                    kind: AttentionKind::RecoveryRequired {
                        cause_kind: RecoveryCauseKind::Interrupted,
                    },
                });
            }
            Readiness::EndpointChanged => {
                items.push(AttentionItem {
                    id: format!("attn-ep-{}", m.id),
                    priority: AttentionPriority::EndpointChanged,
                    mapping_id: Some(m.id.clone()),
                    mapping_name: Some(m.display_name.clone()),
                    pill_label: "Review needed".to_owned(),
                    pill_tone: Tone::Attn,
                    icon: IconKind::Bang,
                    is_error_accent: false,
                    title: format!("{} · folder changed", m.display_name),
                    cause: "The folder or its access marker no longer matches setup.".to_owned(),
                    what_did: "Stopped before syncing so nothing is overwritten.".to_owned(),
                    next_step: "Check the folder and choose Check again.".to_owned(),
                    kind: AttentionKind::EndpointChanged {
                        is_cloud: false,
                        expected_id: m.cloud_folder_id.clone(),
                        found_id: "Changed".to_owned(),
                    },
                });
            }
            Readiness::VolumeUnavailable => items.push(m.volume_attention()),
            Readiness::Ready => {
                if let Some(unrev) = &m.unreviewed_conflicts {
                    items.push(AttentionItem {
                        id: format!("attn-conf-{}", m.id),
                        priority: AttentionPriority::Conflicts,
                        mapping_id: Some(m.id.clone()),
                        mapping_name: Some(m.display_name.clone()),
                        pill_label: "Conflicts".to_owned(),
                        pill_tone: Tone::Attn,
                        icon: IconKind::Bang,
                        is_error_accent: false,
                        title: format!("{} · completed with conflicts", m.display_name),
                        cause: "The same file changed on both sides before syncing.".to_owned(),
                        what_did: "Kept both versions so nothing was lost.".to_owned(),
                        next_step: "Review the conflict copies, then choose Mark as reviewed."
                            .to_owned(),
                        kind: AttentionKind::Conflicts {
                            conflict_paths: unrev.paths.clone(),
                        },
                    });
                }
            }
            Readiness::Unconfigured | Readiness::NeedsInitialization => {}
        }
    }
    items.extend(volumes.iter().filter_map(|volume| {
        AttentionItem::low_space_advisory(volume, mappings, |path| {
            detect_volume_identity(Path::new(path))
                .is_ok_and(|recovery| recovery.identity == volume.identity)
        })
    }));
    items.sort_by_key(|i| i.priority);
    items
}

fn build_redacted_diagnostics_text(state: &WorkerState) -> String {
    let mut out = String::new();
    out.push_str("FolderSync Diagnostics (no file contents and no sign-in data)\n");
    out.push_str(&format!("App version: {}\n", state.preferences.app_version));
    out.push_str(&format!("Engine: {}\n", state.preferences.engine_version));
    out.push_str(&format!("NSH: {}\n\n", state.preferences.nsh_version));
    if let Some(err) = &state.action_error {
        out.push_str(&format!("Latest UI error: {err}\n\n"));
    }
    if let Some(mgr) = &state.mgr {
        let snap = mgr.snapshot();
        out.push_str(&format!("Snapshot sequence: {}\n", snap.sequence));
        out.push_str(&format!("Scheduling paused: {}\n", snap.scheduling_paused));
        out.push_str(&format!("Mappings: {}\n", snap.mappings.len()));
        for m in &snap.mappings {
            out.push_str(&format!(
                "- mapping={} readiness={:?} execution={:?} intent={:?} gen_fp={}\n",
                m.id,
                m.readiness,
                m.execution,
                m.intent,
                m.active_generation_fingerprint.as_deref().unwrap_or("none")
            ));
            if let Ok(runs) = mgr.list_recent_runs(&m.id, 5)
                && !runs.is_empty()
            {
                out.push_str("  Recent runs:\n");
                for r in runs {
                    out.push_str(&format!(
                        "    * run={} mode={} status={} outcome={} detail={}\n",
                        r.id,
                        r.mode,
                        r.status,
                        r.outcome_kind.as_deref().unwrap_or("none"),
                        r.outcome_detail.as_deref().unwrap_or("none")
                    ));
                }
            }
            if let Ok(report) = mgr.inspect_recovery(m.id.clone())
                && !report.open_cases.is_empty()
            {
                out.push_str("  Open recovery cases:\n");
                for c in report.open_cases {
                    out.push_str(&format!(
                        "    * case={} reason={} cause={} next_step={}\n",
                        c.case.id, c.case.reason_kind, c.case.cause, c.case.next_step
                    ));
                }
            }
        }
    }
    out
}

fn push_ui_event(state: &mut WorkerState, message: String, link: Option<(String, ScreenRoute)>) {
    let id = state.next_event_id;
    state.next_event_id += 1;
    let time_label = chrono::Utc::now().format("%H:%M").to_string();
    state.recent_events.insert(
        0,
        ActivityEvent {
            id,
            time_label,
            message,
            link,
        },
    );
    state.recent_events.truncate(200);
}

fn sanitize_id(raw: &str) -> String {
    let mut out = String::new();
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if (ch == '-' || ch == '_' || ch == ' ') && !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-').to_owned();
    if trimmed.is_empty() {
        "mapping-1".to_owned()
    } else {
        trimmed
    }
}

fn format_short_timestamp(rfc3339: &str) -> String {
    DateTime::parse_from_rfc3339(rfc3339)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%d %b %H:%M")
                .to_string()
        })
        .unwrap_or_else(|_| rfc3339.to_owned())
}

fn format_duration_between(start: &str, end: &str) -> String {
    let Ok(s) = DateTime::parse_from_rfc3339(start) else {
        return "—".to_owned();
    };
    let Ok(e) = DateTime::parse_from_rfc3339(end) else {
        return "—".to_owned();
    };
    let secs = (e - s).num_seconds().max(0) as u64;
    crate::contract::format_duration(secs)
}

fn default_real_wizard() -> WizardDraft {
    WizardDraft {
        changing_subfolders: false,
        filter_review: None,
        volume_free_bytes: None,
        step: WizardStep::Account,
        editing_mapping_id: None,
        selected_account: AccountId::new("local-1"),
        selected_folder: String::new(),
        remote_folder_path: None,
        drive_collection: None,
        local_path: String::new(),
        location_check_pending: false,
        volume_name: "unknown".to_owned(),
        volume_kind: VolumeKind::Internal,
        volume_fs: "unknown".to_owned(),
        volume_free_gb: 0,
        volume_total_gb: 1,
        location_checks: Vec::new(),
        subfolders: Vec::new(),
        custom_patterns: Vec::new(),
        show_default_exclusions: false,
        local_recovery_path: String::new(),
        cloud_recovery_path: String::new(),
        recovery_checks: vec![LocationCheck {
            ok: false,
            message: "Choose a local recovery folder outside every synced folder.".to_owned(),
        }],
        review_scenario: ReviewScenario::BothEmpty,
        cloud_file_count: None,
        cloud_total_bytes: None,
        local_file_count: None,
        local_total_bytes: None,
        marker_reused: false,
        found_symlinks: Vec::new(),
        found_name_issues: Vec::new(),
        blocked_populated_pair: None,
        no_other_sync_confirmed: false,
        reviewed_fingerprint: None,
        first_sync_mapping_id: None,
        first_sync_preflight_done: false,
        first_sync_markers_ready: false,
        first_sync_stopping: false,
        first_sync_percent: 0,
        first_sync_file_progress: None,
        first_sync_elapsed_secs: 0,
        first_sync_completed: false,
        first_sync_error: None,
    }
}

fn default_real_preferences() -> PreferencesSnapshot {
    PreferencesSnapshot {
        open_at_login: false,
        open_at_login_pending: false,
        notifications_enabled: false,
        finder_extension: FinderExtensionStatus::Disabled,
        bandwidth_limit_mb_s: None,
        app_version: format!("{} (dev)", env!("CARGO_PKG_VERSION")),
        engine_version: format!("rclone {} (pinned)", rclone_adapter::PINNED_RCLONE_VERSION),
        engine_checksum: "verified against packaging/qualification-manifest.toml".to_owned(),
        nsh_version: "NSH 0.6.1".to_owned(),
        system_summary: format!("{} ({})", std::env::consts::OS, std::env::consts::ARCH),
    }
}

fn empty_loading_snapshot() -> UiSnapshot {
    UiSnapshot {
        is_demo_mode: false,
        device_id: "this-mac".to_owned(),
        scheduling_paused: false,
        demo_preset: DemoPreset::HouseholdOnly,
        accounts: Vec::new(),
        volumes: Vec::new(),
        mappings: Vec::new(),
        active_run: None,
        queue_order: Vec::new(),
        active_job_mapping_id: None,
        attention_items: Vec::new(),
        recent_events: Vec::new(),
        raw_log: Vec::new(),
        inspected_run: None,
        recovery_checklist: None,
        cloud_catalog: Vec::new(),
        skipped_cloud_folders: Vec::new(),
        cloud_catalog_error: None,
        folder_size_pending: false,
        wizard: default_real_wizard(),
        preferences: default_real_preferences(),
        reconnecting_account: None,
        account_operation_pending: false,
        authorization_canceling: false,
        can_reopen_authorization: false,
        status_notice: None,
        action_error: None,
    }
}
