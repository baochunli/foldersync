//! Visual tokens, bundled Paper Mono font asset, SVG icons, and shared UI primitives.
//!
//! Typography rules (approved 2026-11-20):
//! - Sans-serif: macOS system font (no bundled sans).
//! - Monospace: Paper Mono (`assets/fonts/PaperMono-Regular.ttf`, SIL OFL 1.1).

use nsh_base::{Theme, button, progress, theme, tooltip};
use nsh_core::{
    EllipsizeMode, LineHeight,
    element::{ImageSrc, StyleRefinement},
    prelude::*,
};

use crate::contract::{IconKind, Tone};

// One NSH weight step below the mockup to compensate for macOS font smoothing.
pub const W_BODY: FontWeight = FontWeight(300);
pub const W_EMPH: FontWeight = FontWeight::NORMAL;
pub const W_STRONG: FontWeight = FontWeight::MEDIUM;
pub const W_TITLE: FontWeight = FontWeight::SEMIBOLD;
pub const W_MONO: FontWeight = FontWeight::NORMAL;

pub const S_TINY: f32 = 11.0;
pub const S_CAPTION: f32 = 11.5;
pub const S_SMALL: f32 = 12.0;
pub const S_COMPACT: f32 = 12.5;
pub const S_BODY: f32 = 13.0;
pub const S_LEAD: f32 = 13.5;
pub const S_CARD_TITLE: f32 = 14.0;
pub const S_QUESTION: f32 = 15.0;
pub const LH_BODY: LineHeight = LineHeight::Multiple(1.5);
const LH_TITLE: LineHeight = LineHeight::Multiple(1.2);
pub const BADGE_LINE_HEIGHT: f32 = 16.0;
const PILL_LINE_HEIGHT: f32 = 18.0;

pub const SIDEBAR_WIDTH: f32 = 212.0;
pub const NEUTRAL_HOVER: AdaptiveColor = AdaptiveColor::rgb(0x7f7f7f, 0x7f7f7f);
pub const PAGE_DRAG_HEIGHT: f32 = 14.0;
pub const PAGE_TOP_PADDING: f32 = 8.0;
pub const PAGE_SIDE_PADDING: f32 = 26.0;
pub const PAGE_BOTTOM_PADDING: f32 = 28.0;
pub const HEADER_GAP: f32 = 8.0;
pub const BLOCK_GAP: f32 = 14.0;
pub const HEADER_BOTTOM_MARGIN: f32 = 2.0;
pub const CARD_ACTION_MARGIN: f32 = 2.0;
pub const KV_ROW_GAP: f32 = 6.0;

/// Bundled Paper Mono Regular font asset (official release v1.000, SIL OFL 1.1).
pub const PAPER_MONO_FONT: FontAsset = asset!("fonts/PaperMono-Regular.ttf");

/// Bundled tray template icon (normal).
pub const TRAY_ICON: ImageAsset = asset!("icons/tray.svg");

/// Bundled tray template icon (attention variant with "!" shape).
pub const TRAY_ATTENTION_ICON: ImageAsset = asset!("icons/tray-attention.svg");

/// Bundled Welcome illustration SVG.
pub const WELCOME_ILLUSTRATION: ImageAsset = asset!("icons/welcome.svg");

/// Returns the active font family name for Paper Mono (from the bundled asset).
pub const fn mono_font_family() -> &'static str {
    PAPER_MONO_FONT.family
}

/// Helper to read a custom token from `theme.json` with a safe adaptive fallback.
pub fn token_color(t: &Theme, name: &str, light: u32, dark: u32) -> AdaptiveColor {
    t.color(name)
        .unwrap_or_else(|| AdaptiveColor::rgb(light, dark))
}

/// App semantic colors resolved from `theme.json`.
#[derive(Clone, Copy)]
pub struct Palette {
    pub bg: AdaptiveColor,
    pub fg: AdaptiveColor,
    pub card: AdaptiveColor,
    pub muted: AdaptiveColor,
    pub faint: AdaptiveColor,
    pub sidebar: AdaptiveColor,
    pub sidebar_sel: AdaptiveColor,
    pub hair: AdaptiveColor,
    pub border: AdaptiveColor,
    pub sunk: AdaptiveColor,
    pub primary: AdaptiveColor,
    pub primary_fg: AdaptiveColor,
    pub accent_soft: AdaptiveColor,
    pub track: AdaptiveColor,
    pub ring: AdaptiveColor,
    pub ok: AdaptiveColor,
    pub ok_bg: AdaptiveColor,
    pub ok_bd: AdaptiveColor,
    pub run: AdaptiveColor,
    pub run_bg: AdaptiveColor,
    pub run_bd: AdaptiveColor,
    pub attn: AdaptiveColor,
    pub attn_bg: AdaptiveColor,
    pub attn_bd: AdaptiveColor,
    pub err: AdaptiveColor,
    pub err_bg: AdaptiveColor,
    pub err_bd: AdaptiveColor,
    pub neu: AdaptiveColor,
    pub neu_bg: AdaptiveColor,
    pub neu_bd: AdaptiveColor,
}

impl Palette {
    /// Reads the current reactive theme tokens.
    pub fn current() -> Self {
        let t = theme();
        Self {
            bg: t.background,
            fg: t.foreground,
            card: t.card,
            muted: t.muted_foreground,
            faint: token_color(&t, "faint", 0x8a8b93, 0x7f8088),
            sidebar: t.sidebar,
            sidebar_sel: t.sidebar_accent,
            hair: token_color(&t, "hair", 0xe4e4e9, 0x303035),
            border: t.border,
            sunk: token_color(&t, "sunk", 0xf7f7f9, 0x1a1a1d),
            primary: t.primary,
            primary_fg: t.primary_foreground,
            accent_soft: token_color(&t, "accent-soft", 0xe8eefc, 0x25314d),
            track: token_color(&t, "track", 0xe6e7ec, 0x35363c),
            ring: t.ring,
            ok: token_color(&t, "ok", 0x23784a, 0x7fd3a2),
            ok_bg: token_color(&t, "ok-bg", 0xe6f4ec, 0x183326),
            ok_bd: token_color(&t, "ok-bd", 0xa9d6bb, 0x2f6b4b),
            run: token_color(&t, "run", 0x2f62d9, 0x9dbcff),
            run_bg: token_color(&t, "run-bg", 0xe8eefc, 0x1d2a47),
            run_bd: token_color(&t, "run-bd", 0xb3c6f2, 0x3b5795),
            attn: token_color(&t, "attn", 0x8a5200, 0xffcf85),
            attn_bg: token_color(&t, "attn-bg", 0xfdf1dc, 0x3a2b12),
            attn_bd: token_color(&t, "attn-bd", 0xe7bf7a, 0x8f6a2c),
            err: token_color(&t, "err", 0xa8261c, 0xffaca3),
            err_bg: token_color(&t, "err-bg", 0xfcebe9, 0x3d1b18),
            err_bd: token_color(&t, "err-bd", 0xeba7a0, 0x94453d),
            neu: token_color(&t, "neu", 0x4a4b52, 0xd7d8de),
            neu_bg: token_color(&t, "neu-bg", 0xeeeef1, 0x313136),
            neu_bd: token_color(&t, "neu-bd", 0xc9cad0, 0x55565e),
        }
    }

    /// Returns `(fg, bg, border)` for a semantic tone.
    pub fn tone(&self, tone: Tone) -> (AdaptiveColor, AdaptiveColor, AdaptiveColor) {
        match tone {
            Tone::Ok => (self.ok, self.ok_bg, self.ok_bd),
            Tone::Run => (self.run, self.run_bg, self.run_bd),
            Tone::Attn => (self.attn, self.attn_bg, self.attn_bd),
            Tone::Err => (self.err, self.err_bg, self.err_bd),
            Tone::Neutral => (self.neu, self.neu_bg, self.neu_bd),
        }
    }
}

/// 3 pt focus-visible ring outline per `ui-spec.md` §8.
pub fn focus_ring(s: StyleRefinement) -> StyleRefinement {
    let p = Palette::current();
    s.outline(px(3.0))
        .outline_color(p.ring)
        .outline_offset(px(2.0))
}

/// Returns crisp SVG markup for an icon shape (`stroke="#000000"` so `.tint_color(...)` tints it cleanly).
pub fn icon_svg_markup(kind: IconKind) -> &'static str {
    match kind {
        IconKind::Grid => {
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="3.5" y="3.5" width="7" height="7" rx="1.5"/><rect x="13.5" y="3.5" width="7" height="7" rx="1.5"/><rect x="3.5" y="13.5" width="7" height="7" rx="1.5"/><rect x="13.5" y="13.5" width="7" height="7" rx="1.5"/></svg>"##
        }
        IconKind::Pulse => {
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M3 12h4l2.5-6 5 12 2.5-6h4"/></svg>"##
        }
        IconKind::Alert => {
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M12 3.5 21.5 20h-19z"/><path d="M12 10v4.5M12 17.5v.1"/></svg>"##
        }
        IconKind::User => {
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="8.5" r="3.8"/><path d="M4.5 20c1.2-3.8 4-5.5 7.5-5.5s6.3 1.7 7.5 5.5"/></svg>"##
        }
        IconKind::Gear => {
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="3.2"/><path d="M12 2.8v2.6M12 18.6v2.6M2.8 12h2.6M18.6 12h2.6M5.5 5.5l1.8 1.8M16.7 16.7l1.8 1.8M5.5 18.5l1.8-1.8M16.7 7.3l1.8-1.8"/></svg>"##
        }
        IconKind::DriveExternal => {
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="7" width="18" height="10" rx="2.5"/><path d="M7 13.5h.1M10 13.5h.1"/></svg>"##
        }
        IconKind::DriveInternal => {
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="4" y="4" width="16" height="12" rx="2"/><path d="M2.5 20h19"/></svg>"##
        }
        IconKind::DriveMissing => {
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="7" width="18" height="10" rx="2.5" stroke-dasharray="3 2.5"/><path d="M4 4l16 16"/></svg>"##
        }
        IconKind::Cloud => {
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M7 18.5h10.5a4 4 0 0 0 .6-7.95A5.5 5.5 0 0 0 7.5 9.2 4.7 4.7 0 0 0 7 18.5z"/></svg>"##
        }
        IconKind::Check => {
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="2.1" stroke-linecap="round" stroke-linejoin="round"><path d="M5 12.5l4.5 4.5L19 7.5"/></svg>"##
        }
        IconKind::Sync => {
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round"><path d="M19.5 8.5A8 8 0 0 0 5 9M4.5 15.5A8 8 0 0 0 19 15"/><path d="M19.8 4v4.6h-4.6M4.2 20v-4.6h4.6"/></svg>"##
        }
        IconKind::Clock => {
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="8.5"/><path d="M12 7.5V12l3 2"/></svg>"##
        }
        IconKind::Pause => {
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="2.1" stroke-linecap="round" stroke-linejoin="round"><path d="M9 6v12M15 6v12"/></svg>"##
        }
        IconKind::Bang => {
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 5v9M12 18.5v.1"/></svg>"##
        }
        IconKind::Octagon => {
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round"><path d="M8.3 3h7.4L21 8.3v7.4L15.7 21H8.3L3 15.7V8.3z"/><path d="M12 8v5M12 16.2v.1"/></svg>"##
        }
        IconKind::Key => {
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round"><circle cx="8" cy="12" r="4"/><path d="M12 12h9M18 12v3M21 12v2"/></svg>"##
        }
        IconKind::Dash => {
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="2.1" stroke-linecap="round" stroke-linejoin="round"><path d="M6 12h12"/></svg>"##
        }
        IconKind::Folder => {
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M3 7.5A2 2 0 0 1 5 5.5h4l2 2h8a2 2 0 0 1 2 2V17a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/></svg>"##
        }
        IconKind::Plus => {
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="2.0" stroke-linecap="round" stroke-linejoin="round"><path d="M12 5v14M5 12h14"/></svg>"##
        }
        IconKind::Play => {
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round"><path d="M8 5.5v13l10-6.5z"/></svg>"##
        }
        IconKind::Stop => {
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round"><rect x="6.5" y="6.5" width="11" height="11" rx="1.5"/></svg>"##
        }
        IconKind::Swap => {
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round"><path d="M12 3.5v9M8.5 9 12 12.5 15.5 9"/><path d="M5 20.5h14"/></svg>"##
        }
    }
}

/// Renders a tinted template SVG icon at `size_pt` × `size_pt` (hidden from accessibility because paired with text).
pub fn icon(
    kind: IconKind,
    size_pt: f32,
    tint: impl IntoReactive<AdaptiveColor>,
) -> impl IntoElement {
    image(ImageSrc::svg_markup(icon_svg_markup(kind)))
        .w(px(size_pt))
        .h(px(size_pt))
        .flex_none()
        .tint_color(tint)
        .accessible(false)
}

/// Renders monospaced text (paths, IDs, logs, engine options) in bundled **Paper Mono**.
pub fn mono_text(content: impl Into<SharedString>, size_pt: f32, color: AdaptiveColor) -> Text {
    let s: SharedString = content.into();
    text(s)
        .font_family(mono_font_family())
        .font_weight(W_MONO)
        .text_size(px(size_pt))
        .text_color(color)
        .tabular_nums()
}

/// Renders a monospaced path line in Paper Mono at 11 pt (the spec's path typography).
pub fn mono_path(content: impl Into<SharedString>) -> Text {
    let p = Palette::current();
    let full_path = content.into();
    mono_text(full_path.clone(), S_TINY, p.muted)
        .number_of_lines(1)
        .text_overflow(EllipsizeMode::Middle)
        .label(full_path)
}

/// A paragraph with a headline and regular-weight continuation.
pub fn emphasized_line(head: impl Into<SharedString>, tail: impl Into<SharedString>) -> Text {
    text("")
        .child(text(head.into()).font_weight(W_TITLE))
        .child(text(tail.into()))
        .tabular_nums()
}

/// Renders a page heading with accessibility level 1.
pub fn page_title(title: impl Into<SharedString>) -> Text {
    heading(title, 22.0, -0.22)
}

/// Renders a larger page heading for the Welcome states.
pub fn page_title_large(title: impl Into<SharedString>) -> Text {
    heading(title, 24.0, -0.36)
}

fn heading(title: impl Into<SharedString>, size: f32, tracking: f32) -> Text {
    text(title.into())
        .text_size(px(size))
        .line_height(LH_TITLE)
        .letter_spacing(px(tracking))
        .font_weight(W_TITLE)
        .text_color(Palette::current().fg)
        .role(AccessibilityRole::Header)
        .accessibility_level(1)
}

/// Renders a card title (14 pt semibold, AccessibilityRole::Header level 2).
pub fn card_title(title: impl Into<SharedString>) -> Text {
    let p = Palette::current();
    let s: SharedString = title.into();
    text(s)
        .text_size(px(S_CARD_TITLE))
        .font_weight(W_TITLE)
        .text_color(p.fg)
        .role(AccessibilityRole::Header)
        .accessibility_level(2)
}

/// Renders a muted section sub-label (11.5 pt medium, not all caps).
pub fn section_label(label: impl Into<SharedString>) -> Text {
    let p = Palette::current();
    let s: SharedString = label.into();
    text(s)
        .text_size(px(S_CAPTION))
        .font_weight(W_STRONG)
        .letter_spacing(px(0.23))
        .text_color(p.muted)
}

/// Standard hairline card container (12 pt radius, no heavy shadows).
pub fn card() -> View {
    let p = Palette::current();
    view()
        .bg(p.card)
        .border_1()
        .border_color(p.hair)
        .rounded(px(12.0))
        .px(px(16.0))
        .py(px(14.0))
        .flex_col()
        .gap(px(8.0))
}

/// A uniform border keeps all corners rounded. The tone is painted inside it.
pub fn accented_card(accent: AdaptiveColor) -> View {
    card().relative().overflow_hidden().pl(px(19.0)).child(
        view()
            .absolute()
            .left(px(0.0))
            .top(px(0.0))
            .bottom(px(0.0))
            .w(px(4.0))
            .bg(accent)
            .accessible(false),
    )
}

/// Standard key–value row. Stack consecutive rows with `KV_ROW_GAP`.
pub fn kv_row(label: impl Into<SharedString>, value: impl IntoElement) -> View {
    view()
        .flex_row()
        .items_start()
        .gap(px(14.0))
        .child(
            text(label.into())
                .w(px(150.0))
                .flex_none()
                .text_color(Palette::current().muted),
        )
        .child(view().flex_1().min_w(px(0.0)).child(value))
}

/// Status pill with icon shape + calm tint + text label (never color alone).
pub fn status_pill(icon_kind: IconKind, label: impl Into<SharedString>, tone: Tone) -> View {
    let p = Palette::current();
    let (fg, bg, bd) = p.tone(tone);
    let s: SharedString = label.into();
    view()
        .flex_row()
        .items_center()
        .gap(px(5.0))
        .bg(bg)
        .border_1()
        .border_color(bd)
        .rounded_full()
        .pl(px(6.0))
        .pr(px(9.0))
        .py(px(1.0))
        .child(icon(icon_kind, 13.0, fg))
        .child(
            text(s)
                .text_size(px(S_CAPTION))
                .line_height(px(PILL_LINE_HEIGHT))
                .font_weight(W_STRONG)
                .text_color(fg)
                .tabular_nums(),
        )
}

/// 28×28 tinted circular status icon shown at the start of every Overview row.
pub fn status_dot(icon_kind: IconKind, tone: Tone) -> View {
    let p = Palette::current();
    let (fg, bg, _) = p.tone(tone);
    view()
        .w(px(28.0))
        .h(px(28.0))
        .rounded_full()
        .bg(bg)
        .items_center()
        .justify_center()
        .flex_none()
        .child(icon(icon_kind, 15.0, fg))
}

/// Visual variant for app-styled `nsh_base::button()` controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BtnKind {
    Default,
    Primary,
    Ghost,
}

/// App-styled `nsh_base::button()` with visible 3pt focus ring, hover/disabled states, and optional tooltip.
pub fn app_button(
    label: impl Into<SharedString>,
    kind: BtnKind,
    small: bool,
    disabled: bool,
    icon_kind: Option<IconKind>,
    tip: Option<&'static str>,
    on_press: impl Fn() + 'static,
) -> nsh_base::ButtonElement {
    let p = Palette::current();
    let label_str: SharedString = label.into();
    let (bg, border, fg, weight) = match kind {
        BtnKind::Primary => (p.primary, p.primary, p.primary_fg, W_STRONG),
        BtnKind::Default => (p.bg, p.border, p.fg, W_EMPH),
        BtnKind::Ghost => (
            AdaptiveColor::TRANSPARENT,
            AdaptiveColor::TRANSPARENT,
            p.primary,
            W_EMPH,
        ),
    };
    let min_h = if small { 24.0 } else { 28.0 };
    let px_pad = if small { 9.0 } else { 12.0 };
    let py_pad = if small { 2.0 } else { 4.0 };
    let font_sz = if small { S_SMALL } else { S_BODY };

    let el = button()
        .disabled(disabled)
        .flex_row()
        .items_center()
        .justify_center()
        .gap(px(6.0))
        .min_h(px(min_h))
        .px(px(px_pad))
        .py(px(py_pad))
        .rounded(px(7.0))
        .bg(bg)
        .border_1()
        .border_color(border)
        .cursor_pointer()
        .hover(move |s| {
            if kind == BtnKind::Primary {
                s.opacity(0.92)
            } else {
                s.bg(p.sunk)
            }
        })
        .disabled_style(|s| s.opacity(0.42))
        .focus_visible(focus_ring)
        .on_press(on_press);

    let mut content = view()
        .flex_row()
        .items_center()
        .justify_center()
        .gap(px(6.0));
    if let Some(ik) = icon_kind {
        content = content.child(icon(ik, if small { 14.0 } else { 16.0 }, fg));
    }
    content = content.child(
        text(label_str)
            .text_size(px(font_sz))
            .font_weight(weight)
            .text_color(fg)
            .tabular_nums(),
    );
    if let Some(tip_text) = tip {
        let tip_p = p;
        content = content.tooltip(
            tooltip().child(
                view()
                    .bg(tip_p.card)
                    .border_1()
                    .border_color(tip_p.border)
                    .rounded(px(6.0))
                    .px(px(8.0))
                    .py(px(4.0))
                    .child(
                        text(tip_text)
                            .text_size(px(S_CAPTION))
                            .font_weight(W_BODY)
                            .text_color(tip_p.fg),
                    ),
            ),
        );
    }
    el.child(content)
}

/// Inline text link button with focus ring.
pub fn link_button(
    label: impl Into<SharedString>,
    size: f32,
    on_press: impl Fn() + 'static,
) -> nsh_base::ButtonElement {
    text_link(on_press).child(
        text(label.into())
            .text_size(px(size))
            .text_color(Palette::current().primary),
    )
}

/// Foreground-colored mapping name with the same link behavior and focus ring.
pub fn name_link(
    label: impl Into<SharedString>,
    size: f32,
    on_press: impl Fn() + 'static,
) -> nsh_base::ButtonElement {
    text_link(on_press).hover(|style| style.underline()).child(
        text(label.into())
            .text_size(px(size))
            .font_weight(W_STRONG)
            .text_color(Palette::current().fg),
    )
}

fn text_link(on_press: impl Fn() + 'static) -> nsh_base::ButtonElement {
    button()
        .role(AccessibilityRole::Link)
        .cursor_pointer()
        .focus_visible(focus_ring)
        .on_press(on_press)
}

/// Volume capacity meter bar (styled `view`, not a `Progress`, with accessibility label per §5.2).
pub fn capacity_meter(
    used_percent: u8,
    low_space: bool,
    a11y_label: String,
    width_pt: f32,
) -> View {
    let p = Palette::current();
    let fill_color = if low_space {
        p.attn
    } else {
        p.muted.opacity(0.55)
    };
    let fraction = (f32::from(used_percent.min(100))) / 100.0;
    view()
        .w(px(width_pt))
        .h(px(5.0))
        .rounded(px(3.0))
        .bg(p.track)
        .overflow_hidden()
        .accessible(true)
        .label(a11y_label)
        .child(
            view()
                .h_full()
                .w(relative(fraction))
                .rounded(px(3.0))
                .bg(fill_color),
        )
}

/// Determinate or indeterminate progress bar respecting `reduce_motion()` (§4 & §8).
pub fn progress_bar(
    value_percent: Option<f32>,
    height_pt: f32,
    a11y_label: impl Into<SharedString>,
) -> nsh_base::ProgressElement {
    let p = Palette::current();
    let indeterminate = value_percent.is_none();
    let val = value_percent.unwrap_or(0.0);
    let reduced = reduce_motion();
    let a11y_label = a11y_label.into();
    let a11y_label = match value_percent {
        Some(percent) => format!("{a11y_label}, {percent:.0} percent").into(),
        None => a11y_label,
    };

    let radius = height_pt / 2.0;

    progress()
        .w_full()
        .value(val)
        .indeterminate(indeterminate)
        .accessibility_label(a11y_label)
        .track_style(move |s| {
            s.w_full()
                .h(px(height_pt))
                .rounded(px(radius))
                .bg(p.track)
                .overflow_hidden()
        })
        .indicator_style(move |s| {
            if indeterminate {
                if reduced {
                    s.w_full()
                        .h_full()
                        .rounded(px(radius))
                        .bg(p.run.opacity(0.65))
                } else {
                    s.w(relative(0.32)).h_full().rounded(px(radius)).bg(p.run)
                }
            } else {
                s.h_full().rounded(px(radius)).bg(p.primary)
            }
        })
}

/// Small gradient app glyph (the ONLY gradient in the app per §4).
pub fn brand_glyph(size_pt: f32) -> View {
    view()
        .w(px(size_pt))
        .h(px(size_pt))
        .rounded(px((size_pt * 0.27).round()))
        .bg_linear(deg(140.0))
        .gradient_from(AdaptiveColor::rgb(0x4f86f7, 0x4f86f7))
        .gradient_to(AdaptiveColor::rgb(0x2f62d9, 0x2f62d9))
        .items_center()
        .justify_center()
        .flex_none()
        .child(icon(
            IconKind::Sync,
            (size_pt * 0.64).round(),
            AdaptiveColor::rgb(0xffffff, 0xffffff),
        ))
}
