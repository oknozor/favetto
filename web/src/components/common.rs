//! Small shared primitives: a spinner, an empty-state message, and event-kind
//! badges.

use dioxus::prelude::*;
use favetto_wire::model::EventKind;

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

/// A small badge naming an event kind.
#[component]
pub fn EventBadge(kind: EventKind) -> Element {
    let class = format!("event-badge event-{}", kind.as_str());
    rsx! {
        span { class, "{event_kind_label(&kind)}" }
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
}
