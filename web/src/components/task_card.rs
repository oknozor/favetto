//! A compact task summary card shared by the Inbox sections.
//!
//! The card is deliberately dumb: it takes primitives (not a [`Task`]) because
//! `Task` does not derive `PartialEq` and therefore cannot be a Dioxus prop. The
//! same reason is why `workflow_node.rs` normalizes its rows into a `NodeCard`.

use dioxus::prelude::*;
use favetto_wire::model::TaskStatus;
use uuid::Uuid;

use crate::app::Route;
use crate::views::tasks::status_label;

/// A glyph for a task status, used as a quick visual cue on cards.
pub fn status_icon(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Pending => "•",
        TaskStatus::Running => "▶",
        TaskStatus::AwaitingInput => "⏳",
        TaskStatus::Succeeded => "✔",
        TaskStatus::Failed => "✖",
        TaskStatus::Cancelled => "⊘",
    }
}

/// A reusable task card: links to the task detail and shows the status icon,
/// name, status badge, age, and an optional failure/summary line.
#[component]
pub fn TaskCard(
    id: Uuid,
    name: String,
    status: TaskStatus,
    age: String,
    #[props(default = false)] awaiting: bool,
    #[props(default)] detail: Option<String>,
) -> Element {
    let badge = format!("status-badge status-{}", status.as_str());
    let icon = status_icon(status);
    rsx! {
        Link {
            to: Route::TaskDetail { id: id.to_string() },
            class: "task-card",
            span { class: "task-card-icon", aria_hidden: "true", "{icon}" }
            div { class: "task-card-main",
                div { class: "task-card-head",
                    span { class: "task-card-name", "{name}" }
                    span { class: badge, "{status_label(status)}" }
                    if awaiting {
                        span { class: "task-card-awaiting", "needs input" }
                    }
                }
                div { class: "task-card-meta",
                    span { class: "task-card-age", "{age}" }
                    if let Some(detail) = detail {
                        span { class: "task-card-detail", "{detail}" }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_icon_covers_every_status() {
        for status in [
            TaskStatus::Pending,
            TaskStatus::Running,
            TaskStatus::AwaitingInput,
            TaskStatus::Succeeded,
            TaskStatus::Failed,
            TaskStatus::Cancelled,
        ] {
            assert!(!status_icon(status).is_empty(), "{status:?}");
        }
    }

    #[test]
    fn task_card_mounts_host_side() {
        let mut dom = VirtualDom::new(|| {
            let id = Uuid::new_v4();
            rsx! {
                TaskCard {
                    id,
                    name: "implement".to_string(),
                    status: TaskStatus::AwaitingInput,
                    age: "5m".to_string(),
                    awaiting: true,
                    detail: Some("boom".to_string()),
                }
            }
        });
        dom.rebuild_in_place();
    }
}
