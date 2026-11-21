//! FolderSync NSH desktop interface (Group 3: approved mockups driven by an in-memory demo source).
//!
//! No real `sync-manager`, `rclone` binary, Keychain, or filesystem mutation occurs in this crate.

pub mod contract;
pub mod demo;
pub mod screens;
pub mod theme;

use std::rc::Rc;
use std::time::Duration;

use nsh_core::{
    desktop::{MenuRole, QuitRequestOptions},
    element::desktop::{
        MenuItem, main_window, on_quit_request, set_application_menu, set_quit_request_options,
        set_tray, tray,
    },
    prelude::*,
};
use sync_core::{Execution, Intent, Readiness};

use crate::contract::{ScreenRoute, UiCommand};
use crate::demo::DemoSource;
use crate::screens::{
    AppController, accounts::accounts_view, activity::activity_view, attention::attention_view,
    attention::recovery_view, detail::detail_view, overview::overview_view,
    preferences::preferences_view, sidebar::sidebar_view, wizard::wizard_view,
};
use crate::theme::{Palette, TRAY_ATTENTION_ICON, TRAY_ICON};

/// Bundled crate assets (`PaperMono-Regular.ttf`, tray template SVGs, Welcome illustration).
pub const CRATE_ASSETS: &[nsh_core::element::AssetFile] = assets!();

#[native_module("sync-ui/native")]
fn native(registry: &mut Registry) {
    registry.assets(CRATE_ASSETS).register::<App>();
}

/// Marshals a UI state update onto the main thread using NSH's documented
/// `run_on_main_thread` mechanism so worker/background threads never mutate UI signals directly.
pub fn marshal_to_main_thread(work: impl FnOnce() + Send + 'static) {
    drop(run_on_main_thread(work));
}

/// The FolderSync desktop application root component.
#[component]
pub fn App() -> impl IntoElement {
    let demo = Rc::new(DemoSource::default());
    let ctrl = AppController::new(demo);

    // Keep-alive-on-close: closing the window hides it while the tray stays active (§3, §14).
    let win = main_window();
    win.on_close_press(move |_| {
        main_window().hide();
    });

    // Controlled shutdown handler shared by tray Quit, ⌘Q, and system quit (§5.9).
    set_quit_request_options(
        QuitRequestOptions::default()
            .system(true)
            .main_window_close(false),
    );
    {
        let c_quit = ctrl.clone();
        on_quit_request(move || {
            c_quit.request_controlled_quit();
        });
    }

    // Native macOS menu-bar (tray) icon & menu (§5.9).
    // No `on_click` handler so primary click opens the menu directly.
    {
        let c_tray = ctrl.clone();
        effect(move || {
            let snap = c_tray.snapshot.get();
            let attn_count = snap.attention_badge_count();
            let ready_count = snap.ready_mapping_count();
            let sched_paused = snap.scheduling_paused;
            let status_line = snap.tray_status_line();
            let tip = snap.tray_tooltip();

            let c_sync = c_tray.clone();
            let c_pause = c_tray.clone();
            let c_quit = c_tray.clone();

            let tray_icon = if attn_count > 0 {
                TRAY_ATTENTION_ICON
            } else {
                TRAY_ICON
            };

            set_tray(
                tray().icon(tray_icon).tooltip(tip).menu([
                    MenuItem::new(status_line).disabled(true),
                    MenuItem::separator(),
                    MenuItem::new("Show window").on_select(|| {
                        main_window().focus();
                    }),
                    MenuItem::new("Sync ready mappings")
                        .disabled(ready_count == 0)
                        .on_select(move || {
                            c_sync.send(UiCommand::SyncReadyMappings);
                        }),
                    MenuItem::new("Pause scheduling")
                        .checked(sched_paused)
                        .on_select(move || {
                            c_pause.send(UiCommand::SetSchedulingPaused(!sched_paused));
                        }),
                    MenuItem::separator(),
                    MenuItem::new("Quit FolderSync")
                        .shortcut("mod+q")
                        .on_select(move || {
                            c_quit.request_controlled_quit();
                        }),
                ]),
            );
        });
    }

    // Application menu providing keyboard shortcuts while the window is active (§8):
    // ⌘N Add mapping…, ⌘R Sync now, ⌘. Stop, ⌘, Preferences.
    {
        let c_menu = ctrl.clone();
        let c_add = c_menu.clone();
        let c_sync = c_menu.clone();
        let c_stop = c_menu.clone();
        let c_prefs = c_menu.clone();
        let c_quit = c_menu.clone();

        set_application_menu(vec![
            MenuItem::submenu(
                "FolderSync",
                [
                    MenuItem::role(MenuRole::About),
                    MenuItem::separator(),
                    MenuItem::new("Preferences…")
                        .shortcut("mod+,")
                        .on_select(move || {
                            c_prefs.navigate(ScreenRoute::Preferences);
                        }),
                    MenuItem::separator(),
                    MenuItem::role(MenuRole::Hide),
                    MenuItem::role(MenuRole::HideOthers),
                    MenuItem::role(MenuRole::ShowAll),
                    MenuItem::separator(),
                    MenuItem::new("Quit FolderSync")
                        .shortcut("mod+q")
                        .on_select(move || {
                            c_quit.request_controlled_quit();
                        }),
                ],
            ),
            MenuItem::submenu(
                "Mappings",
                [
                    MenuItem::new("Add mapping…")
                        .shortcut("mod+n")
                        .on_select(move || {
                            c_add.open_wizard();
                        }),
                    MenuItem::new("Sync now")
                        .shortcut("mod+r")
                        .on_select(move || {
                            let target = match c_sync.route.get_untracked() {
                                ScreenRoute::MappingDetail(id) => Some(id),
                                _ => c_sync.snapshot.with_untracked(|s| {
                                    s.mappings
                                        .iter()
                                        .find(|m| {
                                            !m.is_variant_showcase
                                                && m.state.readiness() == Readiness::Ready
                                                && m.state.execution == Execution::Idle
                                                && m.state.intent == Intent::Enabled
                                        })
                                        .map(|m| m.id.clone())
                                }),
                            };
                            if let Some(id) = target {
                                c_sync.send(UiCommand::SyncNow(id));
                            }
                        }),
                    MenuItem::new("Stop").shortcut("mod+.").on_select(move || {
                        let active = c_stop.snapshot.with_untracked(|s| {
                            s.active_run.as_ref().map(|r| r.mapping_id.clone())
                        });
                        if let Some(id) = active {
                            c_stop.send(UiCommand::StopRun(id));
                        }
                    }),
                ],
            ),
            MenuItem::submenu(
                "Edit",
                [
                    MenuItem::role(MenuRole::Undo),
                    MenuItem::role(MenuRole::Redo),
                    MenuItem::separator(),
                    MenuItem::role(MenuRole::Cut),
                    MenuItem::role(MenuRole::Copy),
                    MenuItem::role(MenuRole::Paste),
                    MenuItem::role(MenuRole::SelectAll),
                ],
            ),
            MenuItem::submenu(
                "Window",
                [
                    MenuItem::role(MenuRole::Minimize),
                    MenuItem::role(MenuRole::Zoom),
                    MenuItem::role(MenuRole::Close),
                ],
            ),
        ]);
    }

    // Throttled 1 Hz progress refresh (~1 update/sec per §5.5 & §16).
    // Updates only the fine-grained `elapsed_offset` signal so views, inputs, and
    // focus never rebuild on timer ticks.
    {
        let c_tick = ctrl.clone();
        let tick_task = set_interval(Duration::from_secs(1), move || {
            if c_tick.snapshot.with_untracked(|s| s.active_run.is_some()) {
                c_tick.elapsed_offset.update(|v| *v = v.saturating_add(1));
            }
        });
        cleanup(move || tick_task.cancel());
    }

    let c_content = ctrl.clone();
    let c_sidebar = ctrl.clone();

    view()
        .flex_1()
        .w_full()
        .h_full()
        .flex_row()
        .bg(move || Palette::current().bg)
        .text_color(move || Palette::current().fg)
        .child(sidebar_view(c_sidebar))
        .child(
            view()
                .flex_1()
                .h_full()
                .flex_col()
                .bg(move || Palette::current().bg)
                // Top window drag bar above scrollable content
                .child(view().window_drag(true).w_full().h(move || {
                    let inset = nsh_core::element::desktop::title_bar().top.0;
                    px(inset.max(12.0))
                }))
                .child(
                    scroll().flex_1().w_full().child(
                        view()
                            .w_full()
                            .px(px(26.0))
                            .pt(px(8.0))
                            .pb(px(32.0))
                            .child(dynamic(move || match c_content.route.get() {
                                ScreenRoute::Overview => {
                                    overview_view(c_content.clone()).into_element()
                                }
                                ScreenRoute::MappingDetail(id) => {
                                    detail_view(c_content.clone(), id).into_element()
                                }
                                ScreenRoute::AddMapping => {
                                    wizard_view(c_content.clone()).into_element()
                                }
                                ScreenRoute::Activity => {
                                    activity_view(c_content.clone()).into_element()
                                }
                                ScreenRoute::Attention => {
                                    attention_view(c_content.clone()).into_element()
                                }
                                ScreenRoute::Recovery(id) => {
                                    recovery_view(c_content.clone(), id).into_element()
                                }
                                ScreenRoute::Accounts => {
                                    accounts_view(c_content.clone()).into_element()
                                }
                                ScreenRoute::Preferences => {
                                    preferences_view(c_content.clone()).into_element()
                                }
                            })),
                    ),
                ),
        )
}

#[cfg(test)]
mod tests {
    use nsh_base::Theme;
    use sync_core::{Execution, Intent, MappingId, Readiness};

    use crate::contract::{DemoPreset, ReviewScenario, UiCommand};
    use crate::demo::DemoSource;
    use crate::theme::{PAPER_MONO_FONT, mono_font_family};

    #[test]
    fn paper_mono_font_asset_and_theme_parse_cleanly() {
        assert_eq!(PAPER_MONO_FONT.family, "Paper Mono");
        assert_eq!(mono_font_family(), "Paper Mono");
        assert!(
            crate::CRATE_ASSETS
                .iter()
                .any(|a| a.uri.ends_with("fonts/PaperMono-Regular.ttf")),
            "PaperMono-Regular.ttf must be registered in crate assets"
        );

        let raw_theme = include_str!("../theme.json");
        let (parsed, ignored) = Theme::from_json_lenient(raw_theme).expect("theme.json must parse");
        assert!(
            ignored.is_empty(),
            "theme.json must have no ignored entries: {ignored:?}"
        );
        assert_eq!(parsed.font_mono.as_deref(), Some("Paper Mono"));
        assert_eq!(
            parsed.font_sans, None,
            "sans-serif must remain macOS system font (no bundled sans)"
        );

        // Verify mono_text() (used by mono_path()) explicitly applies Paper Mono to rendered path elements.
        use nsh_core::prelude::{AdaptiveColor, Styled};
        let mut path_el = crate::theme::mono_text(
            "household:Data/Documents",
            11.0,
            AdaptiveColor::rgb(0x62636b, 0xa9aab2),
        );
        let style_dbg = format!("{:?}", path_el.style());
        assert!(
            style_dbg.contains("Paper Mono"),
            "mono_text/mono_path must apply Paper Mono font family, got: {style_dbg}"
        );
    }

    #[test]
    fn demo_all_states_covers_every_spec_state_and_attention_count() {
        let demo = DemoSource::new(DemoPreset::AllStates);
        let snap = demo.snapshot();

        // Exact §2 household attention count is 5 (blocking + conflicts; advisory low-space separate).
        assert_eq!(snap.attention_badge_count(), 5);

        // Verify every Readiness × Execution × Intent combination in §6 is present at once.
        let statuses: Vec<_> = snap
            .mappings
            .iter()
            .map(|m| m.presented_status(snap.active_run.as_ref()))
            .collect();

        let has_line = |needle: &str| statuses.iter().any(|s| s.status_line.contains(needle));
        assert!(has_line("Setup not finished"));
        assert!(has_line("First sync needed"));
        assert!(has_line("Last completed at 14:38"));
        assert!(has_line("Completed with conflicts at 14:32"));
        assert!(has_line("Couldn't reach Google Drive · retrying at 14:50"));
        assert!(has_line("Not run yet"));
        assert!(has_line("Runs after Research"));
        assert!(has_line("Scanning Research · 3m 12s"));
        assert!(has_line("pauses after this run"));
        assert!(has_line("Stopping · 0:24"));
        assert!(has_line("Paused by you on 16 Nov"));
        assert!(has_line("Waiting for PhotoSSD"));
        assert!(has_line("Sign-in for lab has expired"));
        assert!(has_line("Cloud folder changed"));
        assert!(has_line("Last run was interrupted at 09:14"));
    }

    #[test]
    fn wizard_review_blocks_both_populated() {
        let demo = DemoSource::new(DemoPreset::AllStates);
        let snap = demo.dispatch(UiCommand::WizardSetNoOtherSyncConfirmed(true));
        assert!(snap.wizard.can_start_first_sync());

        let blocked = demo.dispatch(UiCommand::WizardSetReviewScenario(
            ReviewScenario::BothPopulated,
        ));
        assert!(!blocked.wizard.can_start_first_sync());
    }

    #[test]
    fn demo_commands_mutate_only_in_memory_state() {
        let demo = DemoSource::new(DemoPreset::HouseholdOnly);
        let s1 = demo.dispatch(UiCommand::PauseMapping(MappingId::new("documents")));
        let docs = s1.mapping(&MappingId::new("documents")).unwrap();
        assert_eq!(docs.state.intent, Intent::Paused);
        assert_eq!(docs.state.readiness(), Readiness::Ready);
        assert_eq!(docs.state.execution, Execution::Idle);

        let s2 = demo.dispatch(UiCommand::MarkConflictsReviewed(MappingId::new("family")));
        assert_eq!(s2.attention_badge_count(), 4);

        let s3 = demo.dispatch(UiCommand::SetSchedulingPaused(true));
        assert!(s3.scheduling_paused);
        assert_eq!(s3.tray_status_line(), "Scheduling paused");
    }
}
