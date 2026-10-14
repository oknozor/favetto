//! Tasks view: a filterable list (`/tasks`) and a detail route (`/tasks/:id`).
//!
//! The list seeds the shared [`AppState`] task cache from `tasks.list`; the
//! live SSE `task.updated` pushes already update that cache in `app.rs`, so the
//! list re-renders with no extra wiring. The detail route fetches the full task
//! and its run history (`tasks.runs`), re-running when a push changes the
//! task's status or attempt.

use dioxus::prelude::*;
use favetto_wire::model::{FailureKind, RunStatus, Task, TaskRun, TaskStatus};
use favetto_wire::rpc::{TaskIdParams, TasksListParams};
use uuid::Uuid;

use crate::app::Route;
use crate::client::RpcClient;
use crate::components::output_viewer::{extract_output_text, OutputViewer};
use crate::state::AppState;

/// The status chips above the task list.
///
/// `TaskStatus::Pending` has no dedicated chip (only `All` shows it), matching
/// the filter list in the issue.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum TaskFilter {
    #[default]
    All,
    Running,
    /// `TaskStatus::AwaitingInput`.
    Awaiting,
    /// `TaskStatus::Succeeded`.
    Finished,
    Failed,
    Cancelled,
}

impl TaskFilter {
    /// The chips in display order.
    pub const ALL: [TaskFilter; 6] = [
        TaskFilter::All,
        TaskFilter::Running,
        TaskFilter::Awaiting,
        TaskFilter::Finished,
        TaskFilter::Failed,
        TaskFilter::Cancelled,
    ];

    /// The chip label.
    pub fn label(self) -> &'static str {
        match self {
            TaskFilter::All => "All",
            TaskFilter::Running => "Running",
            TaskFilter::Awaiting => "Awaiting",
            TaskFilter::Finished => "Finished",
            TaskFilter::Failed => "Failed",
            TaskFilter::Cancelled => "Cancelled",
        }
    }

    /// Whether a task with `status` belongs to this filter.
    pub fn matches(self, status: TaskStatus) -> bool {
        match self {
            TaskFilter::All => true,
            TaskFilter::Running => status == TaskStatus::Running,
            TaskFilter::Awaiting => status == TaskStatus::AwaitingInput,
            TaskFilter::Finished => status == TaskStatus::Succeeded,
            TaskFilter::Failed => status == TaskStatus::Failed,
            TaskFilter::Cancelled => status == TaskStatus::Cancelled,
        }
    }
}

/// Human label for a task status badge.
pub fn status_label(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Pending => "pending",
        TaskStatus::Running => "running",
        TaskStatus::AwaitingInput => "awaiting",
        TaskStatus::Succeeded => "succeeded",
        TaskStatus::Failed => "failed",
        TaskStatus::Cancelled => "cancelled",
    }
}

/// Human label for a run status badge.
pub fn run_status_label(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Pending => "pending",
        RunStatus::Running => "running",
        RunStatus::AwaitingInput => "awaiting",
        RunStatus::Succeeded => "succeeded",
        RunStatus::Failed => "failed",
        RunStatus::Cancelled => "cancelled",
        RunStatus::Interrupted => "interrupted",
        RunStatus::TimedOut => "timed out",
    }
}

/// Human label for a failure classification.
pub fn failure_kind_label(kind: FailureKind) -> &'static str {
    match kind {
        FailureKind::Agent => "agent",
        FailureKind::Infrastructure => "infrastructure",
        FailureKind::Timeout => "timeout",
        FailureKind::InvalidInput => "invalid input",
        FailureKind::Dependency => "dependency",
        FailureKind::Cancelled => "cancelled",
        FailureKind::Blocked => "blocked",
        FailureKind::Unknown => "unknown",
    }
}

/// Current UNIX time in milliseconds (browser clock on wasm, system clock on host).
pub fn now_ms() -> i64 {
    #[cfg(target_arch = "wasm32")]
    {
        js_sys::Date::now() as i64
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }
}

/// Compact age such as `12s`, `5m`, `3h`, `2d`.
pub fn format_age(created_ms: i64, now_ms: i64) -> String {
    let secs = (now_ms.saturating_sub(created_ms)).max(0) / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3_600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3_600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

/// Duration between two timestamps, or an em dash when either end is missing.
pub fn format_duration(started_ms: Option<i64>, finished_ms: Option<i64>) -> String {
    let (Some(started), Some(finished)) = (started_ms, finished_ms) else {
        return "—".to_string();
    };
    let ms = finished.saturating_sub(started).max(0);
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else if ms < 3_600_000 {
        let secs = ms / 1000;
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        let secs = ms / 1000;
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// A coloured status pill shared by the list, detail and run rows.
#[component]
fn StatusBadge(status: TaskStatus) -> Element {
    let class = format!("status-badge status-{}", status.as_str());
    rsx! {
        span { class, "{status_label(status)}" }
    }
}

/// The `/tasks` list with status filter chips.
#[component]
pub fn TasksList() -> Element {
    let mut state = use_context::<AppState>();
    let mut filter = use_signal(TaskFilter::default);

    // Seed the cache when the base URL or token changes. `daemon_url`/`token`
    // are read in the synchronous part so the resource subscribes to them.
    let list = use_resource(move || {
        let base = (state.daemon_url)();
        let token = (state.token)();
        async move {
            let client = RpcClient::new(base).with_token(token);
            match client.tasks_list(TasksListParams { limit: None }).await {
                Ok(tasks) => {
                    state.cache.write().set_tasks(tasks);
                    Ok(())
                }
                Err(e) => Err(e.to_string()),
            }
        }
    });

    let now = now_ms();
    let rows: Vec<Task> = {
        let cache = (state.cache)();
        let mut rows: Vec<Task> = cache.tasks.values().cloned().collect();
        rows.sort_by_key(|t| std::cmp::Reverse(t.created_at));
        rows.retain(|t| filter().matches(t.status));
        rows
    };

    rsx! {
        main { id: "favetto-app", class: "tasks-view",
            h1 { "Tasks" }
            div { class: "tasks-toolbar",
                for chip in TaskFilter::ALL {
                    button {
                        class: if filter() == chip { "filter-chip active" } else { "filter-chip" },
                        onclick: move |_| filter.set(chip),
                        "{chip.label()}"
                    }
                }
            }
            if let Some(Err(e)) = list() {
                div { class: "error-panel", "Failed to load tasks: {e}" }
            }
            if rows.is_empty() {
                p { class: "empty-state", "No tasks match this filter." }
            } else {
                ul { class: "task-list",
                    for task in rows {
                        li { class: "task-row",
                            Link {
                                to: Route::TaskDetail { id: task.id.to_string() },
                                class: "task-row-link",
                                div { class: "task-row-main",
                                    span { class: "task-name", "{task.name}" }
                                    StatusBadge { status: task.status }
                                }
                                div { class: "task-row-meta",
                                    span { class: "task-age", "{format_age(task.created_at.timestamp_millis(), now)}" }
                                    span { class: "task-attempts", "{task.attempt} runs" }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The loaded task plus its run history.
#[derive(Clone)]
struct Detail {
    task: Task,
    runs: Vec<TaskRun>,
}

/// The `/tasks/:id` detail route.
#[component]
pub fn TaskDetail(id: String) -> Element {
    let state = use_context::<AppState>();
    let parsed = Uuid::parse_str(&id).ok();

    // Subscribe to the cached task's (status, attempt) so an SSE `task.updated`
    // push re-fetches the full task + run history.
    let detail = use_resource(move || {
        let base = (state.daemon_url)();
        let token = (state.token)();
        let _key = parsed.and_then(|uuid| {
            (state.cache)()
                .tasks
                .get(&uuid)
                .map(|t| (t.status, t.attempt))
        });
        async move {
            let Some(uuid) = parsed else {
                return Err("task not found".to_string());
            };
            let client = RpcClient::new(base).with_token(token);
            let task = client
                .tasks_get(TaskIdParams { id: uuid })
                .await
                .map_err(|e| e.to_string())?;
            let runs = client
                .tasks_runs(TaskIdParams { id: uuid })
                .await
                .map_err(|e| e.to_string())?;
            Ok(Detail { task, runs })
        }
    });

    if parsed.is_none() {
        return not_found_panel(&id);
    }

    let now = now_ms();
    rsx! {
        main { id: "favetto-app", class: "task-detail",
            Link { to: Route::TasksList {}, class: "back-link", "← Tasks" }
            match detail() {
                Some(Err(e)) => rsx! {
                    div { class: "error-panel", "Failed to load task: {e}" }
                },
                Some(Ok(detail)) => {
                    let Detail { task, runs } = detail;
                    let (output_text, output_truncated) = task
                        .output
                        .as_ref()
                        .map(extract_output_text)
                        .unwrap_or_else(|| ("No output recorded.".to_string(), false));
                    let started = task
                        .started_at
                        .map(|t| t.to_string())
                        .unwrap_or_else(|| "—".to_string());
                    let finished = task
                        .finished_at
                        .map(|t| t.to_string())
                        .unwrap_or_else(|| "—".to_string());
                    let failure = task.failure.clone();
                    let legacy_error = task.error.clone();
                    let show_failure = failure.is_some() || task.status == TaskStatus::Failed;
                    rsx! {
                        header { class: "task-header",
                            h1 { class: "task-title", "{task.name}" }
                            StatusBadge { status: task.status }
                            dl { class: "task-timestamps",
                                dt { "Created" }
                                dd { "{task.created_at}" }
                                dt { "Started" }
                                dd { "{started}" }
                                dt { "Finished" }
                                dd { "{finished}" }
                                dt { "Age" }
                                dd { "{format_age(task.created_at.timestamp_millis(), now)}" }
                            }
                        }
                        if show_failure {
                            div { class: "failure-panel",
                                h2 { "Failure" }
                                if let Some(failure) = failure {
                                    dl {
                                        dt { "Kind" }
                                        dd { "{failure_kind_label(failure.kind)}" }
                                        dt { "Message" }
                                        dd { "{failure.message}" }
                                        dt { "Retryable" }
                                        dd { if failure.retryable { "yes" } else { "no" } }
                                        dt { "Retry count" }
                                        dd { "{task.attempt}" }
                                    }
                                } else if let Some(error) = legacy_error {
                                    p { "{error}" }
                                }
                            }
                        }
                        section { class: "run-history",
                            h2 { "Run history" }
                            if runs.is_empty() {
                                p { class: "empty-state", "No runs recorded." }
                            } else {
                                table { class: "run-history-table",
                                    thead {
                                        tr {
                                            th { "Attempt" }
                                            th { "Status" }
                                            th { "Started" }
                                            th { "Duration" }
                                            th { "Exit" }
                                            th { "Agent" }
                                            th { "Error" }
                                        }
                                    }
                                    tbody {
                                        for run in runs {
                                            tr { class: "run-row",
                                                td { "#{run.attempt}" }
                                                td {
                                                    span {
                                                        class: format!("status-badge status-{}", run.status.as_str()),
                                                        "{run_status_label(run.status)}"
                                                    }
                                                }
                                                td { "{run.started_at.map(|t| t.to_string()).unwrap_or_else(|| \"—\".to_string())}" }
                                                td {
                                                    "{format_duration(
                                                        run.started_at.map(|t| t.timestamp_millis()),
                                                        run.finished_at.map(|t| t.timestamp_millis()),
                                                    )}"
                                                }
                                                td { "{run.exit_code.map(|c| c.to_string()).unwrap_or_else(|| \"—\".to_string())}" }
                                                td { "{run.agent.clone().unwrap_or_else(|| \"—\".to_string())}" }
                                                td { "{run.error.clone().unwrap_or_else(|| \"—\".to_string())}" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        section { class: "task-output",
                            h2 { "Output" }
                            OutputViewer { text: output_text, truncated: output_truncated }
                        }
                    }
                }
                None => rsx! {
                    p { class: "loading", "Loading…" }
                },
            }
        }
    }
}

/// Shown when the route parameter is not a UUID.
fn not_found_panel(id: &str) -> Element {
    rsx! {
        main { id: "favetto-app", class: "task-detail",
            Link { to: Route::TasksList {}, class: "back-link", "← Tasks" }
            div { class: "error-panel", "Task not found: {id}" }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_matches_the_expected_statuses() {
        assert!(TaskFilter::All.matches(TaskStatus::Pending));
        assert!(TaskFilter::All.matches(TaskStatus::Failed));
        assert!(TaskFilter::Running.matches(TaskStatus::Running));
        assert!(TaskFilter::Awaiting.matches(TaskStatus::AwaitingInput));
        assert!(TaskFilter::Finished.matches(TaskStatus::Succeeded));
        assert!(TaskFilter::Failed.matches(TaskStatus::Failed));
        assert!(TaskFilter::Cancelled.matches(TaskStatus::Cancelled));
        // `Pending` is reachable only through `All`.
        for filter in TaskFilter::ALL {
            if filter != TaskFilter::All {
                assert!(!filter.matches(TaskStatus::Pending), "{filter:?}");
            }
        }
    }

    #[test]
    fn filter_labels_cover_every_chip() {
        let labels: Vec<&str> = TaskFilter::ALL.iter().map(|f| f.label()).collect();
        assert_eq!(
            labels,
            vec![
                "All",
                "Running",
                "Awaiting",
                "Finished",
                "Failed",
                "Cancelled"
            ]
        );
    }

    #[test]
    fn format_age_uses_the_right_unit() {
        let now = 1_000_000_000_000i64;
        assert_eq!(format_age(now - 12_000, now), "12s");
        assert_eq!(format_age(now - 5 * 60_000, now), "5m");
        assert_eq!(format_age(now - 3 * 3_600_000, now), "3h");
        assert_eq!(format_age(now - 2 * 86_400_000, now), "2d");
        // A clock skew into the future clamps to zero.
        assert_eq!(format_age(now + 5_000, now), "0s");
    }

    #[test]
    fn format_duration_handles_missing_ends_and_units() {
        assert_eq!(format_duration(None, Some(1)), "—");
        assert_eq!(format_duration(Some(1), None), "—");
        assert_eq!(format_duration(Some(0), Some(500)), "500ms");
        assert_eq!(format_duration(Some(0), Some(1_200)), "1.2s");
        assert_eq!(format_duration(Some(0), Some(184_000)), "3m04s");
        assert_eq!(format_duration(Some(0), Some(7_200_000)), "2h00m");
    }

    fn with_state(view: fn() -> Element) -> Element {
        use_context_provider(AppState::new);
        view()
    }

    #[test]
    fn tasks_list_mounts_with_state() {
        let mut dom = VirtualDom::new(|| with_state(|| rsx! { TasksList {} }));
        dom.rebuild_in_place();
    }

    #[test]
    fn task_detail_with_an_invalid_id_mounts() {
        let mut dom =
            VirtualDom::new(|| with_state(|| rsx! { TaskDetail { id: "not-a-uuid".to_string() } }));
        dom.rebuild_in_place();
    }
}
