//! The FolderSync prototype's Rust root component.

use nsh_base::theme;
use nsh_core::prelude::*;

#[native_module("sync-ui/native")]
fn native(registry: &mut Registry) {
    registry.register::<App>();
}

/// The application's root component.
#[component]
pub fn App() -> impl IntoElement {
    view()
        .flex_1()
        .bg(move || theme().background)
        .text_color(move || theme().foreground)
        .items_center()
        .justify_center()
        .child(text("FolderSync — prototype").text_size(px(28.0)))
}
