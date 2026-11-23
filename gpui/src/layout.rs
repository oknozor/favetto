//! Responsive layout decisions for the shell.
//!
//! The design plan asks for `container_query` (docs/design/gpui-client.md §1).
//! The pinned `gpui 0.2.2` build exposes no container-query primitive, so the
//! root container's width is read from the window viewport — which is identical
//! for the full-window shell — behind one named function. `gpui/README.md`
//! records the deviation.

use gpui::{px, Pixels};

use crate::theme::{
    CATALOG_PREVIEW_MIN_WIDTH, DETAIL_MIN_WIDTH, NARROW_BREAKPOINT, PANE_MIN_GAP, SIDEBAR_WIDTH,
};

/// Which layout the current width calls for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutMode {
    /// Sidebar + working list + detail column.
    Wide,
    /// Sidebar + working list; detail is pushed as a page.
    Narrow,
}

/// The shell's container query: compare the available width with the narrow
/// breakpoint. Kept pure so the thresholds are unit-tested.
pub fn container_query(width: Pixels) -> LayoutMode {
    if width < px(NARROW_BREAKPOINT) {
        LayoutMode::Narrow
    } else {
        LayoutMode::Wide
    }
}

/// Clamp a requested detail-column width to the viewport.
///
/// The detail column is the rightmost region, so its cap is
/// `viewport - sidebar - pane gap`: expanding it collapses the working list to
/// the grab gap rather than preserving a fixed list floor. The gap keeps the
/// invisible resize handle (centred on the pane's left border) fully clear of
/// the navigation rail. The lower bound wins if a (narrow) viewport leaves less
/// than `DETAIL_MIN_WIDTH`; the wide layout never reaches that case, but the
/// helper stays total and testable.
pub fn clamp_detail_width(desired: f32, viewport_width: f32) -> f32 {
    let viewport_cap = viewport_width - SIDEBAR_WIDTH - PANE_MIN_GAP;
    let max = viewport_cap.max(DETAIL_MIN_WIDTH);
    desired.clamp(DETAIL_MIN_WIDTH, max)
}

/// Clamp a requested Catalog source-preview width to the space available.
///
/// `available_width` is the Catalog's content region: the window viewport minus
/// the sidebar. Unlike the detail column there is no third region to its right,
/// so the preview may grow until only [`PANE_MIN_GAP`] is left for the
/// definition list — the same grab gap that keeps its resize handle clear.
pub fn clamp_catalog_preview_width(desired: f32, available_width: f32) -> f32 {
    let max = (available_width - PANE_MIN_GAP).max(CATALOG_PREVIEW_MIN_WIDTH);
    desired.clamp(CATALOG_PREVIEW_MIN_WIDTH, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_detail_column_breaks_below_the_narrow_threshold() {
        assert_eq!(
            container_query(px(NARROW_BREAKPOINT - 1.0)),
            LayoutMode::Narrow
        );
        assert_eq!(container_query(px(900.0)), LayoutMode::Narrow);
        assert_eq!(container_query(px(NARROW_BREAKPOINT)), LayoutMode::Wide);
        assert_eq!(container_query(px(1280.0)), LayoutMode::Wide);
    }

    #[test]
    fn detail_width_clamps_to_the_viewport_and_the_grab_gap() {
        use crate::theme::{DETAIL_WIDTH, RESIZE_HANDLE};

        // The default survives on a normal wide window.
        assert_eq!(clamp_detail_width(DETAIL_WIDTH, 1280.0), DETAIL_WIDTH);

        // The lower token bound wins for a too-small request.
        assert_eq!(clamp_detail_width(1.0, 1280.0), DETAIL_MIN_WIDTH);

        // There is no fixed ceiling: a wide viewport lets the detail column
        // grow until only the grab gap is left beside the sidebar.
        assert_eq!(
            clamp_detail_width(10_000.0, 1920.0),
            1920.0 - SIDEBAR_WIDTH - PANE_MIN_GAP
        );

        // A narrower viewport caps the detail the same way.
        assert_eq!(
            clamp_detail_width(10_000.0, 1280.0),
            1280.0 - SIDEBAR_WIDTH - PANE_MIN_GAP
        );

        // The gap never drops below the handle's width, so the divider stays
        // grabbable against the navigation rail.
        let expanded = clamp_detail_width(10_000.0, 1920.0);
        let gap = 1920.0 - SIDEBAR_WIDTH - expanded;
        assert_eq!(gap, PANE_MIN_GAP);
        assert!(gap >= RESIZE_HANDLE);

        // A tiny viewport still leaves the detail readable.
        assert_eq!(clamp_detail_width(10_000.0, 400.0), DETAIL_MIN_WIDTH);

        // At the breakpoint the whole content region is available to the pane.
        assert_eq!(
            clamp_detail_width(10_000.0, NARROW_BREAKPOINT),
            NARROW_BREAKPOINT - SIDEBAR_WIDTH - PANE_MIN_GAP
        );
    }

    #[test]
    fn catalog_preview_width_clamps_to_the_available_width_and_the_grab_gap() {
        use crate::theme::{CATALOG_PREVIEW_WIDTH, RESIZE_HANDLE};

        // The default survives on a normal wide window.
        assert_eq!(
            clamp_catalog_preview_width(CATALOG_PREVIEW_WIDTH, 1280.0 - SIDEBAR_WIDTH),
            CATALOG_PREVIEW_WIDTH
        );

        // The lower token bound wins for a too-small request.
        assert_eq!(
            clamp_catalog_preview_width(1.0, 1280.0 - SIDEBAR_WIDTH),
            CATALOG_PREVIEW_MIN_WIDTH
        );

        // There is no fixed ceiling: the preview grows until only the grab gap
        // is left for the definition list.
        assert_eq!(
            clamp_catalog_preview_width(10_000.0, 1920.0 - SIDEBAR_WIDTH),
            1920.0 - SIDEBAR_WIDTH - PANE_MIN_GAP
        );

        // A narrower content region caps the preview the same way.
        let available = 1280.0 - SIDEBAR_WIDTH;
        assert_eq!(
            clamp_catalog_preview_width(10_000.0, available),
            available - PANE_MIN_GAP
        );

        // The gap never drops below the handle's width.
        let expanded = clamp_catalog_preview_width(10_000.0, 1920.0 - SIDEBAR_WIDTH);
        let gap = 1920.0 - SIDEBAR_WIDTH - expanded;
        assert_eq!(gap, PANE_MIN_GAP);
        assert!(gap >= RESIZE_HANDLE);

        // A tiny viewport still leaves the preview readable.
        assert_eq!(
            clamp_catalog_preview_width(10_000.0, 400.0 - SIDEBAR_WIDTH),
            CATALOG_PREVIEW_MIN_WIDTH
        );
    }
}
