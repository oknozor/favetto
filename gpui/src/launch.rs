//! The launch-failure model: spawn/dispatch failures derived from task rows.
//!
//! A workflow parent can finish successfully while every child it spawned fails
//! before an agent starts (a missing catalog entry, a missing required variable,
//! a worktree setup fault, a spawn manifest that could not be enqueued). Those
//! rows are already in `tasks.list`; this module turns them into a small,
//! persistent summary the Tasks, Workflows and Inbox surfaces can render without
//! opening a hidden error popup or depending on a transient toast.
//!
//! The derivation is deliberately free of GPUI types so it is unit-tested
//! without a window. It keeps the full `repo:<namespace>/<task>` target (display
//! names strip the repo prefix) so a casing mismatch is visible, and it ignores
//! failures left behind by an earlier parent attempt: only children created
//! after the succeeded parent's `finished_at` belong to the latest launch.

use std::collections::HashMap;

use favetto_wire::model::{Failure, FailureKind, Task, TaskStatus};
use uuid::Uuid;

/// A child that failed before its agent started, under a parent that succeeded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchFailure {
    /// The failed child's task id.
    pub task_id: Uuid,
    /// The direct parent that spawned it.
    pub parent_id: Uuid,
    /// The workflow root the failed launch belongs to.
    pub root_id: Uuid,
    /// The full failed target (`repo:Favetto/plan_issue`), casing preserved.
    pub target: String,
    /// The full name of the parent that succeeded.
    pub parent: String,
    /// The full name of the workflow root.
    pub root: String,
    /// The human-readable reason (`Task::error`, else the typed message).
    pub reason: String,
    /// The typed failure kind, when the row carries one.
    pub kind: Option<FailureKind>,
}

impl LaunchFailure {
    /// One complete sentence naming the target, the reason and the succeeded
    /// parent. Long enough for a banner, copy-free of UI types.
    pub fn detail(&self) -> String {
        format!(
            "{} did not start: {} (spawned by '{}', which succeeded)",
            self.target, self.reason, self.parent
        )
    }
}

/// Whether a typed failure is one that happens before an agent launch.
///
/// Ordinary agent/run failures keep their existing treatment. Legacy rows with
/// no typed failure are kept visible: a failed child of a succeeded parent is
/// exactly the silent fan-out this model exists to surface.
pub fn is_launch_failure(task: &Task) -> bool {
    match task.failure.as_ref() {
        Some(failure) => is_pre_launch_kind(failure),
        None => true,
    }
}

fn is_pre_launch_kind(failure: &Failure) -> bool {
    matches!(
        failure.kind,
        FailureKind::InvalidInput | FailureKind::Infrastructure | FailureKind::Unknown
    )
}

/// Derive every launch failure in a `tasks.list` snapshot, newest first.
///
/// A failure is a failed child whose direct parent is in the snapshot with
/// `succeeded` status, whose typed failure is pre-launch, and whose creation
/// time is not older than the parent's latest finish (so a retried parent's
/// stale failures do not keep warning after the work actually started).
pub fn launch_failures(tasks: &[Task]) -> Vec<LaunchFailure> {
    let by_id: HashMap<Uuid, &Task> = tasks.iter().map(|task| (task.id, task)).collect();
    let mut failures: Vec<LaunchFailure> = tasks
        .iter()
        .filter_map(|task| {
            if task.status != TaskStatus::Failed || !is_launch_failure(task) {
                return None;
            }
            let parent = by_id.get(&task.parent_id?)?;
            if parent.status != TaskStatus::Succeeded {
                return None;
            }
            // A parent retry re-spawns the same targets. Ignore rows created by
            // an earlier attempt so a healthy retry is not marked with warnings.
            if let Some(finished_at) = parent.finished_at {
                if task.created_at < finished_at {
                    return None;
                }
            }
            let root_id = task.root_or_self();
            let root = by_id.get(&root_id).copied().unwrap_or(parent);
            let reason = task
                .error
                .as_deref()
                .map(str::trim)
                .filter(|error| !error.is_empty())
                .map(str::to_string)
                .or_else(|| {
                    task.failure
                        .as_ref()
                        .map(|failure| failure.message.trim())
                        .filter(|message| !message.is_empty())
                        .map(str::to_string)
                })
                .unwrap_or_else(|| "launch failed".to_string());
            Some(LaunchFailure {
                task_id: task.id,
                parent_id: parent.id,
                root_id,
                target: task.name.clone(),
                parent: parent.name.clone(),
                root: root.name.clone(),
                reason,
                kind: task.failure.as_ref().map(|failure| failure.kind),
            })
        })
        .collect();

    failures.sort_by(|left, right| {
        let left_created = by_id.get(&left.task_id).map(|task| task.created_at);
        let right_created = by_id.get(&right.task_id).map(|task| task.created_at);
        right_created
            .cmp(&left_created)
            .then_with(|| right.task_id.cmp(&left.task_id))
    });
    failures
}

/// Up to two banner lines plus a `+N more` line, so a large broken fan-out stays
/// readable. Empty when the snapshot is healthy.
pub fn banner_lines(failures: &[LaunchFailure]) -> Vec<String> {
    if failures.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<String> = failures
        .iter()
        .take(2)
        .map(|failure| format!("⚠ Launch failed: {}", failure.detail()))
        .collect();
    if failures.len() > 2 {
        lines.push(format!(
            "⚠ …and {} more failed spawn{}",
            failures.len() - 2,
            if failures.len() - 2 == 1 { "" } else { "s" }
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use favetto_wire::model::Failure;

    fn at(offset_minutes: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000, 0).expect("timestamp")
            + chrono::Duration::minutes(offset_minutes)
    }

    fn task(id: u128, name: &str, status: TaskStatus, created: DateTime<Utc>) -> Task {
        Task {
            id: Uuid::from_u128(id),
            name: name.to_string(),
            status,
            attempt: 1,
            input: serde_json::Value::Null,
            output: None,
            dedupe_key: None,
            created_at: created,
            started_at: None,
            finished_at: None,
            error: None,
            failure: None,
            session_id: None,
            session_title: None,
            parent_id: None,
            root_id: None,
            interactive: false,
        }
    }

    fn succeeded_parent(id: u128, name: &str) -> Task {
        let mut parent = task(id, name, TaskStatus::Succeeded, at(-30));
        parent.finished_at = Some(at(-25));
        parent
    }

    fn failed_child(id: u128, name: &str, parent: &Task, kind: FailureKind, error: &str) -> Task {
        let mut child = task(id, name, TaskStatus::Failed, at(-20));
        child.parent_id = Some(parent.id);
        child.root_id = Some(parent.id);
        child.error = Some(error.to_string());
        child.failure = Some(Failure::new(kind, error));
        child.finished_at = Some(at(-19));
        child
    }

    #[test]
    fn a_succeeded_parent_with_failed_children_names_every_target() {
        let parent = succeeded_parent(1, "repo:favetto/triage_issues");
        let one = failed_child(
            2,
            "repo:Favetto/plan_issue",
            &parent,
            FailureKind::InvalidInput,
            "task 'repo:Favetto/plan_issue' not found in the catalog",
        );
        let two = failed_child(
            3,
            "repo:favetto/plan_issue",
            &parent,
            FailureKind::InvalidInput,
            "task 'repo:favetto/plan_issue' requires input variable 'issue_id'",
        );

        let failures = launch_failures(&[parent.clone(), one, two]);
        assert_eq!(failures.len(), 2);
        assert_eq!(failures[0].target, "repo:favetto/plan_issue");
        assert_eq!(failures[1].target, "repo:Favetto/plan_issue");
        for failure in &failures {
            assert_eq!(failure.parent, "repo:favetto/triage_issues");
            assert_eq!(failure.parent_id, parent.id);
            assert_eq!(failure.root, "repo:favetto/triage_issues");
            assert!(failure.reason.contains("plan_issue"));
            assert_eq!(failure.kind, Some(FailureKind::InvalidInput));
            let detail = failure.detail();
            assert!(detail.contains(&failure.target));
            assert!(detail.contains("which succeeded"));
        }
    }

    #[test]
    fn ordinary_agent_failures_are_not_launch_failures() {
        let parent = succeeded_parent(1, "repo:favetto/triage_issues");
        let child = failed_child(
            2,
            "repo:favetto/plan_issue",
            &parent,
            FailureKind::Agent,
            "exit code 1",
        );
        assert!(launch_failures(&[parent, child]).is_empty());
    }

    #[test]
    fn a_failed_parent_is_not_a_launch_failure() {
        let mut parent = succeeded_parent(1, "repo:favetto/triage_issues");
        parent.status = TaskStatus::Failed;
        let child = failed_child(
            2,
            "repo:favetto/plan_issue",
            &parent,
            FailureKind::InvalidInput,
            "task not found in the catalog",
        );
        assert!(launch_failures(&[parent, child]).is_empty());
    }

    #[test]
    fn stale_failures_from_an_earlier_parent_attempt_are_ignored() {
        let mut parent = succeeded_parent(1, "repo:favetto/triage_issues");
        parent.finished_at = Some(at(-5));
        let mut old = failed_child(
            2,
            "repo:favetto/plan_issue",
            &parent,
            FailureKind::InvalidInput,
            "old failure",
        );
        old.created_at = at(-20);
        old.finished_at = Some(at(-19));
        let mut fresh = failed_child(
            3,
            "repo:favetto/plan_issue",
            &parent,
            FailureKind::InvalidInput,
            "fresh failure",
        );
        fresh.created_at = at(-4);
        fresh.finished_at = Some(at(-3));

        let failures = launch_failures(&[parent, old, fresh]);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].reason, "fresh failure");
    }

    #[test]
    fn banner_lines_name_the_first_two_and_count_the_rest() {
        let parent = succeeded_parent(1, "repo:favetto/triage_issues");
        let children: Vec<Task> = (0..4)
            .map(|index| {
                failed_child(
                    10 + index,
                    "repo:favetto/plan_issue",
                    &parent,
                    FailureKind::InvalidInput,
                    "task not found in the catalog",
                )
            })
            .collect();
        let mut tasks = vec![parent];
        tasks.extend(children);
        let lines = banner_lines(&launch_failures(&tasks));
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("repo:favetto/plan_issue"));
        assert!(lines[2].contains("2 more failed spawns"));

        assert!(banner_lines(&[]).is_empty());
    }
}
