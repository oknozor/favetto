//! The shared Phosphor icon wrapper.
//!
//! Every glyph in the client — navigation, task statuses, event badges, actions
//! and the dismiss/back affordances — is a Phosphor icon rendered through
//! [`AppIcon`]. Centralising the mapping in one [`AppIconKind`] enum keeps the
//! visual language consistent, lets icons inherit the surrounding text colour
//! (`currentColor`), and gives the host tests a single place to assert coverage.
//!
//! Icons are decorative by default: they render `aria-hidden="true"`, so an
//! icon-only control must still carry its own accessible name.

use dioxus::prelude::*;
use dioxus_icons_phosphor::{regular, Icon};

/// Every icon the client can render.
///
/// The enum is the union of the navigation, status, event and action glyphs;
/// the per-area helpers (`NavSection::icon`, `status_icon`, `event_icon`) map
/// their vocabulary onto it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AppIconKind {
    // Navigation.
    Inbox,
    Tasks,
    Catalog,
    Workflow,
    Schedules,
    Settings,
    // Repository switcher.
    Repo,
    ActiveRepo,
    // Actions.
    Back,
    Close,
    Refresh,
    Retry,
    Start,
    ArrowDown,
    // Feed tool cards.
    ToolTerminal,
    ToolEdit,
    ToolRead,
    ToolSearch,
    ToolFetch,
    ToolList,
    ToolTodo,
    ToolGeneric,
    // Task statuses.
    StatusPending,
    StatusRunning,
    StatusAwaitingInput,
    StatusSucceeded,
    StatusFailed,
    StatusCancelled,
    // Toasts / notices.
    ToastSuccess,
    ToastError,
    ToastInfo,
    // Event kinds.
    EventCreated,
    EventUpdated,
    EventCompleted,
    EventFailed,
    EventCancelled,
    EventIdle,
    EventStarted,
    EventAwaitingInput,
    EventFinished,
    EventClock,
    EventArchive,
    EventReopened,
    EventLabeled,
    EventAssigned,
    EventComment,
    EventPullRequest,
    EventMerge,
    EventSync,
    EventReview,
    EventPush,
    EventTicket,
    EventChecks,
    EventEmail,
    EventUnknown,
}

macro_rules! render_icon {
    ($class:expr, $shape:path) => {
        rsx! {
            Icon {
                icon: $shape,
                width: "1em",
                height: "1em",
                class: $class,
                aria_hidden: "true",
            }
        }
    };
}

impl AppIconKind {
    /// Render the concrete Phosphor icon for this kind.
    fn render(self, class: String) -> Element {
        match self {
            AppIconKind::Inbox => render_icon!(class, regular::Tray),
            AppIconKind::Tasks => render_icon!(class, regular::ListChecks),
            AppIconKind::Catalog => render_icon!(class, regular::Books),
            AppIconKind::Workflow => render_icon!(class, regular::TreeStructure),
            AppIconKind::Schedules => render_icon!(class, regular::Clock),
            AppIconKind::Settings => render_icon!(class, regular::Gear),
            AppIconKind::Repo => render_icon!(class, regular::Package),
            AppIconKind::ActiveRepo => render_icon!(class, regular::Circle),
            AppIconKind::Back => render_icon!(class, regular::ArrowLeft),
            AppIconKind::Close => render_icon!(class, regular::X),
            AppIconKind::Refresh => render_icon!(class, regular::ArrowsClockwise),
            AppIconKind::Retry => render_icon!(class, regular::ArrowClockwise),
            AppIconKind::Start => render_icon!(class, regular::Play),
            AppIconKind::ArrowDown => render_icon!(class, regular::ArrowDown),
            AppIconKind::ToolTerminal => render_icon!(class, regular::Terminal),
            AppIconKind::ToolEdit => render_icon!(class, regular::PencilSimple),
            AppIconKind::ToolRead => render_icon!(class, regular::FileText),
            AppIconKind::ToolSearch => render_icon!(class, regular::MagnifyingGlass),
            AppIconKind::ToolFetch => render_icon!(class, regular::ArrowsDownUp),
            AppIconKind::ToolList => render_icon!(class, regular::List),
            AppIconKind::ToolTodo => render_icon!(class, regular::CheckSquare),
            AppIconKind::ToolGeneric => render_icon!(class, regular::Gear),
            AppIconKind::StatusPending => render_icon!(class, regular::Circle),
            AppIconKind::StatusRunning => render_icon!(class, regular::CircleNotch),
            AppIconKind::StatusAwaitingInput => render_icon!(class, regular::HourglassMedium),
            AppIconKind::StatusSucceeded => render_icon!(class, regular::CheckCircle),
            AppIconKind::StatusFailed => render_icon!(class, regular::XCircle),
            AppIconKind::StatusCancelled => render_icon!(class, regular::Prohibit),
            AppIconKind::ToastSuccess => render_icon!(class, regular::CheckCircle),
            AppIconKind::ToastError => render_icon!(class, regular::WarningCircle),
            AppIconKind::ToastInfo => render_icon!(class, regular::Info),
            AppIconKind::EventCreated => render_icon!(class, regular::PlusCircle),
            AppIconKind::EventUpdated => render_icon!(class, regular::PencilSimple),
            AppIconKind::EventCompleted => render_icon!(class, regular::CheckCircle),
            AppIconKind::EventFailed => render_icon!(class, regular::XCircle),
            AppIconKind::EventCancelled => render_icon!(class, regular::Prohibit),
            AppIconKind::EventIdle => render_icon!(class, regular::Circle),
            AppIconKind::EventStarted => render_icon!(class, regular::PlayCircle),
            AppIconKind::EventAwaitingInput => render_icon!(class, regular::HourglassMedium),
            AppIconKind::EventFinished => render_icon!(class, regular::Checks),
            AppIconKind::EventClock => render_icon!(class, regular::Clock),
            AppIconKind::EventArchive => render_icon!(class, regular::Archive),
            AppIconKind::EventReopened => render_icon!(class, regular::ArrowClockwise),
            AppIconKind::EventLabeled => render_icon!(class, regular::Tag),
            AppIconKind::EventAssigned => render_icon!(class, regular::UserPlus),
            AppIconKind::EventComment => render_icon!(class, regular::ChatCircle),
            AppIconKind::EventPullRequest => render_icon!(class, regular::GitPullRequest),
            AppIconKind::EventMerge => render_icon!(class, regular::GitMerge),
            AppIconKind::EventSync => render_icon!(class, regular::Repeat),
            AppIconKind::EventReview => render_icon!(class, regular::SealCheck),
            AppIconKind::EventPush => render_icon!(class, regular::FileArrowUp),
            AppIconKind::EventTicket => render_icon!(class, regular::Ticket),
            AppIconKind::EventChecks => render_icon!(class, regular::Checks),
            AppIconKind::EventEmail => render_icon!(class, regular::EnvelopeSimple),
            AppIconKind::EventUnknown => render_icon!(class, regular::Info),
        }
    }
}

/// A decorative Phosphor icon.
///
/// `class` carries the sizing/colour utilities (the SVG inherits
/// `currentColor`), e.g. `class: "size-5 text-primary"`.
#[component]
pub fn AppIcon(kind: AppIconKind, #[props(default = String::new())] class: String) -> Element {
    kind.render(class)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every kind renders without panicking on the host build. The test mounts
    /// each icon so a bad `rsx!` arm is caught before a `dx build`.
    #[test]
    fn every_icon_renders_host_side() {
        const KINDS: &[AppIconKind] = &[
            AppIconKind::Inbox,
            AppIconKind::Tasks,
            AppIconKind::Catalog,
            AppIconKind::Workflow,
            AppIconKind::Schedules,
            AppIconKind::Settings,
            AppIconKind::Repo,
            AppIconKind::ActiveRepo,
            AppIconKind::Back,
            AppIconKind::Close,
            AppIconKind::Refresh,
            AppIconKind::Retry,
            AppIconKind::Start,
            AppIconKind::ArrowDown,
            AppIconKind::ToolTerminal,
            AppIconKind::ToolEdit,
            AppIconKind::ToolRead,
            AppIconKind::ToolSearch,
            AppIconKind::ToolFetch,
            AppIconKind::ToolList,
            AppIconKind::ToolTodo,
            AppIconKind::ToolGeneric,
            AppIconKind::StatusPending,
            AppIconKind::StatusRunning,
            AppIconKind::StatusAwaitingInput,
            AppIconKind::StatusSucceeded,
            AppIconKind::StatusFailed,
            AppIconKind::StatusCancelled,
            AppIconKind::ToastSuccess,
            AppIconKind::ToastError,
            AppIconKind::ToastInfo,
            AppIconKind::EventCreated,
            AppIconKind::EventUpdated,
            AppIconKind::EventCompleted,
            AppIconKind::EventFailed,
            AppIconKind::EventCancelled,
            AppIconKind::EventIdle,
            AppIconKind::EventStarted,
            AppIconKind::EventAwaitingInput,
            AppIconKind::EventFinished,
            AppIconKind::EventClock,
            AppIconKind::EventArchive,
            AppIconKind::EventReopened,
            AppIconKind::EventLabeled,
            AppIconKind::EventAssigned,
            AppIconKind::EventComment,
            AppIconKind::EventPullRequest,
            AppIconKind::EventMerge,
            AppIconKind::EventSync,
            AppIconKind::EventReview,
            AppIconKind::EventPush,
            AppIconKind::EventTicket,
            AppIconKind::EventChecks,
            AppIconKind::EventEmail,
            AppIconKind::EventUnknown,
        ];
        let mut dom = VirtualDom::new(|| {
            rsx! {
                for kind in KINDS {
                    AppIcon { key: "{kind:?}", kind: *kind, class: "size-4" }
                }
            }
        });
        dom.rebuild_in_place();
    }
}
