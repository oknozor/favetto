//! A single workflow task card and the status rendering helpers it shares with
//! the workflow views.
//!
//! Both the `/workflow` board (tasks from `tasks.list`) and the
//! `/workflow/:root_id` inspect tree (`workflow.inspect`) render the same card,
//! so the two views normalize their rows into one [`NodeCard`] value.

use dioxus::prelude::*;
use favetto_wire::model::{Task, TaskStatus};
use favetto_wire::workflow::WorkflowTaskView;
use uuid::Uuid;

use crate::app::Route;
use crate::state::task_display_name;

/// A single renderable workflow task.
#[derive(Clone, PartialEq)]
pub struct NodeCard {
    pub id: Uuid,
    /// Root to inspect / cancel (`Task::root_or_self()`).
    pub root_id: Uuid,
    pub name: String,
    pub status: TaskStatus,
    pub attempt: u32,
    pub age: Option<String>,
    pub summary: Option<String>,
    /// Indentation depth for the inspect tree (0 = root).
    pub depth: u32,
}

impl NodeCard {
    /// Build a card from a live [`Task`]. The card always links to (and cancels)
    /// the task's root, so a spawned child is still actionable from the board.
    pub fn from_task(task: &Task, now_epoch: i64) -> Self {
        Self {
            id: task.id,
            root_id: task.root_or_self(),
            name: task_display_name(&task.name).to_string(),
            status: task.status,
            attempt: task.attempt,
            age: Some(crate::views::workflow::task_age(task, now_epoch)),
            summary: task.error.clone(),
            depth: 0,
        }
    }

    /// Build a card from one `workflow.inspect` row. `age`/`depth` come from the
    /// caller because [`WorkflowTaskView`] carries neither timestamps nor
    /// `parent_id`.
    pub fn from_view(
        view: &WorkflowTaskView,
        root_id: Uuid,
        age: Option<String>,
        depth: u32,
    ) -> Self {
        Self {
            id: view.id,
            root_id,
            name: task_display_name(&view.name).to_string(),
            status: view.status,
            attempt: view.attempt,
            age,
            summary: view.summary.clone(),
            depth,
        }
    }
}

/// Stable CSS/serialize word for a task status (class-safe, matches `as_str`).
pub fn status_class(status: TaskStatus) -> &'static str {
    status.as_str()
}

/// Human label for a task status. `Succeeded` is shown as "Finished" because
/// that is the user-facing wording the issue uses.
pub fn status_label(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Pending => "Pending",
        TaskStatus::Running => "Running",
        TaskStatus::AwaitingInput => "Awaiting input",
        TaskStatus::Succeeded => "Finished",
        TaskStatus::Failed => "Failed",
        TaskStatus::Cancelled => "Cancelled",
    }
}

/// True while the task can still be cancelled.
pub fn is_cancellable(status: TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Pending | TaskStatus::Running | TaskStatus::AwaitingInput
    )
}

/// True once a terminal task can be retried.
pub fn is_retryable(status: TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Succeeded | TaskStatus::Failed | TaskStatus::Cancelled
    )
}

/// Properties for [`WorkflowNode`].
#[derive(Props, Clone, PartialEq)]
pub struct WorkflowNodeProps {
    pub card: NodeCard,
    /// True while a cancel/retry request for this view is in flight.
    pub busy: bool,
    /// Called with the workflow root id (root-scoped cancel).
    pub on_cancel: EventHandler<Uuid>,
    /// Called with the task id (single-task retry).
    pub on_retry: EventHandler<Uuid>,
}

/// One workflow task: a link to its root, a status badge, and its actions.
#[component]
pub fn WorkflowNode(props: WorkflowNodeProps) -> Element {
    let WorkflowNodeProps {
        card,
        busy,
        on_cancel,
        on_retry,
    } = props;

    let status = card.status;
    let name = card.name.clone();
    let age = card.age.clone();
    let summary = card.summary.clone();
    let attempt = card.attempt;
    let depth = card.depth;
    let root_id = card.root_id;
    let task_id = card.id;
    let to = Route::WorkflowDetail { root_id };

    let class = status_class(status);
    rsx! {
        div { class: "workflow-node status-{class}", style: "margin-left: {depth * 16}px",
            div { class: "workflow-node-head",
                Link { to, class: "workflow-node-name", "{name}" }
                span { class: "status-badge status-{class}", "{status_label(status)}" }
                if attempt > 1 {
                    span { class: "workflow-node-attempt", "attempt {attempt}" }
                }
                if let Some(age) = age {
                    span { class: "workflow-node-age", "{age}" }
                }
            }
            if let Some(summary) = summary {
                p { class: "workflow-node-summary", "{summary}" }
            }
            div { class: "workflow-node-actions",
                button {
                    class: "btn",
                    disabled: busy || !is_cancellable(status),
                    onclick: move |_| on_cancel.call(root_id),
                    "Cancel workflow"
                }
                button {
                    class: "btn",
                    disabled: busy || !is_retryable(status),
                    onclick: move |_| on_retry.call(task_id),
                    "Retry"
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: Uuid, name: &str, status: TaskStatus, root_id: Option<Uuid>) -> Task {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "name": name,
            "status": status.as_str(),
            "attempt": 0,
            "input": {},
            "output": null,
            "dedupe_key": null,
            "created_at": "2026-01-01T00:00:00Z",
            "started_at": null,
            "finished_at": null,
            "error": null,
            "failure": null,
            "session_id": null,
            "session_title": null,
            "parent_id": null,
            "root_id": root_id,
            "interactive": false,
        }))
        .unwrap()
    }

    #[test]
    fn status_class_and_label_cover_every_variant() {
        for (status, class, label) in [
            (TaskStatus::Pending, "pending", "Pending"),
            (TaskStatus::Running, "running", "Running"),
            (
                TaskStatus::AwaitingInput,
                "awaiting_input",
                "Awaiting input",
            ),
            (TaskStatus::Succeeded, "succeeded", "Finished"),
            (TaskStatus::Failed, "failed", "Failed"),
            (TaskStatus::Cancelled, "cancelled", "Cancelled"),
        ] {
            assert_eq!(status_class(status), class);
            assert_eq!(status_label(status), label);
        }
    }

    #[test]
    fn cancellable_and_retryable_partition_the_six_statuses() {
        let all = [
            TaskStatus::Pending,
            TaskStatus::Running,
            TaskStatus::AwaitingInput,
            TaskStatus::Succeeded,
            TaskStatus::Failed,
            TaskStatus::Cancelled,
        ];
        for status in all {
            assert_ne!(
                is_cancellable(status),
                is_retryable(status),
                "{status:?} must be exactly one of cancellable/retryable"
            );
        }
        assert!(is_cancellable(TaskStatus::AwaitingInput));
        assert!(is_retryable(TaskStatus::Failed));
    }

    #[test]
    fn from_task_uses_root_or_self() {
        let root = Uuid::new_v4();
        let child = task(Uuid::new_v4(), "child", TaskStatus::Running, Some(root));
        let card = NodeCard::from_task(&child, child.created_at.timestamp() + 5);
        assert_eq!(card.root_id, root);
        assert_eq!(card.id, child.id);
        assert_eq!(card.depth, 0);
        assert_eq!(card.age.as_deref(), Some("5s"));

        // A task with no stored root is its own root.
        let standalone = task(Uuid::new_v4(), "solo", TaskStatus::Pending, None);
        let card = NodeCard::from_task(&standalone, standalone.created_at.timestamp());
        assert_eq!(card.root_id, standalone.id);

        // Repo-qualified identities are displayed as the bare relative path.
        let qualified = task(
            Uuid::new_v4(),
            "repo:favetto/triage",
            TaskStatus::Pending,
            None,
        );
        let card = NodeCard::from_task(&qualified, qualified.created_at.timestamp());
        assert_eq!(card.name, "triage");
    }

    #[test]
    fn from_view_carries_summary_and_depth() {
        let view = WorkflowTaskView {
            id: Uuid::new_v4(),
            name: "child".to_string(),
            status: TaskStatus::Failed,
            attempt: 2,
            summary: Some("boom".to_string()),
        };
        let root = Uuid::new_v4();
        let card = NodeCard::from_view(&view, root, Some("1m".to_string()), 3);
        assert_eq!(card.root_id, root);
        assert_eq!(card.summary.as_deref(), Some("boom"));
        assert_eq!(card.depth, 3);
        assert_eq!(card.attempt, 2);

        // Repo-qualified identities are displayed as the bare relative path.
        let qualified = WorkflowTaskView {
            id: Uuid::new_v4(),
            name: "repo:web/pipelines/plan".to_string(),
            status: TaskStatus::Pending,
            attempt: 1,
            summary: None,
        };
        let card = NodeCard::from_view(&qualified, root, None, 0);
        assert_eq!(card.name, "pipelines/plan");
    }
}
