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
    MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Style, TextAlign, TextRun, UTF16Selection,
    UnderlineStyle, Window, WrappedLine,
};

use crate::text_input::{TextInput, TextTarget};

/// One `\n`-delimited logical line, shaped into one or more visual rows.
#[derive(Clone, Debug)]
struct ShapedTextLine {
    /// The wrapped shape; `WrappedLine` owns its wrap boundaries and glyphs.
    line: WrappedLine,
    /// The line's byte range in the buffer, excluding the trailing newline.
    start: usize,
    end: usize,
    /// Absolute byte offset where each visual row starts.
    row_starts: Vec<usize>,
    /// This line's first row in the field's global visual-row coordinates.
    first_row: usize,
}

impl ShapedTextLine {
    fn rows(&self) -> usize {
        self.row_starts.len()
    }

    fn row_end(&self, row: usize) -> usize {
        self.row_starts.get(row + 1).copied().unwrap_or(self.end)
    }
}

/// Where the last painted text went. Mouse hit-testing and the platform's
/// `bounds_for_range` need the wrapped lines from the previous frame.
#[derive(Clone, Debug)]
pub struct TextLayout {
    bounds: Bounds<Pixels>,
    lines: Vec<ShapedTextLine>,
    line_height: Pixels,
    /// First visible visual row (vertical scroll) for wrapped/multiline fields.
    scroll_row: usize,
    total_rows: usize,
    visible_rows: usize,
    /// Horizontal scroll for single-line, non-wrapping values.
    scroll_x: Pixels,
    /// Content width the lines were wrapped to.
    wrap_width: Pixels,
}

impl TextLayout {
    fn top_for_row(&self, row: usize) -> Pixels {
        self.bounds.top() + self.line_height * (row as f32 - self.scroll_row as f32)
    }

    fn index_for_position(&self, position: gpui::Point<Pixels>) -> usize {
        if self.lines.is_empty() {
            return 0;
        }
        let line_height = f32::from(self.line_height).max(1.0);
        let relative_y = f32::from(position.y - self.bounds.top());
        let row = ((relative_y / line_height).floor().max(0.0) as usize + self.scroll_row)
            .min(self.total_rows.saturating_sub(1));
        let line = self
            .lines
            .iter()
            .find(|line| row < line.first_row + line.rows())
            .unwrap_or_else(|| self.lines.last().expect("checked non-empty"));
        let row_in_line = row.saturating_sub(line.first_row);
        let x = position.x - self.bounds.left() + self.scroll_x;
        let local = match line.line.index_for_position(
            point(x, self.line_height * row_in_line as f32),
            self.line_height,
        ) {
            Ok(index) | Err(index) => index,
        };
        (line.start + local).min(line.end)
    }

    fn bounds_for_range(&self, range: &Range<usize>) -> Option<Bounds<Pixels>> {
        for line in &self.lines {
            if range.start <= line.end && range.end >= line.start {
                let len = line.end - line.start;
                let local_start = range.start.saturating_sub(line.start).min(len);
                let local_end = range.end.saturating_sub(line.start).min(len);
                let start = line
                    .line
                    .position_for_index(local_start, self.line_height)?;
                let end = line.line.position_for_index(local_end, self.line_height)?;
                let top = self.top_for_row(line.first_row);
                let left = self.bounds.left() - self.scroll_x;
                let same_row = start.y == end.y;
                let right = if same_row { end.x } else { self.wrap_width };
                let bottom = top + if same_row { start.y } else { end.y } + self.line_height;
                return Some(Bounds::from_corners(
                    point(left + start.x, top + start.y),
                    point(left + right, bottom),
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
    wrap: bool,
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
            wrap: false,
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

    /// Soft-wrap long values at the field's content width. Multi-line fields
    /// always wrap; single-line fields opt in so launch-sheet values stay
    /// readable instead of running past the right edge.
    pub fn wrap(mut self, wrap: bool) -> Self {
        self.wrap = wrap;
        self
    }

    /// Cap the painted height of a multiline field (default 8 rows).
    pub fn max_visible_lines(mut self, lines: usize) -> Self {
        self.max_visible_lines = lines;
        self
    }

    fn wraps(&self) -> bool {
        self.wrap || self.multiline
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
        _cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let host = self.host.clone();
        let target = self.target;
        let multiline = self.multiline;
        let wraps = self.wraps();
        let max_visible_lines = self.max_visible_lines;
        let placeholder = self.placeholder.clone();
        let text_style = window.text_style();

        let mut style = Style::default();
        style.size.width = gpui::relative(1.).into();
        let layout_id =
            window.request_measured_layout(style, move |known, available, window, cx| {
                let line_height = window.line_height();
                let available_width = known.width.or(match available.width {
                    gpui::AvailableSpace::Definite(width) => Some(width),
                    _ => None,
                });

                let value = host
                    .read(cx)
                    .text_input(target)
                    .map(|input| input.value().to_string())
                    .unwrap_or_default();
                let mut display = if value.is_empty() {
                    placeholder.clone().unwrap_or_default()
                } else {
                    value
                };
                if !multiline {
                    display = display.replace(['\n', '\r'], " ");
                }

                let font_size = text_style.font_size.to_pixels(window.rem_size());
                let shape = |text: &str, window: &mut Window| {
                    let runs = [text_style.to_run(text.len())];
                    let width = wraps.then_some(available_width).flatten();
                    window
                        .text_system()
                        .shape_text(text.to_string().into(), font_size, &runs, width, None)
                        .map(|lines| {
                            lines
                                .iter()
                                .map(|line| line.wrap_boundaries().len() + 1)
                                .sum::<usize>()
                        })
                        .unwrap_or(1)
                };
                let rows: usize = if multiline {
                    display
                        .split('\n')
                        .map(|line| shape(line, window))
                        .sum::<usize>()
                        .max(1)
                } else {
                    shape(&display, window)
                };
                let visible = if wraps || multiline {
                    rows.max(1).min(max_visible_lines)
                } else {
                    1
                };
                size(
                    available_width.unwrap_or(Pixels::ZERO),
                    line_height * visible as f32,
                )
            });
        (layout_id, ())
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
        let mut display = if use_placeholder {
            self.placeholder.clone().unwrap_or_default()
        } else {
            value.clone()
        };
        if !self.multiline {
            // One byte for one byte, so offsets still map onto the buffer.
            display = display.replace(['\n', '\r'], " ");
        }
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

        // Wrapping fields shape to the content width; the other fields keep a
        // single unwrapped line and scroll horizontally.
        let wrap_width = bounds.size.width;
        let shaping_width = self.wraps().then_some(wrap_width);
        let mut lines = Vec::with_capacity(line_count);
        let mut total_rows = 0;
        for line_index in 0..line_count {
            let line_text = raw_lines[line_index];
            let start = line_starts[line_index];
            let end = line_ends[line_index];
            let runs = text_runs(line_text, start, &run, marked.as_ref(), &marked_underline);
            let Ok(mut shaped) = window.text_system().shape_text(
                line_text.to_string().into(),
                font_size,
                &runs,
                shaping_width,
                None,
            ) else {
                continue;
            };
            let Some(shaped_line) = shaped.drain(..).next() else {
                continue;
            };
            let rows = shaped_line.wrap_boundaries().len() + 1;
            let mut row_starts = Vec::with_capacity(rows);
            for row in 0..rows {
                let local = match shaped_line
                    .index_for_position(point(px(0.), line_height * row as f32), line_height)
                {
                    Ok(index) | Err(index) => index,
                };
                row_starts.push(start + local.min(end.saturating_sub(start)));
            }
            lines.push(ShapedTextLine {
                line: shaped_line,
                start,
                end,
                row_starts,
                first_row: total_rows,
            });
            total_rows += rows;
        }

        let scrollable = self.wraps() || self.multiline;
        let visible = if scrollable {
            total_rows.max(1).min(self.max_visible_lines)
        } else {
            1
        };

        let caret_row = lines
            .get(caret_line.min(lines.len().saturating_sub(1)))
            .map(|line| {
                let local = caret
                    .saturating_sub(line.start)
                    .min(line.end.saturating_sub(line.start));
                let within = line
                    .line
                    .position_for_index(local, line_height)
                    .map(|position| {
                        (f32::from(position.y) / f32::from(line_height).max(1.0)).round() as usize
                    })
                    .unwrap_or(0);
                line.first_row + within
            })
            .unwrap_or(0);
        let scroll_row = if scrollable && caret_row >= visible {
            caret_row + 1 - visible
        } else {
            0
        };

        // Keep the caret inside the field on a long single-line value. Wrapped
        // fields grow instead of scrolling horizontally.
        let mut scroll_x = Pixels::ZERO;
        if active && !self.wraps() {
            if let Some(line) = lines.first() {
                let local = caret
                    .saturating_sub(line.start)
                    .min(line.end.saturating_sub(line.start));
                if let Some(position) = line.line.position_for_index(local, line_height) {
                    let width = bounds.size.width;
                    if position.x > width {
                        scroll_x = position.x - width + px(4.0);
                    }
                }
            }
        }

        let mut selections = Vec::new();
        let mut cursor = None;
        if active {
            for line in &lines {
                let selected_start = selection.start.clamp(line.start, line.end);
                let selected_end = selection.end.clamp(line.start, line.end);
                if selected_start < selected_end {
                    // A selection spanning a wrap paints one quad per row.
                    for row in 0..line.rows() {
                        let start_byte = selected_start.max(line.row_starts[row]);
                        let end_byte = selected_end.min(line.row_end(row));
                        if start_byte >= end_byte {
                            continue;
                        }
                        let x_start = line
                            .line
                            .position_for_index(start_byte - line.start, line_height)
                            .map(|position| position.x)
                            .unwrap_or(Pixels::ZERO);
                        let x_end = line
                            .line
                            .position_for_index(end_byte - line.start, line_height)
                            .map(|position| position.x)
                            .unwrap_or(Pixels::ZERO);
                        let max_x = bounds.size.width.max(Pixels::ZERO);
                        let x_start = (x_start - scroll_x).max(Pixels::ZERO).min(max_x);
                        let x_end = (x_end - scroll_x).max(Pixels::ZERO).min(max_x);
                        let top = bounds.top() + line_height * (line.first_row + row) as f32
                            - line_height * scroll_row as f32;
                        selections.push(fill(
                            Bounds::from_corners(
                                point(bounds.left() + x_start, top),
                                point(bounds.left() + x_end, top + line_height),
                            ),
                            self.selection_color,
                        ));
                    }
                }
            }

            if selection.is_empty() {
                if let Some(line) = lines.get(caret_line.min(lines.len().saturating_sub(1))) {
                    let local = caret
                        .saturating_sub(line.start)
                        .min(line.end.saturating_sub(line.start));
                    if let Some(position) = line.line.position_for_index(local, line_height) {
                        let within = (f32::from(position.y) / f32::from(line_height).max(1.0))
                            .round() as usize;
                        let global_row = line.first_row + within;
                        if global_row >= scroll_row && global_row < scroll_row + visible {
                            let max_x = (bounds.size.width - px(2.0)).max(Pixels::ZERO);
                            let x = (position.x - scroll_x).max(Pixels::ZERO).min(max_x);
                            cursor = Some(fill(
                                Bounds::new(
                                    point(
                                        bounds.left() + x,
                                        bounds.top()
                                            + line_height * (global_row - scroll_row) as f32,
                                    ),
                                    size(px(2.0), line_height),
                                ),
                                self.caret_color,
                            ));
                        }
                    }
                }
            }
        }

        let layout = TextLayout {
            bounds,
            lines,
            line_height,
            scroll_row,
            total_rows,
            visible_rows: visible,
            scroll_x,
            wrap_width,
        };
        let target = self.target;
        self.host.update(cx, |host, _| {
            host.text_edit_state_mut()
                .set_layout(target, layout.clone());
        });

        PrepaintState {
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
            for line in &prepaint.layout.lines {
                let last_visible = prepaint.layout.scroll_row + prepaint.layout.visible_rows;
                if line.first_row + line.rows() <= prepaint.layout.scroll_row
                    || line.first_row >= last_visible
                {
                    continue;
                }
                let origin = point(
                    bounds.left() - prepaint.layout.scroll_x,
                    prepaint.layout.top_for_row(line.first_row),
                );
                line.line
                    .paint(
                        origin,
                        prepaint.layout.line_height,
                        TextAlign::Left,
                        Some(bounds),
                        window,
                        cx,
                    )
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
        multiline: bool,
        wrap: bool,
        width: Pixels,
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
                multiline: true,
                wrap: false,
                width: px(240.0),
            }
        }

        /// A narrow single-line field whose value has to wrap.
        fn wrapping(cx: &mut Context<Self>, value: &str) -> Self {
            Self {
                focus: cx.focus_handle(),
                input: TextInput::new(value.to_string()),
                edit: TextEditState::default(),
                active: true,
                multiline: false,
                wrap: true,
                width: px(120.0),
            }
        }

        /// A narrow single-line field with everything selected.
        fn wrapping_selection(cx: &mut Context<Self>, value: &str) -> Self {
            let mut probe = Self::wrapping(cx, value);
            probe.input.select_all();
            probe
        }

        /// A narrow multi-line field whose caret sits at the end.
        fn wrapping_multiline(cx: &mut Context<Self>, value: &str) -> Self {
            let mut probe = Self::wrapping(cx, value);
            probe.multiline = true;
            probe
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
                    .w(self.width)
                    .debug_selector(|| "text-probe".to_string())
                    .child(
                        TextElement::new(
                            cx.entity(),
                            TextTarget::Palette,
                            self.focus.clone(),
                            theme,
                        )
                        .multiline(self.multiline)
                        .wrap(self.wrap),
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

    fn open_probe(
        cx: &mut TestAppContext,
        build: impl FnOnce(&mut Context<Probe>) -> Probe,
    ) -> (&'static mut VisualTestContext, Entity<Probe>) {
        let probe = cx.update(|cx| cx.new(build));
        let window = cx.update(|cx| {
            let probe = probe.clone();
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                        Point::default(),
                        size(px(400.0), px(400.0)),
                    ))),
                    ..Default::default()
                },
                |window, cx| {
                    window.focus(&probe.read(cx).focus);
                    probe
                },
            )
            .expect("opening the test window")
        });
        let cx = VisualTestContext::from_window(*window, cx).into_mut();
        // The first frame measures the wrapped rows, the next one reserves
        // their height.
        for _ in 0..3 {
            cx.run_until_parked();
        }
        (cx, probe)
    }

    #[gpui::test]
    fn a_long_single_line_value_wraps_and_keeps_the_caret_inside_the_field(
        cx: &mut TestAppContext,
    ) {
        let value = "the quick brown fox jumps over the lazy dog and keeps running far away";
        let (cx, probe) = open_probe(cx, |cx| Probe::wrapping(cx, value));

        let bounds = cx
            .debug_bounds("text-probe")
            .expect("the field is laid out");
        let (line_height, rows, caret_bounds) = cx.update(|window, cx| {
            let line_height = window.line_height();
            let probe = probe.read(cx);
            let layout = probe
                .edit
                .layout(TextTarget::Palette)
                .expect("prepaint cached the layout");
            let caret = probe.input.caret();
            let caret_range = probe.input.range_to_utf16(&(caret..caret));
            let caret_bounds = bounds_for_range(&probe.input, Some(layout), caret_range)
                .expect("the caret has IME bounds");
            (line_height, layout.total_rows, caret_bounds)
        });

        assert!(
            rows > 1,
            "a long value shapes into more than one visual row, got {rows}"
        );
        assert!(
            bounds.size.height > line_height * 1.5,
            "the field grows to show its wrapped value: {:?}",
            bounds.size
        );
        assert!(caret_bounds.top() >= bounds.top());
        assert!(caret_bounds.bottom() <= bounds.bottom() + px(0.5));
        assert!(caret_bounds.left() >= bounds.left() - px(0.5));
        assert!(caret_bounds.right() <= bounds.right() + px(0.5));
    }

    #[gpui::test]
    fn wrapped_hit_testing_and_selection_bounds_use_visual_rows(cx: &mut TestAppContext) {
        let value = "the quick brown fox jumps over the lazy dog and keeps running far away";
        let (cx, probe) = open_probe(cx, |cx| Probe::wrapping_selection(cx, value));

        let bounds = cx
            .debug_bounds("text-probe")
            .expect("the field is laid out");
        let (line_height, rows, selection_bounds, first_row, second_row) =
            cx.update(|window, cx| {
                let line_height = window.line_height();
                let probe = probe.read(cx);
                let layout = probe
                    .edit
                    .layout(TextTarget::Palette)
                    .expect("prepaint cached the layout");
                let whole = probe.input.range_to_utf16(&(0..probe.input.value().len()));
                let selection_bounds = bounds_for_range(&probe.input, Some(layout), whole)
                    .expect("the selection has IME bounds");
                let first_row = layout.index_for_position(point(
                    bounds.left() + px(2.0),
                    bounds.top() + line_height * 0.5,
                ));
                let second_row = layout.index_for_position(point(
                    bounds.left() + px(2.0),
                    bounds.top() + line_height * 1.5,
                ));
                (
                    line_height,
                    layout.total_rows,
                    selection_bounds,
                    first_row,
                    second_row,
                )
            });

        assert!(rows > 1, "the value wraps, got {rows} rows");
        assert!(
            selection_bounds.size.height > line_height * 1.5,
            "a selection across a wrap covers more than one row"
        );
        assert!(
            second_row > first_row,
            "a click on the second visual row maps past the first"
        );
    }

    #[gpui::test]
    fn a_multiline_field_grows_to_eight_rows_then_scrolls_to_the_caret(cx: &mut TestAppContext) {
        let value = (0..40)
            .map(|index| format!("line {index} with enough words to wrap"))
            .collect::<Vec<_>>()
            .join("\n");
        let (cx, probe) = open_probe(cx, |cx| Probe::wrapping_multiline(cx, &value));

        let bounds = cx
            .debug_bounds("text-probe")
            .expect("the field is laid out");
        let (line_height, rows, caret_bounds) = cx.update(|window, cx| {
            let line_height = window.line_height();
            let probe = probe.read(cx);
            let layout = probe
                .edit
                .layout(TextTarget::Palette)
                .expect("prepaint cached the layout");
            let caret = probe.input.caret();
            let caret_range = probe.input.range_to_utf16(&(caret..caret));
            let caret_bounds = bounds_for_range(&probe.input, Some(layout), caret_range)
                .expect("the caret has IME bounds");
            (line_height, layout.total_rows, caret_bounds)
        });

        assert!(
            rows > 8,
            "the value needs vertical scrolling, got {rows} rows"
        );
        assert!(
            (f32::from(bounds.size.height) - f32::from(line_height) * 8.0).abs() < 1.0,
            "the field caps at eight visual rows: {:?}",
            bounds.size
        );
        assert!(caret_bounds.top() >= bounds.top());
        assert!(caret_bounds.bottom() <= bounds.bottom() + px(0.5));
    }
}
