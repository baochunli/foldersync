//! Attention screen (ordered cards with Cause / What FolderSync did / Next)
//! and the guided Start recovery checklist (`ui-spec.md` §5.6).

use nsh_core::prelude::*;
use sync_core::MappingId;

use crate::contract::{
    AttentionItem, AttentionKind, IconKind, RecoveryCauseKind, ScreenRoute, StatusModifier,
    UiCommand,
};
use crate::screens::AppController;
use crate::theme::{
    BtnKind, Palette, app_button, card, card_title, icon, link_button, mono_text, page_title,
    status_pill,
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

        let mut root = view().flex_col().w_full().gap(px(14.0)).child(
            view()
                .flex_col()
                .gap(px(3.0))
                .child(page_title("Attention"))
                .child(
                    text(sub)
                        .test_id("attention-subtitle")
                        .text_size(px(13.0))
                        .text_color(p.muted)
                        .tabular_nums(),
                ),
        );

        for item in &snap.attention_items {
            root = root.child(attention_card(ctrl.clone(), item));
        }

        root.into_element()
    })
}

fn attention_card(ctrl: AppController, item: &AttentionItem) -> View {
    let p = Palette::current();
    let accent_border = if item.is_error_accent {
        p.err_bd
    } else {
        p.attn_bd
    };

    let mut c = card()
        .border_l(px(4.0))
        .border_l_color(accent_border)
        .child(
            view()
                .flex_row()
                .items_center()
                .flex_wrap()
                .gap(px(8.0))
                .child(status_pill(
                    item.icon,
                    item.pill_label.clone(),
                    item.pill_tone,
                ))
                .child(card_title(item.title.clone())),
        )
        .child(labeled_para("Cause:", &item.cause))
        .child(labeled_para("What FolderSync did:", &item.what_did));

    if let AttentionKind::Conflicts { conflict_paths } = &item.kind {
        let mut list = view().flex_col().gap(px(3.0)).pl(px(12.0));
        for path in conflict_paths {
            list = list.child(mono_text(format!("• {path}"), 11.5, p.fg));
        }
        c = c.child(list);
    }

    if let AttentionKind::EndpointChanged {
        expected_id,
        found_id,
        ..
    } = &item.kind
    {
        c = c.child(
            view()
                .flex_col()
                .gap(px(4.0))
                .pt(px(4.0))
                .child(
                    view()
                        .flex_row()
                        .gap(px(12.0))
                        .child(
                            text("Set up with")
                                .w(px(100.0))
                                .text_size(px(12.0))
                                .text_color(p.muted),
                        )
                        .child(mono_text(
                            format!("Data/Archive · ID {expected_id}"),
                            11.5,
                            p.fg,
                        )),
                )
                .child(
                    view()
                        .flex_row()
                        .gap(px(12.0))
                        .child(
                            text("Found now")
                                .w(px(100.0))
                                .text_size(px(12.0))
                                .text_color(p.muted),
                        )
                        .child(mono_text(
                            format!("Data/Archive · ID {found_id}"),
                            11.5,
                            p.fg,
                        )),
                ),
        );
    }

    c = c.child(labeled_para("Next:", &item.next_step));

    let mut actions = view()
        .flex_row()
        .flex_wrap()
        .items_center()
        .gap(px(8.0))
        .pt(px(4.0));

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
                    .child(link_button("Open Accounts", 12.5, move || {
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
                let c2 = ctrl.clone();
                let id1 = mid.clone();
                let id2 = mid.clone();
                let nm = name.clone();
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
                        if *is_cloud {
                            "Open cloud folder"
                        } else {
                            "Reveal in Finder"
                        },
                        BtnKind::Default,
                        true,
                        false,
                        None,
                        None,
                        || {
                            drop(open_url("https://drive.google.com"));
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
                let id = mid.clone();
                match cause_kind {
                    RecoveryCauseKind::Interrupted => {
                        actions = actions
                            .child(app_button(
                                "Start recovery…",
                                BtnKind::Primary,
                                true,
                                false,
                                None,
                                None,
                                move || c1.navigate(ScreenRoute::Recovery(id.clone())),
                            ))
                            .child(app_button(
                                "Show run",
                                BtnKind::Default,
                                true,
                                false,
                                None,
                                None,
                                move || c2.navigate(ScreenRoute::Activity),
                            ))
                            .child(app_button(
                                "Show recovery copies",
                                BtnKind::Default,
                                true,
                                false,
                                None,
                                Some("Demo mode: recovery copies stay outside the synced folder"),
                                || {},
                            ));
                    }
                    RecoveryCauseKind::DeletionLimit => {
                        let id_chk = mid.clone();
                        actions = actions
                            .child(app_button(
                                "Show run",
                                BtnKind::Default,
                                true,
                                false,
                                None,
                                None,
                                move || c2.navigate(ScreenRoute::Activity),
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
                                move || c2.navigate(ScreenRoute::Activity),
                            ));
                    }
                }
            }
        }
        AttentionKind::Conflicts { .. } => {
            if let Some(mid) = &item.mapping_id {
                let c_run = ctrl.clone();
                let c_rev = ctrl.clone();
                let id_rev = mid.clone();
                actions = actions
                    .child(app_button(
                        "Reveal in Finder",
                        BtnKind::Default,
                        true,
                        false,
                        None,
                        None,
                        || {},
                    ))
                    .child(app_button(
                        "Open cloud folder",
                        BtnKind::Default,
                        true,
                        false,
                        None,
                        None,
                        || {
                            drop(open_url("https://drive.google.com"));
                        },
                    ))
                    .child(app_button(
                        "Show run",
                        BtnKind::Default,
                        true,
                        false,
                        None,
                        None,
                        move || c_run.navigate(ScreenRoute::Activity),
                    ))
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
            actions = actions.child(app_button(
                "Reveal recovery folder",
                BtnKind::Default,
                true,
                false,
                None,
                None,
                move || {
                    drop(reveal_path(rp.clone()));
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
                .child(link_button("‹ Attention", 12.5, {
                    let c = ctrl.clone();
                    move || c.navigate(ScreenRoute::Attention)
                }))
                .child(page_title("Recovery case not found"))
                .into_element();
        };

        let (copies_count, ambiguous) = match &m.modifier {
            StatusModifier::RecoveryCause {
                recovery_copies_count,
                ambiguous_ownership,
                ..
            } => (*recovery_copies_count, *ambiguous_ownership),
            _ => (3, false),
        };

        let c_back = ctrl.clone();
        let c_log = ctrl.clone();
        let c_run = ctrl.clone();
        let c_keep = ctrl.clone();
        let id_run = m.id.clone();
        let id_keep = m.id.clone();

        let mut body = card()
            .child(check_line(
                IconKind::Check,
                p.ok,
                &format!("{} is paused while you recover it.", m.display_name),
            ))
            .child(check_line(
                IconKind::Check,
                p.ok,
                "No sync process from the interrupted run is still running.",
            ))
            .child(check_line(
                IconKind::Check,
                p.ok,
                "Local folder, volume and cloud folder match the setup.",
            ))
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
                                text(format!(
                                    "The run log ends during uploads. {copies_count} recovery copies were kept."
                                ))
                                .w_full()
                                .text_size(px(12.5))
                                .text_color(p.fg),
                            )
                            .child(link_button("Show run log", 12.5, move || {
                                c_log.navigate(ScreenRoute::Activity);
                            })),
                    ),
            );

        if ambiguous {
            let c_diag = ctrl.clone();
            body = body
                .child(
                    text("FolderSync can't confirm whether an earlier sync process owns the lock. Export diagnostics before resuming.")
                        .text_size(px(13.0))
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
        } else {
            body = body
                .child(
                    text("A normal run can continue from here. It checks both sides again and keeps recovery copies as usual.")
                        .text_size(px(13.0))
                        .text_color(p.fg)
                        .pt(px(8.0)),
                )
                .child(
                    view()
                        .flex_row()
                        .gap(px(8.0))
                        .pt(px(4.0))
                        .child(app_button(
                            "Run normally and resume",
                            BtnKind::Primary,
                            false,
                            false,
                            None,
                            None,
                            move || {
                                c_run.send(UiCommand::CompleteRecovery {
                                    mapping_id: id_run.clone(),
                                    resume_and_run: true,
                                });
                                c_run.navigate(ScreenRoute::Overview);
                            },
                        ))
                        .child(app_button(
                            "Keep paused",
                            BtnKind::Default,
                            false,
                            false,
                            None,
                            None,
                            move || {
                                c_keep.send(UiCommand::CompleteRecovery {
                                    mapping_id: id_keep.clone(),
                                    resume_and_run: false,
                                });
                                c_keep.navigate(ScreenRoute::Overview);
                            },
                        )),
                );
        }

        body = body.child(
            text("To get back a file: choose it in Show recovery copies, then Copy to inspection folder…. FolderSync never puts it back into the synced folder for you.")
                .text_size(px(12.0))
                .text_color(p.muted)
                .pt(px(8.0)),
        );

        view()
            .flex_col()
            .w_full()
            .gap(px(14.0))
            .child(link_button("‹ Attention", 12.5, move || {
                c_back.navigate(ScreenRoute::Attention);
            }))
            .child(page_title(format!("Recover “{}”", m.display_name)))
            .child(body)
            .into_element()
    })
}

fn labeled_para(prefix: &str, body: &str) -> View {
    let p = Palette::current();
    view().w_full().min_w(px(0.0)).child(
        text(format!("{prefix} {body}"))
            .w_full()
            .text_size(px(13.0))
            .text_color(p.fg),
    )
}

fn check_line(icon_kind: IconKind, tint: AdaptiveColor, line: &str) -> View {
    let p = Palette::current();
    view()
        .flex_row()
        .items_start()
        .gap(px(8.0))
        .py(px(2.0))
        .child(icon(icon_kind, 15.0, tint))
        .child(
            text(line.to_owned())
                .flex_1()
                .text_size(px(13.0))
                .text_color(p.fg),
        )
}
