//! A read-only, mouse-selectable text element.
//!
//! The pinned `gpui 0.2.2` has no selectable-text primitive. [`TextElement`]
//! (the editable counterpart) already shows how to shape, hit-test, paint a
//! selection and register window-level drag handlers; this element reuses that
//! shape without a caret or an input handler. Selection state lives in
//! [`crate::selection`] (GPUI-free) and is stored on [`ClientState`] so the
//! shell can copy it without a focused text field.

use std::ops::Range;

use gpui::{
    fill, point, px, size, App, Bounds, DispatchPhase, Element, ElementId, GlobalElementId, Hsla,
    InspectorElementId, IntoElement, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, PaintQuad, Pixels, Style, TextAlign, TextRun, TextStyle, Window, WrappedLine,
};

use crate::selection::TextSelection;
use crate::state::ClientState;
use crate::theme::Theme;

/// One `\n`-delimited line, shaped into one or more visual rows.
#[derive(Clone, Debug)]
struct SelectableLine {
    line: WrappedLine,
    /// Absolute byte range in the buffer, excluding the trailing newline.
    start: usize,
    end: usize,
    /// Absolute byte offset where each visual row starts.
    row_starts: Vec<usize>,
    /// This line's first row in the element's global visual-row coordinates.
    first_row: usize,
}

impl SelectableLine {
    fn rows(&self) -> usize {
        self.row_starts.len()
    }

    fn row_end(&self, row: usize) -> usize {
        self.row_starts.get(row + 1).copied().unwrap_or(self.end)
    }
}

/// The shaped layout captured during prepaint.
///
/// Mouse hit-testing runs from window-level handlers, so the most recent layout
/// must outlive the paint pass; [`ClientState`] owns one per selection key.
#[derive(Clone, Debug)]
pub struct SelectableTextLayout {
    bounds: Bounds<Pixels>,
    lines: Vec<SelectableLine>,
    line_height: Pixels,
    total_rows: usize,
}

impl SelectableTextLayout {
    /// The byte offset nearest `position` (window coordinates).
    pub fn index_for_position(&self, position: gpui::Point<Pixels>) -> usize {
        if self.lines.is_empty() {
            return 0;
        }
        let line_height = f32::from(self.line_height).max(1.0);
        let relative_y = f32::from(position.y - self.bounds.top()).max(0.0);
        let row =
            ((relative_y / line_height).floor() as usize).min(self.total_rows.saturating_sub(1));
        let line = self
            .lines
            .iter()
            .find(|line| row < line.first_row + line.rows())
            .unwrap_or_else(|| self.lines.last().expect("checked non-empty"));
        let row_in_line = row.saturating_sub(line.first_row);
        let x = position.x - self.bounds.left();
        let local = match line.line.index_for_position(
            point(x, self.line_height * row_in_line as f32),
            self.line_height,
        ) {
            Ok(index) | Err(index) => index,
        };
        (line.start + local).min(line.end)
    }

    fn selection_quads(&self, selection: &TextSelection, color: Hsla) -> Vec<PaintQuad> {
        let range = selection.range();
        if range.is_empty() {
            return Vec::new();
        }
        let mut quads = Vec::new();
        for line in &self.lines {
            let selected_start = range.start.clamp(line.start, line.end);
            let selected_end = range.end.clamp(line.start, line.end);
            if selected_start >= selected_end {
                continue;
            }
            for row in 0..line.rows() {
                let start_byte = selected_start.max(line.row_starts[row]);
                let end_byte = selected_end.min(line.row_end(row));
                if start_byte >= end_byte {
                    continue;
                }
                let x_start = line
                    .line
                    .position_for_index(start_byte - line.start, self.line_height)
                    .map(|position| position.x)
                    .unwrap_or(Pixels::ZERO);
                let x_end = line
                    .line
                    .position_for_index(end_byte - line.start, self.line_height)
                    .map(|position| position.x)
                    .unwrap_or(Pixels::ZERO);
                let max_x = self.bounds.size.width.max(Pixels::ZERO);
                let x_start = x_start.max(Pixels::ZERO).min(max_x);
                let x_end = x_end.max(x_start).min(max_x);
                let top = self.bounds.top() + self.line_height * (line.first_row + row) as f32;
                quads.push(fill(
                    Bounds::from_corners(
                        point(self.bounds.left() + x_start, top),
                        point(self.bounds.left() + x_end, top + self.line_height),
                    ),
                    color,
                ));
            }
        }
        quads
    }
}

/// One read-only, selectable text region.
pub struct SelectableText {
    state: gpui::Entity<ClientState>,
    key: u64,
    text: String,
    runs: Vec<TextRun>,
    style: TextStyle,
    selection_color: Hsla,
    links: Vec<(Range<usize>, String)>,
}

impl SelectableText {
    pub fn new(
        state: &gpui::Entity<ClientState>,
        key: u64,
        text: String,
        runs: Vec<TextRun>,
        style: &TextStyle,
        theme: &Theme,
    ) -> Self {
        Self {
            state: state.clone(),
            key,
            text,
            runs,
            style: style.clone(),
            selection_color: Hsla::from(theme.colors.accent).opacity(0.28),
            links: Vec::new(),
        }
    }

    /// Openable link ranges, in the same byte coordinates as `text`.
    pub fn links(mut self, links: Vec<(Range<usize>, String)>) -> Self {
        self.links = links;
        self
    }

    fn register_mouse_handlers(&self, bounds: Bounds<Pixels>, window: &mut Window) {
        let state = self.state.clone();
        let key = self.key;
        let text = self.text.clone();
        window.on_mouse_event(move |event: &MouseDownEvent, phase, _window, cx| {
            if phase != DispatchPhase::Bubble
                || event.button != MouseButton::Left
                || !bounds.contains(&event.position)
            {
                return;
            }
            let offset = state
                .read(cx)
                .selectable_layout(key)
                .map(|layout| layout.index_for_position(event.position));
            let Some(offset) = offset else {
                return;
            };
            state.update(cx, |state, cx| {
                state.begin_read_only_selection(
                    key,
                    &text,
                    offset,
                    event.click_count,
                    event.modifiers.shift,
                );
                cx.notify();
            });
            cx.stop_propagation();
        });

        let drag_state = self.state.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, _phase, _window, cx| {
            if !event.dragging() {
                return;
            }
            let offset = drag_state
                .read(cx)
                .selectable_layout(key)
                .map(|layout| layout.index_for_position(event.position));
            let Some(offset) = offset else {
                return;
            };
            drag_state.update(cx, |state, cx| {
                if state.read_only_selection_dragging() != Some(key) {
                    return;
                }
                state.extend_read_only_selection(key, offset);
                cx.notify();
            });
        });

        let up_state = self.state.clone();
        let links = self.links.clone();
        window.on_mouse_event(move |event: &MouseUpEvent, _phase, _window, cx| {
            if event.button != MouseButton::Left {
                return;
            }
            let offset = up_state
                .read(cx)
                .selectable_layout(key)
                .map(|layout| layout.index_for_position(event.position));
            up_state.update(cx, |state, _cx| {
                state.end_read_only_selection_drag(key);
            });
            if event.click_count != 1 {
                return;
            }
            let Some(offset) = offset else {
                return;
            };
            let collapsed = up_state
                .read(cx)
                .read_only_selection(key)
                .is_none_or(TextSelection::is_empty);
            if !collapsed {
                return;
            }
            if let Some((_, url)) = links
                .iter()
                .find(|(range, _)| range.contains(&offset))
                .cloned()
            {
                cx.open_url(&url);
            }
        });
    }
}

impl IntoElement for SelectableText {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for SelectableText {
    type RequestLayoutState = ();
    type PrepaintState = Vec<PaintQuad>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        _cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let text = self.text.clone();
        let runs = self.runs.clone();
        let style = self.style.clone();

        let mut measured = Style::default();
        measured.size.width = gpui::relative(1.).into();
        let layout_id =
            window.request_measured_layout(measured, move |known, available, window, _cx| {
                let rem = window.rem_size();
                let line_height = style.line_height_in_pixels(rem);
                let font_size = style.font_size.to_pixels(rem);
                let available_width = known.width.or(match available.width {
                    gpui::AvailableSpace::Definite(width) => Some(width),
                    _ => None,
                });
                let rows = shape_lines(
                    &text,
                    &runs,
                    font_size,
                    line_height,
                    available_width,
                    window,
                )
                .map(|lines| lines.iter().map(SelectableLine::rows).sum::<usize>().max(1))
                .unwrap_or(1);
                size(
                    available_width.unwrap_or(Pixels::ZERO),
                    line_height * rows as f32,
                )
            });
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let rem = window.rem_size();
        let line_height = self.style.line_height_in_pixels(rem);
        let font_size = self.style.font_size.to_pixels(rem);
        let lines = shape_lines(
            &self.text,
            &self.runs,
            font_size,
            line_height,
            Some(bounds.size.width),
            window,
        )
        .unwrap_or_default();
        let total_rows: usize = lines.iter().map(SelectableLine::rows).sum();
        let layout = SelectableTextLayout {
            bounds,
            lines,
            line_height,
            total_rows,
        };

        let selection = self
            .state
            .read(cx)
            .read_only_selection(self.key)
            .cloned()
            .unwrap_or_default();
        let quads = layout.selection_quads(&selection, self.selection_color);

        let state = self.state.clone();
        let key = self.key;
        let text = self.text.clone();
        state.update(cx, |state, _| {
            state.set_selectable_layout(key, layout);
            state.sync_read_only_selection_text(key, &text);
        });
        quads
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let mask = gpui::ContentMask { bounds };
        window.with_content_mask(Some(mask), |window| {
            for quad in prepaint.drain(..) {
                window.paint_quad(quad);
            }
            let Some(layout) = self.state.read(cx).selectable_layout(self.key).cloned() else {
                return;
            };
            for line in &layout.lines {
                let origin = point(
                    bounds.left(),
                    bounds.top() + layout.line_height * line.first_row as f32,
                );
                line.line
                    .paint(
                        origin,
                        layout.line_height,
                        TextAlign::Left,
                        Some(bounds),
                        window,
                        cx,
                    )
                    .ok();
            }
        });

        self.register_mouse_handlers(bounds, window);
    }
}

fn shape_lines(
    text: &str,
    runs: &[TextRun],
    font_size: Pixels,
    line_height: Pixels,
    wrap_width: Option<Pixels>,
    window: &mut Window,
) -> Option<Vec<SelectableLine>> {
    let shaped = window
        .text_system()
        .shape_text(text.to_string().into(), font_size, runs, wrap_width, None)
        .ok()?;
    let mut offset = 0;
    let mut first_row = 0;
    let mut lines = Vec::with_capacity(shaped.len());
    for line in shaped {
        let end = offset + line.text.len();
        let rows = line.wrap_boundaries().len() + 1;
        let mut row_starts = Vec::with_capacity(rows);
        for row in 0..rows {
            let local = match line
                .index_for_position(point(px(0.0), line_height * row as f32), line_height)
            {
                Ok(index) | Err(index) => index,
            };
            row_starts.push(offset + local.min(line.text.len()));
        }
        lines.push(SelectableLine {
            line,
            start: offset,
            end,
            row_starts,
            first_row,
        });
        first_row += rows;
        offset = end + 1;
    }
    Some(lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::prelude::*;
    use gpui::{
        div, point, px, size, AppContext, Bounds, Modifiers, MouseButton, Render, TestAppContext,
        VisualTestContext, WindowOptions,
    };

    struct Probe;

    impl Render for Probe {
        fn render(
            &mut self,
            _window: &mut Window,
            _cx: &mut gpui::Context<Self>,
        ) -> impl IntoElement {
            div()
        }
    }

    fn layout_for(
        cx: &mut TestAppContext,
        text: &str,
        width: f32,
    ) -> (SelectableTextLayout, usize) {
        let window = cx.update(|cx| {
            cx.open_window(WindowOptions::default(), |_window, cx| cx.new(|_| Probe))
                .expect("opening the test window")
        });
        let cx = VisualTestContext::from_window(*window, cx).into_mut();
        cx.update(|window, _cx| {
            let style = TextStyle::default();
            let font_size = px(14.0);
            let line_height = px(16.0);
            let runs = vec![style.to_run(text.len())];
            let lines = shape_lines(text, &runs, font_size, line_height, Some(px(width)), window)
                .expect("shaping succeeds");
            let total_rows = lines.iter().map(SelectableLine::rows).sum();
            (
                SelectableTextLayout {
                    bounds: Bounds::new(point(px(0.0), px(0.0)), size(px(width), px(200.0))),
                    lines,
                    line_height,
                    total_rows,
                },
                text.len(),
            )
        })
    }

    #[gpui::test]
    fn hit_testing_maps_visual_rows_to_byte_offsets(cx: &mut TestAppContext) {
        let (layout, len) = layout_for(cx, "one two\nthree four", 200.0);
        assert_eq!(layout.index_for_position(point(px(0.0), px(4.0))), 0);
        assert_eq!(layout.index_for_position(point(px(0.0), px(20.0))), 8);
        assert_eq!(layout.index_for_position(point(px(1_000.0), px(0.0))), 7);
        assert_eq!(
            layout.index_for_position(point(px(1_000.0), px(10_000.0))),
            len
        );

        let selection = TextSelection::new(0, 3);
        assert!(!layout.selection_quads(&selection, gpui::black()).is_empty());
        assert!(layout
            .selection_quads(&TextSelection::at(3), gpui::black())
            .is_empty());
    }

    #[gpui::test]
    fn wrapped_lines_report_one_selection_quad_per_row(cx: &mut TestAppContext) {
        let (layout, _) = layout_for(
            cx,
            "the quick brown fox jumps over the lazy dog and keeps running far away",
            80.0,
        );
        assert!(layout.total_rows > 1, "the text wraps");
        let selection =
            TextSelection::new(0, layout.lines.last().map(|line| line.end).unwrap_or(0));
        let quads = layout.selection_quads(&selection, gpui::black());
        assert_eq!(quads.len(), layout.total_rows);
    }

    struct SelectionProbe {
        state: gpui::Entity<crate::state::ClientState>,
    }

    impl Render for SelectionProbe {
        fn render(
            &mut self,
            _window: &mut Window,
            _cx: &mut gpui::Context<Self>,
        ) -> impl IntoElement {
            let theme = crate::theme::Theme::for_appearance(gpui::WindowAppearance::Dark);
            let style = TextStyle {
                font_size: px(14.0).into(),
                color: gpui::white(),
                ..TextStyle::default()
            };
            let text = "hello world\nsecond line".to_string();
            let runs = vec![style.to_run(text.len())];
            div()
                .size_full()
                .p(px(8.0))
                .debug_selector(|| "selectable-probe".to_string())
                .child(SelectableText::new(
                    &self.state,
                    7,
                    text,
                    runs,
                    &style,
                    theme,
                ))
        }
    }

    #[gpui::test]
    fn a_drag_records_a_read_only_selection(cx: &mut TestAppContext) {
        let state = cx.update(|cx| cx.new(|_| crate::state::ClientState::test_instance()));
        let probe = cx.update(|cx| {
            let state = state.clone();
            cx.new(|_| SelectionProbe { state })
        });
        let window = cx.update(|cx| {
            let probe = probe.clone();
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(gpui::WindowBounds::Windowed(Bounds::new(
                        point(px(0.0), px(0.0)),
                        size(px(400.0), px(200.0)),
                    ))),
                    ..Default::default()
                },
                |_window, _cx| probe,
            )
            .expect("opening the test window")
        });
        let cx = VisualTestContext::from_window(*window, cx).into_mut();
        for _ in 0..3 {
            cx.run_until_parked();
        }

        let bounds = cx
            .debug_bounds("selectable-probe")
            .expect("the probe is laid out");
        let start = point(bounds.left() + px(12.0), bounds.top() + px(12.0));
        let end = point(bounds.left() + px(120.0), bounds.top() + px(12.0));
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(end, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::default());
        cx.run_until_parked();

        let selection = cx
            .update(|_window, cx| state.read(cx).read_only_selection(7).cloned())
            .expect("the drag started a selection");
        assert!(!selection.is_empty());
        assert!(cx.update(|_window, cx| state.read(cx).selectable_layout(7).is_some()));
    }
}
