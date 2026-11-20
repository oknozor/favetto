//! Responsive layout decisions for the shell.
//!
//! The design plan asks for `container_query` (docs/design/gpui-client.md §1).
//! The pinned `gpui 0.2.2` build exposes no container-query primitive, so the
//! root container's width is read from the window viewport — which is identical
//! for the full-window shell — behind one named function. `gpui/README.md`
//! records the deviation.

use gpui::{px, Pixels};

use crate::theme::NARROW_BREAKPOINT;

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
}
