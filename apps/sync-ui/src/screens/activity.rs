//! Activity screen: current run phase (indeterminate scanning + elapsed time,
//! bytes only when known), queue, held summary, bounded recent events (`virtual_list`),
//! and expandable raw log (`collapsible`, closed by default) (`ui-spec.md` §5.5).

use nsh_base::collapsible;
use nsh_core::{file_dialog::OpenDialog, prelude::*};
use sync_core::{MappingId, RunId};

use crate::contract::{
    ActiveRunSnapshot, DemoActivityPhase, IconKind, RunPhase, ScreenRoute, Tone, UiCommand,
    format_duration, format_mm_ss,
};
use crate::screens::AppController;
use crate::theme::{
    BtnKind, Palette, app_button, card, card_title, focus_ring, link_button, mono_text, page_title,
    progress_bar, status_pill,
};

/// Renders the Activity screen.
pub fn activity_view(ctrl: AppController) -> impl IntoElement {
    let raw_log_open = signal(false);
    let copied_notice = signal(false);

    dynamic(move || {
        let snap = ctrl.snapshot.get();
        let p = Palette::current();

        let current_card = match &snap.active_run {
            Some(run) => render_active_run_card(ctrl.clone(), run),
            None => card().child(card_title("Current run")).child(
                text("No sync is running right now.")
                    .text_size(px(13.0))
                    .text_color(p.muted),
            ),
        };

        // Demo progress-phase preview bar so every §5.5 progress variant can be verified live
        let phase_bar = {
            let make_phase_btn = |label: &'static str, ph: DemoActivityPhase, tid: &'static str| {
                let c = ctrl.clone();
                app_button(
                    label,
                    BtnKind::Default,
                    true,
                    false,
                    None,
                    None,
                    move || {
                        c.send(UiCommand::DemoSetActivityPhase(ph));
                    },
                )
                .test_id(tid)
            };
            view()
                .flex_row()
                .flex_wrap()
                .items_center()
                .gap(px(6.0))
                .child(
                    text("Demo progress phase:")
                        .text_size(px(11.5))
                        .text_color(p.muted),
                )
                .child(make_phase_btn(
                    "Scanning",
                    DemoActivityPhase::Scanning,
                    "activity-phase-scanning",
                ))
                .child(make_phase_btn(
                    "Transfer (known total)",
                    DemoActivityPhase::TransferringKnown,
                    "activity-phase-known",
                ))
                .child(make_phase_btn(
                    "Transfer (unknown total)",
                    DemoActivityPhase::TransferringUnknown,
                    "activity-phase-unknown",
                ))
                .child(make_phase_btn(
                    "Stopping",
                    DemoActivityPhase::Stopping,
                    "activity-phase-stopping",
                ))
        };

        // Queue and Held summary cards — reflect actual `queue_order` (#6).
        let mut queued_names: Vec<String> = Vec::new();
        for qid in &snap.queue_order {
            if let Some(m) = snap.mapping(qid)
                && !m.is_variant_showcase
                && m.state.execution == sync_core::Execution::Queued
            {
                queued_names.push(m.display_name.clone());
            }
        }
        for m in &snap.mappings {
            if !m.is_variant_showcase
                && m.state.execution == sync_core::Execution::Queued
                && !snap.queue_order.contains(&m.id)
            {
                queued_names.push(m.display_name.clone());
            }
        }
        let queue_body = if !queued_names.is_empty() {
            format!("Queued (in order): {}", queued_names.join(" → "))
        } else if snap.scheduling_paused {
            "Nothing waiting. Scheduling is paused — Sync now still works.".to_owned()
        } else if snap.is_demo_mode {
            "Nothing waiting. Next scheduled check: Documents at about 14:48.".to_owned()
        } else {
            "Nothing waiting right now.".to_owned()
        };

        // Count only mappings actually held from running separately from review-only conflicts (§12).
        let held_count = snap
            .mappings
            .iter()
            .filter(|m| {
                !m.is_variant_showcase
                    && matches!(
                        m.state.readiness(),
                        sync_core::Readiness::AuthenticationRequired
                            | sync_core::Readiness::RecoveryRequired
                            | sync_core::Readiness::EndpointChanged
                            | sync_core::Readiness::VolumeUnavailable
                    )
            })
            .count();
        let conflict_count = snap
            .mappings
            .iter()
            .filter(|m| {
                !m.is_variant_showcase
                    && m.state.readiness() == sync_core::Readiness::Ready
                    && (m.unreviewed_conflicts.is_some()
                        || matches!(
                            m.modifier,
                            crate::contract::StatusModifier::UnreviewedConflicts { .. }
                        ))
            })
            .count();
        let held_copy = match (held_count, conflict_count) {
            (0, 0) => "No mappings are held.".to_owned(),
            (1, 0) => "1 mapping is held and won't run until resolved.".to_owned(),
            (h, 0) => format!("{h} mappings are held and won't run until resolved."),
            (0, 1) => {
                "No mappings are held. 1 mapping has unreviewed conflicts (Sync now still works)."
                    .to_owned()
            }
            (0, c) => format!(
                "No mappings are held. {c} mappings have unreviewed conflicts (Sync now still works)."
            ),
            (h, c) => format!(
                "{h} mappings are held and won't run until resolved; {c} more has unreviewed conflicts (Sync now still works)."
            ),
        };
        let ctrl_attn = ctrl.clone();
        let queue_held = view()
            .flex_row()
            .gap(px(14.0))
            .w_full()
            .child(
                card()
                    .flex_1()
                    .child(card_title("Queue"))
                    .child(text(queue_body).text_size(px(12.5)).text_color(p.muted)),
            )
            .child(
                card()
                    .flex_1()
                    .min_w(px(0.0))
                    .child(card_title("Held"))
                    .child(
                        view()
                            .flex_col()
                            .items_start()
                            .w_full()
                            .gap(px(6.0))
                            .child(
                                text(held_copy)
                                    .w_full()
                                    .text_size(px(12.5))
                                    .text_color(p.fg)
                                    .tabular_nums(),
                            )
                            .child(link_button("Attention", 12.5, move || {
                                ctrl_attn.navigate(ScreenRoute::Attention);
                            })),
                    ),
            );

        // Recent events bounded list (`virtual_list` per §5.5)
        let events_data = snap.recent_events.clone();
        let events_len = events_data.len();
        let ctrl_ev = ctrl.clone();
        let events_list = virtual_list(events_len, move |pos| {
            let ev = &events_data[pos.index()];
            let p = Palette::current();
            let c_link = ctrl_ev.clone();
            let mut row = view()
                .flex_row()
                .items_center()
                .gap(px(12.0))
                .py(px(7.0))
                .border_t_1()
                .border_color(p.hair)
                .child(
                    text(ev.time_label.clone())
                        .w(px(56.0))
                        .flex_none()
                        .text_size(px(12.0))
                        .text_color(p.muted)
                        .tabular_nums(),
                )
                .child(
                    text(ev.message.clone())
                        .flex_1()
                        .text_size(px(12.5))
                        .text_color(p.fg),
                );
            if let Some((label, dest)) = ev.link.clone() {
                row = row.child(link_button(label, 12.0, move || match dest.clone() {
                    ScreenRoute::RunInspection { mapping_id, run_id } => {
                        c_link.open_run_inspection(mapping_id, run_id);
                    }
                    ScreenRoute::Recovery(mapping_id) => {
                        c_link.open_recovery(mapping_id);
                    }
                    other => {
                        c_link.navigate(other);
                    }
                }));
            }
            row
        })
        .viewport(ListViewport::Parent)
        .estimated_item_size(px(32.0));

        let recent_card = card()
            .child(card_title("Recent events"))
            .child(events_list)
            .child(
                text("The last 200 events are kept here. Older events stay in each run's record.")
                    .text_size(px(12.0))
                    .text_color(p.muted),
            );

        // Expandable Raw log (`collapsible`, closed by default per §5.5)
        let log_title = match &snap.active_run {
            Some(r) => format!("Raw log · {}, current run", r.mapping_name),
            None => "Raw log · latest run".to_owned(),
        };
        let open_now = raw_log_open.get();
        let log_text = if snap.raw_log.is_empty() {
            "No log lines recorded in this demo session.".to_owned()
        } else {
            snap.raw_log.join("\n")
        };
        let log_for_copy = log_text.clone();
        let is_copied = copied_notice.get();

        let raw_log_card = card().child(
            collapsible()
                .open(open_now)
                .reveal(true)
                .content(
                    view()
                        .flex_col()
                        .gap(px(8.0))
                        .pt(px(6.0))
                        .child(
                            view()
                                .test_id("activity-raw-log-body")
                                .selectable(true)
                                .bg(p.sunk)
                                .border_1()
                                .border_color(p.hair)
                                .rounded(px(8.0))
                                .p(px(11.0))
                                .child(mono_text(log_text, 11.0, p.fg)),
                        )
                        .child(
                            view()
                                .flex_row()
                                .items_center()
                                .gap(px(8.0))
                                .child(app_button(
                                    if is_copied { "Copied" } else { "Copy" },
                                    BtnKind::Default,
                                    true,
                                    false,
                                    None,
                                    None,
                                    move || {
                                        set_clipboard(log_for_copy.clone());
                                        copied_notice.set(true);
                                    },
                                ))
                                .child(app_button(
                                    "Reveal full log in Finder",
                                    BtnKind::Default,
                                    true,
                                    true,
                                    None,
                                    Some("No local log file in demo mode"),
                                    || {},
                                )),
                        ),
                )
                .child(
                    view()
                        .test_id("activity-raw-log-toggle")
                        .role(AccessibilityRole::Button)
                        .accessibility_state(AccessibilityState {
                            expanded: Some(open_now),
                            ..AccessibilityState::default()
                        })
                        .cursor_pointer()
                        .focus_visible(focus_ring)
                        .on_press(move || {
                            raw_log_open.update(|o| *o = !*o);
                            copied_notice.set(false);
                        })
                        .child(
                            text(format!("{} {log_title}", if open_now { "▾" } else { "▸" }))
                                .text_size(px(14.0))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(p.primary),
                        ),
                ),
        );

        let mut root = view()
            .flex_col()
            .w_full()
            .gap(px(14.0))
            .child(page_title("Activity"));
        if snap.is_demo_mode {
            root = root.child(phase_bar);
        }
        root.child(current_card)
            .child(queue_held)
            .child(recent_card)
            .child(raw_log_card)
            .into_element()
    })
}

/// Run-specific log and recovery copies inspection screen (§12 steps 1–4, uit3-review1 #9).
///
/// Never restores recovery copies directly over a live synced folder; copies a selected
/// file to a separate inspection folder chosen by the user.
pub fn run_inspection_view(
    ctrl: AppController,
    mapping_id: MappingId,
    run_id: RunId,
) -> impl IntoElement {
    let copied_log = signal(false);

    dynamic(move || {
        let snap = ctrl.snapshot.get();
        let p = Palette::current();
        let inspected = snap
            .inspected_run
            .as_ref()
            .filter(|r| r.run_id == run_id && r.mapping_id == mapping_id);

        let c_back_detail = ctrl.clone();
        let c_back_act = ctrl.clone();
        let mid_back = mapping_id.clone();

        let nav_bar = view()
            .flex_row()
            .items_center()
            .gap(px(12.0))
            .child(link_button("‹ Mapping", 12.5, move || {
                c_back_detail.navigate(ScreenRoute::MappingDetail(mid_back.clone()));
            }))
            .child(text("·").text_size(px(12.0)).text_color(p.muted))
            .child(link_button("Activity", 12.5, move || {
                c_back_act.navigate(ScreenRoute::Activity);
            }));

        let Some(insp) = inspected else {
            return view()
                .flex_col()
                .w_full()
                .gap(px(14.0))
                .child(nav_bar)
                .child(page_title("Loading run record…"))
                .child(
                    card().child(
                        text(format!("Reading log and recovery copies for run {run_id}."))
                            .text_size(px(13.0))
                            .text_color(p.muted),
                    ),
                )
                .into_element();
        };

        let log_text = if insp.log_lines.is_empty() {
            "No log lines were recorded for this run.".to_owned()
        } else {
            insp.log_lines.join("\n")
        };
        let log_copy = log_text.clone();
        let is_copied = copied_log.get();

        let log_path_opt = insp.log_exists.then(|| insp.log_path.clone()).flatten();
        let c_reveal = ctrl.clone();
        let reveal_btn = match log_path_opt {
            Some(lp) => app_button(
                "Reveal full log in Finder",
                BtnKind::Default,
                true,
                false,
                None,
                None,
                move || c_reveal.send(UiCommand::RevealPath(lp.clone())),
            ),
            None => app_button(
                "Reveal full log in Finder",
                BtnKind::Default,
                true,
                true,
                None,
                Some("No log file on disk for this run"),
                || {},
            ),
        };

        let log_card = card()
            .child(card_title(format!(
                "Run log · {} ({})",
                insp.mapping_name, insp.run_id
            )))
            .child(
                text(format!("{} · {}", insp.started_label, insp.result_label))
                    .text_size(px(12.5))
                    .text_color(p.muted),
            )
            .child(
                view()
                    .test_id("run-inspection-log-body")
                    .selectable(true)
                    .bg(p.sunk)
                    .border_1()
                    .border_color(p.hair)
                    .rounded(px(8.0))
                    .p(px(11.0))
                    .child(mono_text(log_text, 11.0, p.fg)),
            )
            .child(
                view()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .child(app_button(
                        if is_copied { "Copied" } else { "Copy log" },
                        BtnKind::Default,
                        true,
                        false,
                        None,
                        None,
                        move || {
                            set_clipboard(log_copy.clone());
                            copied_log.set(true);
                        },
                    ))
                    .child(reveal_btn),
            );

        let mut copies_card = card()
            .test_id("run-inspection-recovery-copies")
            .child(card_title("Recovery copies from this run"))
            .child(
                text("Choose a file to copy to a separate inspection folder so you can check it before replacing anything yourself. FolderSync never restores recovery copies directly into a synced folder.")
                    .text_size(px(12.0))
                    .text_color(p.muted),
            );

        if insp.recovery_copies.is_empty() {
            copies_card = copies_card.child(
                text("No recovery copies were kept by this run.")
                    .text_size(px(12.5))
                    .text_color(p.muted),
            );
        } else {
            for (idx, copy_item) in insp.recovery_copies.iter().enumerate() {
                let c_copy = ctrl.clone();
                let rid = insp.run_id.clone();
                let rel = copy_item.relative_path.clone();
                let is_remote = copy_item.is_remote;
                let btn_tid = format!("copy-recovery-item-{idx}");
                copies_card = copies_card.child(
                    view()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .gap(px(10.0))
                        .py(px(7.0))
                        .border_t_1()
                        .border_color(p.hair)
                        .child(
                            view()
                                .flex_col()
                                .gap(px(2.0))
                                .flex_1()
                                .child(mono_text(copy_item.relative_path.clone(), 11.5, p.fg))
                                .child(
                                    text(format!(
                                        "{} · {}",
                                        copy_item.side_label, copy_item.size_label
                                    ))
                                    .text_size(px(11.5))
                                    .text_color(p.muted),
                                ),
                        )
                        .child(
                            app_button(
                                "Copy to inspection folder…",
                                BtnKind::Default,
                                true,
                                false,
                                None,
                                None,
                                move || {
                                    let c = c_copy.clone();
                                    let rid_c = rid.clone();
                                    let rel_c = rel.clone();
                                    let answer = pick_paths(
                                        OpenDialog::folders()
                                            .title(
                                                "Choose inspection folder (outside synced folders)",
                                            )
                                            .button_label("Copy here"),
                                    );
                                    spawn(async move {
                                        if let Ok(Some(paths)) = answer.await
                                            && let Some(first) = paths.first()
                                        {
                                            c.send(UiCommand::CopyRecoveryCopyToInspection {
                                                run_id: rid_c,
                                                is_remote,
                                                relative_path: rel_c,
                                                dest_dir: first.to_string_lossy().into_owned(),
                                            });
                                        }
                                    });
                                },
                            )
                            .test_id(&btn_tid),
                        ),
                );
            }
        }

        if let Some(notice) = &insp.copy_notice {
            copies_card =
                copies_card.child(text(notice.clone()).text_size(px(12.0)).text_color(p.ok));
        }

        view()
            .flex_col()
            .w_full()
            .gap(px(14.0))
            .child(nav_bar)
            .child(page_title(format!("Run details · {}", insp.mapping_name)))
            .child(log_card)
            .child(copies_card)
            .into_element()
    })
}

fn render_active_run_card(ctrl: AppController, run: &ActiveRunSnapshot) -> View {
    let p = Palette::current();
    let id_stop = run.mapping_id.clone();
    let id_force = run.mapping_id.clone();
    let name_force = run.mapping_name.clone();
    let c_stop = ctrl.clone();
    let c_force = ctrl.clone();

    match &run.phase {
        RunPhase::Scanning { elapsed_secs } => {
            let base_secs = *elapsed_secs;
            let elapsed_sig = ctrl.elapsed_offset;
            let initial_elapsed = format_duration(base_secs);
            card()
                .border_color(p.run_bd)
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .child(status_pill(IconKind::Sync, "Running", Tone::Run))
                        .child(card_title(run.mapping_name.clone()))
                        .child(
                            text(format!("· {}", run.volume_name))
                                .text_size(px(13.0))
                                .text_color(p.muted),
                        ),
                )
                .child(
                    text(format!(
                        "Scanning {} · checking the local folder and the cloud folder for changes",
                        run.mapping_name
                    ))
                    .text_size(px(13.0))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(p.fg),
                )
                .child(progress_bar(
                    None,
                    6.0,
                    format!(
                        "Scanning {}, in progress, {initial_elapsed}",
                        run.mapping_name
                    ),
                ))
                .child(kv_reactive("Elapsed", move || {
                    format_duration(base_secs.saturating_add(elapsed_sig.get()))
                }))
                .child(kv(
                    "Started",
                    &format!("{} · {}", run.started_label, run.trigger_label),
                ))
                .child(kv("Transferred", "Nothing yet"))
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .flex_wrap()
                        .gap(px(10.0))
                        .pt(px(4.0))
                        .child(
                            app_button(
                                "Stop",
                                BtnKind::Default,
                                false,
                                false,
                                Some(IconKind::Stop),
                                None,
                                move || c_stop.send(UiCommand::StopRun(id_stop.clone())),
                            )
                            .test_id("activity-stop-btn"),
                        )
                        .child(
                            text("Stopping lets the current step finish. It can take about 90 seconds and doesn't undo changes already made.")
                                .text_size(px(12.0))
                                .text_color(p.muted),
                        ),
                )
        }
        RunPhase::TransferringKnown {
            transferred_label,
            total_label,
            percent,
            elapsed_secs,
        } => {
            let base_secs = *elapsed_secs;
            let pct = *percent;
            let elapsed_sig = ctrl.elapsed_offset;
            card()
                .border_color(p.run_bd)
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .child(status_pill(IconKind::Sync, "Running", Tone::Run))
                        .child(card_title(run.mapping_name.clone()))
                        .child(
                            text(format!("· {}", run.volume_name))
                                .text_size(px(13.0))
                                .text_color(p.muted),
                        ),
                )
                .child(
                    text(format!(
                        "Transferring · {transferred_label} of {total_label}"
                    ))
                    .text_size(px(13.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(p.fg)
                    .tabular_nums(),
                )
                .child(progress_bar(
                    Some(f32::from(pct)),
                    6.0,
                    format!("Transferring {}", run.mapping_name),
                ))
                .child(
                    text(move || {
                        let elapsed = format_duration(base_secs.saturating_add(elapsed_sig.get()));
                        format!("{pct}% · elapsed {elapsed} · total known")
                    })
                    .text_size(px(12.0))
                    .text_color(p.muted)
                    .tabular_nums(),
                )
                .child(
                    view().flex_row().pt(px(4.0)).child(
                        app_button(
                            "Stop",
                            BtnKind::Default,
                            false,
                            false,
                            Some(IconKind::Stop),
                            None,
                            move || c_stop.send(UiCommand::StopRun(id_stop.clone())),
                        )
                        .test_id("activity-stop-btn"),
                    ),
                )
        }
        RunPhase::TransferringUnknown {
            transferred_label,
            elapsed_secs,
        } => {
            let base_secs = *elapsed_secs;
            let elapsed_sig = ctrl.elapsed_offset;
            card()
                .border_color(p.run_bd)
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .child(status_pill(IconKind::Sync, "Running", Tone::Run))
                        .child(card_title(run.mapping_name.clone()))
                        .child(
                            text(format!("· {}", run.volume_name))
                                .text_size(px(13.0))
                                .text_color(p.muted),
                        ),
                )
                .child(
                    text(format!("Transferring · {transferred_label} so far"))
                        .text_size(px(13.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(p.fg)
                        .tabular_nums(),
                )
                .child(progress_bar(
                    None,
                    6.0,
                    format!("{transferred_label} so far, total not known yet"),
                ))
                .child(
                    text(move || {
                        let elapsed = format_duration(base_secs.saturating_add(elapsed_sig.get()));
                        format!("Total not known yet · elapsed {elapsed}")
                    })
                    .text_size(px(12.0))
                    .text_color(p.muted)
                    .tabular_nums(),
                )
                .child(
                    view().flex_row().pt(px(4.0)).child(
                        app_button(
                            "Stop",
                            BtnKind::Default,
                            false,
                            false,
                            Some(IconKind::Stop),
                            None,
                            move || c_stop.send(UiCommand::StopRun(id_stop.clone())),
                        )
                        .test_id("activity-stop-btn"),
                    ),
                )
        }
        RunPhase::Stopping { elapsed_secs } => {
            let base_secs = *elapsed_secs;
            let elapsed_sig = ctrl.elapsed_offset;
            let initial_elapsed = format_mm_ss(base_secs);
            card()
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .child(status_pill(IconKind::Stop, "Stopping", Tone::Neutral))
                        .child(card_title(run.mapping_name.clone())),
                )
                .child(
                    text(move || {
                        let elapsed = format_mm_ss(base_secs.saturating_add(elapsed_sig.get()));
                        format!("Stopping · {elapsed}")
                    })
                    .text_size(px(13.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(p.fg)
                    .tabular_nums(),
                )
                .child(progress_bar(
                    None,
                    6.0,
                    format!("Stopping {}, {initial_elapsed}", run.mapping_name),
                ))
                .child(
                    text("Letting the current step finish. This can take about 90 seconds.")
                        .text_size(px(12.0))
                        .text_color(p.muted),
                )
                .child(view().flex_row().pt(px(4.0)).child(app_button(
                    "Force stop…",
                    BtnKind::Default,
                    true,
                    false,
                    None,
                    None,
                    move || c_force.confirm_force_stop(id_force.clone(), name_force.clone()),
                )))
        }
    }
}

fn kv_reactive(label: &str, value: impl Fn() -> String + 'static) -> View {
    let p = Palette::current();
    view()
        .flex_row()
        .items_center()
        .gap(px(12.0))
        .child(
            text(label.to_owned())
                .w(px(110.0))
                .flex_none()
                .text_size(px(12.5))
                .text_color(p.muted),
        )
        .child(
            text(value)
                .text_size(px(12.5))
                .text_color(p.fg)
                .tabular_nums(),
        )
}

fn kv(label: &str, value: &str) -> View {
    let p = Palette::current();
    view()
        .flex_row()
        .items_center()
        .gap(px(12.0))
        .child(
            text(label.to_owned())
                .w(px(110.0))
                .flex_none()
                .text_size(px(12.5))
                .text_color(p.muted),
        )
        .child(
            text(value.to_owned())
                .text_size(px(12.5))
                .text_color(p.fg)
                .tabular_nums(),
        )
}
