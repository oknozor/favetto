//! The client's GPUI entity: connection state, routing, settings and the last
//! ping.
//!
//! Only one [`ClientState`] exists (created by the root view). It owns the
//! [`Runtime`] and a foreground task that drains runtime updates; every UI
//! observation is a read of this entity, so there are no global statics
//! (docs/design/gpui-client.md §4).
//!
//! Step 03 adds the active route and the pushed-detail flag that drive the
//! shell. Steps 04+ add tasks, sessions, the catalog and per-task event feeds
//! here.

#![allow(dead_code)] // Accessors and `ping()` are consumed by the views in steps 03+.

use std::path::Path;
use std::time::Duration;

use chrono::Utc;
use favetto_wire::model::{AgentSessionInfo, Task, TaskStatus};
use gpui::{Context, Task as GpuiTask};

use crate::config::{self, ClientConfig};
use crate::inbox::{self, InboxKey, InboxRows};
use crate::routes::Route;
use crate::runtime::{PingSnapshot, Runtime, Update};
use crate::sse::Push;
use crate::tasks::{self, TaskAction, TaskFilter, TaskList};
use crate::transport::ClientError;

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

        Self {
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
            snapshot_status,
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
            repo: None,
            runtime,
            drain,
        }
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
                        self.upsert_task(task.clone());
                        self.refresh_snapshot();
                    }
                    // A blocked or resolved prompt changes the agent roster,
                    // which only `agents.list` can reconcile.
                    Push::AgentState(_) | Push::AgentEvent(_) => self.refresh_snapshot(),
                    Push::Event(_)
                    | Push::CatalogUpdated
                    | Push::AgentExit(_)
                    | Push::Unknown(_) => {}
                }
            }
            Update::Cursor(cursor) => self.cursor = cursor,
            Update::Malformed(message) => self.last_error = Some(message),
            Update::SnapshotLoaded { tasks, sessions } => {
                self.tasks = tasks;
                self.sessions = sessions;
                self.snapshot_status = SnapshotStatus::Loaded;
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

    /// Refresh the active surface: its task rows on Inbox/Tasks, and always the
    /// ping (so the status bar and repo label stay honest).
    pub fn refresh_active(&mut self) {
        if matches!(self.active_route, Route::Inbox | Route::Tasks) {
            self.refresh_snapshot();
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
        // A confirmation is about one row on one surface; leaving the surface
        // must not leave a hidden dialog targeting it.
        self.task_confirm = None;
        self.active_route = route;
        self.detail_open = false;
        if route == Route::Tasks {
            // The first visit catches up even if the connect-time load was
            // missed; later visits keep the current rows until the new
            // snapshot lands.
            self.refresh_snapshot();
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

        if let Some(runtime) = &self.runtime {
            runtime.task_action(confirm.action, previous, optimistic_snapshot);
        } else {
            self.rollback_task(&previous, &optimistic_snapshot);
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
    pub fn open_task(&mut self, task_id: uuid::Uuid) {
        self.selected_task = Some(task_id);
        self.detail_open = true;
    }

    /// The task opened from the Inbox, when there is one.
    pub fn selected_task_id(&self) -> Option<uuid::Uuid> {
        self.selected_task
    }

    /// The selected task row, for the detail placeholder.
    pub fn selected_task(&self) -> Option<&Task> {
        let id = self.selected_task?;
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

/// The repo name the top bar shows: the final component of the daemon's cwd.
fn repo_name(cwd: &str) -> Option<String> {
    Path::new(cwd)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_string)
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
