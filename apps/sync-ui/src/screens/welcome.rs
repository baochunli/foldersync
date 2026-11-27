//! Welcome and empty-Overview onboarding states (`ui-spec.md` §5.1).

use nsh_core::prelude::*;

use crate::contract::{IconKind, UiCommand, UiSnapshot};
use crate::screens::AppController;
use crate::theme::{
    BtnKind, Palette, S_LEAD, S_SMALL, W_TITLE, WELCOME_ILLUSTRATION, app_button, page_title_large,
};

/// Renders either the "No account" Welcome screen or the "Account connected, no mappings" screen.
pub fn welcome_view(ctrl: AppController, snap: &UiSnapshot) -> View {
    let p = Palette::current();
    let has_account = !snap.accounts.is_empty();

    if !has_account {
        let ctrl_connect = ctrl.clone();
        let is_demo = snap.is_demo_mode;
        view()
            .flex_col()
            .items_center()
            .w_full()
            .pt(px(28.0))
            .pb(px(20.0))
            .px(px(16.0))
            .child(
                image(WELCOME_ILLUSTRATION)
                    .w(px(220.0))
                    .h(px(120.0))
                    .mb(px(18.0))
                    .accessible(false),
            )
            .child(page_title_large("Keep the Drive folders you choose on this Mac").mb(px(6.0)))
            .child(
                text(
                    "Pick folders from Google Drive and put each one on the drive you want, internal or external. They stay ordinary folders in Finder and sync both ways.",
                )
                .text_size(px(S_LEAD))
                .text_color(p.muted)
                .max_w(px(470.0))
                .mb(px(22.0))
                .text_center(),
            )
            .child(
                view()
                    .flex_row()
                    .gap(px(12.0))
                    .w_full()
                    .max_w(px(720.0))
                    .mb(px(18.0))
                    .child(step_card(
                        "1",
                        false,
                        "Connect Google Drive",
                        "Sign in once in your browser.",
                    ))
                    .child(step_card(
                        "2",
                        false,
                        "Choose folders",
                        "Only what fits and what you need. See each folder's size first.",
                    ))
                    .child(step_card(
                        "3",
                        false,
                        "Pick a place for each",
                        "Internal SSD, WorkSSD, PhotoSSD… each with its own folder.",
                    )),
            )
            .child(
                app_button(
                    "Connect Google Drive…",
                    BtnKind::Primary,
                    false,
                    false,
                    None,
                    None,
                    move || {
                        if is_demo {
                            ctrl_connect.send(UiCommand::ConnectDemoAccount(None));
                        } else {
                            ctrl_connect.navigate(crate::contract::ScreenRoute::Accounts);
                        }
                    },
                )
                .test_id("welcome-connect-btn")
                .mb(px(12.0)),
            )
            .child(
                text("Nothing is downloaded or uploaded until you review and start each folder's first sync.")
                    .text_size(px(S_SMALL))
                    .text_color(p.muted),
            )
    } else {
        let acct = &snap.accounts[0];
        let acct_sub = format!("{} · {}", acct.id, acct.email);
        let ctrl_choose = ctrl.clone();
        view()
            .flex_col()
            .items_center()
            .w_full()
            .pt(px(24.0))
            .pb(px(20.0))
            .px(px(16.0))
            .child(
                view()
                    .flex_row()
                    .gap(px(12.0))
                    .w_full()
                    .max_w(px(720.0))
                    .mb(px(18.0))
                    .child(step_card("✓", true, "Connect Google Drive", &acct_sub))
                    .child(step_card(
                        "2",
                        false,
                        "Choose folders",
                        "Data holds about 10.4 TB in 9 folders.",
                    ))
                    .child(step_card(
                        "3",
                        false,
                        "Pick a place for each",
                        "Internal SSD: 212 GB free · WorkSSD: 1.4 TB free",
                    )),
            )
            .child(page_title_large("No folders on this Mac yet").mb(px(6.0)))
            .child(
                text(
                    "Your Drive holds more than this Mac can store, so FolderSync never downloads everything. Choose the folders you want here.",
                )
                .text_size(px(S_LEAD))
                .text_color(p.muted)
                .max_w(px(470.0))
                .mb(px(22.0))
                .text_center(),
            )
            .child(
                app_button(
                    "Choose a folder…",
                    BtnKind::Primary,
                    false,
                    false,
                    Some(IconKind::Plus),
                    None,
                    move || ctrl_choose.open_wizard(),
                )
                .test_id("welcome-choose-folder-btn"),
            )
    }
}

fn step_card(badge: &str, done: bool, title: &str, subtitle: &str) -> View {
    let p = Palette::current();
    let (badge_bg, badge_fg) = if done {
        (p.ok_bg, p.ok)
    } else {
        (p.accent_soft, p.primary)
    };
    view()
        .flex_1()
        .bg(p.card)
        .border_1()
        .border_color(p.hair)
        .rounded(px(12.0))
        .px(px(14.0))
        .py(px(12.0))
        .flex_col()
        .gap(px(6.0))
        .child(
            view()
                .w(px(22.0))
                .h(px(22.0))
                .rounded_full()
                .bg(badge_bg)
                .items_center()
                .justify_center()
                .child(
                    text(badge.to_owned())
                        .text_size(px(S_SMALL))
                        .font_weight(W_TITLE)
                        .text_color(badge_fg),
                ),
        )
        .child(text(title.to_owned()).font_weight(W_TITLE).text_color(p.fg))
        .child(
            text(subtitle.to_owned())
                .text_size(px(S_SMALL))
                .text_color(p.muted),
        )
}
