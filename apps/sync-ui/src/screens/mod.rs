//! Screen views and shared UI controller bridging NSH signals to either the
//! real background-threaded [`ManagerSource`] (default) or the in-memory
//! [`DemoSource`] (`FOLDERSYNC_DEMO=1` / `--demo`).

pub mod accounts;
pub mod activity;
pub mod attention;
pub mod detail;
pub mod overview;
pub mod preferences;
pub mod sidebar;
pub mod welcome;
pub mod wizard;

use std::rc::Rc;

use nsh_base::alert_dialog;
use nsh_core::prelude::*;
use sync_core::{MappingId, RunId};

use crate::contract::{ScreenRoute, UiCommand, UiSnapshot, WizardCheck, WizardStep};
use crate::demo::DemoSource;
use crate::manager_source::ManagerSource;

fn stopped_first_sync_actions(ctrl: &AppController, id: &MappingId, name: &str) -> View {
    use crate::theme::{BtnKind, app_button};

    let resume = ctrl.clone();
    let remove = ctrl.clone();
    let resume_id = id.clone();
    let remove_id = id.clone();
    let name = name.to_owned();
    view()
        .flex_row()
        .flex_wrap()
        .gap(px(6.0))
        .child(
            app_button(
                "Resume first sync",
                BtnKind::Primary,
                true,
                false,
                None,
                None,
                move || {
                    resume.send(UiCommand::ResumeFirstSync(resume_id.clone()));
                    if let AppDataSource::Demo(_) = &resume.source
                        && resume.snapshot.with_untracked(|snap| {
                            snap.wizard.first_sync_mapping_id.as_ref() == Some(&resume_id)
                                && snap.wizard.step == crate::contract::WizardStep::FirstSync
                                && snap.action_error.is_none()
                        })
                    {
                        resume.navigate(ScreenRoute::AddMapping);
                    }
                },
            )
            .test_id(&format!("resume-first-sync-{id}")),
        )
        .child(
            app_button(
                "Remove mapping…",
                BtnKind::Default,
                true,
                false,
                None,
                None,
                move || {
                    remove.confirm_remove_mapping(remove_id.clone(), name.clone());
                },
            )
            .test_id(&format!("remove-stopped-first-sync-{id}")),
        )
}

/// Active backend backing the FolderSync desktop UI.
#[derive(Clone)]
pub enum AppDataSource {
    /// Real `sync-manager` running on a dedicated worker thread (default).
    Manager(Rc<ManagerSource>),
    /// Pure in-memory demo source (`FOLDERSYNC_DEMO=1` or `--demo`).
    Demo(Rc<DemoSource>),
}

/// Lightweight handle passed into every screen to read snapshots, dispatch
/// `UiCommand`s, and switch routes.
#[derive(Clone)]
pub struct AppController {
    pub snapshot: Signal<UiSnapshot>,
    pub route: Signal<ScreenRoute>,
    /// 1 Hz elapsed-second counter read only by active-run elapsed labels so
    /// unrelated views, inputs, and buttons never rebuild on timer ticks.
    pub elapsed_offset: Signal<u64>,
    pub schedule_clock: Signal<chrono::DateTime<chrono::Utc>>,
    /// True while controlled shutdown is waiting for an active run to finish stopping.
    pub quitting: Signal<bool>,
    pub source: AppDataSource,
}

impl AppController {
    /// Creates a controller backed by the real `ManagerSource`.
    pub fn new_manager(manager: Rc<ManagerSource>) -> Self {
        let initial = manager.snapshot();
        Self {
            snapshot: signal(initial),
            route: signal(ScreenRoute::Overview),
            elapsed_offset: signal(0),
            schedule_clock: signal(chrono::Utc::now()),
            quitting: signal(false),
            source: AppDataSource::Manager(manager),
        }
    }

    /// Creates a controller backed by the in-memory `DemoSource`.
    pub fn new_demo(demo: Rc<DemoSource>) -> Self {
        let initial = demo.snapshot();
        Self {
            snapshot: signal(initial),
            route: signal(ScreenRoute::Overview),
            elapsed_offset: signal(0),
            schedule_clock: signal(chrono::Utc::now()),
            quitting: signal(false),
            source: AppDataSource::Demo(demo),
        }
    }

    /// Backwards-compatible constructor for demo tests.
    pub fn new(demo: Rc<DemoSource>) -> Self {
        Self::new_demo(demo)
    }

    /// Applies an immutable snapshot on the main thread and announces attention changes.
    pub fn apply_snapshot(&self, next: UiSnapshot) {
        let prev_attn = self.snapshot.with_untracked(|s| s.attention_badge_count());
        let next_attn = next.attention_badge_count();
        self.snapshot.set(next);
        if next_attn > prev_attn {
            announce_for_accessibility(format!("{next_attn} items need attention."));
        }
    }

    /// Dispatches a `UiCommand` without blocking the NSH thread.
    pub fn send(&self, cmd: UiCommand) {
        let reset_elapsed = matches!(
            cmd,
            UiCommand::DemoSetPreset(_)
                | UiCommand::DemoSetActivityPhase(_)
                | UiCommand::SyncNow(_)
                | UiCommand::StopRun(_)
                | UiCommand::ForceStopRun(_)
        );
        if reset_elapsed {
            self.elapsed_offset.set(0);
        }
        match &self.source {
            AppDataSource::Manager(mgr) => {
                if let UiCommand::WizardSetStep(step) = &cmd {
                    let allowed = self.snapshot.with_untracked(|snap| {
                        let wizard = &snap.wizard;
                        wizard
                            .transition_check
                            .as_ref()
                            .is_none_or(|check| check.waiting && *step < wizard.step)
                    });
                    if !allowed {
                        return;
                    }
                    self.snapshot.update(|snap| {
                        let wizard = &mut snap.wizard;
                        if wizard.transition_check.is_none()
                            && wizard.can_visit_step(*step)
                            && let Some(label) = wizard.transition_label(
                                *step,
                                snap.cloud_catalog.is_empty(),
                                snap.folder_size_pending,
                            )
                        {
                            wizard.step = *step;
                            wizard.transition_check = Some(WizardCheck::new(label));
                            wizard.location_check_pending = *step >= WizardStep::Location;
                            wizard.location_checks.clear();
                            wizard.recovery_checks.clear();
                        }
                    });
                }
                mgr.dispatch(cmd);
            }
            AppDataSource::Demo(demo) => {
                let next = demo.dispatch(cmd);
                self.apply_snapshot(next);
            }
        }
    }

    /// Navigates to a route in the content area.
    pub fn navigate(&self, route: ScreenRoute) {
        if route != ScreenRoute::AddMapping && self.route.get_untracked() == ScreenRoute::AddMapping
        {
            self.send(UiCommand::WizardLeave);
        }
        self.route.set(route);
    }

    /// Opens the Add-mapping wizard (resets to a fresh draft).
    pub fn open_wizard(&self) {
        self.send(UiCommand::WizardReset);
        self.navigate(ScreenRoute::AddMapping);
    }

    /// Loads run-specific log and recovery copies and opens the Run Inspection view (§12, #9).
    pub fn open_run_inspection(&self, mapping_id: MappingId, run_id: RunId) {
        self.send(UiCommand::InspectRun {
            mapping_id: mapping_id.clone(),
            run_id: run_id.clone(),
        });
        self.navigate(ScreenRoute::RunInspection { mapping_id, run_id });
    }

    /// Inspects recovery evidence and opens the guided Recovery checklist (§5.6, #9).
    pub fn open_recovery(&self, mapping_id: MappingId) {
        self.send(UiCommand::InspectRecovery(mapping_id.clone()));
        self.navigate(ScreenRoute::Recovery(mapping_id));
    }

    /// Shows the native `NSAlert` confirmation before removing a mapping (§5.3).
    pub fn confirm_remove_mapping(&self, id: MappingId, name: String) {
        let ctrl = self.clone();
        let answer = alert_dialog()
            .title(format!("Remove the mapping “{name}”?"))
            .description(
                "FolderSync stops syncing this folder. The local folder and the cloud folder both stay as they are, and recovery copies are kept.",
            )
            .cancel("cancel", "Cancel")
            .destructive("remove", "Remove mapping")
            .close_on_escape(true)
            .show();
        spawn(async move {
            if let Ok(res) = answer.await
                && res.action.as_deref() == Some("remove")
            {
                ctrl.send(UiCommand::RemoveMapping(id));
                ctrl.navigate(ScreenRoute::Overview);
            }
        });
    }

    /// Shows the native `NSAlert` confirmation before force-stopping a run (§5.5).
    pub fn confirm_force_stop(&self, id: MappingId, name: String) {
        let ctrl = self.clone();
        let answer = alert_dialog()
            .title(format!("Force stop the run for “{name}”?"))
            .description(
                "The run ends at once. Its result will be uncertain, and the mapping will need recovery before it syncs again.",
            )
            .cancel("wait", "Keep waiting")
            .destructive("force", "Force stop")
            .close_on_escape(true)
            .show();
        spawn(async move {
            if let Ok(res) = answer.await
                && res.action.as_deref() == Some("force")
            {
                ctrl.send(UiCommand::ForceStopRun(id));
            }
        });
    }

    /// Single controlled-stop quit handler used by tray Quit, ⌘Q, and system quit (§5.9).
    pub fn request_controlled_quit(&self) {
        if self.quitting.get_untracked() {
            return;
        }
        let snap = self.snapshot.get_untracked();
        if let Some(run) = snap.active_run {
            let ctrl = self.clone();
            let name = run.mapping_name;
            let answer = alert_dialog()
                .title(format!("Quit while “{name}” is syncing?"))
                .description(
                    "FolderSync asks the run to stop and waits for it. This can take about 90 seconds. Changes already made are not undone. Nothing syncs until you open FolderSync again.",
                )
                .cancel("cancel", "Cancel")
                .action("quit", "Stop and quit")
                .close_on_escape(true)
                .show();
            spawn(async move {
                match answer.await {
                    Ok(res) if res.action.as_deref() == Some("quit") => {
                        ctrl.begin_shutdown();
                    }
                    _ => {
                        nsh_core::element::desktop::cancel_quit_request();
                    }
                }
            });
        } else {
            self.begin_shutdown();
        }
    }

    fn begin_shutdown(&self) {
        self.quitting.set(true);
        match &self.source {
            AppDataSource::Manager(mgr) => {
                // Worker thread stops scheduling, gracefully stops any active run,
                // waits up to the bounded timeout, persists Interrupted/RecoveryRequired
                // on timeout, and sends `UiUpdate::QuitReady` to terminate cleanly.
                mgr.request_controlled_shutdown();
            }
            AppDataSource::Demo(_) => {
                self.send(UiCommand::RequestControlledQuit);
                nsh_core::element::desktop::quit();
            }
        }
    }
}
