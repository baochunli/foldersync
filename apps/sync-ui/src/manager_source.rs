//! Real `sync-manager` adapter for the NSH desktop UI (`ManagerSource`).
//!
//! `ManagerSource` owns a [`SyncManager`] strictly on a dedicated background
//! worker thread so the NSH UI thread never locks manager state or blocks on
//! filesystem/SQLite/process I/O. Updates arrive over a channel and are
//! marshaled onto the main thread with `run_on_main_thread`.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    thread::{self, JoinHandle},
    time::Duration,
};

use chrono::DateTime;
use rclone_adapter::InitPreference;
use sync_core::{
    ACCESS_MARKER_FILENAME, AccountId, CloudCollection, DEFAULT_EXCLUSIONS, Execution, Intent,
    LastRunSummary, MappingId, MappingState, Readiness, RunId, VolumeIdentity, VolumeKind,
    WallTime, available_actions,
};
use sync_manager::{
    AccountRecord, AddMappingCommand, BisyncLauncher, ControlledShutdownOutcome,
    DarwinFinderNotifier, FinderActionDispatch, FinderNotifier, InitializeCommand, ManagerConfig,
    ManagerError, ManagerEvent, MappingStatusSnapshot, RecoveryCopySide, RunRecord, StatusSnapshot,
    SyncManager, ValidatedFinderAction, disk_space_for_path, inspect_local_endpoint_detailed,
};

use crate::{
    contract::{
        AccountSnapshot, AccountStatus, ActiveRunSnapshot, ActivityEvent, AttentionItem,
        AttentionKind, AttentionPriority, CloudFolderCandidate, DemoPreset, FinderExtensionStatus,
        IconKind, InspectedRunSnapshot, LocationCheck, MappingSnapshot, PreferencesSnapshot,
        RecoveryCauseKind, RecoveryChecklistData, RecoveryCopyItem, ReviewScenario, RunPhase,
        RunRecordSnapshot, ScreenRoute, StatusModifier, SubfolderNode, Tone, UiCommand, UiSnapshot,
        UnreviewedConflicts, VolumeSnapshot, WizardDraft, WizardStep,
    },
    system_actions::{LaunchAtLoginOutcome, SystemActions},
};

const DEFAULT_RECENT_RUNS_FOR_UI: usize = 15;
const DEFAULT_RUN_LOG_MAX_LINES: usize = 500;
const DEFAULT_QUIT_TIMEOUT: Duration = Duration::from_secs(85);

/// Messages pushed from the manager worker thread to the NSH UI thread.
#[derive(Debug, Clone)]
pub enum UiUpdate {
    Snapshot(Box<UiSnapshot>),
    Navigate(ScreenRoute),
    ShowWindowAndNavigate(ScreenRoute),
    QuitReady(ControlledShutdownOutcome),
}

/// Configuration for creating a [`ManagerSource`].
pub struct ManagerSourceConfig {
    pub state_dir: Option<PathBuf>,
    pub finder_status_dir: Option<PathBuf>,
    pub launcher: Option<Arc<dyn BisyncLauncher>>,
    pub finder_notifier: Option<Arc<dyn FinderNotifier>>,
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
            finder_notifier: Some(Arc::new(DarwinFinderNotifier)),
            shutdown_timeout: DEFAULT_QUIT_TIMEOUT,
            finder_poll_interval: Some(Duration::from_millis(500)),
        }
    }
}

enum WorkerMsg {
    Command(UiCommand),
    ManagerSnapshot(Arc<StatusSnapshot>),
    ManagerEvent(ManagerEvent),
    DrainFinderActions,
    ControlledShutdown { timeout: Duration },
    ShutdownNow,
}

/// Real manager-backed data source owning `SyncManager` on a background worker thread.
pub struct ManagerSource {
    cmd_tx: mpsc::Sender<WorkerMsg>,
    updates_rx: Arc<Mutex<mpsc::Receiver<UiUpdate>>>,
    latest_cached: Arc<Mutex<UiSnapshot>>,
    shutdown_timeout: Duration,
    worker_handle: Mutex<Option<JoinHandle<()>>>,
}

impl ManagerSource {
    /// Spawns the background manager worker thread and returns immediately
    /// without blocking the NSH thread on SQLite, filesystem, or rclone I/O.
    pub fn spawn(config: ManagerSourceConfig, system_actions: Arc<dyn SystemActions>) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<WorkerMsg>();
        let (ui_tx, ui_rx) = mpsc::channel::<UiUpdate>();
        let initial_snapshot = empty_loading_snapshot();
        let latest_cached = Arc::new(Mutex::new(initial_snapshot));
        let cached_for_worker = Arc::clone(&latest_cached);
        let cmd_tx_for_worker = cmd_tx.clone();
        let shutdown_timeout = config.shutdown_timeout;

        let handle = thread::Builder::new()
            .name("foldersync-ui-manager".to_string())
            .spawn(move || {
                manager_worker_main(
                    config,
                    system_actions,
                    cmd_tx_for_worker,
                    cmd_rx,
                    ui_tx,
                    cached_for_worker,
                );
            })
            .expect("failed to spawn foldersync-ui-manager worker thread");

        Self {
            cmd_tx,
            updates_rx: Arc::new(Mutex::new(ui_rx)),
            latest_cached,
            shutdown_timeout,
            worker_handle: Mutex::new(Some(handle)),
        }
    }

    /// Returns a cloned snapshot immediately without touching manager locks.
    pub fn snapshot(&self) -> UiSnapshot {
        self.latest_cached.lock().unwrap().clone()
    }

    /// Returns the UI update receiver (consumed off the NSH thread via `blocking`
    /// and marshaled onto the main thread with `run_on_main_thread`).
    pub fn updates_receiver(&self) -> Arc<Mutex<mpsc::Receiver<UiUpdate>>> {
        Arc::clone(&self.updates_rx)
    }

    /// Non-blocking dispatch of a [`UiCommand`] to the manager worker thread.
    pub fn dispatch(&self, cmd: UiCommand) {
        let _ = self.cmd_tx.send(WorkerMsg::Command(cmd));
    }

    /// Triggers the single controlled-stop quit flow on the manager worker thread.
    pub fn request_controlled_shutdown(&self) {
        let _ = self.cmd_tx.send(WorkerMsg::ControlledShutdown {
            timeout: self.shutdown_timeout,
        });
    }

    /// Triggers Finder action file drain immediately (useful in deterministic tests).
    pub fn poll_finder_actions(&self) {
        let _ = self.cmd_tx.send(WorkerMsg::DrainFinderActions);
    }

    /// Gracefully stops the worker thread and closes `SyncManager`.
    pub fn shutdown(&self) {
        let _ = self.cmd_tx.send(WorkerMsg::ShutdownNow);
        if let Some(handle) = self.worker_handle.lock().unwrap().take() {
            let _ = handle.join();
        }
    }
}

impl Drop for ManagerSource {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct WorkerState {
    mgr: Option<SyncManager>,
    system_actions: Arc<dyn SystemActions>,
    local_account_parents: BTreeMap<AccountId, PathBuf>,
    wizard: WizardDraft,
    cloud_catalog: Vec<CloudFolderCandidate>,
    cloud_catalog_measured_at: String,
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
    system_actions: Arc<dyn SystemActions>,
    self_tx: mpsc::Sender<WorkerMsg>,
    cmd_rx: mpsc::Receiver<WorkerMsg>,
    ui_tx: mpsc::Sender<UiUpdate>,
    latest_cached: Arc<Mutex<UiSnapshot>>,
) {
    let mut mgr_cfg = match config.state_dir {
        Some(dir) => ManagerConfig::with_state_dir(dir),
        None => ManagerConfig {
            state_dir: None,
            finder_status_dir: None,
            finder_notifier: Arc::new(DarwinFinderNotifier),
            clock: Arc::new(sync_manager::SystemClock),
            process_inspector: Arc::new(sync_manager::OsProcessInspector),
            launcher: Arc::new(sync_manager::RealBisyncLauncher::new(None)),
            event_buffer_capacity: sync_manager::snapshot::DEFAULT_EVENT_BUFFER_CAPACITY,
        },
    };
    mgr_cfg.finder_status_dir = config.finder_status_dir;
    if let Some(launcher) = config.launcher {
        mgr_cfg.launcher = launcher;
    }
    if let Some(notifier) = config.finder_notifier {
        mgr_cfg.finder_notifier = notifier;
    }

    let mut state = WorkerState {
        mgr: None,
        system_actions,
        local_account_parents: BTreeMap::new(),
        wizard: default_real_wizard(),
        cloud_catalog: Vec::new(),
        cloud_catalog_measured_at: "not measured yet".to_owned(),
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
    let stop_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));

    match SyncManager::open(mgr_cfg) {
        Ok(mgr) => {
            let snap_rx = mgr.subscribe_snapshots(1);
            let ev_rx = mgr.subscribe_events(256);
            let tx_snap = self_tx.clone();
            snap_fwd_handle = Some(thread::spawn(move || {
                while let Ok(snap) = snap_rx.recv() {
                    if tx_snap.send(WorkerMsg::ManagerSnapshot(snap)).is_err() {
                        break;
                    }
                }
            }));
            let tx_ev = self_tx.clone();
            ev_fwd_handle = Some(thread::spawn(move || {
                while let Ok(ev) = ev_rx.recv() {
                    if tx_ev.send(WorkerMsg::ManagerEvent(ev)).is_err() {
                        break;
                    }
                }
            }));
            if let Some(interval) = config.finder_poll_interval {
                let tx_poll = self_tx.clone();
                let flag = Arc::clone(&stop_flag);
                finder_poll_handle = Some(thread::spawn(move || {
                    while !flag.load(std::sync::atomic::Ordering::Relaxed) {
                        thread::sleep(interval);
                        if flag.load(std::sync::atomic::Ordering::Relaxed) {
                            break;
                        }
                        if tx_poll.send(WorkerMsg::DrainFinderActions).is_err() {
                            break;
                        }
                    }
                }));
            }
            state.mgr = Some(mgr);
            emit_snapshot(&mut state, None, &ui_tx, &latest_cached);
        }
        Err(err) => {
            state.action_error = Some(format!("{} {}", err.cause(), err.next_step()));
            emit_snapshot(&mut state, None, &ui_tx, &latest_cached);
        }
    }

    while let Ok(msg) = cmd_rx.recv() {
        match msg {
            WorkerMsg::ManagerSnapshot(snap) => {
                // Keep first-sync wizard state in sync with manager progress.
                if let Some(ref mid) = state.wizard.first_sync_mapping_id
                    && let Some(m) = snap.mappings.iter().find(|m| &m.id == mid)
                    && m.readiness == Readiness::Ready
                    && m.execution == Execution::Idle
                {
                    state.wizard.first_sync_completed = true;
                    state.wizard.first_sync_percent = 100;
                }
                emit_snapshot(&mut state, Some(&snap), &ui_tx, &latest_cached);
            }
            WorkerMsg::ManagerEvent(ev) => {
                handle_manager_event(&mut state, ev);
                emit_snapshot(&mut state, None, &ui_tx, &latest_cached);
            }
            WorkerMsg::DrainFinderActions => {
                let mut ui_actions = Vec::new();
                if let Some(mgr) = &state.mgr {
                    let dispatches = mgr.drain_finder_actions();
                    for d in dispatches {
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
                                state.action_error = Some(format!(
                                    "Finder action ({action:?}) could not run: {cause}"
                                ));
                            }
                            FinderActionDispatch::Rejected { file_name, reason } => {
                                push_ui_event(
                                    &mut state,
                                    format!(
                                        "Ignored invalid Finder action `{file_name}`: {reason}"
                                    ),
                                    None,
                                );
                            }
                        }
                    }
                }
                for act in ui_actions {
                    match act {
                        ValidatedFinderAction::OpenApp { mapping_id, .. } => {
                            let route = match mapping_id {
                                Some(id) => ScreenRoute::MappingDetail(id),
                                None => ScreenRoute::Overview,
                            };
                            let _ = ui_tx.send(UiUpdate::ShowWindowAndNavigate(route));
                        }
                        ValidatedFinderAction::ShowLastRun { mapping_id, .. } => {
                            if let Some(mgr) = &state.mgr
                                && let Some(run_id) = mgr.last_run_id(&mapping_id)
                            {
                                load_inspected_run(&mut state, &mapping_id, &run_id);
                                emit_snapshot(&mut state, None, &ui_tx, &latest_cached);
                                let _ = ui_tx.send(UiUpdate::ShowWindowAndNavigate(
                                    ScreenRoute::RunInspection { mapping_id, run_id },
                                ));
                            } else {
                                let _ = ui_tx.send(UiUpdate::ShowWindowAndNavigate(
                                    ScreenRoute::MappingDetail(mapping_id),
                                ));
                            }
                        }
                        _ => {}
                    }
                }
            }
            WorkerMsg::Command(cmd) => {
                let nav = handle_ui_command(&mut state, cmd);
                emit_snapshot(&mut state, None, &ui_tx, &latest_cached);
                if let Some(route) = nav {
                    let _ = ui_tx.send(UiUpdate::Navigate(route));
                }
            }
            WorkerMsg::ControlledShutdown { timeout } => {
                stop_flag.store(true, std::sync::atomic::Ordering::Relaxed);
                let outcome = if let Some(mgr) = state.mgr.take() {
                    let res = mgr
                        .controlled_shutdown(timeout)
                        .unwrap_or(ControlledShutdownOutcome::Clean);
                    // Drop manager cleanly (releases OS lock after SQLite closes).
                    drop(mgr);
                    res
                } else {
                    ControlledShutdownOutcome::Clean
                };
                let _ = ui_tx.send(UiUpdate::QuitReady(outcome));
                break;
            }
            WorkerMsg::ShutdownNow => {
                stop_flag.store(true, std::sync::atomic::Ordering::Relaxed);
                if let Some(mgr) = state.mgr.take() {
                    let _ = mgr.shutdown();
                }
                break;
            }
        }
    }

    stop_flag.store(true, std::sync::atomic::Ordering::Relaxed);
    drop(state.mgr.take());
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

fn handle_manager_event(state: &mut WorkerState, ev: ManagerEvent) {
    match ev {
        ManagerEvent::SnapshotUpdated { .. } => {}
        ManagerEvent::MappingAdded { mapping_id, .. } => {
            push_ui_event(
                state,
                format!("Added mapping `{mapping_id}`."),
                Some(("Open".to_owned(), ScreenRoute::MappingDetail(mapping_id))),
            );
        }
        ManagerEvent::RunStarted {
            mapping_id,
            run_id,
            mode,
            ..
        } => {
            state.raw_log.clear();
            push_ui_event(
                state,
                format!("Started {mode} run for `{mapping_id}`."),
                Some((
                    "Show run".to_owned(),
                    ScreenRoute::RunInspection { mapping_id, run_id },
                )),
            );
        }
        ManagerEvent::RunOutputLine { line, .. } => {
            state.raw_log.push(line);
            if state.raw_log.len() > DEFAULT_RUN_LOG_MAX_LINES {
                let excess = state.raw_log.len() - DEFAULT_RUN_LOG_MAX_LINES;
                state.raw_log.drain(0..excess);
            }
        }
        ManagerEvent::RunFinished {
            mapping_id,
            run_id,
            outcome,
            ..
        } => {
            push_ui_event(
                state,
                format!("Finished run for `{mapping_id}` ({outcome})."),
                Some((
                    "Show run".to_owned(),
                    ScreenRoute::RunInspection { mapping_id, run_id },
                )),
            );
        }
        ManagerEvent::RecoveryOpened {
            mapping_id, cause, ..
        } => {
            push_ui_event(
                state,
                format!("Recovery needed for `{mapping_id}`: {cause}"),
                Some((
                    "Start recovery…".to_owned(),
                    ScreenRoute::Recovery(mapping_id),
                )),
            );
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
            }
            state.action_error = Some(full.clone());
            push_ui_event(state, format!("First sync check failed: {cause}"), None);
        }
        ManagerEvent::ShutdownTimedOut { mapping_id, .. } => {
            push_ui_event(
                state,
                format!(
                    "Quit timed out while stopping `{mapping_id}`; recovery required on restart."
                ),
                None,
            );
        }
    }
}

fn handle_ui_command(state: &mut WorkerState, cmd: UiCommand) -> Option<ScreenRoute> {
    state.status_notice = None;
    if !matches!(cmd, UiCommand::DismissError) {
        state.action_error = None;
    }

    match cmd {
        UiCommand::DismissError => {
            state.action_error = None;
            None
        }
        UiCommand::PollFinderActions => None,
        UiCommand::SyncNow(id) => {
            if let Some(mgr) = &state.mgr {
                record_mgr_result(state, mgr.run_now(id));
            }
            None
        }
        UiCommand::SyncReadyMappings => {
            if let Some(mgr) = &state.mgr {
                record_mgr_result(state, mgr.sync_ready_mappings());
            }
            None
        }
        UiCommand::PauseMapping(id) => {
            if let Some(mgr) = &state.mgr {
                record_mgr_result(state, mgr.pause(id));
            }
            None
        }
        UiCommand::ResumeMapping(id) => {
            if let Some(mgr) = &state.mgr {
                record_mgr_result(state, mgr.resume(id));
            }
            None
        }
        UiCommand::StopRun(id) => {
            if let Some(mgr) = &state.mgr {
                record_mgr_result(state, mgr.stop_run(id));
            }
            None
        }
        UiCommand::ForceStopRun(id) => {
            if let Some(mgr) = &state.mgr {
                record_mgr_result(state, mgr.force_stop(id));
            }
            None
        }
        UiCommand::SetSchedulingPaused(paused) => {
            if let Some(mgr) = &state.mgr {
                record_mgr_result(state, mgr.set_scheduling_paused(paused));
            }
            None
        }
        UiCommand::MarkConflictsReviewed(id) => {
            if let Some(mgr) = &state.mgr {
                record_mgr_result(state, mgr.acknowledge_conflicts(id));
            }
            None
        }
        UiCommand::CheckAgain(id) => {
            if let Some(mgr) = &state.mgr {
                record_mgr_result(state, mgr.check_again(id));
            }
            None
        }
        UiCommand::InspectRecovery(id) => {
            load_recovery_checklist(state, &id);
            None
        }
        UiCommand::InspectRun { mapping_id, run_id } => {
            load_inspected_run(state, &mapping_id, &run_id);
            None
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
            None
        }
        UiCommand::CompleteRecovery {
            mapping_id,
            resume_and_run,
        } => {
            if let Some(mgr) = &state.mgr {
                match mgr.complete_recovery(mapping_id.clone(), resume_and_run) {
                    Ok(_) => {
                        state.recovery_checklist = None;
                        return Some(ScreenRoute::Overview);
                    }
                    Err(err) => {
                        state.action_error = Some(format!("{} {}", err.cause(), err.next_step()));
                    }
                }
            }
            None
        }
        UiCommand::RemoveMapping(id) | UiCommand::DiscardDraft(id) => {
            if let Some(mgr) = &state.mgr {
                record_mgr_result(state, mgr.remove_mapping(id));
            }
            None
        }
        UiCommand::ResumeDraft(id) => {
            if load_mapping_into_wizard(state, &id, false) {
                return Some(ScreenRoute::AddMapping);
            }
            None
        }
        UiCommand::FinishSetup(id) => {
            if load_mapping_into_wizard(state, &id, true) {
                return Some(ScreenRoute::AddMapping);
            }
            None
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
            None
        }
        UiCommand::ConnectDemoAccount => {
            // Honest copy: real Google Drive OAuth lives in drive-t4 (not on main yet).
            state.action_error = Some(
                "Google Drive sign-in is not connected in this build yet. Add a local test folder account in Accounts to exercise real local sync."
                    .to_owned(),
            );
            None
        }
        UiCommand::AddLocalFixtureAccount {
            id,
            display_name,
            parent_dir,
        } => {
            if let Some(mgr) = &state.mgr {
                let acct_id = AccountId::new(id.trim());
                match mgr.register_local_fixture_account(acct_id.clone(), display_name) {
                    Ok(_) => {
                        let parent = PathBuf::from(parent_dir.trim());
                        if parent.is_dir() {
                            state.local_account_parents.insert(acct_id.clone(), parent);
                        }
                        state.wizard.selected_account = acct_id;
                        refresh_real_catalog(state);
                    }
                    Err(err) => {
                        state.action_error = Some(format!("{} {}", err.cause(), err.next_step()));
                    }
                }
            }
            None
        }
        UiCommand::ReconnectAccount(_) => {
            state.action_error =
                Some("Google Drive browser sign-in is not connected in this build yet.".to_owned());
            None
        }
        UiCommand::CancelReconnect => None,
        UiCommand::CheckAccountAccess(account_id) => {
            state.status_notice = Some(format!(
                "Account `{account_id}` configuration is present in FolderSync."
            ));
            None
        }
        UiCommand::RemoveAccount(account_id) => {
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
            None
        }
        UiCommand::SetOpenAtLogin(enabled) => {
            match state.system_actions.set_launch_at_login(enabled) {
                Ok(LaunchAtLoginOutcome::Enabled) => {
                    state.preferences.open_at_login = true;
                    state.preferences.open_at_login_pending = false;
                }
                Ok(LaunchAtLoginOutcome::PendingApproval) => {
                    state.preferences.open_at_login = true;
                    state.preferences.open_at_login_pending = true;
                }
                Ok(LaunchAtLoginOutcome::Disabled) => {
                    state.preferences.open_at_login = false;
                    state.preferences.open_at_login_pending = false;
                }
                Ok(LaunchAtLoginOutcome::Unavailable) => {
                    state.preferences.open_at_login = false;
                    state.preferences.open_at_login_pending = false;
                    state.status_notice = Some(
                        "Open at login is available only in the packaged FolderSync app."
                            .to_owned(),
                    );
                }
                Err(err) => {
                    state.action_error = Some(err);
                }
            }
            None
        }
        UiCommand::SetNotifications(enabled) => {
            state.preferences.notifications_enabled = enabled;
            None
        }
        UiCommand::SetBandwidthLimit(limit) => {
            state.preferences.bandwidth_limit_mb_s = limit;
            None
        }
        UiCommand::WizardReset => {
            state.wizard = default_real_wizard();
            if let Some(mgr) = &state.mgr
                && let Ok(accounts) = mgr.list_accounts()
                && let Some(first) = accounts.first()
            {
                state.wizard.selected_account = first.id.clone();
                if accounts.len() <= 1 {
                    state.wizard.step = WizardStep::Folder;
                }
            }
            refresh_real_catalog(state);
            recompute_real_wizard_inventory(state);
            None
        }
        UiCommand::WizardSetStep(step) => {
            recompute_real_wizard_inventory(state);
            if state.wizard.can_visit_step(step) {
                state.wizard.step = step;
                if step >= WizardStep::Location {
                    persist_real_draft_if_ready(state);
                }
            }
            None
        }
        UiCommand::WizardSelectAccount(acct) => {
            state.wizard.selected_account = acct;
            state.wizard.no_other_sync_confirmed = false;
            refresh_real_catalog(state);
            recompute_real_wizard_inventory(state);
            None
        }
        UiCommand::WizardSelectFolder(folder) => {
            state.wizard.selected_folder = folder.clone();
            if let Some(parent) = state
                .local_account_parents
                .get(&state.wizard.selected_account)
            {
                let candidate = parent.join(&folder);
                state.wizard.remote_folder_path = Some(candidate.to_string_lossy().into_owned());
            }
            state.wizard.no_other_sync_confirmed = false;
            recompute_real_wizard_inventory(state);
            None
        }
        UiCommand::WizardCreateNewFolder(name) => {
            let trimmed = name.trim();
            if !trimmed.is_empty() && !trimmed.contains('/') && !trimmed.contains("..") {
                state.wizard.selected_folder = trimmed.to_owned();
                if let Some(parent) = state
                    .local_account_parents
                    .get(&state.wizard.selected_account)
                {
                    let new_dir = parent.join(trimmed);
                    if let Err(err) = fs::create_dir_all(&new_dir) {
                        state.action_error = Some(format!(
                            "Could not create folder `{}`: {err}.",
                            new_dir.display()
                        ));
                    } else {
                        state.wizard.remote_folder_path =
                            Some(new_dir.to_string_lossy().into_owned());
                    }
                }
                state.wizard.no_other_sync_confirmed = false;
                refresh_real_catalog(state);
                recompute_real_wizard_inventory(state);
            }
            None
        }
        UiCommand::WizardSetRemotePath(path) => {
            let p = PathBuf::from(path.trim());
            if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                state.wizard.selected_folder = name.to_owned();
            }
            state.wizard.remote_folder_path = Some(path.trim().to_owned());
            state.wizard.no_other_sync_confirmed = false;
            recompute_real_wizard_inventory(state);
            None
        }
        UiCommand::WizardSetLocalPath(path) => {
            let p = PathBuf::from(path.trim());
            state.wizard.local_path = path.trim().to_owned();
            if state.wizard.local_recovery_path.is_empty()
                && let Some(parent) = p.parent()
                && let Some(name) = p.file_name().and_then(|n| n.to_str())
            {
                state.wizard.local_recovery_path = parent
                    .join("FolderSyncRecovery")
                    .join(name)
                    .to_string_lossy()
                    .into_owned();
                state.wizard.cloud_recovery_path = parent
                    .join("FolderSyncRecovery-Remote")
                    .join(name)
                    .to_string_lossy()
                    .into_owned();
            }
            state.wizard.no_other_sync_confirmed = false;
            recompute_real_wizard_inventory(state);
            None
        }
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
            state.wizard.no_other_sync_confirmed = false;
            recompute_real_wizard_inventory(state);
            None
        }
        UiCommand::WizardToggleDefaultExclusions => {
            state.wizard.show_default_exclusions = !state.wizard.show_default_exclusions;
            None
        }
        UiCommand::WizardAddPattern(pattern) => {
            let trimmed = pattern.trim().to_owned();
            if !trimmed.is_empty() && !state.wizard.custom_patterns.contains(&trimmed) {
                state.wizard.custom_patterns.push(trimmed);
                state.wizard.no_other_sync_confirmed = false;
                recompute_real_wizard_inventory(state);
            }
            None
        }
        UiCommand::WizardSetLocalRecoveryPath(path) => {
            state.wizard.local_recovery_path = path.trim().to_owned();
            state.wizard.no_other_sync_confirmed = false;
            recompute_real_wizard_inventory(state);
            None
        }
        UiCommand::WizardSetReviewScenario(_) => None,
        UiCommand::WizardSetNoOtherSyncConfirmed(confirmed) => {
            state.wizard.no_other_sync_confirmed = confirmed;
            None
        }
        UiCommand::WizardSaveDraft => {
            persist_real_draft_if_ready(state);
            None
        }
        UiCommand::WizardStartFirstSync => {
            recompute_real_wizard_inventory(state);
            if !state.wizard.can_start_first_sync() {
                state.action_error = Some(
                    "Finish the required checks and confirmation before starting the first sync."
                        .to_owned(),
                );
                return None;
            }
            let Some(add_cmd) = build_add_mapping_from_wizard(state) else {
                state.action_error = Some(
                    "Choose both a local folder and a destination folder before starting."
                        .to_owned(),
                );
                return None;
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
                    return None;
                }
            };

            if let Some(mgr) = &state.mgr {
                let already_needs_init =
                    mgr.snapshot().mappings.iter().any(|m| {
                        m.id == mapping_id && m.readiness == Readiness::NeedsInitialization
                    });
                if !already_needs_init && let Err(err) = mgr.add_mapping(add_cmd) {
                    state.action_error = Some(format!("{} {}", err.cause(), err.next_step()));
                    return None;
                }
                match mgr.start_initialize(InitializeCommand {
                    mapping_id: mapping_id.clone(),
                    expected_direction,
                }) {
                    Ok(_) => {
                        state.wizard.editing_mapping_id = Some(mapping_id.clone());
                        state.wizard.first_sync_mapping_id = Some(mapping_id);
                        state.wizard.first_sync_completed = false;
                        state.wizard.first_sync_error = None;
                        state.wizard.first_sync_percent = 0;
                        state.wizard.first_sync_elapsed_secs = 0;
                        state.wizard.step = WizardStep::FirstSync;
                    }
                    Err(err) => {
                        state.action_error = Some(format!("{} {}", err.cause(), err.next_step()));
                    }
                }
            }
            None
        }
        UiCommand::WizardStopFirstSync => {
            if let Some(mid) = state.wizard.first_sync_mapping_id.clone()
                && let Some(mgr) = &state.mgr
            {
                let _ = mgr.stop_run(mid);
            }
            state.wizard.step = WizardStep::Review;
            None
        }
        UiCommand::WizardCompleteDemoFirstSync => None,
        UiCommand::RevealPath(path) => {
            if let Err(err) = state.system_actions.reveal_path(Path::new(&path)) {
                state.action_error = Some(err);
            }
            None
        }
        UiCommand::OpenCloudUrl(url) => {
            if let Err(err) = state.system_actions.open_url(&url) {
                state.action_error = Some(err);
            }
            None
        }
        UiCommand::OpenExtensionSettings => {
            if let Err(err) = state
                .system_actions
                .open_url("x-apple.systempreferences:com.apple.ExtensionsPreferences")
            {
                state.action_error = Some(err);
            }
            None
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
            None
        }
        UiCommand::DemoSetActivityPhase(_)
        | UiCommand::DemoSetPreset(_)
        | UiCommand::DemoStepMappingState(_)
        | UiCommand::DemoTickElapsed
        | UiCommand::DemoAction(_)
        | UiCommand::RequestControlledQuit => None,
    }
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

    let Ok(Some(run)) = mgr.get_run(run_id) else {
        state.action_error = Some(format!("Run `{run_id}` could not be found."));
        return;
    };
    let log_lines = mgr
        .read_run_log(run_id, DEFAULT_RUN_LOG_MAX_LINES)
        .unwrap_or_default();
    let copies: Vec<RecoveryCopyItem> = mgr
        .list_recovery_copies(run_id)
        .unwrap_or_default()
        .into_iter()
        .map(|c| RecoveryCopyItem {
            side_label: match c.side {
                RecoveryCopySide::Local => "On this Mac".to_owned(),
                RecoveryCopySide::Remote => "In the cloud / destination".to_owned(),
            },
            is_remote: c.side == RecoveryCopySide::Remote,
            relative_path: c.relative_path,
            size_label: format_bytes(c.size_bytes),
        })
        .collect();

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
                    || (c.case.reason_kind == "orphan_running"
                        && c.orphan_still_alive != Some(false))
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

            let recovery_copies = latest_run_id
                .as_ref()
                .and_then(|rid| mgr.list_recovery_copies(rid).ok())
                .unwrap_or_default()
                .into_iter()
                .map(|c| RecoveryCopyItem {
                    side_label: match c.side {
                        RecoveryCopySide::Local => "On this Mac".to_owned(),
                        RecoveryCopySide::Remote => "In the cloud / destination".to_owned(),
                    },
                    is_remote: c.side == RecoveryCopySide::Remote,
                    relative_path: c.relative_path,
                    size_label: format_bytes(c.size_bytes),
                })
                .collect();

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
    go_to_review: bool,
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
    state.wizard.editing_mapping_id = Some(rec.id.clone());
    state.wizard.selected_account = rec.account_id.clone();
    state.wizard.selected_folder = rec.collection.display_name().to_owned();
    state.wizard.local_path = rec
        .local_binding
        .resolved_path()
        .to_string_lossy()
        .into_owned();
    state.wizard.remote_folder_path = if rec.remote_endpoint_kind == "local_dir" {
        Some(rec.remote_endpoint_value.clone())
    } else {
        None
    };
    state.wizard.local_recovery_path = rec.local_recovery_root.to_string_lossy().into_owned();
    state.wizard.cloud_recovery_path = rec.cloud_recovery_path.clone();
    state.wizard.no_other_sync_confirmed = false;
    recompute_real_wizard_inventory(state);
    state.wizard.step = if go_to_review && state.wizard.can_visit_step(WizardStep::Review) {
        WizardStep::Review
    } else {
        WizardStep::Location
    };
    true
}

fn persist_real_draft_if_ready(state: &mut WorkerState) {
    let Some(cmd) = build_add_mapping_from_wizard(state) else {
        return;
    };
    if let Some(mgr) = &state.mgr {
        // Only save draft if this mapping is new or still Unconfigured.
        let can_save = mgr
            .snapshot()
            .mappings
            .iter()
            .find(|m| m.id == cmd.id)
            .is_none_or(|m| m.readiness == Readiness::Unconfigured);
        if can_save && mgr.save_draft_mapping(cmd.clone()).is_ok() {
            state.wizard.editing_mapping_id = Some(cmd.id);
        }
    }
}

fn build_add_mapping_from_wizard(state: &WorkerState) -> Option<AddMappingCommand> {
    let folder = state.wizard.selected_folder.trim();
    let local_str = state.wizard.local_path.trim();
    let remote_str = state.wizard.remote_folder_path.as_deref()?.trim();
    if folder.is_empty() || local_str.is_empty() || remote_str.is_empty() {
        return None;
    }
    let mapping_id = state
        .wizard
        .editing_mapping_id
        .clone()
        .unwrap_or_else(|| MappingId::new(sanitize_id(folder)));
    let local_rec = if state.wizard.local_recovery_path.trim().is_empty() {
        PathBuf::from(local_str)
            .parent()?
            .join("FolderSyncRecovery")
            .join(mapping_id.as_str())
    } else {
        PathBuf::from(state.wizard.local_recovery_path.trim())
    };
    let remote_rec = if state.wizard.cloud_recovery_path.trim().is_empty() {
        PathBuf::from(remote_str)
            .parent()?
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
    .ok()?;

    // Translate unchecked subfolders & custom patterns into real rclone filter exclusions!
    let mut extras = BTreeSet::new();
    for node in &state.wizard.subfolders {
        if node.depth == 1 && !node.included {
            extras.insert(format!("/{}/**", node.name));
        }
    }
    for pat in &state.wizard.custom_patterns {
        if !pat.trim().is_empty() {
            extras.insert(pat.trim().to_owned());
        }
    }
    cmd.extra_exclusions = extras;
    Some(cmd)
}

fn refresh_real_catalog(state: &mut WorkerState) {
    state.cloud_catalog.clear();
    let Some(parent) = state
        .local_account_parents
        .get(&state.wizard.selected_account)
        .cloned()
    else {
        return;
    };
    let existing_mappings = state
        .mgr
        .as_ref()
        .map(|m| m.snapshot().mappings.clone())
        .unwrap_or_default();

    let Ok(entries) = fs::read_dir(&parent) else {
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
        let bytes = inv.as_ref().map(|i| i.total_bytes);
        let used_by_other = path.join(ACCESS_MARKER_FILENAME).is_file();
        let on_this_mac = existing_mappings
            .iter()
            .find(|m| m.display_name.eq_ignore_ascii_case(&name))
            .map(|m| m.volume_name.clone());
        items.push(CloudFolderCandidate {
            name,
            size_gb: bytes.map(|b| (b as f64) / 1_000_000_000.0),
            size_label: bytes
                .map(format_bytes)
                .unwrap_or_else(|| "Measuring…".to_owned()),
            on_this_mac,
            used_by_other_computer: used_by_other,
        });
    }
    items.sort_by(|a, b| a.name.cmp(&b.name));
    state.cloud_catalog = items;
    state.cloud_catalog_measured_at = "just now".to_owned();
}

fn recompute_real_wizard_inventory(state: &mut WorkerState) {
    let mut exclusions = sync_core::default_exclusion_patterns();
    for node in &state.wizard.subfolders {
        if node.depth == 1 && !node.included {
            exclusions.insert(format!("/{}/**", node.name));
        }
    }
    for pat in &state.wizard.custom_patterns {
        if !pat.trim().is_empty() {
            exclusions.insert(pat.trim().to_owned());
        }
    }

    let collection = CloudCollection {
        display_path: format!("Data/{}", state.wizard.selected_folder),
        folder_id: format!("local-col-Data_{}", state.wizard.selected_folder),
        ancestor_ids: vec!["local-root".to_owned()],
    };

    // Inspect remote folder if set.
    let remote_inv = state
        .wizard
        .remote_folder_path
        .as_deref()
        .filter(|s| !s.is_empty())
        .and_then(|p| inspect_local_endpoint_detailed(Path::new(p), &collection, &exclusions).ok());

    // Inspect local folder if set.
    let local_path = Path::new(state.wizard.local_path.trim());
    let local_inv = if !state.wizard.local_path.trim().is_empty() && local_path.is_dir() {
        inspect_local_endpoint_detailed(local_path, &collection, &exclusions).ok()
    } else {
        None
    };

    // Populate real subfolder list from whichever populated side has subfolders (preserving unchecked selections).
    let prev_excluded: BTreeSet<String> = state
        .wizard
        .subfolders
        .iter()
        .filter(|n| n.depth == 1 && !n.included)
        .map(|n| n.name.clone())
        .collect();
    if let Some(src_inv) = remote_inv
        .as_ref()
        .filter(|i| !i.subfolders.is_empty())
        .or_else(|| local_inv.as_ref().filter(|i| !i.subfolders.is_empty()))
    {
        let mut nodes = vec![SubfolderNode {
            id: "root".to_owned(),
            name: state.wizard.selected_folder.clone(),
            depth: 0,
            has_children: !src_inv.subfolders.is_empty(),
            size_gb: (src_inv.total_bytes as f64) / 1_000_000_000.0,
            size_label: format_bytes(src_inv.total_bytes),
            included: true,
        }];
        for sub in &src_inv.subfolders {
            nodes.push(SubfolderNode {
                id: sub.relative_path.clone(),
                name: sub.name.clone(),
                depth: 1,
                has_children: sub.has_children,
                size_gb: (sub.total_bytes as f64) / 1_000_000_000.0,
                size_label: format_bytes(sub.total_bytes),
                included: !prev_excluded.contains(&sub.name),
            });
        }
        state.wizard.subfolders = nodes;
    } else {
        state.wizard.subfolders.clear();
    }

    state.wizard.cloud_file_count = remote_inv.as_ref().map(|i| i.file_count);
    state.wizard.cloud_total_bytes = remote_inv.as_ref().map(|i| i.total_bytes);
    state.wizard.local_file_count = local_inv.as_ref().map(|i| i.file_count);
    state.wizard.local_total_bytes = local_inv.as_ref().map(|i| i.total_bytes);
    state.wizard.marker_reused = remote_inv.as_ref().is_some_and(|i| i.has_valid_marker);

    let mut symlinks = Vec::new();
    let mut name_issues = Vec::new();
    for inv in [remote_inv.as_ref(), local_inv.as_ref()]
        .into_iter()
        .flatten()
    {
        symlinks.extend(inv.symlinks.iter().cloned());
        name_issues.extend(inv.name_findings.iter().cloned());
    }
    state.wizard.found_symlinks = symlinks;
    state.wizard.found_name_issues = name_issues;

    // Derive exact ReviewScenario from real inventory (§9)!
    let local_has = local_inv
        .as_ref()
        .is_some_and(|i| i.classification == sync_manager::SideClassification::HasData);
    let remote_has = remote_inv
        .as_ref()
        .is_some_and(|i| i.classification == sync_manager::SideClassification::HasData);
    state.wizard.review_scenario = match (remote_has, local_has) {
        (true, true) => ReviewScenario::BothPopulated,
        (true, false) => ReviewScenario::CloudToLocal,
        (false, true) => ReviewScenario::LocalToCloud,
        (false, false) => ReviewScenario::BothEmpty,
    };

    // Real disk space via statvfs.
    if let Some(space) = disk_space_for_path(local_path) {
        state.wizard.volume_free_gb = space.free_bytes / 1_000_000_000;
        state.wizard.volume_total_gb = (space.total_bytes / 1_000_000_000).max(1);
    }

    // Real location & recovery validation checks.
    let mut loc_checks = Vec::new();
    let trimmed_local = state.wizard.local_path.trim();
    if trimmed_local.is_empty() {
        loc_checks.push(LocationCheck {
            ok: false,
            message: "Choose an existing local folder on this Mac.".to_owned(),
        });
    } else if !local_path.is_dir() {
        loc_checks.push(LocationCheck {
            ok: false,
            message: format!(
                "Folder `{trimmed_local}` does not exist. Choose or create an existing folder."
            ),
        });
    } else if local_path.parent().is_none() || local_path == Path::new("/Volumes") {
        loc_checks.push(LocationCheck {
            ok: false,
            message: "Cannot sync a volume root. Choose a dedicated subfolder.".to_owned(),
        });
    } else {
        loc_checks.push(LocationCheck {
            ok: true,
            message: if local_has {
                "Local folder exists and contains files.".to_owned()
            } else {
                "Local folder exists and is empty.".to_owned()
            },
        });
    }

    // Check overlaps against existing mappings.
    if let Some(mgr) = &state.mgr
        && let Ok(canon_local) = fs::canonicalize(local_path)
    {
        let existing = mgr.snapshot();
        let editing = state.wizard.editing_mapping_id.as_ref();
        let overlapping = existing.mappings.iter().find(|m| {
            if editing == Some(&m.id) {
                return false;
            }
            let other = fs::canonicalize(&m.local_root).unwrap_or_else(|_| m.local_root.clone());
            canon_local == other
                || canon_local.starts_with(&other)
                || other.starts_with(&canon_local)
        });
        if let Some(ov) = overlapping {
            loc_checks.push(LocationCheck {
                ok: false,
                message: format!(
                    "Overlaps existing mapping `{}` ({}). Choose a separate folder.",
                    ov.display_name,
                    ov.local_root.display()
                ),
            });
        } else {
            loc_checks.push(LocationCheck {
                ok: true,
                message: "No overlap with other mappings.".to_owned(),
            });
        }
    }

    // Check free space against included size.
    let need_bytes = state
        .wizard
        .cloud_total_bytes
        .or(state.wizard.local_total_bytes)
        .unwrap_or(0);
    if let Some(space) = disk_space_for_path(local_path) {
        if space.free_bytes >= need_bytes {
            loc_checks.push(LocationCheck {
                ok: true,
                message: format!("{} free on target drive.", format_bytes(space.free_bytes)),
            });
        } else {
            loc_checks.push(LocationCheck {
                ok: false,
                message: format!(
                    "Not enough free space: {} needed, {} free.",
                    format_bytes(need_bytes),
                    format_bytes(space.free_bytes)
                ),
            });
        }
    }
    state.wizard.location_checks = loc_checks;

    // Recovery checks.
    let mut rec_checks = Vec::new();
    let rec_local = state.wizard.local_recovery_path.trim();
    if rec_local.is_empty() {
        rec_checks.push(LocationCheck {
            ok: false,
            message: "Choose a local recovery folder outside every synced folder.".to_owned(),
        });
    } else if !trimmed_local.is_empty()
        && (Path::new(rec_local).starts_with(local_path)
            || local_path.starts_with(Path::new(rec_local)))
    {
        rec_checks.push(LocationCheck {
            ok: false,
            message: "Local recovery folder cannot be inside the synced folder.".to_owned(),
        });
    } else {
        rec_checks.push(LocationCheck {
            ok: true,
            message: "Recovery folder is outside every synced folder.".to_owned(),
        });
    }
    state.wizard.recovery_checks = rec_checks;
}

fn emit_snapshot(
    state: &mut WorkerState,
    explicit_snap: Option<&StatusSnapshot>,
    ui_tx: &mpsc::Sender<UiUpdate>,
    latest_cached: &Arc<Mutex<UiSnapshot>>,
) {
    let ui_snap = project_ui_snapshot(state, explicit_snap);
    *latest_cached.lock().unwrap() = ui_snap.clone();
    let _ = ui_tx.send(UiUpdate::Snapshot(Box::new(ui_snap)));
}

fn project_ui_snapshot(state: &WorkerState, explicit_snap: Option<&StatusSnapshot>) -> UiSnapshot {
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

    let mut accounts: Vec<AccountSnapshot> = accounts_raw
        .iter()
        .map(|a| project_account(a, &mgr_snap.mappings))
        .collect();
    accounts.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));

    let mut volumes_map: BTreeMap<String, VolumeSnapshot> = BTreeMap::new();
    let mut mappings = Vec::with_capacity(mgr_snap.mappings.len());
    let mut active_run = None;

    for m in &mgr_snap.mappings {
        let vol_id = VolumeIdentity::new(m.volume_kind, m.volume_stable_id.clone());
        volumes_map
            .entry(m.volume_stable_id.clone())
            .or_insert_with(|| {
                build_volume_snapshot(
                    &vol_id,
                    &m.volume_name,
                    &m.local_root,
                    m.readiness != Readiness::VolumeUnavailable,
                )
            });

        let recent_runs: Vec<RunRecordSnapshot> = mgr
            .list_recent_runs(&m.id, DEFAULT_RECENT_RUNS_FOR_UI)
            .unwrap_or_default()
            .into_iter()
            .map(project_run_record)
            .collect();

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
        // Preserve exact state from snapshot.
        mapping_state.execution = m.execution;
        mapping_state.intent = m.intent;

        let last_run = match (m.last_run_outcome, m.last_run_completed_at.as_deref()) {
            (Some(outcome), Some(ts)) => {
                let wall = DateTime::parse_from_rfc3339(ts)
                    .ok()
                    .and_then(|dt| {
                        let utc = dt.with_timezone(&chrono::Utc);
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
            let active_name = mgr_snap
                .active_job_mapping_id
                .as_ref()
                .and_then(|aid| mgr_snap.mappings.iter().find(|x| &x.id == aid))
                .map(|x| x.display_name.clone())
                .unwrap_or_else(|| "active run".to_owned());
            StatusModifier::QueuedBehind { active_name }
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

        if matches!(m.execution, Execution::Running | Execution::Stopping) {
            let phase = if m.execution == Execution::Stopping {
                RunPhase::Stopping { elapsed_secs: 0 }
            } else if m.counters.bytes_transferred > 0 {
                RunPhase::TransferringUnknown {
                    transferred_label: format_bytes(m.counters.bytes_transferred),
                    elapsed_secs: 0,
                }
            } else {
                RunPhase::Scanning { elapsed_secs: 0 }
            };
            active_run = Some(ActiveRunSnapshot {
                run_id: m
                    .last_run_id
                    .clone()
                    .unwrap_or_else(|| RunId::new("run-active")),
                mapping_id: m.id.clone(),
                mapping_name: m.display_name.clone(),
                volume_name: m.volume_name.clone(),
                started_label: "just now".to_owned(),
                trigger_label: "Sync now".to_owned(),
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
            schedule_minutes: (m.schedule_interval_secs / 60).max(1) as u32,
            next_check_label: if mgr_snap.scheduling_paused {
                "Scheduling paused".to_owned()
            } else if !m.scheduling_enabled {
                "Manual runs only".to_owned()
            } else {
                format!("Every {} minutes", (m.schedule_interval_secs / 60).max(1))
            },
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
        attention_items,
        recent_events: state.recent_events.clone(),
        raw_log: state.raw_log.clone(),
        inspected_run: state.inspected_run.clone(),
        recovery_checklist: state.recovery_checklist.clone(),
        cloud_catalog: state.cloud_catalog.clone(),
        cloud_catalog_measured_at: state.cloud_catalog_measured_at.clone(),
        wizard: state.wizard.clone(),
        preferences: state.preferences.clone(),
        reconnecting_account: None,
        status_notice: state.status_notice.clone(),
        action_error: state.action_error.clone(),
    }
}

fn project_account(acct: &AccountRecord, mappings: &[MappingStatusSnapshot]) -> AccountSnapshot {
    let mapping_count = mappings.iter().filter(|m| m.account_id == acct.id).count();
    let status = match acct.auth_status.as_str() {
        "ready" => AccountStatus::Connected,
        "keychain_locked" => AccountStatus::KeychainLocked,
        _ => AccountStatus::SignInRequired,
    };
    let access_summary = if acct.kind == "local_fixture" {
        "Local test folder account (no cloud OAuth).".to_owned()
    } else {
        "Configured in FolderSync.".to_owned()
    };
    AccountSnapshot {
        id: acct.id.clone(),
        email: acct.display_name.clone(),
        shared_parent_label: acct.remote_root_folder_id.clone(),
        status,
        access_summary,
        mapping_count,
    }
}

fn build_volume_snapshot(
    vol_id: &VolumeIdentity,
    display_name: &str,
    sample_path: &Path,
    connected: bool,
) -> VolumeSnapshot {
    let (total_gb, free_gb, free_label, total_label, used_percent, low_space) =
        if connected && let Some(space) = disk_space_for_path(sample_path) {
            let t_gb = (space.total_bytes / 1_000_000_000).max(1);
            let f_gb = space.free_bytes / 1_000_000_000;
            let used = t_gb.saturating_sub(f_gb);
            let pct = ((used.saturating_mul(100)) / t_gb).min(100) as u8;
            let low = f_gb < 10 || pct >= 95;
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
        display_name: display_name.to_owned(),
        filesystem: "APFS".to_owned(),
        connected,
        total_gb,
        free_gb,
        free_label,
        total_label,
        used_percent,
        low_space,
        last_seen_label: None,
        recovery_root_label: None,
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
                    cause: "An earlier run was interrupted or hit a safety stop.".to_owned(),
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
            Readiness::VolumeUnavailable => {
                items.push(AttentionItem {
                    id: format!("attn-vol-{}", m.id),
                    priority: AttentionPriority::VolumeMissing,
                    mapping_id: Some(m.id.clone()),
                    mapping_name: Some(m.display_name.clone()),
                    pill_label: "Waiting".to_owned(),
                    pill_tone: Tone::Attn,
                    icon: IconKind::DriveMissing,
                    is_error_accent: false,
                    title: format!("Waiting for {}", m.volume_name),
                    cause: format!("{} isn't connected right now.", m.volume_name),
                    what_did: "Nothing is deleted in the cloud while the drive is away.".to_owned(),
                    next_step: format!("Connect {}.", m.volume_name),
                    kind: AttentionKind::VolumeMissing {
                        volume_name: m.volume_name.clone(),
                    },
                });
            }
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
    for v in volumes {
        if v.connected && v.low_space {
            items.push(AttentionItem {
                id: format!("attn-space-{}", v.identity.stable_id),
                priority: AttentionPriority::LowSpaceAdvisory,
                mapping_id: None,
                mapping_name: None,
                pill_label: "Low space".to_owned(),
                pill_tone: Tone::Attn,
                icon: IconKind::Alert,
                is_error_accent: false,
                title: format!("{} · {} free", v.display_name, v.free_label),
                cause: format!("{} is running low on free space.", v.display_name),
                what_did: "No recovery copies were removed automatically.".to_owned(),
                next_step: "Free space on the drive so syncs and recovery copies have room."
                    .to_owned(),
                kind: AttentionKind::LowSpaceAdvisory {
                    volume_name: v.display_name.clone(),
                    recovery_path: v.recovery_root_label.clone().unwrap_or_default(),
                },
            });
        }
    }
    items.sort_by_key(|i| i.priority);
    items
}

fn build_redacted_diagnostics_text(state: &WorkerState) -> String {
    let mut out = String::new();
    out.push_str("FolderSync Diagnostics (no file contents and no sign-in data)\n");
    out.push_str(&format!("App version: {}\n", state.preferences.app_version));
    out.push_str(&format!("Engine: {}\n", state.preferences.engine_version));
    out.push_str(&format!("NSH: {}\n\n", state.preferences.nsh_version));
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

fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1_000.0;
    const MB: f64 = 1_000_000.0;
    const GB: f64 = 1_000_000_000.0;
    const TB: f64 = 1_000_000_000_000.0;
    let b = bytes as f64;
    if b >= TB {
        format!("{:.2} TB", b / TB)
    } else if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

fn format_short_timestamp(rfc3339: &str) -> String {
    DateTime::parse_from_rfc3339(rfc3339)
        .map(|dt| dt.format("%d %b %H:%M").to_string())
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
        step: WizardStep::Folder,
        editing_mapping_id: None,
        selected_account: AccountId::new("local-1"),
        selected_folder: String::new(),
        remote_folder_path: None,
        local_path: String::new(),
        volume_name: "Internal SSD".to_owned(),
        volume_kind: VolumeKind::Internal,
        volume_fs: "APFS".to_owned(),
        volume_free_gb: 0,
        volume_total_gb: 1,
        location_checks: vec![LocationCheck {
            ok: false,
            message: "Choose an existing local folder on this Mac.".to_owned(),
        }],
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
        first_sync_mapping_id: None,
        first_sync_percent: 0,
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
        attention_items: Vec::new(),
        recent_events: Vec::new(),
        raw_log: Vec::new(),
        inspected_run: None,
        recovery_checklist: None,
        cloud_catalog: Vec::new(),
        cloud_catalog_measured_at: "not measured yet".to_owned(),
        wizard: default_real_wizard(),
        preferences: default_real_preferences(),
        reconnecting_account: None,
        status_notice: None,
        action_error: None,
    }
}
