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

use chrono::Utc;
use favetto_wire::model::{AgentSessionInfo, Task};
use gpui::{Context, Task as GpuiTask};

use crate::config::{self, ClientConfig};
use crate::inbox::{self, InboxKey, InboxRows};
use crate::routes::Route;
use crate::runtime::{PingSnapshot, Runtime, Update};
use crate::sse::Push;
use crate::transport::ClientError;

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

/// The first-fetch state of the Inbox, rendered as a skeleton or an error.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum InboxStatus {
    /// The first (`or a retried`) fetch is in flight and there are no rows yet.
    #[default]
    Loading,
    /// A snapshot has been folded in.
    Loaded,
    /// The last fetch failed; rows are kept when an older snapshot exists.
    Failed { message: String },
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
    /// First-fetch skeleton / error state for the Inbox.
    inbox_status: InboxStatus,
    /// The highlighted Needs you row, preserved by identity across refreshes.
    inbox_selected: Option<InboxKey>,
    /// The run opened from the Inbox (step 06 renders its detail).
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
        let mut inbox_status = InboxStatus::Loading;

        let (runtime, connection, drain) = match Runtime::spawn(config.clone()) {
            Ok(runtime) => {
                let updates = runtime.updates();
                let drain = cx.spawn(async move |weak, cx| {
                    while let Ok(update) = updates.recv().await {
                        let alive = weak
                            .update(cx, |this: &mut Self, cx| {
                                this.apply(update);
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
                inbox_status = InboxStatus::Failed {
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
            inbox_status,
            inbox_selected: None,
            selected_task: None,
            repo: None,
            runtime,
            drain,
        }
    }

    /// Fold one runtime update into the entity.
    fn apply(&mut self, update: Update) {
        match update {
            Update::Connection(connection) => {
                let became_connected = connection.is_connected() && !self.connection.is_connected();
                self.connection = connection;
                // Re-validate the RPC path and load the Inbox whenever the
                // stream (re)connects, so a recovered daemon also fills the
                // landing surface (docs §2, Doherty: no manual refresh).
                if became_connected {
                    self.ping();
                    self.refresh_inbox();
                } else if !self.connection.is_connected()
                    && !self.has_inbox_data()
                    && matches!(
                        &self.connection,
                        ConnectionState::Unauthorized { .. } | ConnectionState::Disconnected { .. }
                    )
                {
                    // There may never be a Connected transition to fill the
                    // first fetch; show the fix and a Retry instead of a
                    // skeleton that never resolves.
                    self.inbox_status = InboxStatus::Failed {
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
                        self.refresh_inbox();
                    }
                    // A blocked or resolved prompt changes the agent roster,
                    // which only `agents.list` can reconcile.
                    Push::AgentState(_) | Push::AgentEvent(_) => self.refresh_inbox(),
                    Push::Event(_)
                    | Push::CatalogUpdated
                    | Push::AgentExit(_)
                    | Push::Unknown(_) => {}
                }
            }
            Update::Cursor(cursor) => self.cursor = cursor,
            Update::Malformed(message) => self.last_error = Some(message),
            Update::InboxLoaded { tasks, sessions } => {
                self.tasks = tasks;
                self.sessions = sessions;
                self.inbox_status = InboxStatus::Loaded;
            }
            Update::InboxFailed { error } => {
                if error.is_unauthorized() {
                    self.connection = ConnectionState::Unauthorized {
                        token_path: self.config.token_source.label(),
                    };
                }
                self.inbox_status = InboxStatus::Failed {
                    message: error.to_string(),
                };
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

    /// Ask for a fresh Inbox snapshot (`r`, the error Retry, and every push).
    ///
    /// Without rows the surface shows its first-fetch skeleton; with rows the
    /// old snapshot stays visible until the new one lands. The runtime coalesces
    /// overlapping requests into one RPC round trip.
    pub fn refresh_inbox(&mut self) {
        match &self.runtime {
            Some(runtime) => {
                if self.tasks.is_empty() && self.sessions.is_empty() {
                    self.inbox_status = InboxStatus::Loading;
                }
                runtime.refresh_inbox();
            }
            None => {
                self.inbox_status = InboxStatus::Failed {
                    message: "the transport is not running".to_string(),
                };
            }
        }
    }

    /// Refresh the active surface: the Inbox rows on Inbox, and always the ping
    /// (so the status bar and repo label stay honest).
    pub fn refresh_active(&mut self) {
        if self.active_route == Route::Inbox {
            self.refresh_inbox();
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
        self.active_route = route;
        self.detail_open = false;
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
    pub fn inbox_status(&self) -> &InboxStatus {
        &self.inbox_status
    }

    /// Whether any snapshot has produced rows (the error banner keeps them).
    pub fn has_inbox_data(&self) -> bool {
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
}
