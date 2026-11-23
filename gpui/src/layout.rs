//! Responsive layout decisions for the shell.
//!
//! The design plan asks for `container_query` (docs/design/gpui-client.md §1).
//! The pinned `gpui 0.2.2` build exposes no container-query primitive, so the
//! root container's width is read from the window viewport — which is identical
//! for the full-window shell — behind one named function. `gpui/README.md`
//! records the deviation.

use gpui::{px, Pixels};

use crate::theme::{
    CATALOG_PREVIEW_MAX_WIDTH, CATALOG_PREVIEW_MIN_WIDTH, DETAIL_MAX_WIDTH, DETAIL_MIN_WIDTH,
    NARROW_BREAKPOINT, RESIZE_HANDLE, SIDEBAR_WIDTH, WORKING_LIST_MIN_WIDTH,
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

/// Clamp a requested detail-column width to the token bounds and to the space
/// left by the sidebar and a usable working list.
///
/// The detail column is the rightmost region, so the viewport cap is
/// `viewport - sidebar - working-list floor - resize handle`. The lower bound
/// wins if a (narrow) viewport leaves less than `DETAIL_MIN_WIDTH`; the wide
/// layout never reaches that case, but the helper stays total and testable.
pub fn clamp_detail_width(desired: f32, viewport_width: f32) -> f32 {
    let viewport_cap = viewport_width - SIDEBAR_WIDTH - WORKING_LIST_MIN_WIDTH - RESIZE_HANDLE;
    let max = DETAIL_MAX_WIDTH.min(viewport_cap).max(DETAIL_MIN_WIDTH);
    desired.clamp(DETAIL_MIN_WIDTH, max)
}

/// Clamp a requested Catalog source-preview width to the token bounds and to
/// the space left for the definition list.
///
/// `available_width` is the Catalog's content region: the window viewport minus
/// the sidebar and the resize handle. Unlike the detail column there is no
/// third region to its right, so the list floor is the only competing region.
pub fn clamp_catalog_preview_width(desired: f32, available_width: f32) -> f32 {
    let list_cap = available_width - WORKING_LIST_MIN_WIDTH;
    let max = CATALOG_PREVIEW_MAX_WIDTH
        .min(list_cap)
        .max(CATALOG_PREVIEW_MIN_WIDTH);
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
    fn detail_width_clamps_to_the_tokens_and_the_viewport() {
        use crate::theme::DETAIL_WIDTH;

        // The default survives on a normal wide window.
        assert_eq!(clamp_detail_width(DETAIL_WIDTH, 1280.0), DETAIL_WIDTH);

        // The lower token bound wins for a too-small request.
        assert_eq!(clamp_detail_width(1.0, 1280.0), DETAIL_MIN_WIDTH);

        // The upper token bound wins when the viewport has room for it.
        assert_eq!(clamp_detail_width(10_000.0, 1920.0), DETAIL_MAX_WIDTH);

        // A narrower viewport caps the detail before the token maximum.
        assert_eq!(
            clamp_detail_width(10_000.0, 1280.0),
            1280.0 - SIDEBAR_WIDTH - WORKING_LIST_MIN_WIDTH - RESIZE_HANDLE
        );

        // A tiny viewport still leaves the detail readable.
        assert_eq!(clamp_detail_width(10_000.0, 800.0), DETAIL_MIN_WIDTH);

        // At the breakpoint the cap sits between the two token bounds.
        let at_breakpoint = clamp_detail_width(DETAIL_MAX_WIDTH, NARROW_BREAKPOINT);
        assert!(at_breakpoint > DETAIL_MIN_WIDTH);
        assert!(at_breakpoint < DETAIL_MAX_WIDTH);
    }

    #[test]
    fn catalog_preview_width_clamps_to_the_tokens_and_the_list_floor() {
        use crate::theme::CATALOG_PREVIEW_WIDTH;

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

        // The upper token bound wins when the viewport has room for it.
        assert_eq!(
            clamp_catalog_preview_width(10_000.0, 1920.0 - SIDEBAR_WIDTH),
            CATALOG_PREVIEW_MAX_WIDTH
        );

        // The definition-list floor caps a moderate viewport before the token
        // maximum; the Catalog has no third region to reserve.
        let available = 1120.0 - SIDEBAR_WIDTH;
        assert_eq!(
            clamp_catalog_preview_width(10_000.0, available),
            available - WORKING_LIST_MIN_WIDTH
        );

        // A tiny viewport still leaves the preview readable.
        assert_eq!(
            clamp_catalog_preview_width(10_000.0, 800.0 - SIDEBAR_WIDTH - RESIZE_HANDLE),
            CATALOG_PREVIEW_MIN_WIDTH
        );
    }
}
