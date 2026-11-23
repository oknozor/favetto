//! The route vocabulary of the desktop client (docs/design/gpui-client.md §1).
//!
//! The order is not alphabetical on purpose: **Inbox → Tasks → Catalog →
//! Workflows → Schedules → Settings** follows the Serial Position Effect, so
//! the two surfaces users need most are first and configuration is last.
//!
//! [`Route`] is also the single source of truth for the sidebar icon, the
//! palette label, the `g`-prefix jump hint and the intentional empty state of a
//! surface that is still being filled in.

/// A top-level surface of the client.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Route {
    /// What needs me now — the Inbox surface.
    #[default]
    Inbox,
    /// All runs, filter and search — the Tasks surface.
    Tasks,
    /// Find and launch a definition — the Catalog surface.
    Catalog,
    /// One root's DAG — the Workflows surface.
    Workflows,
    /// Recurring triggers — the Schedules surface.
    Schedules,
    /// Connection, agents, theme — the Settings surface.
    Settings,
}

impl Route {
    /// Navigation order (docs §1, Serial Position Effect).
    pub const ALL: [Route; 6] = [
        Route::Inbox,
        Route::Tasks,
        Route::Catalog,
        Route::Workflows,
        Route::Schedules,
        Route::Settings,
    ];

    /// The sidebar label and the page heading.
    pub fn label(self) -> &'static str {
        match self {
            Route::Inbox => "Inbox",
            Route::Tasks => "Tasks",
            Route::Catalog => "Catalog",
            Route::Workflows => "Workflows",
            Route::Schedules => "Schedules",
            Route::Settings => "Settings",
        }
    }

    /// A compact glyph for the sidebar. Text labels always accompany it, so the
    /// icon is decoration rather than the only signal (docs §2 accessibility).
    pub fn icon(self) -> &'static str {
        match self {
            Route::Inbox => "✉",
            Route::Tasks => "☰",
            Route::Catalog => "▤",
            Route::Workflows => "⋔",
            Route::Schedules => "◷",
            Route::Settings => "⚙",
        }
    }

    /// The keyboard jump hint shown in the sidebar, palette and shortcut sheet.
    ///
    /// `g`-prefixed jumps mirror the TUI's muscle memory (Jakob's Law). Settings
    /// has the platform's conventional preferences chord as well as the palette
    /// entry, because `g s` belongs to Schedules.
    pub fn shortcut(self) -> &'static str {
        match self {
            Route::Inbox => "g i",
            Route::Tasks => "g t",
            Route::Catalog => "g c",
            Route::Workflows => "g w",
            Route::Schedules => "g s",
            Route::Settings => "Mod+,",
        }
    }

    /// The one-line promise of the surface, used under the page heading.
    pub fn subtitle(self) -> &'static str {
        match self {
            Route::Inbox => "What needs you now",
            Route::Tasks => "Every run, filtered and searchable",
            Route::Catalog => "Find and launch a task definition",
            Route::Workflows => "One root's DAG and progress",
            Route::Schedules => "Recurring triggers",
            Route::Settings => "Connection, agents and appearance",
        }
    }

    /// Whether this surface can show a run-detail page.
    ///
    /// Inbox and Tasks always have a run list behind the detail column.
    /// Workflows shows the same `DetailView` in place when a graph node is
    /// opened, but its column is only reserved while a detail is open.
    pub fn shows_detail(self) -> bool {
        matches!(self, Route::Inbox | Route::Tasks | Route::Workflows)
    }

    /// Whether the wide layout always reserves the detail column, even with
    /// nothing open. Inbox and Tasks paint their empty detail state; Workflows
    /// must give the graph the full width until a node is opened.
    pub fn always_reserves_detail_column(self) -> bool {
        matches!(self, Route::Inbox | Route::Tasks)
    }

    /// Position in [`Route::ALL`], used for stable element ids and tab order.
    pub fn index(self) -> usize {
        match self {
            Route::Inbox => 0,
            Route::Tasks => 1,
            Route::Catalog => 2,
            Route::Workflows => 3,
            Route::Schedules => 4,
            Route::Settings => 5,
        }
    }

    /// The `g`-prefixed jump for this route, when it has one. Settings is
    /// reached through its own chord or the palette because `g s` is Schedules.
    pub fn jump_shortcut(self) -> Option<&'static str> {
        match self {
            Route::Settings => None,
            other => Some(other.shortcut()),
        }
    }

    /// The intentional empty-state title for surfaces whose row data lands in
    /// later steps. These describe what the surface will hold — never "TODO".
    pub fn empty_title(self) -> &'static str {
        match self {
            Route::Inbox => "Nothing needs you right now",
            Route::Tasks => "No runs yet",
            Route::Catalog => "No definitions loaded",
            Route::Workflows => "No workflow selected",
            Route::Schedules => "No schedules yet",
            Route::Settings => "Connection settings",
        }
    }

    /// The one-sentence explanation shown under the empty-state title.
    #[allow(dead_code)]
    pub fn empty_body(self) -> &'static str {
        match self {
            Route::Inbox => {
                "Blocked and failed runs land here first. A run that asks for permission shows the prompt and the agent that raised it."
            }
            Route::Tasks => {
                "Every run the daemon starts appears here with its status, agent, attempt and elapsed time. Filter by status or search by task name."
            }
            Route::Catalog => {
                "Task definitions discovered by the daemon are grouped by repo. Each definition shows its declared variables before you launch it."
            }
            Route::Workflows => {
                "Choose a root run to see its DAG, completed steps and per-node progress. Retry one node without restarting the root."
            }
            Route::Schedules => {
                "Recurring triggers appear here with their cadence and next fire time. Every schedule points at a catalog task definition."
            }
            Route::Settings => {
                "Connection, agents and appearance. Settings edits connection details and tests the daemon without leaving the window."
            }
        }
    }

    /// The primary action a populated surface would offer (Hick's Law: one per
    /// view). Empty states show it as a calm hint, not a disabled button.
    pub fn primary_hint(self) -> &'static str {
        match self {
            Route::Inbox => "Open the blocked run",
            Route::Tasks => "Open a run",
            Route::Catalog => "Start with variables",
            Route::Workflows => "Inspect a root",
            Route::Schedules => "Create a schedule",
            Route::Settings => "Test connection",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigation_order_is_serial_position_effect() {
        assert_eq!(
            Route::ALL,
            [
                Route::Inbox,
                Route::Tasks,
                Route::Catalog,
                Route::Workflows,
                Route::Schedules,
                Route::Settings,
            ]
        );
        assert_eq!(Route::default(), Route::Inbox);
    }

    #[test]
    fn every_route_has_a_jump_and_a_distinct_icon() {
        let mut icons = Vec::new();
        for route in Route::ALL {
            assert!(
                !route.shortcut().is_empty(),
                "{} has no jump",
                route.label()
            );
            icons.push(route.icon());
        }
        icons.sort_unstable();
        icons.dedup();
        assert_eq!(icons.len(), Route::ALL.len());
    }

    #[test]
    fn schedules_own_g_s_and_settings_has_its_own_chord() {
        assert_eq!(Route::Schedules.shortcut(), "g s");
        assert_eq!(Route::Settings.shortcut(), "Mod+,");
        assert_ne!(Route::Schedules.shortcut(), Route::Settings.shortcut());
    }

    #[test]
    fn only_the_run_lists_reserve_the_detail_column() {
        assert!(Route::Inbox.shows_detail());
        assert!(Route::Tasks.shows_detail());
        assert!(Route::Workflows.shows_detail());
        assert!(Route::Inbox.always_reserves_detail_column());
        assert!(Route::Tasks.always_reserves_detail_column());
        assert!(!Route::Workflows.always_reserves_detail_column());
        for route in [Route::Catalog, Route::Schedules, Route::Settings] {
            assert!(
                !route.shows_detail(),
                "{} has no run detail to show",
                route.label()
            );
            assert!(!route.always_reserves_detail_column());
        }
    }
}
