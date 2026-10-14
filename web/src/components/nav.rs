//! Responsive primary navigation: a desktop sidebar / mobile bottom tab bar.
//!
//! A single semantic `<nav>` renders one icon+label link per section; CSS turns
//! it into a fixed sidebar at ≥ 768px and a bottom tab bar below that. Each link
//! is highlighted whenever the current route belongs to its section — including
//! detail routes such as `/tasks/:id` — because `Link`'s own `active_class` only
//! matches an exact href.
//!
//! Icons are decorative: the label is always present for assistive technology,
//! and the glyph is hidden with `aria-hidden`.

use dioxus::prelude::*;

use crate::app::Route;
use crate::components::icon::{AppIcon, AppIconKind};
use crate::components::repo_switcher::RepoSwitcher;

/// One top-level destination in the primary navigation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NavSection {
    /// The Inbox (awaiting input and the event feed).
    Inbox,
    /// The task list and task detail.
    Tasks,
    /// The catalog list and definition pages.
    Catalog,
    /// The workflow board and workflow detail.
    Workflow,
    /// The cron schedule manager.
    Schedules,
    /// Notification channels, hooks and session info.
    Settings,
}

impl NavSection {
    /// Every section, in display order.
    pub const ALL: [NavSection; 6] = [
        NavSection::Inbox,
        NavSection::Tasks,
        NavSection::Catalog,
        NavSection::Workflow,
        NavSection::Schedules,
        NavSection::Settings,
    ];

    /// The visible, human-readable label.
    pub fn label(self) -> &'static str {
        match self {
            NavSection::Inbox => "Inbox",
            NavSection::Tasks => "Tasks",
            NavSection::Catalog => "Catalog",
            NavSection::Workflow => "Workflow",
            NavSection::Schedules => "Schedules",
            NavSection::Settings => "Settings",
        }
    }

    /// A decorative icon shown next to the label (hidden from assistive tech).
    pub fn icon(self) -> AppIconKind {
        match self {
            NavSection::Inbox => AppIconKind::Inbox,
            NavSection::Tasks => AppIconKind::Tasks,
            NavSection::Catalog => AppIconKind::Catalog,
            NavSection::Workflow => AppIconKind::Workflow,
            NavSection::Schedules => AppIconKind::Schedules,
            NavSection::Settings => AppIconKind::Settings,
        }
    }

    /// The index route this section links to.
    pub fn route(self) -> Route {
        match self {
            NavSection::Inbox => Route::Home {},
            NavSection::Tasks => Route::TasksList {},
            NavSection::Catalog => Route::Catalog {},
            NavSection::Workflow => Route::WorkflowList {},
            NavSection::Schedules => Route::Schedules {},
            NavSection::Settings => Route::Settings {},
        }
    }
}

/// The section a route belongs to, or `None` for routes outside the shell.
///
/// Alias and detail routes map to the section whose index page is highlighted
/// while they are open, so the sidebar/tab bar never loses its position.
pub fn section_for(route: &Route) -> Option<NavSection> {
    Some(match route {
        Route::Home {} | Route::Inbox {} | Route::InboxTask { .. } => NavSection::Inbox,
        Route::TasksList {} | Route::TaskDetail { .. } => NavSection::Tasks,
        Route::Catalog {} | Route::CatalogDetail { .. } => NavSection::Catalog,
        Route::WorkflowList {} | Route::WorkflowDetail { .. } => NavSection::Workflow,
        Route::Schedules {} => NavSection::Schedules,
        Route::Settings {} => NavSection::Settings,
        // Onboarding lives outside `AppLayout`, so it has no nav section.
        Route::Pair {} => return None,
    })
}

/// The primary navigation shared by the authenticated views.
#[component]
pub fn AppNav() -> Element {
    let route: Route = use_route();
    let current = section_for(&route);
    rsx! {
        nav { class: "app-nav", aria_label: "Primary",
            for section in NavSection::ALL {
                Link {
                    key: "{section.label()}",
                    to: section.route(),
                    class: if current == Some(section) {
                        "app-nav-item active"
                    } else {
                        "app-nav-item"
                    },
                    aria_current: if current == Some(section) { "page" } else { "false" },
                    AppIcon { kind: section.icon(), class: "app-nav-icon" }
                    span { class: "app-nav-label", "{section.label()}" }
                }
            }
            RepoSwitcher {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn section_for_maps_index_and_detail_routes() {
        assert_eq!(section_for(&Route::Home {}), Some(NavSection::Inbox));
        assert_eq!(section_for(&Route::Inbox {}), Some(NavSection::Inbox));
        assert_eq!(
            section_for(&Route::InboxTask {
                task_id: "abc".into()
            }),
            Some(NavSection::Inbox)
        );
        assert_eq!(section_for(&Route::TasksList {}), Some(NavSection::Tasks));
        assert_eq!(
            section_for(&Route::TaskDetail { id: "abc".into() }),
            Some(NavSection::Tasks)
        );
        assert_eq!(section_for(&Route::Catalog {}), Some(NavSection::Catalog));
        assert_eq!(
            section_for(&Route::CatalogDetail {
                name: vec!["foo".into()]
            }),
            Some(NavSection::Catalog)
        );
        assert_eq!(
            section_for(&Route::WorkflowList {}),
            Some(NavSection::Workflow)
        );
        assert_eq!(
            section_for(&Route::WorkflowDetail {
                root_id: Uuid::nil()
            }),
            Some(NavSection::Workflow)
        );
        assert_eq!(
            section_for(&Route::Schedules {}),
            Some(NavSection::Schedules)
        );
        assert_eq!(section_for(&Route::Settings {}), Some(NavSection::Settings));
    }

    #[test]
    fn pair_is_outside_the_shell() {
        assert_eq!(section_for(&Route::Pair {}), None);
    }

    #[test]
    fn every_section_has_a_distinct_label_icon_and_route() {
        let labels: Vec<_> = NavSection::ALL.iter().map(|s| s.label()).collect();
        let icons: Vec<_> = NavSection::ALL.iter().map(|s| s.icon()).collect();
        let mut unique_labels = labels.clone();
        unique_labels.sort_unstable();
        unique_labels.dedup();
        assert_eq!(unique_labels.len(), NavSection::ALL.len());
        // Each section has its own icon; no two share one.
        for (i, icon) in icons.iter().enumerate() {
            for other in &icons[i + 1..] {
                assert_ne!(icon, other, "duplicate nav icon");
            }
        }
        // Each section links to a route that maps back to itself, so the
        // highlighted section matches the route it points at.
        for section in NavSection::ALL {
            assert_eq!(section_for(&section.route()), Some(section));
            assert!(!section.label().is_empty());
        }
    }
}
