//! Attention screen (ordered cards with Cause / What FolderSync did / Next)
//! and the guided Start recovery checklist (`ui-spec.md` §5.6).

use nsh_core::{file_dialog::OpenDialog, prelude::*};
use sync_core::{Intent, MappingId, Readiness, RunId};

use crate::contract::{
    AttentionItem, AttentionKind, IconKind, RecoveryCauseKind, ScreenRoute, StatusModifier,
    UiCommand, UiSnapshot,
};
use crate::screens::AppController;
use crate::theme::{
    BLOCK_GAP, BtnKind, CARD_ACTION_MARGIN, HEADER_BOTTOM_MARGIN, KV_ROW_GAP, Palette, S_CAPTION,
    S_COMPACT, S_SMALL, W_STRONG, accented_card, app_button, card, card_title, icon, kv_row,
    link_button, mono_text, page_title, status_pill,
};

/// Renders the Attention screen with blocking items first, then conflicts, then low-space advisories.
pub fn attention_view(ctrl: AppController) -> impl IntoElement {
    dynamic(move || {
        let snap = ctrl.snapshot.get();
        let p = Palette::current();
        let primary_count = snap.attention_badge_count();

        let sub = if snap.attention_items.is_empty() {
            "No mappings are held or waiting for review.".to_owned()
        } else if primary_count == 1 {
            "1 item · items that block syncing first".to_owned()
        } else {
            format!("{primary_count} items · items that block syncing first")
        };

        let mut root = view().flex_col().w_full().gap(px(BLOCK_GAP)).child(
            view()
                .window_drag(true)
                .mb(px(HEADER_BOTTOM_MARGIN))
                .flex_col()
                .gap(px(3.0))
                .child(page_title("Attention"))
                .child(
                    text(sub)
                        .test_id("attention-subtitle")
                        .text_color(p.muted)
                        .tabular_nums(),
                ),
        );

        for item in &snap.attention_items {
            root = root.child(attention_card(ctrl.clone(), &snap, item));
        }

        root.into_element()
    })
}

fn latest_run_id_for(snap: &UiSnapshot, mid: &MappingId) -> RunId {
    if let Some(m) = snap.mapping(mid)
        && let Some(first) = m.recent_runs.first()
    {
        return first.run_id.clone();
    }
    RunId::new(format!("run-{mid}-latest"))
}

fn conflict_run_id_for(snap: &UiSnapshot, mid: &MappingId) -> RunId {
    if let Some(m) = snap.mapping(mid) {
        if let Some(unrev) = &m.unreviewed_conflicts {
            return unrev.run_id.clone();
        }
        if let Some(first) = m.recent_runs.first() {
            return first.run_id.clone();
        }
    }
    RunId::new(format!("run-{mid}-latest"))
}

fn attention_card(ctrl: AppController, snap: &UiSnapshot, item: &AttentionItem) -> View {
    let p = Palette::current();
    let accent_border = if item.is_error_accent {
        p.err_bd
    } else {
        p.attn_bd
    };

    let mut c = accented_card(accent_border);
    let header = view()
        .flex_row()
        .items_center()
        .flex_wrap()
        .gap(px(8.0))
        .child(status_pill(
            item.icon,
            item.pill_label.clone(),
            item.pill_tone,
        ))
        .child(card_title(item.title.clone()));
    let cause_emphasis = match &item.kind {
        AttentionKind::Authentication { account_id, .. } => Some(account_id.as_str()),
        _ => None,
    };
    let mut paragraphs = view()
        .flex_col()
        .gap(px(4.0))
        .child(labeled_para("Cause:", &item.cause, cause_emphasis))
        .child(labeled_para("What FolderSync did:", &item.what_did, None));

    if let AttentionKind::Conflicts { conflict_paths } = &item.kind {
        let mut list = view().flex_col().gap(px(3.0)).pl(px(12.0));
        for path in conflict_paths {
            list = list.child(mono_text(format!("• {path}"), S_CAPTION, p.fg));
        }
        paragraphs = paragraphs.child(list);
    }

    if let AttentionKind::EndpointChanged {
        expected_id,
        found_id,
        ..
    } = &item.kind
    {
        let folder_label = item
            .mapping_id
            .as_ref()
            .and_then(|id| snap.mapping(id))
            .map(|m| m.cloud_path.clone())
            .unwrap_or_else(|| "Folder".to_owned());
        paragraphs = paragraphs.child(
            view()
                .flex_col()
                .gap(px(KV_ROW_GAP))
                .child(kv_row(
                    "Set up with",
                    mono_text(
                        format!("{folder_label} · ID {expected_id}"),
                        S_CAPTION,
                        p.fg,
                    ),
                ))
                .child(kv_row(
                    "Found now",
                    mono_text(format!("{folder_label} · ID {found_id}"), S_CAPTION, p.fg),
                )),
        );
    }

    c = c
        .child(header)
        .child(paragraphs.child(labeled_para("Next:", &item.next_step, None)));

    let mut actions = view()
        .flex_row()
        .flex_wrap()
        .items_center()
        .gap(px(8.0))
        .mt(px(CARD_ACTION_MARGIN));

    match &item.kind {
        AttentionKind::Authentication {
            account_id,
            keychain_locked,
        } => {
            if *keychain_locked {
                if let Some(mid) = &item.mapping_id {
                    let c1 = ctrl.clone();
                    let id = mid.clone();
                    actions = actions.child(app_button(
                        "Check again",
                        BtnKind::Primary,
                        true,
                        false,
                        None,
                        None,
                        move || c1.send(UiCommand::CheckAgain(id.clone())),
                    ));
                }
            } else {
                let c1 = ctrl.clone();
                let acct = account_id.clone();
                let c2 = ctrl.clone();
                actions = actions
                    .child(app_button(
                        "Reconnect…",
                        BtnKind::Primary,
                        true,
                        false,
                        None,
                        None,
                        move || {
                            c1.send(UiCommand::ReconnectAccount(acct.clone()));
                            c1.navigate(ScreenRoute::Accounts);
                        },
                    ))
                    .child(link_button("Open Accounts", S_COMPACT, move || {
                        c2.navigate(ScreenRoute::Accounts);
                    }));
            }
        }
        AttentionKind::VolumeMissing { .. } => {
            // Physical step ("Connect PhotoSSD") has no button; only optional Pause per §5.6
            if let (Some(mid), Some(name)) = (&item.mapping_id, &item.mapping_name) {
                let c1 = ctrl.clone();
                let id = mid.clone();
                actions = actions.child(app_button(
                    format!("Pause {name}"),
                    BtnKind::Default,
                    true,
                    false,
                    None,
                    None,
                    move || c1.send(UiCommand::PauseMapping(id.clone())),
                ));
            }
        }
        AttentionKind::EndpointChanged { is_cloud, .. } => {
            if let (Some(mid), Some(name)) = (&item.mapping_id, &item.mapping_name) {
                let c1 = ctrl.clone();
                let c_open = ctrl.clone();
                let c2 = ctrl.clone();
                let id1 = mid.clone();
                let id2 = mid.clone();
                let nm = name.clone();
                let is_cloud_val = *is_cloud;
                let target_path = snap
                    .mapping(mid)
                    .map(|m| {
                        if is_cloud_val {
                            m.cloud_path.clone()
                        } else {
                            m.local_path.clone()
                        }
                    })
                    .unwrap_or_else(|| name.clone());
                let can_open_cloud = !is_cloud_val || target_path.starts_with("https://");
                actions = actions
                    .child(app_button(
                        "Check again",
                        BtnKind::Primary,
                        true,
                        false,
                        None,
                        None,
                        move || c1.send(UiCommand::CheckAgain(id1.clone())),
                    ))
                    .child(app_button(
                        if is_cloud_val {
                            "Open cloud folder"
                        } else {
                            "Reveal in Finder"
                        },
                        BtnKind::Default,
                        true,
                        !can_open_cloud,
                        None,
                        (!can_open_cloud)
                            .then_some("Cloud web link is not available for this account"),
                        move || {
                            if is_cloud_val {
                                if target_path.starts_with("https://") {
                                    c_open.send(UiCommand::OpenCloudUrl(target_path.clone()));
                                }
                            } else {
                                c_open.send(UiCommand::RevealPath(target_path.clone()));
                            }
                        },
                    ))
                    .child(app_button(
                        "Remove mapping…",
                        BtnKind::Default,
                        true,
                        false,
                        None,
                        None,
                        move || c2.confirm_remove_mapping(id2.clone(), nm.clone()),
                    ));
            }
        }
        AttentionKind::RecoveryRequired { cause_kind } => {
            if let Some(mid) = &item.mapping_id {
                let c1 = ctrl.clone();
                let c2 = ctrl.clone();
                let c3 = ctrl.clone();
                let id = mid.clone();
                let rid = latest_run_id_for(snap, mid);
                match cause_kind {
                    RecoveryCauseKind::Interrupted => {
                        let id_run = mid.clone();
                        let rid_run = rid.clone();
                        let id_copies = mid.clone();
                        let rid_copies = rid;
                        actions = actions
                            .child(
                                app_button(
                                    "Start recovery…",
                                    BtnKind::Primary,
                                    true,
                                    false,
                                    None,
                                    None,
                                    move || c1.open_recovery(id.clone()),
                                )
                                .test_id(&format!("start-recovery-{mid}")),
                            )
                            .child(
                                app_button(
                                    "Show run",
                                    BtnKind::Default,
                                    true,
                                    false,
                                    None,
                                    None,
                                    move || c2.open_run_inspection(id_run.clone(), rid_run.clone()),
                                )
                                .test_id(&format!("attn-show-run-{mid}")),
                            )
                            .child(
                                app_button(
                                    "Show recovery copies",
                                    BtnKind::Default,
                                    true,
                                    false,
                                    None,
                                    None,
                                    move || {
                                        c3.open_run_inspection(
                                            id_copies.clone(),
                                            rid_copies.clone(),
                                        )
                                    },
                                )
                                .test_id(&format!("attn-show-recovery-copies-{mid}")),
                            );
                    }
                    RecoveryCauseKind::DeletionLimit => {
                        let id_chk = mid.clone();
                        let id_run = mid.clone();
                        actions = actions
                            .child(app_button(
                                "Show run",
                                BtnKind::Default,
                                true,
                                false,
                                None,
                                None,
                                move || c2.open_run_inspection(id_run.clone(), rid.clone()),
                            ))
                            .child(app_button(
                                "Check again",
                                BtnKind::Primary,
                                true,
                                false,
                                None,
                                None,
                                move || c1.send(UiCommand::CheckAgain(id_chk.clone())),
                            ));
                    }
                    RecoveryCauseKind::EarlierSyncRunning => {
                        let id_run = mid.clone();
                        actions = actions
                            .child(app_button(
                                "Stop it",
                                BtnKind::Primary,
                                true,
                                false,
                                None,
                                None,
                                move || c1.send(UiCommand::StopRun(id.clone())),
                            ))
                            .child(app_button(
                                "Show details",
                                BtnKind::Default,
                                true,
                                false,
                                None,
                                None,
                                move || c2.open_run_inspection(id_run.clone(), rid.clone()),
                            ));
                    }
                }
            }
        }
        AttentionKind::Conflicts { .. } => {
            if let Some(mid) = &item.mapping_id {
                let c_finder = ctrl.clone();
                let c_cloud = ctrl.clone();
                let c_run = ctrl.clone();
                let c_rev = ctrl.clone();
                let id_rev = mid.clone();
                let id_run = mid.clone();
                let rid = conflict_run_id_for(snap, mid);
                let local_p = snap
                    .mapping(mid)
                    .map(|m| m.local_path.clone())
                    .unwrap_or_default();
                let cloud_p = snap
                    .mapping(mid)
                    .map(|m| m.cloud_path.clone())
                    .unwrap_or_default();
                let has_cloud_url = cloud_p.starts_with("https://");
                actions = actions
                    .child(app_button(
                        "Reveal in Finder",
                        BtnKind::Default,
                        true,
                        false,
                        None,
                        None,
                        move || {
                            c_finder.send(UiCommand::RevealPath(local_p.clone()));
                        },
                    ))
                    .child(app_button(
                        "Open cloud folder",
                        BtnKind::Default,
                        true,
                        !has_cloud_url,
                        None,
                        (!has_cloud_url)
                            .then_some("Cloud web link is not available for this account"),
                        move || {
                            if cloud_p.starts_with("https://") {
                                c_cloud.send(UiCommand::OpenCloudUrl(cloud_p.clone()));
                            }
                        },
                    ))
                    .child(
                        app_button(
                            "Show run",
                            BtnKind::Default,
                            true,
                            false,
                            None,
                            None,
                            move || c_run.open_run_inspection(id_run.clone(), rid.clone()),
                        )
                        .test_id(&format!("attn-show-run-{mid}")),
                    )
                    .child(
                        app_button(
                            "Mark as reviewed",
                            BtnKind::Default,
                            true,
                            false,
                            None,
                            None,
                            move || c_rev.send(UiCommand::MarkConflictsReviewed(id_rev.clone())),
                        )
                        .test_id(&format!("mark-reviewed-{mid}")),
                    );
            }
        }
        AttentionKind::LowSpaceAdvisory { recovery_path, .. } => {
            let rp = recovery_path.clone();
            let c_rec = ctrl.clone();
            actions = actions.child(app_button(
                "Reveal recovery folder",
                BtnKind::Default,
                true,
                false,
                None,
                None,
                move || {
                    c_rec.send(UiCommand::RevealPath(rp.clone()));
                },
            ));
        }
    }

    c.child(actions)
}

/// Renders the Start recovery checklist screen (`ui-spec.md` §5.6).
pub fn recovery_view(ctrl: AppController, mapping_id: MappingId) -> impl IntoElement {
    dynamic(move || {
        let snap = ctrl.snapshot.get();
        let p = Palette::current();
        let Some(m) = snap.mapping(&mapping_id).cloned() else {
            return view()
                .flex_col()
                .gap(px(10.0))
                .child(link_button("‹ Attention", S_COMPACT, {
                    let c = ctrl.clone();
                    move || c.navigate(ScreenRoute::Attention)
                }))
                .child(page_title("Recovery case not found"))
                .into_element();
        };

        let checklist = snap
            .recovery_checklist
            .as_ref()
            .filter(|rc| rc.mapping_id == mapping_id);

        let recovery_info = match &m.modifier {
            StatusModifier::RecoveryCause {
                kind,
                detail,
                recovery_copies_count,
                ambiguous_ownership,
            } => Some((
                *kind,
                detail.clone(),
                *recovery_copies_count,
                *ambiguous_ownership,
            )),
            _ => None,
        };
        let ambiguous = checklist
            .map(|rc| rc.has_ambiguous_case)
            .unwrap_or_else(|| recovery_info.as_ref().is_some_and(|(_, _, _, amb)| *amb));
        let orphan_alive = checklist.and_then(|rc| rc.orphan_alive);

        let latest_run_id = checklist
            .and_then(|rc| rc.latest_run_id.clone())
            .unwrap_or_else(|| latest_run_id_for(&snap, &m.id));

        let c_back = ctrl.clone();
        let c_log = ctrl.clone();
        let c_run = ctrl.clone();
        let c_keep = ctrl.clone();
        let id_log = m.id.clone();
        let rid_log = latest_run_id.clone();
        let id_run = m.id.clone();
        let id_keep = m.id.clone();

        // Render each checklist item as verified, failed, or unknown strictly from snapshot/checklist fields.
        let hold_check = if m.state.intent == Intent::Paused {
            check_line(
                IconKind::Check,
                p.ok,
                &format!("{} is paused while you recover it.", m.display_name),
            )
        } else {
            check_line(
                IconKind::Alert,
                p.attn,
                &format!(
                    "{} is held for recovery (automatic sync will not run until you finish).",
                    m.display_name
                ),
            )
        };

        let inspected_ok = checklist.is_some_and(|rc| rc.inspected);
        let identities_ok = checklist.is_some_and(|rc| rc.identities_verified);

        let proc_check = if !inspected_ok {
            check_line(IconKind::Sync, p.run, "Checking earlier sync processes…")
        } else if ambiguous {
            check_line(
                IconKind::Alert,
                p.attn,
                "Earlier sync process: unknown — can't confirm whether a process still holds the lock.",
            )
        } else if orphan_alive == Some(true)
            || matches!(
                recovery_info,
                Some((RecoveryCauseKind::EarlierSyncRunning, _, _, false))
            )
        {
            check_line(
                IconKind::Octagon,
                p.err,
                "An earlier sync process is still running for this mapping.",
            )
        } else if orphan_alive == Some(false) {
            check_line(IconKind::Check, p.ok, "No earlier sync process is running.")
        } else {
            check_line(IconKind::Alert, p.attn, "Earlier sync process: checking…")
        };

        let vol_snap = snap.volumes.iter().find(|v| v.identity == m.volume_id);
        let volume_check = match vol_snap {
            Some(v) if v.connected && m.state.readiness() != Readiness::VolumeUnavailable => {
                check_line(
                    IconKind::Check,
                    p.ok,
                    &format!("{} is connected.", m.volume_name),
                )
            }
            Some(_) => check_line(
                IconKind::Octagon,
                p.err,
                &format!("{} is not connected right now.", m.volume_name),
            ),
            None => check_line(
                IconKind::Dash,
                p.muted,
                &format!("{}: Checking…", m.volume_name),
            ),
        };

        let identity_check = match checklist {
            None => check_line(
                IconKind::Sync,
                p.run,
                "Checking local and destination folder identities…",
            ),
            Some(rc) if rc.identities_verified => {
                check_line(IconKind::Check, p.ok, &rc.identity_check_detail)
            }
            Some(rc) => check_line(IconKind::Octagon, p.err, &rc.identity_check_detail),
        };

        let copies_list = checklist
            .map(|rc| rc.recovery_copies.clone())
            .unwrap_or_default();
        let copies_count = if !copies_list.is_empty() {
            copies_list.len()
        } else {
            recovery_info.as_ref().map(|(_, _, n, _)| *n).unwrap_or(0)
        };
        let detail_text = checklist
            .map(|rc| rc.cause.clone())
            .or_else(|| recovery_info.as_ref().map(|(_, d, _, _)| d.clone()))
            .unwrap_or_default();
        let copies_part = match copies_count {
            0 => "No recovery copies recorded.".to_owned(),
            1 => "1 recovery copy was kept.".to_owned(),
            n => format!("{n} recovery copies were kept."),
        };
        let evidence_line = if detail_text.trim().is_empty() {
            copies_part
        } else {
            format!("{detail_text}. {copies_part}")
        };

        let mut body = card()
            .child(hold_check)
            .child(proc_check)
            .child(volume_check)
            .child(identity_check)
            .child(
                view()
                    .flex_row()
                    .items_start()
                    .gap(px(8.0))
                    .py(px(2.0))
                    .child(icon(IconKind::Alert, 15.0, p.attn))
                    .child(
                        view()
                            .flex_1()
                            .min_w(px(0.0))
                            .flex_col()
                            .items_start()
                            .gap(px(4.0))
                            .child(
                                text(evidence_line)
                                    .w_full()
                                    .text_size(px(S_COMPACT))
                                    .text_color(p.fg),
                            )
                            .child(link_button(
                                "Show run log and recovery copies",
                                S_COMPACT,
                                move || {
                                    c_log.open_run_inspection(id_log.clone(), rid_log.clone());
                                },
                            )),
                    ),
            );

        if let Some(rc) = checklist
            && (rc.has_lock_file || !rc.workdir_files.is_empty())
        {
            let lock_note = if rc.has_lock_file {
                "Lock file present in work directory."
            } else {
                "No lock file in work directory."
            };
            let wd_note = if rc.workdir_files.is_empty() {
                lock_note.to_owned()
            } else {
                format!("{lock_note} Files: {}", rc.workdir_files.join(", "))
            };
            body = body.child(
                text(wd_note)
                    .text_size(px(S_SMALL))
                    .text_color(p.muted)
                    .pt(px(2.0)),
            );
        }

        if !copies_list.is_empty() {
            let mut copies_section = view()
                .flex_col()
                .gap(px(4.0))
                .pt(px(8.0))
                .border_t_1()
                .border_color(p.hair)
                .child(card_title("Recovery copies"));
            for copy_item in &copies_list {
                let c_cp = ctrl.clone();
                let rid = latest_run_id.clone();
                let rel = copy_item.relative_path.clone();
                let is_remote = copy_item.is_remote;
                copies_section = copies_section.child(
                    view()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .gap(px(8.0))
                        .py(px(4.0))
                        .child(
                            view()
                                .flex_col()
                                .flex_1()
                                .child(mono_text(copy_item.relative_path.clone(), S_CAPTION, p.fg))
                                .child(
                                    text(format!(
                                        "{} · {}",
                                        copy_item.side_label, copy_item.size_label
                                    ))
                                    .text_size(px(S_CAPTION))
                                    .text_color(p.muted),
                                ),
                        )
                        .child(app_button(
                            "Copy to inspection folder…",
                            BtnKind::Default,
                            true,
                            false,
                            None,
                            None,
                            move || {
                                let c = c_cp.clone();
                                let rid_c = rid.clone();
                                let rel_c = rel.clone();
                                let answer = pick_paths(
                                    OpenDialog::folders()
                                        .title("Choose inspection folder (outside synced folders)")
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
                        )),
                );
            }
            body = body.child(copies_section);
        }

        if ambiguous {
            let c_diag = ctrl.clone();
            body = body
                .child(
                    text("FolderSync can't confirm whether an earlier sync process owns the lock. Export diagnostics before resuming.")
                        .text_color(p.err)
                        .pt(px(8.0)),
                )
                .child(
                    view().flex_row().gap(px(8.0)).pt(px(6.0)).child(app_button(
                        "Export diagnostics",
                        BtnKind::Default,
                        false,
                        false,
                        None,
                        None,
                        move || c_diag.navigate(ScreenRoute::Preferences),
                    )),
                );
        } else if orphan_alive == Some(true) {
            let c_recheck = ctrl.clone();
            let id_recheck = m.id.clone();
            body = body
                .child(
                    text("Wait for the earlier sync process to finish, then check again. FolderSync won't start a second sync while it is running.")
                        .text_color(p.err)
                        .pt(px(8.0)),
                )
                .child(
                    view().flex_row().gap(px(8.0)).pt(px(6.0)).child(app_button(
                        "Check again",
                        BtnKind::Default,
                        false,
                        false,
                        None,
                        None,
                        move || c_recheck.send(UiCommand::InspectRecovery(id_recheck.clone())),
                    )),
                );
        } else {
            let can_resume =
                inspected_ok && identities_ok && orphan_alive == Some(false) && !ambiguous;
            let can_keep_paused = inspected_ok && orphan_alive == Some(false) && !ambiguous;
            body = body
                .child(
                    text(if can_resume {
                        "A normal run can continue from here. It checks both sides again and keeps recovery copies as usual."
                    } else if !inspected_ok {
                        "Checking earlier sync processes and folder identities before enabling resume…"
                    } else {
                        "Confirm folder identities match setup before resuming normal sync."
                    })
                    .text_color(p.fg)
                    .pt(px(8.0)),
                )
                .child(
                    view()
                        .flex_row()
                        .gap(px(8.0))
                        .pt(px(4.0))
                        .child(
                            app_button(
                                "Run normally and resume",
                                BtnKind::Primary,
                                false,
                                !can_resume,
                                None,
                                (!can_resume).then_some(
                                    "Waiting for inspection and folder identity verification",
                                ),
                                move || {
                                    c_run.send(UiCommand::CompleteRecovery {
                                        mapping_id: id_run.clone(),
                                        resume_and_run: true,
                                    });
                                    c_run.navigate(ScreenRoute::Overview);
                                },
                            )
                            .test_id("recovery-run-and-resume-btn"),
                        )
                        .child(
                            app_button(
                                "Keep paused",
                                BtnKind::Default,
                                false,
                                !can_keep_paused,
                                None,
                                (!can_keep_paused)
                                    .then_some("Waiting for recovery inspection to finish"),
                                move || {
                                    c_keep.send(UiCommand::CompleteRecovery {
                                        mapping_id: id_keep.clone(),
                                        resume_and_run: false,
                                    });
                                    c_keep.navigate(ScreenRoute::Overview);
                                },
                            )
                            .test_id("recovery-keep-paused-btn"),
                        ),
                );
        }

        body = body.child(
            text("To get back a file: choose it in Show run log and recovery copies, then Copy to inspection folder…. FolderSync never puts it back into the synced folder for you.")
                .text_size(px(S_SMALL))
                .text_color(p.muted)
                .pt(px(8.0)),
        );

        view()
            .flex_col()
            .w_full()
            .gap(px(14.0))
            .child(link_button("‹ Attention", S_COMPACT, move || {
                c_back.navigate(ScreenRoute::Attention);
            }))
            .child(page_title(format!("Recover “{}”", m.display_name)))
            .child(body)
            .into_element()
    })
}

fn labeled_para(prefix: &str, body: &str, emphasis: Option<&str>) -> Text {
    let paragraph = text("")
        .w_full()
        .child(text(prefix.to_owned()).font_weight(W_STRONG));
    if let Some(emphasis) = emphasis
        && let Some((before, after)) = body.split_once(&format!(" {emphasis} "))
    {
        return paragraph
            .child(text(format!(" {before} ")))
            .child(text(emphasis.to_owned()).font_weight(W_STRONG))
            .child(text(format!(" {after}")));
    }
    paragraph.child(text(format!(" {body}")))
}

fn check_line(icon_kind: IconKind, tint: AdaptiveColor, line: &str) -> View {
    let p = Palette::current();
    view()
        .flex_row()
        .items_start()
        .gap(px(8.0))
        .py(px(2.0))
        .child(icon(icon_kind, 15.0, tint))
        .child(text(line.to_owned()).flex_1().text_color(p.fg))
}
