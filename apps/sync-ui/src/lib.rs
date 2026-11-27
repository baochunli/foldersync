//! FolderSync NSH desktop application.
//!
//! By default the app runs against the real [`manager_source::ManagerSource`]
//! owning `sync-manager` on a background worker thread. Set `FOLDERSYNC_DEMO=1`
//! (or pass `--demo`) to run with the in-memory [`demo::DemoSource`] instead.

pub mod contract;
pub mod demo;
pub mod manager_source;
pub mod screens;
pub mod system_actions;
pub mod theme;

use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use nsh_core::{
    desktop::{MenuRole, QuitRequestOptions},
    element::desktop::{
        MenuItem, main_window, on_quit_request, set_application_menu, set_quit_request_options,
        set_tray, tray,
    },
    prelude::*,
};
#[cfg(target_os = "macos")]
use nsh_platform_macos::objc2_app_kit::NSApplicationActivationPolicy;

use crate::contract::{
    IconKind, SYNC_ALL_NOW_LABEL, ScreenRoute, Tone, UiCommand, selected_stop_target,
    selected_sync_now_target, should_show_window_at_launch,
};
use crate::demo::DemoSource;
use crate::manager_source::{ManagerSource, ManagerSourceConfig, UiControlEvent};
use crate::screens::{
    AppController, accounts::accounts_view, activity::activity_view, activity::run_inspection_view,
    attention::attention_view, attention::recovery_view, detail::detail_view,
    overview::overview_view, preferences::preferences_view, sidebar::sidebar_view,
    wizard::wizard_view,
};
use crate::system_actions::execute_system_action_on_main_thread;
use crate::theme::{
    BtnKind, LH_BODY, PAGE_BOTTOM_PADDING, PAGE_DRAG_HEIGHT, PAGE_SIDE_PADDING, PAGE_TOP_PADDING,
    Palette, S_BODY, S_COMPACT, TRAY_ATTENTION_ICON, TRAY_ICON, W_BODY, app_button, status_pill,
};

/// Bundled crate assets (`PaperMono-Regular.ttf`, tray template SVGs, Welcome illustration).
pub const CRATE_ASSETS: &[nsh_core::element::AssetFile] = assets!();

#[native_module("sync-ui/native")]
fn native(registry: &mut Registry) {
    handle_credential_helper_invocation();
    registry.assets(CRATE_ASSETS).register::<App>();
}

/// The direct binary and the NSH launcher both enter here before starting the UI.
#[doc(hidden)]
pub fn handle_credential_helper_invocation() {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() == Some(std::ffi::OsStr::new("credential-helper")) {
        let key = match (args.next(), args.next(), args.next()) {
            (Some(flag), Some(key), None) if flag == "--account" => key.into_string().ok(),
            _ => None,
        };
        let Some(key) = key else {
            eprintln!("Usage: credential-helper --account <id>");
            std::process::exit(3);
        };
        if let Err(err) = sync_manager::run_credential_helper(
            &sync_manager::SystemSecretStore,
            &key,
            &mut std::io::stdout().lock(),
        ) {
            eprintln!("{err}");
            std::process::exit(3);
        }
        std::process::exit(0);
    }
}

/// Returns `true` when in-memory demo mode is explicitly requested via
/// `FOLDERSYNC_DEMO=1` or `--demo`, OR when running in a headless test binary
/// without an explicit isolated `FOLDERSYNC_STATE_DIR` (HIGH #15).
pub fn is_demo_mode_requested() -> bool {
    if std::env::var("FOLDERSYNC_DEMO").is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        || std::env::args().any(|a| a == "--demo")
    {
        return true;
    }
    // Never default a bare headless test open to the real user's ~/Library/Application Support/FolderSync.
    let explicit_state_dir =
        std::env::var_os("FOLDERSYNC_STATE_DIR").is_some_and(|v| !v.is_empty());
    if !explicit_state_dir {
        if cfg!(test) {
            return true;
        }
        if let Ok(exe) = std::env::current_exe()
            && let Some(name) = exe.file_name().and_then(|n| n.to_str())
            && name.starts_with("nsh-app-")
            && !name.ends_with("-macos-host")
        {
            return true;
        }
    }
    false
}

/// Marshals a UI state update onto the main thread using NSH's documented
/// `run_on_main_thread` mechanism so worker/background threads never mutate UI signals directly.
pub fn marshal_to_main_thread(work: impl FnOnce() + Send + 'static) {
    drop(run_on_main_thread(work));
}

#[cfg(target_os = "macos")]
fn activation_policy_for_window(visible: bool, minimized: bool) -> NSApplicationActivationPolicy {
    if visible || minimized {
        NSApplicationActivationPolicy::Regular
    } else {
        NSApplicationActivationPolicy::Accessory
    }
}

#[cfg(target_os = "macos")]
fn update_activation_policy(policy: NSApplicationActivationPolicy) {
    use nsh_platform_macos::{
        objc2::MainThreadMarker,
        objc2_app_kit::{NSApplication, NSRunningApplication},
    };

    let Some(main_thread) = MainThreadMarker::new() else {
        return;
    };
    // Leave command-line and background NSH test hosts alone.
    if NSRunningApplication::currentApplication().activationPolicy()
        == NSApplicationActivationPolicy::Prohibited
    {
        return;
    }
    let app = NSApplication::sharedApplication(main_thread);
    if app.activationPolicy() != policy
        && app.setActivationPolicy(policy)
        && policy == NSApplicationActivationPolicy::Regular
    {
        app.activate();
    }
}

/// The FolderSync desktop application root component.
#[component]
pub fn App() -> impl IntoElement {
    let demo_mode = is_demo_mode_requested();
    let launched_at_login = std::env::args().any(|a| a == "--login-launch")
        || std::env::var_os("FOLDERSYNC_LOGIN_LAUNCH").is_some();

    let (ctrl, manager_for_updates) = if demo_mode {
        let demo = Rc::new(DemoSource::default());
        (AppController::new_demo(demo), None)
    } else {
        let mgr = Rc::new(ManagerSource::spawn_for_app(ManagerSourceConfig::default()));
        (AppController::new_manager(Rc::clone(&mgr)), Some(mgr))
    };

    // Keep-alive-on-close: closing the window hides it while the tray stays active (§3, §14).
    let win = main_window();
    win.on_close_press(move |_| {
        main_window().hide();
    });
    #[cfg(target_os = "macos")]
    effect(move || {
        let (visible, minimized) = win
            .state()
            .map(|state| (state.visible, state.minimized))
            .unwrap_or_default();
        let policy = activation_policy_for_window(visible, minimized);
        marshal_to_main_thread(move || update_activation_policy(policy));
    });

    // Spec §3: `nsh.json` starts the window hidden (`defaultVisible: false`).
    // Show it when no account or mapping exists yet, or on a manual launch (not a login launch).
    if ctrl
        .snapshot
        .with_untracked(|s| should_show_window_at_launch(s, launched_at_login))
    {
        main_window().show();
        main_window().focus();
    }

    // Consume coalesced latest snapshots & control events off the NSH thread via
    // `blocking` and marshal every update onto the main thread via `run_on_main_thread` (HIGH #8, #14).
    if let Some(mgr) = manager_for_updates {
        let slot = mgr.ui_slot();
        let mgr_for_sys = Rc::clone(&mgr);
        let c_updates = ctrl.clone();
        let mut checked_initial_launch_visibility = false;
        let task = spawn(async move {
            loop {
                let slot_clone = Arc::clone(&slot);
                let recv_res = blocking(move || slot_clone.wait_for_batch()).await;
                let Ok(Some((latest_snap, events))) = recv_res else {
                    break;
                };
                // Marshal onto the main thread before mutating signals or invoking NSH desktop APIs.
                let Ok((latest_snap, events)) =
                    run_on_main_thread(move || (latest_snap, events)).await
                else {
                    break;
                };
                if let Some(snap) = latest_snap {
                    if !checked_initial_launch_visibility {
                        checked_initial_launch_visibility = true;
                        if should_show_window_at_launch(&snap, launched_at_login) {
                            main_window().show();
                            main_window().focus();
                        }
                    }
                    c_updates.apply_snapshot(*snap);
                }
                let mut quit_now = false;
                for ev in events {
                    match ev {
                        UiControlEvent::Navigate(route) => {
                            c_updates.navigate(route);
                        }
                        UiControlEvent::ShowWindowAndNavigate(route) => {
                            c_updates.navigate(route);
                            main_window().show();
                            main_window().focus();
                        }
                        UiControlEvent::SystemAction {
                            request_id,
                            request,
                        } => {
                            let mgr_reply = Rc::clone(&mgr_for_sys);
                            spawn(async move {
                                let outcome = execute_system_action_on_main_thread(request).await;
                                mgr_reply.complete_system_action(request_id, outcome);
                            });
                        }
                        UiControlEvent::QuitDeferred => {
                            c_updates.quitting.set(false);
                            nsh_core::element::desktop::cancel_quit_request();
                        }
                        UiControlEvent::QuitReady(_outcome) => {
                            nsh_core::element::desktop::quit();
                            quit_now = true;
                            break;
                        }
                    }
                }
                if quit_now {
                    break;
                }
            }
        });
        cleanup(move || task.cancel());
    }

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
            let quitting = c_tray.quitting.get();
            let attn_count = snap.attention_badge_count();
            let ready_count = snap.ready_mapping_count();
            let sched_paused = snap.scheduling_paused;
            let status_line = if quitting {
                "Stopping…".to_owned()
            } else {
                snap.tray_status_line()
            };
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
                        main_window().show();
                        main_window().focus();
                    }),
                    MenuItem::new(SYNC_ALL_NOW_LABEL)
                        .disabled(ready_count == 0 || quitting)
                        .on_select(move || {
                            c_sync.send(UiCommand::SyncReadyMappings);
                        }),
                    MenuItem::new("Pause scheduling")
                        .checked(sched_paused)
                        .disabled(quitting)
                        .on_select(move || {
                            c_pause.send(UiCommand::SetSchedulingPaused(!sched_paused));
                        }),
                    MenuItem::separator(),
                    MenuItem::new(if quitting {
                        "Stopping…"
                    } else {
                        "Quit FolderSync"
                    })
                    .shortcut("mod+q")
                    .disabled(quitting)
                    .on_select(move || {
                        c_quit.request_controlled_quit();
                    }),
                ]),
            );
        });
    }

    // Application menu providing keyboard shortcuts while the window is active (§8):
    // ⌘N Add mapping…, ⌘R Sync now (selected mapping only), ⌘. Stop (displayed run), ⌘, Preferences.
    {
        let c_menu = ctrl.clone();
        effect(move || {
            let route = c_menu.route.get();
            let snap = c_menu.snapshot.get();
            let sync_target = selected_sync_now_target(&route, &snap);
            let stop_target = selected_stop_target(&route, &snap);

            let c_add = c_menu.clone();
            let c_sync = c_menu.clone();
            let c_stop = c_menu.clone();
            let c_prefs = c_menu.clone();
            let c_quit = c_menu.clone();

            let sync_disabled = sync_target.is_none();
            let stop_disabled = stop_target.is_none();

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
                            .disabled(sync_disabled)
                            .on_select(move || {
                                if let Some(id) = sync_target.clone() {
                                    c_sync.send(UiCommand::SyncNow(id));
                                }
                            }),
                        MenuItem::new("Stop")
                            .shortcut("mod+.")
                            .disabled(stop_disabled)
                            .on_select(move || {
                                if let Some(id) = stop_target.clone() {
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
                        MenuItem::new("Show window")
                            .shortcut("mod+0")
                            .on_select(|| {
                                main_window().show();
                                main_window().focus();
                            }),
                        MenuItem::separator(),
                        MenuItem::role(MenuRole::Minimize),
                        MenuItem::role(MenuRole::Zoom),
                        MenuItem::role(MenuRole::Close),
                    ],
                ),
            ]);
        });
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

    let schedule_clock = ctrl.schedule_clock;
    let schedule_tick = set_interval(Duration::from_secs(30), move || {
        schedule_clock.set(chrono::Utc::now());
    });
    cleanup(move || schedule_tick.cancel());

    let c_content = ctrl.clone();
    let c_banners = ctrl.clone();
    let c_sidebar = ctrl.clone();

    view()
        .flex_1()
        .w_full()
        .h_full()
        .flex_row()
        .bg(move || Palette::current().bg)
        .text_color(move || Palette::current().fg)
        .text_size(px(S_BODY))
        .line_height(LH_BODY)
        .font_weight(W_BODY)
        .tabular_nums()
        .child(sidebar_view(c_sidebar))
        .child(
            view()
                .flex_1()
                .h_full()
                .flex_col()
                .bg(move || Palette::current().bg)
                // Top window drag bar above scrollable content
                .child(view().window_drag(true).w_full().h(px(PAGE_DRAG_HEIGHT)))
                // Top banners: controlled shutdown status + actionable manager errors
                .child(dynamic(move || {
                    let p = Palette::current();
                    let quitting = c_banners.quitting.get();
                    let snap = c_banners.snapshot.get();
                    let mut col = view().w_full().flex_col().gap(px(8.0));
                    let mut any = false;

                    if quitting {
                        any = true;
                        col = col.child(
                            view()
                                .test_id("quitting-banner")
                                .flex_row()
                                .items_center()
                                .gap(px(10.0))
                                .mx(px(PAGE_SIDE_PADDING))
                                .mb(px(4.0))
                                .px(px(14.0))
                                .py(px(10.0))
                                .rounded(px(10.0))
                                .bg(p.neu_bg)
                                .border_1()
                                .border_color(p.neu_bd)
                                .child(status_pill(IconKind::Stop, "Stopping…", Tone::Neutral))
                                .child(
                                    text("Waiting for active sync work to stop cleanly before quitting.")
                                        .flex_1()
                                        .text_size(px(S_COMPACT))
                                        .text_color(p.fg),
                                ),
                        );
                    }

                    if let Some(err) = &snap.action_error {
                        any = true;
                        let c_dismiss = c_banners.clone();
                        col = col.child(
                            view()
                                .test_id("action-error-banner")
                                .role(AccessibilityRole::Alert)
                                .flex_row()
                                .items_center()
                                .gap(px(10.0))
                                .mx(px(PAGE_SIDE_PADDING))
                                .mb(px(4.0))
                                .px(px(14.0))
                                .py(px(10.0))
                                .rounded(px(10.0))
                                .bg(p.err_bg)
                                .border_1()
                                .border_color(p.err_bd)
                                .child(status_pill(IconKind::Octagon, "Couldn't complete", Tone::Err))
                                .child(
                                    text(err.clone())
                                        .flex_1()
                                        .text_size(px(S_COMPACT))
                                        .text_color(p.fg),
                                )
                                .child(app_button(
                                    "Dismiss",
                                    BtnKind::Default,
                                    true,
                                    false,
                                    None,
                                    None,
                                    move || c_dismiss.send(UiCommand::DismissError),
                                )),
                        );
                    }

                    if any {
                        col.into_element()
                    } else {
                        view().into_element()
                    }
                }))
                .child(
                    scroll().flex_1().w_full().child(
                        view()
                            .w_full()
                            .px(px(PAGE_SIDE_PADDING))
                            .pt(px(PAGE_TOP_PADDING))
                            .pb(px(PAGE_BOTTOM_PADDING))
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
                                ScreenRoute::RunInspection { mapping_id, run_id } => {
                                    run_inspection_view(c_content.clone(), mapping_id, run_id)
                                        .into_element()
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
    use sync_core::{Execution, Intent, MappingId, Readiness, RunOutcome};

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
            .map(|m| m.presented_status(snap.active_run.as_ref(), snap.scheduling_paused))
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
        demo.dispatch(UiCommand::WizardSetLocalPath(
            "/Volumes/WorkSSD/Projects".to_owned(),
        ));
        let snap = demo.dispatch(UiCommand::WizardSetNoOtherSyncConfirmed(true));
        assert!(snap.wizard.can_start_first_sync());

        let blocked = demo.dispatch(UiCommand::WizardSetReviewScenario(
            ReviewScenario::BothPopulated,
        ));
        assert!(!blocked.wizard.can_start_first_sync());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_policy_tracks_open_and_hidden_windows() {
        use super::{NSApplicationActivationPolicy, activation_policy_for_window};

        for (visible, minimized, expected) in [
            (true, false, NSApplicationActivationPolicy::Regular),
            (false, false, NSApplicationActivationPolicy::Accessory),
            (true, false, NSApplicationActivationPolicy::Regular),
            (false, true, NSApplicationActivationPolicy::Regular),
        ] {
            assert_eq!(activation_policy_for_window(visible, minimized), expected);
        }
    }

    #[test]
    fn inaccessible_volume_attention_cause_ends_with_one_period() {
        let demo = DemoSource::new(DemoPreset::HouseholdOnly);
        let mut mapping = demo.snapshot().mappings.remove(0);
        let cause = "Can’t access the local folder because permission was denied";
        for issue in [cause.to_owned(), format!("{cause}.")] {
            mapping.volume_issue = Some(issue);
            assert_eq!(mapping.volume_attention().cause, format!("{cause}."));
        }
    }

    #[test]
    fn demo_commands_mutate_only_in_memory_state_and_preserve_conflict_history() {
        let demo = DemoSource::new(DemoPreset::HouseholdOnly);
        let s1 = demo.dispatch(UiCommand::PauseMapping(MappingId::new("documents")));
        let docs = s1.mapping(&MappingId::new("documents")).unwrap();
        assert_eq!(docs.state.intent, Intent::Paused);
        assert_eq!(docs.state.readiness(), Readiness::Ready);
        assert_eq!(docs.state.execution, Execution::Idle);

        // Pausing Family must NOT erase its unreviewed conflicts (#7).
        let s_pf = demo.dispatch(UiCommand::PauseMapping(MappingId::new("family")));
        assert_eq!(s_pf.attention_badge_count(), 5);
        assert!(
            s_pf.mapping(&MappingId::new("family"))
                .unwrap()
                .unreviewed_conflicts
                .is_some()
        );

        // Marking conflicts reviewed drops attention count to 4 while preserving RunOutcome::SuccessWithConflicts (#7).
        let s2 = demo.dispatch(UiCommand::MarkConflictsReviewed(MappingId::new("family")));
        assert_eq!(s2.attention_badge_count(), 4);
        let fam = s2.mapping(&MappingId::new("family")).unwrap();
        assert!(fam.unreviewed_conflicts.is_none());
        assert_eq!(
            fam.last_run.unwrap().outcome,
            RunOutcome::SuccessWithConflicts
        );

        let s3 = demo.dispatch(UiCommand::SetSchedulingPaused(true));
        assert!(s3.scheduling_paused);
        assert_eq!(s3.tray_status_line(), "Scheduling paused");
    }

    #[test]
    fn wizard_gating_blocks_invalid_steps_and_resets_consent_on_plan_change() {
        use crate::contract::WizardStep;
        let demo = DemoSource::new(DemoPreset::AllStates);

        // Fail location check (volume root) -> cannot jump past Location to Review and cannot Start.
        let s = demo.dispatch(UiCommand::WizardSetLocalPath("/Volumes/WorkSSD".to_owned()));
        assert!(!s.wizard.location_valid());
        assert!(!s.wizard.can_visit_step(WizardStep::Subfolders));
        assert!(!s.wizard.can_visit_step(WizardStep::Review));
        let jumped = demo.dispatch(UiCommand::WizardSetStep(WizardStep::Review));
        assert_ne!(jumped.wizard.step, WizardStep::Review);

        // Fail location check via nested overlap inside an existing mapping.
        let overlap = demo.dispatch(UiCommand::WizardSetLocalPath(
            "~/FolderSync/Documents/Nested".to_owned(),
        ));
        assert!(!overlap.wizard.location_valid());

        // Restore valid path, confirm consent, then verify changing recovery path resets consent.
        let valid = demo.dispatch(UiCommand::WizardSetLocalPath(
            "/Volumes/WorkSSD/Projects".to_owned(),
        ));
        assert!(valid.wizard.location_valid());
        assert!(valid.wizard.recovery_valid());
        let consented = demo.dispatch(UiCommand::WizardSetNoOtherSyncConfirmed(true));
        assert!(consented.wizard.can_start_first_sync());

        // Recovery path overlapping the local folder fails recovery checks and resets consent.
        let bad_rec = demo.dispatch(UiCommand::WizardSetLocalRecoveryPath(
            "/Volumes/WorkSSD/Projects/Recovery".to_owned(),
        ));
        assert!(!bad_rec.wizard.recovery_valid());
        assert!(!bad_rec.wizard.no_other_sync_confirmed);
        assert!(!bad_rec.wizard.can_start_first_sync());
        assert!(!bad_rec.wizard.can_visit_step(WizardStep::Review));
    }

    #[test]
    fn shortcuts_and_run_controls_target_displayed_mapping_only() {
        use crate::contract::{
            DemoActivityPhase, ScreenRoute, selected_stop_target, selected_sync_now_target,
        };
        let demo = DemoSource::new(DemoPreset::AllStates);
        let snap = demo.snapshot();

        // ⌘R must be disabled outside MappingDetail (Overview, Accounts, Activity, etc.).
        assert_eq!(
            selected_sync_now_target(&ScreenRoute::Overview, &snap),
            None
        );
        assert_eq!(
            selected_sync_now_target(&ScreenRoute::Accounts, &snap),
            None
        );
        assert_eq!(
            selected_sync_now_target(&ScreenRoute::Activity, &snap),
            None
        );

        // ⌘R targets the displayed MappingDetail only when Sync now is enabled.
        assert_eq!(
            selected_sync_now_target(
                &ScreenRoute::MappingDetail(MappingId::new("documents")),
                &snap
            ),
            Some(MappingId::new("documents"))
        );
        assert_eq!(
            selected_sync_now_target(
                &ScreenRoute::MappingDetail(MappingId::new("photos-archive")),
                &snap
            ),
            None
        );

        // Switching Activity progress preview preserves the displayed mapping's ID (`research`).
        let preview = demo.dispatch(UiCommand::DemoSetActivityPhase(
            DemoActivityPhase::TransferringKnown,
        ));
        let active = preview.active_run.as_ref().unwrap();
        assert_eq!(active.mapping_id, MappingId::new("research"));
        assert_eq!(active.mapping_name, "Research");
        assert_eq!(
            selected_stop_target(&ScreenRoute::Activity, &preview),
            Some(MappingId::new("research"))
        );
        assert_eq!(selected_stop_target(&ScreenRoute::Accounts, &preview), None);
    }

    #[test]
    fn launch_visibility_respects_default_hidden_on_login_launch() {
        use crate::contract::should_show_window_at_launch;

        let configured = DemoSource::new(DemoPreset::HouseholdOnly).snapshot();
        assert!(!should_show_window_at_launch(&configured, true));
        assert!(should_show_window_at_launch(&configured, false));

        let no_acct = DemoSource::new(DemoPreset::WelcomeNoAccount).snapshot();
        assert!(should_show_window_at_launch(&no_acct, true));

        let no_map = DemoSource::new(DemoPreset::WelcomeNoMappings).snapshot();
        assert!(should_show_window_at_launch(&no_map, true));
    }
}
