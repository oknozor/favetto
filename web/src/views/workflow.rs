//! The Workflow view: a runtime board (`/workflow`) and a per-root inspect tree
//! (`/workflow/:root_id`).
//!
//! There is no `workflow.list` RPC: the board derives its runtime rows from
//! `tasks.list` (seeded into the cache and kept live by SSE `task.updated`
//! pushes) and shows `workflow.get`'s catalog graph as a separate section. The
//! detail view calls `workflow.inspect` and joins its rows with the cached tasks
//! for the fields the inspect payload omits (`parent_id`, timestamps).

use std::collections::{HashMap, HashSet};

use dioxus::prelude::*;
use favetto_wire::model::{Task, TaskStatus};
use favetto_wire::rpc::{
    EmptyParams, TasksListParams, WorkflowGetResult, WorkflowInspectParams, WorkflowRetryParams,
    WorkflowSpawnParams,
};
use favetto_wire::workflow::{WorkflowEdgeKind, WorkflowInspect, WorkflowState, WorkflowTaskView};
use uuid::Uuid;

use crate::app::Route;
use crate::client::RpcClient;
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

/// Bucket tasks newest-first. Always returns all six groups (possibly empty),
/// so the accordions are stable across renders.
pub fn bucket_tasks(tasks: &[Task]) -> Vec<(TaskStatus, Vec<Task>)> {
    GROUPS
        .iter()
        .map(|&status| {
            let mut group: Vec<Task> = tasks
                .iter()
                .filter(|task| task.status == status)
                .cloned()
                .collect();
            group.sort_by(|a, b| {
                b.created_at
                    .cmp(&a.created_at)
                    .then_with(|| b.id.cmp(&a.id))
            });
            (status, group)
        })
        .collect()
}

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

/// Parse the spawn dialog's `input` textarea. Empty/whitespace means "no input";
/// anything else must be valid JSON.
pub fn parse_variables(raw: &str) -> Result<Option<serde_json::Value>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    serde_json::from_str(trimmed)
        .map(Some)
        .map_err(|err| format!("variables must be valid JSON: {err}"))
}

/// Parse a comma-separated list of task ids. Blank input is an empty list.
pub fn parse_depends_on(raw: &str) -> Result<Vec<Uuid>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    trimmed
        .split(',')
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(|token| {
            Uuid::parse_str(token).map_err(|err| format!("invalid task id '{token}': {err}"))
        })
        .collect()
}

/// The spawn dialog's raw text fields.
#[derive(Clone, PartialEq, Default)]
pub struct SpawnForm {
    pub name: String,
    pub variables: String,
    pub root_id: String,
    pub depends_on: String,
    pub dedupe_key: String,
}

/// Validate a raw form into `workflow.spawn` params.
pub fn build_spawn_params(form: &SpawnForm) -> Result<WorkflowSpawnParams, String> {
    let name = form.name.trim();
    if name.is_empty() {
        return Err("task name is required".to_string());
    }

    let input = parse_variables(&form.variables)?;
    let depends_on = parse_depends_on(&form.depends_on)?;

    let root_id = match form.root_id.trim() {
        "" => None,
        raw => Some(Uuid::parse_str(raw).map_err(|err| format!("invalid root id '{raw}': {err}"))?),
    };
    let dedupe_key = match form.dedupe_key.trim() {
        "" => None,
        raw => Some(raw.to_string()),
    };

    Ok(WorkflowSpawnParams {
        name: name.to_string(),
        input,
        root_id,
        depends_on: (!depends_on.is_empty()).then_some(depends_on),
        dedupe_key,
    })
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

/// Human label for a catalog graph edge kind.
pub fn edge_kind_label(kind: WorkflowEdgeKind) -> &'static str {
    match kind {
        WorkflowEdgeKind::Spawn => "spawn",
        WorkflowEdgeKind::Needs => "needs",
        WorkflowEdgeKind::NeedsSucceeded => "needs:succeeded",
        WorkflowEdgeKind::NeedsFailed => "needs:failed",
        WorkflowEdgeKind::Join => "join",
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

/// The workflow board: runtime tasks grouped by status, plus the catalog graph.
#[component]
pub fn WorkflowList() -> Element {
    let state = use_context::<AppState>();
    let navigator = use_navigator();
    let mut catalog = use_signal(|| None::<WorkflowGetResult>);
    let mut error = use_signal(|| None::<String>);
    let busy = use_signal(|| false);
    let mut show_spawn = use_signal(|| false);

    use_future(move || async move {
        let client = client(state);
        match client
            .tasks_list(TasksListParams { limit: Some(500) })
            .await
        {
            Ok(tasks) => {
                let mut cache = state.cache;
                cache.write().set_tasks(tasks);
            }
            Err(err) => error.set(Some(err.to_string())),
        }
        match client.workflow_get(EmptyParams {}).await {
            Ok(result) => catalog.set(Some(result)),
            Err(err) => error.set(Some(err.to_string())),
        }
    });

    let all: Vec<Task> = (state.cache)().tasks.values().cloned().collect();
    let buckets = bucket_tasks(&all);
    let catalog_value = catalog();
    let error_value = error();

    rsx! {
        main { id: "favetto-app", class: "workflow-view",
            div { class: "workflow-header",
                h1 { "Workflows" }
                div { class: "workflow-actions",
                    button {
                        class: "btn primary",
                        onclick: move |_| show_spawn.set(true),
                        "Spawn workflow"
                    }
                }
            }
            if let Some(err) = error_value {
                p { class: "error-banner", role: "alert", "{err}" }
            }
            for (status, tasks) in buckets {
                details { class: "status-group", open: true, key: "{status_class(status)}",
                    summary { class: "status-group-summary",
                        span { class: "status-badge status-{status_class(status)}", "{status_label(status)}" }
                        span { class: "status-group-count", "{tasks.len()}" }
                    }
                    if tasks.is_empty() {
                        p { class: "empty-hint", "No tasks." }
                    }
                    for task in tasks {
                        WorkflowNode {
                            key: "{task.id}",
                            card: NodeCard::from_task(&task, now_epoch()),
                            busy: busy(),
                            on_cancel: move |root_id: Uuid| run_cancel(state, busy, error, root_id),
                            on_retry: move |task_id: Uuid| run_retry(state, busy, error, task_id),
                        }
                    }
                }
            }
            if let Some(catalog) = catalog_value {
                CatalogGraph { catalog }
            }
            if show_spawn() {
                SpawnDialog {
                    on_close: move |_| show_spawn.set(false),
                    on_spawned: move |root_id: Uuid| {
                        show_spawn.set(false);
                        navigator.push(Route::WorkflowDetail { root_id });
                    },
                }
            }
        }
    }
}

/// The catalog workflow graph (`workflow.get`): definitions, not a runtime
/// instance. DOT/SVG rendering is deferred to the Phase 5 DAG work.
#[component]
fn CatalogGraph(catalog: WorkflowGetResult) -> Element {
    let nodes = catalog.graph.nodes.clone();
    let edges = catalog.graph.edges.clone();
    let path = catalog.path.clone();

    rsx! {
        details { class: "catalog-graph",
            summary { "Catalog graph" }
            if nodes.is_empty() {
                p { class: "empty-hint", "The catalog has no workflow edges." }
            } else {
                ul { class: "catalog-graph-nodes",
                    for node in nodes {
                        li { class: "catalog-graph-node", key: "{node.name}",
                            span { class: "catalog-graph-node-name", "{node.name}" }
                            if node.scheduled {
                                span { class: "catalog-graph-marker", "(scheduled)" }
                            }
                            if node.external {
                                span { class: "catalog-graph-marker external", "(external)" }
                            }
                        }
                    }
                }
                ul { class: "catalog-graph-edges",
                    for edge in edges {
                        li { class: "catalog-graph-edge", key: "{edge.from}-{edge.to}-{edge_kind_label(edge.kind)}",
                            "{edge.from} → {edge.to} ({edge_kind_label(edge.kind)})"
                        }
                    }
                }
            }
            if !path.is_empty() {
                p { class: "catalog-graph-path", "source: {path}" }
            }
        }
    }
}

/// The `workflow.spawn` dialog: name + variables + optional lineage.
#[component]
fn SpawnDialog(props: SpawnDialogProps) -> Element {
    let state = use_context::<AppState>();
    let on_close = props.on_close;
    let on_spawned = props.on_spawned;
    let mut form = use_signal(SpawnForm::default);
    let mut error = use_signal(|| None::<String>);
    let mut submitting = use_signal(|| false);

    let submit = move |_| {
        let params = match build_spawn_params(&form()) {
            Ok(params) => params,
            Err(err) => {
                error.set(Some(err));
                return;
            }
        };
        error.set(None);
        submitting.set(true);
        spawn(async move {
            match client(state).workflow_spawn(params).await {
                Ok(task) => {
                    submitting.set(false);
                    on_spawned.call(task.root_or_self());
                }
                Err(err) => {
                    submitting.set(false);
                    error.set(Some(err.to_string()));
                }
            }
        });
    };

    let error_value = error();
    rsx! {
        div { class: "spawn-overlay",
            div { class: "spawn-dialog", role: "dialog", "aria-modal": "true",
                h2 { "Spawn workflow" }
                label { class: "field",
                    span { class: "field-label", "Task name" }
                    input {
                        class: "input",
                        value: "{form().name}",
                        placeholder: "e.g. implement_issue",
                        oninput: move |event| form.write().name = event.value(),
                    }
                }
                label { class: "field",
                    span { class: "field-label", "Variables (JSON, optional)" }
                    textarea {
                        class: "input",
                        value: "{form().variables}",
                        oninput: move |event| form.write().variables = event.value(),
                    }
                }
                label { class: "field",
                    span { class: "field-label", "Root id (optional)" }
                    input {
                        class: "input",
                        value: "{form().root_id}",
                        oninput: move |event| form.write().root_id = event.value(),
                    }
                }
                label { class: "field",
                    span { class: "field-label", "Depends on (comma-separated task ids, optional)" }
                    input {
                        class: "input",
                        value: "{form().depends_on}",
                        oninput: move |event| form.write().depends_on = event.value(),
                    }
                }
                label { class: "field",
                    span { class: "field-label", "Dedupe key (optional)" }
                    input {
                        class: "input",
                        value: "{form().dedupe_key}",
                        oninput: move |event| form.write().dedupe_key = event.value(),
                    }
                }
                if let Some(err) = error_value {
                    p { class: "spawn-error", role: "alert", "{err}" }
                }
                div { class: "spawn-dialog-actions",
                    button { class: "btn", onclick: move |_| on_close.call(()), "Cancel" }
                    button { class: "btn primary", disabled: submitting(), onclick: submit, "Spawn" }
                }
            }
        }
    }
}

/// Properties for [`SpawnDialog`].
#[derive(Props, Clone, PartialEq)]
pub struct SpawnDialogProps {
    pub on_close: EventHandler<()>,
    pub on_spawned: EventHandler<Uuid>,
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
    fn bucket_tasks_returns_all_six_groups_in_display_order() {
        let statuses = [
            TaskStatus::Running,
            TaskStatus::AwaitingInput,
            TaskStatus::Pending,
            TaskStatus::Failed,
            TaskStatus::Succeeded,
            TaskStatus::Cancelled,
        ];
        let tasks: Vec<Task> = statuses
            .iter()
            .map(|&status| task(Uuid::new_v4(), status, "2026-01-01T00:00:00Z"))
            .collect();
        let buckets = bucket_tasks(&tasks);
        assert_eq!(
            buckets.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
            GROUPS.to_vec()
        );
        for (status, group) in &buckets {
            assert_eq!(group.len(), 1, "{status:?} should hold exactly one task");
            assert_eq!(group[0].status, *status);
        }

        // Newest-first within a bucket.
        let older = task(Uuid::new_v4(), TaskStatus::Failed, "2026-01-01T00:00:00Z");
        let newer = task(Uuid::new_v4(), TaskStatus::Failed, "2026-06-01T00:00:00Z");
        let buckets = bucket_tasks(&[older.clone(), newer.clone()]);
        let failed = buckets
            .iter()
            .find(|(s, _)| *s == TaskStatus::Failed)
            .unwrap();
        assert_eq!(failed.1[0].id, newer.id);
        assert_eq!(failed.1[1].id, older.id);
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
    fn parse_variables_accepts_empty_and_rejects_invalid() {
        assert_eq!(parse_variables(""), Ok(None));
        assert_eq!(parse_variables("   "), Ok(None));
        assert_eq!(
            parse_variables(r#"{"a":1}"#),
            Ok(Some(serde_json::json!({ "a": 1 })))
        );
        assert!(parse_variables("not json").is_err());
    }

    #[test]
    fn parse_variables_handles_null_and_arrays() {
        assert_eq!(parse_variables("null"), Ok(Some(serde_json::Value::Null)));
        assert_eq!(
            parse_variables("[1,2]"),
            Ok(Some(serde_json::json!([1, 2])))
        );
    }

    #[test]
    fn parse_depends_on_splits_and_validates() {
        assert_eq!(parse_depends_on("   "), Ok(Vec::new()));
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        assert_eq!(parse_depends_on(&format!("{a}, {b}")), Ok(vec![a, b]));
        assert!(parse_depends_on("nope").is_err());
    }

    #[test]
    fn build_spawn_params_requires_name() {
        let form = SpawnForm::default();
        assert!(build_spawn_params(&form).is_err());

        let a = Uuid::new_v4();
        let form = SpawnForm {
            name: " implement_issue ".to_string(),
            variables: r#"{"issue":248}"#.to_string(),
            root_id: String::new(),
            depends_on: a.to_string(),
            dedupe_key: " dedupe ".to_string(),
        };
        let params = build_spawn_params(&form).unwrap();
        assert_eq!(params.name, "implement_issue");
        assert_eq!(params.input, Some(serde_json::json!({ "issue": 248 })));
        assert_eq!(params.depends_on, Some(vec![a]));
        assert_eq!(params.root_id, None);
        assert_eq!(params.dedupe_key.as_deref(), Some("dedupe"));
    }

    #[test]
    fn build_spawn_params_rejects_a_bad_root_id() {
        let form = SpawnForm {
            name: "x".to_string(),
            root_id: "not-a-uuid".to_string(),
            ..SpawnForm::default()
        };
        assert!(build_spawn_params(&form).is_err());
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
    fn edge_kind_labels_are_distinct() {
        let labels = [
            edge_kind_label(WorkflowEdgeKind::Spawn),
            edge_kind_label(WorkflowEdgeKind::Needs),
            edge_kind_label(WorkflowEdgeKind::NeedsSucceeded),
            edge_kind_label(WorkflowEdgeKind::NeedsFailed),
            edge_kind_label(WorkflowEdgeKind::Join),
        ];
        let unique: HashSet<&str> = labels.iter().copied().collect();
        assert_eq!(unique.len(), labels.len());
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
