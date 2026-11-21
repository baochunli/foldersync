//! Accounts screen (`ui-spec.md` §5.7): connected identity, access status,
//! reconnect, check access, and guarded remove account.

use nsh_core::{file_dialog::OpenDialog, prelude::*};

use crate::contract::{AccountSnapshot, AccountStatus, IconKind, Tone, UiCommand};
use crate::screens::AppController;
use crate::theme::{
    BtnKind, Palette, app_button, card, card_title, icon, page_title, progress_bar, status_pill,
};

/// Renders the Accounts screen.
pub fn accounts_view(ctrl: AppController) -> impl IntoElement {
    let show_real_connect_help = signal(false);

    dynamic(move || {
        let snap = ctrl.snapshot.get();
        let p = Palette::current();
        let c_connect = ctrl.clone();
        let is_demo = snap.is_demo_mode;

        let mut root = view().flex_col().w_full().gap(px(14.0)).child(
            view()
                .flex_row()
                .items_end()
                .justify_between()
                .child(page_title("Accounts"))
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .child(if !is_demo {
                            let c_local = ctrl.clone();
                            app_button(
                                "Add local folder account…",
                                BtnKind::Default,
                                false,
                                false,
                                Some(IconKind::Folder),
                                None,
                                move || {
                                    let c = c_local.clone();
                                    let answer = pick_paths(
                                        OpenDialog::folders()
                                            .title("Choose parent folder for local test account")
                                            .button_label("Use folder"),
                                    );
                                    spawn(async move {
                                        if let Ok(Some(paths)) = answer.await
                                            && let Some(first) = paths.first()
                                        {
                                            let dir_str = first.to_string_lossy().into_owned();
                                            let name = first
                                                .file_name()
                                                .map(|s| s.to_string_lossy().into_owned())
                                                .unwrap_or_else(|| "local-fixture".to_owned());
                                            let slug: String = name
                                                .to_ascii_lowercase()
                                                .chars()
                                                .map(|ch| {
                                                    if ch.is_ascii_alphanumeric() {
                                                        ch
                                                    } else {
                                                        '-'
                                                    }
                                                })
                                                .collect();
                                            c.send(UiCommand::AddLocalFixtureAccount {
                                                id: if slug.is_empty() {
                                                    "local-account".to_owned()
                                                } else {
                                                    slug
                                                },
                                                display_name: format!("{name} (local folder)"),
                                                parent_dir: dir_str,
                                            });
                                        }
                                    });
                                },
                            )
                            .test_id("accounts-add-local-fixture-btn")
                            .into_element()
                        } else {
                            view().into_element()
                        })
                        .child(
                            app_button(
                                "Connect account…",
                                BtnKind::Primary,
                                false,
                                false,
                                Some(IconKind::Plus),
                                None,
                                move || {
                                    if is_demo {
                                        c_connect.send(UiCommand::ConnectDemoAccount);
                                    } else {
                                        show_real_connect_help.set(true);
                                    }
                                },
                            )
                            .test_id("accounts-connect-btn"),
                        ),
                ),
        );

        if !is_demo && show_real_connect_help.get() {
            root = root.child(
                card()
                    .border_color(p.run_bd)
                    .child(card_title("Connect a Google Drive account"))
                    .child(
                        text("Browser Google Drive sign-in is provided by the Drive account onboarding flow. For local-to-local folder testing right now, use “Add local folder account…” above.")
                            .text_size(px(12.5))
                            .text_color(p.fg),
                    )
                    .child(view().flex_row().pt(px(4.0)).child(app_button(
                        "Dismiss",
                        BtnKind::Default,
                        true,
                        false,
                        None,
                        None,
                        move || show_real_connect_help.set(false),
                    ))),
            );
        }

        if snap.accounts.is_empty() {
            root = root.child(
                card().child(
                    text("No accounts connected yet.")
                        .text_size(px(13.0))
                        .text_color(p.muted),
                ),
            );
        } else {
            for acct in &snap.accounts {
                root = root.child(account_card(ctrl.clone(), acct));
            }
        }

        if let Some(reconnecting_id) = &snap.reconnecting_account {
            let c_open = ctrl.clone();
            let c_cancel = ctrl.clone();
            let c_complete = ctrl.clone();
            let id_open = reconnecting_id.clone();
            let id_complete = reconnecting_id.clone();
            let mut btn_row = view()
                .flex_row()
                .flex_wrap()
                .gap(px(8.0))
                .pt(px(4.0))
                .child(app_button(
                    "Open the sign-in page again",
                    BtnKind::Default,
                    true,
                    false,
                    None,
                    None,
                    move || {
                        c_open.send(UiCommand::OpenCloudUrl(format!(
                            "https://accounts.google.com/?account={id_open}"
                        )));
                    },
                ));
            if is_demo {
                btn_row = btn_row.child(
                    app_button(
                        "Simulate sign-in complete (demo)",
                        BtnKind::Primary,
                        true,
                        false,
                        None,
                        None,
                        move || {
                            c_complete.send(UiCommand::CheckAccountAccess(id_complete.clone()));
                        },
                    )
                    .test_id("accounts-simulate-signin-btn"),
                );
            }
            btn_row = btn_row.child(
                app_button(
                    "Cancel",
                    BtnKind::Default,
                    true,
                    false,
                    None,
                    None,
                    move || c_cancel.send(UiCommand::CancelReconnect),
                )
                .test_id("accounts-cancel-reconnect-btn"),
            );

            root = root.child(
                card()
                    .border_color(p.run_bd)
                    .child(card_title("Finish signing in in your browser"))
                    .child(progress_bar(None, 6.0, "Waiting for browser sign-in"))
                    .child(btn_row),
            );
        }

        root.into_element()
    })
}

fn account_card(ctrl: AppController, acct: &AccountSnapshot) -> View {
    let p = Palette::current();
    let (pill_icon, pill_label, pill_tone, is_err) = match acct.status {
        AccountStatus::Connected => (IconKind::Check, "Connected", Tone::Ok, false),
        AccountStatus::SignInRequired => (IconKind::Key, "Sign-in required", Tone::Err, true),
        AccountStatus::KeychainLocked => (IconKind::Key, "Keychain locked", Tone::Err, true),
        AccountStatus::Reconnecting => (IconKind::Sync, "Reconnecting", Tone::Run, false),
    };

    let mut c = card();
    if is_err {
        c = c.border_l(px(4.0)).border_l_color(p.err_bd);
    }

    let mappings_text = if acct.mapping_count == 1 {
        "1 on this Mac".to_owned()
    } else {
        format!("{} on this Mac", acct.mapping_count)
    };
    let remove_reason = if acct.mapping_count == 1 {
        "Remove its 1 mapping first.".to_owned()
    } else {
        format!("Remove its {} mappings first.", acct.mapping_count)
    };
    let remove_disabled = acct.mapping_count > 0;

    let c_rec = ctrl.clone();
    let c_chk = ctrl.clone();
    let c_rm = ctrl.clone();
    let id_rec = acct.id.clone();
    let id_chk = acct.id.clone();
    let id_rm = acct.id.clone();

    let mut actions = view()
        .flex_row()
        .flex_wrap()
        .items_center()
        .gap(px(8.0))
        .pt(px(4.0))
        .child(app_button(
            "Reconnect…",
            if is_err {
                BtnKind::Primary
            } else {
                BtnKind::Default
            },
            true,
            false,
            None,
            None,
            move || c_rec.send(UiCommand::ReconnectAccount(id_rec.clone())),
        ));

    if !is_err {
        actions = actions.child(app_button(
            "Check access",
            BtnKind::Default,
            true,
            false,
            None,
            None,
            move || c_chk.send(UiCommand::CheckAccountAccess(id_chk.clone())),
        ));
    }

    actions = actions.child(
        app_button(
            "Remove account…",
            BtnKind::Default,
            true,
            remove_disabled,
            None,
            None,
            move || c_rm.send(UiCommand::RemoveAccount(id_rm.clone())),
        )
        .accessibility_label(if remove_disabled {
            format!("Remove account, disabled: {remove_reason}")
        } else {
            "Remove account".to_owned()
        }),
    );

    if remove_disabled {
        actions = actions.child(text(remove_reason).text_size(px(12.0)).text_color(p.muted));
    }

    c.child(
        view()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .child(icon(IconKind::Cloud, 16.0, p.muted))
            .child(card_title(acct.id.to_string()))
            .child(status_pill(pill_icon, pill_label, pill_tone)),
    )
    .child(acct_kv("Google account", &acct.email))
    .child(acct_kv("Shared parent folder", &acct.shared_parent_label))
    .child(acct_kv("Access", &acct.access_summary))
    .child(acct_kv("Mappings", &mappings_text))
    .child(actions)
}

fn acct_kv(label: &str, value: &str) -> View {
    let p = Palette::current();
    view()
        .flex_row()
        .items_start()
        .gap(px(12.0))
        .child(
            text(label.to_owned())
                .w(px(140.0))
                .flex_none()
                .text_size(px(12.5))
                .text_color(p.muted),
        )
        .child(
            text(value.to_owned())
                .flex_1()
                .text_size(px(12.5))
                .text_color(p.fg),
        )
}
