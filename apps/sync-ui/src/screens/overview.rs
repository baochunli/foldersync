//! Overview screen: mappings grouped by volume with free space, missing-drive
//! group styling, scheduling-paused banner, and live interactive actions (`ui-spec.md` §5.2).

use nsh_base::{table, table_body, table_cell, table_row};
use nsh_core::prelude::*;
use sync_core::{Execution, VolumeKind};

use crate::contract::{
    IconKind, MappingSnapshot, ScreenRoute, StatusLink, Tone, UiCommand, UiSnapshot, VolumeSnapshot,
};
use crate::screens::{AppController, welcome::welcome_view};
use crate::theme::{
    BtnKind, Palette, app_button, capacity_meter, focus_ring, icon, link_button, mono_path,
    page_title, progress_bar, status_dot, status_pill,
};

/// Renders Overview (or Welcome when no accounts or mappings exist).
pub fn overview_view(ctrl: AppController) -> impl IntoElement {
    dynamic(move || {
        let snap = ctrl.snapshot.get();
        if snap.accounts.is_empty() || snap.mappings.is_empty() {
            return welcome_view(ctrl.clone(), &snap).into_element();
        }
        render_overview(ctrl.clone(), &snap).into_element()
    })
}

fn render_overview(ctrl: AppController, snap: &UiSnapshot) -> View {
    let p = Palette::current();
    let real_mappings: Vec<&MappingSnapshot> = snap
        .mappings
        .iter()
        .filter(|m| !m.is_variant_showcase)
        .collect();
    let running_count = real_mappings
        .iter()
        .filter(|m| matches!(m.state.execution, Execution::Running | Execution::Stopping))
        .count();
    let attn_count = snap.attention_badge_count();
    let vol_count = snap
        .volumes
        .iter()
        .filter(|v| !snap.mappings_for_volume(&v.identity).is_empty())
        .count();

    let summary = format!(
        "{} mappings on {} volumes · {} running · {} need attention",
        real_mappings.len(),
        vol_count,
        running_count,
        attn_count
    );

    let ready_count = snap.ready_mapping_count();
    let ctrl_sync_ready = ctrl.clone();
    let ctrl_add = ctrl.clone();

    let mut root = view()
        .flex_col()
        .w_full()
        .gap(px(14.0))
        // Header row
        .child(
            view()
                .flex_row()
                .items_end()
                .justify_between()
                .gap(px(10.0))
                .flex_wrap()
                .child(
                    view()
                        .flex_col()
                        .gap(px(3.0))
                        .flex_1()
                        .child(page_title("Overview"))
                        .child(
                            text(summary)
                                .test_id("overview-summary")
                                .text_size(px(13.0))
                                .text_color(p.muted)
                                .tabular_nums(),
                        ),
                )
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .child(
                            app_button(
                                "Sync ready mappings",
                                BtnKind::Default,
                                false,
                                ready_count == 0,
                                Some(IconKind::Play),
                                (ready_count == 0).then_some("No mappings are ready and idle"),
                                move || ctrl_sync_ready.send(UiCommand::SyncReadyMappings),
                            )
                            .test_id("sync-ready-btn"),
                        )
                        .child(
                            app_button(
                                "Add mapping…",
                                BtnKind::Primary,
                                false,
                                false,
                                Some(IconKind::Plus),
                                None,
                                move || ctrl_add.open_wizard(),
                            )
                            .test_id("add-mapping-btn"),
                        ),
                ),
        );

    // Scheduling paused banner (§5.2 variant + D1)
    if snap.scheduling_paused {
        let ctrl_resume_sched = ctrl.clone();
        let active_note = snap
            .active_run
            .as_ref()
            .map(|r| format!(" {} finishes its current run.", r.mapping_name))
            .unwrap_or_default();
        root = root.child(
            view()
                .test_id("scheduling-paused-banner")
                .flex_row()
                .items_center()
                .gap(px(12.0))
                .bg(p.neu_bg)
                .border_1()
                .border_color(p.neu_bd)
                .rounded(px(10.0))
                .px(px(14.0))
                .py(px(10.0))
                .child(status_pill(IconKind::Pause, "Paused", Tone::Neutral))
                .child(
                    text(format!(
                        "Scheduling paused. No mapping starts on its own. Sync now still works.{active_note}"
                    ))
                    .flex_1()
                    .text_size(px(12.5))
                    .text_color(p.fg),
                )
                .child(app_button(
                    "Resume scheduling",
                    BtnKind::Default,
                    true,
                    false,
                    None,
                    None,
                    move || ctrl_resume_sched.send(UiCommand::SetSchedulingPaused(false)),
                )),
        );
    }

    // Volume groups: Internal SSD first, then external drives in stable order (§5.2)
    let mut ordered_vols: Vec<&VolumeSnapshot> = snap.volumes.iter().collect();
    ordered_vols.sort_by_key(|v| match v.identity.kind {
        VolumeKind::Internal => 0u8,
        VolumeKind::External => 1u8,
    });

    for vol in ordered_vols {
        let rows = snap.mappings_for_volume(&vol.identity);
        if rows.is_empty() {
            continue;
        }
        root = root.child(volume_group_card(ctrl.clone(), snap, vol, &rows));
    }

    // Additional §6 Readiness × Execution × Intent states (shown in AllStates demo preset)
    let variants = snap.variant_showcase_mappings();
    if !variants.is_empty() {
        root = root.child(state_variants_card(ctrl.clone(), snap, &variants));
    }

    root
}

fn volume_group_card(
    ctrl: AppController,
    snap: &UiSnapshot,
    vol: &VolumeSnapshot,
    rows: &[&MappingSnapshot],
) -> View {
    let p = Palette::current();
    let missing = !vol.connected;
    let kind_label = match vol.identity.kind {
        VolumeKind::Internal => "Internal",
        VolumeKind::External => "External",
    };
    let drive_icon = if missing {
        IconKind::DriveMissing
    } else if vol.identity.kind == VolumeKind::Internal {
        IconKind::DriveInternal
    } else {
        IconKind::DriveExternal
    };
    let count_word = if rows.len() == 1 {
        "1 mapping".to_owned()
    } else {
        format!("{} mappings", rows.len())
    };

    let header_bg = if missing { p.attn_bg } else { p.sunk };
    let icon_border = if missing { p.attn_bd } else { p.hair };
    let icon_tint = if missing { p.attn } else { p.muted };

    let vol_a11y = if missing {
        format!("{}, not connected", vol.display_name)
    } else if vol.low_space {
        format!(
            "{}, connected, {} free, low",
            vol.display_name, vol.free_label
        )
    } else {
        format!("{}, connected, {} free", vol.display_name, vol.free_label)
    };

    let cap_block = if missing {
        view().w(px(180.0)).items_end().child(
            text("Free space unknown")
                .text_size(px(11.5))
                .text_color(p.muted),
        )
    } else {
        let cap_text = if vol.low_space {
            format!("Low space: {} free of {}", vol.free_label, vol.total_label)
        } else {
            format!("{} free of {}", vol.free_label, vol.total_label)
        };
        let cap_color = if vol.low_space { p.attn } else { p.muted };
        view()
            .w(px(195.0))
            .flex_col()
            .items_end()
            .gap(px(4.0))
            .child(capacity_meter(
                vol.used_percent,
                vol.low_space,
                cap_text.clone(),
                180.0,
            ))
            .child(
                text(cap_text)
                    .text_size(px(11.5))
                    .text_color(cap_color)
                    .tabular_nums(),
            )
    };

    let meta_block = if missing {
        let last_seen = vol.last_seen_label.as_deref().unwrap_or("earlier");
        view()
            .flex_1()
            .flex_col()
            .gap(px(2.0))
            .child(
                view()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        text(vol.display_name.clone())
                            .text_size(px(14.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.fg),
                    )
                    .child(status_pill(
                        IconKind::DriveMissing,
                        "Not connected",
                        Tone::Attn,
                    )),
            )
            .child(
                text(format!(
                    "Last seen {last_seen} · connect it and sync continues on its own"
                ))
                .text_size(px(12.0))
                .text_color(p.attn),
            )
    } else {
        view()
            .flex_1()
            .flex_col()
            .gap(px(1.0))
            .child(
                text(vol.display_name.clone())
                    .text_size(px(14.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(p.fg),
            )
            .child(
                text(format!("{kind_label} · connected · {count_word}"))
                    .text_size(px(12.0))
                    .text_color(p.muted),
            )
    };

    let mut body = table_body().flex_col().w_full();
    for (idx, mapping) in rows.iter().enumerate() {
        body = body.child(mapping_row(ctrl.clone(), snap, mapping, idx > 0));
    }

    view()
        .bg(p.card)
        .border_1()
        .border_color(p.hair)
        .rounded(px(12.0))
        .overflow_hidden()
        .flex_col()
        .child(
            view()
                .flex_row()
                .items_center()
                .gap(px(12.0))
                .px(px(16.0))
                .py(px(11.0))
                .bg(header_bg)
                .border_b_1()
                .border_color(p.hair)
                .child(
                    view()
                        .w(px(30.0))
                        .h(px(30.0))
                        .rounded(px(8.0))
                        .bg(p.bg)
                        .border_1()
                        .border_color(icon_border)
                        .items_center()
                        .justify_center()
                        .flex_none()
                        .child(icon(drive_icon, 17.0, icon_tint)),
                )
                .child(meta_block)
                .child(cap_block),
        )
        .child(table().accessibility_label(vol_a11y).w_full().child(body))
}

fn state_variants_card(ctrl: AppController, snap: &UiSnapshot, rows: &[&MappingSnapshot]) -> View {
    let p = Palette::current();
    let mut body = table_body().flex_col().w_full();
    for (idx, mapping) in rows.iter().enumerate() {
        body = body.child(mapping_row(ctrl.clone(), snap, mapping, idx > 0));
    }

    view()
        .bg(p.card)
        .border_1()
        .border_color(p.hair)
        .rounded(px(12.0))
        .overflow_hidden()
        .flex_col()
        .child(
            view()
                .flex_row()
                .items_center()
                .justify_between()
                .px(px(16.0))
                .py(px(10.0))
                .bg(p.sunk)
                .border_b_1()
                .border_color(p.hair)
                .child(
                    view()
                        .flex_col()
                        .gap(px(1.0))
                        .child(
                            text("Additional mapping states (demo showcase)")
                                .text_size(px(13.0))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(p.fg),
                        )
                        .child(
                            text("Draft, Needs initialization, Not run yet, Queued, Running + paused, Stopping, and Retry pending")
                                .text_size(px(11.5))
                                .text_color(p.muted),
                        ),
                ),
        )
        .child(
            table()
                .accessibility_label("Additional mapping states")
                .w_full()
                .child(body),
        )
}

fn mapping_row(
    ctrl: AppController,
    snap: &UiSnapshot,
    m: &MappingSnapshot,
    top_border: bool,
) -> nsh_base::TableRowElement {
    let p = Palette::current();
    let pres = m.presented_status(snap.active_run.as_ref());
    let id_detail = m.id.clone();
    let id_row = m.id.clone();
    let ctrl_row = ctrl.clone();
    let ctrl_name = ctrl.clone();
    let is_draft = pres.is_draft;

    // Column 1: status dot + name link + cloud path + local path (both in Paper Mono)
    let path_tid = format!("path-{}", m.id);
    let col_identity = table_cell()
        .flex_1()
        .min_w(px(180.0))
        .flex_row()
        .items_center()
        .gap(px(10.0))
        .child(status_dot(pres.icon, pres.pill_tone))
        .child(
            view()
                .flex_col()
                .gap(px(1.0))
                .flex_1()
                .child(if is_draft {
                    text(m.display_name.clone())
                        .text_size(px(13.5))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(p.fg)
                        .into_element()
                } else {
                    link_button(m.display_name.clone(), 13.5, move || {
                        ctrl_name.navigate(ScreenRoute::MappingDetail(id_detail.clone()));
                    })
                    .test_id(&format!("open-detail-{}", m.id))
                    .into_element()
                })
                .child(mono_path(m.cloud_path.clone()).test_id(&path_tid))
                .child(mono_path(format!("↳ {}", m.local_path))),
        );

    // Column 2: status pill + status line + why/next line + optional thin indeterminate bar
    let mut col_status = table_cell()
        .flex_1()
        .min_w(px(200.0))
        .overflow_hidden()
        .flex_col()
        .gap(px(3.0));

    if let Some(pill_label) = &pres.pill_label {
        col_status = col_status.child(view().flex_row().child(status_pill(
            pres.icon,
            pill_label.clone(),
            pres.pill_tone,
        )));
    }
    col_status = col_status.child(
        text(pres.status_line.clone())
            .w_full()
            .text_size(px(13.0))
            .font_weight(FontWeight::MEDIUM)
            .text_color(p.fg)
            .tabular_nums(),
    );

    match (&pres.why_line, pres.secondary_link) {
        (Some(why), None) => {
            col_status = col_status.child(
                text(why.clone())
                    .w_full()
                    .text_size(px(12.0))
                    .text_color(p.muted),
            );
        }
        (why_opt, Some(link)) => {
            let ctrl_link = ctrl.clone();
            let id_rec = m.id.clone();
            let mut why_row = view()
                .w_full()
                .min_w(px(0.0))
                .flex_row()
                .flex_wrap()
                .items_center()
                .gap(px(4.0));
            if let Some(why) = why_opt {
                why_row = why_row.child(
                    text(format!("{why} ·"))
                        .text_size(px(12.0))
                        .text_color(p.muted),
                );
            }
            why_row = why_row.child(link_button(link.label(), 12.0, move || match link {
                StatusLink::StartRecovery => {
                    ctrl_link.navigate(ScreenRoute::Recovery(id_rec.clone()))
                }
                StatusLink::ShowConflicts | StatusLink::Reconnect | StatusLink::ReviewEndpoint => {
                    ctrl_link.navigate(ScreenRoute::Attention)
                }
            }));
            col_status = col_status.child(why_row);
        }
        (None, None) => {}
    }

    if m.state.execution == Execution::Running {
        col_status = col_status.child(view().max_w(px(220.0)).pt(px(2.0)).child(progress_bar(
            None,
            4.0,
            format!("Scanning {}", m.display_name),
        )));
    }

    // Column 3: last completed timestamp
    let col_last = table_cell()
        .w(px(120.0))
        .flex_none()
        .flex_col()
        .gap(px(1.0))
        .child(
            text(m.last_completed_label.clone())
                .text_size(px(12.5))
                .font_weight(FontWeight::MEDIUM)
                .text_color(p.fg)
                .tabular_nums(),
        )
        .child(if m.last_completed_label != "Never" {
            text("last completed")
                .text_size(px(11.5))
                .text_color(p.muted)
                .into_element()
        } else {
            view().into_element()
        });

    // Column 4: action buttons (§6 table)
    let mut col_actions = table_cell()
        .w(px(210.0))
        .flex_none()
        .flex_row()
        .items_center()
        .justify_end()
        .gap(px(6.0));

    if pres.is_draft {
        let ctrl_cont = ctrl.clone();
        let ctrl_disc = ctrl.clone();
        let id_disc = m.id.clone();
        col_actions = col_actions
            .child(app_button(
                "Continue setup…",
                BtnKind::Default,
                true,
                false,
                None,
                None,
                move || ctrl_cont.navigate(ScreenRoute::AddMapping),
            ))
            .child(app_button(
                "Discard draft",
                BtnKind::Default,
                true,
                false,
                None,
                None,
                move || ctrl_disc.send(UiCommand::DiscardDraft(id_disc.clone())),
            ));
    } else if pres.needs_init {
        let ctrl_init = ctrl.clone();
        col_actions = col_actions.child(app_button(
            "Finish setup…",
            BtnKind::Default,
            true,
            false,
            None,
            None,
            move || ctrl_init.navigate(ScreenRoute::AddMapping),
        ));
    } else if pres.show_stop {
        let ctrl_stop = ctrl.clone();
        let id_stop = m.id.clone();
        col_actions = col_actions.child(
            app_button(
                "Stop",
                BtnKind::Default,
                true,
                false,
                Some(IconKind::Stop),
                None,
                move || ctrl_stop.send(UiCommand::StopRun(id_stop.clone())),
            )
            .test_id(&format!("stop-{}", m.id)),
        );
    } else if pres.show_force_stop {
        let ctrl_fs = ctrl.clone();
        let id_fs = m.id.clone();
        let name_fs = m.display_name.clone();
        col_actions = col_actions.child(
            app_button(
                "Force stop…",
                BtnKind::Default,
                true,
                false,
                None,
                None,
                move || ctrl_fs.confirm_force_stop(id_fs.clone(), name_fs.clone()),
            )
            .test_id(&format!("force-stop-{}", m.id)),
        );
    } else {
        let ctrl_sync = ctrl.clone();
        let id_sync = m.id.clone();
        col_actions = col_actions.child(
            app_button(
                "Sync now",
                BtnKind::Default,
                true,
                !pres.sync_now_enabled,
                None,
                pres.sync_now_tooltip,
                move || ctrl_sync.send(UiCommand::SyncNow(id_sync.clone())),
            )
            .test_id(&format!("sync-now-{}", m.id)),
        );
    }

    if pres.show_pause {
        let ctrl_pause = ctrl.clone();
        let id_pause = m.id.clone();
        col_actions = col_actions.child(
            app_button(
                "Pause",
                BtnKind::Default,
                true,
                false,
                None,
                None,
                move || ctrl_pause.send(UiCommand::PauseMapping(id_pause.clone())),
            )
            .test_id(&format!("pause-{}", m.id)),
        );
    } else if pres.show_resume {
        let ctrl_resume = ctrl.clone();
        let id_resume = m.id.clone();
        col_actions = col_actions.child(
            app_button(
                "Resume",
                BtnKind::Default,
                true,
                false,
                None,
                None,
                move || ctrl_resume.send(UiCommand::ResumeMapping(id_resume.clone())),
            )
            .test_id(&format!("resume-{}", m.id)),
        );
    }

    let mut inner = view()
        .w_full()
        .flex_row()
        .items_center()
        .gap(px(12.0))
        .px(px(16.0))
        .py(px(11.0))
        .min_h(px(52.0))
        .hover(move |s| s.bg(p.sunk.opacity(0.65)))
        .focus_visible(focus_ring)
        .child(col_identity)
        .child(col_status)
        .child(col_last)
        .child(col_actions);

    if !is_draft {
        inner = inner.on_press(move || {
            ctrl_row.navigate(ScreenRoute::MappingDetail(id_row.clone()));
        });
    }

    let mut row = table_row().w_full().child(inner);
    if top_border {
        row = row.border_t_1().border_color(p.hair);
    }
    row
}
