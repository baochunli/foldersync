//! In-memory demo source driving the FolderSync UI when `FOLDERSYNC_DEMO=1`
//! (or `--demo`) is explicitly set.
//!
//! Never touches the filesystem, rclone, Keychain, real macOS system services,
//! or `sync-manager`. All sample data is neutral and matches `ui-spec.md` §2 and §6.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::Arc;

use sync_core::{
    AccountId, Execution, Intent, LastRunSummary, MappingId, MappingState, ObservedEvent,
    PostRecoveryReadiness, Readiness, RunId, RunOutcome, UserAction, VolumeIdentity, VolumeKind,
    WallTime, apply_event, apply_user_action, available_actions,
};

use crate::contract::{
    AccountSnapshot, AccountStatus, ActiveRunSnapshot, ActivityEvent, AttentionItem, AttentionKind,
    AttentionPriority, CloudFolderCandidate, DemoActivityPhase, DemoPreset, FinderExtensionStatus,
    FolderSize, IconKind, InspectedRunSnapshot, LocationCheck, MappingSnapshot,
    PreferencesSnapshot, RecoveryCauseKind, RecoveryChecklistData, RecoveryCopyItem,
    ReviewScenario, RunPhase, RunRecordSnapshot, ScheduledCheck, ScreenRoute, StatusModifier,
    SubfolderNode, Tone, UiCommand, UiSnapshot, UnreviewedConflicts, VolumeSnapshot, WizardDraft,
    WizardStep,
};
use crate::system_actions::{RecordingSystemActions, SystemActions};

/// In-memory manager stand-in used only when demo mode is explicitly requested.
pub struct DemoSource {
    snapshot: RefCell<UiSnapshot>,
    next_event_id: RefCell<u64>,
    next_account_seq: RefCell<u32>,
    prior_account_status: RefCell<BTreeMap<AccountId, AccountStatus>>,
    system_actions: Arc<dyn SystemActions>,
}

impl Default for DemoSource {
    fn default() -> Self {
        Self::new(DemoPreset::AllStates)
    }
}

impl DemoSource {
    /// Builds a fresh in-memory demo source for the requested preset with a recording fake
    /// for `SystemActions` (never touches real macOS services).
    pub fn new(preset: DemoPreset) -> Self {
        Self::with_system_actions(preset, Arc::new(RecordingSystemActions::new()))
    }

    /// Builds a demo source with an explicit `SystemActions` implementation (always a fake in tests/demo).
    pub fn with_system_actions(preset: DemoPreset, system_actions: Arc<dyn SystemActions>) -> Self {
        let snap = build_snapshot(preset);
        Self {
            snapshot: RefCell::new(snap),
            next_event_id: RefCell::new(100),
            next_account_seq: RefCell::new(2),
            prior_account_status: RefCell::new(BTreeMap::new()),
            system_actions,
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
                self.prior_account_status.borrow_mut().clear();
            }
            UiCommand::DismissError => {
                snap.action_error = None;
                snap.status_notice = None;
            }
            UiCommand::PollFinderActions => {}
            UiCommand::SyncNow(id) => {
                let mut event_msg = None;
                let mut new_active_run = None;
                let active_name = snap.active_run.as_ref().map(|r| r.mapping_name.clone());
                let mut move_to_front = false;
                let mut enqueue_back = false;
                if let Some(m) = snap.mappings.iter_mut().find(|m| m.id == id) {
                    if m.state.execution == Execution::Queued {
                        move_to_front = true;
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
                                if let Some(active) = active_name.clone() {
                                    m.modifier = StatusModifier::QueuedBehind {
                                        active_name: active,
                                    };
                                } else {
                                    m.modifier = StatusModifier::None;
                                }
                                enqueue_back = true;
                                event_msg = Some(format!("{}: queued (Sync now)", m.display_name));
                            }
                        }
                        m.available_actions = available_actions(&m.state);
                    }
                }
                if move_to_front {
                    snap.queue_order.retain(|q| q != &id);
                    snap.queue_order.insert(0, id);
                } else if enqueue_back && !snap.queue_order.contains(&id) {
                    snap.queue_order.push(id);
                }
                if let Some(run) = new_active_run {
                    snap.queue_order.retain(|q| q != &run.mapping_id);
                    snap.active_run = Some(run);
                }
                if let Some(msg) = event_msg {
                    self.push_event(&mut snap, "14:46", msg, None);
                }
            }
            UiCommand::SyncReadyMappings => {
                // #6: Start the first eligible mapping when idle, and enqueue all remaining
                // eligible mappings in real `queue_order` behind the real active run.
                let mut started_name = None;
                let mut queued_names = Vec::new();
                let mut newly_queued_ids = Vec::new();

                let mut current_active_name =
                    snap.active_run.as_ref().map(|r| r.mapping_name.clone());
                let mut new_active_run = None;

                for m in &mut snap.mappings {
                    if !m.is_variant_showcase
                        && m.state.readiness() == Readiness::Ready
                        && m.state.execution == Execution::Idle
                        && m.state.intent == Intent::Enabled
                    {
                        let res = apply_user_action(m.state, UserAction::RunNow);
                        m.state = res.state;
                        if m.state.execution == Execution::Queued {
                            if current_active_name.is_none() {
                                let started = apply_event(m.state, ObservedEvent::RunStarted);
                                m.state = started.state;
                                current_active_name = Some(m.display_name.clone());
                                started_name = Some(m.display_name.clone());
                                new_active_run = Some(ActiveRunSnapshot {
                                    run_id: RunId::new(format!("run-demo-{}", m.id)),
                                    mapping_id: m.id.clone(),
                                    mapping_name: m.display_name.clone(),
                                    volume_name: m.volume_name.clone(),
                                    started_label: "14:46".to_owned(),
                                    trigger_label: "Sync ready mappings".to_owned(),
                                    phase: RunPhase::Scanning { elapsed_secs: 1 },
                                });
                            } else {
                                m.modifier = StatusModifier::QueuedBehind {
                                    active_name: current_active_name.clone().unwrap_or_default(),
                                };
                                newly_queued_ids.push(m.id.clone());
                                queued_names.push(m.display_name.clone());
                            }
                        }
                        m.available_actions = available_actions(&m.state);
                    }
                }
                if let Some(run) = new_active_run {
                    snap.queue_order.retain(|q| q != &run.mapping_id);
                    snap.active_run = Some(run);
                }
                for qid in newly_queued_ids {
                    if !snap.queue_order.contains(&qid) {
                        snap.queue_order.push(qid);
                    }
                }
                match (started_name, queued_names.is_empty()) {
                    (Some(started), true) => {
                        self.push_event(
                            &mut snap,
                            "14:46",
                            format!("{started}: run started (Sync ready mappings)"),
                            None,
                        );
                    }
                    (Some(started), false) => {
                        self.push_event(
                            &mut snap,
                            "14:46",
                            format!(
                                "Started {started}; queued ready mappings: {}",
                                queued_names.join(", ")
                            ),
                            None,
                        );
                    }
                    (None, false) => {
                        self.push_event(
                            &mut snap,
                            "14:46",
                            format!("Queued ready mappings: {}", queued_names.join(", ")),
                            None,
                        );
                    }
                    (None, true) => {}
                }
            }
            UiCommand::PauseMapping(id) => {
                let mut event_msg = None;
                let mut remove_from_queue = false;
                if let Some(m) = snap.mappings.iter_mut().find(|m| m.id == id) {
                    let res = apply_user_action(m.state, UserAction::Pause);
                    m.state = res.state;
                    if m.state.execution != Execution::Queued {
                        remove_from_queue = true;
                    }
                    if m.state.readiness() == Readiness::Ready
                        && m.state.execution == Execution::Idle
                    {
                        m.modifier = StatusModifier::PausedOn {
                            date_label: "today".to_owned(),
                        };
                    }
                    // #7: `m.unreviewed_conflicts` is intentionally untouched by Pause.
                    m.available_actions = available_actions(&m.state);
                    event_msg = Some(format!("{}: paused by you", m.display_name));
                }
                if remove_from_queue {
                    snap.queue_order.retain(|q| q != &id);
                }
                if let Some(msg) = event_msg {
                    self.push_event(&mut snap, "14:46", msg, None);
                }
                rebuild_attention(&mut snap);
            }
            UiCommand::ResumeMapping(id) => {
                let mut event_msg = None;
                let active_name = snap.active_run.as_ref().map(|r| r.mapping_name.clone());
                if let Some(m) = snap.mappings.iter_mut().find(|m| m.id == id) {
                    let res = apply_user_action(m.state, UserAction::Resume);
                    m.state = res.state;
                    if matches!(m.modifier, StatusModifier::PausedOn { .. }) {
                        if m.state.execution == Execution::Queued {
                            m.modifier = match active_name.clone() {
                                Some(active) => StatusModifier::QueuedBehind {
                                    active_name: active,
                                },
                                None => StatusModifier::None,
                            };
                        } else {
                            m.modifier = StatusModifier::None;
                        }
                    }
                    // #7: `m.unreviewed_conflicts` is intentionally preserved on Resume.
                    m.available_actions = available_actions(&m.state);
                    event_msg = Some(format!("{}: resumed", m.display_name));
                }
                if let Some(msg) = event_msg {
                    self.push_event(&mut snap, "14:46", msg, None);
                }
                rebuild_attention(&mut snap);
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
                    promote_next_queued(&mut snap);
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
                // #7: Clear ONLY the unreviewed conflict flag; preserve `RunOutcome::SuccessWithConflicts`
                // on `last_run` so historical outcome remains truthful.
                let mut event_msg = None;
                if let Some(m) = snap.mappings.iter_mut().find(|m| m.id == id)
                    && (m.unreviewed_conflicts.is_some()
                        || matches!(m.modifier, StatusModifier::UnreviewedConflicts { .. }))
                {
                    m.unreviewed_conflicts = None;
                    if matches!(m.modifier, StatusModifier::UnreviewedConflicts { .. }) {
                        m.modifier = StatusModifier::None;
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
            UiCommand::InspectRun { mapping_id, run_id } => {
                if let Some(m) = snap.mapping(&mapping_id).cloned() {
                    snap.inspected_run = Some(build_demo_inspected_run(&m, &run_id));
                }
            }
            UiCommand::CopyRecoveryCopyToInspection {
                run_id,
                is_remote: _,
                relative_path,
                dest_dir,
            } => {
                let norm_dest = normalize_path(&dest_dir);
                let overlaps_live = snap
                    .mappings
                    .iter()
                    .filter(|m| !m.is_variant_showcase)
                    .any(|m| paths_overlap(&norm_dest, &m.local_path));
                if overlaps_live {
                    snap.action_error = Some(
                        "Choose a folder outside your synced folders. FolderSync never restores recovery copies directly over a live synced folder.".to_owned(),
                    );
                } else {
                    let file_name = relative_path.rsplit('/').next().unwrap_or(&relative_path);
                    let notice = format!(
                        "Simulated copy of {relative_path} (run {run_id}) to {norm_dest}/{file_name} (demo mode — no files written)."
                    );
                    if let Some(insp) = &mut snap.inspected_run {
                        insp.copy_notice = Some(notice.clone());
                    }
                    snap.status_notice = Some(notice.clone());
                    self.push_event(&mut snap, "14:46", notice, None);
                }
            }
            UiCommand::InspectRecovery(mapping_id) => {
                if let Some(m) = snap.mapping(&mapping_id).cloned() {
                    let (cause_kind, detail, ambiguous) = match &m.modifier {
                        StatusModifier::RecoveryCause {
                            kind,
                            detail,
                            ambiguous_ownership,
                            ..
                        } => (*kind, detail.clone(), *ambiguous_ownership),
                        _ => (
                            RecoveryCauseKind::Interrupted,
                            "Recovery inspection requested.".to_owned(),
                            false,
                        ),
                    };
                    let latest_run_id = m
                        .recent_runs
                        .first()
                        .map(|r| r.run_id.clone())
                        .unwrap_or_else(|| RunId::new(format!("run-{}-interrupted", m.id)));
                    let recovery_copies = demo_recovery_copies_for(&m.id, &latest_run_id);
                    snap.recovery_checklist = Some(RecoveryChecklistData {
                        mapping_id: m.id.clone(),
                        inspected: true,
                        is_paused: m.state.intent == Intent::Paused,
                        orphan_alive: if ambiguous {
                            None
                        } else {
                            Some(cause_kind == RecoveryCauseKind::EarlierSyncRunning)
                        },
                        has_ambiguous_case: ambiguous,
                        has_lock_file: cause_kind == RecoveryCauseKind::EarlierSyncRunning,
                        workdir_files: vec!["listing.local.lst".to_owned(), "listing.remote.lst".to_owned()],
                        cause: detail,
                        next_step: "Review the run log and recovery copies, then choose Run normally and resume or Keep paused.".to_owned(),
                        latest_run_id: Some(latest_run_id),
                        recovery_copies,
                        identities_verified: m.state.readiness() != Readiness::EndpointChanged,
                        identity_check_detail: if m.state.readiness() == Readiness::EndpointChanged
                        {
                            "Folder identity changed since setup; review before resuming."
                                .to_owned()
                        } else {
                            "Local and cloud folders match setup.".to_owned()
                        },
                    });
                }
            }
            UiCommand::CompleteRecovery {
                mapping_id,
                resume_and_run,
            } => {
                let mut event_msg = None;
                let active_name = snap.active_run.as_ref().map(|r| r.mapping_name.clone());
                let mut enqueue_id = None;
                let mut new_active = None;
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
                        let queued = apply_user_action(m.state, UserAction::RunNow);
                        m.state = queued.state;
                        if active_name.is_none() {
                            let started = apply_event(m.state, ObservedEvent::RunStarted);
                            m.state = started.state;
                            m.modifier = StatusModifier::None;
                            new_active = Some(ActiveRunSnapshot {
                                run_id: RunId::new(format!("run-demo-rec-{}", m.id)),
                                mapping_id: m.id.clone(),
                                mapping_name: m.display_name.clone(),
                                volume_name: m.volume_name.clone(),
                                started_label: "14:46".to_owned(),
                                trigger_label: "Recovery".to_owned(),
                                phase: RunPhase::Scanning { elapsed_secs: 1 },
                            });
                            event_msg = Some(format!(
                                "{}: recovery complete — normal run started",
                                m.display_name
                            ));
                        } else {
                            m.modifier = StatusModifier::QueuedBehind {
                                active_name: active_name.clone().unwrap_or_default(),
                            };
                            enqueue_id = Some(m.id.clone());
                            event_msg = Some(format!(
                                "{}: recovery complete — queued for a normal run",
                                m.display_name
                            ));
                        }
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
                if let Some(run) = new_active {
                    snap.active_run = Some(run);
                }
                if let Some(qid) = enqueue_id
                    && !snap.queue_order.contains(&qid)
                {
                    snap.queue_order.push(qid);
                }
                if snap
                    .recovery_checklist
                    .as_ref()
                    .is_some_and(|rc| rc.mapping_id == mapping_id)
                {
                    snap.recovery_checklist = None;
                }
                if let Some(msg) = event_msg {
                    self.push_event(&mut snap, "14:46", msg, None);
                }
                rebuild_attention(&mut snap);
            }
            UiCommand::RemoveMapping(id) | UiCommand::DiscardDraft(id) => {
                if let Some(pos) = snap.mappings.iter().position(|m| m.id == id) {
                    let removed = snap.mappings.remove(pos);
                    snap.queue_order.retain(|q| q != &removed.id);
                    if snap
                        .active_run
                        .as_ref()
                        .is_some_and(|r| r.mapping_id == removed.id)
                    {
                        snap.active_run = None;
                        promote_next_queued(&mut snap);
                    }
                    // Free folder in cloud catalog if no other real mapping uses it.
                    let still_mapped = snap
                        .mappings
                        .iter()
                        .any(|m| !m.is_variant_showcase && m.display_name == removed.display_name);
                    if !still_mapped
                        && let Some(c) = snap
                            .cloud_catalog
                            .iter_mut()
                            .find(|c| c.name == removed.display_name)
                    {
                        c.on_this_mac = None;
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
            UiCommand::ResumeDraft(id) => {
                if let Some(m) = snap.mapping(&id).cloned() {
                    load_mapping_into_wizard(&mut snap, &m, WizardStep::Location);
                }
            }
            UiCommand::FinishSetup(id) => {
                if let Some(m) = snap.mapping(&id).cloned() {
                    load_mapping_into_wizard(&mut snap, &m, WizardStep::Review);
                }
            }
            UiCommand::ChangeSubfolders(id) => {
                if let Some(m) = snap.mapping(&id).cloned() {
                    snap.wizard = default_wizard_draft(true);
                    load_mapping_into_wizard(&mut snap, &m, WizardStep::Subfolders);
                    snap.wizard.changing_subfolders = true;
                    snap.wizard.first_sync_mapping_id = None;
                    snap.wizard.step = WizardStep::Subfolders;
                    snap.wizard.volume_free_bytes =
                        Some(snap.wizard.volume_free_gb * 1_000_000_000);
                    if m.id.as_str() == "family" {
                        snap.wizard.subfolders = [
                            ("root", "Family", 86.0, true),
                            ("photos", "Photos", 36.0, true),
                            ("videos", "Videos", 22.0, true),
                            ("scans", "Scans", 6.0, true),
                            ("albums", "Albums", 5.0, true),
                            ("shared", "Shared", 2.0, true),
                            ("recipes", "Recipes", 1.0, true),
                            ("video-raw", "Video-Raw", 11.0, false),
                            ("old-backups", "Old-Backups", 3.0, false),
                        ]
                        .into_iter()
                        .map(|(id, name, size_gb, included)| SubfolderNode {
                            id: id.to_owned(),
                            name: name.to_owned(),
                            depth: u32::from(id != "root"),
                            has_children: id == "root",
                            size_gb,
                            size_label: format!("{size_gb:.0} GB"),
                            included,
                        })
                        .collect();
                    }
                }
            }
            UiCommand::PreviewFilterChange if snap.wizard.changing_subfolders => {
                let Some(m) = snap
                    .wizard
                    .editing_mapping_id
                    .as_ref()
                    .and_then(|id| snap.mapping(id))
                else {
                    return snap.clone();
                };
                let previous: std::collections::BTreeSet<_> = m
                    .excluded_subfolders
                    .iter()
                    .filter_map(|(path, _)| {
                        path.rsplit('/').next().map(|name| format!("/{name}/**"))
                    })
                    .chain(m.custom_exclusions.iter().cloned())
                    .collect();
                let proposed = snap.wizard.extra_exclusions();
                snap.wizard.filter_review = Some(crate::contract::FilterReview {
                    added: proposed.difference(&previous).cloned().collect(),
                    removed: previous.difference(&proposed).cloned().collect(),
                    checks: vec![
                        LocationCheck {
                            ok: true,
                            message: "Local folder checked · 0 files".to_owned(),
                        },
                        LocationCheck {
                            ok: true,
                            message: "Cloud folder checked · download first sync".to_owned(),
                        },
                    ],
                });
                snap.wizard.no_other_sync_confirmed = false;
                snap.wizard.step = WizardStep::Review;
            }
            UiCommand::ApplyFilterChange
                if snap.wizard.changing_subfolders
                    && snap.wizard.no_other_sync_confirmed
                    && snap.wizard.step == WizardStep::Review
                    && snap
                        .wizard
                        .filter_review
                        .as_ref()
                        .is_some_and(|review| review.can_apply()) =>
            {
                if let Some(id) = snap.wizard.editing_mapping_id.clone() {
                    let excluded = snap
                        .wizard
                        .subfolders
                        .iter()
                        .filter(|n| n.depth == 1 && !n.included)
                        .map(|n| (n.name.clone(), n.size_label.clone()))
                        .collect();
                    let custom_patterns = snap.wizard.custom_patterns.clone();
                    if let Some(mapping) = snap.mappings.iter_mut().find(|m| m.id == id) {
                        mapping.excluded_subfolders = excluded;
                        mapping.custom_exclusions = custom_patterns;
                        mapping.generation += 1;
                        mapping.scheduling_enabled = false;
                        mapping.state = MappingState::new(
                            Readiness::NeedsInitialization,
                            Execution::Queued,
                            mapping.state.intent,
                        );
                        mapping.available_actions = available_actions(&mapping.state);
                    }
                    snap.queue_order.retain(|id_in_queue| id_in_queue != &id);
                    snap.queue_order.insert(0, id.clone());
                    snap.wizard.first_sync_mapping_id = Some(id);
                    snap.wizard.step = WizardStep::FirstSync;
                }
            }
            UiCommand::PreviewFilterChange | UiCommand::ApplyFilterChange => {}
            UiCommand::SetScheduleInterval {
                mapping_id,
                minutes,
            } => {
                if let Some(m) = snap.mappings.iter_mut().find(|m| m.id == mapping_id) {
                    m.schedule_minutes = minutes;
                    m.next_check_label = "Time not available".to_owned();
                    m.next_check = None;
                }
            }
            UiCommand::ConnectDrive { .. } | UiCommand::ImportDrive { .. } => {}
            UiCommand::ReopenDriveAuthorization => {
                if snap.reconnecting_account.is_some() {
                    let _ = self
                        .system_actions
                        .open_drive_authorization("https://accounts.google.com/o/oauth2/auth");
                    self.push_event(
                        &mut snap,
                        "14:46",
                        "Reopened sign-in (demo)".to_owned(),
                        None,
                    );
                }
            }
            UiCommand::ConnectDemoAccount => {
                if snap.accounts.is_empty() {
                    *snap = build_snapshot(DemoPreset::WelcomeNoMappings);
                } else {
                    let mut seq = self.next_account_seq.borrow_mut();
                    let new_id = AccountId::new(format!("work-{}", *seq));
                    *seq += 1;
                    snap.reconnecting_account = Some(new_id);
                }
            }
            UiCommand::AddLocalFixtureAccount {
                id,
                display_name,
                parent_dir,
            } => {
                let acct_id = AccountId::new(id);
                if !snap.accounts.iter().any(|a| a.id == acct_id) {
                    snap.accounts.push(AccountSnapshot {
                        id: acct_id.clone(),
                        email: display_name,
                        shared_parent_label: format!("{parent_dir} (demo local fixture)"),
                        status: AccountStatus::Connected,
                        access_summary: "Ready for local testing (demo mode).".to_owned(),
                        is_drive: true,
                        mapping_count: 0,
                    });
                }
            }
            UiCommand::ReconnectAccount(account_id) => {
                snap.reconnecting_account = Some(account_id.clone());
                if let Some(acct) = snap.accounts.iter_mut().find(|a| a.id == account_id) {
                    if acct.status != AccountStatus::Reconnecting {
                        self.prior_account_status
                            .borrow_mut()
                            .insert(account_id, acct.status.clone());
                    }
                    acct.status = AccountStatus::Reconnecting;
                }
            }
            UiCommand::CancelReconnect => {
                // #10: Restore the account's previous status instead of demoting Connected -> SignInRequired.
                if let Some(account_id) = snap.reconnecting_account.take() {
                    let prior = self
                        .prior_account_status
                        .borrow_mut()
                        .remove(&account_id)
                        .unwrap_or(AccountStatus::SignInRequired);
                    if let Some(acct) = snap.accounts.iter_mut().find(|a| a.id == account_id) {
                        acct.status = prior;
                    }
                }
            }
            UiCommand::CheckAccountAccess(account_id) => {
                // #10: If completing sign-in for a brand-new account, actually add it!
                self.prior_account_status.borrow_mut().remove(&account_id);
                if let Some(acct) = snap.accounts.iter_mut().find(|a| a.id == account_id) {
                    acct.status = AccountStatus::Connected;
                    acct.access_summary =
                        "Checked today at 14:46. List, upload, overwrite, move to Recovery and delete were tested on test files."
                            .to_owned();
                } else {
                    snap.accounts.push(AccountSnapshot {
                        id: account_id.clone(),
                        email: format!("{account_id}@example.com"),
                        shared_parent_label: format!(
                            "{account_id} shared (contains Data and Recovery)"
                        ),
                        status: AccountStatus::Connected,
                        access_summary:
                            "Checked today at 14:46. List, upload, overwrite, move to Recovery and delete were tested on test files."
                                .to_owned(),
                        is_drive: true,
                        mapping_count: 0,
                    });
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
                    self.prior_account_status.borrow_mut().remove(&account_id);
                }
            }
            UiCommand::SetOpenAtLogin(enabled) => {
                let _ = self.system_actions.set_launch_at_login(enabled);
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
                    if step != WizardStep::Folder {
                        cancel_demo_size(&mut snap);
                    }
                    snap.wizard.step = step;
                }
            }
            UiCommand::WizardSelectAccount(acct) => {
                cancel_demo_size(&mut snap);
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
                cancel_demo_size(&mut snap);
                apply_wizard_folder_selection(&mut snap, &folder);
                if let Some(item) = snap
                    .cloud_catalog
                    .iter_mut()
                    .find(|item| item.name == folder)
                    && item.size.bytes().is_none()
                {
                    item.size = FolderSize::Calculating;
                    snap.folder_size_pending = true;
                }
            }
            UiCommand::WizardCancelFolderSize | UiCommand::WizardLeave => {
                cancel_demo_size(&mut snap)
            }
            UiCommand::WizardRefreshFolders => {}
            UiCommand::DemoFailFolderSize => {
                let selected = snap.wizard.selected_folder.clone();
                if let Some(item) = snap
                    .cloud_catalog
                    .iter_mut()
                    .find(|item| item.name == selected && item.size == FolderSize::Calculating)
                {
                    item.size = FolderSize::Failed {
                        cause: "Some files did not report a size.".to_owned(),
                    };
                    snap.folder_size_pending = false;
                }
            }
            UiCommand::WizardCreateNewFolder(name) => {
                let name = name.trim();
                if snap
                    .cloud_catalog
                    .iter()
                    .any(|item| item.name.eq_ignore_ascii_case(name))
                {
                    snap.action_error = Some(format!(
                        "The name `{name}` is already used in Data. Choose a different folder name."
                    ));
                } else if !name.is_empty() {
                    snap.cloud_catalog.push(CloudFolderCandidate {
                        name: name.to_owned(),
                        collection: None,
                        size: FolderSize::Known {
                            bytes: 0,
                            files: Some(0),
                            measured_at: None,
                        },
                        on_this_mac: None,
                        used_by_other_computer: false,
                    });
                    apply_wizard_folder_selection(&mut snap, name);
                    snap.wizard.review_scenario = ReviewScenario::LocalToCloud;
                    snap.wizard.marker_reused = false;
                    snap.wizard.cloud_file_count = Some(0);
                    snap.wizard.cloud_total_bytes = Some(0);
                    snap.wizard.local_file_count = Some(240);
                    snap.wizard.local_total_bytes = Some(1_200_000_000);
                    recompute_wizard_checks(&mut snap);
                }
            }
            UiCommand::WizardSetRemotePath(path) => {
                snap.wizard.remote_folder_path = Some(path);
                snap.wizard.no_other_sync_confirmed = false;
                recompute_wizard_checks(&mut snap);
            }
            UiCommand::WizardSetLocalPath(path) => {
                snap.wizard.local_path = path;
                snap.wizard.no_other_sync_confirmed = false;
                rederive_wizard_scenario(&mut snap);
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
                match scenario {
                    ReviewScenario::CloudToLocal => {
                        snap.wizard.blocked_populated_pair = None;
                        snap.wizard.cloud_file_count = Some(2_410);
                        snap.wizard.cloud_total_bytes =
                            Some((snap.wizard.total_gb().max(1.0) * 1_000_000_000.0) as u64);
                        snap.wizard.local_file_count = Some(0);
                        snap.wizard.local_total_bytes = Some(0);
                    }
                    ReviewScenario::LocalToCloud => {
                        snap.wizard.blocked_populated_pair = None;
                        snap.wizard.cloud_file_count = Some(0);
                        snap.wizard.cloud_total_bytes = Some(0);
                        snap.wizard.local_file_count = Some(812);
                        snap.wizard.local_total_bytes = Some(4_200_000_000);
                    }
                    ReviewScenario::BothPopulated => {
                        snap.wizard.blocked_populated_pair = Some((
                            snap.wizard.selected_folder.clone(),
                            normalize_path(&snap.wizard.local_path),
                        ));
                        snap.wizard.cloud_file_count = Some(2_410);
                        snap.wizard.local_file_count = Some(1_020);
                    }
                    ReviewScenario::BothEmpty => {
                        snap.wizard.blocked_populated_pair = None;
                        snap.wizard.cloud_file_count = Some(0);
                        snap.wizard.cloud_total_bytes = Some(0);
                        snap.wizard.local_file_count = Some(0);
                        snap.wizard.local_total_bytes = Some(0);
                    }
                }
                snap.wizard.no_other_sync_confirmed = false;
                recompute_wizard_checks(&mut snap);
            }
            UiCommand::WizardSetNoOtherSyncConfirmed(confirmed) => {
                snap.wizard.no_other_sync_confirmed = confirmed;
            }
            UiCommand::WizardSaveDraft => {
                // #3: Persist an Unconfigured draft mapping into the list so it can be resumed later.
                let id = upsert_wizard_mapping(&mut snap, MappingState::unconfigured());
                snap.wizard.editing_mapping_id = Some(id.clone());
                let name = snap.wizard.selected_folder.clone();
                self.push_event(
                    &mut snap,
                    "14:46",
                    format!("{name}: saved setup draft (nothing synced yet)"),
                    None,
                );
            }
            UiCommand::WizardStartFirstSync => {
                // #3: Create/update a real mapping and start its first sync cleanly.
                recompute_wizard_checks(&mut snap);
                if snap.wizard.can_start_first_sync() {
                    let id = upsert_wizard_mapping(
                        &mut snap,
                        MappingState::new(
                            Readiness::NeedsInitialization,
                            Execution::Running,
                            Intent::Enabled,
                        ),
                    );
                    let name = snap.wizard.selected_folder.clone();
                    let vol_name = snap.wizard.volume_name.clone();
                    snap.wizard.editing_mapping_id = Some(id.clone());
                    snap.wizard.first_sync_mapping_id = Some(id.clone());
                    snap.wizard.first_sync_completed = false;
                    snap.wizard.first_sync_error = None;
                    snap.wizard.step = WizardStep::FirstSync;
                    snap.wizard.first_sync_percent =
                        if snap.wizard.review_scenario == ReviewScenario::BothEmpty {
                            90
                        } else {
                            33
                        };
                    snap.wizard.first_sync_elapsed_secs = 21 * 60 + 40;
                    if snap.active_run.is_none() {
                        snap.active_run = Some(ActiveRunSnapshot {
                            run_id: RunId::new(format!("run-init-{id}")),
                            mapping_id: id,
                            mapping_name: name.clone(),
                            volume_name: vol_name,
                            started_label: "14:46".to_owned(),
                            trigger_label: "First sync".to_owned(),
                            phase: RunPhase::Scanning { elapsed_secs: 1 },
                        });
                    }
                    recount_account_mappings(&mut snap);
                    self.push_event(
                        &mut snap,
                        "14:46",
                        format!("{name}: first sync started (demo)"),
                        None,
                    );
                }
            }
            UiCommand::WizardStopFirstSync => {
                // #3: Stop an in-progress first sync honestly and return to Review.
                if let Some(mid) = snap.wizard.first_sync_mapping_id.clone() {
                    if let Some(m) = snap.mappings.iter_mut().find(|m| m.id == mid) {
                        m.state = MappingState::new(
                            Readiness::NeedsInitialization,
                            Execution::Idle,
                            Intent::Enabled,
                        );
                        m.available_actions = available_actions(&m.state);
                    }
                    if snap
                        .active_run
                        .as_ref()
                        .is_some_and(|r| r.mapping_id == mid)
                    {
                        snap.active_run = None;
                        promote_next_queued(&mut snap);
                    }
                }
                snap.wizard.first_sync_completed = false;
                snap.wizard.first_sync_error = Some(
                    "First sync stopped before completion. You can review and start again."
                        .to_owned(),
                );
                if !snap.wizard.changing_subfolders {
                    snap.wizard.step = WizardStep::Review;
                }
            }
            UiCommand::WizardCompleteDemoFirstSync => {
                // #3: Finish the demo first sync into a real Ready mapping.
                if let Some(mid) = snap.wizard.first_sync_mapping_id.clone() {
                    let mut completed_name = snap.wizard.selected_folder.clone();
                    let mut vol_name = snap.wizard.volume_name.clone();
                    snap.queue_order.retain(|queued| queued != &mid);
                    if let Some(m) = snap.mappings.iter_mut().find(|m| m.id == mid) {
                        m.state =
                            MappingState::new(Readiness::Ready, Execution::Idle, Intent::Enabled);
                        m.scheduling_enabled = true;

                        m.last_run = Some(LastRunSummary {
                            outcome: RunOutcome::Success,
                            completed_at: WallTime::new(14, 46).expect("valid wall time"),
                        });
                        m.last_completed_label = "Today 14:46".to_owned();
                        m.modifier = StatusModifier::None;
                        m.is_variant_showcase = false;
                        m.available_actions = available_actions(&m.state);
                        completed_name = m.display_name.clone();
                        vol_name = m.volume_name.clone();
                    }
                    if snap
                        .active_run
                        .as_ref()
                        .is_some_and(|r| r.mapping_id == mid)
                    {
                        snap.active_run = None;
                        promote_next_queued(&mut snap);
                    }
                    if let Some(c) = snap
                        .cloud_catalog
                        .iter_mut()
                        .find(|c| c.name == completed_name)
                    {
                        c.on_this_mac = Some(format!("On {vol_name}"));
                    }
                    snap.wizard.first_sync_percent = 100;
                    snap.wizard.first_sync_completed = true;
                    snap.wizard.first_sync_error = None;
                    recount_account_mappings(&mut snap);
                    rebuild_attention(&mut snap);
                    self.push_event(
                        &mut snap,
                        "14:46",
                        format!("{completed_name}: first sync completed; automatic sync is on"),
                        Some(("Open mapping".to_owned(), ScreenRoute::MappingDetail(mid))),
                    );
                }
            }
            UiCommand::WizardReset => {
                cancel_demo_size(&mut snap);
                let skip_acct = snap
                    .accounts
                    .iter()
                    .filter(|a| a.status == AccountStatus::Connected)
                    .count()
                    <= 1;
                snap.wizard = default_wizard_draft(skip_acct);
                recompute_wizard_checks(&mut snap);
            }
            UiCommand::RevealPath(path) => {
                let _ = self.system_actions.reveal_path(std::path::Path::new(&path));
                let msg = format!("Simulated Reveal in Finder for {path} (demo mode)");
                snap.status_notice = Some(msg.clone());
                self.push_event(&mut snap, "14:46", msg, None);
            }
            UiCommand::OpenCloudUrl(url_or_path) => {
                let _ = self.system_actions.open_url(&url_or_path);
                let msg = format!("Simulated Open cloud folder for {url_or_path} (demo mode)");
                snap.status_notice = Some(msg.clone());
                self.push_event(&mut snap, "14:46", msg, None);
            }
            UiCommand::OpenExtensionSettings => {
                let _ = self
                    .system_actions
                    .open_url("x-apple.systempreferences:com.apple.ExtensionsPreferences");
                let msg = "Simulated Open Extension Settings (demo mode)".to_owned();
                snap.status_notice = Some(msg.clone());
                self.push_event(&mut snap, "14:46", msg, None);
            }
            UiCommand::ExportDiagnostics(path) => {
                let msg = format!(
                    "Simulated diagnostics export to {path} (demo mode — no file written)."
                );
                snap.status_notice = Some(msg.clone());
                self.push_event(&mut snap, "14:46", msg, None);
            }
            UiCommand::DemoSetActivityPhase(phase) => {
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
                            snap.queue_order.retain(|q| q != &mid);
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
                        Execution::Queued => {
                            if !snap.queue_order.contains(&mid) {
                                snap.queue_order.push(mid);
                            }
                        }
                        _ => {
                            snap.queue_order.retain(|q| q != &mid);
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
                snap.status_notice = Some("Stopping… (demo mode).".to_owned());
            }
        }
        snap.active_job_mapping_id = snap.active_run.as_ref().map(|run| run.mapping_id.clone());
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

fn promote_next_queued(snap: &mut UiSnapshot) {
    if snap.active_run.is_some() {
        return;
    }
    while !snap.queue_order.is_empty() {
        let next_id = snap.queue_order.remove(0);
        if let Some(m) = snap.mappings.iter_mut().find(|m| m.id == next_id)
            && m.state.execution == Execution::Queued
            && m.state.readiness() == Readiness::Ready
            && m.state.intent == Intent::Enabled
        {
            let started = apply_event(m.state, ObservedEvent::RunStarted);
            m.state = started.state;
            m.modifier = StatusModifier::None;
            m.available_actions = available_actions(&m.state);
            snap.active_run = Some(ActiveRunSnapshot {
                run_id: RunId::new(format!("run-demo-{}", m.id)),
                mapping_id: m.id.clone(),
                mapping_name: m.display_name.clone(),
                volume_name: m.volume_name.clone(),
                started_label: "14:46".to_owned(),
                trigger_label: "Queued".to_owned(),
                phase: RunPhase::Scanning { elapsed_secs: 1 },
            });
            break;
        }
    }
}

fn cancel_demo_size(snap: &mut UiSnapshot) {
    snap.folder_size_pending = false;
    for folder in &mut snap.cloud_catalog {
        if folder.size == FolderSize::Calculating {
            folder.size = FolderSize::Unmeasured;
        }
    }
}

fn demo_subfolders_for_folder(folder: &str, size_gb: Option<f64>) -> Vec<SubfolderNode> {
    if folder == "Projects" {
        return vec![
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
        ];
    }
    let Some(gb) = size_gb else {
        // Unknown size ("Measuring…"): do not fabricate a fake 18.6 GB Projects tree (#4).
        return Vec::new();
    };
    if gb <= 0.0 {
        return vec![SubfolderNode {
            id: "root".to_owned(),
            name: folder.to_owned(),
            depth: 0,
            has_children: false,
            size_gb: 0.0,
            size_label: "0 B".to_owned(),
            included: true,
        }];
    }
    vec![SubfolderNode {
        id: "root".to_owned(),
        name: folder.to_owned(),
        depth: 0,
        has_children: false,
        size_gb: gb,
        size_label: format!("{gb:.1} GB"),
        included: true,
    }]
}

fn apply_wizard_folder_selection(snap: &mut UiSnapshot, folder: &str) {
    let candidate = snap
        .cloud_catalog
        .iter()
        .find(|c| c.name == folder)
        .cloned();
    snap.wizard.selected_folder = folder.to_owned();
    snap.wizard.local_path = format!("/Volumes/WorkSSD/{folder}");
    snap.wizard.local_recovery_path = format!("/Volumes/WorkSSD/FolderSyncRecovery/{folder}");
    snap.wizard.cloud_recovery_path = format!(
        "{}:Recovery/{}/{}",
        snap.wizard.selected_account,
        folder.to_ascii_lowercase(),
        snap.device_id
    );
    snap.wizard.no_other_sync_confirmed = false;
    snap.wizard.subfolders = demo_subfolders_for_folder(
        folder,
        candidate
            .as_ref()
            .and_then(|c| c.size.bytes())
            .map(|bytes| bytes as f64 / 1_000_000_000.0),
    );
    snap.wizard.marker_reused = candidate.as_ref().is_some_and(|c| c.used_by_other_computer);
    snap.wizard.cloud_total_bytes = candidate.as_ref().and_then(|c| c.size.bytes());
    snap.wizard.cloud_file_count = match snap.wizard.cloud_total_bytes {
        Some(0) => Some(0),
        Some(_) if folder == "Projects" => Some(2_410),
        Some(_) => Some(420),
        None => None,
    };
    snap.wizard.local_file_count = Some(0);
    snap.wizard.local_total_bytes = Some(0);
    rederive_wizard_scenario(snap);
    sync_wizard_volume_from_path(snap);
    recompute_wizard_checks(snap);
}

fn load_mapping_into_wizard(snap: &mut UiSnapshot, m: &MappingSnapshot, step: WizardStep) {
    let folder = m.display_name.clone();
    let candidate = snap
        .cloud_catalog
        .iter()
        .find(|c| c.name == folder)
        .cloned();
    snap.wizard.editing_mapping_id = Some(m.id.clone());
    snap.wizard.selected_account = m.account_id.clone();
    snap.wizard.selected_folder = folder.clone();
    snap.wizard.local_path = m.local_path.clone();
    snap.wizard.local_recovery_path = m.local_recovery_path.clone();
    snap.wizard.cloud_recovery_path = m.cloud_recovery_path.clone();
    snap.wizard.subfolders = demo_subfolders_for_folder(
        &folder,
        candidate
            .as_ref()
            .and_then(|c| c.size.bytes())
            .map(|bytes| bytes as f64 / 1_000_000_000.0),
    );
    snap.wizard.custom_patterns = m.custom_exclusions.clone();
    snap.wizard.marker_reused = candidate.as_ref().is_some_and(|c| c.used_by_other_computer);
    snap.wizard.no_other_sync_confirmed = false;
    snap.wizard.first_sync_mapping_id = Some(m.id.clone());
    snap.wizard.first_sync_completed = false;
    snap.wizard.first_sync_error = None;
    snap.wizard.review_scenario = ReviewScenario::CloudToLocal;
    snap.wizard.blocked_populated_pair = None;
    sync_wizard_volume_from_path(snap);
    recompute_wizard_checks(snap);
    snap.wizard.step = if snap.wizard.can_visit_step(step) {
        step
    } else {
        WizardStep::Location
    };
}

fn upsert_wizard_mapping(snap: &mut UiSnapshot, state: MappingState) -> MappingId {
    let draft = snap.wizard.clone();
    let id = draft.editing_mapping_id.clone().unwrap_or_else(|| {
        let slug: String = draft
            .selected_folder
            .to_ascii_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let base = if slug.is_empty() {
            "mapping".to_owned()
        } else {
            slug
        };
        if !snap.mappings.iter().any(|m| m.id.as_str() == base) {
            MappingId::new(base)
        } else {
            let mut n = 2;
            while snap
                .mappings
                .iter()
                .any(|m| m.id.as_str() == format!("{base}-{n}"))
            {
                n += 1;
            }
            MappingId::new(format!("{base}-{n}"))
        }
    });

    let vol = resolve_volume_for_path(&draft.local_path, &snap.volumes)
        .cloned()
        .unwrap_or_else(|| snap.volumes[0].clone());
    let acct_email = snap
        .accounts
        .iter()
        .find(|a| a.id == draft.selected_account)
        .map(|a| a.email.clone())
        .unwrap_or_else(|| "user-a@example.com".to_owned());

    let excluded_subs: Vec<(String, String)> = draft
        .subfolders
        .iter()
        .filter(|n| n.depth == 1 && !n.included)
        .map(|n| {
            (
                format!("{}/{}", draft.selected_folder, n.name),
                n.size_label.clone(),
            )
        })
        .collect();
    let total_subs = draft
        .subfolders
        .iter()
        .filter(|n| n.depth == 1)
        .count()
        .max(1);
    let inc_subs = total_subs.saturating_sub(excluded_subs.len());

    let new_record = make_mapping(
        id.as_str(),
        &draft.selected_folder,
        draft.selected_account.as_str(),
        &acct_email,
        &format!("{}:Data/{}", draft.selected_account, draft.selected_folder),
        "1Nw…d4R",
        &draft.local_path,
        &vol.identity,
        &vol.display_name,
        state,
        None,
        "Never",
        StatusModifier::None,
        15,
        if state.readiness() == Readiness::Unconfigured {
            "Automatic sync is off"
        } else {
            "First sync running"
        },
        inc_subs,
        total_subs,
        &format!("{:.1} GB", draft.included_gb()),
        &format!("{:.1} GB", draft.total_gb()),
        excluded_subs,
        &draft.local_recovery_path,
        "None yet",
        &draft.cloud_recovery_path,
        draft.found_symlinks.clone(),
        Vec::new(),
        1,
        "sha256:demo-init",
        false,
    );

    if let Some(existing) = snap.mappings.iter_mut().find(|m| m.id == id) {
        *existing = new_record;
    } else {
        snap.mappings.push(new_record);
    }
    id
}

fn demo_recovery_copies_for(mapping_id: &MappingId, run_id: &RunId) -> Vec<RecoveryCopyItem> {
    match mapping_id.as_str() {
        "teaching" => vec![
            RecoveryCopyItem {
                side_label: "On this Mac".to_owned(),
                is_remote: false,
                relative_path: "Lectures/week-04-slides.pdf".to_owned(),
                size_label: "4.2 MB".to_owned(),
            },
            RecoveryCopyItem {
                side_label: "On this Mac".to_owned(),
                is_remote: false,
                relative_path: "Grades/midterm-rubric.xlsx".to_owned(),
                size_label: "86 KB".to_owned(),
            },
            RecoveryCopyItem {
                side_label: "In the cloud".to_owned(),
                is_remote: true,
                relative_path: "Syllabus/2026-fall.docx".to_owned(),
                size_label: "142 KB".to_owned(),
            },
        ],
        "family" if run_id.as_str() == "run-family-04" => vec![
            RecoveryCopyItem {
                side_label: "On this Mac".to_owned(),
                is_remote: false,
                relative_path: "Budget/2026.xlsx".to_owned(),
                size_label: "64 KB".to_owned(),
            },
            RecoveryCopyItem {
                side_label: "On this Mac".to_owned(),
                is_remote: false,
                relative_path: "Notes/trip-plan.md".to_owned(),
                size_label: "12 KB".to_owned(),
            },
        ],
        _ => Vec::new(),
    }
}

fn build_demo_inspected_run(m: &MappingSnapshot, run_id: &RunId) -> InspectedRunSnapshot {
    let rec = m.recent_runs.iter().find(|r| &r.run_id == run_id);
    let started_label = rec
        .map(|r| r.started_label.clone())
        .unwrap_or_else(|| m.last_completed_label.clone());
    let result_label = rec
        .map(|r| r.result_label.clone())
        .unwrap_or_else(|| "Recorded run".to_owned());
    let changes_label = rec
        .map(|r| r.changes_label.clone())
        .unwrap_or_else(|| "See summary below".to_owned());

    let mut log_lines = vec![
        format!("Run ID: {run_id}"),
        format!("Mapping: {} ({})", m.display_name, m.cloud_path),
        format!("Started: {started_label}"),
        format!("Result: {result_label} · {changes_label}"),
    ];
    if m.id.as_str() == "teaching" {
        log_lines.push("09:14:01 INFO  Checking access marker files".to_owned());
        log_lines.push("09:14:04 INFO  Preserving 3 earlier file versions in recovery".to_owned());
        log_lines.push(
            "09:14:19 WARN  Run interrupted before completion; mapping held for recovery"
                .to_owned(),
        );
    } else if run_id.as_str() == "run-family-04" {
        log_lines.push("14:30:02 INFO  Checking access marker files".to_owned());
        log_lines.push(
            "14:31:40 NOTICE Conflict kept: Family/Budget/2026.xlsx (both versions kept)"
                .to_owned(),
        );
        log_lines.push(
            "14:31:44 NOTICE Conflict kept: Family/Notes/trip-plan.md (both versions kept)"
                .to_owned(),
        );
        log_lines.push(
            "14:31:50 INFO  Completed with conflicts (14 down, 3 up, 2 conflicts)".to_owned(),
        );
    } else {
        log_lines.push("INFO  Access markers verified".to_owned());
        log_lines.push(format!("INFO  {result_label} ({changes_label})"));
    }

    InspectedRunSnapshot {
        run_id: run_id.clone(),
        mapping_id: m.id.clone(),
        mapping_name: m.display_name.clone(),
        started_label,
        result_label,
        log_lines,
        log_path: None,
        log_exists: false,
        recovery_copies: demo_recovery_copies_for(&m.id, run_id),
        copy_notice: None,
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

fn rederive_wizard_scenario(snap: &mut UiSnapshot) {
    let norm = normalize_path(&snap.wizard.local_path);
    let pair = (snap.wizard.selected_folder.clone(), norm.clone());
    let still_blocked_pair = snap.wizard.blocked_populated_pair.as_ref() == Some(&pair);
    let editing = snap.wizard.editing_mapping_id.as_ref();
    let local_has_files = still_blocked_pair
        || snap
            .mappings
            .iter()
            .filter(|m| !m.is_variant_showcase && Some(&m.id) != editing)
            .any(|m| paths_overlap(&norm, &m.local_path));
    let cloud_has_files = snap
        .cloud_catalog
        .iter()
        .find(|c| c.name == snap.wizard.selected_folder)
        .is_none_or(|c| c.size.bytes().is_none_or(|bytes| bytes > 0));

    if snap.wizard.review_scenario == ReviewScenario::BothPopulated {
        if still_blocked_pair || (local_has_files && cloud_has_files) {
            snap.wizard.review_scenario = ReviewScenario::BothPopulated;
        } else {
            snap.wizard.blocked_populated_pair = None;
            snap.wizard.review_scenario = if cloud_has_files {
                ReviewScenario::CloudToLocal
            } else {
                ReviewScenario::BothEmpty
            };
        }
    } else if local_has_files && cloud_has_files {
        snap.wizard.review_scenario = ReviewScenario::BothPopulated;
    }
}

fn sync_wizard_volume_from_path(snap: &mut UiSnapshot) {
    if let Some(vol) = resolve_volume_for_path(&snap.wizard.local_path, &snap.volumes).cloned() {
        snap.wizard.volume_name = vol.display_name;
        snap.wizard.volume_kind = vol.identity.kind;
        snap.wizard.volume_fs = vol.filesystem;
        snap.wizard.volume_free_gb = vol.free_gb;
        snap.wizard.volume_free_bytes = Some(vol.free_gb * 1_000_000_000);
        snap.wizard.volume_total_gb = vol.total_gb;
    }
}

fn recompute_wizard_checks(snap: &mut UiSnapshot) {
    let included_gb = snap.wizard.included_gb();
    let editing = snap.wizard.editing_mapping_id.as_ref();
    let other_mappings: Vec<MappingSnapshot> = snap
        .mappings
        .iter()
        .filter(|m| Some(&m.id) != editing)
        .cloned()
        .collect();
    snap.wizard.location_checks = evaluate_location_checks(
        &snap.wizard.local_path,
        &other_mappings,
        &snap.volumes,
        included_gb,
        snap.wizard.review_scenario,
    );
    snap.wizard.recovery_checks = evaluate_recovery_checks(
        &snap.wizard.local_recovery_path,
        &snap.wizard.local_path,
        &other_mappings,
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
        changing_subfolders: false,
        filter_review: None,
        volume_free_bytes: Some(1_400_000_000_000),
        step: if skip_account {
            WizardStep::Folder
        } else {
            WizardStep::Account
        },
        editing_mapping_id: None,
        selected_account: AccountId::new("household"),
        selected_folder: "Projects".to_owned(),
        remote_folder_path: None,
        drive_collection: None,
        location_checks: Vec::new(),
        local_path,
        volume_name: "WorkSSD".to_owned(),
        volume_kind: VolumeKind::External,
        volume_fs: "APFS".to_owned(),
        volume_free_gb: 1400,
        volume_total_gb: 2000,
        subfolders: demo_subfolders_for_folder("Projects", Some(18.6)),
        custom_patterns: Vec::new(),
        show_default_exclusions: false,
        local_recovery_path: "/Volumes/WorkSSD/FolderSyncRecovery/Projects".to_owned(),
        cloud_recovery_path: "household:Recovery/projects/studio-mac".to_owned(),
        recovery_checks: Vec::new(),
        review_scenario: ReviewScenario::CloudToLocal,
        cloud_file_count: Some(2_410),
        cloud_total_bytes: Some(18_600_000_000),
        local_file_count: Some(0),
        local_total_bytes: Some(0),
        marker_reused: true,
        found_symlinks: Vec::new(),
        found_name_issues: Vec::new(),
        blocked_populated_pair: None,
        no_other_sync_confirmed: false,
        reviewed_fingerprint: None,
        first_sync_mapping_id: None,
        first_sync_preflight_done: true,
        first_sync_markers_ready: true,
        first_sync_stopping: false,
        first_sync_percent: 33,
        first_sync_elapsed_secs: 21 * 60 + 40,
        first_sync_completed: false,
        first_sync_error: None,
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
            free_bytes: Some(212_000_000_000),
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
            free_bytes: Some(1_400_000_000_000),
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
            free_bytes: None,
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
            free_bytes: Some(260_000_000_000),
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
            collection: None,
            size: FolderSize::Known {
                bytes: 4_600_000_000_000,
                files: None,
                measured_at: Some("14:20".to_owned()),
            },
            on_this_mac: Some("On ArchiveSSD".to_owned()),
            used_by_other_computer: false,
        },
        CloudFolderCandidate {
            name: "Documents".to_owned(),
            collection: None,
            size: FolderSize::Known {
                bytes: 38_000_000_000,
                files: None,
                measured_at: Some("14:20".to_owned()),
            },
            on_this_mac: Some("On Internal SSD".to_owned()),
            used_by_other_computer: false,
        },
        CloudFolderCandidate {
            name: "Family".to_owned(),
            collection: None,
            size: FolderSize::Known {
                bytes: 86_000_000_000,
                files: None,
                measured_at: Some("14:20".to_owned()),
            },
            on_this_mac: Some("On Internal SSD".to_owned()),
            used_by_other_computer: false,
        },
        CloudFolderCandidate {
            name: "Photos-Archive".to_owned(),
            collection: None,
            size: FolderSize::Known {
                bytes: 3_100_000_000_000,
                files: None,
                measured_at: Some("14:20".to_owned()),
            },
            on_this_mac: Some("On ArchiveSSD".to_owned()),
            used_by_other_computer: false,
        },
        CloudFolderCandidate {
            name: "Photos-Family".to_owned(),
            collection: None,
            size: FolderSize::Known {
                bytes: 1_200_000_000_000,
                files: None,
                measured_at: Some("14:20".to_owned()),
            },
            on_this_mac: Some("On PhotoSSD".to_owned()),
            used_by_other_computer: false,
        },
        CloudFolderCandidate {
            name: "Projects".to_owned(),
            collection: None,
            size: FolderSize::Known {
                bytes: 18_600_000_000,
                files: Some(2_410),
                measured_at: Some("14:20".to_owned()),
            },
            on_this_mac: None,
            used_by_other_computer: true,
        },
        CloudFolderCandidate {
            name: "Research".to_owned(),
            collection: None,
            size: FolderSize::Known {
                bytes: 412_000_000_000,
                files: None,
                measured_at: Some("14:20".to_owned()),
            },
            on_this_mac: Some("On WorkSSD".to_owned()),
            used_by_other_computer: false,
        },
        CloudFolderCandidate {
            name: "Shared-Media".to_owned(),
            collection: None,
            size: FolderSize::Unmeasured,
            on_this_mac: None,
            used_by_other_computer: false,
        },
        CloudFolderCandidate {
            name: "Teaching".to_owned(),
            collection: None,
            size: FolderSize::Known {
                bytes: 22_000_000_000,
                files: None,
                measured_at: Some("14:20".to_owned()),
            },
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
        app_version: "0.1.0 (demo)".to_owned(),
        engine_version: "rclone <pinned version>".to_owned(),
        engine_checksum: "sha256 <checksum> · verified at launch".to_owned(),
        nsh_version: "NSH 0.6.1".to_owned(),
        system_summary: "macOS · arm64".to_owned(),
    };

    if preset == DemoPreset::WelcomeNoAccount {
        let mut snap = UiSnapshot {
            is_demo_mode: true,
            device_id: "studio-mac".to_owned(),
            scheduling_paused: false,
            active_job_mapping_id: None,
            demo_preset: preset,
            accounts: Vec::new(),
            volumes,
            mappings: Vec::new(),
            active_run: None,
            queue_order: Vec::new(),
            attention_items: Vec::new(),
            recent_events: Vec::new(),
            raw_log: Vec::new(),
            inspected_run: None,
            recovery_checklist: None,
            cloud_catalog,
            skipped_cloud_folders: Vec::new(),
            cloud_catalog_error: None,
            folder_size_pending: false,
            wizard: default_wizard_draft(false),
            preferences,
            reconnecting_account: None,
            account_operation_pending: false,
            authorization_canceling: false,
            can_reopen_authorization: false,
            status_notice: None,
            action_error: None,
        };
        recompute_wizard_checks(&mut snap);
        return snap;
    }

    if preset == DemoPreset::WelcomeNoMappings {
        let mut snap = UiSnapshot {
            is_demo_mode: true,
            device_id: "studio-mac".to_owned(),
            scheduling_paused: false,
            active_job_mapping_id: None,
            demo_preset: preset,
            accounts: vec![AccountSnapshot {
                id: AccountId::new("household"),
                email: "user-a@example.com".to_owned(),
                shared_parent_label: "Household (contains Data and Recovery)".to_owned(),
                status: AccountStatus::Connected,
                access_summary:
                    "Checked today at 14:30. List, upload, overwrite, move to Recovery and delete were tested on test files."
                        .to_owned(),
                is_drive: true,
                mapping_count: 0,
            }],
            volumes,
            mappings: Vec::new(),
            active_run: None,
            queue_order: Vec::new(),
            attention_items: Vec::new(),
            recent_events: Vec::new(),
            raw_log: Vec::new(),
            inspected_run: None,
            recovery_checklist: None,
            cloud_catalog,
            skipped_cloud_folders: Vec::new(),
            cloud_catalog_error: None,
            folder_size_pending: false,
            wizard: default_wizard_draft(true),
            preferences,
            reconnecting_account: None,
            account_operation_pending: false,
            authorization_canceling: false,
            can_reopen_authorization: false,
            status_notice: None,
            action_error: None,
        };
        recompute_wizard_checks(&mut snap);
        return snap;
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
            is_drive: true,
            mapping_count: 7,
        },
        AccountSnapshot {
            id: AccountId::new("lab"),
            email: "user-b@example.org".to_owned(),
            shared_parent_label: "Lab shared (contains Data and Recovery)".to_owned(),
            status: AccountStatus::SignInRequired,
            access_summary: "Sign-in expired yesterday at 17:52. Measurements is held.".to_owned(),
            is_drive: true,
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

    let teaching_runs = vec![
        RunRecordSnapshot {
            run_id: RunId::new("run-teaching-interrupted"),
            started_label: "Today 09:14".to_owned(),
            duration_label: "0m 19s".to_owned(),
            result_label: "Interrupted".to_owned(),
            result_tone: Tone::Err,
            result_icon: IconKind::Octagon,
            changes_label: "3 recovery copies kept".to_owned(),
        },
        RunRecordSnapshot {
            run_id: RunId::new("run-teaching-01"),
            started_label: "Today 08:02".to_owned(),
            duration_label: "41s".to_owned(),
            result_label: "Completed".to_owned(),
            result_tone: Tone::Ok,
            result_icon: IconKind::Check,
            changes_label: "1 up".to_owned(),
        },
    ];

    let mut family_mapping = make_mapping(
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
        StatusModifier::None,
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
    );
    family_mapping.unreviewed_conflicts = Some(UnreviewedConflicts {
        run_id: RunId::new("run-family-04"),
        paths: vec![
            "Family/Budget/2026.xlsx · and its conflict copy".to_owned(),
            "Family/Notes/trip-plan.md · and its conflict copy".to_owned(),
        ],
    });

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
            "In about 7 minutes",
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
        family_mapping,
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
            teaching_runs,
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
        let mut wrong_disk = mappings
            .iter()
            .find(|m| m.id.as_str() == "photos-family")
            .unwrap()
            .clone();
        wrong_disk.id = MappingId::new("photos-wrong-disk");
        wrong_disk.volume_issue = Some("A different disk named PhotoSSD is connected".to_owned());
        wrong_disk.is_variant_showcase = true;
        mappings.push(wrong_disk);
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
        is_demo_mode: true,
        device_id: "studio-mac".to_owned(),
        scheduling_paused: false,
        active_job_mapping_id: active_run.as_ref().map(|r| r.mapping_id.clone()),
        demo_preset: preset,
        accounts,
        volumes,
        mappings,
        active_run,
        queue_order: Vec::new(),
        attention_items: Vec::new(),
        recent_events,
        raw_log,
        inspected_run: None,
        recovery_checklist: None,
        cloud_catalog,
        skipped_cloud_folders: vec![
            sync_manager::SkippedCollection {
                name: "Old Projects".to_owned(),
                reason: sync_manager::CollectionSkipReason::Duplicate,
            },
            sync_manager::SkippedCollection {
                name: "Draft:Notes".to_owned(),
                reason: sync_manager::CollectionSkipReason::UnsupportedName,
            },
        ],
        cloud_catalog_error: None,
        folder_size_pending: false,
        wizard: default_wizard_draft(false),
        preferences,
        reconnecting_account: None,
        account_operation_pending: false,
        authorization_canceling: false,
        can_reopen_authorization: false,
        status_notice: None,
        action_error: None,
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
        unreviewed_conflicts: None,
        available_actions: available,
        schedule_minutes,
        next_check_label: next_check_label.to_owned(),
        next_check: (id == "documents").then(|| ScheduledCheck {
            at: chrono::Utc::now() + chrono::TimeDelta::minutes(7),
            retry: false,
        }),
        scheduling_enabled: state.readiness() == Readiness::Ready,
        volume_issue: None,
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
            Readiness::VolumeUnavailable => items.push(m.volume_attention()),
            Readiness::Ready => {
                if let Some(unrev) = &m.unreviewed_conflicts {
                    let count = unrev.paths.len().max(1);
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
                            conflict_paths: unrev.paths.clone(),
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
