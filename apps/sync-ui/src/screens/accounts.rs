//! Accounts screen (`ui-spec.md` §5.7): connected identity, access status,
//! reconnect, check access, and guarded remove account.

use nsh_core::prelude::*;

use crate::contract::{AccountSnapshot, AccountStatus, IconKind, Tone, UiCommand};
use crate::screens::AppController;
use crate::theme::{
    BtnKind, Palette, app_button, card, card_title, icon, page_title, progress_bar, status_pill,
};

/// Renders the Accounts screen.
pub fn accounts_view(ctrl: AppController) -> impl IntoElement {
    dynamic(move || {
        let snap = ctrl.snapshot.get();
        let p = Palette::current();
        let c_connect = ctrl.clone();

        let mut root = view().flex_col().w_full().gap(px(14.0)).child(
            view()
                .flex_row()
                .items_end()
                .justify_between()
                .child(page_title("Accounts"))
                .child(app_button(
                    "Connect account…",
                    BtnKind::Primary,
                    false,
                    false,
                    Some(IconKind::Plus),
                    None,
                    move || c_connect.send(UiCommand::ConnectDemoAccount),
                )),
        );

        if snap.accounts.is_empty() {
            root = root.child(
                card().child(
                    text("No Google Drive accounts connected yet.")
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
            root = root.child(
                card()
                    .border_color(p.run_bd)
                    .child(card_title("Finish signing in in your browser"))
                    .child(progress_bar(None, 6.0, "Waiting for browser sign-in"))
                    .child(
                        view()
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
                                    c_open.send(UiCommand::DemoAction(format!(
                                        "Simulated browser sign-in page for {id_open} (demo mode)"
                                    )));
                                },
                            ))
                            .child(app_button(
                                "Simulate sign-in complete (demo)",
                                BtnKind::Primary,
                                true,
                                false,
                                None,
                                None,
                                move || {
                                    c_complete
                                        .send(UiCommand::CheckAccountAccess(id_complete.clone()));
                                },
                            ))
                            .child(app_button(
                                "Cancel",
                                BtnKind::Default,
                                true,
                                false,
                                None,
                                None,
                                move || c_cancel.send(UiCommand::CancelReconnect),
                            )),
                    ),
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
