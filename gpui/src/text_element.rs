//! A real GPUI text element: shapes the buffer, paints the caret/selection and
//! registers an [`EntityInputHandler`] so the platform can send IME, dead keys
//! and clipboard paste.
//!
//! The published `gpui` crate does not export a reusable input widget, but its
//! `examples/input.rs` documents the required pieces. This is the shared
//! implementation for every input in the client; views only choose a
//! [`TextTarget`], the inherited text style and the placeholder.

use std::collections::HashMap;
use std::ops::Range;

use gpui::{
    fill, point, px, size, App, Bounds, DispatchPhase, Element, ElementId, ElementInputHandler,
    Entity, FocusHandle, GlobalElementId, Hsla, IntoElement, LayoutId, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, ShapedLine, Style, TextRun, UTF16Selection,
    UnderlineStyle, Window,
};

use crate::text_input::{TextInput, TextTarget};

/// Where the last painted text went. Mouse hit-testing and the platform's
/// `bounds_for_range` need the shaped lines from the previous frame.
#[derive(Clone, Debug)]
pub struct TextLayout {
    bounds: Bounds<Pixels>,
    lines: Vec<ShapedLine>,
    line_starts: Vec<usize>,
    line_ends: Vec<usize>,
    line_height: Pixels,
    /// Horizontal scroll for single-line values longer than the field.
    scroll_x: Pixels,
}

impl TextLayout {
    fn index_for_position(&self, position: gpui::Point<Pixels>) -> usize {
        if self.lines.is_empty() {
            return 0;
        }
        let line_height = f32::from(self.line_height).max(1.0);
        let relative_y = f32::from(position.y - self.bounds.top());
        let line_index = (relative_y / line_height).floor().max(0.0) as usize;
        let line_index = line_index.min(self.lines.len() - 1);
        let x = position.x - self.bounds.left() + self.scroll_x;
        let local = self.lines[line_index].closest_index_for_x(x);
        let offset = self.line_starts[line_index] + local;
        offset.min(self.line_ends[line_index])
    }

    fn bounds_for_range(&self, range: &Range<usize>) -> Option<Bounds<Pixels>> {
        for (index, line) in self.lines.iter().enumerate() {
            let start = self.line_starts[index];
            let end = self.line_ends[index];
            if range.start <= end && range.end >= start {
                let local_start = range.start.saturating_sub(start).min(end - start);
                let local_end = range.end.saturating_sub(start).min(end - start);
                let top = self.bounds.top() + self.line_height * index as f32;
                return Some(Bounds::from_corners(
                    point(
                        self.bounds.left() + line.x_for_index(local_start) - self.scroll_x,
                        top,
                    ),
                    point(
                        self.bounds.left() + line.x_for_index(local_end) - self.scroll_x,
                        top + self.line_height,
                    ),
                ));
            }
        }
        None
    }
}

/// Layout caches and transient drag state owned by a text host.
#[derive(Default)]
pub struct TextEditState {
    layouts: HashMap<TextTarget, TextLayout>,
    dragging: Option<TextTarget>,
}

impl TextEditState {
    pub(crate) fn layout(&self, target: TextTarget) -> Option<&TextLayout> {
        self.layouts.get(&target)
    }

    pub(crate) fn set_layout(&mut self, target: TextTarget, layout: TextLayout) {
        self.layouts.insert(target, layout);
    }

    pub(crate) fn dragging(&self) -> Option<TextTarget> {
        self.dragging
    }

    pub(crate) fn set_dragging(&mut self, target: Option<TextTarget>) {
        self.dragging = target;
    }
}

/// The state a [`TextElement`] needs from its owning view/entity.
pub trait TextHost: 'static {
    fn text_input(&self, target: TextTarget) -> Option<&TextInput>;
    fn text_input_mut(&mut self, target: TextTarget) -> Option<&mut TextInput>;
    fn active_text_target(&self) -> Option<TextTarget>;
    fn focus_text_target(&mut self, target: TextTarget);
    fn after_text_edit(&mut self, target: TextTarget);
    fn text_edit_state(&self) -> &TextEditState;
    fn text_edit_state_mut(&mut self) -> &mut TextEditState;
}

pub struct PrepaintState {
    lines: Vec<(ShapedLine, Pixels)>,
    selection: Vec<PaintQuad>,
    cursor: Option<PaintQuad>,
    layout: TextLayout,
}

/// One editable line or block, backed by a [`TextHost`] buffer.
pub struct TextElement<V: TextHost> {
    host: Entity<V>,
    target: TextTarget,
    focus_handle: FocusHandle,
    placeholder: Option<String>,
    placeholder_color: Hsla,
    text_color: Hsla,
    caret_color: Hsla,
    selection_color: Hsla,
    multiline: bool,
    max_visible_lines: usize,
}

impl<V: TextHost> TextElement<V> {
    pub fn new(
        host: Entity<V>,
        target: TextTarget,
        focus_handle: FocusHandle,
        theme: &crate::theme::Theme,
    ) -> Self {
        Self {
            host,
            target,
            focus_handle,
            placeholder: None,
            placeholder_color: theme.colors.muted.into(),
            text_color: theme.colors.text.into(),
            caret_color: theme.colors.accent.into(),
            selection_color: Hsla::from(theme.colors.accent).opacity(0.25),
            multiline: false,
            max_visible_lines: 8,
        }
    }

    pub fn placeholder(mut self, placeholder: impl Into<String>) -> Self {
        self.placeholder = Some(placeholder.into());
        self
    }

    pub fn multiline(mut self, multiline: bool) -> Self {
        self.multiline = multiline;
        self
    }

    fn register_mouse_handlers(&self, bounds: Bounds<Pixels>, window: &mut Window) {
        let host = self.host.clone();
        let target = self.target;
        let focus_handle = self.focus_handle.clone();
        window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble
                || event.button != MouseButton::Left
                || !bounds.contains(&event.position)
            {
                return;
            }
            window.focus(&focus_handle);
            host.update(cx, |host, cx| {
                host.focus_text_target(target);
                if let Some(index) = host
                    .text_edit_state()
                    .layout(target)
                    .map(|layout| layout.index_for_position(event.position))
                {
                    if let Some(input) = host.text_input_mut(target) {
                        if event.modifiers.shift {
                            input.select_to(index);
                        } else {
                            input.move_to(index);
                        }
                    }
                }
                host.text_edit_state_mut().set_dragging(Some(target));
                host.after_text_edit(target);
                cx.notify();
            });
            cx.stop_propagation();
        });

        let drag_host = self.host.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, _phase, _window, cx| {
            if !event.dragging() {
                return;
            }
            drag_host.update(cx, |host, cx| {
                if host.text_edit_state().dragging() != Some(target) {
                    return;
                }
                if let Some(index) = host
                    .text_edit_state()
                    .layout(target)
                    .map(|layout| layout.index_for_position(event.position))
                {
                    if let Some(input) = host.text_input_mut(target) {
                        input.select_to(index);
                    }
                    host.after_text_edit(target);
                    cx.notify();
                }
            });
        });

        let up_host = self.host.clone();
        window.on_mouse_event(move |_: &MouseUpEvent, _phase, _window, cx| {
            up_host.update(cx, |host, _| {
                if host.text_edit_state().dragging() == Some(target) {
                    host.text_edit_state_mut().set_dragging(None);
                }
            });
        });
    }
}

impl<V: TextHost + gpui::EntityInputHandler> IntoElement for TextElement<V> {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl<V: TextHost + gpui::EntityInputHandler> Element for TextElement<V> {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let line_count = self
            .host
            .read(cx)
            .text_input(self.target)
            .map(|input| input.value().split('\n').count())
            .unwrap_or(1);
        let visible = if self.multiline {
            line_count.min(self.max_visible_lines)
        } else {
            1
        };
        let mut style = Style::default();
        style.size.width = gpui::relative(1.).into();
        style.size.height = (window.line_height() * visible as f32).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let (input, active) = {
            let host = self.host.read(cx);
            (
                host.text_input(self.target).cloned().unwrap_or_default(),
                host.active_text_target() == Some(self.target),
            )
        };
        let text_style = window.text_style();
        let line_height = window.line_height();
        let font_size = text_style.font_size.to_pixels(window.rem_size());
        let value = input.value().to_string();
        let use_placeholder = value.is_empty() && self.placeholder.is_some();
        let display = if use_placeholder {
            self.placeholder.clone().unwrap_or_default()
        } else {
            value.clone()
        };
        let color = if use_placeholder {
            self.placeholder_color
        } else {
            self.text_color
        };
        let selection = if use_placeholder {
            0..0
        } else {
            input.selected_range().clone()
        };
        let marked = if use_placeholder {
            None
        } else {
            input.marked_range().cloned()
        };
        let caret = input.caret().min(display.len());

        let raw_lines: Vec<&str> = if self.multiline {
            display.split('\n').collect()
        } else {
            vec![display.as_str()]
        };
        let line_count = raw_lines.len().max(1);
        let caret_line = if self.multiline {
            display[..caret.min(display.len())]
                .matches('\n')
                .count()
                .min(line_count - 1)
        } else {
            0
        };
        let visible = if self.multiline {
            line_count.min(self.max_visible_lines)
        } else {
            1
        };
        let first_line = if self.multiline && caret_line >= visible {
            caret_line + 1 - visible
        } else {
            0
        };

        let mut line_starts = Vec::with_capacity(line_count);
        let mut line_ends = Vec::with_capacity(line_count);
        let mut offset = 0;
        for (index, line) in raw_lines.iter().enumerate() {
            line_starts.push(offset);
            line_ends.push(offset + line.len());
            offset += line.len();
            if index + 1 < raw_lines.len() {
                offset += 1; // the '\n'
            }
        }

        let run = TextRun {
            len: 0,
            font: text_style.font(),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let marked_underline = UnderlineStyle {
            color: Some(self.caret_color),
            thickness: px(1.0),
            wavy: false,
        };

        let mut lines = Vec::with_capacity(visible);
        let mut selections = Vec::new();
        let mut cursor = None;
        let mut scroll_x = Pixels::ZERO;
        for line_index in first_line..(first_line + visible).min(line_count) {
            let line_text = raw_lines[line_index];
            let start = line_starts[line_index];
            let end = line_ends[line_index];
            let runs = text_runs(line_text, start, &run, marked.as_ref(), &marked_underline);
            let shaped = window.text_system().shape_line(
                line_text.to_string().into(),
                font_size,
                &runs,
                None,
            );
            let top = bounds.top() + line_height * (line_index - first_line) as f32;

            if active {
                let selected_start = selection.start.clamp(start, end);
                let selected_end = selection.end.clamp(start, end);
                if selected_start < selected_end {
                    let x_start = shaped.x_for_index(selected_start - start);
                    let x_end = shaped.x_for_index(selected_end - start);
                    selections.push(fill(
                        Bounds::from_corners(
                            point(bounds.left() + x_start, top),
                            point(bounds.left() + x_end, top + line_height),
                        ),
                        self.selection_color,
                    ));
                }
            }

            if active && selection.is_empty() && caret >= start && caret <= end {
                let x = shaped.x_for_index(caret - start);
                cursor = Some(fill(
                    Bounds::new(point(bounds.left() + x, top), size(px(2.0), line_height)),
                    self.caret_color,
                ));
            }
            lines.push((shaped, top));
        }

        // Keep the caret inside the field on a long single-line value.
        if active && !self.multiline {
            if let Some((line, _)) = lines.first() {
                let caret_x = line.x_for_index(caret.min(line.text.len()));
                let width = bounds.size.width;
                if caret_x > width {
                    scroll_x = caret_x - width + px(4.0);
                }
            }
            if let Some(cursor_quad) = cursor.as_mut() {
                cursor_quad.bounds.origin.x -= scroll_x;
            }
            for quad in &mut selections {
                quad.bounds.origin.x -= scroll_x;
            }
        }

        let shaped_lines: Vec<ShapedLine> = lines.iter().map(|(line, _)| line.clone()).collect();
        let visible_end = (first_line + visible).min(line_count);
        let layout = TextLayout {
            bounds,
            lines: shaped_lines,
            line_starts: line_starts[first_line..visible_end].to_vec(),
            line_ends: line_ends[first_line..visible_end].to_vec(),
            line_height,
            scroll_x,
        };
        let target = self.target;
        self.host.update(cx, |host, _| {
            host.text_edit_state_mut()
                .set_layout(target, layout.clone());
        });

        PrepaintState {
            lines,
            selection: selections,
            cursor,
            layout,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let mask = gpui::ContentMask { bounds };
        window.with_content_mask(Some(mask), |window| {
            for quad in prepaint.selection.drain(..) {
                window.paint_quad(quad);
            }
            for (line, top) in &prepaint.lines {
                let origin = point(bounds.left() - prepaint.layout.scroll_x, *top);
                line.paint(origin, prepaint.layout.line_height, window, cx)
                    .ok();
            }
            if self.focus_handle.is_focused(window) {
                if let Some(cursor) = prepaint.cursor.take() {
                    window.paint_quad(cursor);
                }
            }
        });

        self.register_mouse_handlers(bounds, window);
        if self.host.read(cx).active_text_target() == Some(self.target) {
            window.handle_input(
                &self.focus_handle,
                ElementInputHandler::new(bounds, self.host.clone()),
                cx,
            );
        }
    }
}

fn text_runs(
    line_text: &str,
    line_start: usize,
    base: &TextRun,
    marked: Option<&Range<usize>>,
    underline: &UnderlineStyle,
) -> Vec<TextRun> {
    let Some(marked) = marked else {
        return vec![TextRun {
            len: line_text.len(),
            ..base.clone()
        }];
    };
    let line_end = line_start + line_text.len();
    let start = marked.start.clamp(line_start, line_end) - line_start;
    let end = marked.end.clamp(line_start, line_end) - line_start;
    if start >= end {
        return vec![TextRun {
            len: line_text.len(),
            ..base.clone()
        }];
    }
    [
        TextRun {
            len: start,
            ..base.clone()
        },
        TextRun {
            len: end - start,
            underline: Some(*underline),
            ..base.clone()
        },
        TextRun {
            len: line_text.len() - end,
            ..base.clone()
        },
    ]
    .into_iter()
    .filter(|run| run.len > 0)
    .collect()
}

/// Build an element backed by the client state. Views use this so the focus
/// handle the shell keeps while a route-level field is active (and the shaped
/// layout cache) are wired consistently.
pub fn state_text_element<T: 'static>(
    state: &Entity<crate::state::ClientState>,
    target: TextTarget,
    theme: &crate::theme::Theme,
    cx: &mut gpui::Context<T>,
) -> TextElement<crate::state::ClientState> {
    let focus = state
        .read(cx)
        .shell_focus()
        .unwrap_or_else(|| cx.focus_handle());
    TextElement::new(state.clone(), target, focus, theme)
}

/// The platform-facing helper shared by [`crate::state::ClientState`] and the
/// palette owner. Keeping the UTF-16 math here means both handlers behave
/// identically.
pub fn text_for_range(
    input: &TextInput,
    range_utf16: Range<usize>,
    actual_range: &mut Option<Range<usize>>,
) -> Option<String> {
    let (text, actual) = input.text_for_range_utf16(range_utf16);
    actual_range.replace(actual);
    Some(text)
}

pub fn selected_text_range(input: &TextInput) -> UTF16Selection {
    let (range, reversed) = input.selected_text_range_utf16();
    UTF16Selection { range, reversed }
}

pub fn marked_text_range(input: &TextInput) -> Option<Range<usize>> {
    input.marked_text_range_utf16()
}

pub fn bounds_for_range(
    input: &TextInput,
    layout: Option<&TextLayout>,
    range_utf16: Range<usize>,
) -> Option<Bounds<Pixels>> {
    let layout = layout?;
    let range = input.range_from_utf16(&range_utf16);
    layout.bounds_for_range(&range)
}

pub fn character_index_for_point(
    input: &TextInput,
    layout: Option<&TextLayout>,
    point: gpui::Point<Pixels>,
) -> Option<usize> {
    let layout = layout?;
    let utf8 = layout.index_for_position(point);
    let utf8 = input.snap_boundary(utf8);
    Some(input.range_to_utf16(&(utf8..utf8)).start)
}

/// Replace text from the platform, keeping single-line fields on one line.
pub fn replace_text_in_range(
    input: &mut TextInput,
    range_utf16: Option<Range<usize>>,
    text: &str,
    multiline: bool,
) {
    let text = sanitize(text, multiline);
    input.replace_text_in_range_utf16(range_utf16, &text);
}

pub fn replace_and_mark_text_in_range(
    input: &mut TextInput,
    range_utf16: Option<Range<usize>>,
    text: &str,
    selected_range_utf16: Option<Range<usize>>,
    multiline: bool,
) {
    let text = sanitize(text, multiline);
    input.replace_and_mark_text_in_range_utf16(range_utf16, &text, selected_range_utf16);
}

fn sanitize(text: &str, multiline: bool) -> String {
    if multiline {
        text.to_string()
    } else {
        text.replace(['\n', '\r'], " ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Density, ThemeChoice};
    use gpui::{
        div, prelude::*, px, size, Bounds, Point, TestAppContext, VisualTestContext, WindowBounds,
        WindowOptions,
    };

    /// A tiny real view that owns one buffer, so the element is exercised
    /// through GPUI's real layout, shape and paint path without the daemon.
    struct Probe {
        focus: FocusHandle,
        input: TextInput,
        edit: TextEditState,
        active: bool,
    }

    impl Probe {
        fn new(cx: &mut Context<Self>) -> Self {
            let mut input = TextInput::new("one\ntwo\nthree".to_string());
            input.move_to(2);
            input.select_to(8);
            Self {
                focus: cx.focus_handle(),
                input,
                edit: TextEditState::default(),
                active: true,
            }
        }
    }

    impl TextHost for Probe {
        fn text_input(&self, target: TextTarget) -> Option<&TextInput> {
            (target == TextTarget::Palette).then_some(&self.input)
        }

        fn text_input_mut(&mut self, target: TextTarget) -> Option<&mut TextInput> {
            (target == TextTarget::Palette).then_some(&mut self.input)
        }

        fn active_text_target(&self) -> Option<TextTarget> {
            self.active.then_some(TextTarget::Palette)
        }

        fn focus_text_target(&mut self, _target: TextTarget) {}

        fn after_text_edit(&mut self, _target: TextTarget) {}

        fn text_edit_state(&self) -> &TextEditState {
            &self.edit
        }

        fn text_edit_state_mut(&mut self) -> &mut TextEditState {
            &mut self.edit
        }
    }

    impl gpui::EntityInputHandler for Probe {
        fn text_for_range(
            &mut self,
            range_utf16: std::ops::Range<usize>,
            actual_range: &mut Option<std::ops::Range<usize>>,
            _window: &mut Window,
            _cx: &mut Context<Self>,
        ) -> Option<String> {
            crate::text_element::text_for_range(&self.input, range_utf16, actual_range)
        }

        fn selected_text_range(
            &mut self,
            _ignore_disabled_input: bool,
            _window: &mut Window,
            _cx: &mut Context<Self>,
        ) -> Option<UTF16Selection> {
            Some(crate::text_element::selected_text_range(&self.input))
        }

        fn marked_text_range(
            &self,
            _window: &mut Window,
            _cx: &mut Context<Self>,
        ) -> Option<std::ops::Range<usize>> {
            crate::text_element::marked_text_range(&self.input)
        }

        fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
            self.input.unmark_text();
        }

        fn replace_text_in_range(
            &mut self,
            range_utf16: Option<std::ops::Range<usize>>,
            text: &str,
            _window: &mut Window,
            _cx: &mut Context<Self>,
        ) {
            crate::text_element::replace_text_in_range(&mut self.input, range_utf16, text, false);
        }

        fn replace_and_mark_text_in_range(
            &mut self,
            range_utf16: Option<std::ops::Range<usize>>,
            text: &str,
            selected_range_utf16: Option<std::ops::Range<usize>>,
            _window: &mut Window,
            _cx: &mut Context<Self>,
        ) {
            crate::text_element::replace_and_mark_text_in_range(
                &mut self.input,
                range_utf16,
                text,
                selected_range_utf16,
                false,
            );
        }

        fn bounds_for_range(
            &mut self,
            range_utf16: std::ops::Range<usize>,
            _element_bounds: Bounds<Pixels>,
            _window: &mut Window,
            _cx: &mut Context<Self>,
        ) -> Option<Bounds<Pixels>> {
            crate::text_element::bounds_for_range(
                &self.input,
                self.edit.layout(TextTarget::Palette),
                range_utf16,
            )
        }

        fn character_index_for_point(
            &mut self,
            point: Point<Pixels>,
            _window: &mut Window,
            _cx: &mut Context<Self>,
        ) -> Option<usize> {
            crate::text_element::character_index_for_point(
                &self.input,
                self.edit.layout(TextTarget::Palette),
                point,
            )
        }
    }

    impl Render for Probe {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let theme = crate::theme::Theme::for_settings(
                window.appearance(),
                ThemeChoice::Dark,
                Density::Comfortable,
            );
            div().size_full().p(px(10.0)).text_size(px(14.0)).child(
                div()
                    .w(px(240.0))
                    .debug_selector(|| "text-probe".to_string())
                    .child(
                        TextElement::new(
                            cx.entity(),
                            TextTarget::Palette,
                            self.focus.clone(),
                            theme,
                        )
                        .multiline(true),
                    ),
            )
        }
    }

    #[gpui::test]
    fn the_element_paints_a_multiline_selection(cx: &mut TestAppContext) {
        let window = cx.update(|cx| {
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                        Point::default(),
                        size(px(400.0), px(200.0)),
                    ))),
                    ..Default::default()
                },
                |window, cx| {
                    let probe = cx.new(Probe::new);
                    window.focus(&probe.read(cx).focus);
                    probe
                },
            )
            .expect("opening the test window")
        });
        let cx = VisualTestContext::from_window(*window, cx).into_mut();
        cx.run_until_parked();

        let bounds = cx
            .debug_bounds("text-probe")
            .expect("the field is laid out");
        assert!(bounds.size.width > px(0.0));
        assert!(bounds.size.height > px(0.0));
    }
}
