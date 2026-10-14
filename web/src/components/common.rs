//! Small shared primitives: a spinner, an empty-state message, and event-kind
//! badges.

use dioxus::prelude::*;
use favetto_wire::model::EventKind;

use crate::components::icon::{AppIcon, AppIconKind};

/// A spinning loading indicator (`role="status"`, with a hidden text node so
/// screen readers announce it).
#[component]
pub fn Spinner() -> Element {
    rsx! {
        div { class: "spinner", role: "status", aria_label: "Loading…",
            span { class: "visually-hidden", "Loading…" }
        }
    }
}

/// A centered message for an empty section.
#[component]
pub fn EmptyState(message: String) -> Element {
    rsx! {
        p { class: "empty-state", role: "status", "{message}" }
    }
}

/// Humanize an event kind's `snake_case` label: `task_awaiting_input` becomes
/// `task awaiting input`.
pub fn event_kind_label(kind: &EventKind) -> String {
    kind.as_str().replace('_', " ")
}

/// The decorative icon for an event kind.
///
/// The match is exhaustive on purpose: a new [`EventKind`] must pick an icon
/// rather than silently fall through.
pub fn event_icon(kind: &EventKind) -> AppIconKind {
    match kind {
        EventKind::TaskCreated => AppIconKind::EventCreated,
        EventKind::TaskUpdated => AppIconKind::EventUpdated,
        EventKind::TaskCompleted => AppIconKind::EventCompleted,
        EventKind::TaskFailed => AppIconKind::EventFailed,
        EventKind::TaskCancelled => AppIconKind::EventCancelled,
        EventKind::TaskIdle => AppIconKind::EventIdle,
        EventKind::TaskStarted => AppIconKind::EventStarted,
        EventKind::TaskAwaitingInput => AppIconKind::EventAwaitingInput,
        EventKind::TaskFinished => AppIconKind::EventFinished,
        EventKind::CronTick => AppIconKind::EventClock,
        EventKind::IssueCreated => AppIconKind::EventCreated,
        EventKind::IssueUpdated => AppIconKind::EventUpdated,
        EventKind::IssueClosed => AppIconKind::EventArchive,
        EventKind::IssueReopened => AppIconKind::EventReopened,
        EventKind::IssueLabeled => AppIconKind::EventLabeled,
        EventKind::IssueAssigned => AppIconKind::EventAssigned,
        EventKind::IssueCommentCreated => AppIconKind::EventComment,
        EventKind::PrCreated => AppIconKind::EventPullRequest,
        EventKind::PrMerged => AppIconKind::EventMerge,
        EventKind::PrClosed => AppIconKind::EventPullRequest,
        EventKind::PrReopened => AppIconKind::EventReopened,
        EventKind::PrSynchronized => AppIconKind::EventSync,
        EventKind::PrReadyForReview => AppIconKind::EventReview,
        EventKind::PrReviewRequested => AppIconKind::EventAssigned,
        EventKind::PrUpdated => AppIconKind::EventUpdated,
        EventKind::PrReviewSubmitted => AppIconKind::EventComment,
        EventKind::PushReceived => AppIconKind::EventPush,
        EventKind::TicketCreated => AppIconKind::EventTicket,
        EventKind::TicketUpdated => AppIconKind::EventTicket,
        EventKind::ActionRunCompleted => AppIconKind::EventChecks,
        EventKind::CheckSuiteCompleted => AppIconKind::EventChecks,
        EventKind::CheckRunCompleted => AppIconKind::EventChecks,
        EventKind::EmailReceived => AppIconKind::EventEmail,
        EventKind::Unknown => AppIconKind::EventUnknown,
    }
}

/// A small badge naming an event kind.
#[component]
pub fn EventBadge(kind: EventKind) -> Element {
    let class = format!("event-badge event-{}", kind.as_str());
    let icon = event_icon(&kind);
    let label = event_kind_label(&kind);
    rsx! {
        span { class,
            AppIcon { kind: icon, class: "event-badge-icon" }
            "{label}"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_kind_label_humanizes_snake_case() {
        assert_eq!(
            event_kind_label(&EventKind::TaskAwaitingInput),
            "task awaiting input"
        );
        assert_eq!(event_kind_label(&EventKind::PrMerged), "pr merged");
        assert_eq!(event_kind_label(&EventKind::Unknown), "unknown");
    }

    #[test]
    fn event_badge_and_empty_state_mount_host_side() {
        let mut dom = VirtualDom::new(|| {
            rsx! {
                EventBadge { kind: EventKind::TaskFinished }
                EmptyState { message: "All caught up.".to_string() }
                Spinner {}
            }
        });
        dom.rebuild_in_place();
    }

    /// Every wire event kind maps to an icon, and the mapping is exercised for
    /// the whole table so a new kind cannot be forgotten.
    #[test]
    fn every_event_kind_has_an_icon() {
        for kind in EventKind::ALL {
            let _ = event_icon(kind);
        }
    }
}
