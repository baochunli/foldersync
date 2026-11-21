//! Accounts screen (`ui-spec.md` §5.7): connected identity, access status,
//! reconnect, check access, and guarded remove account.

use std::path::PathBuf;

use nsh_base::input;
use nsh_core::{file_dialog::OpenDialog, prelude::*};
use sync_core::AccountId;
use sync_manager::SecretString;

use crate::contract::{
    AccountSnapshot, AccountStatus, IconKind, ScreenRoute, Tone, UiCommand, UiSnapshot,
};
use crate::screens::AppController;
use crate::theme::{
    BLOCK_GAP, BtnKind, CARD_ACTION_MARGIN, HEADER_BOTTOM_MARGIN, HEADER_GAP, KV_ROW_GAP, Palette,
    S_SMALL, accented_card, app_button, card, card_title, focus_ring, icon, kv_row, page_title,
    progress_bar, status_pill,
};

/// Renders the Accounts screen.
pub fn accounts_view(ctrl: AppController) -> impl IntoElement {
    let form = signal(None::<AccountForm>);

    dynamic(move || {
        let snap = ctrl.snapshot.get();
        let p = Palette::current();
        let c_connect = ctrl.clone();
        let is_demo = snap.is_demo_mode;

        let mut root = view().flex_col().w_full().gap(px(BLOCK_GAP)).child(
            view()
                .window_drag(true)
                .mb(px(HEADER_BOTTOM_MARGIN))
                .flex_row()
                .gap(px(HEADER_GAP))
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
                                snap.account_operation_pending,
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
                        .child(connection_buttons(c_connect, &snap, form)),
                ),
        );

        let c_form = ctrl.clone();
        root = root.child(account_form_view(c_form, form));

        if snap.accounts.is_empty() {
            root = root.child(card().child(text("No accounts connected yet.").text_color(p.muted)));
        } else {
            for acct in &snap.accounts {
                root = root.child(account_card(
                    ctrl.clone(),
                    acct,
                    snap.account_operation_pending,
                ));
            }
        }

        root.child(authorization_card(ctrl.clone(), &snap))
            .into_element()
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AccountForm {
    Connect,
    Import,
}

pub(super) fn wizard_account_controls(ctrl: AppController) -> impl IntoElement {
    let form = signal(None::<AccountForm>);
    dynamic(move || {
        let snap = ctrl.snapshot.get();
        let c_accounts = ctrl.clone();
        view()
            .flex_col()
            .gap(px(8.0))
            .child(
                view()
                    .flex_row()
                    .items_center()
                    .flex_wrap()
                    .gap(px(8.0))
                    .child(connection_buttons(ctrl.clone(), &snap, form))
                    .child(app_button(
                        "Open Accounts",
                        BtnKind::Ghost,
                        false,
                        false,
                        None,
                        None,
                        move || c_accounts.navigate(ScreenRoute::Accounts),
                    )),
            )
            .child(account_form_view(ctrl.clone(), form))
            .child(authorization_card(ctrl.clone(), &snap))
            .into_element()
    })
}

fn connection_buttons(
    ctrl: AppController,
    snap: &UiSnapshot,
    form: Signal<Option<AccountForm>>,
) -> View {
    let is_demo = snap.is_demo_mode;
    view()
        .flex_row()
        .flex_wrap()
        .gap(px(8.0))
        .child(if is_demo {
            view().into_element()
        } else {
            app_button(
                "Import existing remote…",
                BtnKind::Default,
                false,
                snap.account_operation_pending,
                None,
                None,
                move || form.set(Some(AccountForm::Import)),
            )
            .test_id("accounts-import-btn")
            .into_element()
        })
        .child(
            app_button(
                "Connect Google Drive…",
                BtnKind::Primary,
                false,
                snap.account_operation_pending,
                Some(IconKind::Plus),
                None,
                move || {
                    if is_demo {
                        ctrl.send(UiCommand::ConnectDemoAccount)
                    } else {
                        form.set(Some(AccountForm::Connect));
                    }
                },
            )
            .test_id("accounts-connect-btn"),
        )
}

fn account_form_view(ctrl: AppController, form: Signal<Option<AccountForm>>) -> impl IntoElement {
    dynamic(move || match form.get() {
        Some(mode) => drive_setup_form(ctrl.clone(), mode, form).into_element(),
        None => view().into_element(),
    })
}

fn drive_setup_form(
    ctrl: AppController,
    mode: AccountForm,
    form: Signal<Option<AccountForm>>,
) -> impl IntoElement {
    let account_id = signal(String::new());
    let display_name = signal(String::new());
    let parent = signal(String::new());
    let client_id = signal(String::new());
    let secret = signal(SecretString::new(""));
    let remote = signal(String::new());
    let source = signal(None::<PathBuf>);
    let p = Palette::current();
    let is_import = mode == AccountForm::Import;
    let mut body = card()
        .child(card_title(if is_import {
            "Import a Google Drive remote"
        } else {
            "Connect Google Drive"
        }))
        .child(
            text(if is_import {
                "Choose an unencrypted export containing the remote you want to import."
            } else {
                "Enter your desktop OAuth client details and the shared parent folder ID."
            })
            .text_color(p.muted),
        )
        .child(account_field("Account ID", account_id))
        .child(account_field("Account display name", display_name))
        .child(account_field(
            if is_import {
                "Shared parent folder ID (optional)"
            } else {
                "Shared parent folder ID"
            },
            parent,
        ));
    if is_import {
        body = body.child(account_field("Remote name", remote)).child(
            view()
                .flex_row()
                .flex_wrap()
                .gap(px(8.0))
                .items_center()
                .child(app_button(
                    "Choose configuration file…",
                    BtnKind::Default,
                    true,
                    false,
                    None,
                    None,
                    move || {
                        let answer = pick_paths(
                            OpenDialog::files().title("Choose exported configuration file"),
                        );
                        spawn(async move {
                            if let Ok(Some(paths)) = answer.await {
                                source.set(paths.into_iter().next());
                            }
                        });
                    },
                ))
                .child(dynamic(move || {
                    text(
                        source
                            .get()
                            .and_then(|path| {
                                path.file_name()
                                    .map(|name| name.to_string_lossy().into_owned())
                            })
                            .unwrap_or_else(|| "Choose a file".to_owned()),
                    )
                    .text_color(p.muted)
                    .into_element()
                })),
        );
    } else {
        body = body
            .child(account_field("OAuth client ID", client_id))
            .child(kv_row(
                "OAuth client secret",
                input()
                    .masked(true)
                    .placeholder("OAuth client secret")
                    .flex_1()
                    .min_h(px(28.0))
                    .px(px(8.0))
                    .rounded(px(7.0))
                    .bg(p.bg)
                    .border_1()
                    .border_color(p.border)
                    .focus_visible(focus_ring)
                    .on_change(move |value: &str| secret.set(SecretString::new(value)))
                    .into_element(),
            ));
    }

    body.child(dynamic(move || {
        let have = |value: Signal<String>| !value.get().trim().is_empty();
        let valid = have(account_id)
            && have(display_name)
            && if is_import {
                have(remote) && source.get().is_some()
            } else {
                have(parent) && have(client_id) && !secret.with(|value| value.is_empty())
            };
        let c_submit = ctrl.clone();
        view()
            .flex_row()
            .gap(px(8.0))
            .child(app_button(
                "Cancel",
                BtnKind::Default,
                true,
                false,
                None,
                None,
                move || {
                    secret.set(SecretString::new(""));
                    form.set(None);
                },
            ))
            .child(app_button(
                if is_import {
                    "Import remote"
                } else {
                    "Open browser to sign in"
                },
                BtnKind::Primary,
                true,
                !valid,
                None,
                None,
                move || {
                    let id = AccountId::new(account_id.get_untracked().trim());
                    let display_name = display_name.get_untracked().trim().to_owned();
                    let parent = parent.get_untracked().trim().to_owned();
                    if is_import {
                        let Some(path) = source.get_untracked() else {
                            return;
                        };
                        c_submit.send(UiCommand::ImportDrive {
                            account_id: id,
                            display_name,
                            source_config_path: path,
                            source_remote_name: remote.get_untracked().trim().to_owned(),
                            root_folder_id: (!parent.is_empty()).then_some(parent),
                        });
                    } else {
                        let client_secret = secret.get_untracked();
                        secret.set(SecretString::new(""));
                        c_submit.send(UiCommand::ConnectDrive {
                            account_id: id,
                            display_name,
                            root_folder_id: parent,
                            client_id: client_id.get_untracked().trim().to_owned(),
                            client_secret,
                        });
                    }
                    form.set(None);
                },
            ))
            .into_element()
    }))
}

fn account_field(label: &'static str, value: Signal<String>) -> View {
    let p = Palette::current();
    kv_row(
        label,
        input()
            .placeholder(label)
            .flex_1()
            .min_h(px(28.0))
            .px(px(8.0))
            .rounded(px(7.0))
            .bg(p.bg)
            .border_1()
            .border_color(p.border)
            .focus_visible(focus_ring)
            .on_change(move |text: &str| value.set(text.to_owned()))
            .into_element(),
    )
}

fn authorization_card(ctrl: AppController, snap: &UiSnapshot) -> View {
    let p = Palette::current();
    let Some(id) = &snap.reconnecting_account else {
        return if snap.account_operation_pending {
            card()
                .child(card_title("Checking account access"))
                .child(progress_bar(None, 6.0, "Checking account access"))
        } else {
            view()
        };
    };
    let is_demo = snap.is_demo_mode;
    let ready = is_demo || snap.can_reopen_authorization;
    let demo_id = id.clone();
    let c_open = ctrl.clone();
    let mut buttons = view()
        .flex_row()
        .flex_wrap()
        .gap(px(8.0))
        .pt(px(4.0))
        .child(app_button(
            "Open the sign-in page again",
            BtnKind::Default,
            true,
            !ready,
            None,
            None,
            move || c_open.send(UiCommand::ReopenDriveAuthorization),
        ));
    if is_demo {
        let c_complete = ctrl.clone();
        buttons = buttons
            .child(
                app_button(
                    "Simulate sign-in complete (demo)",
                    BtnKind::Primary,
                    true,
                    false,
                    None,
                    None,
                    move || c_complete.send(UiCommand::CheckAccountAccess(demo_id.clone())),
                )
                .test_id("accounts-simulate-signin-btn"),
            )
            .child(
                app_button(
                    "Cancel",
                    BtnKind::Default,
                    true,
                    false,
                    None,
                    None,
                    move || ctrl.send(UiCommand::CancelReconnect),
                )
                .test_id("accounts-cancel-reconnect-btn"),
            );
    }
    card()
        .border_color(p.run_bd)
        .child(card_title(if ready {
            "Finish signing in in your browser"
        } else {
            "Preparing Google Drive sign-in"
        }))
        .child(progress_bar(None, 6.0, "Waiting for browser sign-in"))
        .child(buttons)
}

fn account_card(ctrl: AppController, acct: &AccountSnapshot, busy: bool) -> View {
    let p = Palette::current();
    let (pill_icon, pill_label, pill_tone, is_err) = match acct.status {
        AccountStatus::Connected => (IconKind::Check, "Connected", Tone::Ok, false),
        AccountStatus::Unverified => (IconKind::Bang, "Access unverified", Tone::Attn, false),
        AccountStatus::SignInRequired => (IconKind::Key, "Sign-in required", Tone::Err, true),
        AccountStatus::KeychainLocked => (IconKind::Key, "Keychain locked", Tone::Err, true),
        AccountStatus::Reconnecting => (IconKind::Sync, "Reconnecting", Tone::Run, false),
    };

    let c = if is_err {
        accented_card(p.err_bd)
    } else {
        card()
    };

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
    let remove_disabled = acct.mapping_count > 0 || busy;

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
        .mt(px(CARD_ACTION_MARGIN));
    if acct.is_drive {
        actions = actions.child(app_button(
            "Reconnect…",
            if is_err {
                BtnKind::Primary
            } else {
                BtnKind::Default
            },
            true,
            busy,
            None,
            None,
            move || c_rec.send(UiCommand::ReconnectAccount(id_rec.clone())),
        ));
    }

    if acct.is_drive {
        actions = actions.child(app_button(
            "Check access",
            BtnKind::Default,
            true,
            busy,
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
        .accessibility_label(if acct.mapping_count > 0 {
            format!("Remove account, disabled: {remove_reason}")
        } else {
            "Remove account".to_owned()
        }),
    );

    if acct.mapping_count > 0 {
        actions = actions.child(
            text(remove_reason)
                .text_size(px(S_SMALL))
                .text_color(p.muted),
        );
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
    .child(
        view()
            .flex_col()
            .gap(px(KV_ROW_GAP))
            .child(kv_row(
                if acct.is_drive {
                    "Google account"
                } else {
                    "Account"
                },
                text(acct.email.clone()),
            ))
            .child(kv_row(
                "Shared parent folder",
                text(acct.shared_parent_label.clone()),
            ))
            .child(kv_row("Access", text(acct.access_summary.clone())))
            .child(kv_row("Mappings", text(mappings_text))),
    )
    .child(actions)
}
