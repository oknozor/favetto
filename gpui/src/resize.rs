//! Shared drag-to-resize state for the right-hand reading panes.
//!
//! Both the task-detail column and the Catalog source preview follow the same
//! interaction: grab an invisible hit area on the pane's left border and drag.
//! The pointer math lives here so the two panes cannot drift apart; each view
//! still owns its width, its clamp and whether the handle is rendered.

use gpui::{div, prelude::*, px, Context, IntoElement, Render, Window};

/// Pointer and pane width captured when a resize drag starts.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResizeStart {
    pointer_x: f32,
    width: f32,
}

impl ResizeStart {
    pub fn new(pointer_x: f32, width: f32) -> Self {
        Self { pointer_x, width }
    }

    /// The width requested by moving the pointer to `pointer_x`.
    ///
    /// A right-hand pane grows when the pointer moves left, so the delta is
    /// inverted relative to the pane's left border.
    pub fn requested_width(&self, pointer_x: f32) -> f32 {
        self.width + (self.pointer_x - pointer_x)
    }
}

/// Marker payload for a pane-resize drag; no data is needed because the start
/// state lives in [`ResizeStart`].
#[derive(Clone, Copy, Debug)]
pub struct ResizeDrag;

/// Zero-sized drag preview. `on_drag` requires a view to render while dragging;
/// a resize has no ghost, so this renders nothing.
pub struct ResizePreview;

impl Render for ResizePreview {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().w(px(0.0)).h(px(0.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dragging_left_grows_the_pane_and_right_shrinks_it() {
        let start = ResizeStart::new(1_000.0, 350.0);

        assert_eq!(start.requested_width(1_000.0), 350.0);
        assert_eq!(start.requested_width(900.0), 450.0);
        assert_eq!(start.requested_width(1_100.0), 250.0);
    }
}
