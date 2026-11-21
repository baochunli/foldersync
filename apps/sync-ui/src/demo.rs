//! In-memory demo source driving the FolderSync UI without touching the filesystem,
//! rclone, Keychain, or real sync-manager.
//!
//! All sample data is neutral and matches `ui-spec.md` §2 and §6 verbatim.

use std::cell::RefCell;

use sync_core::{
    AccountId, Execution, Intent, LastRunSummary, MappingId, MappingState, ObservedEvent,
    PostRecoveryReadiness, Readiness, RunId, RunOutcome, UserAction, VolumeIdentity, VolumeKind,
    WallTime, apply_event, apply_user_action, available_actions,
};

use crate::contract::{
    AccountSnapshot, AccountStatus, ActiveRunSnapshot, ActivityEvent, AttentionItem, AttentionKind,
    AttentionPriority, CloudFolderCandidate, DemoActivityPhase, DemoPreset, FinderExtensionStatus,
    IconKind, LocationCheck, MappingSnapshot, PreferencesSnapshot, RecoveryCauseKind,
    ReviewScenario, RunPhase, RunRecordSnapshot, ScreenRoute, StatusModifier, SubfolderNode, Tone,
    UiCommand, UiSnapshot, VolumeSnapshot, WizardDraft, WizardStep,
};

/// In-memory manager stand-in for Group 3 UI verification.
pub struct DemoSource {
    snapshot: RefCell<UiSnapshot>,
    next_event_id: RefCell<u64>,
}

impl Default for DemoSource {
    fn default() -> Self {
        Self::new(DemoPreset::AllStates)
    }
}

impl DemoSource {
    /// Builds a fresh in-memory demo source for the requested preset.
    pub fn new(preset: DemoPreset) -> Self {
        let snap = build_snapshot(preset);
        Self {
            snapshot: RefCell::new(snap),
            next_event_id: RefCell::new(100),
        }
    }

    /// Returns a cloned immutable snapshot for rendering.
    pub fn snapshot(&self) -> UiSnapshot {
        self.snapshot.borrow().clone()
    }

    /// Applies a `UiCommand` strictly in memory and returns the updated snapshot.
    pub fn dispatch(&self, cmd: UiCommand) -> UiSnapshot {
        let mut snap = self.snapshot.borrow_mut();
        snap.status_notice = None;
        match cmd {
            UiCommand::DemoSetPreset(preset) => {
                *snap = build_snapshot(preset);
            }
            UiCommand::SyncNow(id) => {
                let mut event_msg = None;
                let mut new_active_run = None;
                let active_name = snap.active_run.as_ref().map(|r| r.mapping_name.clone());
                if let Some(m) = snap.mappings.iter_mut().find(|m| m.id == id) {
                    if m.state.execution == Execution::Queued {
                        event_msg = Some(format!(
                            "{}: moved to front of queue (Sync now)",
                            m.display_name
                        ));
                    } else {
                        let res = apply_user_action(m.state, UserAction::RunNow);
                        m.state = res.state;
                        if m.state.execution == Execution::Queued {
                            if active_name.is_none() && !m.is_variant_showcase {
                                let started = apply_event(m.state, ObservedEvent::RunStarted);
                                m.state = started.state;
                                new_active_run = Some(ActiveRunSnapshot {
                                    run_id: RunId::new("run-demo-live"),
                                    mapping_id: m.id.clone(),
                                    mapping_name: m.display_name.clone(),
                                    volume_name: m.volume_name.clone(),
                                    started_label: "14:46".to_owned(),
                                    trigger_label: "Sync now".to_owned(),
                                    phase: RunPhase::Scanning { elapsed_secs: 1 },
                                });
                                event_msg =
                                    Some(format!("{}: run started (Sync now)", m.display_name));
                            } else {
                                m.modifier = StatusModifier::QueuedBehind {
                                    active_name: active_name
                                        .clone()
                                        .unwrap_or_else(|| "Research".to_owned()),
                                };
                                event_msg = Some(format!("{}: queued (Sync now)", m.display_name));
                            }
                        }
                        m.available_actions = available_actions(&m.state);
                    }
                }
                if let Some(run) = new_active_run {
                    snap.active_run = Some(run);
                }
                if let Some(msg) = event_msg {
                    self.push_event(&mut snap, "14:46", msg, None);
                }
            }
            UiCommand::SyncReadyMappings => {
                let mut queued_names = Vec::new();
                let active_name = snap
                    .active_run
                    .as_ref()
                    .map(|r| r.mapping_name.clone())
                    .unwrap_or_else(|| "Research".to_owned());
                for m in &mut snap.mappings {
                    if !m.is_variant_showcase
                        && m.state.readiness() == Readiness::Ready
                        && m.state.execution == Execution::Idle
                        && m.state.intent == Intent::Enabled
                    {
                        let res = apply_user_action(m.state, UserAction::RunNow);
                        m.state = res.state;
                        m.modifier = StatusModifier::QueuedBehind {
                            active_name: active_name.clone(),
                        };
                        m.available_actions = available_actions(&m.state);
                        queued_names.push(m.display_name.clone());
                    }
                }
                if !queued_names.is_empty() {
                    self.push_event(
                        &mut snap,
                        "14:46",
                        format!("Queued ready mappings: {}", queued_names.join(", ")),
                        None,
                    );
                }
            }
            UiCommand::PauseMapping(id) => {
                let mut event_msg = None;
                if let Some(m) = snap.mappings.iter_mut().find(|m| m.id == id) {
                    let res = apply_user_action(m.state, UserAction::Pause);
                    m.state = res.state;
                    if m.state.readiness() == Readiness::Ready
                        && m.state.execution == Execution::Idle
                    {
                        m.modifier = StatusModifier::PausedOn {
                            date_label: "today".to_owned(),
                        };
                    }
                    m.available_actions = available_actions(&m.state);
                    event_msg = Some(format!("{}: paused by you", m.display_name));
                }
                if let Some(msg) = event_msg {
                    self.push_event(&mut snap, "14:46", msg, None);
                }
            }
            UiCommand::ResumeMapping(id) => {
                let mut event_msg = None;
                let active_name = snap
                    .active_run
                    .as_ref()
                    .map(|r| r.mapping_name.clone())
                    .unwrap_or_else(|| "Research".to_owned());
                if let Some(m) = snap.mappings.iter_mut().find(|m| m.id == id) {
                    let res = apply_user_action(m.state, UserAction::Resume);
                    m.state = res.state;
                    if matches!(m.modifier, StatusModifier::PausedOn { .. }) {
                        if m.state.execution == Execution::Queued {
                            m.modifier = StatusModifier::QueuedBehind {
                                active_name: active_name.clone(),
                            };
                        } else {
                            m.modifier = StatusModifier::None;
                        }
                    }
                    m.available_actions = available_actions(&m.state);
                    event_msg = Some(format!("{}: resumed", m.display_name));
                }
                if let Some(msg) = event_msg {
                    self.push_event(&mut snap, "14:46", msg, None);
                }
            }
            UiCommand::StopRun(id) => {
                let mut event_msg = None;
                if let Some(m) = snap.mappings.iter_mut().find(|m| m.id == id) {
                    let res = apply_user_action(m.state, UserAction::StopRun);
                    m.state = res.state;
                    m.available_actions = available_actions(&m.state);
                    event_msg = Some(format!("{}: graceful stop requested", m.display_name));
                }
                if let Some(run) = &mut snap.active_run
                    && run.mapping_id == id
                {
                    run.phase = RunPhase::Stopping { elapsed_secs: 1 };
                }
                if let Some(msg) = event_msg {
                    self.push_event(&mut snap, "14:46", msg, None);
                }
            }
            UiCommand::ForceStopRun(id) => {
                let mut event_msg = None;
                if let Some(m) = snap.mappings.iter_mut().find(|m| m.id == id) {
                    let res = apply_user_action(m.state, UserAction::ForceStop);
                    m.state = res.state;
                    m.modifier = StatusModifier::RecoveryCause {
                        kind: RecoveryCauseKind::Interrupted,
                        detail: "Last run was force stopped at 14:46".to_owned(),
                        recovery_copies_count: 0,
                        ambiguous_ownership: false,
                    };
                    m.available_actions = available_actions(&m.state);
                    event_msg = Some(format!(
                        "{}: force stopped — recovery required before syncing again",
                        m.display_name
                    ));
                }
                if snap.active_run.as_ref().is_some_and(|r| r.mapping_id == id) {
                    snap.active_run = None;
                }
                if let Some(msg) = event_msg {
                    self.push_event(
                        &mut snap,
                        "14:46",
                        msg,
                        Some(("Start recovery…".to_owned(), ScreenRoute::Recovery(id))),
                    );
                }
                rebuild_attention(&mut snap);
            }
            UiCommand::SetSchedulingPaused(paused) => {
                snap.scheduling_paused = paused;
                let msg = if paused {
                    "Scheduling paused. Sync now still works.".to_owned()
                } else {
                    "Scheduling resumed.".to_owned()
                };
                self.push_event(&mut snap, "14:46", msg, None);
            }
            UiCommand::MarkConflictsReviewed(id) => {
                let mut event_msg = None;
                if let Some(m) = snap.mappings.iter_mut().find(|m| m.id == id)
                    && matches!(m.modifier, StatusModifier::UnreviewedConflicts { .. })
                {
                    m.modifier = StatusModifier::None;
                    if let Some(lr) = &mut m.last_run {
                        lr.outcome = RunOutcome::Success;
                    }
                    event_msg = Some(format!("{}: conflicts marked as reviewed", m.display_name));
                }
                if let Some(msg) = event_msg {
                    self.push_event(&mut snap, "14:46", msg, None);
                }
                rebuild_attention(&mut snap);
            }
            UiCommand::CheckAgain(id) => {
                let mut event_msg = None;
                if let Some(m) = snap.mappings.iter_mut().find(|m| m.id == id) {
                    match m.state.readiness() {
                        Readiness::EndpointChanged => {
                            let res = apply_event(m.state, ObservedEvent::EndpointValidated);
                            m.state = res.state;
                            m.modifier = StatusModifier::None;
                            m.available_actions = available_actions(&m.state);
                            event_msg = Some(format!(
                                "{}: cloud folder identity verified",
                                m.display_name
                            ));
                        }
                        Readiness::AuthenticationRequired => {
                            let res = apply_event(m.state, ObservedEvent::AuthenticationRestored);
                            m.state = res.state;
                            m.modifier = StatusModifier::None;
                            m.available_actions = available_actions(&m.state);
                            event_msg =
                                Some(format!("{}: account access verified", m.display_name));
                        }
                        _ => {}
                    }
                }
                if let Some(msg) = event_msg {
                    self.push_event(&mut snap, "14:46", msg, None);
                }
                rebuild_attention(&mut snap);
            }
            UiCommand::CompleteRecovery {
                mapping_id,
                resume_and_run,
            } => {
                let mut event_msg = None;
                let active_name = snap
                    .active_run
                    .as_ref()
                    .map(|r| r.mapping_name.clone())
                    .unwrap_or_else(|| "Research".to_owned());
                if let Some(m) = snap.mappings.iter_mut().find(|m| m.id == mapping_id) {
                    let res = apply_event(
                        m.state,
                        ObservedEvent::RecoveryCompleted {
                            next_readiness: PostRecoveryReadiness::Ready,
                        },
                    );
                    m.state = res.state;
                    if resume_and_run {
                        let resumed = apply_user_action(m.state, UserAction::Resume);
                        m.state = resumed.state;
                        m.modifier = StatusModifier::QueuedBehind {
                            active_name: active_name.clone(),
                        };
                        event_msg = Some(format!(
                            "{}: recovery complete — queued for a normal run",
                            m.display_name
                        ));
                    } else {
                        let paused = apply_user_action(m.state, UserAction::Pause);
                        m.state = paused.state;
                        m.modifier = StatusModifier::PausedOn {
                            date_label: "today".to_owned(),
                        };
                        event_msg = Some(format!(
                            "{}: recovery complete — kept paused",
                            m.display_name
                        ));
                    }
                    m.available_actions = available_actions(&m.state);
                }
                if let Some(msg) = event_msg {
                    self.push_event(&mut snap, "14:46", msg, None);
                }
                rebuild_attention(&mut snap);
            }
            UiCommand::RemoveMapping(id) | UiCommand::DiscardDraft(id) => {
                if let Some(pos) = snap.mappings.iter().position(|m| m.id == id) {
                    let removed = snap.mappings.remove(pos);
                    if snap
                        .active_run
                        .as_ref()
                        .is_some_and(|r| r.mapping_id == removed.id)
                    {
                        snap.active_run = None;
                    }
                    recount_account_mappings(&mut snap);
                    self.push_event(
                        &mut snap,
                        "14:46",
                        format!(
                            "{}: removed from FolderSync (local and cloud folders kept)",
                            removed.display_name
                        ),
                        None,
                    );
                    rebuild_attention(&mut snap);
                }
            }
            UiCommand::SetScheduleInterval {
                mapping_id,
                minutes,
            } => {
                if let Some(m) = snap.mappings.iter_mut().find(|m| m.id == mapping_id) {
                    m.schedule_minutes = minutes;
                }
            }
            UiCommand::ConnectDemoAccount => {
                if snap.accounts.is_empty() {
                    *snap = build_snapshot(DemoPreset::WelcomeNoMappings);
                } else {
                    snap.reconnecting_account = Some(AccountId::new("new-account"));
                }
            }
            UiCommand::ReconnectAccount(account_id) => {
                snap.reconnecting_account = Some(account_id.clone());
                if let Some(acct) = snap.accounts.iter_mut().find(|a| a.id == account_id) {
                    acct.status = AccountStatus::Reconnecting;
                }
            }
            UiCommand::CancelReconnect => {
                if let Some(account_id) = snap.reconnecting_account.take()
                    && let Some(acct) = snap.accounts.iter_mut().find(|a| a.id == account_id)
                {
                    acct.status = AccountStatus::SignInRequired;
                }
            }
            UiCommand::CheckAccountAccess(account_id) => {
                if let Some(acct) = snap.accounts.iter_mut().find(|a| a.id == account_id) {
                    acct.status = AccountStatus::Connected;
                    acct.access_summary =
                        "Checked today at 14:46. List, upload, overwrite, move to Recovery and delete were tested on test files."
                            .to_owned();
                }
                if snap.reconnecting_account.as_ref() == Some(&account_id) {
                    snap.reconnecting_account = None;
                }
                for m in &mut snap.mappings {
                    if m.account_id == account_id
                        && m.state.readiness() == Readiness::AuthenticationRequired
                    {
                        let res = apply_event(m.state, ObservedEvent::AuthenticationRestored);
                        m.state = res.state;
                        m.modifier = StatusModifier::None;
                        m.available_actions = available_actions(&m.state);
                    }
                }
                self.push_event(
                    &mut snap,
                    "14:46",
                    format!("{account_id}: access check passed"),
                    None,
                );
                rebuild_attention(&mut snap);
            }
            UiCommand::RemoveAccount(account_id) => {
                let count = snap
                    .mappings
                    .iter()
                    .filter(|m| !m.is_variant_showcase && m.account_id == account_id)
                    .count();
                if count == 0 {
                    snap.accounts.retain(|a| a.id != account_id);
                }
            }
            UiCommand::SetOpenAtLogin(enabled) => {
                snap.preferences.open_at_login = enabled;
                snap.preferences.open_at_login_pending = false;
                let state_str = if enabled { "enabled" } else { "disabled" };
                self.push_event(
                    &mut snap,
                    "14:46",
                    format!(
                        "Open at login {state_str} (demo state only; macOS Login Items unchanged)"
                    ),
                    None,
                );
            }
            UiCommand::SetNotifications(enabled) => {
                snap.preferences.notifications_enabled = enabled;
            }
            UiCommand::SetBandwidthLimit(limit) => {
                snap.preferences.bandwidth_limit_mb_s = limit;
            }
            UiCommand::WizardSetStep(step) => {
                recompute_wizard_checks(&mut snap);
                if snap.wizard.can_visit_step(step) {
                    snap.wizard.step = step;
                }
            }
            UiCommand::WizardSelectAccount(acct) => {
                snap.wizard.selected_account = acct;
                snap.wizard.no_other_sync_confirmed = false;
                snap.wizard.cloud_recovery_path = format!(
                    "{}:Recovery/{}/{}",
                    snap.wizard.selected_account,
                    snap.wizard.selected_folder.to_ascii_lowercase(),
                    snap.device_id
                );
                recompute_wizard_checks(&mut snap);
            }
            UiCommand::WizardSelectFolder(folder) => {
                snap.wizard.selected_folder = folder.clone();
                snap.wizard.local_path = format!("/Volumes/WorkSSD/{folder}");
                snap.wizard.local_recovery_path =
                    format!("/Volumes/WorkSSD/FolderSyncRecovery/{folder}");
                snap.wizard.cloud_recovery_path = format!(
                    "{}:Recovery/{}/{}",
                    snap.wizard.selected_account,
                    folder.to_ascii_lowercase(),
                    snap.device_id
                );
                snap.wizard.no_other_sync_confirmed = false;
                sync_wizard_volume_from_path(&mut snap);
                recompute_wizard_checks(&mut snap);
            }
            UiCommand::WizardSetLocalPath(path) => {
                snap.wizard.local_path = path;
                snap.wizard.no_other_sync_confirmed = false;
                sync_wizard_volume_from_path(&mut snap);
                recompute_wizard_checks(&mut snap);
            }
            UiCommand::WizardToggleSubfolder(sub_id) => {
                if sub_id == "root" {
                    let next = !snap
                        .wizard
                        .subfolders
                        .first()
                        .map(|n| n.included)
                        .unwrap_or(true);
                    for node in &mut snap.wizard.subfolders {
                        node.included = next;
                    }
                } else if let Some(node) =
                    snap.wizard.subfolders.iter_mut().find(|n| n.id == sub_id)
                {
                    node.included = !node.included;
                }
                snap.wizard.no_other_sync_confirmed = false;
                recompute_wizard_checks(&mut snap);
            }
            UiCommand::WizardToggleDefaultExclusions => {
                snap.wizard.show_default_exclusions = !snap.wizard.show_default_exclusions;
            }
            UiCommand::WizardAddPattern(pattern) => {
                let trimmed = pattern.trim().to_owned();
                if !trimmed.is_empty() && !snap.wizard.custom_patterns.contains(&trimmed) {
                    snap.wizard.custom_patterns.push(trimmed);
                    snap.wizard.no_other_sync_confirmed = false;
                }
            }
            UiCommand::WizardSetLocalRecoveryPath(path) => {
                snap.wizard.local_recovery_path = path;
                snap.wizard.no_other_sync_confirmed = false;
                recompute_wizard_checks(&mut snap);
            }
            UiCommand::WizardSetReviewScenario(scenario) => {
                snap.wizard.review_scenario = scenario;
                snap.wizard.no_other_sync_confirmed = false;
                recompute_wizard_checks(&mut snap);
            }
            UiCommand::WizardSetNoOtherSyncConfirmed(confirmed) => {
                snap.wizard.no_other_sync_confirmed = confirmed;
            }
            UiCommand::WizardStartFirstSync => {
                recompute_wizard_checks(&mut snap);
                if snap.wizard.can_start_first_sync() {
                    snap.wizard.step = WizardStep::FirstSync;
                    snap.wizard.first_sync_percent = 33;
                    snap.wizard.first_sync_elapsed_secs = 21 * 60 + 40;
                    let name = snap.wizard.selected_folder.clone();
                    self.push_event(
                        &mut snap,
                        "14:46",
                        format!("{name}: first sync started (demo)"),
                        None,
                    );
                }
            }
            UiCommand::WizardReset => {
                let skip_acct = snap
                    .accounts
                    .iter()
                    .filter(|a| a.status == AccountStatus::Connected)
                    .count()
                    <= 1;
                snap.wizard = default_wizard_draft(skip_acct);
                recompute_wizard_checks(&mut snap);
            }
            UiCommand::DemoSetActivityPhase(phase) => {
                // Always keep the previewed active run consistent with its real mapping_id (`research`).
                if let Some(run) = &mut snap.active_run {
                    let target_id = run.mapping_id.clone();
                    match phase {
                        DemoActivityPhase::Scanning => {
                            run.phase = RunPhase::Scanning {
                                elapsed_secs: 3 * 60 + 12,
                            };
                        }
                        DemoActivityPhase::TransferringKnown => {
                            run.phase = RunPhase::TransferringKnown {
                                transferred_label: "1.2 GB".to_owned(),
                                total_label: "3.4 GB".to_owned(),
                                percent: 35,
                                elapsed_secs: 2 * 60 + 40,
                            };
                        }
                        DemoActivityPhase::TransferringUnknown => {
                            run.phase = RunPhase::TransferringUnknown {
                                transferred_label: "820 MB".to_owned(),
                                elapsed_secs: 6 * 60 + 5,
                            };
                        }
                        DemoActivityPhase::Stopping => {
                            run.phase = RunPhase::Stopping { elapsed_secs: 24 };
                        }
                    }
                    if let Some(m) = snap.mappings.iter_mut().find(|m| m.id == target_id) {
                        m.state.execution = if matches!(phase, DemoActivityPhase::Stopping) {
                            Execution::Stopping
                        } else {
                            Execution::Running
                        };
                        m.available_actions = available_actions(&m.state);
                    }
                }
            }
            UiCommand::DemoStepMappingState(id) => {
                let mut stepped_info = None;
                if let Some(m) = snap.mappings.iter_mut().find(|m| m.id == id) {
                    step_mapping_demo_state(m);
                    stepped_info = Some((
                        m.id.clone(),
                        m.display_name.clone(),
                        m.volume_name.clone(),
                        m.state.execution,
                    ));
                }
                if let Some((mid, name, vol_name, exec)) = stepped_info {
                    match exec {
                        Execution::Running => {
                            snap.active_run = Some(ActiveRunSnapshot {
                                run_id: RunId::new(format!("run-demo-{mid}")),
                                mapping_id: mid,
                                mapping_name: name,
                                volume_name: vol_name,
                                started_label: "14:46".to_owned(),
                                trigger_label: "demo preview".to_owned(),
                                phase: RunPhase::Scanning { elapsed_secs: 64 },
                            });
                        }
                        Execution::Stopping => {
                            snap.active_run = Some(ActiveRunSnapshot {
                                run_id: RunId::new(format!("run-demo-{mid}")),
                                mapping_id: mid,
                                mapping_name: name,
                                volume_name: vol_name,
                                started_label: "14:46".to_owned(),
                                trigger_label: "demo preview".to_owned(),
                                phase: RunPhase::Stopping { elapsed_secs: 24 },
                            });
                        }
                        _ => {
                            if snap
                                .active_run
                                .as_ref()
                                .is_some_and(|r| r.mapping_id == mid)
                            {
                                snap.active_run = None;
                            }
                        }
                    }
                }
                rebuild_attention(&mut snap);
            }
            UiCommand::DemoTickElapsed => {
                if let Some(run) = &mut snap.active_run {
                    match &mut run.phase {
                        RunPhase::Scanning { elapsed_secs }
                        | RunPhase::TransferringKnown { elapsed_secs, .. }
                        | RunPhase::TransferringUnknown { elapsed_secs, .. }
                        | RunPhase::Stopping { elapsed_secs } => {
                            *elapsed_secs = elapsed_secs.saturating_add(1);
                        }
                    }
                }
            }
            UiCommand::DemoAction(msg) => {
                snap.status_notice = Some(msg.clone());
                self.push_event(&mut snap, "14:46", msg, None);
            }
            UiCommand::RequestControlledQuit => {
                snap.scheduling_paused = true;
                if let Some(run) = &mut snap.active_run {
                    run.phase = RunPhase::Stopping { elapsed_secs: 1 };
                }
                snap.status_notice = Some("Controlled shutdown requested (demo mode).".to_owned());
            }
        }
        snap.clone()
    }

    fn push_event(
        &self,
        snap: &mut UiSnapshot,
        time: &str,
        msg: String,
        link: Option<(String, ScreenRoute)>,
    ) {
        let mut next = self.next_event_id.borrow_mut();
        let id = *next;
        *next += 1;
        snap.recent_events.insert(
            0,
            ActivityEvent {
                id,
                time_label: time.to_owned(),
                message: msg,
                link,
            },
        );
        snap.recent_events.truncate(200);
    }
}

fn normalize_path(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed == "/" {
        "/".to_owned()
    } else {
        trimmed.trim_end_matches('/').to_owned()
    }
}

fn paths_overlap(a: &str, b: &str) -> bool {
    let na = normalize_path(a);
    let nb = normalize_path(b);
    if na.is_empty() || nb.is_empty() {
        return false;
    }
    na == nb
        || na.starts_with(&format!("{nb}/"))
        || nb.starts_with(&format!("{na}/"))
        || na == "/"
        || nb == "/"
}

fn is_volume_root(path: &str) -> bool {
    let norm = normalize_path(path);
    if norm == "/" || norm == "/Volumes" {
        return true;
    }
    if let Some(rest) = norm.strip_prefix("/Volumes/") {
        return !rest.is_empty() && !rest.contains('/');
    }
    false
}

fn resolve_volume_for_path<'a>(
    path: &str,
    volumes: &'a [VolumeSnapshot],
) -> Option<&'a VolumeSnapshot> {
    let norm = normalize_path(path);
    if let Some(rest) = norm.strip_prefix("/Volumes/") {
        let vol_name = rest.split('/').next().unwrap_or("");
        return volumes.iter().find(|v| v.display_name == vol_name);
    }
    volumes
        .iter()
        .find(|v| v.identity.kind == VolumeKind::Internal)
}

fn sync_wizard_volume_from_path(snap: &mut UiSnapshot) {
    if let Some(vol) = resolve_volume_for_path(&snap.wizard.local_path, &snap.volumes).cloned() {
        snap.wizard.volume_name = vol.display_name;
        snap.wizard.volume_kind = vol.identity.kind;
        snap.wizard.volume_fs = vol.filesystem;
        snap.wizard.volume_free_gb = vol.free_gb;
        snap.wizard.volume_total_gb = vol.total_gb;
    }
}

fn recompute_wizard_checks(snap: &mut UiSnapshot) {
    let included_gb = snap.wizard.included_gb();
    snap.wizard.location_checks = evaluate_location_checks(
        &snap.wizard.local_path,
        &snap.mappings,
        &snap.volumes,
        included_gb,
        snap.wizard.review_scenario,
    );
    snap.wizard.recovery_checks = evaluate_recovery_checks(
        &snap.wizard.local_recovery_path,
        &snap.wizard.local_path,
        &snap.mappings,
        &snap.volumes,
    );
}

/// Evaluates Location step checks against the chosen path and demo fixtures.
pub fn evaluate_location_checks(
    path: &str,
    mappings: &[MappingSnapshot],
    volumes: &[VolumeSnapshot],
    included_gb: f64,
    review_scenario: ReviewScenario,
) -> Vec<LocationCheck> {
    let norm = normalize_path(path);
    let vol_root = is_volume_root(&norm);
    let resolved_vol = resolve_volume_for_path(&norm, volumes);
    let vol_connected = resolved_vol.is_some_and(|v| v.connected);
    let vol_name = resolved_vol
        .map(|v| v.display_name.as_str())
        .unwrap_or("the target volume");

    let overlapping = mappings
        .iter()
        .filter(|m| !m.is_variant_showcase)
        .find(|m| paths_overlap(&norm, &m.local_path));

    let exists_and_empty_ok =
        !norm.is_empty() && vol_connected && review_scenario != ReviewScenario::BothPopulated;
    let exists_and_empty_msg = if norm.is_empty() {
        "Choose a local folder before continuing.".to_owned()
    } else if !vol_connected {
        format!("{vol_name} is not connected in this demo snapshot. Choose a connected volume.")
    } else if review_scenario == ReviewScenario::BothPopulated {
        "Both the local folder and cloud folder already contain files. Choose an empty local folder.".to_owned()
    } else {
        "Folder exists and contains no files (per demo fixture).".to_owned()
    };

    let enough_space_ok =
        resolved_vol.is_some_and(|v| v.connected && (v.free_gb as f64) >= included_gb);
    let space_msg = match resolved_vol {
        Some(v) if !v.connected => {
            format!(
                "Free space on {} is unknown while the drive is not connected.",
                v.display_name
            )
        }
        Some(v) if (v.free_gb as f64) < included_gb => format!(
            "{} has {} free, which is less than the {:.1} GB selected.",
            v.display_name, v.free_label, included_gb
        ),
        Some(v) => format!(
            "{} has {} free for about {:.1} GB selected.",
            v.display_name, v.free_label, included_gb
        ),
        None => "Target volume is not recognized in demo volumes.".to_owned(),
    };

    vec![
        LocationCheck {
            ok: exists_and_empty_ok,
            message: exists_and_empty_msg,
        },
        LocationCheck {
            ok: overlapping.is_none(),
            message: match overlapping {
                Some(m) => format!(
                    "{norm} overlaps with existing mapping “{}” ({}). Choose a separate folder.",
                    m.display_name, m.local_path
                ),
                None => "Not inside another mapping, and no mapping is inside it.".to_owned(),
            },
        },
        LocationCheck {
            ok: !vol_root,
            message: if vol_root {
                format!(
                    "{norm} is the top of the volume. Choose or create a folder inside it, such as {norm}/Projects."
                )
            } else {
                "Not the top of the volume.".to_owned()
            },
        },
        LocationCheck {
            ok: enough_space_ok,
            message: space_msg,
        },
        LocationCheck {
            ok: true,
            message: "No folder of another sync app was detected.".to_owned(),
        },
        LocationCheck {
            ok: vol_connected,
            message: if vol_connected {
                format!(
                    "Volume identity recorded. When {vol_name} is reconnected, sync continues without asking."
                )
            } else {
                format!("Cannot record volume identity while {vol_name} is not connected.")
            },
        },
    ]
}

/// Evaluates Recovery step checks against the chosen local recovery path and demo fixtures.
pub fn evaluate_recovery_checks(
    local_recovery_path: &str,
    draft_local_path: &str,
    mappings: &[MappingSnapshot],
    volumes: &[VolumeSnapshot],
) -> Vec<LocationCheck> {
    let norm = normalize_path(local_recovery_path);
    let vol_root = is_volume_root(&norm);
    let resolved_vol = resolve_volume_for_path(&norm, volumes);
    let vol_connected = resolved_vol.is_some_and(|v| v.connected);

    let overlaps_draft =
        !draft_local_path.trim().is_empty() && paths_overlap(&norm, draft_local_path);
    let overlaps_existing = mappings
        .iter()
        .filter(|m| !m.is_variant_showcase)
        .find(|m| paths_overlap(&norm, &m.local_path));

    let outside_ok =
        !norm.is_empty() && !vol_root && !overlaps_draft && overlaps_existing.is_none();
    let outside_msg = if norm.is_empty() {
        "Choose a local recovery folder outside every synced folder.".to_owned()
    } else if vol_root {
        format!("{norm} is the top of a volume. Choose a recovery subfolder inside it.")
    } else if overlaps_draft {
        "Local recovery folder must be outside the folder being set up.".to_owned()
    } else if let Some(m) = overlaps_existing {
        format!(
            "Local recovery folder overlaps existing mapping “{}” ({}). Choose another location.",
            m.display_name, m.local_path
        )
    } else {
        "Outside every synced folder.".to_owned()
    };

    vec![
        LocationCheck {
            ok: outside_ok,
            message: outside_msg,
        },
        LocationCheck {
            ok: vol_connected && !norm.is_empty(),
            message: if vol_connected && !norm.is_empty() {
                "Target volume is connected and writable (per demo fixture).".to_owned()
            } else {
                "Local recovery folder must be on a connected volume.".to_owned()
            },
        },
    ]
}

fn step_mapping_demo_state(m: &mut MappingSnapshot) {
    let next = match (m.state.readiness(), m.state.execution, m.state.intent) {
        (Readiness::Ready, Execution::Idle, Intent::Enabled) => (
            MappingState::new(Readiness::Ready, Execution::Queued, Intent::Enabled),
            StatusModifier::QueuedBehind {
                active_name: "Research".to_owned(),
            },
        ),
        (Readiness::Ready, Execution::Queued, Intent::Enabled) => (
            MappingState::new(Readiness::Ready, Execution::Running, Intent::Enabled),
            StatusModifier::None,
        ),
        (Readiness::Ready, Execution::Running, Intent::Enabled) => (
            MappingState::new(Readiness::Ready, Execution::Stopping, Intent::Enabled),
            StatusModifier::None,
        ),
        (Readiness::Ready, Execution::Stopping, _) => (
            MappingState::new(Readiness::Ready, Execution::Idle, Intent::Paused),
            StatusModifier::PausedOn {
                date_label: "today".to_owned(),
            },
        ),
        (Readiness::Ready, Execution::Idle, Intent::Paused) => (
            MappingState::new(
                Readiness::VolumeUnavailable,
                Execution::Idle,
                Intent::Enabled,
            ),
            StatusModifier::None,
        ),
        (Readiness::VolumeUnavailable, _, _) => (
            MappingState::new(
                Readiness::AuthenticationRequired,
                Execution::Idle,
                Intent::Enabled,
            ),
            StatusModifier::AuthProblem {
                account_label: m.account_id.to_string(),
                keychain_locked: false,
            },
        ),
        (Readiness::AuthenticationRequired, _, _) => (
            MappingState::new(Readiness::EndpointChanged, Execution::Idle, Intent::Enabled),
            StatusModifier::EndpointChange {
                is_cloud: true,
                expected_id: m.cloud_folder_id.clone(),
                found_id: "1Zx…m9T".to_owned(),
            },
        ),
        (Readiness::EndpointChanged, _, _) => (
            MappingState::new(
                Readiness::RecoveryRequired,
                Execution::Interrupted,
                Intent::Enabled,
            ),
            StatusModifier::RecoveryCause {
                kind: RecoveryCauseKind::Interrupted,
                detail: "Last run was interrupted at 09:14".to_owned(),
                recovery_copies_count: 3,
                ambiguous_ownership: false,
            },
        ),
        (Readiness::RecoveryRequired, _, _) => (
            MappingState::new(
                Readiness::NeedsInitialization,
                Execution::Idle,
                Intent::Enabled,
            ),
            StatusModifier::None,
        ),
        _ => (
            MappingState::new(Readiness::Ready, Execution::Idle, Intent::Enabled),
            StatusModifier::None,
        ),
    };
    m.state = next.0;
    m.modifier = next.1;
    m.available_actions = available_actions(&m.state);
}

fn recount_account_mappings(snap: &mut UiSnapshot) {
    for acct in &mut snap.accounts {
        acct.mapping_count = snap
            .mappings
            .iter()
            .filter(|m| !m.is_variant_showcase && m.account_id == acct.id)
            .count();
    }
}

fn default_wizard_draft(skip_account: bool) -> WizardDraft {
    let local_path = "/Volumes/WorkSSD/Projects".to_owned();
    WizardDraft {
        step: if skip_account {
            WizardStep::Folder
        } else {
            WizardStep::Account
        },
        selected_account: AccountId::new("household"),
        selected_folder: "Projects".to_owned(),
        location_checks: Vec::new(),
        local_path,
        volume_name: "WorkSSD".to_owned(),
        volume_kind: VolumeKind::External,
        volume_fs: "APFS".to_owned(),
        volume_free_gb: 1400,
        volume_total_gb: 2000,
        subfolders: vec![
            SubfolderNode {
                id: "root".to_owned(),
                name: "Projects".to_owned(),
                depth: 0,
                has_children: true,
                size_gb: 18.6,
                size_label: "18.6 GB".to_owned(),
                included: true,
            },
            SubfolderNode {
                id: "proposals".to_owned(),
                name: "Proposals".to_owned(),
                depth: 1,
                has_children: false,
                size_gb: 1.1,
                size_label: "1.1 GB".to_owned(),
                included: true,
            },
            SubfolderNode {
                id: "field-data".to_owned(),
                name: "Field-Data".to_owned(),
                depth: 1,
                has_children: true,
                size_gb: 9.8,
                size_label: "9.8 GB".to_owned(),
                included: true,
            },
            SubfolderNode {
                id: "renders".to_owned(),
                name: "Renders".to_owned(),
                depth: 1,
                has_children: true,
                size_gb: 4.4,
                size_label: "4.4 GB".to_owned(),
                included: false,
            },
            SubfolderNode {
                id: "reports".to_owned(),
                name: "Reports".to_owned(),
                depth: 1,
                has_children: false,
                size_gb: 3.3,
                size_label: "3.3 GB".to_owned(),
                included: true,
            },
        ],
        custom_patterns: Vec::new(),
        show_default_exclusions: false,
        local_recovery_path: "/Volumes/WorkSSD/FolderSyncRecovery/Projects".to_owned(),
        cloud_recovery_path: "household:Recovery/projects/studio-mac".to_owned(),
        recovery_checks: Vec::new(),
        review_scenario: ReviewScenario::CloudToLocal,
        no_other_sync_confirmed: false,
        first_sync_percent: 33,
        first_sync_elapsed_secs: 21 * 60 + 40,
    }
}

fn default_engine_options() -> Vec<String> {
    vec![
        "--check-access --check-filename RCLONE_TEST".to_owned(),
        "--max-delete 10 --compare size,modtime".to_owned(),
        "--conflict-resolve none --conflict-loser num".to_owned(),
        "--check-sync true --resilient --recover".to_owned(),
        "--drive-skip-gdocs --drive-skip-shortcuts".to_owned(),
        "--filters-file <generation filters>".to_owned(),
        "(config, password and work-directory paths redacted)".to_owned(),
    ]
}

fn build_snapshot(preset: DemoPreset) -> UiSnapshot {
    let vol_internal = VolumeIdentity::new(VolumeKind::Internal, "apfs-internal-studio-mac");
    let vol_work = VolumeIdentity::new(VolumeKind::External, "apfs-workssd-01");
    let vol_photo = VolumeIdentity::new(VolumeKind::External, "apfs-photossd-02");
    let vol_archive = VolumeIdentity::new(VolumeKind::External, "apfs-archivessd-03");

    let volumes = vec![
        VolumeSnapshot {
            identity: vol_internal.clone(),
            display_name: "Internal SSD".to_owned(),
            filesystem: "APFS".to_owned(),
            connected: true,
            total_gb: 994,
            free_gb: 212,
            free_label: "212 GB".to_owned(),
            total_label: "994 GB".to_owned(),
            used_percent: 79,
            low_space: false,
            last_seen_label: None,
            recovery_root_label: Some("~/FolderSyncRecovery".to_owned()),
        },
        VolumeSnapshot {
            identity: vol_work.clone(),
            display_name: "WorkSSD".to_owned(),
            filesystem: "APFS".to_owned(),
            connected: true,
            total_gb: 2000,
            free_gb: 1400,
            free_label: "1.4 TB".to_owned(),
            total_label: "2 TB".to_owned(),
            used_percent: 30,
            low_space: false,
            last_seen_label: None,
            recovery_root_label: Some("/Volumes/WorkSSD/FolderSyncRecovery".to_owned()),
        },
        VolumeSnapshot {
            identity: vol_photo.clone(),
            display_name: "PhotoSSD".to_owned(),
            filesystem: "APFS".to_owned(),
            connected: false,
            total_gb: 4000,
            free_gb: 0,
            free_label: "Unknown".to_owned(),
            total_label: "4 TB".to_owned(),
            used_percent: 0,
            low_space: false,
            last_seen_label: Some("yesterday at 21:10".to_owned()),
            recovery_root_label: Some("/Volumes/PhotoSSD/FolderSyncRecovery".to_owned()),
        },
        VolumeSnapshot {
            identity: vol_archive.clone(),
            display_name: "ArchiveSSD".to_owned(),
            filesystem: "APFS".to_owned(),
            connected: true,
            total_gb: 8000,
            free_gb: 260,
            free_label: "260 GB".to_owned(),
            total_label: "8 TB".to_owned(),
            used_percent: 97,
            low_space: true,
            last_seen_label: None,
            recovery_root_label: Some("/Volumes/ArchiveSSD/FolderSyncRecovery".to_owned()),
        },
    ];

    let cloud_catalog = vec![
        CloudFolderCandidate {
            name: "Archive".to_owned(),
            size_gb: Some(4600.0),
            size_label: "4.6 TB".to_owned(),
            on_this_mac: Some("On ArchiveSSD".to_owned()),
            used_by_other_computer: false,
        },
        CloudFolderCandidate {
            name: "Documents".to_owned(),
            size_gb: Some(38.0),
            size_label: "38 GB".to_owned(),
            on_this_mac: Some("On Internal SSD".to_owned()),
            used_by_other_computer: false,
        },
        CloudFolderCandidate {
            name: "Family".to_owned(),
            size_gb: Some(86.0),
            size_label: "86 GB".to_owned(),
            on_this_mac: Some("On Internal SSD".to_owned()),
            used_by_other_computer: false,
        },
        CloudFolderCandidate {
            name: "Photos-Archive".to_owned(),
            size_gb: Some(3100.0),
            size_label: "3.1 TB".to_owned(),
            on_this_mac: Some("On ArchiveSSD".to_owned()),
            used_by_other_computer: false,
        },
        CloudFolderCandidate {
            name: "Photos-Family".to_owned(),
            size_gb: Some(1200.0),
            size_label: "1.2 TB".to_owned(),
            on_this_mac: Some("On PhotoSSD".to_owned()),
            used_by_other_computer: false,
        },
        CloudFolderCandidate {
            name: "Projects".to_owned(),
            size_gb: Some(18.6),
            size_label: "18.6 GB".to_owned(),
            on_this_mac: None,
            used_by_other_computer: true,
        },
        CloudFolderCandidate {
            name: "Research".to_owned(),
            size_gb: Some(412.0),
            size_label: "412 GB".to_owned(),
            on_this_mac: Some("On WorkSSD".to_owned()),
            used_by_other_computer: false,
        },
        CloudFolderCandidate {
            name: "Shared-Media".to_owned(),
            size_gb: None,
            size_label: "Measuring…".to_owned(),
            on_this_mac: None,
            used_by_other_computer: false,
        },
        CloudFolderCandidate {
            name: "Teaching".to_owned(),
            size_gb: Some(22.0),
            size_label: "22 GB".to_owned(),
            on_this_mac: Some("On Internal SSD".to_owned()),
            used_by_other_computer: false,
        },
    ];

    let preferences = PreferencesSnapshot {
        open_at_login: true,
        open_at_login_pending: false,
        notifications_enabled: true,
        finder_extension: FinderExtensionStatus::Enabled {
            last_status_label: "14:41".to_owned(),
        },
        bandwidth_limit_mb_s: None,
        app_version: "0.1.0 (placeholder)".to_owned(),
        engine_version: "rclone <pinned version>".to_owned(),
        engine_checksum: "sha256 <checksum> · verified at launch".to_owned(),
        nsh_version: "NSH 0.6.1".to_owned(),
        system_summary: "macOS <version> · arm64".to_owned(),
    };

    if preset == DemoPreset::WelcomeNoAccount {
        return UiSnapshot {
            device_id: "studio-mac".to_owned(),
            scheduling_paused: false,
            demo_preset: preset,
            accounts: Vec::new(),
            volumes,
            mappings: Vec::new(),
            active_run: None,
            attention_items: Vec::new(),
            recent_events: Vec::new(),
            raw_log: Vec::new(),
            cloud_catalog,
            cloud_catalog_measured_at: "14:20".to_owned(),
            wizard: default_wizard_draft(false),
            preferences,
            reconnecting_account: None,
            status_notice: None,
        };
    }

    if preset == DemoPreset::WelcomeNoMappings {
        return UiSnapshot {
            device_id: "studio-mac".to_owned(),
            scheduling_paused: false,
            demo_preset: preset,
            accounts: vec![AccountSnapshot {
                id: AccountId::new("household"),
                email: "user-a@example.com".to_owned(),
                shared_parent_label: "Household (contains Data and Recovery)".to_owned(),
                status: AccountStatus::Connected,
                access_summary:
                    "Checked today at 14:30. List, upload, overwrite, move to Recovery and delete were tested on test files."
                        .to_owned(),
                mapping_count: 0,
            }],
            volumes,
            mappings: Vec::new(),
            active_run: None,
            attention_items: Vec::new(),
            recent_events: Vec::new(),
            raw_log: Vec::new(),
            cloud_catalog,
            cloud_catalog_measured_at: "14:20".to_owned(),
            wizard: default_wizard_draft(true),
            preferences,
            reconnecting_account: None,
            status_notice: None,
        };
    }

    let accounts = vec![
        AccountSnapshot {
            id: AccountId::new("household"),
            email: "user-a@example.com".to_owned(),
            shared_parent_label: "Household (contains Data and Recovery)".to_owned(),
            status: AccountStatus::Connected,
            access_summary:
                "Checked today at 14:30. List, upload, overwrite, move to Recovery and delete were tested on test files."
                    .to_owned(),
            mapping_count: 7,
        },
        AccountSnapshot {
            id: AccountId::new("lab"),
            email: "user-b@example.org".to_owned(),
            shared_parent_label: "Lab shared (contains Data and Recovery)".to_owned(),
            status: AccountStatus::SignInRequired,
            access_summary: "Sign-in expired yesterday at 17:52. Measurements is held.".to_owned(),
            mapping_count: 1,
        },
    ];

    let family_runs = vec![
        RunRecordSnapshot {
            run_id: RunId::new("run-family-04"),
            started_label: "Today 14:30".to_owned(),
            duration_label: "1m 48s".to_owned(),
            result_label: "Completed with conflicts".to_owned(),
            result_tone: Tone::Attn,
            result_icon: IconKind::Bang,
            changes_label: "14 down · 3 up · 2 conflicts".to_owned(),
        },
        RunRecordSnapshot {
            run_id: RunId::new("run-family-03"),
            started_label: "Today 14:00".to_owned(),
            duration_label: "52s".to_owned(),
            result_label: "Completed".to_owned(),
            result_tone: Tone::Ok,
            result_icon: IconKind::Check,
            changes_label: "No changes".to_owned(),
        },
        RunRecordSnapshot {
            run_id: RunId::new("run-family-02"),
            started_label: "Today 13:30".to_owned(),
            duration_label: "2m 05s".to_owned(),
            result_label: "Couldn't reach Google Drive".to_owned(),
            result_tone: Tone::Neutral,
            result_icon: IconKind::Clock,
            changes_label: "None · retried at 13:40".to_owned(),
        },
        RunRecordSnapshot {
            run_id: RunId::new("run-family-01"),
            started_label: "Today 13:00".to_owned(),
            duration_label: "49s".to_owned(),
            result_label: "Completed".to_owned(),
            result_tone: Tone::Ok,
            result_icon: IconKind::Check,
            changes_label: "2 down".to_owned(),
        },
    ];

    let mut mappings = vec![
        make_mapping(
            "documents",
            "Documents",
            "household",
            "user-a@example.com",
            "household:Data/Documents",
            "1Dc…a4K",
            "~/FolderSync/Documents",
            &vol_internal,
            "Internal SSD",
            MappingState::new(Readiness::Ready, Execution::Idle, Intent::Enabled),
            Some(LastRunSummary {
                outcome: RunOutcome::Success,
                completed_at: WallTime::new(14, 38).expect("valid wall time"),
            }),
            "Today 14:38",
            StatusModifier::None,
            5,
            "About 14:48, after the queue",
            5,
            5,
            "38 GB",
            "38 GB",
            Vec::new(),
            "~/FolderSyncRecovery/Documents",
            "410 MB · 4 runs",
            "household:Recovery/documents/studio-mac",
            Vec::new(),
            family_runs[1..].to_vec(),
            2,
            "sha256:4a7c…11e9",
            false,
        ),
        make_mapping(
            "family",
            "Family",
            "household",
            "user-a@example.com",
            "household:Data/Family",
            "1Fa…Q7c",
            "~/FolderSync/Family",
            &vol_internal,
            "Internal SSD",
            MappingState::new(Readiness::Ready, Execution::Idle, Intent::Enabled),
            Some(LastRunSummary {
                outcome: RunOutcome::SuccessWithConflicts,
                completed_at: WallTime::new(14, 32).expect("valid wall time"),
            }),
            "Today 14:32",
            StatusModifier::UnreviewedConflicts {
                count: 2,
                paths: vec![
                    "Family/Budget/2026.xlsx · and its conflict copy".to_owned(),
                    "Family/Notes/trip-plan.md · and its conflict copy".to_owned(),
                ],
            },
            5,
            "About 14:50, after the queue",
            6,
            8,
            "72 GB",
            "86 GB",
            vec![
                ("Family/Video-Raw".to_owned(), "11 GB".to_owned()),
                ("Family/Old-Backups".to_owned(), "3 GB".to_owned()),
            ],
            "~/FolderSyncRecovery/Family",
            "1.3 GB · 9 runs",
            "household:Recovery/family/studio-mac",
            vec!["Family/Scans/latest".to_owned()],
            family_runs.clone(),
            3,
            "sha256:7f21…88d2",
            false,
        ),
        make_mapping(
            "teaching",
            "Teaching",
            "household",
            "user-a@example.com",
            "household:Data/Teaching",
            "1Tc…b3M",
            "~/FolderSync/Teaching",
            &vol_internal,
            "Internal SSD",
            MappingState::new(
                Readiness::RecoveryRequired,
                Execution::Interrupted,
                Intent::Enabled,
            ),
            Some(LastRunSummary {
                outcome: RunOutcome::Interrupted,
                completed_at: WallTime::new(9, 14).expect("valid wall time"),
            }),
            "Today 08:02",
            StatusModifier::RecoveryCause {
                kind: RecoveryCauseKind::Interrupted,
                detail: "Last run was interrupted at 09:14".to_owned(),
                recovery_copies_count: 3,
                ambiguous_ownership: false,
            },
            15,
            "Held until recovery",
            4,
            4,
            "22 GB",
            "22 GB",
            Vec::new(),
            "~/FolderSyncRecovery/Teaching",
            "240 MB · 3 runs",
            "household:Recovery/teaching/studio-mac",
            Vec::new(),
            family_runs[1..].to_vec(),
            1,
            "sha256:2c09…e41a",
            false,
        ),
        make_mapping(
            "measurements",
            "Measurements",
            "lab",
            "user-b@example.org",
            "lab:Data/Measurements",
            "1Ms…p9R",
            "~/FolderSync/Measurements",
            &vol_internal,
            "Internal SSD",
            MappingState::new(
                Readiness::AuthenticationRequired,
                Execution::Idle,
                Intent::Enabled,
            ),
            Some(LastRunSummary {
                outcome: RunOutcome::AuthenticationFailure,
                completed_at: WallTime::new(17, 45).expect("valid wall time"),
            }),
            "Yesterday 17:45",
            StatusModifier::AuthProblem {
                account_label: "lab".to_owned(),
                keychain_locked: false,
            },
            15,
            "Held until lab reconnects",
            3,
            3,
            "14 GB",
            "14 GB",
            Vec::new(),
            "~/FolderSyncRecovery/Measurements",
            "110 MB · 2 runs",
            "lab:Recovery/measurements/studio-mac",
            Vec::new(),
            family_runs[1..].to_vec(),
            1,
            "sha256:6b55…09f3",
            false,
        ),
        make_mapping(
            "research",
            "Research",
            "household",
            "user-a@example.com",
            "household:Data/Research",
            "1Rs…d8W",
            "/Volumes/WorkSSD/Research",
            &vol_work,
            "WorkSSD",
            MappingState::new(Readiness::Ready, Execution::Running, Intent::Enabled),
            Some(LastRunSummary {
                outcome: RunOutcome::Success,
                completed_at: WallTime::new(13, 58).expect("valid wall time"),
            }),
            "Today 13:58",
            StatusModifier::None,
            15,
            "Running now",
            10,
            10,
            "412 GB",
            "412 GB",
            Vec::new(),
            "/Volumes/WorkSSD/FolderSyncRecovery/Research",
            "2.8 GB · 12 runs",
            "household:Recovery/research/studio-mac",
            Vec::new(),
            family_runs[1..].to_vec(),
            3,
            "sha256:9c1e…4b7a",
            false,
        ),
        make_mapping(
            "photos-family",
            "Photos-Family",
            "household",
            "user-a@example.com",
            "household:Data/Photos-Family",
            "1Pf…y2L",
            "/Volumes/PhotoSSD/Photos-Family",
            &vol_photo,
            "PhotoSSD",
            MappingState::new(
                Readiness::VolumeUnavailable,
                Execution::Idle,
                Intent::Enabled,
            ),
            Some(LastRunSummary {
                outcome: RunOutcome::Success,
                completed_at: WallTime::new(21, 10).expect("valid wall time"),
            }),
            "Yesterday 21:10",
            StatusModifier::None,
            30,
            "Waiting for PhotoSSD",
            12,
            12,
            "1.2 TB",
            "1.2 TB",
            Vec::new(),
            "/Volumes/PhotoSSD/FolderSyncRecovery/Photos-Family",
            "Size unknown (drive not connected)",
            "household:Recovery/photos-family/studio-mac",
            Vec::new(),
            family_runs[1..].to_vec(),
            2,
            "sha256:88d1…33b0",
            false,
        ),
        make_mapping(
            "archive",
            "Archive",
            "household",
            "user-a@example.com",
            "household:Data/Archive",
            "1Ar…k2P",
            "/Volumes/ArchiveSSD/Archive",
            &vol_archive,
            "ArchiveSSD",
            MappingState::new(Readiness::EndpointChanged, Execution::Idle, Intent::Enabled),
            Some(LastRunSummary {
                outcome: RunOutcome::EndpointMismatch,
                completed_at: WallTime::new(18, 5).expect("valid wall time"),
            }),
            "17 Nov 18:05",
            StatusModifier::EndpointChange {
                is_cloud: true,
                expected_id: "1Ar…k2P".to_owned(),
                found_id: "1Zx…m9T".to_owned(),
            },
            60,
            "Held until review",
            18,
            18,
            "4.6 TB",
            "4.6 TB",
            Vec::new(),
            "/Volumes/ArchiveSSD/FolderSyncRecovery/Archive",
            "14.2 GB · 7 runs",
            "household:Recovery/archive/studio-mac",
            Vec::new(),
            family_runs[1..].to_vec(),
            1,
            "sha256:5e10…77a4",
            false,
        ),
        make_mapping(
            "photos-archive",
            "Photos-Archive",
            "household",
            "user-a@example.com",
            "household:Data/Photos-Archive",
            "1Pa…n6H",
            "/Volumes/ArchiveSSD/Photos-Archive",
            &vol_archive,
            "ArchiveSSD",
            MappingState::new(Readiness::Ready, Execution::Idle, Intent::Paused),
            Some(LastRunSummary {
                outcome: RunOutcome::Success,
                completed_at: WallTime::new(9, 30).expect("valid wall time"),
            }),
            "16 Nov 09:30",
            StatusModifier::PausedOn {
                date_label: "16 Nov".to_owned(),
            },
            60,
            "Paused by you",
            9,
            9,
            "3.1 TB",
            "3.1 TB",
            Vec::new(),
            "/Volumes/ArchiveSSD/FolderSyncRecovery/Photos-Archive",
            "6.1 GB · 4 runs",
            "household:Recovery/photos-archive/studio-mac",
            Vec::new(),
            family_runs[1..].to_vec(),
            1,
            "sha256:1d84…92c5",
            false,
        ),
    ];

    if preset == DemoPreset::AllStates {
        // Every remaining state from the §6 Readiness × Execution × Intent table at once.
        mappings.extend([
            make_mapping(
                "projects-draft",
                "Projects",
                "household",
                "user-a@example.com",
                "household:Data/Projects",
                "1Pr…w5K",
                "/Volumes/WorkSSD/Projects",
                &vol_work,
                "WorkSSD",
                MappingState::unconfigured(),
                None,
                "Never",
                StatusModifier::None,
                15,
                "Automatic sync is off",
                4,
                5,
                "14.2 GB",
                "18.6 GB",
                vec![("Projects/Renders".to_owned(), "4.4 GB".to_owned())],
                "/Volumes/WorkSSD/FolderSyncRecovery/Projects",
                "None yet",
                "household:Recovery/projects/studio-mac",
                Vec::new(),
                Vec::new(),
                0,
                "unconfigured",
                true,
            ),
            make_mapping(
                "notes-init",
                "Notes",
                "household",
                "user-a@example.com",
                "household:Data/Notes",
                "1Nt…k8J",
                "~/FolderSync/Notes",
                &vol_internal,
                "Internal SSD",
                MappingState::new(
                    Readiness::NeedsInitialization,
                    Execution::Idle,
                    Intent::Enabled,
                ),
                None,
                "Never",
                StatusModifier::None,
                5,
                "First sync needed",
                2,
                2,
                "1.4 GB",
                "1.4 GB",
                Vec::new(),
                "~/FolderSyncRecovery/Notes",
                "None yet",
                "household:Recovery/notes/studio-mac",
                Vec::new(),
                Vec::new(),
                1,
                "sha256:a10b…44c2",
                true,
            ),
            make_mapping(
                "reports-never",
                "Reports",
                "household",
                "user-a@example.com",
                "household:Data/Reports",
                "1Rp…q2F",
                "~/FolderSync/Reports",
                &vol_internal,
                "Internal SSD",
                MappingState::new(Readiness::Ready, Execution::Idle, Intent::Enabled),
                None,
                "Never",
                StatusModifier::NeverCompleted,
                15,
                "About 14:55",
                3,
                3,
                "3.3 GB",
                "3.3 GB",
                Vec::new(),
                "~/FolderSyncRecovery/Reports",
                "None yet",
                "household:Recovery/reports/studio-mac",
                Vec::new(),
                Vec::new(),
                1,
                "sha256:b31d…55f1",
                true,
            ),
            make_mapping(
                "backups-queued",
                "Backups",
                "household",
                "user-a@example.com",
                "household:Data/Backups",
                "1Bk…m1V",
                "~/FolderSync/Backups",
                &vol_internal,
                "Internal SSD",
                MappingState::new(Readiness::Ready, Execution::Queued, Intent::Enabled),
                Some(LastRunSummary {
                    outcome: RunOutcome::Success,
                    completed_at: WallTime::new(14, 38).expect("valid wall time"),
                }),
                "Today 14:38",
                StatusModifier::QueuedBehind {
                    active_name: "Research".to_owned(),
                },
                15,
                "Runs after Research",
                2,
                2,
                "12 GB",
                "12 GB",
                Vec::new(),
                "~/FolderSyncRecovery/Backups",
                "120 MB · 2 runs",
                "household:Recovery/backups/studio-mac",
                Vec::new(),
                Vec::new(),
                1,
                "sha256:c42e…66a8",
                true,
            ),
            make_mapping(
                "scratch-run-paused",
                "Scratch",
                "household",
                "user-a@example.com",
                "household:Data/Scratch",
                "1Sc…v4X",
                "/Volumes/WorkSSD/Scratch",
                &vol_work,
                "WorkSSD",
                MappingState::new(Readiness::Ready, Execution::Running, Intent::Paused),
                Some(LastRunSummary {
                    outcome: RunOutcome::Success,
                    completed_at: WallTime::new(13, 15).expect("valid wall time"),
                }),
                "Today 13:15",
                StatusModifier::None,
                15,
                "Pauses after this run",
                2,
                2,
                "6.2 GB",
                "6.2 GB",
                Vec::new(),
                "/Volumes/WorkSSD/FolderSyncRecovery/Scratch",
                "80 MB · 1 run",
                "household:Recovery/scratch/studio-mac",
                Vec::new(),
                Vec::new(),
                1,
                "sha256:d53f…77b9",
                true,
            ),
            make_mapping(
                "media-stopping",
                "Media",
                "household",
                "user-a@example.com",
                "household:Data/Media",
                "1Md…r9C",
                "/Volumes/WorkSSD/Media",
                &vol_work,
                "WorkSSD",
                MappingState::new(Readiness::Ready, Execution::Stopping, Intent::Enabled),
                Some(LastRunSummary {
                    outcome: RunOutcome::Success,
                    completed_at: WallTime::new(13, 58).expect("valid wall time"),
                }),
                "Today 13:58",
                StatusModifier::None,
                30,
                "Stopping",
                4,
                4,
                "84 GB",
                "84 GB",
                Vec::new(),
                "/Volumes/WorkSSD/FolderSyncRecovery/Media",
                "520 MB · 3 runs",
                "household:Recovery/media/studio-mac",
                Vec::new(),
                Vec::new(),
                1,
                "sha256:e64a…88c0",
                true,
            ),
            make_mapping(
                "exports-retry",
                "Exports",
                "household",
                "user-a@example.com",
                "household:Data/Exports",
                "1Ex…t7N",
                "~/FolderSync/Exports",
                &vol_internal,
                "Internal SSD",
                MappingState::new(Readiness::Ready, Execution::Idle, Intent::Enabled),
                Some(LastRunSummary {
                    outcome: RunOutcome::RetryableFailure,
                    completed_at: WallTime::new(14, 32).expect("valid wall time"),
                }),
                "Today 14:32",
                StatusModifier::RetryPending {
                    retry_at: "14:50".to_owned(),
                },
                15,
                "Retrying at 14:50",
                3,
                3,
                "9.4 GB",
                "9.4 GB",
                Vec::new(),
                "~/FolderSyncRecovery/Exports",
                "95 MB · 2 runs",
                "household:Recovery/exports/studio-mac",
                Vec::new(),
                Vec::new(),
                1,
                "sha256:f75b…99d1",
                true,
            ),
        ]);
    }

    let active_run = Some(ActiveRunSnapshot {
        run_id: RunId::new("run-research-current"),
        mapping_id: MappingId::new("research"),
        mapping_name: "Research".to_owned(),
        volume_name: "WorkSSD".to_owned(),
        started_label: "14:41".to_owned(),
        trigger_label: "scheduled check".to_owned(),
        phase: RunPhase::Scanning {
            elapsed_secs: 3 * 60 + 12,
        },
    });

    let recent_events = vec![
        ActivityEvent {
            id: 5,
            time_label: "14:41".to_owned(),
            message: "Research: run started (scheduled check)".to_owned(),
            link: None,
        },
        ActivityEvent {
            id: 4,
            time_label: "14:38".to_owned(),
            message: "Documents: completed · 3 down".to_owned(),
            link: None,
        },
        ActivityEvent {
            id: 3,
            time_label: "14:32".to_owned(),
            message: "Family: completed with 2 conflicts".to_owned(),
            link: Some(("Show conflicts".to_owned(), ScreenRoute::Attention)),
        },
        ActivityEvent {
            id: 2,
            time_label: "13:05".to_owned(),
            message: "Photos-Family: waiting for PhotoSSD (drive not connected)".to_owned(),
            link: None,
        },
        ActivityEvent {
            id: 1,
            time_label: "09:14".to_owned(),
            message: "Teaching: run interrupted. FolderSync quit before the run finished."
                .to_owned(),
            link: Some((
                "Start recovery…".to_owned(),
                ScreenRoute::Recovery(MappingId::new("teaching")),
            )),
        },
    ];

    let raw_log = vec![
        "14:41:02 INFO  Starting bisync (illustrative sample)".to_owned(),
        "14:41:02 INFO  Checking access marker files".to_owned(),
        "14:41:03 INFO  Building local and cloud listings".to_owned(),
        "14:43:40 INFO  Listing: 41,280 items checked so far".to_owned(),
        "14:44:14 INFO  Comparing listings".to_owned(),
    ];

    let mut snap = UiSnapshot {
        device_id: "studio-mac".to_owned(),
        scheduling_paused: false,
        demo_preset: preset,
        accounts,
        volumes,
        mappings,
        active_run,
        attention_items: Vec::new(),
        recent_events,
        raw_log,
        cloud_catalog,
        cloud_catalog_measured_at: "14:20".to_owned(),
        wizard: default_wizard_draft(false),
        preferences,
        reconnecting_account: None,
        status_notice: None,
    };
    recompute_wizard_checks(&mut snap);
    rebuild_attention(&mut snap);
    snap
}

#[allow(clippy::too_many_arguments)]
fn make_mapping(
    id: &str,
    name: &str,
    account_id: &str,
    account_email: &str,
    cloud_path: &str,
    cloud_folder_id: &str,
    local_path: &str,
    volume_id: &VolumeIdentity,
    volume_name: &str,
    state: MappingState,
    last_run: Option<LastRunSummary>,
    last_completed_label: &str,
    modifier: StatusModifier,
    schedule_minutes: u32,
    next_check_label: &str,
    included_subfolders: usize,
    total_subfolders: usize,
    included_size_label: &str,
    total_size_label: &str,
    excluded_subfolders: Vec<(String, String)>,
    local_recovery_path: &str,
    local_recovery_size_label: &str,
    cloud_recovery_path: &str,
    found_symlinks: Vec<String>,
    recent_runs: Vec<RunRecordSnapshot>,
    generation: u32,
    fingerprint: &str,
    is_variant_showcase: bool,
) -> MappingSnapshot {
    let available = available_actions(&state);
    MappingSnapshot {
        id: MappingId::new(id),
        display_name: name.to_owned(),
        account_id: AccountId::new(account_id),
        account_email: account_email.to_owned(),
        cloud_path: cloud_path.to_owned(),
        cloud_folder_id: cloud_folder_id.to_owned(),
        local_path: local_path.to_owned(),
        volume_id: volume_id.clone(),
        volume_name: volume_name.to_owned(),
        state,
        last_run,
        last_completed_label: last_completed_label.to_owned(),
        modifier,
        available_actions: available,
        schedule_minutes,
        next_check_label: next_check_label.to_owned(),
        included_subfolders,
        total_subfolders,
        included_size_label: included_size_label.to_owned(),
        total_size_label: total_size_label.to_owned(),
        excluded_subfolders,
        custom_exclusions: vec!["*.lrdata/".to_owned()],
        local_recovery_path: local_recovery_path.to_owned(),
        local_recovery_size_label: local_recovery_size_label.to_owned(),
        cloud_recovery_path: cloud_recovery_path.to_owned(),
        found_symlinks,
        recent_runs,
        generation,
        fingerprint: fingerprint.to_owned(),
        engine_options: default_engine_options(),
        is_variant_showcase,
    }
}

/// Rebuilds ordered Attention cards from current mapping and volume states (§5.6).
pub fn rebuild_attention(snap: &mut UiSnapshot) {
    let mut items = Vec::new();
    for m in &snap.mappings {
        if m.is_variant_showcase {
            continue;
        }
        match m.state.readiness() {
            Readiness::AuthenticationRequired => {
                let keychain_locked = matches!(
                    m.modifier,
                    StatusModifier::AuthProblem {
                        keychain_locked: true,
                        ..
                    }
                );
                items.push(AttentionItem {
                    id: format!("attn-auth-{}", m.id),
                    priority: AttentionPriority::SignIn,
                    mapping_id: Some(m.id.clone()),
                    mapping_name: Some(m.display_name.clone()),
                    pill_label: "Sign-in required".to_owned(),
                    pill_tone: Tone::Err,
                    icon: IconKind::Key,
                    is_error_accent: true,
                    title: if keychain_locked {
                        format!("{} · keychain locked", m.account_id)
                    } else {
                        m.display_name.clone()
                    },
                    cause: if keychain_locked {
                        "FolderSync can't read its saved sign-in because the login keychain is locked.".to_owned()
                    } else {
                        format!(
                            "The sign-in for the account {} ({}) has expired or was revoked, so FolderSync can't reach Google Drive for it.",
                            m.account_id, m.account_email
                        )
                    },
                    what_did: "Holds every mapping of this account (1 mapping). No files were changed.".to_owned(),
                    next_step: if keychain_locked {
                        "Unlock the keychain (log in again or open Keychain Access), then choose Check again.".to_owned()
                    } else {
                        "Reconnect the account in your browser. Syncing continues after the checks pass.".to_owned()
                    },
                    kind: AttentionKind::Authentication {
                        account_id: m.account_id.clone(),
                        keychain_locked,
                    },
                });
            }
            Readiness::RecoveryRequired => {
                let cause_kind = match &m.modifier {
                    StatusModifier::RecoveryCause { kind, .. } => *kind,
                    _ => RecoveryCauseKind::Interrupted,
                };
                let (title, cause, next_step) = match cause_kind {
                    RecoveryCauseKind::Interrupted => (
                        format!("{} · last run was interrupted", m.display_name),
                        "FolderSync quit at 09:14 before the run finished, so it can't confirm what the run completed.".to_owned(),
                        "Start recovery. FolderSync checks that no earlier sync process is still running, shows the log and recovery copies, and offers a normal run if that is safe.".to_owned(),
                    ),
                    RecoveryCauseKind::DeletionLimit => (
                        format!("{} · stopped by the deletion limit", m.display_name),
                        "The run would have deleted 14% of tracked files (limit: 10%). Nothing was deleted.".to_owned(),
                        "Look at the deletions in the run log. If they were a mistake, put the files back and choose Check again. If you meant them, see the help topic “Deleting many files”.".to_owned(),
                    ),
                    RecoveryCauseKind::EarlierSyncRunning => (
                        format!("{} · earlier sync still running", m.display_name),
                        format!("A sync process from an earlier run of FolderSync is still running for {}.", m.display_name),
                        "Wait for it to finish, or stop it. FolderSync won't start a second one.".to_owned(),
                    ),
                };
                items.push(AttentionItem {
                    id: format!("attn-rec-{}", m.id),
                    priority: AttentionPriority::Recovery,
                    mapping_id: Some(m.id.clone()),
                    mapping_name: Some(m.display_name.clone()),
                    pill_label: "Recovery required".to_owned(),
                    pill_tone: Tone::Err,
                    icon: IconKind::Octagon,
                    is_error_accent: true,
                    title,
                    cause,
                    what_did:
                        "Holds this mapping and keeps the run log and its state for inspection."
                            .to_owned(),
                    next_step,
                    kind: AttentionKind::RecoveryRequired { cause_kind },
                });
            }
            Readiness::EndpointChanged => {
                let (is_cloud, expected_id, found_id) = match &m.modifier {
                    StatusModifier::EndpointChange {
                        is_cloud,
                        expected_id,
                        found_id,
                    } => (*is_cloud, expected_id.clone(), found_id.clone()),
                    _ => (true, m.cloud_folder_id.clone(), "1Zx…m9T".to_owned()),
                };
                items.push(AttentionItem {
                    id: format!("attn-ep-{}", m.id),
                    priority: AttentionPriority::EndpointChanged,
                    mapping_id: Some(m.id.clone()),
                    mapping_name: Some(m.display_name.clone()),
                    pill_label: "Review needed".to_owned(),
                    pill_tone: Tone::Attn,
                    icon: IconKind::Bang,
                    is_error_accent: false,
                    title: if is_cloud {
                        format!("{} · cloud folder changed", m.display_name)
                    } else {
                        format!("{} · local folder changed", m.display_name)
                    },
                    cause: format!(
                        "{} is now a different folder than the one this mapping was set up with. It may have been renamed, moved or replaced in Google Drive.",
                        m.cloud_path
                    ),
                    what_did: "Holds this mapping, so files aren't sent to the wrong folder.".to_owned(),
                    next_step: "If the change was a mistake, undo it in Google Drive, then choose Check again. If it was intended, remove this mapping and add the folder again.".to_owned(),
                    kind: AttentionKind::EndpointChanged {
                        is_cloud,
                        expected_id,
                        found_id,
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
                    cause: format!(
                        "{} is stored on {}, which isn't connected.",
                        m.display_name, m.volume_name
                    ),
                    what_did: format!(
                        "Nothing runs for {}. Nothing is deleted in the cloud because the drive is missing.",
                        m.display_name
                    ),
                    next_step: format!(
                        "Connect {}. FolderSync checks it and continues on its own.",
                        m.volume_name
                    ),
                    kind: AttentionKind::VolumeMissing {
                        volume_name: m.volume_name.clone(),
                    },
                });
            }
            Readiness::Ready => {
                if let StatusModifier::UnreviewedConflicts { count, paths } = &m.modifier {
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
                        cause: format!(
                            "{count} files changed on both sides before they could sync. The run at 14:32 kept both versions."
                        ),
                        what_did: "Kept both versions as separate files and recorded their paths."
                            .to_owned(),
                        next_step: "Open both versions and keep the one you want under the original name. Rename or delete the other. The next run syncs your choice. Some apps may not find the file at its old name until you do this.".to_owned(),
                        kind: AttentionKind::Conflicts {
                            conflict_paths: paths.clone(),
                        },
                    });
                }
            }
            Readiness::Unconfigured | Readiness::NeedsInitialization => {}
        }
    }

    // Advisory: low free space on connected volumes hosting mappings.
    for vol in &snap.volumes {
        if vol.connected && vol.low_space && !snap.mappings_for_volume(&vol.identity).is_empty() {
            items.push(AttentionItem {
                id: format!("attn-space-{}", vol.identity.stable_id),
                priority: AttentionPriority::LowSpaceAdvisory,
                mapping_id: None,
                mapping_name: None,
                pill_label: "Low space".to_owned(),
                pill_tone: Tone::Attn,
                icon: IconKind::Bang,
                is_error_accent: false,
                title: format!("{} · {} free", vol.display_name, vol.free_label),
                cause: format!(
                    "Recovery copies for mappings on {} are kept on that drive, which is nearly full.",
                    vol.display_name
                ),
                what_did: "FolderSync doesn't delete recovery copies by itself.".to_owned(),
                next_step: format!(
                    "Free space on {}, or move older recovery copies elsewhere after checking them.",
                    vol.display_name
                ),
                kind: AttentionKind::LowSpaceAdvisory {
                    volume_name: vol.display_name.clone(),
                    recovery_path: vol
                        .recovery_root_label
                        .clone()
                        .unwrap_or_else(|| "/Volumes/ArchiveSSD/FolderSyncRecovery".to_owned()),
                },
            });
        }
    }

    items.sort_by_key(|item| item.priority);
    snap.attention_items = items;
}
