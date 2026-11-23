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

/// The widest the shortcut sheet grows on a large window.
pub const SHORTCUT_SHEET_MAX_WIDTH: f32 = 640.0;

/// Breathing room between the shortcut sheet and the window edge.
pub const SHORTCUT_SHEET_GUTTER: f32 = 24.0;

/// Below this sheet width the shortcut groups stack into one column.
///
/// This is the panel width, not the viewport: a window narrower than
/// [`SHORTCUT_SHEET_MAX_WIDTH`] plus two gutters already produces a narrower
/// sheet even before the window drops below the breakpoint itself.
pub const SHORTCUT_SHEET_SINGLE_COLUMN_WIDTH: f32 = 560.0;

/// The size and column count of the shortcut sheet for a viewport.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShortcutSheetLayout {
    /// The panel width, capped by the viewport minus a gutter on each side.
    pub width: f32,
    /// The panel height ceiling, leaving a gutter above and below.
    pub max_height: f32,
    /// `2` side by side, or `1` when the panel is too narrow.
    pub columns: usize,
}

/// Bound the shortcut sheet to the viewport.
///
/// The sheet never grows wider than [`SHORTCUT_SHEET_MAX_WIDTH`] or taller than
/// the space left after a [`SHORTCUT_SHEET_GUTTER`] on every side. The panel
/// width also decides the column count so a narrow window reflows instead of
/// crushing labels against their keycaps.
pub fn shortcut_sheet_layout(viewport_width: f32, viewport_height: f32) -> ShortcutSheetLayout {
    let gutter = 2.0 * SHORTCUT_SHEET_GUTTER;
    let width = (viewport_width - gutter).clamp(0.0, SHORTCUT_SHEET_MAX_WIDTH);
    let max_height = (viewport_height - gutter).max(0.0);
    let columns = if width < SHORTCUT_SHEET_SINGLE_COLUMN_WIDTH {
        1
    } else {
        2
    };

    ShortcutSheetLayout {
        width,
        max_height,
        columns,
    }
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

    #[test]
    fn the_shortcut_sheet_stays_inside_a_large_and_a_minimum_window() {
        // The documented default remains the previous 640 px two-column panel,
        // and an 800 px-tall window has room to spare below the 24 px gutters.
        let large = shortcut_sheet_layout(1280.0, 800.0);
        assert_eq!(large.width, SHORTCUT_SHEET_MAX_WIDTH);
        assert_eq!(large.max_height, 800.0 - 2.0 * SHORTCUT_SHEET_GUTTER);
        assert_eq!(large.columns, 2);

        // At the supported 900x600 minimum the sheet still fits with a gutter
        // on each edge instead of being centered past both of them.
        let minimum = shortcut_sheet_layout(900.0, 600.0);
        assert_eq!(minimum.width, SHORTCUT_SHEET_MAX_WIDTH);
        assert_eq!(minimum.max_height, 600.0 - 2.0 * SHORTCUT_SHEET_GUTTER);
        assert!(minimum.width < 900.0 - SHORTCUT_SHEET_GUTTER);
        assert!(minimum.max_height < 600.0);

        // A capped panel still leaves at least the gutter free on both sides,
        // and the height ceiling leaves exactly one gutter above and below.
        assert!(large.width + 2.0 * SHORTCUT_SHEET_GUTTER <= 1280.0);
        assert_eq!(large.max_height + 2.0 * SHORTCUT_SHEET_GUTTER, 800.0);
    }

    #[test]
    fn the_shortcut_sheet_shrinks_and_reflows_below_the_column_breakpoint() {
        // A forced/tiled window narrower than the sheet: the panel follows the
        // available width and the paired groups become one column.
        let narrow = shortcut_sheet_layout(500.0, 600.0);
        assert_eq!(narrow.width, 500.0 - 2.0 * SHORTCUT_SHEET_GUTTER);
        assert_eq!(narrow.columns, 1);

        // The transition is measured on the panel width: 559 px is still one
        // column, 560 px has room for two.
        let one_column = shortcut_sheet_layout(
            SHORTCUT_SHEET_SINGLE_COLUMN_WIDTH + 2.0 * SHORTCUT_SHEET_GUTTER - 1.0,
            600.0,
        );
        assert_eq!(one_column.width, SHORTCUT_SHEET_SINGLE_COLUMN_WIDTH - 1.0);
        assert_eq!(one_column.columns, 1);

        let two_columns = shortcut_sheet_layout(
            SHORTCUT_SHEET_SINGLE_COLUMN_WIDTH + 2.0 * SHORTCUT_SHEET_GUTTER,
            600.0,
        );
        assert_eq!(two_columns.width, SHORTCUT_SHEET_SINGLE_COLUMN_WIDTH);
        assert_eq!(two_columns.columns, 2);

        // A viewport smaller than the gutter itself never yields a negative
        // size, so layout maths cannot overflow the window in either axis.
        let tiny = shortcut_sheet_layout(10.0, 10.0);
        assert_eq!(tiny.width, 0.0);
        assert_eq!(tiny.max_height, 0.0);
        assert_eq!(tiny.columns, 1);
    }
}
