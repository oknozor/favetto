//! Small presentation pieces shared by the shell and the route views.
//!
//! Everything here takes the active [`Theme`] and never hard-codes a color or
//! a spacing value (docs/design/gpui-client.md §3, Aesthetic-Usability).

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    div, point, prelude::*, px, transparent_black, Animation, AnimationExt, AnyElement, App, Div,
    DragMoveEvent, ElementId, FocusHandle, Hsla, ListState, MouseButton, MouseDownEvent, Render,
    Rgba, ScrollHandle, Stateful, StyleRefinement, Window,
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
    empty_state_with_icon(
        theme,
        icon.to_string().into_any_element(),
        eyebrow,
        title,
        body,
        primary_hint,
    )
}

/// Like [`empty_state`], but the glyph spins while `running` (the Agent panel's
/// "starting the session…" state). Under reduced motion the glyph is static.
pub fn animated_empty_state(
    theme: &Theme,
    icon: &'static str,
    running: bool,
    eyebrow: &str,
    title: &str,
    body: &str,
    primary_hint: &str,
) -> Div {
    let glyph = animated_icon(
        icon,
        running,
        theme.colors.accent,
        theme.font.xl,
        motion_id("empty-state-spinner", title),
    );
    empty_state_with_icon(theme, glyph, eyebrow, title, body, primary_hint)
}

fn empty_state_with_icon(
    theme: &Theme,
    icon: AnyElement,
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
                .child(icon),
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
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(theme.font.sm))
                .text_color(theme.colors.text)
                .child(label.to_string()),
        )
        .child(div().flex_none().child(keycap(theme, keys)))
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

/// The visible scrollbar column in px. The pinned `gpui 0.2.2` paints no
/// scrollbar of its own, so the overlay below draws a slim track and thumb.
const SCROLLBAR_WIDTH: f32 = 8.0;

/// Inset from the pane's right edge, so the bar reads as an overlay rather
/// than a border.
const SCROLLBAR_INSET: f32 = 2.0;

/// The floor for the thumb height: a long document must still leave a target
/// large enough to grab.
const SCROLLBAR_MIN_THUMB: f32 = 24.0;

/// Thumb geometry for one viewport/content/offset snapshot, in viewport px.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrollbarThumb {
    /// Distance from the top of the track.
    pub top: f32,
    /// Visible thumb height.
    pub height: f32,
}

/// The values a scrollbar needs, normalized to positive pixels from the top.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrollbarMetrics {
    /// Visible viewport height.
    pub viewport: f32,
    /// Total content height (viewport + maximum offset).
    pub content: f32,
    /// Current scroll distance from the top, `0..=max_offset`.
    pub offset: f32,
}

impl ScrollbarMetrics {
    /// The furthest the content can scroll.
    pub fn max_offset(self) -> f32 {
        (finite(self.content) - finite(self.viewport)).max(0.0)
    }

    /// The thumb position and height, or `None` while the content fits.
    pub fn thumb(self, min_height: f32) -> Option<ScrollbarThumb> {
        let viewport = finite(self.viewport);
        let max = self.max_offset();
        if viewport <= 0.0 || max <= 0.0 {
            return None;
        }
        let height = (viewport * viewport / finite(self.content))
            .max(finite(min_height))
            .min(viewport);
        let travel = viewport - height;
        let offset = finite(self.offset).min(max);
        let top = if travel > 0.0 {
            offset / max * travel
        } else {
            0.0
        };
        Some(ScrollbarThumb { top, height })
    }

    /// The scroll offset requested by a thumb whose top is `thumb_top`.
    ///
    /// Pure so dragging can be unit-tested without a window. Values outside
    /// the track clamp to the ends instead of overshooting.
    pub fn offset_for_thumb_top(self, thumb_height: f32, thumb_top: f32) -> f32 {
        let viewport = finite(self.viewport);
        let max = self.max_offset();
        let travel = viewport - finite(thumb_height);
        if max <= 0.0 || travel <= 0.0 {
            return 0.0;
        }
        (finite(thumb_top) / travel).clamp(0.0, 1.0) * max
    }

    /// The offset requested by clicking the track at `click_y`: the thumb
    /// centres under the pointer, so the click lands where the reader aimed.
    pub fn offset_for_track_click(self, thumb_height: f32, click_y: f32) -> f32 {
        self.offset_for_thumb_top(thumb_height, finite(click_y) - finite(thumb_height) / 2.0)
    }

    /// Whether the viewport is within `threshold` px of the tail.
    pub fn at_bottom(self, threshold: f32) -> bool {
        let max = self.max_offset();
        max <= 0.0 || finite(self.offset) >= max - finite(threshold)
    }
}

/// The scroll container driving a [`vertical_scrollbar`].
///
/// The two GPUI scroll sources expose slightly different offset APIs; the
/// adapter keeps that difference out of every view.
#[derive(Clone, Debug)]
pub enum ScrollbarSource {
    /// A plain `overflow_y_scroll` pane tracked by a [`ScrollHandle`].
    Handle(ScrollHandle),
    /// A virtualized [`ListState`], whose scrollbar helpers freeze the content
    /// height for the duration of a drag.
    List(ListState),
}

impl ScrollbarSource {
    /// Snapshot the normalized scroll metrics.
    pub fn metrics(&self) -> ScrollbarMetrics {
        match self {
            Self::Handle(handle) => {
                let viewport = f32::from(handle.bounds().size.height);
                let max = f32::from(handle.max_offset().height);
                ScrollbarMetrics {
                    viewport,
                    content: viewport + max,
                    offset: -f32::from(handle.offset().y),
                }
                .normalized()
            }
            Self::List(list) => {
                let viewport = f32::from(list.viewport_bounds().size.height);
                let max = f32::from(list.max_offset_for_scrollbar().height);
                ScrollbarMetrics {
                    viewport,
                    content: viewport + max,
                    offset: -f32::from(list.scroll_px_offset_for_scrollbar().y),
                }
                .normalized()
            }
        }
    }

    /// The scroll viewport bounds in window coordinates, used to map pointer
    /// positions onto the track.
    fn viewport_bounds(&self) -> gpui::Bounds<gpui::Pixels> {
        match self {
            Self::Handle(handle) => handle.bounds(),
            Self::List(list) => list.viewport_bounds(),
        }
    }

    /// Move the content so `offset` px are scrolled past the top.
    fn set_offset(&self, offset: f32) {
        let offset = finite(offset);
        match self {
            Self::Handle(handle) => handle.set_offset(point(px(0.0), px(-offset))),
            Self::List(list) => list.set_offset_from_scrollbar(point(px(0.0), px(-offset))),
        }
    }

    fn begin_drag(&self) {
        if let Self::List(list) = self {
            list.scrollbar_drag_started();
        }
    }

    fn end_drag(&self) {
        if let Self::List(list) = self {
            list.scrollbar_drag_ended();
        }
    }

    /// Whether the source is within `threshold` px of the tail. The detail view
    /// uses this to pause/resume follow from a scrollbar drag.
    pub fn at_bottom(&self, threshold: f32) -> bool {
        self.metrics().at_bottom(threshold)
    }
}

/// The live grab offset shared by a bar's thumb and its drag listeners.
///
/// `None` means idle; `Some(grab)` stores where inside the thumb the pointer
/// grabbed it, so the thumb does not jump on the first move.
#[derive(Clone, Debug, Default)]
pub struct ScrollbarDrag(Rc<Cell<Option<f32>>>);

impl ScrollbarDrag {
    pub fn new() -> Self {
        Self::default()
    }

    fn begin(&self, grab: f32) {
        self.0.set(Some(finite(grab)));
    }

    fn end(&self) -> bool {
        self.0.take().is_some()
    }

    fn grab(&self) -> Option<f32> {
        self.0.get()
    }
}

/// The notify callback shared by every scrollbar event handler.
type ScrollbarChange = Rc<dyn Fn(&mut Window, &mut App)>;

/// Zero-sized drag preview: a scrollbar drag has no ghost to paint.
struct ScrollbarPreview;

impl Render for ScrollbarPreview {
    fn render(
        &mut self,
        _window: &mut Window,
        _cx: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        div().w(px(0.0)).h(px(0.0))
    }
}

/// A slim vertical scrollbar overlay for a pane that already scrolls.
///
/// Returns `None` while the content fits, so no track or thumb is painted. The
/// caller places the result inside a `.relative()` container: the bar is an
/// absolutely positioned overlay and never reflows or rewraps the pane.
///
/// The thumb follows every source of scrolling because its geometry is derived
/// from the source each render. Dragging, clicking the track and (where the
/// view wires it) wheel/keyboard scrolling therefore share one scroll offset.
/// `on_change` runs after a click or drag moves the source so the view can
/// notify/render and, in the detail Output tab, pause/resume follow.
pub fn vertical_scrollbar(
    theme: &Theme,
    id: impl Into<gpui::SharedString>,
    source: ScrollbarSource,
    drag: &ScrollbarDrag,
    on_change: impl Fn(&mut Window, &mut App) + 'static,
) -> Option<Div> {
    let metrics = source.metrics();
    let thumb = metrics.thumb(SCROLLBAR_MIN_THUMB)?;
    let viewport_top = f32::from(source.viewport_bounds().origin.y);
    let bar_name = id.into();
    let on_change: ScrollbarChange = Rc::new(on_change);

    let track_id = ElementId::Name(gpui::SharedString::new(format!("{bar_name}-track")));
    let thumb_id = ElementId::Name(gpui::SharedString::new(format!("{bar_name}-thumb")));

    let thumb_grab = drag.clone();
    let thumb_source = source.clone();
    let thumb_drag = drag.clone();
    let thumb_top = thumb.top;
    let thumb_element = div()
        .id(thumb_id)
        .absolute()
        .top(px(thumb.top))
        .left(px(1.0))
        .right(px(1.0))
        .h(px(thumb.height))
        .rounded(px(theme.radii.r4))
        .bg(Hsla::from(theme.colors.muted).opacity(0.75))
        .cursor_pointer()
        .hover(|style| style.bg(theme.colors.accent))
        .on_mouse_down(
            MouseButton::Left,
            move |event: &MouseDownEvent, _window, _cx| {
                thumb_grab.begin(f32::from(event.position.y) - viewport_top - thumb_top);
                thumb_source.begin_drag();
            },
        )
        .on_drag(thumb_drag, |_drag, _offset, _window, cx| {
            cx.new(|_| ScrollbarPreview)
        });

    let track_source = source.clone();
    let track_drag = drag.clone();
    let track_click = on_change.clone();
    let track_element = div()
        .id(track_id)
        .absolute()
        .top(px(0.0))
        .bottom(px(0.0))
        .right(px(SCROLLBAR_INSET))
        .w(px(SCROLLBAR_WIDTH))
        .rounded(px(theme.radii.r4))
        .bg(Hsla::from(theme.colors.border).opacity(0.5))
        .cursor_pointer()
        .on_mouse_down(
            MouseButton::Left,
            move |event: &MouseDownEvent, window, cx| {
                // The thumb's listener runs first and marks the drag; a press
                // on the thumb must not also page the track.
                if track_drag.grab().is_some() {
                    return;
                }
                let click_y = f32::from(event.position.y) - viewport_top;
                let offset = metrics.offset_for_track_click(thumb.height, click_y);
                track_source.set_offset(offset);
                track_click(window, cx);
            },
        )
        .child(thumb_element);

    let end_drag = drag.clone();
    let end_source = source.clone();
    let end_change = on_change.clone();
    let move_drag = drag.clone();
    let move_source = source.clone();
    let move_change = on_change.clone();
    Some(
        div()
            .absolute()
            .top_0()
            .bottom_0()
            .right_0()
            .w(px(SCROLLBAR_WIDTH + SCROLLBAR_INSET))
            .on_mouse_up(
                MouseButton::Left,
                finish_drag(end_drag.clone(), end_source.clone(), end_change.clone()),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                finish_drag(end_drag, end_source, end_change),
            )
            .on_drag_move::<ScrollbarDrag>(
                move |event: &DragMoveEvent<ScrollbarDrag>, window, cx| {
                    let Some(grab) = move_drag.grab() else {
                        return;
                    };
                    let metrics = move_source.metrics();
                    let Some(thumb) = metrics.thumb(SCROLLBAR_MIN_THUMB) else {
                        return;
                    };
                    let thumb_top = f32::from(event.event.position.y) - viewport_top - grab;
                    let offset = metrics.offset_for_thumb_top(thumb.height, thumb_top);
                    move_source.set_offset(offset);
                    move_change(window, cx);
                },
            )
            .child(track_element),
    )
}

/// Settle a drag once: the `up` and `up_out` listeners share the same logic.
fn finish_drag(
    drag: ScrollbarDrag,
    source: ScrollbarSource,
    on_change: ScrollbarChange,
) -> impl Fn(&gpui::MouseUpEvent, &mut Window, &mut App) + 'static {
    move |_event, window, cx| {
        if drag.end() {
            source.end_drag();
            on_change(window, cx);
        }
    }
}

impl ScrollbarMetrics {
    fn normalized(self) -> Self {
        let viewport = finite(self.viewport);
        let content = finite(self.content).max(viewport);
        let offset = finite(self.offset).min((content - viewport).max(0.0));
        Self {
            viewport,
            content,
            offset,
        }
    }
}

fn finite(value: f32) -> f32 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    }
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

    fn metrics(viewport: f32, content: f32, offset: f32) -> ScrollbarMetrics {
        ScrollbarMetrics {
            viewport,
            content,
            offset,
        }
    }

    #[test]
    fn no_thumb_when_the_content_fits() {
        assert!(metrics(400.0, 400.0, 0.0).thumb(24.0).is_none());
        assert!(metrics(400.0, 320.0, 0.0).thumb(24.0).is_none());
        assert!(metrics(0.0, 900.0, 0.0).thumb(24.0).is_none());
    }

    #[test]
    fn thumb_is_proportional_and_reaches_both_ends() {
        let top = metrics(100.0, 400.0, 0.0).thumb(24.0).unwrap();
        // 1/4 of the content is visible, so the thumb is a quarter of the track.
        assert_eq!(top.height, 25.0);
        assert_eq!(top.top, 0.0);

        let middle = metrics(100.0, 400.0, 150.0).thumb(24.0).unwrap();
        assert_eq!(middle.top, 37.5);

        let bottom = metrics(100.0, 400.0, 300.0).thumb(24.0).unwrap();
        assert_eq!(bottom.top, 75.0);
        assert_eq!(bottom.top + bottom.height, 100.0);
    }

    #[test]
    fn a_very_long_document_keeps_a_grabbable_thumb() {
        let thumb = metrics(100.0, 10_000.0, 0.0).thumb(24.0).unwrap();
        assert_eq!(thumb.height, 24.0);
        assert!(thumb.height <= 100.0);
    }

    #[test]
    fn dragging_maps_the_thumb_back_onto_the_offset() {
        let metrics = metrics(100.0, 400.0, 0.0);
        assert_eq!(metrics.offset_for_thumb_top(25.0, 0.0), 0.0);
        assert_eq!(metrics.offset_for_thumb_top(25.0, 75.0), 300.0);
        assert_eq!(metrics.offset_for_thumb_top(25.0, 37.5), 150.0);
        // Overshoot clamps to the ends instead of jumping past them.
        assert_eq!(metrics.offset_for_thumb_top(25.0, -40.0), 0.0);
        assert_eq!(metrics.offset_for_thumb_top(25.0, 400.0), 300.0);
    }

    #[test]
    fn track_clicks_land_the_thumb_under_the_pointer() {
        let metrics = metrics(100.0, 400.0, 0.0);
        // Clicking the middle of the track centres the 25 px thumb there.
        assert_eq!(metrics.offset_for_track_click(25.0, 50.0), 150.0);
        assert_eq!(metrics.offset_for_track_click(25.0, 0.0), 0.0);
        assert_eq!(metrics.offset_for_track_click(25.0, 100.0), 300.0);
    }

    #[test]
    fn follow_threshold_only_triggers_at_the_tail() {
        assert!(metrics(100.0, 100.0, 0.0).at_bottom(24.0));
        assert!(metrics(100.0, 400.0, 300.0).at_bottom(24.0));
        assert!(metrics(100.0, 400.0, 280.0).at_bottom(24.0));
        assert!(!metrics(100.0, 400.0, 200.0).at_bottom(24.0));
    }

    #[test]
    fn metrics_clamp_invalid_and_out_of_range_values() {
        let normalized = metrics(100.0, 400.0, 9_999.0).normalized();
        assert_eq!(normalized.offset, 300.0);
        let inverted = metrics(100.0, 20.0, 10.0).normalized();
        assert_eq!(inverted.content, 100.0);
        assert_eq!(inverted.offset, 0.0);
        let nan = metrics(f32::NAN, f32::NAN, f32::NAN).normalized();
        assert_eq!(nan, metrics(0.0, 0.0, 0.0));
    }
}
