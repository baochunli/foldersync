//! Screen views and shared UI controller bridging signals to `DemoSource`.

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
use sync_core::MappingId;

use crate::contract::{ScreenRoute, UiCommand, UiSnapshot};
use crate::demo::DemoSource;

/// Lightweight handle passed into every screen to read snapshots, dispatch
/// `UiCommand`s to the in-memory demo source, and switch routes.
#[derive(Clone)]
pub struct AppController {
    pub snapshot: Signal<UiSnapshot>,
    pub route: Signal<ScreenRoute>,
    /// 1 Hz elapsed-second counter read only by active-run elapsed labels so
    /// unrelated views, inputs, and buttons never rebuild on timer ticks.
    pub elapsed_offset: Signal<u64>,
    pub demo: Rc<DemoSource>,
}

impl AppController {
    /// Creates a new controller backed by the given `DemoSource`.
    pub fn new(demo: Rc<DemoSource>) -> Self {
        let initial = demo.snapshot();
        Self {
            snapshot: signal(initial),
            route: signal(ScreenRoute::Overview),
            elapsed_offset: signal(0),
            demo,
        }
    }

    /// Dispatches a `UiCommand` to the demo source and updates the reactive snapshot.
    pub fn send(&self, cmd: UiCommand) {
        let reset_elapsed = matches!(
            cmd,
            UiCommand::DemoSetPreset(_)
                | UiCommand::DemoSetActivityPhase(_)
                | UiCommand::SyncNow(_)
                | UiCommand::StopRun(_)
                | UiCommand::ForceStopRun(_)
        );
        let prev_attn = self.snapshot.with_untracked(|s| s.attention_badge_count());
        let next = self.demo.dispatch(cmd);
        let next_attn = next.attention_badge_count();
        if reset_elapsed {
            self.elapsed_offset.set(0);
        }
        self.snapshot.set(next);
        if next_attn > prev_attn {
            announce_for_accessibility(format!("{next_attn} items need attention."));
        }
    }

    /// Navigates to a route in the content area.
    pub fn navigate(&self, route: ScreenRoute) {
        self.route.set(route);
    }

    /// Opens the Add-mapping wizard (resets to a fresh draft when needed).
    pub fn open_wizard(&self) {
        self.send(UiCommand::WizardReset);
        self.navigate(ScreenRoute::AddMapping);
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
                        ctrl.send(UiCommand::RequestControlledQuit);
                        nsh_core::element::desktop::quit();
                    }
                    _ => {
                        nsh_core::element::desktop::cancel_quit_request();
                    }
                }
            });
        } else {
            self.send(UiCommand::RequestControlledQuit);
            nsh_core::element::desktop::quit();
        }
    }
}
