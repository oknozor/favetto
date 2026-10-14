//! The Workflow view: the implicit catalog dependency graph (`/workflow`) and a
//! per-root runtime inspect tree (`/workflow/:root_id`).
//!
//! There is no explicit workflow object in favetto, so the panel renders the
//! graph `workflow.get` derives from the catalog's `needs`/`spawn` edges. The
//! catalog usually contains several weakly-connected components; a picker
//! selects one. The runtime instance view stays on `/workflow/:root_id`, which
//! calls `workflow.inspect` and joins its rows with the cached tasks for the
//! fields the inspect payload omits (`parent_id`, timestamps).

use std::collections::{HashMap, HashSet};

use dioxus::prelude::*;
use favetto_wire::model::{Task, TaskStatus};
use favetto_wire::rpc::{EmptyParams, TasksListParams, WorkflowInspectParams, WorkflowRetryParams};
use favetto_wire::workflow::{WorkflowInspect, WorkflowState, WorkflowTaskView};
use uuid::Uuid;

use crate::app::Route;
use crate::client::RpcClient;
use crate::components::workflow_graph::{
    component_label, weakly_connected_components, WorkflowGraphView,
};
use crate::components::workflow_node::{status_class, status_label, NodeCard, WorkflowNode};
use crate::state::AppState;

/// The six status buckets, attention-first.
pub const GROUPS: [TaskStatus; 6] = [
    TaskStatus::Running,
    TaskStatus::AwaitingInput,
    TaskStatus::Pending,
    TaskStatus::Failed,
    TaskStatus::Succeeded,
    TaskStatus::Cancelled,
];

/// Bucket already-built cards (used by the inspect tree).
pub fn bucket_cards(cards: &[NodeCard]) -> Vec<(TaskStatus, Vec<NodeCard>)> {
    GROUPS
        .iter()
        .map(|&status| {
            let group: Vec<NodeCard> = cards
                .iter()
                .filter(|card| card.status == status)
                .cloned()
                .collect();
            (status, group)
        })
        .collect()
}

/// Epoch seconds: the JS clock on wasm, `SystemTime` on the host.
pub fn now_epoch() -> i64 {
    #[cfg(target_arch = "wasm32")]
    {
        (js_sys::Date::now() / 1000.0) as i64
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }
}

/// Whole seconds elapsed, never negative.
pub fn age_secs(created: i64, now: i64) -> i64 {
    (now - created).max(0)
}

/// Humanize a duration: `5s`, `1m`, `2h`, `3d`. Negative values clamp to `0s`.
pub fn humanize_age(secs: i64) -> String {
    let secs = secs.max(0);
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

/// Age reference: `finished_at` for terminal tasks, `started_at` while in
/// flight, else `created_at`.
pub fn task_age(task: &Task, now: i64) -> String {
    let reference = task
        .finished_at
        .or(task.started_at)
        .unwrap_or(task.created_at)
        .timestamp();
    humanize_age(age_secs(reference, now))
}

/// Depth-first order (parent before child) with indentation depth.
///
/// `parent_of` returns the cached parent id when the caller has one. Tasks whose
/// parent is absent from `tasks` are treated as roots.
pub fn tree_order(
    tasks: &[WorkflowTaskView],
    parent_of: impl Fn(Uuid) -> Option<Uuid>,
) -> Vec<(Uuid, u32)> {
    let ids: HashSet<Uuid> = tasks.iter().map(|task| task.id).collect();
    let mut children: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    let mut roots: Vec<Uuid> = Vec::new();

    for task in tasks {
        match parent_of(task.id) {
            Some(parent) if parent != task.id && ids.contains(&parent) => {
                children.entry(parent).or_default().push(task.id);
            }
            _ => roots.push(task.id),
        }
    }

    let mut ordered: Vec<(Uuid, u32)> = Vec::with_capacity(tasks.len());
    let mut stack: Vec<(Uuid, u32)> = roots.iter().rev().map(|&id| (id, 0)).collect();
    while let Some((id, depth)) = stack.pop() {
        ordered.push((id, depth));
        if let Some(kids) = children.get(&id) {
            for &child in kids.iter().rev() {
                stack.push((child, depth + 1));
            }
        }
    }

    // Defensive: a parent cycle leaves tasks unreachable from any root.
    if ordered.len() < tasks.len() {
        for task in tasks {
            if !ordered.iter().any(|(id, _)| *id == task.id) {
                ordered.push((task.id, 0));
            }
        }
    }

    ordered
}

/// Stable CSS word for a workflow root state.
pub fn workflow_state_class(state: WorkflowState) -> &'static str {
    match state {
        WorkflowState::Running => "running",
        WorkflowState::Succeeded => "succeeded",
        WorkflowState::Failed => "failed",
        WorkflowState::Cancelled => "cancelled",
    }
}

/// Human label for a workflow root state.
pub fn workflow_state_label(state: WorkflowState) -> &'static str {
    match state {
        WorkflowState::Running => "Running",
        WorkflowState::Succeeded => "Succeeded",
        WorkflowState::Failed => "Failed",
        WorkflowState::Cancelled => "Cancelled",
    }
}

/// A bearer client built from the shared state.
fn client(state: AppState) -> RpcClient {
    RpcClient::new((state.daemon_url)()).with_token((state.token)())
}

/// Root-scoped cancel: `workflow.cancel` cancels every non-terminal task under
/// the root, so the card's button always targets `root_or_self()`.
fn run_cancel(
    state: AppState,
    mut busy: Signal<bool>,
    mut error: Signal<Option<String>>,
    root_id: Uuid,
) {
    spawn(async move {
        busy.set(true);
        if let Err(err) = client(state)
            .workflow_cancel(WorkflowInspectParams { root_id })
            .await
        {
            error.set(Some(err.to_string()));
        }
        busy.set(false);
    });
}

/// Single-task retry: `workflow.retry` re-enqueues one terminal task.
fn run_retry(
    state: AppState,
    mut busy: Signal<bool>,
    mut error: Signal<Option<String>>,
    task_id: Uuid,
) {
    spawn(async move {
        busy.set(true);
        if let Err(err) = client(state)
            .workflow_retry(WorkflowRetryParams { task_id })
            .await
        {
            error.set(Some(err.to_string()));
        }
        busy.set(false);
    });
}

/// The catalog dependency graph: one weakly-connected component at a time.
#[component]
pub fn WorkflowList() -> Element {
    let state = use_context::<AppState>();
    let navigator = use_navigator();
    let mut selected = use_signal(|| 0usize);

    // Re-fetch whenever a `catalog.updated` push bumps the version, mirroring
    // the Catalog view (there is no `workflow.list` RPC).
    let resource = use_resource(move || {
        let base = (state.daemon_url)();
        let token = (state.token)();
        let _ = (state.catalog_version)();
        async move {
            RpcClient::new(base)
                .with_token(token)
                .workflow_get(EmptyParams {})
                .await
        }
    });

    let (loaded, error_value) = match &*resource.read_unchecked() {
        Some(Ok(result)) => (Some(result.clone()), None),
        Some(Err(err)) => (None, Some(err.to_string())),
        None => (None, None),
    };
    let components = loaded
        .as_ref()
        .map(|result| weakly_connected_components(&result.graph))
        .unwrap_or_default();
    // Clamp after a catalog edit shrinks the component list.
    let index = if components.is_empty() {
        0
    } else {
        selected().min(components.len() - 1)
    };

    rsx! {
        main { id: "favetto-app", class: "workflow-view",
            div { class: "workflow-header",
                h1 { "Workflows" }
            }
            if let Some(err) = error_value {
                p { class: "error-banner", role: "alert", "{err}" }
            }
            if components.len() > 1 {
                label { class: "workflow-picker",
                    span { class: "workflow-picker-label", "Workflow" }
                    select {
                        aria_label: "Select workflow",
                        value: "{index}",
                        onchange: move |event| {
                            if let Ok(next) = event.value().parse::<usize>() {
                                selected.set(next);
                            }
                        },
                        for (position, component) in components.iter().enumerate() {
                            option { key: "{position}", value: "{position}", "{component_label(component)}" }
                        }
                    }
                }
            }
            match loaded {
                None => rsx! {},
                Some(_) if components.is_empty() => rsx! {
                    p { class: "empty-hint", "The catalog has no tasks." }
                },
                Some(result) => rsx! {
                    WorkflowGraphView {
                        graph: components[index].clone(),
                        on_select: move |name: String| {
                            navigator.push(Route::CatalogDetail {
                                name: name.split('/').map(str::to_string).collect(),
                            });
                        },
                    }
                    details { class: "workflow-dot",
                        summary { "Raw DOT" }
                        pre { "{result.dot}" }
                    }
                },
            }
        }
    }
}

/// The inspect tree for one workflow root (`workflow.inspect`).
#[component]
pub fn WorkflowDetail(root_id: Uuid) -> Element {
    let state = use_context::<AppState>();
    let mut inspect = use_signal(|| None::<WorkflowInspect>);
    let mut error = use_signal(|| None::<String>);
    let busy = use_signal(|| false);

    use_future(move || async move {
        let client = client(state);
        // Deep links may land before the SSE feed has populated the cache; seed
        // it so the parent/age joins resolve.
        if (state.cache)().tasks.is_empty() {
            if let Ok(tasks) = client
                .tasks_list(TasksListParams { limit: Some(500) })
                .await
            {
                let mut cache = state.cache;
                cache.write().set_tasks(tasks);
            }
        }
        match client
            .workflow_inspect(WorkflowInspectParams { root_id })
            .await
        {
            Ok(result) => inspect.set(Some(result)),
            Err(err) => error.set(Some(err.to_string())),
        }
    });

    let cache = (state.cache)();
    let error_value = error();
    let now = now_epoch();

    let (cards, header) = match inspect() {
        Some(view) => {
            let ids: HashSet<Uuid> = view.tasks.iter().map(|task| task.id).collect();
            let order = tree_order(&view.tasks, |id| {
                cache
                    .tasks
                    .get(&id)
                    .and_then(|task| task.parent_id)
                    .filter(|parent| ids.contains(parent))
            });
            let by_id: HashMap<Uuid, &WorkflowTaskView> =
                view.tasks.iter().map(|task| (task.id, task)).collect();
            let cards: Vec<NodeCard> = order
                .iter()
                .filter_map(|(id, depth)| {
                    let task_view = by_id.get(id)?;
                    let cached = cache.tasks.get(id);
                    let mut card = NodeCard::from_view(
                        task_view,
                        view.root_id,
                        cached.map(|task| task_age(task, now)),
                        *depth,
                    );
                    // Live status overlay: prefer the SSE-updated cache row.
                    if let Some(task) = cached {
                        card.status = task.status;
                        card.attempt = task.attempt;
                        if let Some(err) = task.error.clone() {
                            card.summary = Some(err);
                        }
                    }
                    Some(card)
                })
                .collect();
            let header = (
                view.root_task.clone(),
                view.state,
                view.ready.len(),
                view.running.len(),
                view.failed.len(),
                view.blocked.len(),
            );
            (cards, Some(header))
        }
        None => (Vec::new(), None),
    };

    let buckets = bucket_cards(&cards);

    rsx! {
        main { id: "favetto-app", class: "workflow-view",
            div { class: "workflow-header",
                h1 { "Workflow" }
                if let Some((root_task, root_state, ready, running, failed, blocked)) = header {
                    p { class: "workflow-root",
                        span { class: "workflow-root-name", "{root_task}" }
                        span { class: "status-badge status-{workflow_state_class(root_state)}", "{workflow_state_label(root_state)}" }
                    }
                    ul { class: "workflow-counts",
                        li { "ready {ready}" }
                        li { "running {running}" }
                        li { "failed {failed}" }
                        li { "blocked {blocked}" }
                    }
                }
                Link { to: Route::WorkflowList {}, class: "workflow-back", "← All workflows" }
            }
            if let Some(err) = error_value {
                p { class: "error-banner", role: "alert", "{err}" }
            }
            for (status, group) in buckets {
                details { class: "status-group", open: true, key: "{status_class(status)}",
                    summary { class: "status-group-summary",
                        span { class: "status-badge status-{status_class(status)}", "{status_label(status)}" }
                        span { class: "status-group-count", "{group.len()}" }
                    }
                    if group.is_empty() {
                        p { class: "empty-hint", "No tasks." }
                    }
                    for card in group {
                        WorkflowNode {
                            key: "{card.id}",
                            card,
                            busy: busy(),
                            on_cancel: move |root_id: Uuid| run_cancel(state, busy, error, root_id),
                            on_retry: move |task_id: Uuid| run_retry(state, busy, error, task_id),
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: Uuid, status: TaskStatus, created_at: &str) -> Task {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "name": "t",
            "status": status.as_str(),
            "attempt": 0,
            "input": {},
            "output": null,
            "dedupe_key": null,
            "created_at": created_at,
            "started_at": null,
            "finished_at": null,
            "error": null,
            "failure": null,
            "session_id": null,
            "session_title": null,
            "parent_id": null,
            "root_id": null,
            "interactive": false,
        }))
        .unwrap()
    }

    fn view(id: Uuid, name: &str) -> WorkflowTaskView {
        WorkflowTaskView {
            id,
            name: name.to_string(),
            status: TaskStatus::Running,
            attempt: 1,
            summary: None,
        }
    }

    #[test]
    fn humanize_age_units_and_clamp() {
        assert_eq!(humanize_age(5), "5s");
        assert_eq!(humanize_age(90), "1m");
        assert_eq!(humanize_age(7_200), "2h");
        assert_eq!(humanize_age(172_800), "2d");
        assert_eq!(humanize_age(-10), "0s");
    }

    #[test]
    fn task_age_prefers_finished_for_terminal_and_started_in_flight() {
        let mut finished = task(
            Uuid::new_v4(),
            TaskStatus::Succeeded,
            "2026-01-01T00:00:00Z",
        );
        finished.started_at = Some("2026-01-01T00:00:10Z".parse().unwrap());
        finished.finished_at = Some("2026-01-01T00:00:40Z".parse().unwrap());
        // 40s after the finish, not 30s after start.
        assert_eq!(
            task_age(&finished, finished.finished_at.unwrap().timestamp() + 40),
            "40s"
        );

        let mut in_flight = task(Uuid::new_v4(), TaskStatus::Running, "2026-01-01T00:00:00Z");
        in_flight.started_at = Some("2026-01-01T00:00:05Z".parse().unwrap());
        assert_eq!(
            task_age(&in_flight, in_flight.started_at.unwrap().timestamp() + 90),
            "1m"
        );

        let fresh = task(Uuid::new_v4(), TaskStatus::Pending, "2026-01-01T00:00:00Z");
        assert_eq!(task_age(&fresh, fresh.created_at.timestamp() + 5), "5s");
    }

    #[test]
    fn tree_order_places_parents_before_children() {
        let root = Uuid::new_v4();
        let child = Uuid::new_v4();
        let grandchild = Uuid::new_v4();
        let orphan = Uuid::new_v4();

        let tasks = vec![
            view(root, "root"),
            view(child, "child"),
            view(grandchild, "grandchild"),
            view(orphan, "orphan"),
        ];
        let parent_of = |id: Uuid| {
            if id == child {
                Some(root)
            } else if id == grandchild {
                Some(child)
            } else if id == orphan {
                // Parent is not part of the inspected set.
                Some(Uuid::new_v4())
            } else {
                None
            }
        };
        let order = tree_order(&tasks, parent_of);
        let positions: HashMap<Uuid, usize> = order
            .iter()
            .enumerate()
            .map(|(index, (id, _))| (*id, index))
            .collect();

        assert!(positions[&root] < positions[&child]);
        assert!(positions[&child] < positions[&grandchild]);
        // The orphan is treated as a root.
        assert_eq!(order.iter().find(|(id, _)| *id == orphan).unwrap().1, 0);
        assert_eq!(order.len(), 4);
        assert_eq!(order.iter().find(|(id, _)| *id == grandchild).unwrap().1, 2);
    }

    #[test]
    fn bucket_cards_returns_all_six_groups_in_display_order() {
        let cards: Vec<NodeCard> = [
            TaskStatus::Running,
            TaskStatus::AwaitingInput,
            TaskStatus::Pending,
            TaskStatus::Failed,
            TaskStatus::Succeeded,
            TaskStatus::Cancelled,
        ]
        .iter()
        .map(|&status| NodeCard {
            id: Uuid::new_v4(),
            root_id: Uuid::new_v4(),
            name: "t".to_string(),
            status,
            attempt: 1,
            age: None,
            summary: None,
            depth: 0,
        })
        .collect();
        let buckets = bucket_cards(&cards);
        assert_eq!(
            buckets
                .iter()
                .map(|(status, _)| *status)
                .collect::<Vec<_>>(),
            GROUPS.to_vec()
        );
        for (status, group) in &buckets {
            assert_eq!(group.len(), 1);
            assert_eq!(group[0].status, *status);
        }
    }

    #[test]
    fn workflow_state_labels_and_classes_match() {
        for state in [
            WorkflowState::Running,
            WorkflowState::Succeeded,
            WorkflowState::Failed,
            WorkflowState::Cancelled,
        ] {
            assert_eq!(
                workflow_state_class(state),
                workflow_state_label(state).to_lowercase()
            );
        }
    }
}
