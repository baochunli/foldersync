//! 6-step Add-mapping flow (Account, Folder, Location, Subfolders, Recovery, Review)
//! plus the post-start First sync checklist (`ui-spec.md` §5.4).

use nsh_base::{
    checkbox, input, radio, radio_group, table, table_body, table_cell, table_head, table_header,
    table_row,
};
use nsh_core::{file_dialog::OpenDialog, prelude::*};
use sync_core::{DEFAULT_EXCLUSIONS, DEFAULT_MAX_DELETE_PERCENT, VolumeKind};

use crate::contract::{
    AccountStatus, IconKind, ReviewScenario, ScreenRoute, Tone, UiCommand, UiSnapshot, WizardDraft,
    WizardStep, format_duration,
};
use crate::screens::AppController;
use crate::theme::{
    BtnKind, Palette, app_button, capacity_meter, card, card_title, focus_ring, icon, link_button,
    mono_text, page_title, progress_bar, section_label, status_pill,
};

/// Renders the Add-mapping wizard inside the main content area (sidebar stays visible).
pub fn wizard_view(ctrl: AppController) -> impl IntoElement {
    let pattern_input = signal(String::new());
    dynamic(move || {
        let snap = ctrl.snapshot.get();
        let draft = &snap.wizard;
        let skip_account = snap
            .accounts
            .iter()
            .filter(|a| a.status == AccountStatus::Connected)
            .count()
            <= 1;

        if draft.step == WizardStep::FirstSync {
            return render_first_sync(ctrl.clone(), draft).into_element();
        }

        let (title, subtitle) = match draft.step {
            WizardStep::Account => ("Add mapping".to_owned(), None),
            WizardStep::Folder => (
                "Choose a folder".to_owned(),
                Some(format!(
                    "Folders in {}:Data · about 10.4 TB in total. Pick one; add more afterwards.",
                    draft.selected_account
                )),
            ),
            WizardStep::Location => (format!("Where should {} live?", draft.selected_folder), None),
            WizardStep::Subfolders => (
                "Choose what to include".to_owned(),
                Some("Clear a subfolder to leave it in the cloud only.".to_owned()),
            ),
            WizardStep::Recovery => (
                "Recovery copies".to_owned(),
                Some(
                    "When a run overwrites or removes a file, FolderSync keeps the earlier version. Each run gets its own folder."
                        .to_owned(),
                ),
            ),
            WizardStep::Review => (format!("Review “{}”", draft.selected_folder), None),
            WizardStep::FirstSync => (format!("Setting up “{}”", draft.selected_folder), None),
        };

        let p = Palette::current();
        let mut header = view().flex_col().gap(px(3.0)).child(page_title(title));
        if let Some(sub) = subtitle {
            header = header.child(text(sub).text_size(px(13.0)).text_color(p.muted));
        }

        let body = match draft.step {
            WizardStep::Account => render_step_account(ctrl.clone(), &snap),
            WizardStep::Folder => render_step_folder(ctrl.clone(), &snap),
            WizardStep::Location => render_step_location(ctrl.clone(), draft),
            WizardStep::Subfolders => render_step_subfolders(ctrl.clone(), draft, pattern_input),
            WizardStep::Recovery => render_step_recovery(ctrl.clone(), draft),
            WizardStep::Review => render_step_review(ctrl.clone(), draft),
            WizardStep::FirstSync => render_first_sync(ctrl.clone(), draft),
        };

        let footer = render_wizard_footer(ctrl.clone(), draft, skip_account);

        view()
            .flex_col()
            .w_full()
            .gap(px(16.0))
            .child(link_button("‹ Overview", 12.5, {
                let c = ctrl.clone();
                move || c.navigate(ScreenRoute::Overview)
            }))
            .child(header)
            .child(step_indicator(ctrl.clone(), draft.step))
            .child(body)
            .child(footer)
            .into_element()
    })
}

fn step_indicator(ctrl: AppController, current: WizardStep) -> View {
    let p = Palette::current();
    let mut row = view()
        .flex_row()
        .flex_wrap()
        .gap(px(6.0))
        .accessibility_description("Add mapping steps");

    for (step, label) in WizardStep::SIX_STEPS {
        let is_current = step == current;
        let is_done = step < current;
        let (pill_bg, pill_bd, pill_fg) = if is_current {
            (p.accent_soft, p.primary, p.primary)
        } else {
            (p.bg, p.hair, if is_done { p.fg } else { p.muted })
        };
        let (dot_bg, dot_fg, dot_text) = if is_current {
            (p.primary, p.primary_fg, step.number().to_string())
        } else if is_done {
            (p.ok_bg, p.ok, "✓".to_owned())
        } else {
            (p.sunk, p.muted, step.number().to_string())
        };
        let c = ctrl.clone();
        let tid = format!("wizard-step-{}", step.number());
        row = row.child(
            view()
                .test_id(&tid)
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .pl(px(4.0))
                .pr(px(11.0))
                .py(px(3.0))
                .rounded_full()
                .bg(pill_bg)
                .border_1()
                .border_color(pill_bd)
                .cursor_pointer()
                .focus_visible(focus_ring)
                .on_press(move || c.send(UiCommand::WizardSetStep(step)))
                .child(
                    view()
                        .w(px(18.0))
                        .h(px(18.0))
                        .rounded_full()
                        .bg(dot_bg)
                        .items_center()
                        .justify_center()
                        .child(
                            text(dot_text)
                                .text_size(px(11.0))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(dot_fg),
                        ),
                )
                .child(
                    text(label)
                        .text_size(px(12.0))
                        .font_weight(if is_current {
                            FontWeight::SEMIBOLD
                        } else {
                            FontWeight::NORMAL
                        })
                        .text_color(pill_fg),
                ),
        );
    }
    row
}

fn render_step_account(ctrl: AppController, snap: &UiSnapshot) -> View {
    let p = Palette::current();
    let mut group = radio_group()
        .accessibility_label("Which Google account holds the folder?")
        .flex_col()
        .gap(px(8.0));

    for acct in &snap.accounts {
        let is_disabled = acct.status != AccountStatus::Connected;
        let is_checked = !is_disabled && snap.wizard.selected_account == acct.id;
        let c = ctrl.clone();
        let id = acct.id.clone();
        let sub = if is_disabled {
            "Sign-in required. Reconnect it in Accounts first.".to_owned()
        } else {
            "Connected · access checked today at 14:30".to_owned()
        };

        group = group.child(
            radio()
                .checked(is_checked)
                .disabled(is_disabled)
                .flex_row()
                .items_start()
                .gap(px(10.0))
                .p(px(10.0))
                .rounded(px(8.0))
                .border_1()
                .border_color(if is_checked { p.primary } else { p.hair })
                .bg(if is_checked { p.accent_soft } else { p.card })
                .disabled_style(|s| s.opacity(0.55))
                .focus_visible(focus_ring)
                .on_change(move |checked: &bool| {
                    if *checked {
                        c.send(UiCommand::WizardSelectAccount(id.clone()));
                    }
                })
                .child(radio_circle(is_checked, is_disabled))
                .child(
                    view()
                        .flex_col()
                        .gap(px(2.0))
                        .child(
                            text(format!("{} · {}", acct.id, acct.email))
                                .text_size(px(13.0))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(p.fg),
                        )
                        .child(text(sub).text_size(px(12.0)).text_color(p.muted)),
                ),
        );
    }

    let c_conn = ctrl.clone();
    card()
        .child(
            text("Which Google account holds the folder?")
                .text_size(px(15.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(p.fg),
        )
        .child(group)
        .child(view().pt(px(4.0)).child(app_button(
            "Connect account…",
            BtnKind::Default,
            false,
            false,
            None,
            None,
            move || c_conn.navigate(ScreenRoute::Accounts),
        )))
}

fn render_step_folder(ctrl: AppController, snap: &UiSnapshot) -> View {
    let p = Palette::current();
    let selected_folder = &snap.wizard.selected_folder;

    let mut body = table_body().flex_col().w_full();
    for item in &snap.cloud_catalog {
        let already_mapped = item.on_this_mac.is_some();
        let is_selected = item.name == *selected_folder;
        let c = ctrl.clone();
        let name = item.name.clone();

        let mut inner = view()
            .w_full()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .px(px(10.0))
            .py(px(8.0))
            .bg(if is_selected {
                p.accent_soft
            } else {
                AdaptiveColor::TRANSPARENT
            })
            .focus_visible(focus_ring)
            .child(
                table_cell()
                    .w(px(24.0))
                    .child(radio_circle(is_selected, already_mapped)),
            )
            .child(
                table_cell()
                    .flex_1()
                    .flex_col()
                    .child(
                        text(item.name.clone())
                            .text_size(px(13.0))
                            .font_weight(if is_selected {
                                FontWeight::SEMIBOLD
                            } else {
                                FontWeight::NORMAL
                            })
                            .text_color(if already_mapped { p.muted } else { p.fg }),
                    )
                    .child(if item.used_by_other_computer {
                        text("Used by another computer")
                            .text_size(px(11.5))
                            .text_color(p.muted)
                            .into_element()
                    } else {
                        view().into_element()
                    }),
            )
            .child(
                table_cell().w(px(86.0)).items_end().child(
                    text(item.size_label.clone())
                        .text_size(px(12.5))
                        .font_weight(if is_selected {
                            FontWeight::SEMIBOLD
                        } else {
                            FontWeight::NORMAL
                        })
                        .text_color(if item.size_gb.is_none() {
                            p.muted
                        } else {
                            p.fg
                        })
                        .tabular_nums(),
                ),
            )
            .child(
                table_cell().w(px(124.0)).child(
                    text(
                        item.on_this_mac
                            .clone()
                            .unwrap_or_else(|| "Not on this Mac".to_owned()),
                    )
                    .text_size(px(12.0))
                    .text_color(if already_mapped { p.muted } else { p.fg }),
                ),
            );

        if !already_mapped {
            inner = inner.cursor_pointer().on_press(move || {
                c.send(UiCommand::WizardSelectFolder(name.clone()));
            });
        }
        let row = table_row()
            .w_full()
            .border_t_1()
            .border_color(p.hair)
            .child(inner);
        body = body.child(row);
    }

    let c_new = ctrl.clone();
    let folder_table_card = card()
        .flex_1()
        .p(px(8.0))
        .child(
            table()
                .accessibility_label("Cloud folders in Data")
                .w_full()
                .child(
                    table_header().child(
                        table_row()
                            .w_full()
                            .gap(px(10.0))
                            .px(px(10.0))
                            .py(px(6.0))
                            .child(table_head().w(px(24.0)))
                            .child(table_head().flex_1().child(section_label("Folder")))
                            .child(
                                table_head()
                                    .w(px(86.0))
                                    .items_end()
                                    .child(section_label("Size")),
                            )
                            .child(table_head().w(px(124.0)).child(section_label("On this Mac"))),
                    ),
                )
                .child(body),
        )
        .child(
            view()
                .flex_row()
                .items_center()
                .flex_wrap()
                .gap(px(8.0))
                .px(px(8.0))
                .pt(px(6.0))
                .child(app_button(
                    "New folder in Data…",
                    BtnKind::Default,
                    true,
                    false,
                    Some(IconKind::Plus),
                    None,
                    move || {
                        c_new.send(UiCommand::WizardSelectFolder("Shared-Media".to_owned()));
                        c_new.send(UiCommand::WizardSetReviewScenario(
                            ReviewScenario::LocalToCloud,
                        ));
                    },
                ))
                .child(
                    text("Only folders directly inside Data are offered, so collections never overlap.")
                        .text_size(px(11.5))
                        .text_color(p.muted),
                ),
        );

    // Side card: "Where <Folder> fits"
    let selected_size_gb = snap
        .cloud_catalog
        .iter()
        .find(|f| f.name == *selected_folder)
        .and_then(|f| f.size_gb)
        .unwrap_or(18.6);
    let selected_size_label = snap
        .cloud_catalog
        .iter()
        .find(|f| f.name == *selected_folder)
        .map(|f| f.size_label.clone())
        .unwrap_or_else(|| "18.6 GB".to_owned());

    let mut fit_card = card()
        .w(px(276.0))
        .flex_none()
        .child(card_title(format!("Where {selected_folder} fits")))
        .child(
            text(format!(
                "About {selected_size_label}, sizes from Google Drive at {}",
                snap.cloud_catalog_measured_at
            ))
            .text_size(px(12.0))
            .text_color(p.muted),
        );

    for vol in &snap.volumes {
        let icon_kind = if !vol.connected {
            IconKind::DriveMissing
        } else if vol.identity.kind == VolumeKind::Internal {
            IconKind::DriveInternal
        } else {
            IconKind::DriveExternal
        };
        let (fit_text, fit_color) = if !vol.connected {
            ("Not connected".to_owned(), p.muted)
        } else if (vol.free_gb as f64) < selected_size_gb {
            (format!("{} free · doesn't fit", vol.free_label), p.err)
        } else if vol.low_space {
            (format!("{} free · fits, low", vol.free_label), p.attn)
        } else {
            (format!("{} free · fits", vol.free_label), p.ok)
        };
        fit_card = fit_card.child(
            view()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(8.0))
                .py(px(6.0))
                .border_t_1()
                .border_color(p.hair)
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .child(icon(icon_kind, 14.0, p.muted))
                        .child(
                            text(vol.display_name.clone())
                                .text_size(px(12.5))
                                .text_color(p.fg),
                        ),
                )
                .child(
                    text(fit_text)
                        .text_size(px(12.0))
                        .text_color(fit_color)
                        .tabular_nums(),
                ),
        );
    }

    fit_card = fit_card.child(
        text("You can leave out large subfolders in step 4.")
            .text_size(px(11.5))
            .text_color(p.muted)
            .pt(px(4.0)),
    );

    view()
        .flex_row()
        .items_start()
        .gap(px(16.0))
        .w_full()
        .child(folder_table_card)
        .child(fit_card)
}

fn render_step_location(ctrl: AppController, draft: &WizardDraft) -> View {
    let p = Palette::current();
    let c_pick = ctrl.clone();
    let c_bad = ctrl.clone();
    let c_good = ctrl.clone();
    let vol_kind_str = match draft.volume_kind {
        VolumeKind::Internal => "internal",
        VolumeKind::External => "external",
    };

    let mut checks_col = view().flex_col().gap(px(6.0)).pt(px(8.0));
    checks_col = checks_col.child(section_label("Checks"));
    for chk in &draft.location_checks {
        checks_col = checks_col.child(
            view()
                .flex_row()
                .items_start()
                .gap(px(8.0))
                .child(icon(
                    if chk.ok {
                        IconKind::Check
                    } else {
                        IconKind::Octagon
                    },
                    15.0,
                    if chk.ok { p.ok } else { p.err },
                ))
                .child(
                    text(chk.message.clone())
                        .flex_1()
                        .text_size(px(12.5))
                        .text_color(if chk.ok { p.fg } else { p.err }),
                ),
        );
    }

    let free_after = (draft.volume_free_gb as f64 - draft.included_gb()).max(0.0);
    let free_after_label = if free_after >= 1000.0 {
        format!("{:.2} TB", free_after / 1000.0)
    } else {
        format!("{free_after:.0} GB")
    };
    let free_now_label = if draft.volume_free_gb >= 1000 {
        format!("{:.1} TB", draft.volume_free_gb as f64 / 1000.0)
    } else {
        format!("{} GB", draft.volume_free_gb)
    };
    let space_line =
        format!("{free_now_label} free now · about {free_after_label} free after the first sync");
    let used_pct = draft
        .volume_total_gb
        .saturating_sub(draft.volume_free_gb)
        .saturating_mul(100)
        .checked_div(draft.volume_total_gb)
        .map(|v| v as u8)
        .unwrap_or(31);

    card()
        .child(
            view()
                .flex_row()
                .items_center()
                .gap(px(12.0))
                .child(
                    text("Local folder")
                        .w(px(120.0))
                        .text_size(px(12.5))
                        .text_color(p.muted),
                )
                .child(mono_text(draft.local_path.clone(), 12.0, p.fg).test_id("wizard-local-path"))
                .child(
                    app_button(
                        "Choose folder…",
                        BtnKind::Default,
                        true,
                        false,
                        None,
                        None,
                        move || {
                            let c = c_pick.clone();
                            let answer = pick_paths(
                                OpenDialog::folders()
                                    .title("Choose local folder")
                                    .button_label("Choose"),
                            );
                            spawn(async move {
                                if let Ok(Some(paths)) = answer.await
                                    && let Some(first) = paths.first()
                                {
                                    c.send(UiCommand::WizardSetLocalPath(
                                        first.to_string_lossy().into_owned(),
                                    ));
                                }
                            });
                        },
                    )
                    .test_id("wizard-choose-folder-btn"),
                ),
        )
        .child(
            view()
                .flex_row()
                .items_center()
                .gap(px(12.0))
                .child(
                    text("Volume")
                        .w(px(120.0))
                        .text_size(px(12.5))
                        .text_color(p.muted),
                )
                .child(
                    text(format!(
                        "{} · {} · {}",
                        draft.volume_name, vol_kind_str, draft.volume_fs
                    ))
                    .text_size(px(12.5))
                    .text_color(p.fg),
                ),
        )
        .child(
            view()
                .flex_row()
                .items_start()
                .gap(px(12.0))
                .child(
                    text("Space")
                        .w(px(120.0))
                        .text_size(px(12.5))
                        .text_color(p.muted),
                )
                .child(
                    view()
                        .flex_col()
                        .gap(px(4.0))
                        .child(capacity_meter(used_pct, false, space_line.clone(), 280.0))
                        .child(
                            text(space_line)
                                .text_size(px(12.0))
                                .text_color(p.muted)
                                .tabular_nums(),
                        ),
                ),
        )
        .child(checks_col)
        // Demo helper buttons to exercise the volume-root validation check cleanly without real /Volumes
        .child(
            view()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .pt(px(8.0))
                .border_t_1()
                .border_color(p.hair)
                .child(
                    text("Demo check preview:")
                        .text_size(px(11.5))
                        .text_color(p.muted),
                )
                .child(app_button(
                    "Valid folder",
                    BtnKind::Ghost,
                    true,
                    false,
                    None,
                    None,
                    move || {
                        c_good.send(UiCommand::WizardSetLocalPath(
                            "/Volumes/WorkSSD/Projects".to_owned(),
                        ));
                    },
                ))
                .child(app_button(
                    "Test volume root error",
                    BtnKind::Ghost,
                    true,
                    false,
                    None,
                    None,
                    move || {
                        c_bad.send(UiCommand::WizardSetLocalPath("/Volumes/WorkSSD".to_owned()));
                    },
                )),
        )
}

fn render_step_subfolders(
    ctrl: AppController,
    draft: &WizardDraft,
    pattern_input: Signal<String>,
) -> View {
    let p = Palette::current();
    // Fallback table of checkboxes with indentation per §5.4 (reliable keyboard focus & accessibility)
    let mut body = table_body().flex_col().w_full();
    for node in &draft.subfolders {
        let c = ctrl.clone();
        let node_id = node.id.clone();
        let indent = 10.0 + (node.depth as f32) * 20.0;
        let chevron = if node.depth == 0 {
            "▾ "
        } else if node.has_children {
            "▸ "
        } else {
            ""
        };
        let label_text = format!("{chevron}{}", node.name);

        body = body.child(
            table_row()
                .w_full()
                .items_center()
                .justify_between()
                .py(px(7.0))
                .pr(px(12.0))
                .pl(px(indent))
                .border_t_1()
                .border_color(p.hair)
                .child(
                    table_cell().flex_1().child(
                        checkbox()
                            .checked(node.included)
                            .flex_row()
                            .items_center()
                            .gap(px(8.0))
                            .cursor_pointer()
                            .focus_visible(focus_ring)
                            .indicator_style({
                                let inc = node.included;
                                move |s| {
                                    s.w(px(16.0))
                                        .h(px(16.0))
                                        .rounded(px(4.0))
                                        .border_1()
                                        .border_color(if inc { p.primary } else { p.border })
                                        .bg(if inc { p.primary } else { p.bg })
                                        .items_center()
                                        .justify_center()
                                }
                            })
                            .indicator(if node.included {
                                icon(IconKind::Check, 11.0, p.primary_fg).into_element()
                            } else {
                                view().into_element()
                            })
                            .on_change(move |_: &bool| {
                                c.send(UiCommand::WizardToggleSubfolder(node_id.clone()));
                            })
                            .child(
                                view()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(6.0))
                                    .child(text(label_text).text_size(px(13.0)).text_color(p.fg))
                                    .child(if !node.included {
                                        text("· cloud only")
                                            .text_size(px(11.5))
                                            .text_color(p.muted)
                                            .into_element()
                                    } else {
                                        view().into_element()
                                    }),
                            ),
                    ),
                )
                .child(
                    table_cell().w(px(80.0)).items_end().child(
                        text(node.size_label.clone())
                            .text_size(px(12.5))
                            .text_color(p.fg)
                            .tabular_nums(),
                    ),
                ),
        );
    }

    let tree_card = card().flex_1().p(px(6.0)).child(
        table()
            .accessibility_label(format!("Subfolders of {}", draft.selected_folder))
            .w_full()
            .child(
                table_header().child(
                    table_row()
                        .w_full()
                        .justify_between()
                        .px(px(10.0))
                        .py(px(6.0))
                        .child(table_head().flex_1().child(section_label("Subfolder")))
                        .child(
                            table_head()
                                .w(px(80.0))
                                .items_end()
                                .child(section_label("Size")),
                        ),
                ),
            )
            .child(body),
    );

    let included_gb = draft.included_gb();
    let total_gb = draft.total_gb();
    let remaining_tb = (draft.volume_free_gb as f64 - included_gb).max(0.0) / 1000.0;

    let c_toggle_def = ctrl.clone();
    let c_add_pat = ctrl.clone();
    let mut side_card = card()
        .w(px(276.0))
        .flex_none()
        .child(card_title("Will be on this Mac"))
        .child(
            view()
                .flex_row()
                .items_baseline()
                .gap(px(6.0))
                .child(
                    text(format!("{included_gb:.1} GB"))
                        .text_size(px(16.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(p.fg)
                        .tabular_nums(),
                )
                .child(
                    text(format!("of {total_gb:.1} GB"))
                        .text_size(px(12.5))
                        .text_color(p.muted)
                        .tabular_nums(),
                ),
        )
        .child(
            text(format!(
                "{} keeps about {remaining_tb:.2} TB free",
                draft.volume_name
            ))
            .text_size(px(12.0))
            .text_color(p.muted),
        )
        .child(view().pt(px(4.0)).child(section_label("Always excluded")))
        .child(
            view()
                .flex_row()
                .flex_wrap()
                .items_center()
                .gap(px(6.0))
                .child(
                    text("macOS metadata and temporary files")
                        .text_size(px(12.0))
                        .text_color(p.fg),
                )
                .child(link_button(
                    if draft.show_default_exclusions {
                        "Hide list"
                    } else {
                        "Show list"
                    },
                    12.0,
                    move || c_toggle_def.send(UiCommand::WizardToggleDefaultExclusions),
                )),
        );

    if draft.show_default_exclusions {
        let mut list = view()
            .flex_col()
            .gap(px(3.0))
            .bg(p.sunk)
            .rounded(px(6.0))
            .p(px(8.0));
        for entry in DEFAULT_EXCLUSIONS {
            list = list.child(mono_text(entry.pattern, 11.0, p.muted));
        }
        side_card = side_card.child(list);
    }

    side_card = side_card
        .child(
            view()
                .pt(px(4.0))
                .child(section_label("Pattern (optional)")),
        )
        .child(
            view()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .child(
                    input()
                        .flex_1()
                        .min_h(px(28.0))
                        .px(px(8.0))
                        .py(px(4.0))
                        .rounded(px(7.0))
                        .bg(p.bg)
                        .border_1()
                        .border_color(p.border)
                        .text_size(px(12.0))
                        .text_color(p.fg)
                        .placeholder("e.g. *.lrdata/")
                        .value(move || pattern_input.get())
                        .on_change(move |val: &str| pattern_input.set(val.to_owned())),
                )
                .child(app_button(
                    "Add",
                    BtnKind::Default,
                    true,
                    false,
                    None,
                    None,
                    move || {
                        let pat = pattern_input.get_untracked();
                        c_add_pat.send(UiCommand::WizardAddPattern(pat));
                        pattern_input.set(String::new());
                    },
                )),
        );

    if !draft.custom_patterns.is_empty() {
        side_card = side_card.child(mono_text(
            format!("Added: {}", draft.custom_patterns.join(", ")),
            11.0,
            p.muted,
        ));
    }

    let found_card = card()
        .flex_1()
        .child(card_title("What was found"))
        .child(check_row(
            IconKind::Check,
            p.ok,
            "No duplicate or clashing names.",
        ))
        .child(check_row(
            IconKind::Check,
            p.ok,
            "No symbolic links in the local folder.",
        ))
        .child(check_row(
            IconKind::Alert,
            p.attn,
            "Google Docs, Sheets, Slides and Drive shortcuts are skipped. FolderSync can't list all of them, so it can't say how many there are.",
        ));

    let supported_card = card()
        .flex_1()
        .child(card_title("Supported files"))
        .child(bullet(
            "Ordinary files are synced. Symbolic links are not followed.",
        ))
        .child(bullet("Empty folders are not copied yet."))
        .child(bullet(
            "Finder tags, extended attributes and permissions are not kept.",
        ))
        .child(bullet(
            "Don't sync open databases, virtual machine disks or app libraries.",
        ));

    view()
        .flex_col()
        .w_full()
        .gap(px(14.0))
        .child(
            view()
                .flex_row()
                .items_start()
                .gap(px(16.0))
                .w_full()
                .child(tree_card)
                .child(side_card),
        )
        .child(
            view()
                .flex_row()
                .items_stretch()
                .gap(px(14.0))
                .w_full()
                .child(found_card)
                .child(supported_card),
        )
}

fn render_step_recovery(ctrl: AppController, draft: &WizardDraft) -> View {
    let p = Palette::current();
    let c_change = ctrl.clone();
    card()
        .child(
            view()
                .flex_row()
                .items_center()
                .gap(px(12.0))
                .child(
                    text("On this Mac")
                        .w(px(120.0))
                        .text_size(px(12.5))
                        .text_color(p.muted),
                )
                .child(mono_text(draft.local_recovery_path.clone(), 12.0, p.fg))
                .child(app_button(
                    "Change…",
                    BtnKind::Default,
                    true,
                    false,
                    None,
                    None,
                    move || {
                        let c = c_change.clone();
                        let answer = pick_paths(
                            OpenDialog::folders()
                                .title("Choose local recovery folder")
                                .button_label("Choose"),
                        );
                        spawn(async move {
                            if let Ok(Some(paths)) = answer.await
                                && let Some(first) = paths.first()
                            {
                                c.send(UiCommand::WizardSetLocalRecoveryPath(
                                    first.to_string_lossy().into_owned(),
                                ));
                            }
                        });
                    },
                )),
        )
        .child(
            view()
                .flex_row()
                .items_center()
                .gap(px(12.0))
                .child(
                    text("In the cloud")
                        .w(px(120.0))
                        .text_size(px(12.5))
                        .text_color(p.muted),
                )
                .child(mono_text(draft.cloud_recovery_path.clone(), 12.0, p.fg)),
        )
        .child(
            view().pt(px(8.0)).flex_col().gap(px(6.0))
                .child(check_row(
                    IconKind::Check,
                    p.ok,
                    "Both are outside every synced folder, and both can be written.",
                ))
                .child(check_row(
                    IconKind::Alert,
                    p.attn,
                    &format!(
                        "The local recovery folder is on {}. If that drive fails, its recovery copies are lost too. Keep a separate backup of files you can't replace.",
                        draft.volume_name
                    ),
                )),
        )
}

fn render_step_review(ctrl: AppController, draft: &WizardDraft) -> View {
    let p = Palette::current();
    let cloud_path = format!("{}:Data/{}", draft.selected_account, draft.selected_folder);
    let (cloud_files_line, local_files_line) = match draft.review_scenario {
        ReviewScenario::CloudToLocal => (
            format!(
                "Contains files: Yes · about 2,410 files, {:.1} GB",
                draft.total_gb()
            ),
            "Contains files: No · empty".to_owned(),
        ),
        ReviewScenario::LocalToCloud => (
            "Contains files: No · empty (new folder)".to_owned(),
            "Contains files: Yes · about 812 files, 4.2 GB".to_owned(),
        ),
        ReviewScenario::BothPopulated => (
            "Contains files: Yes · about 2,410 files".to_owned(),
            "Contains files: Yes · about 1,020 files".to_owned(),
        ),
        ReviewScenario::BothEmpty => (
            "Contains files: No · empty".to_owned(),
            "Contains files: No · empty".to_owned(),
        ),
    };

    let sides = view()
        .flex_row()
        .gap(px(14.0))
        .w_full()
        .child(
            card()
                .flex_1()
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .child(icon(IconKind::Cloud, 16.0, p.muted))
                        .child(card_title("Cloud")),
                )
                .child(mono_text(cloud_path.clone(), 12.0, p.fg))
                .child(
                    text(cloud_files_line)
                        .text_size(px(12.5))
                        .text_color(p.fg)
                        .tabular_nums(),
                ),
        )
        .child(
            card()
                .flex_1()
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .child(icon(IconKind::DriveExternal, 16.0, p.muted))
                        .child(card_title("This Mac")),
                )
                .child(mono_text(draft.local_path.clone(), 12.0, p.fg))
                .child(
                    text(local_files_line)
                        .text_size(px(12.5))
                        .text_color(p.fg)
                        .tabular_nums(),
                ),
        );

    let excluded = draft.excluded_names();
    let left_out_note = if excluded.is_empty() {
        String::new()
    } else {
        format!(" ({} left out)", excluded.join(", "))
    };

    let direction_card = match draft.review_scenario {
        ReviewScenario::CloudToLocal => view()
            .bg(p.run_bg)
            .border_1()
            .border_color(p.run_bd)
            .rounded(px(12.0))
            .p(px(16.0))
            .flex_col()
            .gap(px(6.0))
            .child(
                view()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .child(icon(IconKind::Swap, 16.0, p.run))
                    .child(
                        text("First sync: download from the cloud")
                            .text_size(px(14.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.run),
                    ),
            )
            .child(
                text(format!(
                    "The local folder is filled from Data/{}. Nothing in the cloud is changed by the first sync. After that, changes on either side sync both ways.",
                    draft.selected_folder
                ))
                .text_size(px(13.0))
                .text_color(p.fg),
            )
            .child(
                text(format!(
                    "Preview: about 1,980 files, {:.1} GB{left_out_note}. This is an estimate; files can change before the sync starts.",
                    draft.included_gb()
                ))
                .text_size(px(12.0))
                .text_color(p.muted),
            ),
        ReviewScenario::LocalToCloud => view()
            .bg(p.run_bg)
            .border_1()
            .border_color(p.run_bd)
            .rounded(px(12.0))
            .p(px(16.0))
            .flex_col()
            .gap(px(6.0))
            .child(
                view()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .child(icon(IconKind::Swap, 16.0, p.run))
                    .child(
                        text("First sync: upload to the cloud")
                            .text_size(px(14.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.run),
                    ),
            )
            .child(
                text(format!(
                    "Data/{} is filled from the local folder. Local files are not changed by the first sync.",
                    draft.selected_folder
                ))
                .text_size(px(13.0))
                .text_color(p.fg),
            )
            .child(
                text("A marker file is added to both sides so other computers can join. This is an estimate; files can change before the sync starts.")
                    .text_size(px(12.0))
                    .text_color(p.muted),
            ),
        ReviewScenario::BothPopulated => {
            let c_choose = ctrl.clone();
            let lp_rev = draft.local_path.clone();
            view()
                .test_id("review-both-populated-blocked")
                .role(AccessibilityRole::Alert)
                .bg(p.card)
                .border_1()
                .border_color(p.err_bd)
                .border_l(px(4.0))
                .border_l_color(p.err_bd)
                .rounded(px(12.0))
                .p(px(16.0))
                .flex_col()
                .gap(px(8.0))
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .child(status_pill(IconKind::Octagon, "Can't start", Tone::Err))
                        .child(
                            text("Both sides already contain files")
                                .text_size(px(14.0))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(p.fg),
                        ),
                )
                .child(
                    text("FolderSync can't combine two existing folders in this version. Choose an empty local folder. To keep the existing local files, move them somewhere outside FolderSync first.")
                        .text_size(px(13.0))
                        .text_color(p.fg),
                )
                .child(
                    view()
                        .flex_row()
                        .gap(px(8.0))
                        .child(app_button(
                            "Choose another folder…",
                            BtnKind::Default,
                            true,
                            false,
                            None,
                            None,
                            move || c_choose.send(UiCommand::WizardSetStep(WizardStep::Location)),
                        ))
                        .child(app_button(
                            "Reveal current folder",
                            BtnKind::Default,
                            true,
                            false,
                            None,
                            None,
                            move || {
                                drop(reveal_path(lp_rev.clone()));
                            },
                        )),
                )
        }
        ReviewScenario::BothEmpty => card()
            .child(card_title("Both folders are empty"))
            .child(
                text("Nothing to copy. FolderSync adds the marker files and starts syncing.")
                    .text_size(px(13.0))
                    .text_color(p.fg),
            ),
    };

    let rec_local = draft.local_recovery_path.clone();
    let not_synced_text = if excluded.is_empty() {
        "macOS metadata and temporary files".to_owned()
    } else {
        format!(
            "{} · macOS metadata and temporary files",
            excluded
                .iter()
                .map(|n| format!("{}/{n}", draft.selected_folder))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };

    let details = card()
        .child(review_kv(
            "Recovery locations",
            view()
                .flex_col()
                .gap(px(4.0))
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .child(mono_text(draft.local_recovery_path.clone(), 11.5, p.fg))
                        .child(text("·").text_size(px(12.0)).text_color(p.muted))
                        .child(link_button("Reveal", 12.0, move || {
                            drop(reveal_path(rec_local.clone()));
                        })),
                )
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .child(mono_text(draft.cloud_recovery_path.clone(), 11.5, p.fg))
                        .child(text("·").text_size(px(12.0)).text_color(p.muted))
                        .child(link_button("Open", 12.0, move || {
                            drop(open_url("https://drive.google.com"));
                        })),
                )
                .into_element(),
        ))
        .child(review_kv(
            "Not synced",
            text(not_synced_text)
                .text_size(px(12.5))
                .text_color(p.fg)
                .into_element(),
        ))
        .child(review_kv(
            "Access marker",
            text("Another computer already uses this folder. FolderSync reuses its marker file and adds the same marker locally.")
                .text_size(px(12.5))
                .text_color(p.fg)
                .into_element(),
        ))
        .child(review_kv(
            "Protection",
            text(format!(
                "Deletion limit: {DEFAULT_MAX_DELETE_PERCENT}% of tracked files · Keep both versions on conflict · Keep recovery copies"
            ))
            .text_size(px(12.5))
            .text_color(p.fg)
            .into_element(),
        ))
        .child(review_kv(
            "Automatic sync",
            text("Starts only after the first sync is checked.")
                .text_size(px(12.5))
                .text_color(p.fg)
                .into_element(),
        ));

    // Demo scenario switcher for Review variants (download, upload, both-populated, both-empty)
    let scenario_bar = {
        let make_btn = |label: &'static str, sc: ReviewScenario, tid: &'static str| {
            let c = ctrl.clone();
            app_button(
                label,
                if draft.review_scenario == sc {
                    BtnKind::Primary
                } else {
                    BtnKind::Default
                },
                true,
                false,
                None,
                None,
                move || c.send(UiCommand::WizardSetReviewScenario(sc)),
            )
            .test_id(tid)
        };
        view()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap(px(6.0))
            .child(
                text("Demo review variant:")
                    .text_size(px(11.5))
                    .text_color(p.muted),
            )
            .child(make_btn(
                "Download",
                ReviewScenario::CloudToLocal,
                "review-variant-download",
            ))
            .child(make_btn(
                "Upload",
                ReviewScenario::LocalToCloud,
                "review-variant-upload",
            ))
            .child(make_btn(
                "Both populated (blocked)",
                ReviewScenario::BothPopulated,
                "review-variant-both-populated",
            ))
            .child(make_btn(
                "Both empty",
                ReviewScenario::BothEmpty,
                "review-variant-both-empty",
            ))
    };

    let mut root = view()
        .flex_col()
        .gap(px(14.0))
        .w_full()
        .child(scenario_bar)
        .child(sides)
        .child(direction_card)
        .child(details);

    if draft.review_scenario != ReviewScenario::BothPopulated {
        let c_chk = ctrl.clone();
        let checked = draft.no_other_sync_confirmed;
        root = root.child(
            card()
                .child(
                    checkbox()
                        .test_id("wizard-no-other-sync-checkbox")
                        .checked(checked)
                        .flex_row()
                        .items_start()
                        .gap(px(10.0))
                        .cursor_pointer()
                        .focus_visible(focus_ring)
                        .indicator_style(move |s| {
                            s.w(px(16.0))
                                .h(px(16.0))
                                .rounded(px(4.0))
                                .border_1()
                                .border_color(if checked { p.primary } else { p.border })
                                .bg(if checked { p.primary } else { p.bg })
                                .items_center()
                                .justify_center()
                        })
                        .indicator(if checked {
                            icon(IconKind::Check, 11.0, p.primary_fg).into_element()
                        } else {
                            view().into_element()
                        })
                        .on_change(move |next: &bool| {
                            c_chk.send(UiCommand::WizardSetNoOtherSyncConfirmed(*next));
                        })
                        .child(
                            text(format!(
                                "No other sync app manages {}. If one does, stop it for this folder first.",
                                draft.local_path
                            ))
                            .flex_1()
                            .text_size(px(13.0))
                            .text_color(p.fg),
                        ),
                )
                .child(
                    text(format!(
                        "If you can, ask others not to change Data/{} until the first sync finishes.",
                        draft.selected_folder
                    ))
                    .pl(px(26.0))
                    .text_size(px(12.0))
                    .text_color(p.muted),
                ),
        );
    }

    root
}

fn render_first_sync(ctrl: AppController, draft: &WizardDraft) -> View {
    let p = Palette::current();
    let c_stop = ctrl.clone();
    let c_done = ctrl.clone();
    let elapsed = format_duration(draft.first_sync_elapsed_secs);
    view()
        .flex_col()
        .w_full()
        .gap(px(14.0))
        .child(
            view()
                .flex_row()
                .items_end()
                .justify_between()
                .child(
                    view()
                        .flex_col()
                        .gap(px(3.0))
                        .child(page_title(format!("Setting up “{}”", draft.selected_folder)))
                        .child(
                            text("You can close this window. The first sync continues while FolderSync runs.")
                                .text_size(px(13.0))
                                .text_color(p.muted),
                        ),
                )
                .child(app_button(
                    "Stop",
                    BtnKind::Default,
                    false,
                    false,
                    Some(IconKind::Stop),
                    None,
                    move || c_stop.send(UiCommand::WizardSetStep(WizardStep::Review)),
                )),
        )
        .child(
            card()
                .child(check_row(
                    IconKind::Check,
                    p.ok,
                    "Checked account, folders and volume again",
                ))
                .child(check_row(IconKind::Check, p.ok, "Marker files in place"))
                .child(
                    view()
                        .flex_row()
                        .items_start()
                        .gap(px(10.0))
                        .py(px(4.0))
                        .child(icon(IconKind::Sync, 16.0, p.run))
                        .child(
                            view()
                                .flex_1()
                                .max_w(px(460.0))
                                .flex_col()
                                .gap(px(6.0))
                                .child(
                                    text(format!(
                                        "Downloading · 4.7 GB of about {:.1} GB · elapsed {elapsed}",
                                        draft.included_gb()
                                    ))
                                    .text_size(px(13.0))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(p.fg)
                                    .tabular_nums(),
                                )
                                .child(progress_bar(
                                    Some(f32::from(draft.first_sync_percent)),
                                    6.0,
                                    format!("Downloading {}", draft.selected_folder),
                                )),
                        ),
                )
                .child(check_row(IconKind::Dash, p.muted, "Check the result"))
                .child(check_row(IconKind::Dash, p.muted, "Turn on automatic sync"))
                .child(
                    view().pt(px(8.0)).child(app_button(
                        "Back to Overview",
                        BtnKind::Default,
                        true,
                        false,
                        None,
                        None,
                        move || c_done.navigate(ScreenRoute::Overview),
                    )),
                ),
        )
}

fn render_wizard_footer(ctrl: AppController, draft: &WizardDraft, skip_account: bool) -> View {
    let p = Palette::current();
    let c_cancel = ctrl.clone();
    let mut row = view()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .pt(px(14.0))
        .border_t_1()
        .border_color(p.hair)
        .child(app_button(
            "Cancel",
            BtnKind::Default,
            false,
            false,
            None,
            None,
            move || c_cancel.navigate(ScreenRoute::Overview),
        ))
        .child(view().flex_1());

    if let Some(prev) = draft.step.prev(skip_account) {
        let c_back = ctrl.clone();
        row = row.child(app_button(
            "Back",
            BtnKind::Default,
            false,
            false,
            None,
            None,
            move || c_back.send(UiCommand::WizardSetStep(prev)),
        ));
    }

    if draft.step == WizardStep::Review {
        // Blocked variant (BothPopulated): NO Start first sync button at all (§5.4, plan §9)!
        if draft.review_scenario != ReviewScenario::BothPopulated {
            let c_start = ctrl.clone();
            let can_start = draft.can_start_first_sync();
            row = row.child(
                app_button(
                    "Start first sync",
                    BtnKind::Primary,
                    false,
                    !can_start,
                    None,
                    (!can_start).then_some("Confirm the checkbox first"),
                    move || c_start.send(UiCommand::WizardStartFirstSync),
                )
                .test_id("wizard-start-first-sync-btn"),
            );
        }
    } else if let Some(next) = draft.step.next() {
        let c_next = ctrl.clone();
        let next_disabled = draft.step == WizardStep::Location && !draft.location_valid();
        row = row.child(
            app_button(
                "Continue",
                BtnKind::Primary,
                false,
                next_disabled,
                None,
                next_disabled.then_some("Fix the location check first"),
                move || c_next.send(UiCommand::WizardSetStep(next)),
            )
            .test_id("wizard-continue-btn"),
        );
    }

    row
}

fn radio_circle(checked: bool, disabled: bool) -> View {
    let p = Palette::current();
    let border = if checked && !disabled {
        p.primary
    } else {
        p.border
    };
    view()
        .w(px(16.0))
        .h(px(16.0))
        .rounded_full()
        .border_1()
        .border_color(border)
        .bg(p.bg)
        .items_center()
        .justify_center()
        .flex_none()
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

fn check_row(icon_kind: IconKind, tint: AdaptiveColor, message: &str) -> View {
    let p = Palette::current();
    view()
        .flex_row()
        .items_start()
        .gap(px(8.0))
        .py(px(2.0))
        .child(icon(icon_kind, 15.0, tint))
        .child(
            text(message.to_owned())
                .flex_1()
                .text_size(px(12.5))
                .text_color(p.fg),
        )
}

fn bullet(line: &str) -> View {
    let p = Palette::current();
    view()
        .flex_row()
        .items_start()
        .gap(px(6.0))
        .child(text("•").text_size(px(12.0)).text_color(p.muted))
        .child(
            text(line.to_owned())
                .flex_1()
                .text_size(px(12.0))
                .text_color(p.fg),
        )
}

fn review_kv(label: &str, value: AnyElement) -> View {
    let p = Palette::current();
    view()
        .flex_row()
        .items_start()
        .gap(px(12.0))
        .py(px(2.0))
        .child(
            text(label.to_owned())
                .w(px(136.0))
                .flex_none()
                .text_size(px(12.5))
                .text_color(p.muted),
        )
        .child(view().flex_1().child(value))
}
