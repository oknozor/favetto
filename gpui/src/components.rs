//! Small presentation pieces shared by the shell and the route views.
//!
//! Everything here takes the active [`Theme`] and never hard-codes a color or
//! a spacing value (docs/design/gpui-client.md §3, Aesthetic-Usability).

use std::time::Duration;

use gpui::{
    div, prelude::*, px, transparent_black, Animation, AnimationExt, AnyElement, Div, ElementId,
    FocusHandle, Hsla, Rgba, Stateful, StyleRefinement,
};

use crate::state::ConnectionState;
use crate::theme::{Theme, MICRO_GAP};

/// One spinner revolution. Four quadrant frames step every 130 ms, so the
/// glyph moves at ~2 Hz — well under the WCAG flash threshold.
const SPINNER_DURATION: Duration = Duration::from_millis(520);

/// One pulse cycle: the dot fades down and back up over 1.2 s.
const PULSE_DURATION: Duration = Duration::from_millis(1200);

/// The spinner frame for a normalized animation delta.
///
/// The frames stay inside the quadrant family that already provides the static
/// `◐` running glyph, so the vocabulary does not change. The calculation is
/// pure so the frame math is unit-tested without a window.
pub fn spinner_glyph(delta: f32) -> &'static str {
    const FRAMES: [&str; 4] = ["◐", "◓", "◑", "◒"];
    let delta = normalized_delta(delta);
    let frame = (delta * FRAMES.len() as f32) as usize;
    FRAMES[frame.min(FRAMES.len() - 1)]
}

/// The opacity for a normalized animation delta: a triangle wave between `0.3`
/// and `1.0`. A slow pulse, never a flash.
pub fn pulse_opacity(delta: f32) -> f32 {
    0.3 + 0.7 * (1.0 - (normalized_delta(delta) * 2.0 - 1.0).abs())
}

/// Whether a live animation should run. Reduced motion always wins, and a
/// non-live state is static. Keeping this predicate pure lets the reduced-motion
/// guarantee be unit-tested without touching the process environment.
pub fn should_animate(live: bool, reduced_motion: bool) -> bool {
    live && !reduced_motion
}

fn normalized_delta(delta: f32) -> f32 {
    if delta.is_finite() {
        delta.rem_euclid(1.0)
    } else {
        0.0
    }
}

/// A stable animation element id namespaced by surface and a per-row key.
///
/// `with_animation` keeps its phase in element state keyed by this id: a
/// re-render must reuse it, and a virtualized list must not let the same key
/// stand for two different rows. Callers pass the task UUID or tool correlation
/// id, never the list index.
pub fn motion_id(surface: &'static str, key: impl std::fmt::Display) -> ElementId {
    ElementId::Name(gpui::SharedString::new(format!("{surface}-{key}")))
}

/// The turning `◐` used wherever a status glyph means "work in progress".
///
/// `icon` is the static glyph for every other state (and for reduced motion),
/// so a terminal state and a screenshot stay fully readable. The fixed-width
/// slot keeps the adjacent label from jittering by a pixel every frame.
pub fn animated_icon(
    icon: &'static str,
    running: bool,
    color: Rgba,
    text_size: f32,
    id: impl Into<ElementId>,
) -> AnyElement {
    let slot = div()
        .w(px(text_size))
        .flex_shrink_0()
        .text_center()
        .text_size(px(text_size))
        .text_color(color);

    if !should_animate(running, crate::motion::prefers_reduced_motion()) {
        return slot.child(icon).into_any_element();
    }

    slot.with_animation(
        id,
        Animation::new(SPINNER_DURATION).repeat(),
        |slot, delta| slot.child(spinner_glyph(delta)),
    )
    .into_any_element()
}

/// A small dot that pulses while `live` (the Follow tail and workflow
/// attention marker). Reduced motion and a non-live state render the static
/// dot immediately.
pub fn pulse_dot(color: Rgba, id: impl Into<ElementId>, live: bool) -> AnyElement {
    let dot = div()
        .w(px(6.0))
        .h(px(6.0))
        .flex_shrink_0()
        .rounded_full()
        .bg(color);
    pulse_element(dot, id, live)
}

/// A text glyph that pulses while `live` (the armed Follow dot). The glyph,
/// color and label stay in place; only the opacity moves.
pub fn pulse_glyph(
    glyph: &'static str,
    color: Rgba,
    text_size: f32,
    id: impl Into<ElementId>,
    live: bool,
) -> AnyElement {
    let glyph = div()
        .flex_shrink_0()
        .text_size(px(text_size))
        .text_color(color)
        .child(glyph);
    pulse_element(glyph, id, live)
}

fn pulse_element(element: Div, id: impl Into<ElementId>, live: bool) -> AnyElement {
    if !should_animate(live, crate::motion::prefers_reduced_motion()) {
        return element.into_any_element();
    }
    element
        .with_animation(
            id,
            Animation::new(PULSE_DURATION).repeat(),
            |element, delta| element.opacity(pulse_opacity(delta)),
        )
        .into_any_element()
}

/// The focus ring used by every interactive element. The transparent base
/// border reserves the 2px so focusing never reflows the layout.
pub fn focus_ring(theme: &Theme) -> impl FnOnce(StyleRefinement) -> StyleRefinement + use<'_> {
    move |style| style.border_color(theme.colors.accent)
}

/// A small keycap used for `⌘K`, `g i`, `↑` and friends.
pub fn keycap(theme: &Theme, keys: &str) -> Div {
    div()
        .px(px(theme.spacing.s1 + MICRO_GAP))
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
    // Only the retry loop is in flight; a connected or failed pill stays still.
    let retrying = matches!(
        connection,
        ConnectionState::Connecting | ConnectionState::Reconnecting { .. }
    );

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
        .child(animated_icon(
            symbol,
            retrying,
            color,
            theme.font.xs,
            "connection-pill",
        ))
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
///
/// Every interactive control routes through here or the equivalent
/// `.focus(..)` style. The minimum size is enforced here so a later view cannot
/// accidentally ship a target below the 32 px accessibility floor (docs §2).
pub fn with_focus<T: Styled + gpui::InteractiveElement>(element: T, theme: &Theme) -> T {
    element
        .min_h(px(theme.controls.min))
        .min_w(px(theme.controls.min))
        .border_2()
        .border_color(transparent_black())
        .focus(focus_ring(theme))
}

/// A small secondary action used to make an error message copyable. The caller
/// supplies the listener (the message and the toast copy live in the view).
pub fn copy_button(
    theme: &Theme,
    id: impl Into<ElementId>,
    label: &str,
    focus: &FocusHandle,
) -> Stateful<Div> {
    with_focus(
        div()
            .id(id.into())
            .flex()
            .items_center()
            .gap(px(theme.spacing.s1))
            .h(px(theme.controls.min))
            .px(px(theme.spacing.s2))
            .rounded(px(theme.radii.r4))
            .border_1()
            .border_color(theme.colors.border)
            .text_size(px(theme.font.xs))
            .text_color(theme.colors.text)
            .cursor_pointer()
            .hover(|style| style.bg(theme.colors.bg))
            .track_focus(focus)
            .child(label.to_string()),
        theme,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spinner_frames_cycle_through_the_quadrant_family() {
        assert_eq!(spinner_glyph(0.0), "◐");
        assert_eq!(spinner_glyph(0.26), "◓");
        assert_eq!(spinner_glyph(0.51), "◑");
        assert_eq!(spinner_glyph(0.76), "◒");
        // A full turn wraps to the first frame instead of falling off the end.
        assert_eq!(spinner_glyph(1.0), "◐");
        assert_eq!(spinner_glyph(f32::NAN), "◐");
    }

    #[test]
    fn pulse_stays_between_point_three_and_one() {
        assert_eq!(pulse_opacity(0.0), 0.3);
        assert_eq!(pulse_opacity(0.5), 1.0);
        assert_eq!(pulse_opacity(1.0), 0.3);
        for step in 0..=1_000 {
            let opacity = pulse_opacity(step as f32 / 1_000.0);
            assert!(
                (0.3..=1.0).contains(&opacity),
                "pulse opacity {opacity} left [0.3, 1.0] at step {step}"
            );
        }
    }

    #[test]
    fn reduced_motion_always_returns_the_static_state() {
        assert!(!should_animate(true, true));
        assert!(!should_animate(false, true));
        assert!(!should_animate(false, false));
        assert!(should_animate(true, false));
    }

    #[test]
    fn motion_ids_are_stable_and_namespaced_per_surface_and_key() {
        assert_eq!(
            motion_id("task-spinner", 7),
            motion_id("task-spinner", 7),
            "the same row must reuse its animation phase"
        );
        assert_ne!(
            motion_id("task-spinner", 7),
            motion_id("task-spinner", 8),
            "recycled rows must never inherit another row's phase"
        );
        assert_ne!(
            motion_id("task-spinner", 7),
            motion_id("inbox-spinner", 7),
            "two surfaces showing the same task keep independent phases"
        );
    }
}
