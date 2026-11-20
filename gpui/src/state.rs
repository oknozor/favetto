//! The client's GPUI entity: connection state, routing, settings and the last
//! ping.
//!
//! Only one [`ClientState`] exists (created by the root view). It owns the
//! [`Runtime`] and a foreground task that drains runtime updates; every UI
//! observation is a read of this entity, so there are no global statics
//! (docs/design/gpui-client.md §4).
//!
//! Step 03 adds the active route and the pushed-detail flag that drive the
//! shell. Steps 04–05 add the shared task/session snapshot and list models;
//! step 06 adds the opened run's output, structured feed, attempt table and
//! permission reply.

#![allow(dead_code)] // Accessors and `ping()` are consumed by the views in steps 03+.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use chrono::Utc;
use favetto_wire::model::{
    AgentCatalogEntry, AgentSessionInfo, AgentUsage, InputReply, Task, TaskStatus,
};
use favetto_wire::rpc::AgentEventPush;
use gpui::{ClipboardItem, Context, Task as GpuiTask};

use crate::catalog::{AgentChoice, CatalogState, OneShotFocus};
use crate::config::{self, ClientConfig};
use crate::detail::{DetailTab, PendingRequest, TaskDetail};
use crate::inbox::{self, InboxKey, InboxRows};
use crate::routes::Route;
use crate::runtime::{PingSnapshot, Runtime, Update};
use crate::sse::Push;
use crate::tasks::{self, TaskAction, TaskFilter, TaskList};
use crate::transport::ClientError;
use crate::workflow::{
    self, WorkflowCancelConfirm, WorkflowDetail, WorkflowGraphState, WorkflowRow,
};

/// How long a toast stays on screen before dismissing itself.
const TOAST_LINGER: Duration = Duration::from_secs(6);

/// Rows moved by PageUp/PageDown on the Tasks list.
const TASKS_PAGE_ROWS: usize = 10;

/// What the status line renders, derived from the transport.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ConnectionState {
    /// The first connection attempt has not finished yet.
    #[default]
    Connecting,
    /// The SSE stream is open and RPC calls are expected to succeed.
    Connected,
    /// The stream dropped and the reader is backing off before retrying.
    Reconnecting { attempt: u32, delay_ms: u64 },
    /// The daemon rejected the bearer token. `token_path` is the file to fix.
    Unauthorized { token_path: String },
    /// The runtime could not start, or the transport was not configured.
    Disconnected { message: String },
}

impl ConnectionState {
    /// A status-bar label. Status is never color-only: this string accompanies
    /// the colored dot (docs §3 accessibility note).
    pub fn label(&self) -> String {
        match self {
            Self::Connecting => "Connecting…".to_string(),
            Self::Connected => "Connected".to_string(),
            Self::Reconnecting { attempt, delay_ms } => {
                format!(
                    "Reconnecting (attempt {attempt}, retry in {:.0}s)",
                    *delay_ms as f64 / 1_000.0
                )
            }
            Self::Unauthorized { token_path } => {
                format!("Unauthorized — check the token in {token_path}")
            }
            Self::Disconnected { message } => format!("Disconnected — {message}"),
        }
    }

    /// True only when the stream is live.
    pub fn is_connected(&self) -> bool {
        matches!(self, Self::Connected)
    }
}

/// The first-fetch state of the shared task snapshot. The Inbox and the Tasks
/// surface read the same `tasks.list` + `agents.list` pair, so they share one
/// skeleton / error state (and one refresh, never two competing round trips).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SnapshotStatus {
    /// The first (`or a retried`) fetch is in flight and there are no rows yet.
    #[default]
    Loading,
    /// A snapshot has been folded in.
    Loaded,
    /// The last fetch failed; rows are kept when an older snapshot exists.
    Failed { message: String },
}

/// The semantic tone of a toast.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToastKind {
    /// Neutral information (an action did not apply).
    Info,
    /// The action succeeded.
    Success,
    /// The action failed and was rolled back.
    Error,
}

/// A short, self-dismissing message above the status bar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Toast {
    /// The tone.
    pub kind: ToastKind,
    /// The message. It names the task and the consequence, not just the RPC.
    pub message: String,
    /// Monotonic id so a stale timer cannot dismiss a newer toast.
    id: u64,
}

/// A cancel/retry awaiting explicit confirmation (docs §2, Hick's Law: the
/// destructive action is separated from the row and names the task).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskConfirm {
    /// Which RPC will run.
    pub action: TaskAction,
    /// The task the action applies to.
    pub task_id: uuid::Uuid,
    /// The display name shown in the dialog.
    pub task_name: String,
}

impl TaskConfirm {
    /// The dialog title.
    pub fn title(&self) -> &'static str {
        match self.action {
            TaskAction::Cancel => "Cancel task",
            TaskAction::Retry => "Retry task",
        }
    }

    /// The confirm button label.
    pub fn confirm_label(&self) -> &'static str {
        match self.action {
            TaskAction::Cancel => "Cancel task",
            TaskAction::Retry => "Retry task",
        }
    }

    /// Whether the confirm button uses the destructive style. Cancel is the
    /// only danger-colored control on the surface (docs §2, Von Restorff).
    pub fn danger(&self) -> bool {
        self.action == TaskAction::Cancel
    }

    /// The consequence, with the task name in quotes.
    pub fn body(&self) -> String {
        match self.action {
            TaskAction::Cancel => format!(
                "Cancel '{}'? Any in-flight agent process is stopped.",
                self.task_name
            ),
            TaskAction::Retry => format!(
                "Retry '{}'? Its prior run history is preserved.",
                self.task_name
            ),
        }
    }
}

/// The single client entity.
pub struct ClientState {
    connection: ConnectionState,
    config: ClientConfig,
    last_ping: Option<PingSnapshot>,
    cursor: Option<i64>,
    push_count: u64,
    last_push: Option<String>,
    last_error: Option<String>,
    /// The active top-level surface (owned here so views and the palette agree).
    active_route: Route,
    /// On narrow windows the detail region is a pushed page; this is the flag
    /// the root view uses instead of rendering a third column.
    detail_open: bool,
    /// The latest `tasks.list` snapshot.
    tasks: Vec<Task>,
    /// The flattened live sessions from `agents.list`.
    sessions: Vec<AgentSessionInfo>,
    /// The configured agent roster (the one-shot picker).
    agents: Vec<AgentCatalogEntry>,
    /// The Catalog surface: entries, filter, preview and the launch sheets.
    catalog: CatalogState,
    /// First-fetch skeleton / error state for the shared task snapshot.
    snapshot_status: SnapshotStatus,
    /// The highlighted Needs you row, preserved by identity across refreshes.
    inbox_selected: Option<InboxKey>,
    /// The Tasks segmented control.
    tasks_filter: TaskFilter,
    /// The Tasks fuzzy-search query. It survives filter changes.
    tasks_query: String,
    /// True while typed characters go to the Tasks search field (`/`).
    tasks_search_active: bool,
    /// The highlighted Tasks row, preserved by id across live updates.
    tasks_selected: Option<uuid::Uuid>,
    /// The last known position, used when the selected id disappears.
    tasks_selected_index: usize,
    /// The cancel/retry awaiting confirmation.
    task_confirm: Option<TaskConfirm>,
    /// The current toast and its monotonic id.
    toast: Option<Toast>,
    toast_id: u64,
    /// The run opened from a list (step 06 renders its detail).
    selected_task: Option<uuid::Uuid>,
    /// The open detail's output, structured feed and attempt table.
    detail: Option<TaskDetail>,
    /// The selected workflow root in the Workflows list.
    workflow_root: Option<uuid::Uuid>,
    /// The selected root's runtime inspect joined with cached dependencies.
    workflow_detail: Option<WorkflowDetail>,
    /// The last raw `workflow.inspect`, kept so a fresh task snapshot can
    /// re-join dependency labels without another RPC.
    workflow_inspect: Option<favetto_wire::workflow::WorkflowInspect>,
    /// True while a `workflow.inspect` read is in flight.
    workflow_loading: bool,
    /// A `task.updated` arrived while the inspect read was in flight; reload
    /// once it lands (coalesced, never one RPC per push).
    workflow_dirty: bool,
    /// The catalog DAG source shown beside the runtime view.
    workflow_graph: WorkflowGraphState,
    /// The root cancel awaiting confirmation.
    workflow_confirm: Option<WorkflowCancelConfirm>,
    /// The detail before an optimistic root cancel, for a guarded rollback.
    workflow_cancel_previous: Option<WorkflowDetail>,
    /// The prior row for each retry in flight, for a guarded rollback.
    workflow_retry_previous: HashMap<uuid::Uuid, WorkflowRow>,
    /// The daemon's repo name, derived from the `system.ping` cwd.
    repo: Option<String>,
    /// The foreground task that drains [`Runtime`] updates. Declared before the
    /// runtime so dropping the entity cancels the drain first.
    drain: Option<GpuiTask<()>>,
    runtime: Option<Runtime>,
}

impl ClientState {
    /// Load the settings, start the runtime, and begin draining updates.
    pub fn new(cx: &mut Context<Self>) -> Self {
        let config = config::load();
        let mut last_error = config.warnings.first().cloned();
        let mut snapshot_status = SnapshotStatus::Loading;

        let (runtime, connection, drain) = match Runtime::spawn(config.clone()) {
            Ok(runtime) => {
                let updates = runtime.updates();
                let drain = cx.spawn(async move |weak, cx| {
                    while let Ok(update) = updates.recv().await {
                        let alive = weak
                            .update(cx, |this: &mut Self, cx| {
                                this.apply(update, cx);
                                cx.notify();
                            })
                            .is_ok();
                        if !alive {
                            break;
                        }
                    }
                });
                (Some(runtime), ConnectionState::Connecting, Some(drain))
            }
            Err(message) => {
                if last_error.is_none() {
                    last_error = Some(message.clone());
                }
                snapshot_status = SnapshotStatus::Failed {
                    message: message.clone(),
                };
                (None, ConnectionState::Disconnected { message }, None)
            }
        };

        let mut state = Self {
            connection,
            config,
            last_ping: None,
            cursor: None,
            push_count: 0,
            last_push: None,
            last_error,
            active_route: Route::default(),
            detail_open: false,
            tasks: Vec::new(),
            sessions: Vec::new(),
            agents: Vec::new(),
            catalog: CatalogState::default(),
            snapshot_status: snapshot_status.clone(),
            inbox_selected: None,
            tasks_filter: TaskFilter::default(),
            tasks_query: String::new(),
            tasks_search_active: false,
            tasks_selected: None,
            tasks_selected_index: 0,
            task_confirm: None,
            toast: None,
            toast_id: 0,
            selected_task: None,
            detail: None,
            workflow_root: None,
            workflow_detail: None,
            workflow_inspect: None,
            workflow_loading: false,
            workflow_dirty: false,
            workflow_graph: WorkflowGraphState::default(),
            workflow_confirm: None,
            workflow_cancel_previous: None,
            workflow_retry_previous: HashMap::new(),
            repo: None,
            runtime,
            drain,
        };
        if let SnapshotStatus::Failed { message } = snapshot_status {
            state.catalog.fail(message);
        }
        state
    }

    /// Fold one runtime update into the entity.
    fn apply(&mut self, update: Update, cx: &mut Context<Self>) {
        match update {
            Update::Connection(connection) => {
                let became_connected = connection.is_connected() && !self.connection.is_connected();
                self.connection = connection;
                // Re-validate the RPC path and load the Inbox whenever the
                // stream (re)connects, so a recovered daemon also fills the
                // landing surface (docs §2, Doherty: no manual refresh).
                if became_connected {
                    self.ping();
                    self.refresh_snapshot();
                    self.refresh_catalog();
                } else if !self.connection.is_connected()
                    && !self.has_snapshot_data()
                    && matches!(
                        &self.connection,
                        ConnectionState::Unauthorized { .. } | ConnectionState::Disconnected { .. }
                    )
                {
                    // There may never be a Connected transition to fill the
                    // first fetch; show the fix and a Retry instead of a
                    // skeleton that never resolves.
                    self.snapshot_status = SnapshotStatus::Failed {
                        message: self.connection.label(),
                    };
                    if !self.catalog.has_entries() {
                        self.catalog.fail(self.connection.label());
                    }
                }
            }
            Update::Ping(snapshot) => {
                // A rejected token is a connection-level fact, whichever
                // transport noticed it first.
                if snapshot
                    .result
                    .as_ref()
                    .err()
                    .is_some_and(ClientError::is_unauthorized)
                {
                    self.connection = ConnectionState::Unauthorized {
                        token_path: self.config.token_source.label(),
                    };
                }
                if let Ok(result) = &snapshot.result {
                    self.repo = repo_name(&result.cwd);
                }
                self.last_ping = Some(snapshot);
            }
            Update::Push(push) => {
                self.push_count = self.push_count.saturating_add(1);
                self.last_push = Some(push.label());
                match push.as_ref() {
                    // Fold the row immediately (Doherty: a local change within
                    // 100 ms), then reload to pick up the session/request text.
                    Push::TaskUpdated(task) => {
                        // The open workflow view gets the row immediately, then
                        // one coalesced inspect reload (Doherty Threshold).
                        let workflow_root = self
                            .workflow_detail
                            .as_ref()
                            .filter(|detail| {
                                detail.root_id == task.root_or_self() || detail.root_id == task.id
                            })
                            .map(|detail| detail.root_id);
                        if let Some(root_id) = workflow_root {
                            if let Some(detail) = &mut self.workflow_detail {
                                detail.apply_task(task);
                            }
                            if self.active_route == Route::Workflows {
                                self.load_workflow_inspect(root_id);
                            }
                        }
                        let open =
                            self.detail.as_ref().map(|detail| detail.task_id) == Some(task.id);
                        let was_terminal = self
                            .detail
                            .as_ref()
                            .and_then(TaskDetail::status)
                            .is_some_and(tasks::is_terminal);
                        let attempt_changed = self
                            .detail
                            .as_ref()
                            .and_then(|detail| detail.task.as_ref())
                            .is_some_and(|existing| existing.attempt != task.attempt);
                        self.upsert_task(task.clone());
                        if open {
                            if let Some(detail) = &mut self.detail {
                                detail.update_summary(task.clone());
                            }
                            // A finished run persists its output and final
                            // attempt row, and a retry starts a fresh feed;
                            // both need the detail reads again.
                            if attempt_changed || (!was_terminal && tasks::is_terminal(task.status))
                            {
                                self.load_detail(task.id);
                            }
                        }
                        self.refresh_snapshot();
                    }
                    // Fold one structured event into the open detail, then
                    // reconcile the session roster (the permission banner's
                    // authoritative source).
                    Push::AgentEvent(event) => {
                        if self.event_belongs_to_open_task(event) {
                            if let Some(detail) = &mut self.detail {
                                detail.push_event(event.clone());
                            }
                        }
                        self.refresh_snapshot();
                    }
                    // A blocked or resolved prompt changes the agent roster,
                    // which only `agents.list` can reconcile.
                    Push::AgentState(_) => self.refresh_snapshot(),
                    // The definition files changed on disk: reload the list and
                    // drop the cached source so the preview shows the new text.
                    Push::CatalogUpdated => {
                        self.catalog.invalidate_preview();
                        self.refresh_catalog();
                        // The DAG source is derived from the same catalog.
                        if self.active_route == Route::Workflows || self.workflow_graph.loaded {
                            self.refresh_workflow_graph();
                        }
                    }
                    // Unknown method or event kind: counted above, ignored here
                    // (docs step 06: never fatal).
                    Push::Event(_)
                    | Push::AgentExit(_)
                    | Push::UnsupportedAgentEvent { .. }
                    | Push::Unknown(_) => {}
                }
            }
            Update::Cursor(cursor) => self.cursor = cursor,
            Update::Malformed(message) => self.last_error = Some(message),
            Update::SnapshotLoaded {
                tasks,
                sessions,
                agents,
            } => {
                self.tasks = tasks;
                self.sessions = sessions;
                self.agents = agents;
                self.snapshot_status = SnapshotStatus::Loaded;
                // The Workflows root list is derived from this same snapshot,
                // so a recovered daemon fills it without another RPC. The open
                // root re-joins its dependency labels against the new rows.
                self.rejoin_workflow_detail();
                self.ensure_workflow_root();
            }
            Update::SnapshotFailed { error } => {
                if error.is_unauthorized() {
                    self.connection = ConnectionState::Unauthorized {
                        token_path: self.config.token_source.label(),
                    };
                }
                self.snapshot_status = SnapshotStatus::Failed {
                    message: error.to_string(),
                };
            }
            Update::CatalogLoaded { entries } => {
                if let Some(name) = self.catalog.set_entries(entries) {
                    self.load_catalog_preview(&name);
                }
            }
            Update::CatalogFailed { error } => {
                if error.is_unauthorized() {
                    self.connection = ConnectionState::Unauthorized {
                        token_path: self.config.token_source.label(),
                    };
                }
                self.catalog.fail(error.to_string());
            }
            Update::CatalogPreviewLoaded { name, markdown } => {
                self.catalog.set_preview(&name, markdown);
            }
            Update::CatalogPreviewFailed { name, error } => {
                if error.is_unauthorized() {
                    self.connection = ConnectionState::Unauthorized {
                        token_path: self.config.token_source.label(),
                    };
                }
                self.catalog.fail_preview(&name, error.to_string());
            }
            Update::CatalogLaunchFinished { name, result } => {
                match result {
                    Ok(task) => {
                        let task = *task;
                        if self
                            .catalog
                            .launch()
                            .is_some_and(|form| form.entry.name == name)
                        {
                            self.catalog.close_launch();
                        }
                        let display = inbox::task_display_name(&task.name).to_string();
                        self.upsert_task(task.clone());
                        // The new run is the best possible confirmation: land
                        // on its detail instead of leaving the user in a list
                        // where the row may not have arrived yet.
                        self.navigate(Route::Tasks);
                        self.open_task(task.id);
                        self.show_toast(ToastKind::Success, format!("Started '{display}'."), cx);
                    }
                    Err(error) => {
                        if error.is_unauthorized() {
                            self.connection = ConnectionState::Unauthorized {
                                token_path: self.config.token_source.label(),
                            };
                        }
                        if let Some(form) = self.catalog.launch_mut() {
                            if form.entry.name == name {
                                form.submitting = false;
                                form.server_error =
                                    Some(format!("The daemon rejected the launch: {error}."));
                            }
                        }
                    }
                }
            }
            Update::OneshotFinished { result } => match result {
                Ok(result) => {
                    let result = *result;
                    self.catalog.close_oneshot();
                    self.upsert_task(result.task.clone());
                    // The session is already live; fold it in so the detail
                    // header shows its agent and id without waiting for the
                    // next roster poll.
                    self.sessions.push(result.session);
                    self.navigate(Route::Tasks);
                    self.open_task(result.task.id);
                    self.show_toast(
                        ToastKind::Success,
                        "Opened a new one-shot session.".to_string(),
                        cx,
                    );
                }
                Err(error) => {
                    if error.is_unauthorized() {
                        self.connection = ConnectionState::Unauthorized {
                            token_path: self.config.token_source.label(),
                        };
                    }
                    if let Some(form) = self.catalog.oneshot_mut() {
                        form.submitting = false;
                        form.server_error =
                            Some(format!("The daemon could not open the session: {error}."));
                    }
                }
            },
            Update::TaskLoaded { task } => {
                if let Some(detail) = self.detail.as_mut() {
                    if detail.task_id == task.id {
                        detail.set_task(*task);
                    }
                }
            }
            Update::TaskLoadFailed { task_id, error } => {
                if error.is_unauthorized() {
                    self.connection = ConnectionState::Unauthorized {
                        token_path: self.config.token_source.label(),
                    };
                }
                if let Some(detail) = self.detail.as_mut() {
                    if detail.task_id == task_id {
                        detail.fail_task(error.to_string());
                    }
                }
            }
            Update::EventsLoaded { task_id, result } => {
                if let Some(detail) = self.detail.as_mut() {
                    if detail.task_id == task_id {
                        let result = *result;
                        detail.merge_events(
                            result.session_id,
                            result.events,
                            result.next_cursor,
                            result.ignored,
                        );
                    }
                }
            }
            Update::EventsFailed { task_id, error } => {
                if error.is_unauthorized() {
                    self.connection = ConnectionState::Unauthorized {
                        token_path: self.config.token_source.label(),
                    };
                }
                if let Some(detail) = self.detail.as_mut() {
                    if detail.task_id == task_id {
                        detail.fail_events(error.to_string());
                    }
                }
            }
            Update::RunsLoaded { task_id, runs } => {
                if let Some(detail) = self.detail.as_mut() {
                    if detail.task_id == task_id {
                        detail.set_runs(runs);
                    }
                }
            }
            Update::RunsFailed { task_id, error } => {
                if error.is_unauthorized() {
                    self.connection = ConnectionState::Unauthorized {
                        token_path: self.config.token_source.label(),
                    };
                }
                if let Some(detail) = self.detail.as_mut() {
                    if detail.task_id == task_id {
                        detail.fail_runs(error.to_string());
                    }
                }
            }
            Update::ReplyFinished {
                request_id,
                reply,
                result,
                ..
            } => {
                let open = self.detail.as_ref().is_some_and(|detail| {
                    detail.reply_pending.as_deref() == Some(request_id.as_str())
                });
                if let Some(detail) = self.detail.as_mut() {
                    if detail.reply_pending.as_deref() == Some(request_id.as_str()) {
                        detail.reply_pending = None;
                    }
                }
                match result {
                    Ok(result) if result.replied => {
                        if open {
                            if let Some(detail) = self.detail.as_mut() {
                                detail.answered_request = Some(request_id);
                            }
                        }
                        self.show_toast(ToastKind::Success, reply_done_message(&reply), cx);
                    }
                    Ok(_) => self.show_toast(
                        ToastKind::Error,
                        "The daemon did not accept the reply. The prompt is still open."
                            .to_string(),
                        cx,
                    ),
                    Err(error) => {
                        if error.is_unauthorized() {
                            self.connection = ConnectionState::Unauthorized {
                                token_path: self.config.token_source.label(),
                            };
                        }
                        self.show_toast(
                            ToastKind::Error,
                            format!("Could not answer the agent: {error}."),
                            cx,
                        );
                    }
                }
                // The daemon also pushes `agent.state`; one reconciling load
                // keeps the banner's request text in step.
                self.refresh_snapshot();
            }
            Update::TaskActionFinished {
                action,
                previous,
                optimistic,
                result,
            } => {
                let previous = *previous;
                let optimistic = *optimistic;
                let name = inbox::task_display_name(&previous.name).to_string();
                match result {
                    Ok(task) => {
                        let name = inbox::task_display_name(&task.name).to_string();
                        self.upsert_task(*task);
                        let verb = match action {
                            TaskAction::Cancel => "Cancelled",
                            TaskAction::Retry => "Retried",
                        };
                        self.show_toast(ToastKind::Success, format!("{verb} '{name}'."), cx);
                    }
                    Err(error) => {
                        if error.is_unauthorized() {
                            self.connection = ConnectionState::Unauthorized {
                                token_path: self.config.token_source.label(),
                            };
                        }
                        self.rollback_task(&previous, &optimistic);
                        let verb = match action {
                            TaskAction::Cancel => "cancel",
                            TaskAction::Retry => "retry",
                        };
                        self.show_toast(
                            ToastKind::Error,
                            format!(
                                "Could not {verb} '{name}': {error}. The change was rolled back."
                            ),
                            cx,
                        );
                    }
                }
                // The daemon also pushes `task.updated`; one reconciling load
                // keeps session-derived fields (agent, cost) in step.
                if self
                    .detail
                    .as_ref()
                    .is_some_and(|detail| detail.task_id == previous.id)
                {
                    self.load_detail(previous.id);
                }
                self.refresh_snapshot();
            }
            Update::WorkflowInspectLoaded { root_id, view } => {
                if self
                    .workflow_detail
                    .as_ref()
                    .is_none_or(|detail| detail.root_id != root_id)
                {
                    // A stale response for a root the user already left.
                    return;
                }
                self.workflow_inspect = Some((*view).clone());
                self.workflow_loading = false;
                self.rejoin_workflow_detail();
                if self.workflow_dirty {
                    self.workflow_dirty = false;
                    if self.active_route == Route::Workflows {
                        if let Some(root_id) = self.workflow_root {
                            self.load_workflow_inspect(root_id);
                        }
                    }
                }
            }
            Update::WorkflowInspectFailed { root_id, error } => {
                if self
                    .workflow_detail
                    .as_ref()
                    .is_none_or(|detail| detail.root_id != root_id)
                {
                    // A stale failure for a root the user already left.
                    return;
                }
                self.workflow_loading = false;
                self.workflow_dirty = false;
                if error.is_unauthorized() {
                    self.connection = ConnectionState::Unauthorized {
                        token_path: self.config.token_source.label(),
                    };
                }
                if let Some(detail) = self.workflow_detail.as_mut() {
                    detail.loading = false;
                    detail.error = Some(error.to_string());
                }
            }
            Update::WorkflowGraphLoaded { result } => {
                self.workflow_graph.set(result.dot, result.path);
            }
            Update::WorkflowGraphFailed { error } => {
                if error.is_unauthorized() {
                    self.connection = ConnectionState::Unauthorized {
                        token_path: self.config.token_source.label(),
                    };
                }
                self.workflow_graph.fail(error.to_string());
            }
            Update::WorkflowCancelFinished { root_id, result } => {
                match result {
                    Ok(result) => {
                        let cancelled: Vec<uuid::Uuid> = result.cancelled;
                        if let Some(detail) = self.workflow_detail.as_mut() {
                            if detail.root_id == root_id {
                                detail.finish_cancel(&cancelled);
                            }
                        }
                        // Keep the shared snapshot in step until its own push
                        // and reconciling load land.
                        let now = Utc::now();
                        for task in &mut self.tasks {
                            if cancelled.contains(&task.id) {
                                task.status = TaskStatus::Cancelled;
                                task.finished_at = Some(now);
                            }
                        }
                        self.workflow_cancel_previous = None;
                        let count = cancelled.len();
                        let label = if count == 1 {
                            "1 instance".to_string()
                        } else {
                            format!("{count} instances")
                        };
                        self.show_toast(
                            ToastKind::Success,
                            format!("Cancelled {label} in the workflow."),
                            cx,
                        );
                    }
                    Err(error) => {
                        if error.is_unauthorized() {
                            self.connection = ConnectionState::Unauthorized {
                                token_path: self.config.token_source.label(),
                            };
                        }
                        if let Some(previous) = self.workflow_cancel_previous.take() {
                            if let Some(detail) = self.workflow_detail.as_mut() {
                                if detail.root_id == root_id {
                                    *detail = previous;
                                }
                            }
                        }
                        self.show_toast(
                            ToastKind::Error,
                            format!("Could not cancel the workflow: {error}. The change was rolled back."),
                            cx,
                        );
                    }
                }
                if self.workflow_root == Some(root_id)
                    || self
                        .workflow_detail
                        .as_ref()
                        .is_some_and(|detail| detail.root_id == root_id)
                {
                    self.load_workflow_inspect(root_id);
                }
                self.refresh_snapshot();
            }
            Update::WorkflowRetryFinished { task_id, result } => {
                match result {
                    Ok(task) => {
                        self.workflow_retry_previous.remove(&task_id);
                        let task = *task;
                        let name = inbox::task_display_name(&task.name).to_string();
                        let attempt = task.attempt;
                        if let Some(detail) = self.workflow_detail.as_mut() {
                            detail.finish_retry(&task);
                        }
                        self.upsert_task(task);
                        self.show_toast(
                            ToastKind::Success,
                            format!("Retried '{name}' (attempt {attempt})."),
                            cx,
                        );
                    }
                    Err(error) => {
                        if error.is_unauthorized() {
                            self.connection = ConnectionState::Unauthorized {
                                token_path: self.config.token_source.label(),
                            };
                        }
                        if let Some(previous) = self.workflow_retry_previous.remove(&task_id) {
                            if let Some(detail) = self.workflow_detail.as_mut() {
                                detail.rollback_retry(previous);
                            }
                        }
                        self.show_toast(
                            ToastKind::Error,
                            format!("Could not retry the instance: {error}. The change was rolled back."),
                            cx,
                        );
                    }
                }
                if let Some(root_id) = self.workflow_root {
                    self.load_workflow_inspect(root_id);
                }
                self.refresh_snapshot();
            }
        }
    }

    /// Restart the transport (the status bar's Reconnect action).
    pub fn reconnect(&mut self) {
        match &self.runtime {
            Some(runtime) => {
                self.connection = ConnectionState::Connecting;
                self.last_error = None;
                runtime.reconnect();
            }
            None => {
                self.last_error = Some("the transport is not running".to_string());
            }
        }
    }

    /// Ask the runtime for a fresh ping.
    pub fn ping(&mut self) {
        if let Some(runtime) = &self.runtime {
            runtime.ping();
        }
    }

    /// Ask for a fresh task snapshot (the surfaces' Retry action and pushes).
    ///
    /// Without rows the surfaces show their first-fetch skeleton; with rows the
    /// old snapshot stays visible until the new one lands, so a refresh never
    /// flickers the list away. The runtime coalesces overlapping requests into
    /// one RPC round trip.
    pub fn refresh_snapshot(&mut self) {
        match &self.runtime {
            Some(runtime) => {
                // Keep an already-loaded (possibly empty) list visible. Only a
                // first fetch, or a retry after a data-less failure, shows the
                // skeleton.
                let show_skeleton = !self.has_snapshot_data()
                    && !matches!(self.snapshot_status, SnapshotStatus::Loaded);
                if show_skeleton {
                    self.snapshot_status = SnapshotStatus::Loading;
                }
                runtime.refresh_snapshot();
            }
            None => {
                self.snapshot_status = SnapshotStatus::Failed {
                    message: "the transport is not running".to_string(),
                };
            }
        }
    }

    /// Ask the runtime for a fresh `workflow.inspect` for the selected root.
    ///
    /// Overlapping requests coalesce: a burst of `task.updated` pushes marks the
    /// view dirty and a single follow-up read lands after the current one.
    fn load_workflow_inspect(&mut self, root_id: uuid::Uuid) {
        let same_root = self
            .workflow_detail
            .as_ref()
            .is_some_and(|detail| detail.root_id == root_id);
        if self.workflow_loading && same_root {
            self.workflow_dirty = true;
            return;
        }

        let name = self.workflow_root_name(root_id);
        if same_root {
            if let Some(detail) = &mut self.workflow_detail {
                detail.loading = true;
                detail.error = None;
            }
        } else {
            // A new root starts from a loading placeholder, never the previous
            // root's rows.
            self.workflow_detail = Some(WorkflowDetail::loading(root_id, name));
            self.workflow_inspect = None;
            self.workflow_dirty = false;
        }
        self.workflow_loading = true;
        match &self.runtime {
            Some(runtime) => runtime.load_workflow_inspect(root_id),
            None => {
                self.workflow_loading = false;
                if let Some(detail) = self.workflow_detail.as_mut() {
                    if detail.root_id == root_id {
                        detail.loading = false;
                        detail.error = Some("the transport is not running".to_string());
                    }
                }
            }
        }
    }

    /// Rebuild the open root's rows from the cached inspect payload and the
    /// current task snapshot. This refreshes dependency labels (which only live
    /// on the full task rows) without another `workflow.inspect` RPC, while
    /// preserving the highlight and any optimistic action.
    fn rejoin_workflow_detail(&mut self) {
        let Some(inspect) = self.workflow_inspect.clone() else {
            return;
        };
        let Some(old) = self.workflow_detail.as_ref() else {
            return;
        };
        if old.root_id != inspect.root_id {
            return;
        }
        let selected = old.selected_id();
        let pending_cancel = old.pending_cancel;
        let pending_retries: Vec<uuid::Uuid> = old
            .rows
            .iter()
            .filter(|row| row.retry_pending)
            .map(|row| row.id)
            .collect();
        let mut fresh = WorkflowDetail::from_inspect(&inspect, &self.tasks, selected);
        if pending_cancel {
            // Keep the optimistic cancellation while the RPC is still in
            // flight; the response reconciles it.
            fresh.begin_cancel();
            fresh.pending_cancel = true;
        }
        for id in pending_retries {
            fresh.begin_retry(id);
        }
        self.workflow_detail = Some(fresh);
    }

    /// Keep the selected root valid as the shared task snapshot changes. Picks
    /// the newest root when nothing is selected and drops a root that vanished.
    fn ensure_workflow_root(&mut self) {
        if self.active_route != Route::Workflows {
            return;
        }
        let roots = self.workflow_roots();
        if roots.is_empty() {
            self.workflow_root = None;
            self.workflow_detail = None;
            self.workflow_inspect = None;
            self.workflow_loading = false;
            self.workflow_dirty = false;
            return;
        }
        let selected = self
            .workflow_root
            .filter(|id| roots.iter().any(|root| root.root_id == *id))
            .unwrap_or_else(|| roots[0].root_id);
        let changed = self.workflow_root != Some(selected);
        self.workflow_root = Some(selected);
        let detail_matches = self
            .workflow_detail
            .as_ref()
            .is_some_and(|detail| detail.root_id == selected);
        if !detail_matches {
            self.load_workflow_inspect(selected);
        } else if changed {
            // The previous selection came back into the list; refresh it.
            self.load_workflow_inspect(selected);
        }
    }

    fn workflow_root_name(&self, root_id: uuid::Uuid) -> String {
        self.tasks
            .iter()
            .find(|task| task.id == root_id)
            .or_else(|| {
                self.tasks
                    .iter()
                    .find(|task| task.root_or_self() == root_id)
            })
            .map(|task| inbox::task_display_name(&task.name).to_string())
            .unwrap_or_else(|| format!("workflow {}", short_id(root_id)))
    }

    /// The roots visible in the shared task snapshot, newest first.
    pub fn workflow_roots(&self) -> Vec<workflow::WorkflowRoot> {
        workflow::roots(&self.tasks)
    }

    /// The selected root, falling back to the newest one so the surface is
    /// useful immediately.
    pub fn selected_workflow_root(&self) -> Option<uuid::Uuid> {
        let roots = self.workflow_roots();
        self.workflow_root
            .filter(|id| roots.iter().any(|root| root.root_id == *id))
            .or_else(|| roots.first().map(|root| root.root_id))
    }

    /// The selected root's joined runtime view.
    pub fn workflow_detail(&self) -> Option<&WorkflowDetail> {
        self.workflow_detail.as_ref()
    }

    /// The catalog DAG source (`workflow.get`).
    pub fn workflow_graph(&self) -> &WorkflowGraphState {
        &self.workflow_graph
    }

    /// Select a root in the Workflows list (a row click).
    pub fn select_workflow_root(&mut self, root_id: uuid::Uuid) {
        if self.workflow_root == Some(root_id)
            && self
                .workflow_detail
                .as_ref()
                .is_some_and(|detail| detail.root_id == root_id)
        {
            return;
        }
        self.workflow_root = Some(root_id);
        if self
            .workflow_detail
            .as_ref()
            .is_none_or(|detail| detail.root_id != root_id)
        {
            self.load_workflow_inspect(root_id);
        }
    }

    /// Open the Workflows surface for one root (the task detail's button).
    pub fn open_workflow_root(&mut self, root_id: uuid::Uuid) {
        self.workflow_root = Some(root_id);
        self.navigate(Route::Workflows);
        // `navigate` may have fallen back to another root if the requested one
        // was not in the snapshot yet; the explicit request wins.
        self.workflow_root = Some(root_id);
        if self
            .workflow_detail
            .as_ref()
            .is_none_or(|detail| detail.root_id != root_id)
        {
            self.load_workflow_inspect(root_id);
        }
        if !self.workflow_graph.loaded && !self.workflow_graph.loading {
            self.refresh_workflow_graph();
        }
    }

    /// The "View workflow" action on a task: use `root_or_self` exactly like
    /// the TUI, so a directly started run inspects its own single-node root.
    pub fn open_workflow_for_task(&mut self, task_id: uuid::Uuid, cx: &mut Context<Self>) {
        let root_id = self
            .tasks
            .iter()
            .find(|task| task.id == task_id)
            .map(Task::root_or_self)
            .or_else(|| self.selected_task().map(Task::root_or_self));
        match root_id {
            Some(root_id) => self.open_workflow_root(root_id),
            None => self.show_toast(
                ToastKind::Info,
                "That run is no longer in the list, so its workflow cannot be opened.".to_string(),
                cx,
            ),
        }
    }

    /// Ask for a fresh inspect read and DAG source (the surface's Refresh).
    pub fn refresh_workflow(&mut self) {
        self.refresh_snapshot();
        if let Some(root_id) = self.workflow_root.or_else(|| self.selected_workflow_root()) {
            self.load_workflow_inspect(root_id);
        }
        self.refresh_workflow_graph();
    }

    /// Fetch the catalog DAG source. An older source stays visible on error.
    pub fn refresh_workflow_graph(&mut self) {
        if self.workflow_graph.loading {
            return;
        }
        self.workflow_graph.begin_load();
        match &self.runtime {
            Some(runtime) => runtime.load_workflow_graph(),
            None => self
                .workflow_graph
                .fail("the transport is not running".to_string()),
        }
    }

    /// Move the highlighted instance within the selected root.
    pub fn move_workflow_selection(&mut self, delta: isize) {
        if let Some(detail) = self.workflow_detail.as_mut() {
            detail.move_selection(delta);
        }
    }

    /// Home/End on the Workflows instance list.
    pub fn workflow_selection_edge(&mut self, last: bool) {
        if let Some(detail) = self.workflow_detail.as_mut() {
            detail.select_edge(last);
        }
    }

    /// PageUp/PageDown on the Workflows instance list.
    pub fn workflow_selection_page(&mut self, direction: isize) {
        if let Some(detail) = self.workflow_detail.as_mut() {
            detail.move_selection(direction * TASKS_PAGE_ROWS as isize);
        }
    }

    /// Highlight one workflow instance (a row click).
    pub fn select_workflow_instance(&mut self, task_id: uuid::Uuid) {
        if let Some(detail) = self.workflow_detail.as_mut() {
            detail.select(task_id);
        }
    }

    /// Open the highlighted workflow instance in the task detail.
    pub fn open_workflow_selected(&mut self) -> Option<uuid::Uuid> {
        let task_id = self.workflow_detail.as_ref()?.selected_id()?;
        self.navigate(Route::Tasks);
        self.select_tasks_row(task_id);
        self.open_task(task_id);
        Some(task_id)
    }

    /// Open the root-cancel confirmation, naming the number of instances.
    pub fn request_workflow_cancel(&mut self, cx: &mut Context<Self>) {
        let Some(detail) = self.workflow_detail.as_ref() else {
            self.show_toast(
                ToastKind::Info,
                "Select a workflow root first.".to_string(),
                cx,
            );
            return;
        };
        let cancellable = detail.cancellable();
        if cancellable == 0 {
            self.show_toast(
                ToastKind::Info,
                format!(
                    "Every instance in '{}' has already finished.",
                    detail.root_name
                ),
                cx,
            );
            return;
        }
        self.workflow_confirm = Some(WorkflowCancelConfirm {
            root_id: detail.root_id,
            root_name: detail.root_name.clone(),
            cancellable,
        });
    }

    /// The root-cancel dialog, when one is open.
    pub fn workflow_confirm(&self) -> Option<&WorkflowCancelConfirm> {
        self.workflow_confirm.as_ref()
    }

    /// Dismiss the root-cancel dialog.
    pub fn cancel_workflow_confirm(&mut self) {
        self.workflow_confirm = None;
    }

    /// Confirm the root cancel: mark every non-terminal row locally, send the
    /// RPC, and reconcile (or roll back) when the response lands.
    pub fn accept_workflow_confirm(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(confirm) = self.workflow_confirm.take() else {
            return false;
        };
        let Some(detail) = self.workflow_detail.as_mut() else {
            return true;
        };
        if detail.root_id != confirm.root_id {
            return true;
        }
        let previous = detail.clone();
        if detail.begin_cancel() == 0 {
            return true;
        }
        self.workflow_cancel_previous = Some(previous.clone());
        match &self.runtime {
            Some(runtime) => runtime.cancel_workflow(confirm.root_id),
            None => {
                if let Some(detail) = self.workflow_detail.as_mut() {
                    if detail.root_id == confirm.root_id {
                        *detail = previous;
                    }
                }
                self.workflow_cancel_previous = None;
                self.show_toast(
                    ToastKind::Error,
                    "The transport is not running. The workflow was not cancelled.".to_string(),
                    cx,
                );
            }
        }
        true
    }

    /// Retry one failed instance (`workflow.retry`).
    ///
    /// The row moves to Ready with its optimistic marker immediately; the
    /// response and the next `task.updated` carry the real attempt.
    pub fn retry_workflow_instance(&mut self, task_id: uuid::Uuid, cx: &mut Context<Self>) {
        let Some(detail) = self.workflow_detail.as_mut() else {
            return;
        };
        let Some(previous) = detail.begin_retry(task_id) else {
            let message = detail
                .rows
                .iter()
                .find(|row| row.id == task_id)
                .map(|row| {
                    format!(
                        "Only a failed instance can be retried; '{}' is {}.",
                        row.name,
                        row.status_kind().label()
                    )
                })
                .unwrap_or_else(|| "Select a failed instance first.".to_string());
            self.show_toast(ToastKind::Info, message, cx);
            return;
        };
        self.workflow_retry_previous.insert(task_id, previous);
        match &self.runtime {
            Some(runtime) => runtime.retry_workflow_task(task_id),
            None => {
                if let Some(previous) = self.workflow_retry_previous.remove(&task_id) {
                    if let Some(detail) = self.workflow_detail.as_mut() {
                        detail.rollback_retry(previous);
                    }
                }
                self.show_toast(
                    ToastKind::Error,
                    "The transport is not running. The retry was not sent.".to_string(),
                    cx,
                );
            }
        }
    }

    /// Retry the highlighted instance (the `r` shortcut).
    pub fn retry_workflow_selected(&mut self, cx: &mut Context<Self>) {
        let Some(task_id) = self
            .workflow_detail
            .as_ref()
            .and_then(WorkflowDetail::selected_id)
        else {
            self.show_toast(
                ToastKind::Info,
                "Select a failed instance first.".to_string(),
                cx,
            );
            return;
        };
        self.retry_workflow_instance(task_id, cx);
    }

    /// Copy the catalog DOT source (the monospace pane has no text selection in
    /// this GPUI build).
    pub fn copy_workflow_dot(&mut self, cx: &mut Context<Self>) {
        if self.workflow_graph.dot.is_empty() {
            self.show_toast(
                ToastKind::Info,
                "The catalog DAG source has not loaded yet.".to_string(),
                cx,
            );
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(self.workflow_graph.dot.clone()));
        self.show_toast(
            ToastKind::Success,
            "Copied the catalog DAG source.".to_string(),
            cx,
        );
    }

    /// Ask the runtime for a fresh `catalog.list`. Like the task snapshot, an
    /// older list stays visible; only a data-less surface shows the skeleton.
    pub fn refresh_catalog(&mut self) {
        match &self.runtime {
            Some(runtime) => {
                if !self.catalog.has_entries()
                    && !matches!(self.catalog.status(), crate::catalog::CatalogStatus::Loaded)
                {
                    self.catalog.begin_load();
                }
                runtime.refresh_catalog();
            }
            None => self
                .catalog
                .fail("the transport is not running".to_string()),
        }
    }

    /// Fetch the selected definition's raw Markdown.
    fn load_catalog_preview(&mut self, name: &str) {
        self.catalog.begin_preview(name);
        match &self.runtime {
            Some(runtime) => runtime.load_catalog_entry(name.to_string()),
            None => self
                .catalog
                .fail_preview(name, "the transport is not running".to_string()),
        }
    }

    fn ensure_catalog_preview(&mut self) {
        if let Some(name) = self.catalog.preview_needed() {
            self.load_catalog_preview(&name);
        }
    }

    /// The Catalog model (groups, search, preview, launch sheets).
    pub fn catalog(&self) -> &CatalogState {
        &self.catalog
    }

    /// The configured agent roster, for the one-shot picker.
    pub fn agent_roster(&self) -> &[AgentCatalogEntry] {
        &self.agents
    }

    /// Highlight a catalog row and load its preview when it changed.
    pub fn select_catalog_entry(&mut self, name: &str) {
        self.catalog.select(name);
        self.ensure_catalog_preview();
    }

    /// Keyboard selection on the Catalog list (↑/↓).
    pub fn move_catalog_selection(&mut self, delta: isize) {
        self.catalog.move_selection(delta);
        self.ensure_catalog_preview();
    }

    /// Enter on the Catalog list opens the launch sheet for the selected
    /// definition. While a sheet is already open the key is a no-op, so typed
    /// var values can never be reset by Enter.
    pub fn open_catalog_selected(&mut self) {
        if self.catalog.any_form_open() {
            return;
        }
        self.ensure_catalog_preview();
        if let Some(name) = self.catalog.selected().map(str::to_string) {
            self.catalog.open_launch(&name);
        }
    }

    pub fn is_catalog_search_active(&self) -> bool {
        self.catalog.is_search_active()
    }

    /// Collapse or expand one Catalog namespace/folder.
    pub fn toggle_catalog_group(&mut self, key: &str) {
        self.catalog.toggle_collapsed(key);
    }

    pub fn activate_catalog_search(&mut self) {
        self.catalog.activate_search();
    }

    pub fn deactivate_catalog_search(&mut self) {
        self.catalog.deactivate_search();
    }

    /// Route one typed character to the launch sheet, the one-shot sheet or the
    /// search field, in that order (the sheets are modal).
    pub fn catalog_push_char(&mut self, character: char) {
        if self.catalog.launch().is_some() {
            self.catalog.push_launch_char(character);
        } else if self.catalog.oneshot().is_some() {
            self.catalog.push_oneshot_char(character);
        } else if self.catalog.is_search_active() {
            if let Some(name) = self.catalog.push_search_char(character) {
                self.load_catalog_preview(&name);
            }
        }
    }

    pub fn catalog_backspace(&mut self) {
        if self.catalog.launch().is_some() {
            self.catalog.backspace_launch();
        } else if self.catalog.oneshot().is_some() {
            self.catalog.backspace_oneshot();
        } else if self.catalog.is_search_active() {
            if let Some(name) = self.catalog.backspace_search() {
                self.load_catalog_preview(&name);
            }
        }
    }

    /// Enter inside a multiline var inserts a line break and returns true.
    pub fn catalog_push_newline(&mut self) -> bool {
        self.catalog.push_launch_newline()
    }

    /// Tab/Shift+Tab inside the active sheet.
    pub fn cycle_catalog_form_focus(&mut self, delta: isize) {
        if self.catalog.launch().is_some() {
            self.catalog.cycle_launch_focus(delta);
        } else if self.catalog.oneshot().is_some() {
            self.catalog.cycle_oneshot_focus();
        }
    }

    /// Clear the Catalog search. Returns whether anything changed.
    pub fn clear_catalog_search(&mut self) -> bool {
        let changed = self.catalog.clear_search();
        if changed {
            self.ensure_catalog_preview();
        }
        changed
    }

    /// The topmost Catalog overlay consumed Escape.
    pub fn close_catalog_overlay(&mut self) -> bool {
        self.catalog.close_overlay()
    }

    /// Open the launch sheet for one definition.
    pub fn open_catalog_launch(&mut self, name: &str, cx: &mut Context<Self>) -> bool {
        if self.catalog.open_launch(name) {
            true
        } else {
            self.show_toast(
                ToastKind::Info,
                "That definition is no longer in the catalog. Refresh and try again.".to_string(),
                cx,
            );
            false
        }
    }

    /// Focus one var in the launch sheet (a field click).
    pub fn focus_catalog_field(&mut self, index: usize) {
        if let Some(form) = self.catalog.launch_mut() {
            if index < form.fields.len() {
                form.focused = index;
            }
        }
    }

    /// Set a var value from a declared choice chip.
    pub fn set_catalog_field_value(&mut self, index: usize, value: String) {
        if let Some(form) = self.catalog.launch_mut() {
            if let Some(field) = form.fields.get_mut(index) {
                field.value = value;
                field.error = None;
                form.server_error = None;
                form.focused = index;
            }
        }
    }

    /// Retry a failed preview read.
    pub fn retry_catalog_preview(&mut self) {
        self.catalog.invalidate_preview();
        self.ensure_catalog_preview();
    }

    /// Copy the selected definition's Markdown source (the preview pane has no
    /// text selection in this GPUI build).
    pub fn copy_catalog_source(&mut self, cx: &mut Context<Self>) {
        let Some(markdown) = self
            .catalog
            .preview()
            .and_then(|preview| preview.markdown.clone())
        else {
            self.show_toast(
                ToastKind::Info,
                "Select a definition with a loaded source first.".to_string(),
                cx,
            );
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(markdown));
        self.show_toast(
            ToastKind::Success,
            "Copied the definition source.".to_string(),
            cx,
        );
    }

    /// Validate the launch sheet and send `tasks.start`. A validation failure
    /// stays inline; a transport failure is attached to the form too.
    pub fn submit_catalog_launch(&mut self) {
        // A declared agent the daemon does not offer is caught here as well as
        // on the server: the roster is the same `agents.list` the daemon uses,
        // so the sheet can explain the typo without a round trip.
        let missing_agent = self
            .catalog
            .launch()
            .and_then(|form| form.entry.agent.clone())
            .filter(|agent| {
                !self.agents.is_empty()
                    && !self
                        .agents
                        .iter()
                        .any(|choice| &choice.name == agent && choice.available)
            });
        if let Some(agent) = missing_agent {
            if let Some(form) = self.catalog.launch_mut() {
                form.submitting = false;
                form.server_error = Some(format!(
                    "Agent '{agent}' is not configured or not available on the daemon."
                ));
            }
            return;
        }
        let Some(form) = self.catalog.launch_mut() else {
            return;
        };
        if form.submitting {
            return;
        }
        if !form.validate() {
            return;
        }
        let Some(input) = form.input() else {
            return;
        };
        let name = form.entry.name.clone();
        form.submitting = true;
        form.server_error = None;
        match &self.runtime {
            Some(runtime) => runtime.start_catalog_task(name, input),
            None => {
                if let Some(form) = self.catalog.launch_mut() {
                    form.submitting = false;
                    form.server_error = Some(
                        "The transport is not running, so the task was not started.".to_string(),
                    );
                }
            }
        }
    }

    pub fn close_catalog_launch(&mut self) {
        self.catalog.close_launch();
    }

    /// Open the secondary one-shot sheet with the daemon's agent roster.
    pub fn open_oneshot(&mut self) {
        let agents = self
            .agents
            .iter()
            .map(|agent| AgentChoice {
                name: agent.name.clone(),
                display_name: if agent.display_name.is_empty() {
                    agent.name.clone()
                } else {
                    agent.display_name.clone()
                },
                available: agent.available,
                default: agent.default,
            })
            .collect();
        self.catalog.open_oneshot(agents);
    }

    pub fn close_oneshot(&mut self) {
        self.catalog.close_oneshot();
    }

    /// Send `tasks.start_oneshot` with the chosen agent and cwd.
    pub fn submit_oneshot(&mut self) {
        let Some(form) = self.catalog.oneshot_mut() else {
            return;
        };
        if form.submitting {
            return;
        }
        if form.agent.is_none() {
            form.server_error = Some("Choose an agent first.".to_string());
            return;
        }
        let unavailable = form
            .agent
            .as_ref()
            .and_then(|name| form.agents.iter().find(|choice| &choice.name == name))
            .is_some_and(|choice| !choice.available);
        if unavailable {
            form.server_error =
                Some("That agent is not available on the daemon; choose another one.".to_string());
            return;
        }
        let agent = form.agent.clone();
        let cwd = form.cwd.trim().to_string();
        form.submitting = true;
        form.server_error = None;
        match &self.runtime {
            Some(runtime) => runtime.start_oneshot(agent, (!cwd.is_empty()).then_some(cwd)),
            None => {
                if let Some(form) = self.catalog.oneshot_mut() {
                    form.submitting = false;
                    form.server_error =
                        Some("The transport is not running, so no session was opened.".to_string());
                }
            }
        }
    }

    /// Select the agent chip in the one-shot sheet.
    pub fn select_oneshot_agent(&mut self, name: &str) {
        self.catalog.set_oneshot_agent(name);
    }

    pub fn focus_oneshot(&mut self, focus: OneShotFocus) {
        self.catalog.set_oneshot_focus(focus);
    }

    /// ↑/↓ while the one-shot agent picker has focus.
    pub fn cycle_oneshot_agent(&mut self, delta: isize) {
        self.catalog.cycle_oneshot_agent(delta);
    }

    /// Refresh the active surface: its task rows on Inbox/Tasks, the catalog
    /// on Catalog, and always the ping (so the status bar and repo label stay
    /// honest).
    pub fn refresh_active(&mut self) {
        match self.active_route {
            Route::Inbox | Route::Tasks => self.refresh_snapshot(),
            Route::Catalog => self.refresh_catalog(),
            Route::Workflows => {
                self.refresh_snapshot();
                self.refresh_workflow_graph();
                if let Some(root_id) = self.workflow_root.or_else(|| self.selected_workflow_root())
                {
                    self.load_workflow_inspect(root_id);
                }
            }
            _ => {}
        }
        self.ping();
    }

    pub fn connection(&self) -> &ConnectionState {
        &self.connection
    }

    pub fn config(&self) -> &ClientConfig {
        &self.config
    }

    /// The active top-level surface.
    pub fn active_route(&self) -> Route {
        self.active_route
    }

    /// Switch surfaces. Changing routes always leaves a pushed detail page, so
    /// the sidebar never navigates "back" into a stale detail.
    pub fn navigate(&mut self, route: Route) {
        if self.active_route == Route::Tasks && route != Route::Tasks {
            // Leave search mode but keep the query and filter for the session
            // (docs §2, Working Memory).
            self.tasks_search_active = false;
        }
        if self.active_route == Route::Catalog && route != Route::Catalog {
            // A sheet belongs to the Catalog surface; leaving it must not leave
            // a hidden modal that still owns the keyboard.
            self.catalog.deactivate_search();
            self.catalog.close_launch();
            self.catalog.close_oneshot();
        }
        // A confirmation is about one row on one surface; leaving the surface
        // must not leave a hidden dialog targeting it.
        self.task_confirm = None;
        self.workflow_confirm = None;
        self.active_route = route;
        self.detail_open = false;
        if route == Route::Tasks {
            // The first visit catches up even if the connect-time load was
            // missed; later visits keep the current rows until the new
            // snapshot lands.
            self.refresh_snapshot();
        } else if route == Route::Catalog {
            self.refresh_catalog();
        } else if route == Route::Workflows {
            // The root list is derived from the shared snapshot; a first visit
            // fetches it and then selects the newest root.
            self.refresh_snapshot();
            self.ensure_workflow_root();
            self.refresh_workflow_graph();
        }
    }

    /// Whether the narrow layout is showing the pushed detail page.
    pub fn is_detail_open(&self) -> bool {
        self.detail_open
    }

    /// Push the detail page on a narrow window.
    pub fn open_detail(&mut self) {
        self.detail_open = true;
    }

    /// Leave the pushed detail page (Escape/back).
    pub fn close_detail(&mut self) {
        self.detail_open = false;
    }

    /// The active repo name shown in the top bar. Until the first successful
    /// `system.ping` the daemon has not told us where it runs.
    pub fn repo_label(&self) -> &str {
        self.repo.as_deref().unwrap_or("—")
    }

    /// The current Needs you + Recent groups, derived from the latest snapshot.
    pub fn inbox_rows(&self) -> InboxRows {
        inbox::build(&self.tasks, &self.sessions, Utc::now())
    }

    /// First-fetch skeleton / error banner state.
    pub fn snapshot_status(&self) -> &SnapshotStatus {
        &self.snapshot_status
    }

    /// Whether any snapshot has produced rows (the error banner keeps them).
    pub fn has_snapshot_data(&self) -> bool {
        !self.tasks.is_empty() || !self.sessions.is_empty()
    }

    /// Blocked/failed runs waiting for a decision (the Inbox badge).
    pub fn needs_count(&self) -> u32 {
        self.inbox_rows().needs_count()
    }

    /// The highlighted row, falling back to the first row so `Enter` works
    /// immediately (the primary action is one keystroke away). The selection
    /// covers Needs you and Recent so `↑`/`↓` are useful on a quiet day too.
    pub fn selected_inbox_key(&self, rows: &InboxRows) -> Option<InboxKey> {
        let keys = rows.keys();
        if let Some(selected) = &self.inbox_selected {
            if keys.contains(selected) {
                return Some(selected.clone());
            }
        }
        keys.into_iter().next()
    }

    /// Move the row highlight by one, clamped (lists do not wrap).
    pub fn move_inbox_selection(&mut self, delta: isize) {
        let rows = self.inbox_rows();
        let keys = rows.keys();
        if keys.is_empty() {
            self.inbox_selected = None;
            return;
        }
        let index = self
            .selected_inbox_key(&rows)
            .and_then(|key| keys.iter().position(|candidate| candidate == &key))
            .unwrap_or(0);
        let last = keys.len() as isize - 1;
        let next = (index as isize + delta).clamp(0, last) as usize;
        self.inbox_selected = Some(keys[next].clone());
    }

    /// Highlight a row without opening it (a click highlights then opens).
    pub fn select_inbox_row(&mut self, key: InboxKey) {
        self.inbox_selected = Some(key);
    }

    /// Open the highlighted row's task detail (step 06 renders its content).
    /// Session-only rows have no task to open.
    pub fn open_inbox_selected(&mut self) -> Option<uuid::Uuid> {
        let rows = self.inbox_rows();
        let task_id = self.selected_inbox_key(&rows)?.task_id()?;
        self.open_task(task_id);
        Some(task_id)
    }

    /// The Tasks rows for the active filter and query.
    pub fn task_list(&self) -> TaskList {
        tasks::build(
            &self.tasks,
            &self.sessions,
            &self.tasks_query,
            self.tasks_filter,
            Utc::now(),
        )
    }

    /// The active Tasks segment.
    pub fn tasks_filter(&self) -> TaskFilter {
        self.tasks_filter
    }

    /// Select a segment (a click or a `[`/`]` step).
    pub fn set_tasks_filter(&mut self, filter: TaskFilter) {
        self.tasks_filter = filter;
    }

    /// Step through the segmented control, wrapping at both ends.
    pub fn cycle_tasks_filter(&mut self, delta: isize) {
        self.tasks_filter = self.tasks_filter.cycle(delta);
    }

    /// The current fuzzy query.
    pub fn tasks_query(&self) -> &str {
        &self.tasks_query
    }

    /// True while typed characters go to the search field.
    pub fn is_tasks_search_active(&self) -> bool {
        self.tasks_search_active
    }

    /// `/` focuses the search field (the root keeps keyboard focus, so the
    /// list shortcuts stay live; the flag only routes typed characters).
    pub fn activate_tasks_search(&mut self) {
        self.tasks_search_active = true;
    }

    /// Leave search mode without clearing the query, so the filter and the
    /// typed text survive a row click (Working Memory).
    pub fn deactivate_tasks_search(&mut self) {
        self.tasks_search_active = false;
    }

    /// Append one typed character to the query.
    pub fn push_tasks_query_char(&mut self, character: char) {
        if character.is_control() {
            return;
        }
        let mut query = self.tasks_query.clone();
        query.push(character);
        self.set_tasks_query(query);
    }

    /// Remove the last query character.
    pub fn backspace_tasks_query(&mut self) {
        let mut query = self.tasks_query.clone();
        query.pop();
        self.set_tasks_query(query);
    }

    /// Clear the query and leave search mode. Returns whether anything changed,
    /// so `Esc` only falls through to closing the detail when the field is
    /// already empty.
    pub fn clear_tasks_search(&mut self) -> bool {
        let changed = self.tasks_search_active || !self.tasks_query.is_empty();
        self.tasks_search_active = false;
        self.set_tasks_query(String::new());
        changed
    }

    /// The filtered-empty state's "clear search and filters" action.
    pub fn reset_tasks_filters(&mut self) {
        self.tasks_search_active = false;
        self.tasks_filter = TaskFilter::All;
        self.set_tasks_query(String::new());
    }

    fn set_tasks_query(&mut self, query: String) {
        self.tasks_query = query;
        self.tasks_selected_index = 0;
    }

    /// The highlighted Tasks row, falling back to the first visible row so
    /// `Enter` works immediately.
    pub fn selected_tasks_id(&self) -> Option<uuid::Uuid> {
        self.task_list()
            .selected_id(self.tasks_selected, self.tasks_selected_index)
    }

    /// Highlight a row without opening it (a click selects then opens).
    pub fn select_tasks_row(&mut self, task_id: uuid::Uuid) {
        self.tasks_selected = Some(task_id);
        if let Some(index) = self.task_list().position(task_id) {
            self.tasks_selected_index = index;
        }
    }

    /// Move the Tasks highlight by one, clamped (lists do not wrap).
    pub fn move_tasks_selection(&mut self, delta: isize) {
        self.set_tasks_selection(|list, current| {
            let last = list.rows.len() as isize - 1;
            (current as isize + delta).clamp(0, last) as usize
        });
    }

    /// Home on the Tasks list.
    pub fn tasks_home(&mut self) {
        self.set_tasks_selection(|_, _| 0);
    }

    /// End on the Tasks list.
    pub fn tasks_end(&mut self) {
        self.set_tasks_selection(|list, _| list.rows.len().saturating_sub(1));
    }

    /// PageUp/PageDown on the Tasks list.
    pub fn tasks_page(&mut self, direction: isize) {
        self.set_tasks_selection(|list, current| {
            let last = list.rows.len() as isize - 1;
            let delta = direction * TASKS_PAGE_ROWS as isize;
            (current as isize + delta).clamp(0, last) as usize
        });
    }

    fn set_tasks_selection(&mut self, next_index: impl FnOnce(&TaskList, usize) -> usize) {
        let list = self.task_list();
        let Some(current) = list.resolve_index(self.tasks_selected, self.tasks_selected_index)
        else {
            self.tasks_selected = None;
            self.tasks_selected_index = 0;
            return;
        };
        let next = next_index(&list, current).min(list.rows.len() - 1);
        self.tasks_selected = Some(list.rows[next].id);
        self.tasks_selected_index = next;
    }

    /// Open the highlighted Tasks row (step 06 renders its detail).
    pub fn open_tasks_selected(&mut self) -> Option<uuid::Uuid> {
        let task_id = self.selected_tasks_id()?;
        self.deactivate_tasks_search();
        self.open_task(task_id);
        Some(task_id)
    }

    /// Open the cancel/retry confirmation for the highlighted row. Actions that
    /// cannot apply are explained instead of silently doing nothing.
    pub fn request_task_action(&mut self, action: TaskAction, cx: &mut Context<Self>) {
        let list = self.task_list();
        let Some(task_id) = list.selected_id(self.tasks_selected, self.tasks_selected_index) else {
            self.show_toast(ToastKind::Info, "Select a run first.".to_string(), cx);
            return;
        };
        let Some(row) = list.rows.iter().find(|row| row.id == task_id) else {
            return;
        };
        let allowed = match action {
            TaskAction::Cancel => tasks::can_cancel(row.status),
            TaskAction::Retry => tasks::can_retry(row.status),
        };
        if !allowed {
            let message = match action {
                TaskAction::Cancel => {
                    format!("'{}' has already finished. Press r to retry it.", row.name)
                }
                TaskAction::Retry => {
                    format!("'{}' is still active. Cancel it before retrying.", row.name)
                }
            };
            self.show_toast(ToastKind::Info, message, cx);
            return;
        }
        self.task_confirm = Some(TaskConfirm {
            action,
            task_id,
            task_name: row.name.clone(),
        });
    }

    /// The confirmation dialog state, when one is open.
    pub fn task_confirm(&self) -> Option<&TaskConfirm> {
        self.task_confirm.as_ref()
    }

    /// Dismiss the confirmation (Esc / the secondary button).
    pub fn cancel_task_confirm(&mut self) {
        self.task_confirm = None;
    }

    /// Confirm the pending action: apply it locally at once (Doherty
    /// Threshold), send the RPC, and let [`ClientState::apply`] reconcile or
    /// roll back when the result lands.
    pub fn accept_task_confirm(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(confirm) = self.task_confirm.take() else {
            return false;
        };
        let Some(previous) = self
            .tasks
            .iter()
            .find(|task| task.id == confirm.task_id)
            .cloned()
        else {
            self.show_toast(
                ToastKind::Error,
                "That run is no longer in the list.".to_string(),
                cx,
            );
            return true;
        };

        let mut optimistic = previous.clone();
        match confirm.action {
            TaskAction::Cancel => {
                optimistic.status = TaskStatus::Cancelled;
                optimistic.finished_at = Some(Utc::now());
            }
            TaskAction::Retry => {
                optimistic.status = TaskStatus::Pending;
                optimistic.started_at = None;
                optimistic.finished_at = None;
                optimistic.error = None;
                optimistic.failure = None;
            }
        }
        let optimistic_snapshot = optimistic.clone();
        self.upsert_task(optimistic);
        if let Some(detail) = self.detail.as_mut() {
            if detail.task_id == optimistic_snapshot.id {
                detail.task = Some(optimistic_snapshot.clone());
                detail.task_loading = false;
                detail.task_error = None;
                if confirm.action == TaskAction::Retry {
                    // A retry starts a new attempt: the prior feed and attempt
                    // rows belong to the old run, so clear them until the
                    // reconciling reload lands.
                    detail.events.clear();
                    detail.events_cursor = 0;
                    detail.events_loaded = false;
                    detail.events_ignored = 0;
                    detail.new_events = 0;
                    detail.runs.clear();
                    detail.runs_loaded = false;
                    detail.answered_request = None;
                    detail.reply_pending = None;
                }
            }
        }

        if let Some(runtime) = &self.runtime {
            runtime.task_action(confirm.action, previous, optimistic_snapshot);
        } else {
            self.rollback_task(&previous, &optimistic_snapshot);
            if let Some(detail) = self.detail.as_mut() {
                if detail.task_id == previous.id {
                    detail.task = Some(previous);
                }
            }
            self.show_toast(
                ToastKind::Error,
                "The transport is not running. The change was rolled back.".to_string(),
                cx,
            );
        }
        true
    }

    /// The current toast, if one is visible.
    pub fn toast(&self) -> Option<&Toast> {
        self.toast.as_ref()
    }

    /// Dismiss the toast early (the × button).
    pub fn dismiss_toast(&mut self) {
        self.toast = None;
    }

    /// Show a message and schedule its self-dismissal. A newer toast cancels
    /// the older timer by id.
    fn show_toast(&mut self, kind: ToastKind, message: String, cx: &mut Context<Self>) {
        self.toast_id = self.toast_id.wrapping_add(1);
        let id = self.toast_id;
        self.toast = Some(Toast { kind, message, id });
        cx.spawn(async move |weak, cx| {
            cx.background_executor().timer(TOAST_LINGER).await;
            let _ = weak.update(cx, |this: &mut Self, cx| {
                if this.toast.as_ref().is_some_and(|toast| toast.id == id) {
                    this.toast = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Undo the optimistic write, but never overwrite a newer push: the row is
    /// restored only while it is still exactly the local snapshot.
    fn rollback_task(&mut self, previous: &Task, optimistic: &Task) {
        if let Some(current) = self.tasks.iter_mut().find(|task| task.id == previous.id) {
            if current == optimistic {
                *current = previous.clone();
            }
        }
    }

    /// Select and push the detail page for a specific task (a Recent row click).
    ///
    /// Opening the same run again refreshes it without discarding the feed; a
    /// different run starts a fresh detail so stale output can never appear
    /// under another task's header.
    pub fn open_task(&mut self, task_id: uuid::Uuid) {
        let same = self
            .detail
            .as_ref()
            .is_some_and(|detail| detail.task_id == task_id);
        if same {
            if let Some(detail) = &mut self.detail {
                detail.reload();
            }
        } else {
            self.detail = Some(TaskDetail::new(task_id));
        }
        self.selected_task = Some(task_id);
        self.detail_open = true;
        self.load_detail(task_id);
    }

    /// Fetch every part of one task's detail (initial open, terminal transition
    /// and post-action reconciliation).
    fn load_detail(&mut self, task_id: uuid::Uuid) {
        if let Some(detail) = self.detail.as_mut() {
            if detail.task_id == task_id {
                // Keep loaded rows visible; this only marks the reads in
                // flight and clears the previous errors.
                detail.reload();
            }
        }
        match &self.runtime {
            Some(runtime) => runtime.load_task_detail(task_id),
            None => {
                if let Some(detail) = self.detail.as_mut() {
                    if detail.task_id == task_id {
                        let message = "the transport is not running".to_string();
                        detail.fail_task(message.clone());
                        detail.fail_events(message.clone());
                        detail.fail_runs(message);
                    }
                }
            }
        }
    }

    /// The open detail, when a run is selected.
    pub fn detail(&self) -> Option<&TaskDetail> {
        self.detail.as_ref()
    }

    /// The active detail tab.
    pub fn detail_tab(&self) -> DetailTab {
        self.detail
            .as_ref()
            .map(|detail| detail.tab)
            .unwrap_or_default()
    }

    /// Switch tabs. Selecting Events while following consumes the unseen count.
    pub fn set_detail_tab(&mut self, tab: DetailTab) {
        if let Some(detail) = self.detail.as_mut() {
            detail.tab = tab;
            if tab == DetailTab::Events && detail.follow {
                detail.new_events = 0;
            }
        }
    }

    /// Whether the detail viewport follows new content.
    pub fn detail_follow(&self) -> bool {
        self.detail
            .as_ref()
            .map(|detail| detail.follow)
            .unwrap_or(true)
    }

    /// Pause or resume tailing. Resuming clears the unseen counter.
    pub fn set_detail_follow(&mut self, follow: bool) {
        if let Some(detail) = self.detail.as_mut() {
            detail.follow = follow;
            if follow {
                detail.new_events = 0;
            }
        }
    }

    /// Events that arrived while the detail was pinned off the tail.
    pub fn detail_new_events(&self) -> u64 {
        self.detail
            .as_ref()
            .map(|detail| detail.new_events)
            .unwrap_or(0)
    }

    /// The prompt the permission banner should render, when one is open.
    pub fn detail_pending_request(&self) -> Option<PendingRequest> {
        self.detail.as_ref()?.pending_request(&self.sessions)
    }

    /// The agent that owns the selected run, from the live session or the most
    /// recent attempt (the task row itself carries no agent).
    pub fn detail_agent(&self) -> Option<String> {
        let task_id = self.selected_task?;
        let task_id = task_id.to_string();
        self.sessions
            .iter()
            .find(|session| session.task_id.as_deref() == Some(task_id.as_str()))
            .map(|session| session.agent.clone())
            .or_else(|| {
                self.detail
                    .as_ref()
                    .and_then(|detail| detail.runs.iter().rev().find_map(|run| run.agent.clone()))
            })
    }

    /// Token/cost usage for the selected run: the live session wins while it is
    /// running, otherwise the most recent attempt that reported usage.
    pub fn detail_usage(&self) -> Option<AgentUsage> {
        let task_id = self.selected_task?;
        let task_id = task_id.to_string();
        let live = self
            .sessions
            .iter()
            .find(|session| session.task_id.as_deref() == Some(task_id.as_str()))
            .and_then(|session| session.usage.clone())
            .filter(|usage| !usage.is_empty());
        live.or_else(|| {
            self.detail
                .as_ref()
                .and_then(|detail| detail.runs.iter().rev().find_map(|run| run.usage.clone()))
        })
    }

    /// The agent's own session id, from whichever source has it.
    pub fn detail_session_id(&self) -> Option<String> {
        let detail = self.detail.as_ref()?;
        detail
            .task
            .as_ref()
            .and_then(|task| task.session_id.clone())
            .or_else(|| {
                detail
                    .runs
                    .iter()
                    .rev()
                    .find_map(|run| run.session_id.clone())
            })
            .or_else(|| detail.events_session.clone())
            .or_else(|| {
                let task_id = detail.task_id.to_string();
                self.sessions
                    .iter()
                    .find(|session| session.task_id.as_deref() == Some(task_id.as_str()))
                    .and_then(|session| session.session_id.clone())
            })
            .or_else(|| {
                self.selected_task()
                    .and_then(|task| task.session_id.clone())
            })
    }

    /// Retry the persisted-output fetch for the open detail.
    pub fn retry_detail_task(&mut self) {
        let Some(task_id) = self.detail.as_ref().map(|detail| detail.task_id) else {
            return;
        };
        if let Some(detail) = self.detail.as_mut() {
            detail.task_loading = true;
            detail.task_error = None;
        }
        match &self.runtime {
            Some(runtime) => runtime.load_task(task_id),
            None => {
                if let Some(detail) = self.detail.as_mut() {
                    detail.fail_task("the transport is not running".to_string());
                }
            }
        }
    }

    /// Retry the structured-event backfill for the open detail.
    pub fn retry_detail_events(&mut self) {
        let Some(task_id) = self.detail.as_ref().map(|detail| detail.task_id) else {
            return;
        };
        if let Some(detail) = self.detail.as_mut() {
            detail.events_loading = true;
            detail.events_error = None;
        }
        match &self.runtime {
            Some(runtime) => runtime.load_task_events(task_id),
            None => {
                if let Some(detail) = self.detail.as_mut() {
                    detail.fail_events("the transport is not running".to_string());
                }
            }
        }
    }

    /// Retry the attempt-table fetch for the open detail.
    pub fn retry_detail_runs(&mut self) {
        let Some(task_id) = self.detail.as_ref().map(|detail| detail.task_id) else {
            return;
        };
        if let Some(detail) = self.detail.as_mut() {
            detail.runs_loading = true;
            detail.runs_error = None;
        }
        match &self.runtime {
            Some(runtime) => runtime.load_task_runs(task_id),
            None => {
                if let Some(detail) = self.detail.as_mut() {
                    detail.fail_runs("the transport is not running".to_string());
                }
            }
        }
    }

    /// Answer the open permission prompt. The request id is correlated exactly;
    /// a screen-detected prompt has none and is explained instead of guessed.
    pub fn reply_to_pending(
        &mut self,
        request: &PendingRequest,
        reply: InputReply,
        cx: &mut Context<Self>,
    ) {
        let Some(request_id) = request.request_id.clone() else {
            self.show_toast(
                ToastKind::Info,
                "This prompt was detected on the agent's screen and cannot be answered here. \
                 Answer it in the agent's terminal."
                    .to_string(),
                cx,
            );
            return;
        };
        if request.session_id.is_empty() {
            self.show_toast(
                ToastKind::Info,
                "The daemon has not reported which session owns this prompt yet.".to_string(),
                cx,
            );
            return;
        }
        let already_pending = self
            .detail
            .as_ref()
            .is_some_and(|detail| detail.reply_pending.is_some());
        if already_pending {
            return;
        }
        if let Some(detail) = self.detail.as_mut() {
            detail.reply_pending = Some(request_id.clone());
        }
        match &self.runtime {
            Some(runtime) => runtime.reply(request.session_id.clone(), request_id, reply),
            None => {
                if let Some(detail) = self.detail.as_mut() {
                    detail.reply_pending = None;
                }
                self.show_toast(
                    ToastKind::Error,
                    "The transport is not running, so the reply was not sent.".to_string(),
                    cx,
                );
            }
        }
    }

    /// Copy the persisted output to the clipboard (the plain-text pane has no
    /// text selection in this GPUI build, so Copy is the accessible path).
    pub fn copy_detail_output(&mut self, cx: &mut Context<Self>) {
        let Some(output) = self.detail.as_ref().and_then(TaskDetail::output) else {
            self.show_toast(
                ToastKind::Info,
                "This run has no persisted output yet.".to_string(),
                cx,
            );
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(output.text));
        self.show_toast(
            ToastKind::Success,
            "Copied the run output to the clipboard.".to_string(),
            cx,
        );
    }

    /// Copy the agent's session id (the header's copyable identity).
    pub fn copy_session_id(&mut self, session_id: String, cx: &mut Context<Self>) {
        if session_id.is_empty() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(session_id));
        self.show_toast(ToastKind::Success, "Copied the session id.".to_string(), cx);
    }

    /// Cancel/retry for the open detail, with the same confirmation and
    /// validation as the list rows (docs §2, Von Restorff: one danger control).
    pub fn request_detail_action(&mut self, action: TaskAction, cx: &mut Context<Self>) {
        let Some(task) = self.selected_task().cloned() else {
            self.show_toast(ToastKind::Info, "Select a run first.".to_string(), cx);
            return;
        };
        let allowed = match action {
            TaskAction::Cancel => tasks::can_cancel(task.status),
            TaskAction::Retry => tasks::can_retry(task.status),
        };
        if !allowed {
            let name = inbox::task_display_name(&task.name);
            let message = match action {
                TaskAction::Cancel => {
                    format!("'{name}' has already finished. Retry it instead.")
                }
                TaskAction::Retry => {
                    format!("'{name}' is still active. Cancel it before retrying.")
                }
            };
            self.show_toast(ToastKind::Info, message, cx);
            return;
        }
        self.task_confirm = Some(TaskConfirm {
            action,
            task_id: task.id,
            task_name: inbox::task_display_name(&task.name).to_string(),
        });
    }

    /// Whether a live agent event belongs to the open detail. The daemon marks
    /// pushes with `task_id`; older pushes only carry the session.
    fn event_belongs_to_open_task(&self, event: &AgentEventPush) -> bool {
        let Some(detail) = &self.detail else {
            return false;
        };
        if event
            .task_id
            .as_deref()
            .and_then(|id| uuid::Uuid::parse_str(id).ok())
            == Some(detail.task_id)
        {
            return true;
        }
        if detail.events_session.as_deref() == Some(event.session_id.as_str()) {
            return true;
        }
        self.sessions.iter().any(|session| {
            session.id == event.session_id
                && session
                    .task_id
                    .as_deref()
                    .and_then(|id| uuid::Uuid::parse_str(id).ok())
                    == Some(detail.task_id)
        })
    }

    /// The task opened from the Inbox, when there is one.
    pub fn selected_task_id(&self) -> Option<uuid::Uuid> {
        self.selected_task
    }

    /// The selected task row, for the detail header. The full `tasks.get` copy
    /// wins over the summary in the list because it carries the output blob.
    pub fn selected_task(&self) -> Option<&Task> {
        let id = self.selected_task?;
        if let Some(detail) = &self.detail {
            if detail.task_id == id {
                if let Some(task) = &detail.task {
                    return Some(task);
                }
            }
        }
        self.tasks.iter().find(|task| task.id == id)
    }

    /// Fold a `task.updated` push into the cached list immediately, before the
    /// reconciling `tasks.list` round trip lands (Doherty Threshold).
    fn upsert_task(&mut self, task: Task) {
        if let Some(existing) = self
            .tasks
            .iter_mut()
            .find(|existing| existing.id == task.id)
        {
            *existing = task;
        } else {
            self.tasks.push(task);
        }
    }

    pub fn last_ping(&self) -> Option<&PingSnapshot> {
        self.last_ping.as_ref()
    }

    /// A status-line summary of the last ping round trip.
    pub fn ping_label(&self) -> String {
        let Some(snapshot) = &self.last_ping else {
            return "ping: waiting…".to_string();
        };
        let latency = snapshot.round_trip.as_millis();
        match &snapshot.result {
            Ok(result) => format!("ping ok · {} · {latency} ms", result.cwd),
            Err(err) => format!("ping failed: {err}"),
        }
    }

    pub fn cursor(&self) -> Option<i64> {
        self.cursor
    }

    pub fn push_count(&self) -> u64 {
        self.push_count
    }

    pub fn last_push(&self) -> Option<&str> {
        self.last_push.as_deref()
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }
}

/// An id suffix for the rare placeholder whose root task is outside the
/// snapshot window: short enough for a heading, unique enough to disambiguate.
fn short_id(id: uuid::Uuid) -> String {
    let value = id.to_string();
    value.chars().take(8).collect()
}

/// The repo name the top bar shows: the final component of the daemon's cwd.
fn repo_name(cwd: &str) -> Option<String> {
    Path::new(cwd)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

/// The success toast for one answered prompt, in the user's terms.
fn reply_done_message(reply: &InputReply) -> String {
    match reply {
        InputReply::Once => "Allowed the agent's request once.".to_string(),
        InputReply::Always => "Allowed the agent's request and remembered it.".to_string(),
        InputReply::Reject => "Rejected the agent's request.".to_string(),
        InputReply::Value { .. } => "Sent the requested value to the agent.".to_string(),
        InputReply::Confirmed { confirmed: true } => "Confirmed the agent's request.".to_string(),
        InputReply::Confirmed { confirmed: false } => "Declined the agent's request.".to_string(),
        InputReply::Cancelled => "Dismissed the agent's request.".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_labels_name_the_state_and_the_fix() {
        assert_eq!(ConnectionState::Connecting.label(), "Connecting…");
        assert_eq!(ConnectionState::Connected.label(), "Connected");
        assert!(ConnectionState::Reconnecting {
            attempt: 3,
            delay_ms: 4_000
        }
        .label()
        .contains("attempt 3"));
        assert!(ConnectionState::Unauthorized {
            token_path: "/home/u/.local/share/favetto/token".to_string()
        }
        .label()
        .contains("/home/u/.local/share/favetto/token"));
        assert!(ConnectionState::Disconnected {
            message: "offline".to_string()
        }
        .label()
        .contains("offline"));

        assert!(ConnectionState::Connected.is_connected());
        assert!(!ConnectionState::Connecting.is_connected());
    }

    #[test]
    fn repo_name_is_the_daemon_cwd_basename() {
        assert_eq!(repo_name("/code/favetto"), Some("favetto".to_string()));
        assert_eq!(repo_name("/code/favetto/"), Some("favetto".to_string()));
        assert_eq!(repo_name("/"), None);
        assert_eq!(repo_name(""), None);
    }

    #[test]
    fn the_confirmation_names_the_task_and_its_consequence() {
        let cancel = TaskConfirm {
            action: TaskAction::Cancel,
            task_id: uuid::Uuid::from_u128(1),
            task_name: "triage".to_string(),
        };
        assert_eq!(cancel.title(), "Cancel task");
        assert_eq!(cancel.confirm_label(), "Cancel task");
        assert!(cancel.danger());
        assert!(cancel.body().contains("'triage'"));
        assert!(cancel.body().contains("agent process"));

        let retry = TaskConfirm {
            action: TaskAction::Retry,
            task_id: uuid::Uuid::from_u128(2),
            task_name: "release".to_string(),
        };
        assert_eq!(retry.title(), "Retry task");
        assert!(!retry.danger());
        assert!(retry.body().contains("'release'"));
        assert!(retry.body().contains("prior run history"));
    }
}
