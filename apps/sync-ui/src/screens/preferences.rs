//! Preferences screen with General and Diagnostics tabs (`ui-spec.md` §5.8).
//!
//! Raw engine flags, fingerprints, and redacted options live ONLY in Diagnostics.

use nsh_base::{SelectItem, SelectValue, input, radio, radio_group, select, switch, tab, tabs};
use nsh_core::{file_dialog::SaveDialog, prelude::*};

use crate::contract::{FinderExtensionStatus, IconKind, Tone, UiCommand, UiSnapshot};
use crate::screens::AppController;
use crate::theme::{
    BtnKind, Palette, app_button, card, card_title, focus_ring, mono_text, page_title,
    section_label, status_pill,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrefsTab {
    General,
    Diagnostics,
}

/// Renders Preferences (General & Diagnostics tabs).
pub fn preferences_view(ctrl: AppController) -> impl IntoElement {
    let active_tab = signal(PrefsTab::General);
    let bw_input = signal("20".to_owned());
    let selected_diag_mapping = signal("research".to_owned());
    let copied_opts = signal(false);
    let export_notice = signal(Option::<String>::None);

    dynamic(move || {
        let snap = ctrl.snapshot.get();
        let p = Palette::current();
        let current_tab = active_tab.get();

        let tab_bar = tabs()
            .accessibility_label("Preferences")
            .flex_row()
            .gap(px(4.0))
            .border_b_1()
            .border_color(p.hair)
            .child(
                tab()
                    .test_id("prefs-tab-general")
                    .selected(current_tab == PrefsTab::General)
                    .px(px(12.0))
                    .py(px(7.0))
                    .border_b(px(2.0))
                    .border_b_color(if current_tab == PrefsTab::General {
                        p.primary
                    } else {
                        AdaptiveColor::TRANSPARENT
                    })
                    .cursor_pointer()
                    .focus_visible(focus_ring)
                    .on_press(move || {
                        active_tab.set(PrefsTab::General);
                    })
                    .child(
                        text("General")
                            .text_size(px(13.0))
                            .font_weight(if current_tab == PrefsTab::General {
                                FontWeight::SEMIBOLD
                            } else {
                                FontWeight::NORMAL
                            })
                            .text_color(if current_tab == PrefsTab::General {
                                p.primary
                            } else {
                                p.muted
                            }),
                    ),
            )
            .child(
                tab()
                    .test_id("prefs-tab-diagnostics")
                    .selected(current_tab == PrefsTab::Diagnostics)
                    .px(px(12.0))
                    .py(px(7.0))
                    .border_b(px(2.0))
                    .border_b_color(if current_tab == PrefsTab::Diagnostics {
                        p.primary
                    } else {
                        AdaptiveColor::TRANSPARENT
                    })
                    .cursor_pointer()
                    .focus_visible(focus_ring)
                    .on_press(move || {
                        active_tab.set(PrefsTab::Diagnostics);
                    })
                    .child(
                        text("Diagnostics")
                            .text_size(px(13.0))
                            .font_weight(if current_tab == PrefsTab::Diagnostics {
                                FontWeight::SEMIBOLD
                            } else {
                                FontWeight::NORMAL
                            })
                            .text_color(if current_tab == PrefsTab::Diagnostics {
                                p.primary
                            } else {
                                p.muted
                            }),
                    ),
            );

        let content = match current_tab {
            PrefsTab::General => render_general(ctrl.clone(), &snap, bw_input),
            PrefsTab::Diagnostics => render_diagnostics(
                ctrl.clone(),
                &snap,
                selected_diag_mapping,
                copied_opts,
                export_notice,
            ),
        };

        view()
            .flex_col()
            .w_full()
            .gap(px(14.0))
            .child(page_title("Preferences"))
            .child(tab_bar)
            .child(content)
            .into_element()
    })
}

fn render_general(ctrl: AppController, snap: &UiSnapshot, bw_input: Signal<String>) -> View {
    let p = Palette::current();
    let prefs = &snap.preferences;
    let c_login = ctrl.clone();
    let c_notif = ctrl.clone();
    let c_ext = ctrl.clone();
    let c_bw_none = ctrl.clone();
    let c_bw_lim = ctrl.clone();
    let c_bw_edit = ctrl.clone();

    let (ext_pill_icon, ext_pill_label, ext_pill_tone) = match &prefs.finder_extension {
        FinderExtensionStatus::Enabled { .. } => (IconKind::Check, "Extension on", Tone::Ok),
        FinderExtensionStatus::Disabled => (IconKind::Dash, "Extension off", Tone::Neutral),
        FinderExtensionStatus::Unavailable => (IconKind::Dash, "Not installed", Tone::Neutral),
    };

    let general_card = card()
        .gap(px(14.0))
        .child(
            view()
                .flex_col()
                .gap(px(4.0))
                .child(app_switch(
                    prefs.open_at_login,
                    "Open at login",
                    "FolderSync starts in the menu bar when you log in (demo toggle — does not change macOS Login Items).",
                    move |next| {
                        c_login.send(UiCommand::SetOpenAtLogin(next));
                    },
                ))
                .child(if prefs.open_at_login_pending {
                    text("Waiting for approval in System Settings → Login Items.")
                        .pl(px(44.0))
                        .text_size(px(12.0))
                        .text_color(p.attn)
                        .into_element()
                } else {
                    view().into_element()
                }),
        )
        .child(app_switch(
            prefs.notifications_enabled,
            "Notifications",
            "Notify when a mapping needs attention or completes with conflicts (works in a signed app bundle).",
            move |next| c_notif.send(UiCommand::SetNotifications(next)),
        ))
        .child(
            view()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(12.0))
                .pt(px(4.0))
                .border_t_1()
                .border_color(p.hair)
                .child(
                    view()
                        .flex_col()
                        .gap(px(3.0))
                        .flex_1()
                        .child(
                            view()
                                .flex_row()
                                .items_center()
                                .gap(px(8.0))
                                .child(
                                    text("Finder status")
                                        .text_size(px(13.0))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(p.fg),
                                )
                                .child(status_pill(ext_pill_icon, ext_pill_label, ext_pill_tone)),
                        )
                        .child(
                            text("Badges and menu items on mapped folders. macOS controls whether the extension is on.")
                                .text_size(px(12.0))
                                .text_color(p.muted),
                        ),
                )
                .child(app_button(
                    "Open Extension Settings…",
                    BtnKind::Default,
                    true,
                    false,
                    None,
                    None,
                    move || {
                        c_ext.send(UiCommand::DemoAction(
                            "Simulated Open Extension Settings (demo mode)".to_owned(),
                        ));
                    },
                )),
        );

    let limited = prefs.bandwidth_limit_mb_s.is_some();
    let bandwidth_card = card().child(card_title("Bandwidth")).child(
        radio_group()
            .accessibility_label("Bandwidth")
            .flex_col()
            .gap(px(10.0))
            .child(
                radio()
                    .checked(!limited)
                    .flex_row()
                    .items_center()
                    .gap(px(10.0))
                    .cursor_pointer()
                    .focus_visible(focus_ring)
                    .on_change(move |checked: &bool| {
                        if *checked {
                            c_bw_none.send(UiCommand::SetBandwidthLimit(None));
                        }
                    })
                    .child(radio_dot(!limited))
                    .child(text("No limit").text_size(px(13.0)).text_color(p.fg)),
            )
            .child(
                view()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        radio()
                            .checked(limited)
                            .flex_row()
                            .items_center()
                            .gap(px(10.0))
                            .cursor_pointer()
                            .focus_visible(focus_ring)
                            .on_change(move |checked: &bool| {
                                if *checked {
                                    let val = bw_input
                                        .get_untracked()
                                        .parse::<u32>()
                                        .unwrap_or(20)
                                        .max(1);
                                    c_bw_lim.send(UiCommand::SetBandwidthLimit(Some(val)));
                                }
                            })
                            .child(radio_dot(limited))
                            .child(text("Limit to").text_size(px(13.0)).text_color(p.fg)),
                    )
                    .child(
                        input()
                            .w(px(64.0))
                            .min_h(px(28.0))
                            .px(px(8.0))
                            .py(px(3.0))
                            .rounded(px(7.0))
                            .bg(p.bg)
                            .border_1()
                            .border_color(p.border)
                            .focus(focus_ring)
                            .focus_visible(focus_ring)
                            .text_size(px(12.5))
                            .text_color(p.fg)
                            .label("Megabytes per second")
                            .value(move || bw_input.get())
                            .on_change(move |s: &str| {
                                bw_input.set(s.to_owned());
                                if let Ok(v) = s.trim().parse::<u32>()
                                    && v > 0
                                {
                                    c_bw_edit.send(UiCommand::SetBandwidthLimit(Some(v)));
                                }
                            }),
                    )
                    .child(text("MB/s").text_size(px(13.0)).text_color(p.fg)),
            ),
    );

    view()
        .flex_col()
        .w_full()
        .gap(px(14.0))
        .child(general_card)
        .child(bandwidth_card)
}

fn render_diagnostics(
    ctrl: AppController,
    snap: &UiSnapshot,
    selected_diag_mapping: Signal<String>,
    copied_opts: Signal<bool>,
    export_notice: Signal<Option<String>>,
) -> View {
    let p = Palette::current();
    let prefs = &snap.preferences;

    let ext_version_line = match &prefs.finder_extension {
        FinderExtensionStatus::Enabled { last_status_label } => {
            format!("0.1.0 · enabled · last status {last_status_label}")
        }
        FinderExtensionStatus::Disabled => "0.1.0 · disabled".to_owned(),
        FinderExtensionStatus::Unavailable => "Not installed".to_owned(),
    };

    let versions_card = card()
        .flex_1()
        .child(card_title("Versions"))
        .child(diag_kv("FolderSync", &prefs.app_version))
        .child(diag_kv(
            "Sync engine",
            &format!("{}\n{}", prefs.engine_version, prefs.engine_checksum),
        ))
        .child(diag_kv("Interface", &prefs.nsh_version))
        .child(diag_kv("Finder extension", &ext_version_line))
        .child(diag_kv("System", &prefs.system_summary));

    let c_exp = ctrl.clone();
    let mut export_card = card()
        .flex_1()
        .child(card_title("Export diagnostics"))
        .child(
            text("Prepares versions, configuration without secrets, selected run logs and error details with no file contents or sign-in data. Demo mode simulates export without writing a file.")
                .text_size(px(12.5))
                .text_color(p.fg),
        )
        .child(
            view().flex_row().pt(px(4.0)).child(app_button(
                "Export diagnostics…",
                BtnKind::Default,
                false,
                false,
                None,
                None,
                move || {
                    let c = c_exp.clone();
                    let answer = pick_save_path(
                        SaveDialog::new()
                            .title("Export FolderSync diagnostics (simulated)")
                            .file_name("foldersync-diagnostics.txt"),
                    );
                    spawn(async move {
                        if let Ok(Some(path)) = answer.await {
                            let msg = format!(
                                "Simulated diagnostics export to {} (demo mode — no file written).",
                                path.display()
                            );
                            export_notice.set(Some(msg.clone()));
                            c.send(UiCommand::DemoAction(msg));
                        }
                    });
                },
            )),
        );
    if let Some(notice) = export_notice.get() {
        export_card = export_card.child(text(notice).text_size(px(12.0)).text_color(p.muted));
    }

    let real_mappings: Vec<_> = snap
        .mappings
        .iter()
        .filter(|m| !m.is_variant_showcase)
        .collect();
    let sel_id = selected_diag_mapping.get();
    let current_m = real_mappings
        .iter()
        .find(|m| m.id.as_str() == sel_id)
        .or_else(|| real_mappings.first())
        .copied();

    let mut mapping_card = card().child(card_title("Mapping configuration"));

    if let Some(m) = current_m {
        let items: Vec<SelectItem> = real_mappings
            .iter()
            .map(|item| SelectItem::new(item.id.as_str().to_owned(), item.display_name.clone()))
            .collect();
        let trigger_label = m.display_name.clone();
        let opts_text = m.engine_options.join("\n");
        let opts_copy = opts_text.clone();
        let is_copied = copied_opts.get();

        mapping_card = mapping_card
            .child(
                view()
                    .flex_row()
                    .items_center()
                    .gap(px(10.0))
                    .child(text("Mapping").text_size(px(12.5)).text_color(p.muted))
                    .child(
                        select()
                            .accessibility_label("Diagnostic mapping")
                            .items(items)
                            .value(SelectValue::from(m.id.as_str().to_owned()))
                            .trigger(
                                view()
                                    .flex_row()
                                    .items_center()
                                    .justify_between()
                                    .gap(px(8.0))
                                    .min_w(px(160.0))
                                    .min_h(px(28.0))
                                    .px(px(10.0))
                                    .py(px(4.0))
                                    .rounded(px(7.0))
                                    .bg(p.bg)
                                    .border_1()
                                    .border_color(p.border)
                                    .cursor_pointer()
                                    .focus_visible(focus_ring)
                                    .child(text(trigger_label).text_size(px(12.5)).text_color(p.fg))
                                    .child(text("▾").text_size(px(11.0)).text_color(p.muted)),
                            )
                            .popup_style(move |s| {
                                s.bg(p.card)
                                    .border_1()
                                    .border_color(p.border)
                                    .rounded(px(8.0))
                                    .p(px(4.0))
                            })
                            .on_change(move |val: &Option<SharedString>| {
                                if let Some(val) = val {
                                    selected_diag_mapping.set(val.to_string());
                                    copied_opts.set(false);
                                }
                            }),
                    ),
            )
            .child(diag_kv("Generation", &m.generation.to_string()))
            .child(diag_kv("Fingerprint", &m.fingerprint))
            .child(diag_kv("Local volume ID", &m.volume_id.stable_id))
            .child(diag_kv("Cloud folder ID", &m.cloud_folder_id))
            .child(
                view()
                    .pt(px(6.0))
                    .child(section_label("Engine options (this generation)")),
            )
            .child(
                view()
                    .test_id("diagnostics-engine-options")
                    .selectable(true)
                    .bg(p.sunk)
                    .border_1()
                    .border_color(p.hair)
                    .rounded(px(8.0))
                    .p(px(11.0))
                    .child(mono_text(opts_text, 11.0, p.fg)),
            )
            .child(view().flex_row().pt(px(4.0)).child(app_button(
                if is_copied { "Copied" } else { "Copy" },
                BtnKind::Default,
                true,
                false,
                None,
                None,
                move || {
                    set_clipboard(opts_copy.clone());
                    copied_opts.set(true);
                },
            )));
    } else {
        mapping_card = mapping_card.child(
            text("No mappings configured yet.")
                .text_size(px(12.5))
                .text_color(p.muted),
        );
    }

    view()
        .flex_col()
        .w_full()
        .gap(px(14.0))
        .child(
            view()
                .flex_row()
                .items_stretch()
                .gap(px(14.0))
                .w_full()
                .child(versions_card)
                .child(export_card),
        )
        .child(mapping_card)
}

fn app_switch(
    checked: bool,
    title: &str,
    subtitle: &str,
    on_change: impl Fn(bool) + 'static,
) -> nsh_base::SwitchElement {
    let p = Palette::current();
    switch()
        .checked(checked)
        .flex_row()
        .items_start()
        .gap(px(12.0))
        .cursor_pointer()
        .focus_visible(focus_ring)
        .track_style(move |s| {
            s.w(px(34.0))
                .h(px(20.0))
                .rounded_full()
                .p(px(2.0))
                .flex_row()
                .items_center()
                .justify_start()
                .bg(if checked { p.primary } else { p.border })
        })
        .thumb_style(move |s| {
            s.w(px(16.0))
                .h(px(16.0))
                .rounded_full()
                .bg(AdaptiveColor::rgb(0xffffff, 0xffffff))
                .ml(px(if checked { 14.0 } else { 0.0 }))
        })
        .on_change(move |next: &bool| {
            on_change(*next);
        })
        .child(
            view()
                .flex_col()
                .gap(px(2.0))
                .flex_1()
                .child(
                    text(title.to_owned())
                        .text_size(px(13.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(p.fg),
                )
                .child(
                    text(subtitle.to_owned())
                        .text_size(px(12.0))
                        .text_color(p.muted),
                ),
        )
}

fn radio_dot(checked: bool) -> View {
    let p = Palette::current();
    view()
        .w(px(16.0))
        .h(px(16.0))
        .rounded_full()
        .border_1()
        .border_color(if checked { p.primary } else { p.border })
        .bg(p.bg)
        .items_center()
        .justify_center()
        .child(if checked {
            view()
                .w(px(8.0))
                .h(px(8.0))
                .rounded_full()
                .bg(p.primary)
                .into_element()
        } else {
            view().into_element()
        })
}

fn diag_kv(label: &str, value: &str) -> View {
    let p = Palette::current();
    view()
        .flex_row()
        .items_start()
        .gap(px(12.0))
        .py(px(2.0))
        .child(
            text(label.to_owned())
                .w(px(130.0))
                .flex_none()
                .text_size(px(12.5))
                .text_color(p.muted),
        )
        .child(
            view()
                .flex_1()
                .child(mono_text(value.to_owned(), 11.5, p.fg)),
        )
}
