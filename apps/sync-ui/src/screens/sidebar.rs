//! Left sidebar workspace built with `nsh_base::tree()` (one keyboard Tab stop,
//! arrow-key navigation per `ui-spec.md` §3 & §8), plus footer and demo switcher.

use nsh_base::{SelectValue, TreeEntry, TreeItem, tree};
use nsh_core::prelude::*;

use crate::contract::{DemoPreset, IconKind, ScreenRoute, UiCommand};
use crate::screens::AppController;
use crate::theme::{
    BADGE_LINE_HEIGHT, BtnKind, NEUTRAL_HOVER, Palette, S_CAPTION, S_CARD_TITLE, S_TINY,
    SIDEBAR_WIDTH, W_BODY, W_STRONG, W_TITLE, app_button, brand_glyph, focus_ring, icon,
};

/// Renders the 212 pt sidebar with window-drag title-bar insets, `Tree` sections,
/// live status footer, and a compact demo preset switcher.
pub fn sidebar_view(ctrl: AppController) -> impl IntoElement {
    let p = Palette::current();
    let ctrl_select = ctrl.clone();
    let ctrl_render = ctrl.clone();
    let ctrl_foot = ctrl.clone();
    let ctrl_demo = ctrl.clone();

    let items = vec![
        TreeItem::new("overview", "Overview"),
        TreeItem::new("activity", "Activity"),
        TreeItem::new("attention", "Attention"),
        TreeItem::new("accounts", "Accounts"),
        TreeItem::new("preferences", "Preferences"),
    ];

    let selected_id = move || SelectValue::from(ctrl.route.get().sidebar_id());

    let render_item = move |entry: &TreeEntry| {
        let p = Palette::current();
        let entry_id = entry.id.to_string();
        let entry_label = entry.label.to_string();
        let ctrl_row = ctrl_render.clone();

        let icon_kind = match entry_id.as_str() {
            "overview" => IconKind::Grid,
            "activity" => IconKind::Pulse,
            "attention" => IconKind::Alert,
            "accounts" => IconKind::User,
            _ => IconKind::Gear,
        };

        let is_selected = {
            let id = entry_id.clone();
            let route = ctrl_row.route;
            move || route.get().sidebar_id() == id
        };
        let bg_selected = is_selected.clone();
        let icon_selected = is_selected.clone();

        let attn_count = {
            let snap_sig = ctrl_row.snapshot;
            move || snap_sig.get().attention_badge_count()
        };

        let test_id_str = format!("nav-{entry_id}");
        let ctrl_click = ctrl_row.clone();
        let target_route = match entry_id.as_str() {
            "overview" => ScreenRoute::Overview,
            "activity" => ScreenRoute::Activity,
            "attention" => ScreenRoute::Attention,
            "accounts" => ScreenRoute::Accounts,
            "preferences" => ScreenRoute::Preferences,
            _ => ScreenRoute::Overview,
        };
        view()
            .test_id(&test_id_str)
            .w_full()
            .flex_row()
            .items_center()
            .gap(px(9.0))
            .px(px(9.0))
            .py(px(6.0))
            .rounded(px(7.0))
            .cursor_pointer()
            .on_press(move || {
                ctrl_click.navigate(target_route.clone());
            })
            .bg(move || {
                if bg_selected() {
                    p.sidebar_sel
                } else {
                    AdaptiveColor::TRANSPARENT
                }
            })
            .hover(|s| s.bg(NEUTRAL_HOVER.opacity(0.08)))
            .focus_visible(focus_ring)
            .child(icon(icon_kind, 16.0, move || {
                if icon_selected() { p.primary } else { p.muted }
            }))
            .child(
                text(entry_label)
                    .flex_1()
                    .font_weight(W_BODY)
                    .style_when(is_selected, |s| s.font_weight(W_STRONG))
                    .text_color(p.fg),
            )
            .child(dynamic(move || {
                if entry_id == "attention" {
                    let count = attn_count();
                    if count > 0 {
                        return view()
                            .test_id("sidebar-attention-count")
                            .bg(p.attn_bg)
                            .border_1()
                            .border_color(p.attn_bd)
                            .rounded(px(10.0))
                            .px(px(7.0))
                            .label(format!("{count} items need attention"))
                            .child(
                                text(count.to_string())
                                    .text_size(px(S_TINY))
                                    .line_height(px(BADGE_LINE_HEIGHT))
                                    .font_weight(W_TITLE)
                                    .text_color(p.attn)
                                    .tabular_nums(),
                            )
                            .into_element();
                    }
                }
                view().into_element()
            }))
            .into_element()
    };

    view()
        .w(px(SIDEBAR_WIDTH))
        .flex_none()
        .h_full()
        .bg(p.sidebar)
        .border_r_1()
        .border_color(p.hair)
        .flex_col()
        .px(px(10.0))
        .pb(px(12.0))
        .pt(move || {
            let inset = nsh_core::element::desktop::title_bar().top.0;
            px(inset.max(14.0))
        })
        // Window drag strip in the top area of the sidebar
        .child(
            view()
                .window_drag(true)
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .px(px(8.0))
                .pb(px(12.0))
                .pt(px(4.0))
                .child(brand_glyph(22.0))
                .child(
                    text("FolderSync")
                        .text_size(px(S_CARD_TITLE))
                        .font_weight(W_TITLE)
                        .text_color(p.fg),
                ),
        )
        // Navigation tree (single Tab stop, arrow-key navigation)
        .child({
            let ctrl_sel = ctrl_select.clone();
            let ctrl_act = ctrl_select;
            let nav_for = |id: &SharedString| match id.as_str() {
                "overview" => ScreenRoute::Overview,
                "activity" => ScreenRoute::Activity,
                "attention" => ScreenRoute::Attention,
                "accounts" => ScreenRoute::Accounts,
                "preferences" => ScreenRoute::Preferences,
                _ => ScreenRoute::Overview,
            };
            tree()
                .flex_1()
                .accessibility_label("Sections")
                .items(items)
                .selected(selected_id)
                .list_style(|s| s.gap(px(1.0)))
                .render_item(render_item)
                .on_selected_change(move |id: &SharedString| {
                    ctrl_sel.navigate(nav_for(id));
                })
                .on_action(move |id: &SharedString| {
                    ctrl_act.navigate(nav_for(id));
                })
        })
        // Compact demo preset switcher (shown ONLY in explicit demo mode)
        .child(dynamic(move || {
            if ctrl_demo.snapshot.get().is_demo_mode {
                demo_switcher_box(ctrl_demo.clone()).into_element()
            } else {
                view().into_element()
            }
        }))
        // Sidebar footer: scheduling status & running mapping
        .child(dynamic(move || {
            let snap = ctrl_foot.snapshot.get();
            let p = Palette::current();
            let sched_text = if snap.scheduling_paused {
                "Scheduling paused"
            } else {
                "Scheduling on"
            };
            let sched_icon = if snap.scheduling_paused {
                IconKind::Pause
            } else {
                IconKind::Clock
            };
            let running_text = snap
                .active_run
                .as_ref()
                .map(|r| format!("Running: {}", r.mapping_name));

            let mut foot = view()
                .border_t_1()
                .border_color(p.hair)
                .pt(px(10.0))
                .px(px(9.0))
                .pb(px(2.0))
                .mt(px(8.0))
                .flex_col()
                .gap(px(3.0))
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .child(icon(sched_icon, 13.0, p.muted))
                        .child(
                            text(sched_text)
                                .text_size(px(S_CAPTION))
                                .text_color(p.muted),
                        ),
                );
            if let Some(rt) = running_text {
                foot = foot.child(
                    view()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .child(icon(IconKind::Sync, 13.0, p.run))
                        .child(text(rt).text_size(px(S_CAPTION)).text_color(p.muted)),
                );
            }
            foot.into_element()
        }))
}

fn demo_switcher_box(ctrl: AppController) -> impl IntoElement {
    dynamic(move || {
        let p = Palette::current();
        let snap = ctrl.snapshot.get();
        let current = snap.demo_preset;

        let preset_btn = |label: &'static str, preset: DemoPreset, tid: &'static str| {
            let c = ctrl.clone();
            let active = current == preset;
            app_button(
                label,
                if active {
                    BtnKind::Primary
                } else {
                    BtnKind::Default
                },
                true,
                false,
                None,
                None,
                move || {
                    c.send(UiCommand::DemoSetPreset(preset));
                    c.navigate(ScreenRoute::Overview);
                },
            )
            .test_id(tid)
        };

        let c_sched = ctrl.clone();
        let paused = snap.scheduling_paused;

        view()
            .border_t_1()
            .border_color(p.hair)
            .pt(px(8.0))
            .mt(px(6.0))
            .flex_col()
            .gap(px(5.0))
            .child(
                text("Demo states")
                    .text_size(px(S_TINY))
                    .font_weight(W_TITLE)
                    .text_color(p.muted)
                    .px(px(4.0)),
            )
            .child(
                view()
                    .flex_row()
                    .flex_wrap()
                    .gap(px(4.0))
                    .child(preset_btn("All states", DemoPreset::AllStates, "demo-all"))
                    .child(preset_btn(
                        "Household",
                        DemoPreset::HouseholdOnly,
                        "demo-household",
                    ))
                    .child(preset_btn(
                        "No account",
                        DemoPreset::WelcomeNoAccount,
                        "demo-welcome-empty",
                    ))
                    .child(preset_btn(
                        "No folders",
                        DemoPreset::WelcomeNoMappings,
                        "demo-welcome-connected",
                    )),
            )
            .child(
                app_button(
                    if paused {
                        "Resume scheduling"
                    } else {
                        "Pause scheduling"
                    },
                    BtnKind::Default,
                    true,
                    false,
                    Some(if paused {
                        IconKind::Play
                    } else {
                        IconKind::Pause
                    }),
                    None,
                    move || c_sched.send(UiCommand::SetSchedulingPaused(!paused)),
                )
                .test_id("demo-toggle-scheduling"),
            )
            .into_element()
    })
}
