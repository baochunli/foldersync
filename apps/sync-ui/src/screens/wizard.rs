//! 6-step Add-mapping flow (Account, Folder, Location, Subfolders, Recovery, Review)
//! plus the post-start First sync checklist (`ui-spec.md` §5.4).

use nsh_base::{
    button, checkbox, input, radio, radio_group, table, table_body, table_cell, table_head,
    table_header, table_row,
};
use nsh_core::{file_dialog::OpenDialog, prelude::*};
use sync_core::{DEFAULT_EXCLUSIONS, DEFAULT_MAX_DELETE_PERCENT, VolumeKind};
use sync_manager::ArchiveState;

use crate::contract::{
    AccountStatus, FolderSize, IconKind, ReviewScenario, ScreenRoute, Tone, UiCommand, UiSnapshot,
    WizardDraft, WizardStep, archive_percent, archive_progress_label, format_bytes,
    format_duration,
};
use crate::screens::AppController;
use crate::theme::{
    BLOCK_GAP, BtnKind, KV_ROW_GAP, Palette, S_CAPTION, S_CARD_TITLE, S_COMPACT, S_QUESTION,
    S_SMALL, S_TINY, W_BODY, W_STRONG, W_TITLE, accented_card, app_button, capacity_meter, card,
    card_title, emphasized_line, focus_ring, icon, kv_row, link_button, mono_text, page_title,
    progress_bar, section_label, status_pill,
};

/// Renders the Add-mapping wizard inside the main content area (sidebar stays visible).
pub fn wizard_view(ctrl: AppController) -> impl IntoElement {
    let pattern_input = signal(String::new());
    let new_folder_open = signal(false);
    let new_folder_name = signal(String::new());
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
            return render_first_sync(ctrl.clone(), &snap).into_element();
        }

        if draft.changing_subfolders {
            return render_filter_change(ctrl.clone(), draft, pattern_input).into_element();
        }

        let (title, subtitle) = match draft.step {
            WizardStep::Account => ("Add mapping".to_owned(), None),
            WizardStep::Folder => (
                "Choose a folder".to_owned(),
                Some(if snap.is_demo_mode {
                    format!(
                        "Folders in {}:Data · about 10.4 TB in total. Pick one; add more afterwards.",
                        draft.selected_account
                    )
                } else if snap.accounts.iter().any(|a| a.id == draft.selected_account && a.is_drive) {
                    format!("Folders in {}:Data. Pick one; add more afterwards.", draft.selected_account)
                } else {
                    "Choose a folder from the local test account.".to_owned()
                }),
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
        let mut header = view()
            .window_drag(true)
            .flex_col()
            .gap(px(3.0))
            .child(page_title(title));
        if let Some(sub) = subtitle {
            header = header.child(text(sub).text_color(p.muted));
        }

        let body = match draft.step {
            WizardStep::Account => render_step_account(ctrl.clone(), &snap),
            WizardStep::Folder => {
                render_step_folder(ctrl.clone(), &snap, new_folder_open, new_folder_name)
            }
            WizardStep::Location => render_step_location(ctrl.clone(), draft, snap.is_demo_mode),
            WizardStep::Subfolders => render_step_subfolders(ctrl.clone(), draft, pattern_input),
            WizardStep::Recovery => render_step_recovery(ctrl.clone(), draft),
            WizardStep::Review => render_step_review(ctrl.clone(), draft, snap.is_demo_mode),
            WizardStep::FirstSync => render_first_sync(ctrl.clone(), &snap),
        };

        let account_ready = snap
            .accounts
            .iter()
            .any(|a| a.id == draft.selected_account && a.status == AccountStatus::Connected);
        let folder_ready = snap.is_demo_mode || draft.has_cloud_folder();
        let footer = render_wizard_footer(
            ctrl.clone(),
            draft,
            skip_account,
            account_ready,
            folder_ready,
        );

        view()
            .flex_col()
            .w_full()
            .child(
                view()
                    .flex_col()
                    .items_start()
                    .gap(px(8.0))
                    .child(link_button("‹ Overview", S_SMALL, {
                        let c = ctrl.clone();
                        move || c.navigate(ScreenRoute::Overview)
                    }))
                    .child(header.w_full())
                    .mb(px(16.0)),
            )
            .child(step_indicator(ctrl.clone(), draft))
            .child(body)
            .child(footer)
            .into_element()
    })
}

fn render_filter_change(
    ctrl: AppController,
    draft: &WizardDraft,
    pattern_input: Signal<String>,
) -> View {
    let Some(mapping_id) = draft.editing_mapping_id.clone() else {
        return view();
    };
    let p = Palette::current();
    let reviewing = draft.step == WizardStep::Review;
    let mut content = view()
        .w_full()
        .flex_col()
        .gap(px(BLOCK_GAP))
        .child(page_title(if reviewing {
            "Review subfolder changes"
        } else {
            "Change subfolders"
        }));
    let c_back = ctrl.clone();
    let c_submit = ctrl.clone();
    let mut footer = view()
        .flex_row()
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
            move || c_back.navigate(ScreenRoute::MappingDetail(mapping_id.clone())),
        ))
        .child(view().flex_1());
    if let Some(review) = draft.filter_review.as_ref().filter(|_| reviewing) {
        let mut body = card().child(card_title("Changes for this mapping"));
        for (label, patterns) in [
            ("New exclusions", &review.added),
            ("Removed exclusions", &review.removed),
        ] {
            body = body.child(kv_row(
                label,
                text(if patterns.is_empty() {
                    "None".to_owned()
                } else {
                    patterns.join(", ")
                })
                .text_color(p.fg)
                .into_element(),
            ));
        }
        body = body.child(text("Previous sync setup is kept. Local copies of newly excluded files stay in place. Applying these changes starts a new first sync.").text_color(p.fg));
        let mut checks = card().child(card_title("Before first sync"));
        for check in &review.checks {
            checks = checks.child(check_row(
                if check.ok {
                    IconKind::Check
                } else {
                    IconKind::Octagon
                },
                if check.ok { p.ok } else { p.err },
                &check.message,
            ));
        }
        let can_apply = review.can_apply();
        let c_consent = ctrl.clone();
        if can_apply {
            let checked = draft.no_other_sync_confirmed;
            checks = checks.child(
                checkbox()
                    .checked(draft.no_other_sync_confirmed)
                    .test_id("filter-change-consent")
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .focus_visible(focus_ring)
                    .on_change(move |checked: &bool| {
                        c_consent.send(UiCommand::WizardSetNoOtherSyncConfirmed(*checked))
                    })
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
                    .child(
                        text("I’ve reviewed the changes and want to start a new first sync.")
                            .text_color(p.fg),
                    ),
            );
        }
        footer = footer
            .child(app_button(
                "Back",
                BtnKind::Default,
                false,
                false,
                None,
                None,
                move || ctrl.send(UiCommand::WizardSetStep(WizardStep::Subfolders)),
            ))
            .child(
                app_button(
                    "Apply and start first sync",
                    BtnKind::Primary,
                    false,
                    !can_apply || !draft.no_other_sync_confirmed,
                    None,
                    None,
                    move || c_submit.send(UiCommand::ApplyFilterChange),
                )
                .test_id("filter-change-confirm"),
            );
        content = content.child(body).child(checks);
    } else {
        content = content.child(render_step_subfolders(ctrl, draft, pattern_input));
        footer = footer.child(
            app_button(
                "Review changes",
                BtnKind::Primary,
                false,
                false,
                None,
                None,
                move || c_submit.send(UiCommand::PreviewFilterChange),
            )
            .test_id("filter-change-preview"),
        );
    }
    content.child(footer)
}

fn step_indicator(ctrl: AppController, draft: &WizardDraft) -> View {
    let p = Palette::current();
    let current = draft.step;
    let mut row = view()
        .flex_row()
        .flex_wrap()
        .gap(px(6.0))
        .mb(px(18.0))
        .accessibility_description("Add mapping steps");

    for (step, label) in WizardStep::SIX_STEPS {
        let is_current = step == current;
        let is_done = step < current;
        let can_visit = draft.can_visit_step(step);
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
        let a11y_label = if is_current {
            format!("Step {} of 6: {label}, current step", step.number())
        } else if is_done {
            format!("Step {} of 6: {label}, completed", step.number())
        } else if !can_visit {
            format!(
                "Step {} of 6: {label}, unavailable until earlier checks pass",
                step.number()
            )
        } else {
            format!("Step {} of 6: {label}", step.number())
        };

        let pill = button()
            .test_id(&tid)
            .disabled(!can_visit)
            .accessibility_label(a11y_label)
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
            .disabled_style(|s| s.opacity(0.45))
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
                            .text_size(px(S_TINY))
                            .font_weight(W_TITLE)
                            .text_color(dot_fg),
                    ),
            )
            .child(
                text(label)
                    .text_size(px(S_SMALL))
                    .font_weight(if is_current { W_STRONG } else { W_BODY })
                    .text_color(pill_fg),
            );
        row = row.child(pill);
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
        let sub = if snap.is_demo_mode && !is_disabled {
            "Connected · access checked today at 14:30".to_owned()
        } else {
            acct.access_summary.clone()
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
                                .font_weight(W_TITLE)
                                .text_color(p.fg),
                        )
                        .child(text(sub).text_size(px(S_SMALL)).text_color(p.muted)),
                ),
        );
    }

    let c_conn = ctrl.clone();
    card()
        .child(
            text("Which Google account holds the folder?")
                .text_size(px(S_QUESTION))
                .font_weight(W_STRONG)
                .text_color(p.fg),
        )
        .child(group)
        .child(if snap.is_demo_mode {
            view()
                .pt(px(4.0))
                .child(app_button(
                    "Connect account…",
                    BtnKind::Default,
                    false,
                    false,
                    None,
                    None,
                    move || c_conn.navigate(ScreenRoute::Accounts),
                ))
                .into_element()
        } else {
            super::accounts::wizard_account_controls(ctrl).into_element()
        })
}

fn render_step_folder(
    ctrl: AppController,
    snap: &UiSnapshot,
    new_folder_open: Signal<bool>,
    new_folder_name: Signal<String>,
) -> View {
    let p = Palette::current();
    let selected_folder = &snap.wizard.selected_folder;
    let can_create = snap.is_demo_mode
        || snap.accounts.iter().any(|a| {
            a.id == snap.wizard.selected_account
                && a.is_drive
                && a.status == AccountStatus::Connected
        });

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
                            .font_weight(if is_selected { W_TITLE } else { W_BODY })
                            .text_color(if already_mapped { p.muted } else { p.fg }),
                    )
                    .child(if item.used_by_other_computer {
                        text("Used by another computer")
                            .text_size(px(S_CAPTION))
                            .text_color(p.muted)
                            .into_element()
                    } else {
                        view().into_element()
                    }),
            )
            .child(
                table_cell().w(px(86.0)).items_end().child(
                    text(match &item.size {
                        FolderSize::Unmeasured => "Not measured".to_owned(),
                        FolderSize::Calculating => "Calculating…".to_owned(),
                        FolderSize::Known { bytes, .. } => format_bytes(*bytes),
                        FolderSize::Failed { .. } => "Unavailable".to_owned(),
                    })
                    .text_size(px(S_COMPACT))
                    .font_weight(if is_selected { W_TITLE } else { W_BODY })
                    .text_color(if item.size.bytes().is_none() {
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
                    .text_size(px(S_SMALL))
                    .text_color(if already_mapped { p.muted } else { p.fg }),
                ),
            );

        if !already_mapped {
            inner = inner
                .test_id(&format!("wizard-folder-{name}"))
                .cursor_pointer()
                .on_press(move || c.send(UiCommand::WizardSelectFolder(name.clone())));
        }
        let row = table_row()
            .w_full()
            .border_t_1()
            .border_color(p.hair)
            .child(inner);
        body = body.child(row);
    }

    for skipped in &snap.skipped_cloud_folders {
        let reason = match skipped.reason {
            sync_manager::CollectionSkipReason::Duplicate => "Duplicate name",
            sync_manager::CollectionSkipReason::UnsupportedName => "Unsupported name",
        };
        body = body.child(
            table_row()
                .w_full()
                .border_t_1()
                .border_color(p.hair)
                .child(
                    table_cell()
                        .px(px(10.0))
                        .py(px(8.0))
                        .child(text(format!("{} · {reason}", skipped.name)).text_color(p.muted)),
                ),
        );
    }

    let mut folder_footer = view().flex_col().gap(px(8.0)).px(px(8.0)).pt(px(6.0));
    if let Some(error) = &snap.cloud_catalog_error {
        let c = ctrl.clone();
        folder_footer = folder_footer
            .child(text(error.clone()).text_color(p.err))
            .child(view().flex_row().child(app_button(
                "Try again",
                BtnKind::Default,
                true,
                false,
                None,
                None,
                move || c.send(UiCommand::WizardRefreshFolders),
            )));
    } else if snap.cloud_catalog.is_empty() && snap.skipped_cloud_folders.is_empty() {
        folder_footer = folder_footer.child(
            text(if snap.folder_size_pending {
                "Finishing the previous size check…"
            } else {
                "No folders found in Data."
            })
            .text_color(p.muted),
        );
    }

    if can_create {
        let c = ctrl.clone();
        let size_pending = snap.folder_size_pending;
        folder_footer = folder_footer.child(
            view()
                .flex_row()
                .items_center()
                .flex_wrap()
                .gap(px(8.0))
                .child(app_button(
                    "New folder in Data…",
                    BtnKind::Default,
                    true,
                    snap.folder_size_pending,
                    Some(IconKind::Plus),
                    snap.folder_size_pending.then_some("Finish or cancel the size check first"),
                    move || new_folder_open.set(true),
                ).test_id("wizard-new-folder-btn"))
                .child(
                    text("Only folders directly inside Data are offered, so collections never overlap.")
                        .text_size(px(S_CAPTION))
                        .text_color(p.muted),
                ),
        );
        if new_folder_open.get() {
            folder_footer = folder_footer.child(
                view()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        input()
                            .test_id("wizard-new-folder-name")
                            .placeholder("Folder name")
                            .flex_1()
                            .min_w(px(0.0))
                            .px(px(8.0))
                            .py(px(4.0))
                            .rounded(px(7.0))
                            .border_1()
                            .border_color(p.border)
                            .bg(p.bg)
                            .text_color(p.fg)
                            .focus(focus_ring)
                            .value(move || new_folder_name.get())
                            .on_change(move |value: &str| new_folder_name.set(value.to_owned())),
                    )
                    .child(
                        app_button(
                            "Cancel",
                            BtnKind::Default,
                            true,
                            false,
                            None,
                            None,
                            move || {
                                new_folder_open.set(false);
                                new_folder_name.set(String::new());
                            },
                        )
                        .test_id("wizard-cancel-new-folder-btn"),
                    )
                    .child(dynamic(move || {
                        let c = c.clone();
                        app_button(
                            "Create folder",
                            BtnKind::Primary,
                            true,
                            new_folder_name.get().trim().is_empty() || size_pending,
                            None,
                            None,
                            move || {
                                c.send(UiCommand::WizardCreateNewFolder(
                                    new_folder_name.get_untracked(),
                                ));
                                new_folder_open.set(false);
                                new_folder_name.set(String::new());
                            },
                        )
                        .test_id("wizard-create-folder-btn")
                        .into_element()
                    })),
            );
        }
    }

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
                            .child(
                                table_head()
                                    .w(px(124.0))
                                    .child(section_label("On this Mac")),
                            ),
                    ),
                )
                .child(body),
        )
        .child(folder_footer);

    let selected_size = snap
        .cloud_catalog
        .iter()
        .find(|item| item.name == *selected_folder)
        .map(|item| &item.size);
    let size_header = match selected_size {
        Some(FolderSize::Known { bytes, files, .. }) => {
            let size = format_bytes(*bytes);
            files.map_or(size.clone(), |count| {
                format!(
                    "{size} · {count} {}",
                    if count == 1 { "file" } else { "files" }
                )
            })
        }
        Some(FolderSize::Calculating) => "Calculating the selected folder’s size…".to_owned(),
        Some(FolderSize::Failed { cause }) => cause.clone(),
        Some(FolderSize::Unmeasured) => "Select the folder again to calculate its size.".to_owned(),
        None => "Choose a folder to compare its size with available space.".to_owned(),
    };

    let fit_title = if selected_folder.is_empty() {
        "Space on this Mac".to_owned()
    } else {
        format!("Where {selected_folder} fits")
    };
    let mut fit_card = card()
        .w(px(276.0))
        .flex_none()
        .child(card_title(fit_title))
        .child(text(size_header).text_size(px(S_SMALL)).text_color(p.muted));
    if let Some(FolderSize::Known {
        measured_at: Some(at),
        ..
    }) = selected_size
    {
        fit_card = fit_card.child(
            text(format!("Measured at {at}"))
                .text_size(px(S_CAPTION))
                .text_color(p.muted),
        );
    }
    if selected_size == Some(&FolderSize::Calculating) {
        let c = ctrl.clone();
        let mut actions = view().flex_row().flex_wrap().gap(px(8.0)).child(
            app_button(
                "Cancel calculation",
                BtnKind::Default,
                true,
                false,
                None,
                None,
                move || c.send(UiCommand::WizardCancelFolderSize),
            )
            .test_id("wizard-cancel-size-btn"),
        );
        if snap.is_demo_mode {
            let c = ctrl.clone();
            actions = actions.child(
                app_button(
                    "Show size failure (demo)",
                    BtnKind::Ghost,
                    true,
                    false,
                    None,
                    None,
                    move || c.send(UiCommand::DemoFailFolderSize),
                )
                .test_id("wizard-demo-fail-size-btn"),
            );
        }
        fit_card = fit_card.child(actions);
    }
    if snap.volumes.is_empty() {
        fit_card = fit_card
            .child(text("Choose a destination to check its free space.").text_color(p.muted));
    }

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
        } else if let Some((size, free)) = selected_size
            .and_then(FolderSize::bytes)
            .zip(vol.free_bytes)
        {
            if free < size {
                (format!("{} free · doesn't fit", vol.free_label), p.err)
            } else if vol.low_space {
                (format!("{} free · fits, low", vol.free_label), p.attn)
            } else {
                (format!("{} free · fits", vol.free_label), p.ok)
            }
        } else {
            (format!("{} free", vol.free_label), p.muted)
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
                                .text_size(px(S_COMPACT))
                                .text_color(p.fg),
                        ),
                )
                .child(
                    text(fit_text)
                        .text_size(px(S_SMALL))
                        .text_color(fit_color)
                        .tabular_nums(),
                ),
        );
    }

    fit_card = fit_card.child(
        text("You can leave out large subfolders in step 4.")
            .text_size(px(S_CAPTION))
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

fn render_step_location(ctrl: AppController, draft: &WizardDraft, is_demo: bool) -> View {
    let p = Palette::current();
    let c_pick = ctrl.clone();
    let c_bad = ctrl.clone();
    let c_good = ctrl.clone();
    let mut checks_col = view()
        .flex_col()
        .gap(px(6.0))
        .pt(px(8.0))
        .test_id("wizard-location-checks");
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
                        .text_size(px(S_COMPACT))
                        .text_color(if chk.ok { p.fg } else { p.err }),
                ),
        );
    }

    let mut loc_card = card().child(
        view()
            .flex_row()
            .items_center()
            .gap(px(12.0))
            .child(
                text("Local folder")
                    .w(px(120.0))
                    .text_size(px(S_COMPACT))
                    .text_color(p.muted),
            )
            .child(mono_text(draft.local_path.clone(), S_SMALL, p.fg).test_id("wizard-local-path"))
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
    );

    if draft.local_path.is_empty() {
        loc_card = loc_card
            .child(text("Choose a folder to see its disk and free space.").text_color(p.muted));
    } else if draft.location_check_pending {
        const CHECKING: &str = "Checking disk and free space…";
        loc_card = loc_card.child(
            view()
                .flex_col()
                .gap(px(6.0))
                .child(text(CHECKING).text_color(p.muted))
                .child(progress_bar(None, 4.0, CHECKING)),
        );
    } else {
        if let Some(free) = draft.volume_free_bytes {
            loc_card = loc_card.child(render_location_capacity(draft, free));
        }
        if !draft.location_checks.is_empty() {
            loc_card = loc_card.child(checks_col);
        }
    }

    if is_demo {
        let usual_path = format!("/Volumes/WorkSSD/{}", draft.selected_folder);
        let replacement = if draft.location_check_pending {
            draft.local_path.clone()
        } else if draft.review_scenario == ReviewScenario::BothPopulated
            && draft.local_path == usual_path
        {
            format!("{usual_path}-New")
        } else {
            usual_path
        };
        let pending_path = replacement.clone();
        loc_card = loc_card.child(
            view()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .pt(px(8.0))
                .border_t_1()
                .border_color(p.hair)
                .child(
                    text("Demo check preview:")
                        .text_size(px(S_CAPTION))
                        .text_color(p.muted),
                )
                .child(
                    app_button(
                        "Checking disk",
                        BtnKind::Ghost,
                        true,
                        false,
                        None,
                        None,
                        move || {
                            ctrl.send(UiCommand::DemoPreviewLocationCheck(pending_path.clone()));
                        },
                    )
                    .test_id("wizard-demo-checking-folder"),
                )
                .child(
                    app_button(
                        "Valid folder",
                        BtnKind::Ghost,
                        true,
                        false,
                        None,
                        None,
                        move || {
                            c_good.send(UiCommand::WizardSetLocalPath(replacement.clone()));
                        },
                    )
                    .test_id("wizard-demo-valid-folder"),
                )
                .child(
                    app_button(
                        "Test volume root error",
                        BtnKind::Ghost,
                        true,
                        false,
                        None,
                        None,
                        move || {
                            c_bad
                                .send(UiCommand::WizardSetLocalPath("/Volumes/WorkSSD".to_owned()));
                        },
                    )
                    .test_id("wizard-demo-invalid-root"),
                ),
        );
    }

    loc_card
}

fn render_location_capacity(draft: &WizardDraft, free: u64) -> View {
    let p = Palette::current();
    let vol_kind_str = match draft.volume_kind {
        VolumeKind::Internal => "internal",
        VolumeKind::External => "external",
    };

    let free_now = format_bytes(free);
    let space_line = match (draft.review_scenario, draft.cloud_total_bytes) {
        (ReviewScenario::CloudToLocal, Some(bytes)) if bytes > free => {
            format!(
                "{free_now} free · download needs about {}",
                format_bytes(bytes)
            )
        }
        (ReviewScenario::CloudToLocal, Some(bytes)) => {
            format!(
                "{free_now} free now · about {} free after the first sync",
                format_bytes(free - bytes)
            )
        }
        _ => format!("{free_now} free"),
    };
    let used_pct = draft
        .volume_total_gb
        .saturating_sub(draft.volume_free_gb)
        .saturating_mul(100)
        .checked_div(draft.volume_total_gb)
        .map(|v| v as u8)
        .unwrap_or(31);

    view()
        .flex_col()
        .gap(px(8.0))
        .test_id("wizard-location-capacity")
        .child(
            view()
                .flex_row()
                .items_center()
                .gap(px(12.0))
                .child(
                    text("Volume")
                        .w(px(120.0))
                        .text_size(px(S_COMPACT))
                        .text_color(p.muted),
                )
                .child(
                    text(format!(
                        "{} · {} · {}",
                        draft.volume_name, vol_kind_str, draft.volume_fs
                    ))
                    .text_size(px(S_COMPACT))
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
                        .text_size(px(S_COMPACT))
                        .text_color(p.muted),
                )
                .child(
                    view()
                        .flex_col()
                        .gap(px(4.0))
                        .child(capacity_meter(used_pct, false, space_line.clone(), 280.0))
                        .child(
                            text(space_line)
                                .text_size(px(S_SMALL))
                                .text_color(p.muted)
                                .tabular_nums(),
                        ),
                ),
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
                            .test_id(&format!("subfolder-{}", node.id))
                            .disabled(node.depth == 0 && !node.has_children)
                            .checked(if node.depth == 0 && node.has_children {
                                draft
                                    .subfolders
                                    .iter()
                                    .filter(|n| n.depth == 1)
                                    .all(|n| n.included)
                            } else {
                                node.included
                            })
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
                                    .child(text(label_text).text_color(p.fg))
                                    .child(if !node.included {
                                        text("· cloud only")
                                            .text_size(px(S_CAPTION))
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
                            .text_size(px(S_COMPACT))
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
    let total_opt = draft.total_gb_opt();
    let free_gb = draft
        .volume_free_bytes
        .map(|bytes| bytes as f64 / 1_000_000_000.0);
    let fit = match (free_gb, total_opt) {
        (Some(free), Some(_)) if included_gb > free => format!(
            "Free on {} · {free:.1} GB. The selection exceeds available space.",
            draft.volume_name
        ),
        (Some(free), Some(_)) => format!(
            "Free on {} · {free:.1} GB. The selection fits in the available space.",
            draft.volume_name
        ),
        (Some(free), None) => format!(
            "Free on {} · {free:.1} GB. Selection size unknown.",
            draft.volume_name
        ),
        (None, _) => format!("Free space on {} is unknown.", draft.volume_name),
    };

    let c_toggle_def = ctrl.clone();
    let c_add_pat = ctrl.clone();
    let size_headline = match total_opt {
        Some(total_gb) => view()
            .flex_row()
            .items_baseline()
            .gap(px(6.0))
            .child(
                text(format!("{included_gb:.1} GB"))
                    .text_size(px(S_QUESTION))
                    .font_weight(W_STRONG)
                    .text_color(p.fg)
                    .tabular_nums(),
            )
            .child(
                text(format!("of {total_gb:.1} GB"))
                    .text_size(px(S_COMPACT))
                    .text_color(p.muted)
                    .tabular_nums(),
            ),
        None => view().flex_row().child(
            text("Size measuring…")
                .text_size(px(S_CARD_TITLE))
                .font_weight(W_TITLE)
                .text_color(p.muted),
        ),
    };
    let mut side_card = card()
        .w(px(276.0))
        .flex_none()
        .child(card_title("Will be on this Mac"))
        .child(size_headline)
        .child(text(fit).text_size(px(S_SMALL)).text_color(p.muted))
        .child(view().pt(px(4.0)).child(section_label("Always excluded")))
        .child(
            view()
                .flex_row()
                .flex_wrap()
                .items_center()
                .gap(px(6.0))
                .child(
                    text("macOS metadata and temporary files")
                        .text_size(px(S_SMALL))
                        .text_color(p.fg),
                )
                .child(
                    link_button(
                        if draft.show_default_exclusions {
                            "Hide list"
                        } else {
                            "Show list"
                        },
                        S_SMALL,
                        move || c_toggle_def.send(UiCommand::WizardToggleDefaultExclusions),
                    )
                    .test_id("wizard-default-exclusions-toggle"),
                ),
        );

    if draft.show_default_exclusions {
        let mut list = view()
            .flex_col()
            .gap(px(3.0))
            .bg(p.sunk)
            .rounded(px(6.0))
            .p(px(8.0));
        for entry in DEFAULT_EXCLUSIONS {
            list = list.child(mono_text(entry.pattern, S_TINY, p.muted));
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
                        .test_id("wizard-pattern-input")
                        .flex_1()
                        .min_h(px(28.0))
                        .px(px(8.0))
                        .py(px(4.0))
                        .rounded(px(7.0))
                        .bg(p.bg)
                        .border_1()
                        .border_color(p.border)
                        .focus(focus_ring)
                        .focus_visible(focus_ring)
                        .text_size(px(S_SMALL))
                        .text_color(p.fg)
                        .placeholder("e.g. *.lrdata/")
                        .value(move || pattern_input.get())
                        .on_change(move |val: &str| pattern_input.set(val.to_owned())),
                )
                .child(
                    app_button(
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
                    )
                    .test_id("wizard-add-pattern-btn"),
                ),
        );

    for pattern in &draft.custom_patterns {
        let c_remove = ctrl.clone();
        let removed = pattern.clone();
        side_card = side_card.child(
            view()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(6.0))
                .child(mono_text(pattern.clone(), S_SMALL, p.fg).min_w(px(0.0)))
                .child(
                    app_button(
                        "Remove",
                        BtnKind::Ghost,
                        true,
                        false,
                        None,
                        None,
                        move || {
                            c_remove.send(UiCommand::WizardRemovePattern(removed.clone()));
                        },
                    )
                    .flex_none()
                    .accessibility_label(format!("Remove {pattern}"))
                    .test_id(&format!("remove-pattern-{pattern}")),
                ),
        );
    }

    let name_check = if draft.found_name_issues.is_empty() {
        check_row(IconKind::Check, p.ok, "No duplicate or clashing names.")
    } else {
        check_row(
            IconKind::Octagon,
            p.err,
            &format!(
                "{} duplicate or clashing names found: {}",
                draft.found_name_issues.len(),
                draft.found_name_issues.join(", ")
            ),
        )
    };
    let symlink_check = if draft.found_symlinks.is_empty() {
        check_row(IconKind::Check, p.ok, "No symbolic links found.")
    } else {
        check_row(
            IconKind::Alert,
            p.attn,
            &format!(
                "{} symbolic link(s) found and will be skipped: {}",
                draft.found_symlinks.len(),
                draft.found_symlinks.join(", ")
            ),
        )
    };

    let found_card = if draft.changing_subfolders {
        card()
            .flex_1()
            .child(card_title("Before first sync"))
            .child(
                text("Review changes to check both folders and the recovery locations.")
                    .text_color(p.muted),
            )
    } else {
        card()
        .flex_1()
        .child(card_title("What was found"))
        .child(name_check)
        .child(symlink_check)
        .child(check_row(
            IconKind::Alert,
            p.attn,
            "Google Docs, Sheets, Slides and Drive shortcuts are skipped. FolderSync can't list all of them, so it can't say how many there are.",
        ))
    };

    let supported_card = card()
        .flex_1()
        .child(card_title("Supported files"))
        .child(bullet(
            "Ordinary documents, source files, images, videos and archives are synced.",
        ))
        .child(bullet(
            "Symbolic links are not followed; hard-link relationships are not preserved.",
        ))
        .child(bullet(
            "Google Docs, Sheets, Slides and Drive shortcuts are skipped.",
        ))
        .child(bullet("Empty folders are not copied yet."))
        .child(bullet(
            "Finder tags, resource forks, ACLs, permissions and extended attributes are not preserved in v1.",
        ))
        .child(bullet(
            "Don't sync active databases, virtual machine images or application libraries, or folders managed by another sync provider.",
        ))
        .child(bullet(
            "Duplicate names or case/Unicode clashes stop the run for review.",
        ));

    let mut content = view().flex_col().w_full().gap(px(14.0)).child(
        view()
            .flex_row()
            .items_start()
            .gap(px(16.0))
            .w_full()
            .child(tree_card)
            .child(side_card),
    );
    if !draft.archive_suggestions.is_empty() {
        let mut suggestions = card().child(card_title("Upload as one archive"));
        for item in &draft.archive_suggestions {
            let sender = ctrl.clone();
            let folder = item.name.clone();
            let checked = !draft.declined_archives.contains(&folder);
            let last_changed = chrono::DateTime::from_timestamp(item.last_changed_unix_secs, 0)
                .map(|date| date.format("%b %-d, %Y").to_string())
                .unwrap_or_else(|| "Unknown".to_owned());
            let saved_minutes = item.estimated_seconds_saved.div_ceil(300) * 5;
            let saved = if saved_minutes < 60 {
                format!("{saved_minutes} min")
            } else {
                format!("{} hr {} min", saved_minutes / 60, saved_minutes % 60)
            };
            suggestions = suggestions.child(
                checkbox()
                    .test_id(&format!("wizard-archive-{}", item.name))
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
                    .on_change(move |_: &bool| {
                        sender.send(UiCommand::WizardToggleArchive(folder.clone()));
                    })
                    .child(
                        view()
                            .flex_col()
                            .gap(px(3.0))
                            .child(text(item.name.clone()).font_weight(W_TITLE))
                            .child(
                                text(archive_file_summary(item.file_count, item.bytes))
                                    .text_color(p.fg),
                            )
                            .child(
                                text(format!(
                                    "Last change {last_changed} · Time saved about {saved}"
                                ))
                                .text_size(px(S_SMALL))
                                .text_color(p.muted),
                            ),
                    ),
            );
        }
        content = content.child(suggestions);
    }
    content.child(
        view()
            .flex_row()
            .items_stretch()
            .gap(px(14.0))
            .w_full()
            .child(found_card)
            .child(supported_card),
    )
}

fn archive_file_summary(files: u64, bytes: u64) -> String {
    let plain = files.to_string();
    let mut count = String::new();
    for (index, digit) in plain.chars().enumerate() {
        if index > 0 && (plain.len() - index).is_multiple_of(3) {
            count.push(',');
        }
        count.push(digit);
    }
    format!("{count} files · {}", format_bytes(bytes))
}

fn render_step_recovery(ctrl: AppController, draft: &WizardDraft) -> View {
    let p = Palette::current();
    let c_change = ctrl.clone();
    let mut rec_checks = view().pt(px(8.0)).flex_col().gap(px(6.0));
    for chk in &draft.recovery_checks {
        rec_checks = rec_checks.child(check_row(
            if chk.ok {
                IconKind::Check
            } else {
                IconKind::Octagon
            },
            if chk.ok { p.ok } else { p.err },
            &chk.message,
        ));
    }
    rec_checks = rec_checks.child(check_row(
        IconKind::Alert,
        p.attn,
        &format!(
            "The local recovery folder is on {}. If that drive fails, its recovery copies are lost too. Keep a separate backup of files you can't replace.",
            draft.volume_name
        ),
    ));

    card()
        .child(
            view()
                .flex_row()
                .items_center()
                .gap(px(12.0))
                .child(
                    text("On this Mac")
                        .w(px(120.0))
                        .text_size(px(S_COMPACT))
                        .text_color(p.muted),
                )
                .child(mono_text(draft.local_recovery_path.clone(), S_SMALL, p.fg))
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
                        .text_size(px(S_COMPACT))
                        .text_color(p.muted),
                )
                .child(mono_text(draft.cloud_recovery_path.clone(), S_SMALL, p.fg)),
        )
        .child(rec_checks)
}

fn format_bytes_short(bytes: u64) -> String {
    const GB: f64 = 1_000_000_000.0;
    const MB: f64 = 1_000_000.0;
    const KB: f64 = 1_000.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

fn populated_side(file_count: Option<usize>, total_bytes: Option<u64>) -> Text {
    let details = match (file_count, total_bytes) {
        (Some(n), Some(b)) => format!("{n} files, {}", format_bytes_short(b)),
        (Some(n), None) => format!("{n} files"),
        (None, Some(b)) => format!("about {}", format_bytes_short(b)),
        (None, None) => "size measuring".to_owned(),
    };
    review_files_line("Yes", details)
}

fn review_files_line(contains: &'static str, details: impl Into<SharedString>) -> Text {
    text("Contains files: ")
        .child(text(contains).font_weight(W_TITLE))
        .child(text(" · "))
        .child(text(details.into()))
        .tabular_nums()
}

fn render_step_review(ctrl: AppController, draft: &WizardDraft, is_demo: bool) -> View {
    let p = Palette::current();
    let cloud_path = draft
        .remote_folder_path
        .clone()
        .unwrap_or_else(|| format!("{}:Data/{}", draft.selected_account, draft.selected_folder));
    let cloud_files = || populated_side(draft.cloud_file_count, draft.cloud_total_bytes);
    let local_files = || populated_side(draft.local_file_count, draft.local_total_bytes);
    let empty = || review_files_line("No", "empty");
    let (cloud_files_line, local_files_line) = match draft.review_scenario {
        ReviewScenario::CloudToLocal => (cloud_files(), empty()),
        ReviewScenario::LocalToCloud => {
            (review_files_line("No", "empty (new folder)"), local_files())
        }
        ReviewScenario::BothPopulated => (cloud_files(), local_files()),
        ReviewScenario::BothEmpty => (empty(), empty()),
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
                .child(mono_text(cloud_path.clone(), S_SMALL, p.fg))
                .child(cloud_files_line),
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
                .child(mono_text(draft.local_path.clone(), S_SMALL, p.fg))
                .child(local_files_line),
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
            .px(px(16.0))
            .py(px(14.0))
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
                            .text_size(px(S_CARD_TITLE))
                            .font_weight(W_TITLE)
                            .text_color(p.run),
                    ),
            )
            .child(
                text("The local folder is filled from ")
                    .child(mono_text(format!("Data/{}", draft.selected_folder), S_CAPTION, p.fg))
                    .child(text(". Nothing in the cloud is changed by the first sync. After that, changes on either side sync both ways.")),
            )
            .child(
                text(match (draft.cloud_file_count, draft.total_gb_opt()) {
                    (Some(n), Some(_)) => format!(
                        "Preview: about {n} files, {:.1} GB{left_out_note}. This is an estimate; files can change before the sync starts.",
                        draft.included_gb()
                    ),
                    (Some(n), None) => format!(
                        "Preview: about {n} files{left_out_note}. This is an estimate; files can change before the sync starts."
                    ),
                    (None, Some(_)) => format!(
                        "Preview: about {:.1} GB{left_out_note}. This is an estimate; files can change before the sync starts.",
                        draft.included_gb()
                    ),
                    (None, None) => format!(
                        "Preview: size is still measuring{left_out_note}. Files can change before the sync starts."
                    ),
                })
                .text_size(px(S_SMALL))
                .text_color(p.muted),
            ),
        ReviewScenario::LocalToCloud => view()
            .bg(p.run_bg)
            .border_1()
            .border_color(p.run_bd)
            .rounded(px(12.0))
            .px(px(16.0))
            .py(px(14.0))
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
                            .text_size(px(S_CARD_TITLE))
                            .font_weight(W_TITLE)
                            .text_color(p.run),
                    ),
            )
            .child(
                text("")
                    .child(mono_text(format!("Data/{}", draft.selected_folder), S_CAPTION, p.fg))
                    .child(text(" is filled from the local folder. Local files are not changed by the first sync.")),
            )
            .child(
                text("A marker file is added to both sides so other computers can join. This is an estimate; files can change before the sync starts.")
                    .text_size(px(S_SMALL))
                    .text_color(p.muted),
            ),
        ReviewScenario::BothPopulated => {
            let c_choose = ctrl.clone();
            let c_rev = ctrl.clone();
            let lp_rev = draft.local_path.clone();
            accented_card(p.err_bd)
                .test_id("review-both-populated-blocked")
                .role(AccessibilityRole::Alert)
                .border_color(p.err_bd)
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .child(status_pill(IconKind::Octagon, "Can't start", Tone::Err))
                        .child(
                            text("Both sides already contain files")
                                .text_size(px(S_CARD_TITLE))
                                .font_weight(W_TITLE)
                                .text_color(p.fg),
                        ),
                )
                .child(
                    text("FolderSync can't combine two existing folders in this version. Choose an empty local folder. To keep the existing local files, move them somewhere outside FolderSync first.")
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
                                c_rev.send(UiCommand::RevealPath(lp_rev.clone()));
                            },
                        )),
                )
        }
        ReviewScenario::BothEmpty => card()
            .child(card_title("Both folders are empty"))
            .child(
                text("Nothing to copy. FolderSync adds the marker files and starts syncing.")
                    .text_color(p.fg),
            ),
    };

    let rec_local = draft.local_recovery_path.clone();
    let rec_cloud = draft.cloud_recovery_path.clone();
    let c_rec_rev = ctrl.clone();
    let c_rec_open = ctrl.clone();
    // #4: Include custom exclusion patterns in Review "Not synced" summary.
    let mut not_synced_parts: Vec<String> = Vec::new();
    if !excluded.is_empty() {
        not_synced_parts.push(
            excluded
                .iter()
                .map(|n| format!("{}/{n}", draft.selected_folder))
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
    if !draft.custom_patterns.is_empty() {
        not_synced_parts.push(draft.custom_patterns.join(", "));
    }
    not_synced_parts.push("macOS metadata and temporary files".to_owned());
    let not_synced_text = not_synced_parts.join(" · ");

    let marker_copy = if draft.marker_reused {
        "Another computer already uses this folder. FolderSync reuses its marker file and adds the same marker locally."
    } else {
        "FolderSync creates a new access marker file in both folders so future runs confirm both sides before syncing."
    };

    let details = view().flex_col().gap(px(KV_ROW_GAP))
        .child(kv_row(
            "Recovery locations",
            view()
                .flex_col()
                .gap(px(4.0))
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .child(mono_text(draft.local_recovery_path.clone(), S_CAPTION, p.fg))
                        .child(text("·").text_size(px(S_SMALL)).text_color(p.muted))
                        .child(link_button("Reveal", S_SMALL, move || {
                            c_rec_rev.send(UiCommand::RevealPath(rec_local.clone()));
                        })),
                )
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .child(mono_text(draft.cloud_recovery_path.clone(), S_CAPTION, p.fg))
                        .child(text("·").text_size(px(S_SMALL)).text_color(p.muted))
                        .child(link_button("Open", S_SMALL, move || {
                            c_rec_open.send(UiCommand::OpenCloudUrl(rec_cloud.clone()));
                        })),
                )
                .into_element(),
        ))
        .child(kv_row(
            "Not synced",
            text(not_synced_text)
                .text_color(p.fg)
                .into_element(),
        ))
        .child(kv_row(
            "Access marker",
            text(marker_copy)
                .text_color(p.fg)
                .into_element(),
        ))
        .child(kv_row(
            "Protection",
            text(format!(
                "Deletion limit: {DEFAULT_MAX_DELETE_PERCENT}% of tracked files · Keep both versions on conflict · Keep recovery copies"
            ))
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
                    .text_size(px(S_CAPTION))
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

    let mut root = view().flex_col().gap(px(14.0)).w_full();
    if is_demo {
        root = root.child(scenario_bar);
    }
    root = root.child(sides).child(direction_card);
    if draft.selected_archives().next().is_some() {
        let mut archives = card().child(card_title("Upload as archives"));
        let (files, bytes) = draft.selected_archives().fold((0_u64, 0_u64), |acc, item| {
            (
                acc.0.saturating_add(item.file_count),
                acc.1.saturating_add(item.bytes),
            )
        });
        for item in draft.selected_archives() {
            archives = archives.child(text(format!("{}.zip", item.name)).text_color(p.fg));
        }
        archives = archives.child(text(archive_file_summary(files, bytes)).text_color(p.fg));
        if let Some(holding) = &draft.archive_holding_folder {
            archives = archives.child(text(format!(
                "Each folder is replaced by one .zip in this folder and in Google Drive, so every Mac gets the same .zip. The original folder is moved to {}; delete it there when you no longer need it.",
                holding.display()
            )).text_color(p.muted));
        }
        for error in &draft.archive_errors {
            archives = archives.child(check_row(IconKind::Octagon, p.err, error));
        }
        if draft.archive_holding_folder.is_none() && draft.archive_errors.is_empty() {
            archives = archives.child(check_row(
                IconKind::Octagon,
                p.err,
                "Could not check where the original folders will be kept.",
            ));
        }
        root = root.child(archives);
    }
    root = root.child(details);
    if draft.download_exceeds_free_space() {
        root = root.child(
            accented_card(p.err_bd)
                .child(card_title("The selection exceeds available space"))
                .child(
                    text("Leave out more subfolders or choose a volume with more free space.")
                        .text_color(p.fg),
                ),
        );
    }

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
                            .text_color(p.fg),
                        ),
                )
                .child(
                    text("If you can, ask others not to change ")
                    .child(mono_text(format!("Data/{}", draft.selected_folder), S_CAPTION, p.muted))
                    .child(text(" until the first sync finishes."))
                    .pl(px(26.0))
                    .text_size(px(S_SMALL))
                    .text_color(p.muted),
                ),
        );
    }

    root
}

fn render_first_sync(ctrl: AppController, snap: &UiSnapshot) -> View {
    let draft = &snap.wizard;
    let p = Palette::current();
    let c_stop = ctrl.clone();
    let c_done = ctrl.clone();
    let c_open_detail = ctrl.clone();
    let c_demo_finish = ctrl.clone();
    let elapsed = format_duration(draft.first_sync_elapsed_secs);
    let archives = || {
        snap.archives
            .iter()
            .filter(|entry| draft.first_sync_mapping_id.as_ref() == Some(&entry.mapping_id))
    };
    let archive_progress = archives()
        .find_map(|entry| match entry.state {
            ArchiveState::Creating { bytes, total } => Some((
                archive_progress_label(&entry.folder, bytes, total),
                archive_percent(bytes, total),
            )),
            _ => None,
        })
        .or_else(|| {
            archives()
                .find(|entry| entry.state == ArchiveState::Pending)
                .map(|entry| {
                    (
                        format!("Preparing to create archive {}", entry.folder),
                        None,
                    )
                })
        });

    // #4: Direction-accurate verb and progress text.
    let (verb, in_flight_line) = match draft.review_scenario {
        ReviewScenario::LocalToCloud => (
            "Uploading",
            format!("Uploading to the cloud · elapsed {elapsed}"),
        ),
        ReviewScenario::BothEmpty => (
            "Setting up",
            format!("Checking access markers · elapsed {elapsed}"),
        ),
        ReviewScenario::CloudToLocal | ReviewScenario::BothPopulated => (
            "Downloading",
            if draft.total_gb_opt().is_some() {
                format!(
                    "Downloading · about {:.1} GB · elapsed {elapsed}",
                    draft.included_gb()
                )
            } else {
                format!("Downloading · elapsed {elapsed}")
            },
        ),
    };

    let preflight_done = snap.is_demo_mode || draft.first_sync_preflight_done;
    let markers_ready = snap.is_demo_mode || draft.first_sync_markers_ready;
    let preflight_row = if preflight_done {
        check_row(
            IconKind::Check,
            p.ok,
            "Checked account, folders and volume again",
        )
    } else if draft.first_sync_error.is_some() {
        check_row(
            IconKind::Octagon,
            p.err,
            "Account, folder, and volume check failed",
        )
    } else {
        check_row(
            IconKind::Sync,
            p.run,
            "Checking account, folders and volume again…",
        )
    };
    let marker_row = if markers_ready {
        check_row(IconKind::Check, p.ok, "Marker files in place")
    } else if draft.first_sync_error.is_some() {
        check_row(IconKind::Dash, p.muted, "Marker files not verified")
    } else {
        check_row(IconKind::Dash, p.muted, "Verifying marker files")
    };

    let mut progress_card = card().child(preflight_row).child(marker_row);

    let completed_mapping = draft
        .first_sync_mapping_id
        .as_ref()
        .and_then(|id| snap.mapping(id));
    let finish_mode_done =
        if completed_mapping.is_some_and(|m| m.state.intent == sync_core::Intent::Paused) {
            "Automatic sync stays paused for this mapping"
        } else if snap.scheduling_paused {
            "Automatic sync is ready when scheduling resumes"
        } else {
            "Automatic sync is now on"
        };
    let finish_mode_pending = "Enable automatic sync after verification";

    if let Some(err) = &draft.first_sync_error {
        progress_card = progress_card.child(check_row(IconKind::Octagon, p.err, err));
    } else if draft.first_sync_completed {
        progress_card = progress_card
            .child(check_row(
                IconKind::Check,
                p.ok,
                &format!("{verb} finished"),
            ))
            .child(check_row(IconKind::Check, p.ok, "Checked the result"))
            .child(check_row(IconKind::Check, p.ok, finish_mode_done));
    } else {
        let active_line = if draft.first_sync_stopping && archive_progress.is_some() {
            text("Stopping archive creation…")
        } else if draft.first_sync_stopping {
            emphasized_line("Stopping", format!(" first sync… · elapsed {elapsed}"))
        } else if let Some((label, _)) = &archive_progress {
            text(label.clone())
        } else if !preflight_done {
            text(format!(
                "Checking folders before syncing… · elapsed {elapsed}"
            ))
        } else if let Some(rest) = in_flight_line.strip_prefix(verb) {
            emphasized_line(verb, rest.to_owned())
        } else {
            text(in_flight_line)
        };
        progress_card = progress_card
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
                            .child(active_line.tabular_nums())
                            .child(progress_bar(
                                if let Some((_, percent)) = &archive_progress {
                                    *percent
                                } else if snap.is_demo_mode {
                                    Some(f32::from(draft.first_sync_percent))
                                } else {
                                    None
                                },
                                6.0,
                                archive_progress.as_ref().map_or_else(
                                    || format!("{verb} {}", draft.selected_folder),
                                    |(label, _)| label.clone(),
                                ),
                            )),
                    ),
            )
            .child(check_row(IconKind::Dash, p.muted, "Check the result"))
            .child(check_row(IconKind::Dash, p.muted, finish_mode_pending));
    }

    let c_back_review = ctrl.clone();
    let mut footer_btns = view()
        .flex_row()
        .flex_wrap()
        .gap(px(8.0))
        .pt(px(8.0))
        .child(app_button(
            "Back to Overview",
            BtnKind::Default,
            true,
            false,
            None,
            None,
            move || c_done.navigate(ScreenRoute::Overview),
        ));

    if draft.first_sync_error.is_some() {
        let changed_mapping = draft
            .first_sync_mapping_id
            .clone()
            .filter(|_| draft.changing_subfolders);
        footer_btns = footer_btns.child(app_button(
            if changed_mapping.is_some() {
                "Open mapping"
            } else {
                "Back to Review"
            },
            BtnKind::Primary,
            true,
            false,
            None,
            None,
            move || {
                if let Some(id) = &changed_mapping {
                    c_back_review.navigate(ScreenRoute::MappingDetail(id.clone()));
                } else {
                    c_back_review.send(UiCommand::WizardSetStep(WizardStep::Review));
                }
            },
        ));
    }

    if draft.first_sync_completed
        && let Some(mid) = draft.first_sync_mapping_id.clone()
    {
        footer_btns = footer_btns.child(
            app_button(
                "Open mapping",
                BtnKind::Primary,
                true,
                false,
                None,
                None,
                move || c_open_detail.navigate(ScreenRoute::MappingDetail(mid.clone())),
            )
            .test_id("wizard-open-completed-mapping-btn"),
        );
    } else if snap.is_demo_mode && archive_progress.is_some() {
        footer_btns = footer_btns.child(
            app_button(
                "Finish archive (demo)",
                BtnKind::Primary,
                true,
                false,
                None,
                None,
                move || c_demo_finish.send(UiCommand::WizardCompleteDemoArchive),
            )
            .test_id("wizard-demo-complete-archive-btn"),
        );
    } else if snap.is_demo_mode {
        footer_btns = footer_btns.child(
            app_button(
                "Finish first sync (demo)",
                BtnKind::Primary,
                true,
                false,
                None,
                None,
                move || c_demo_finish.send(UiCommand::WizardCompleteDemoFirstSync),
            )
            .test_id("wizard-demo-complete-first-sync-btn"),
        );
    }

    progress_card = progress_card.child(footer_btns);

    let mut header_row = view().flex_row().items_end().justify_between().child(
        view()
            .flex_col()
            .gap(px(3.0))
            .child(page_title(format!(
                "First sync · {}",
                draft.selected_folder
            )))
            .child(
                text("You can close this window. The first sync continues while FolderSync runs.")
                    .text_color(p.muted),
            ),
    );

    if !draft.first_sync_completed && draft.first_sync_error.is_none() {
        let stopping = draft.first_sync_stopping;
        header_row = header_row.child(
            app_button(
                if stopping { "Stopping…" } else { "Stop" },
                BtnKind::Default,
                false,
                stopping,
                Some(IconKind::Stop),
                stopping.then_some("Waiting for the active first sync to stop"),
                move || c_stop.send(UiCommand::WizardStopFirstSync),
            )
            .test_id("wizard-stop-first-sync-btn"),
        );
    }

    view()
        .flex_col()
        .w_full()
        .gap(px(14.0))
        .child(header_row)
        .child(progress_card)
}

fn render_wizard_footer(
    ctrl: AppController,
    draft: &WizardDraft,
    skip_account: bool,
    account_ready: bool,
    folder_ready: bool,
) -> View {
    let p = Palette::current();
    let c_cancel = ctrl.clone();
    let c_save = ctrl.clone();
    let mut row = view()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .pt(px(14.0))
        .mt(px(18.0))
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
        .child(
            app_button(
                "Save draft",
                BtnKind::Default,
                false,
                false,
                None,
                None,
                move || {
                    c_save.send(UiCommand::WizardSaveDraft);
                    c_save.navigate(ScreenRoute::Overview);
                },
            )
            .test_id("wizard-save-draft-btn"),
        )
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
            let tip = if !draft.location_valid() {
                Some("Fix the failing location checks first")
            } else if !draft.recovery_valid() {
                Some("Fix the failing recovery location checks first")
            } else if draft.download_exceeds_free_space() {
                Some("Leave out more subfolders or choose another volume")
            } else if !draft.archives_reviewed() {
                Some("Resolve the archive checks first")
            } else if !draft.no_other_sync_confirmed {
                Some("Confirm the checkbox first")
            } else {
                None
            };
            row = row.child(
                app_button(
                    if draft.selected_archives().next().is_some() {
                        "Create archives and start first sync"
                    } else {
                        "Start first sync"
                    },
                    BtnKind::Primary,
                    false,
                    !can_start,
                    None,
                    tip,
                    move || c_start.send(UiCommand::WizardStartFirstSync),
                )
                .test_id("wizard-start-first-sync-btn"),
            );
        }
    } else if let Some(next) = draft.step.next() {
        let c_next = ctrl.clone();
        let next_disabled = !draft.can_visit_step(next)
            || (draft.step == WizardStep::Account && !account_ready)
            || (draft.step == WizardStep::Folder && !folder_ready);
        let tip = if draft.step == WizardStep::Location
            && !draft.local_path.is_empty()
            && !draft.location_check_pending
            && !draft.location_valid()
        {
            Some("Fix the location checks first")
        } else if draft.step == WizardStep::Recovery && !draft.recovery_valid() {
            Some("Fix the recovery folder checks first")
        } else {
            None
        };
        row = row.child(
            app_button(
                "Continue",
                BtnKind::Primary,
                false,
                next_disabled,
                None,
                tip,
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
                .text_size(px(S_COMPACT))
                .text_color(p.fg),
        )
}

fn bullet(line: &str) -> View {
    let p = Palette::current();
    view()
        .flex_row()
        .items_start()
        .gap(px(6.0))
        .child(text("•").text_size(px(S_SMALL)).text_color(p.muted))
        .child(
            text(line.to_owned())
                .flex_1()
                .text_size(px(S_SMALL))
                .text_color(p.fg),
        )
}
