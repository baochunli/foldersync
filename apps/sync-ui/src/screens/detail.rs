//! Mapping detail screen (`ui-spec.md` §5.3).

use nsh_base::{
    SelectItem, SelectValue, select, table, table_body, table_cell, table_head, table_header,
    table_row,
};
use nsh_core::{
    LineHeight,
    element::desktop::{MenuItem, popup_menu},
    prelude::*,
};
use sync_core::{DEFAULT_MAX_DELETE_PERCENT, MappingId, Readiness};

use crate::contract::{
    IconKind, MappingSnapshot, ScreenRoute, StatusLink, StatusModifier, UiCommand, UiSnapshot,
};
use crate::screens::AppController;
use crate::theme::{
    BtnKind, CARD_ACTION_MARGIN, HEADER_BOTTOM_MARGIN, HEADER_GAP, KV_ROW_GAP, Palette, S_CAPTION,
    S_COMPACT, S_SMALL, S_TINY, app_button, card, card_title, focus_ring, icon, kv_row,
    link_button, mono_text, page_title, section_label, status_pill,
};

/// Renders the Mapping detail view for `mapping_id`.
pub fn detail_view(ctrl: AppController, mapping_id: MappingId) -> impl IntoElement {
    dynamic(move || {
        let snap = ctrl.snapshot.get();
        let Some(m) = snap.mapping(&mapping_id).cloned() else {
            return view()
                .flex_col()
                .gap(px(8.0))
                .child(link_button("‹ Overview", S_SMALL, {
                    let c = ctrl.clone();
                    move || c.navigate(ScreenRoute::Overview)
                }))
                .child(page_title("Mapping not found"))
                .into_element();
        };
        render_detail(ctrl.clone(), &snap, &m).into_element()
    })
}

fn render_detail(ctrl: AppController, snap: &UiSnapshot, m: &MappingSnapshot) -> View {
    let p = Palette::current();
    let pres = m.presented_status(snap.active_run.as_ref(), snap.scheduling_paused);

    let ctrl_back = ctrl.clone();
    let ctrl_sync = ctrl.clone();
    let ctrl_pause = ctrl.clone();
    let ctrl_more = ctrl.clone();
    let ctrl_step = ctrl.clone();
    let id_sync = m.id.clone();
    let id_pause = m.id.clone();
    let id_more = m.id.clone();
    let id_step = m.id.clone();
    let name_more = m.display_name.clone();
    let local_more = m.local_path.clone();

    let mut status_row = view().flex_row().items_center().gap(px(6.0)).flex_wrap();
    if let Some(pill_label) = &pres.pill_label {
        status_row = status_row.child(status_pill(pres.icon, pill_label.clone(), pres.pill_tone));
    }
    status_row = status_row.child(text(pres.status_line.clone()).text_color(p.fg));
    if let Some(link) = pres.secondary_link {
        let ctrl_l = ctrl.clone();
        let id_rec = m.id.clone();
        status_row = status_row.child(link_button(link.label(), S_COMPACT, move || match link {
            StatusLink::StartRecovery => ctrl_l.open_recovery(id_rec.clone()),
            StatusLink::ShowConflicts | StatusLink::Reconnect | StatusLink::ReviewEndpoint => {
                ctrl_l.navigate(ScreenRoute::Attention)
            }
        }));
    }

    let name_force = m.display_name.clone();
    let primary_action = if pres.show_stop {
        app_button(
            "Stop",
            BtnKind::Default,
            false,
            false,
            Some(IconKind::Stop),
            None,
            move || ctrl_sync.send(UiCommand::StopRun(id_sync.clone())),
        )
        .test_id("detail-stop-btn")
    } else if pres.show_force_stop {
        app_button(
            "Force stop…",
            BtnKind::Default,
            false,
            false,
            Some(IconKind::Stop),
            None,
            move || ctrl_sync.confirm_force_stop(id_sync.clone(), name_force.clone()),
        )
        .test_id("detail-force-stop-btn")
    } else {
        app_button(
            "Sync now",
            BtnKind::Default,
            false,
            !pres.sync_now_enabled,
            Some(IconKind::Play),
            pres.sync_now_tooltip,
            move || ctrl_sync.send(UiCommand::SyncNow(id_sync.clone())),
        )
        .test_id("detail-sync-now")
    };

    let pause_resume_action = if pres.show_resume {
        app_button(
            "Resume",
            BtnKind::Default,
            false,
            false,
            Some(IconKind::Play),
            None,
            move || ctrl_pause.send(UiCommand::ResumeMapping(id_pause.clone())),
        )
    } else {
        app_button(
            "Pause",
            BtnKind::Default,
            false,
            false,
            Some(IconKind::Pause),
            None,
            move || ctrl_pause.send(UiCommand::PauseMapping(id_pause.clone())),
        )
    };

    let cloud_more = m.cloud_path.clone();
    let has_cloud_url = cloud_more.starts_with("https://");
    let more_btn = app_button(
        "More ▾",
        BtnKind::Default,
        false,
        false,
        None,
        None,
        move || {
            let c_rev = ctrl_more.clone();
            let c_open = ctrl_more.clone();
            let c_rm = ctrl_more.clone();
            let id_rm = id_more.clone();
            let nm_rm = name_more.clone();
            let lp = local_more.clone();
            let cp = cloud_more.clone();
            drop(popup_menu(vec![
                MenuItem::new("Reveal in Finder").on_select(move || {
                    c_rev.send(UiCommand::RevealPath(lp.clone()));
                }),
                MenuItem::new("Open cloud folder")
                    .disabled(!has_cloud_url)
                    .on_select(move || {
                        if cp.starts_with("https://") {
                            c_open.send(UiCommand::OpenCloudUrl(cp.clone()));
                        }
                    }),
                MenuItem::separator(),
                MenuItem::new("Remove mapping…").on_select(move || {
                    c_rm.confirm_remove_mapping(id_rm.clone(), nm_rm.clone());
                }),
            ]));
        },
    )
    .test_id("detail-more-btn");

    // Locations card — derive cloud ID and volume identity status from snapshot (never hardcoded).
    let lp_reveal = m.local_path.clone();
    let cp_open = m.cloud_path.clone();
    let c_loc_rev = ctrl.clone();
    let c_loc_open = ctrl.clone();

    let endpoint_change = if m.state.readiness() == Readiness::EndpointChanged {
        match &m.modifier {
            StatusModifier::EndpointChange {
                is_cloud,
                expected_id,
                found_id,
            } => Some((*is_cloud, expected_id.clone(), found_id.clone())),
            _ => Some((true, m.cloud_folder_id.clone(), "Unknown".to_owned())),
        }
    } else {
        None
    };

    let vol_snap = snap.volumes.iter().find(|v| v.identity == m.volume_id);
    let vol_fs = vol_snap.map(|v| v.filesystem.as_str()).unwrap_or("Unknown");
    let local_changed = matches!(endpoint_change, Some((false, _, _)));
    let vol_status_note = if let Some(issue) = &m.volume_issue {
        issue.as_str()
    } else if vol_snap.is_some_and(|v| !v.connected)
        || m.state.readiness() == Readiness::VolumeUnavailable
    {
        "not connected"
    } else if local_changed {
        "local folder changed since setup"
    } else {
        "identity matches setup"
    };

    let locations_card = card().flex_1().child(
        view()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .child(icon(IconKind::Swap, 16.0, p.muted))
            .child(card_title("Locations")),
    );
    let mut locations = view().flex_col().gap(px(KV_ROW_GAP)).child(kv_row(
        "Cloud folder",
        mono_text(m.cloud_path.clone(), S_CAPTION, p.fg).into_element(),
    ));

    if let Some((true, expected_id, found_id)) = &endpoint_change {
        locations = locations
            .child(kv_row(
                "At setup",
                mono_text(expected_id.clone(), S_CAPTION, p.fg).into_element(),
            ))
            .child(kv_row(
                "Found now",
                view()
                    .flex_row()
                    .items_center()
                    .flex_wrap()
                    .gap(px(6.0))
                    .child(mono_text(found_id.clone(), S_CAPTION, p.attn))
                    .child(
                        text("· changed since setup")
                            .text_size(px(S_SMALL))
                            .text_color(p.attn),
                    )
                    .into_element(),
            ));
    } else {
        locations = locations.child(kv_row(
            "Cloud folder ID",
            view()
                .flex_row()
                .items_center()
                .flex_wrap()
                .gap(px(6.0))
                .child(mono_text(m.cloud_folder_id.clone(), S_CAPTION, p.fg))
                .child(
                    text("· matches setup")
                        .text_size(px(S_SMALL))
                        .text_color(p.muted),
                )
                .into_element(),
        ));
    }

    locations = locations
        .child(kv_row(
            "Account",
            text(format!("{} · {}", m.account_id, m.account_email))
                .text_color(p.fg)
                .into_element(),
        ))
        .child(kv_row(
            "Local folder",
            mono_text(m.local_path.clone(), S_CAPTION, p.fg).into_element(),
        ));

    if let Some((false, expected_id, found_id)) = &endpoint_change {
        locations = locations
            .child(kv_row(
                "At setup",
                mono_text(expected_id.clone(), S_CAPTION, p.fg).into_element(),
            ))
            .child(kv_row(
                "Found now",
                view()
                    .flex_row()
                    .items_center()
                    .flex_wrap()
                    .gap(px(6.0))
                    .child(mono_text(found_id.clone(), S_CAPTION, p.attn))
                    .child(
                        text("· changed since setup")
                            .text_size(px(S_SMALL))
                            .text_color(p.attn),
                    )
                    .into_element(),
            ));
    }

    locations = locations.child(kv_row(
        "Volume",
        text(format!("{} · {vol_fs} · {vol_status_note}", m.volume_name))
            .text_color(p.fg)
            .into_element(),
    ));
    if let Some(vol) = vol_snap {
        let space = if vol.total_gb > 0 {
            format!("{} free of {}", vol.free_label, vol.total_label)
        } else {
            "Free space unknown".to_owned()
        };
        locations = locations.child(kv_row(
            "Capacity",
            text(space).text_color(p.fg).into_element(),
        ));
    }
    let locations_card = locations_card.child(locations).child(
        view()
            .flex_row()
            .flex_wrap()
            .gap(px(8.0))
            .mt(px(CARD_ACTION_MARGIN))
            .child(app_button(
                "Reveal in Finder",
                BtnKind::Default,
                true,
                false,
                None,
                None,
                move || {
                    c_loc_rev.send(UiCommand::RevealPath(lp_reveal.clone()));
                },
            ))
            .child(app_button(
                "Open cloud folder",
                BtnKind::Default,
                true,
                !cp_open.starts_with("https://"),
                None,
                (!cp_open.starts_with("https://"))
                    .then_some("Cloud web link is not available for this account"),
                move || {
                    if cp_open.starts_with("https://") {
                        c_loc_open.send(UiCommand::OpenCloudUrl(cp_open.clone()));
                    }
                },
            )),
    );

    let mut subfolders_card = card()
        .flex_1()
        .child(
            view()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .child(icon(IconKind::Folder, 16.0, p.muted))
                .child(card_title("Included subfolders")),
        )
        .child(
            text(if snap.is_demo_mode {
                format!(
                    "{} of {} subfolders · about {} of {}",
                    m.included_subfolders,
                    m.total_subfolders,
                    m.included_size_label,
                    m.total_size_label
                )
            } else {
                "Review the selection to check individual folder sizes.".to_owned()
            })
            .text_color(p.fg)
            .tabular_nums(),
        );
    if !m.excluded_subfolders.is_empty() {
        let excluded_list = m
            .excluded_subfolders
            .iter()
            .map(|(path, sz)| format!("{path} ({sz})"))
            .collect::<Vec<_>>()
            .join(", ");
        subfolders_card = subfolders_card.child(
            view()
                .flex_row()
                .flex_wrap()
                .gap(px(4.0))
                .child(
                    text("Not synced:")
                        .text_size(px(S_SMALL))
                        .text_color(p.muted),
                )
                .child(mono_text(excluded_list, S_CAPTION, p.muted)),
        );
    } else {
        subfolders_card = subfolders_card.child(
            text("All subfolders in this folder are included.")
                .text_size(px(S_SMALL))
                .text_color(p.muted),
        );
    }

    let can_change = m.generation > 0
        && m.state.readiness() == Readiness::Ready
        && matches!(
            m.state.execution,
            sync_core::Execution::Idle | sync_core::Execution::Queued
        );
    let snap_is_demo = snap.is_demo_mode;
    let c_change = ctrl.clone();
    let change_id = m.id.clone();
    subfolders_card = subfolders_card.child(
        view().items_start().child(
            app_button(
                "Change subfolders…",
                BtnKind::Default,
                true,
                !can_change,
                None,
                (!can_change).then_some("Available after first sync when this mapping is ready"),
                move || {
                    c_change.send(UiCommand::ChangeSubfolders(change_id.clone()));
                    if snap_is_demo {
                        c_change.navigate(ScreenRoute::AddMapping);
                    }
                },
            )
            .test_id("change-subfolders-btn"),
        ),
    );

    // Recent runs table card
    let mut runs_body = table_body().flex_col().w_full();
    let mut runs = m.recent_runs.iter().peekable();
    while let Some(run) = runs.next() {
        let ctrl_show = ctrl.clone();
        let mid_show = m.id.clone();
        let rid_show = run.run_id.clone();
        let btn_tid = format!("show-run-{}", run.run_id);
        let mut row =
            table_row()
                .w_full()
                .items_center()
                .gap(px(0.0))
                .child(
                    table_cell().px(px(10.0)).py(px(8.0)).w(px(110.0)).child(
                        text(run.started_label.clone())
                            .text_color(p.fg)
                            .tabular_nums(),
                    ),
                )
                .child(
                    table_cell().px(px(10.0)).py(px(8.0)).w(px(72.0)).child(
                        text(run.duration_label.clone())
                            .text_color(p.fg)
                            .tabular_nums(),
                    ),
                )
                .child(table_cell().px(px(10.0)).py(px(8.0)).flex_1().child(
                    view().flex_row().child(status_pill(
                        run.result_icon,
                        run.result_label.clone(),
                        run.result_tone,
                    )),
                ))
                .child(
                    table_cell().px(px(10.0)).py(px(8.0)).flex_1().child(
                        text(run.changes_label.clone())
                            .text_color(p.muted)
                            .tabular_nums(),
                    ),
                )
                .child(
                    table_cell()
                        .px(px(10.0))
                        .py(px(8.0))
                        .w(px(90.0))
                        .items_end()
                        .child(
                            app_button(
                                "Show run",
                                BtnKind::Default,
                                true,
                                false,
                                None,
                                None,
                                move || {
                                    ctrl_show
                                        .open_run_inspection(mid_show.clone(), rid_show.clone())
                                },
                            )
                            .test_id(&btn_tid),
                        ),
                );
        if runs.peek().is_some() {
            row = row.border_b_1().border_color(p.hair);
        }
        runs_body = runs_body.child(row);
    }

    let recent_runs_card = card()
        .child(
            view()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .child(icon(IconKind::Pulse, 16.0, p.muted))
                .child(card_title("Recent runs")),
        )
        .child(
            table()
                .accessibility_label("Recent runs")
                .w_full()
                .child(
                    table_header().child(
                        table_row()
                            .w_full()
                            .border_b_1()
                            .border_color(p.hair)
                            .child(
                                table_head()
                                    .px(px(10.0))
                                    .py(px(6.0))
                                    .w(px(110.0))
                                    .child(section_label("Started")),
                            )
                            .child(
                                table_head()
                                    .px(px(10.0))
                                    .py(px(6.0))
                                    .w(px(72.0))
                                    .child(section_label("Duration")),
                            )
                            .child(
                                table_head()
                                    .px(px(10.0))
                                    .py(px(6.0))
                                    .flex_1()
                                    .child(section_label("Result")),
                            )
                            .child(
                                table_head()
                                    .px(px(10.0))
                                    .py(px(6.0))
                                    .flex_1()
                                    .child(section_label("Changes")),
                            )
                            .child(table_head().px(px(10.0)).py(px(6.0)).w(px(90.0))),
                    ),
                )
                .child(runs_body),
        );

    // Schedule & Protection card
    let ctrl_sched = ctrl.clone();
    let id_sched = m.id.clone();
    let current_sched_val = m.schedule_minutes.to_string();
    let sched_label = format!("Every {} minutes", m.schedule_minutes);
    let sched_select = select()
        .accessibility_label("Schedule interval")
        .items(vec![
            SelectItem::new("5", "Every 5 minutes"),
            SelectItem::new("15", "Every 15 minutes"),
            SelectItem::new("30", "Every 30 minutes"),
            SelectItem::new("60", "Every 60 minutes"),
        ])
        .value(SelectValue::from(current_sched_val))
        .trigger(
            view()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(8.0))
                .min_w(px(150.0))
                .min_h(px(28.0))
                .px(px(9.0))
                .py(px(3.0))
                .line_height(LineHeight::Normal)
                .rounded(px(7.0))
                .bg(p.bg)
                .border_1()
                .border_color(p.border)
                .cursor_pointer()
                .focus_visible(focus_ring)
                .child(text(sched_label).text_color(p.fg))
                .child(text("▾").text_size(px(S_TINY)).text_color(p.muted)),
        )
        .popup_style(move |s| {
            s.bg(p.card)
                .border_1()
                .border_color(p.border)
                .rounded(px(8.0))
                .p(px(4.0))
        })
        .on_change(move |val: &Option<SharedString>| {
            if let Some(val) = val
                && let Ok(mins) = val.parse::<u32>()
            {
                ctrl_sched.send(UiCommand::SetScheduleInterval {
                    mapping_id: id_sched.clone(),
                    minutes: mins,
                });
            }
        });

    let scheduling = if m.generation == 0 {
        "Starts after first sync"
    } else if m.state.intent == sync_core::Intent::Paused {
        "Mapping paused"
    } else if snap.scheduling_paused {
        "Scheduling paused"
    } else if !m.scheduling_enabled || m.state.readiness() != Readiness::Ready {
        "Automatic sync on hold"
    } else {
        "Automatic sync on"
    };
    let next_check = if scheduling != "Automatic sync on" {
        text(scheduling).text_color(p.fg).into_element()
    } else if let Some(check) = m.next_check {
        let clock = ctrl.schedule_clock;
        dynamic(move || {
            text(check.label(clock.get(), false))
                .text_color(p.fg)
                .into_element()
        })
        .into_element()
    } else {
        text(m.next_check_label.clone())
            .text_color(p.fg)
            .into_element()
    };
    let schedule_card = card()
        .flex_1()
        .child(card_title("Schedule"))
        .child(view().flex_col().gap(px(KV_ROW_GAP))
        .child(kv_row("Check schedule", sched_select.into_element()))
        .child(kv_row(
            "Automatic sync",
            text(scheduling)
                .text_color(p.fg)
                .into_element(),
        ))
        .child(kv_row("Next check", next_check))
        .child(kv_row("Last run", text(m.last_completed_label.clone()).text_color(p.fg).into_element()))
        )
        .child(
            text("Local changes can prompt an earlier run. Changes in the cloud are found when a run checks them.")
                .text_size(px(S_SMALL))
                .text_color(p.muted),
        )
        .child(view().pt(px(6.0)).child(section_label("Protection")))
        .child(view().flex_col().gap(px(KV_ROW_GAP))
        .child(kv_row(
            "Deletion limit",
            text(format!(
                "{DEFAULT_MAX_DELETE_PERCENT}% of tracked files. A run that would delete more stops for review."
            ))
            .text_color(p.fg)
            .into_element(),
        ))
        .child(kv_row(
            "Conflicts",
            text("Keep both versions")
                .text_color(p.fg)
                .into_element(),
        ))
        .child(kv_row(
            "Recovery copies",
            text("Keep recovery copies (never pruned automatically)")
                .text_color(p.fg)
                .into_element(),
        )));

    // Recovery locations & Supported files card (full plan §18 policy)
    let rec_local = m.local_recovery_path.clone();
    let rec_cloud = m.cloud_recovery_path.clone();
    let c_rec_rev = ctrl.clone();
    let c_rec_open = ctrl.clone();
    let symlink_line = if let Some(sym) = m.found_symlinks.first() {
        format!(
            "Symbolic links are not followed. {} found and excluded: {sym}",
            m.found_symlinks.len()
        )
    } else {
        "Symbolic links are not followed.".to_owned()
    };
    let exclusions_summary = if m.custom_exclusions.is_empty() {
        "macOS metadata and temporary files".to_owned()
    } else {
        format!(
            "macOS metadata and temporary files, plus {}",
            m.custom_exclusions.join(", ")
        )
    };

    let recovery_card = card()
        .flex_1()
        .child(card_title("Recovery locations"))
        .child(view().flex_col().gap(px(KV_ROW_GAP))
        .child(kv_row(
            "On this Mac",
            view()
                .flex_col()
                .gap(px(2.0))
                .child(mono_text(m.local_recovery_path.clone(), S_CAPTION, p.fg))
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .child(
                            text(m.local_recovery_size_label.clone())
                                .text_size(px(S_SMALL))
                                .text_color(p.muted),
                        )
                        .child(text("·").text_size(px(S_SMALL)).text_color(p.muted))
                        .child(link_button("Reveal", S_SMALL, move || {
                            c_rec_rev.send(UiCommand::RevealPath(rec_local.clone()));
                        })),
                )
                .into_element(),
        ))
        .child(kv_row(
            "In the cloud",
            view()
                .flex_col()
                .gap(px(2.0))
                .child(mono_text(m.cloud_recovery_path.clone(), S_CAPTION, p.fg))
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .child(
                            text("Size not measured")
                                .text_size(px(S_SMALL))
                                .text_color(p.muted),
                        )
                        .child(text("·").text_size(px(S_SMALL)).text_color(p.muted))
                        .child(link_button("Open", S_SMALL, move || {
                            c_rec_open.send(UiCommand::OpenCloudUrl(rec_cloud.clone()));
                        })),
                )
                .into_element(),
        ))
        )
        .child(
            text(
                "Earlier versions kept when a run overwrote or removed a file. Not a full backup.",
            )
            .text_size(px(S_SMALL))
            .text_color(p.muted),
        )
        .child(view().pt(px(6.0)).child(section_label("Supported files")))
        .child(policy_bullet(
            "Ordinary documents, source files, images, videos and archives are synced.",
        ))
        .child(policy_bullet(&symlink_line))
        .child(policy_bullet(
            "Hard-link relationships are not preserved across syncs.",
        ))
        .child(policy_bullet(
            "Google Drive shortcuts are skipped; their targets are not synced.",
        ))
        .child(policy_bullet(
            "Google Docs, Sheets and Slides stay outside the managed file scope.",
        ))
        .child(policy_bullet("Empty folders are not copied yet."))
        .child(policy_bullet(
            "Finder tags, resource forks, ACLs, permissions and extended attributes are not preserved in v1.",
        ))
        .child(policy_bullet(
            "Exclude active databases, virtual machine images and application libraries.",
        ))
        .child(policy_bullet(
            "Folders managed by another sync provider (such as iCloud, Dropbox or Drive for Desktop) are rejected as local roots.",
        ))
        .child(policy_bullet(
            "Duplicate names or case/Unicode clashes stop the run for review.",
        ))
        .child(
            text(format!("Exclusions: {exclusions_summary}"))
                .text_size(px(S_SMALL))
                .text_color(p.muted)
                .pt(px(4.0)),
        );

    view()
        .flex_col()
        .w_full()
        .gap(px(14.0))
        .child(
            view()
                .flex_col()
                .items_start()
                .child(
                    link_button("‹ Overview", S_SMALL, move || {
                        ctrl_back.navigate(ScreenRoute::Overview)
                    })
                    .mb(px(8.0)),
                )
                .child(
                    view()
                        .w_full()
                        .window_drag(true)
                        .mb(px(HEADER_BOTTOM_MARGIN))
                        .flex_row()
                        .items_end()
                        .justify_between()
                        .gap(px(HEADER_GAP))
                        .flex_wrap()
                        .child(
                            view()
                                .flex_col()
                                .gap(px(6.0))
                                .flex_1()
                                .child(page_title(m.display_name.clone()))
                                .child(status_row),
                        )
                        .child({
                            let mut bar = view()
                                .flex_row()
                                .items_center()
                                .gap(px(8.0))
                                .child(primary_action)
                                .child(pause_resume_action)
                                .child(more_btn);
                            if snap.is_demo_mode {
                                bar = bar.child(
                                    app_button(
                                        "Step state (demo)",
                                        BtnKind::Ghost,
                                        true,
                                        false,
                                        None,
                                        Some("Cycle this mapping through demo states"),
                                        move || {
                                            ctrl_step.send(UiCommand::DemoStepMappingState(
                                                id_step.clone(),
                                            ))
                                        },
                                    )
                                    .test_id("detail-step-state-btn"),
                                );
                            }
                            bar
                        }),
                ),
        )
        .child(
            view()
                .flex_row()
                .gap(px(14.0))
                .w_full()
                .child(locations_card)
                .child(subfolders_card),
        )
        .child(recent_runs_card)
        .child(
            view()
                .flex_row()
                .gap(px(14.0))
                .w_full()
                .child(schedule_card)
                .child(recovery_card),
        )
}

fn policy_bullet(line: &str) -> View {
    let p = Palette::current();
    view()
        .flex_row()
        .items_start()
        .gap(px(6.0))
        .child(text("•").text_color(p.muted))
        .child(text(line.to_owned()).flex_1().text_color(p.fg))
}
