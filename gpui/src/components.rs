//! Small presentation pieces shared by the shell and the route views.
//!
//! Everything here takes the active [`Theme`] and never hard-codes a color or
//! a spacing value (docs/design/gpui-client.md §3, Aesthetic-Usability).

use gpui::{div, prelude::*, px, transparent_black, Div, Hsla, StyleRefinement};

use crate::state::ConnectionState;
use crate::theme::Theme;

/// The focus ring used by every interactive element. The transparent base
/// border reserves the 2px so focusing never reflows the layout.
pub fn focus_ring(theme: &Theme) -> impl FnOnce(StyleRefinement) -> StyleRefinement + use<'_> {
    move |style| style.border_color(theme.colors.accent)
}

/// A small keycap used for `⌘K`, `g i`, `↑` and friends.
pub fn keycap(theme: &Theme, keys: &str) -> Div {
    div()
        .px(px(theme.spacing.s1 + 2.0))
        .py(px(2.0))
        .rounded(px(theme.radii.r4))
        .border_1()
        .border_color(theme.colors.border)
        .bg(theme.colors.surface)
        .font_family("monospace")
        .text_size(px(theme.font.xs))
        .text_color(theme.colors.muted)
        .child(keys.to_string())
}

/// A faint section label such as `CONNECTION` or `NEEDS YOU`.
pub fn section_label(theme: &Theme, label: &str) -> Div {
    div()
        .text_size(px(theme.font.xs))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(theme.colors.muted)
        .child(label.to_string())
}

/// The connection pill in the top bar. Status is never color-only: the symbol
/// and the label are always rendered beside the semantic color (docs §3).
pub fn connection_pill(theme: &Theme, connection: &ConnectionState) -> Div {
    let (symbol, label, color) = match connection {
        ConnectionState::Connecting => ("◌", "Connecting".to_string(), theme.colors.warning),
        ConnectionState::Connected => ("●", "Connected".to_string(), theme.colors.success),
        ConnectionState::Reconnecting { attempt, .. } => (
            "◌",
            format!("Reconnecting ({attempt})"),
            theme.colors.warning,
        ),
        ConnectionState::Unauthorized { .. } => {
            ("●", "Unauthorized".to_string(), theme.colors.danger)
        }
        ConnectionState::Disconnected { .. } => {
            ("●", "Disconnected".to_string(), theme.colors.danger)
        }
    };

    div()
        .flex()
        .items_center()
        .gap(px(theme.spacing.s1 + 2.0))
        .px(px(theme.spacing.s2))
        .py(px(3.0))
        .rounded(px(theme.radii.r12))
        .border_1()
        .border_color(Hsla::from(color).opacity(0.45))
        .bg(Hsla::from(color).opacity(0.12))
        .child(
            div()
                .text_color(color)
                .text_size(px(theme.font.xs))
                .child(symbol),
        )
        .child(
            div()
                .text_color(theme.colors.text)
                .text_size(px(theme.font.xs))
                .child(label),
        )
}

/// The calm, intentional empty state used by routes whose rows arrive in later
/// steps. It names the surface, what belongs there, and the primary action —
/// never a "TODO".
pub fn empty_state(
    theme: &Theme,
    icon: &str,
    eyebrow: &str,
    title: &str,
    body: &str,
    primary_hint: &str,
) -> Div {
    div()
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(theme.spacing.s3))
        .p(px(theme.spacing.s6))
        .child(
            div()
                .flex()
                .items_center()
                .justify_center()
                .w(px(48.0))
                .h(px(48.0))
                .rounded(px(theme.radii.r12))
                .border_1()
                .border_color(Hsla::from(theme.colors.accent).opacity(0.35))
                .bg(Hsla::from(theme.colors.accent).opacity(0.12))
                .text_color(theme.colors.accent)
                .text_size(px(theme.font.xl))
                .child(icon.to_string()),
        )
        .child(section_label(theme, eyebrow))
        .child(
            div()
                .text_size(px(theme.font.lg))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(theme.colors.text)
                .text_center()
                .child(title.to_string()),
        )
        .child(
            div()
                .max_w(px(480.0))
                .text_size(px(theme.font.sm))
                .text_color(theme.colors.muted)
                .text_center()
                .child(body.to_string()),
        )
        .child(
            div()
                .mt(px(theme.spacing.s1))
                .flex()
                .items_center()
                .gap(px(theme.spacing.s2))
                .px(px(theme.spacing.s2))
                .py(px(theme.spacing.s1))
                .rounded(px(theme.radii.r4))
                .border_1()
                .border_color(theme.colors.border)
                .text_size(px(theme.font.xs))
                .text_color(theme.colors.muted)
                .child(
                    div()
                        .text_color(theme.colors.accent)
                        .child("Primary action"),
                )
                .child(div().child("·"))
                .child(div().child(primary_hint.to_string())),
        )
}

/// One keyboard-shortcut row: label on the left, keycap on the right.
pub fn shortcut_row(theme: &Theme, label: &str, keys: &str) -> Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .h(px(32.0))
        .px(px(theme.spacing.s2))
        .rounded(px(theme.radii.r4))
        .child(
            div()
                .text_size(px(theme.font.sm))
                .text_color(theme.colors.text)
                .child(label.to_string()),
        )
        .child(keycap(theme, keys))
}

/// Visible focus ring for an already-built button.
pub fn with_focus<T: Styled + gpui::InteractiveElement>(element: T, theme: &Theme) -> T {
    element
        .border_2()
        .border_color(transparent_black())
        .focus(focus_ring(theme))
}
