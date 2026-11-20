//! The tokio runtime thread and its GPUI channel bridge.
//!
//! `reqwest` and the SSE reader are async; GPUI's UI thread is not a tokio
//! runtime. The desktop client therefore owns a dedicated tokio runtime on a
//! background thread and talks to it over two `async-channel`s:
//!
//! ```text
//!   UI thread (GPUI)                    runtime thread (tokio)
//!   ────────────────                    ─────────────────────
//!   Command::Ping / Reconnect  ───────▶  RPC calls, SSE stream
//!   Update::Connection / Ping / Push ◀─  (decoded, typed values)
//! ```
//!
//! Commands are best-effort: a full command queue drops a duplicate ping or
//! reconnect rather than blocking the UI. Updates are bounded too — a UI that
//! stops draining loses old pushes, which the durable event cursor can replay.

#![allow(dead_code)] // `ping()` and the push accessors are consumed by steps 03+.

use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use async_channel::{Receiver, Sender};
use favetto_wire::model::{AgentCatalogEntry, AgentSessionInfo, InputReply, Task, TaskRun};
use favetto_wire::rpc::{
    AgentReplyParams, AgentReplyResult, CatalogEntry, CatalogGetParams, EmptyParams, PingParams,
    PingResult, StartOneshotParams, StartOneshotResult, TaskIdParams, TasksEventsParams,
    TasksListParams, TasksStartParams,
};
use uuid::Uuid;

use crate::config::ClientConfig;
use crate::detail::{DetailEventsCall, DetailEventsResult};
use crate::sse::{self, Push, SseEvent, SseStatus};
use crate::state::ConnectionState;
use crate::tasks::TaskAction;
use crate::transport::{ClientError, RpcClient};

/// How many rows `tasks.list` fetches for the Inbox and the Tasks surface. The
/// Inbox caps its Recent group; the larger window keeps every run the Tasks
/// filter and search can reach.
pub const TASK_LIST_LIMIT: u64 = 500;

/// Work the UI asks the runtime to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Run one `system.ping` round trip.
    Ping,
    /// Drop the SSE stream, start a fresh one, and re-validate the connection.
    Reconnect,
    /// Re-fetch `tasks.list` and `agents.list` for the Inbox and Tasks views.
    /// Loads coalesce, so a burst of pushes costs one round trip, not one per
    /// frame.
    RefreshSnapshot,
    /// Re-fetch `catalog.list` (connect, Retry or a `catalog.updated` push).
    RefreshCatalog,
    /// Fetch one definition's raw Markdown for the preview pane.
    LoadCatalogEntry { name: String },
    /// Enqueue a catalog definition with its collected variables.
    StartCatalogTask {
        name: String,
        input: serde_json::Value,
    },
    /// Open an inline one-shot agent session (the Catalog toolbar's secondary
    /// action).
    StartOneshot {
        agent: Option<String>,
        cwd: Option<String>,
    },
    /// Send `tasks.cancel` or `tasks.retry` for one task. `previous` is the row
    /// the UI optimistically replaced and `optimistic` is the replacement, so a
    /// failed RPC can restore the former only if no newer push replaced it.
    TaskAction {
        action: TaskAction,
        previous: Box<Task>,
        optimistic: Box<Task>,
    },
    /// Fetch `tasks.get`, `tasks.events` and `tasks.runs` for the open detail.
    LoadTaskDetail { task_id: Uuid },
    /// Fetch only the detail's persisted output (the Output tab's Retry).
    LoadTask { task_id: Uuid },
    /// Re-run the structured-event backfill (the Events tab's Retry).
    LoadTaskEvents { task_id: Uuid },
    /// Re-fetch the attempt table (the Runs tab's Retry).
    LoadTaskRuns { task_id: Uuid },
    /// Answer one blocked prompt. The request id comes from the session's
    /// `awaiting_input` reason or the feed's `InputRequested` event.
    Reply {
        session_id: String,
        request_id: String,
        reply: InputReply,
    },
    /// Stop the worker thread.
    Shutdown,
}

/// The result of one `system.ping` round trip, with its wall-clock latency.
#[derive(Debug, Clone)]
pub struct PingSnapshot {
    pub result: Result<PingResult, ClientError>,
    pub round_trip: Duration,
}

/// Values the runtime sends back to the UI.
#[derive(Debug, Clone)]
pub enum Update {
    /// The connection-state transitioned (SSE status or a transport failure).
    Connection(ConnectionState),
    /// A `system.ping` round trip completed.
    Ping(PingSnapshot),
    /// One decoded server push.
    Push(Box<Push>),
    /// The durable resume cursor advanced.
    Cursor(Option<i64>),
    /// A push whose payload could not be decoded (shown, never fatal).
    Malformed(String),
    /// A fresh Inbox snapshot: task rows plus every live agent session.
    SnapshotLoaded {
        tasks: Vec<Task>,
        sessions: Vec<AgentSessionInfo>,
        /// The agent roster, used by the one-shot picker.
        agents: Vec<AgentCatalogEntry>,
    },
    /// The Inbox/Tasks fetch failed (`tasks.list` is required; a failing agents
    /// list keeps the task rows and drops the session overlay).
    SnapshotFailed { error: ClientError },
    /// A fresh `catalog.list` snapshot.
    CatalogLoaded { entries: Vec<CatalogEntry> },
    /// `catalog.list` failed.
    CatalogFailed { error: ClientError },
    /// `catalog.get` completed for the selected definition.
    CatalogPreviewLoaded { name: String, markdown: String },
    /// `catalog.get` failed for the selected definition.
    CatalogPreviewFailed { name: String, error: ClientError },
    /// `tasks.start` completed for the launch sheet.
    CatalogLaunchFinished {
        name: String,
        result: Result<Box<Task>, ClientError>,
    },
    /// `tasks.start_oneshot` completed for the one-shot sheet.
    OneshotFinished {
        result: Result<Box<StartOneshotResult>, ClientError>,
    },
    /// A `tasks.cancel`/`tasks.retry` round trip completed. `previous` is the
    /// pre-optimistic row and `optimistic` the local write, used for the guarded
    /// rollback on error.
    TaskActionFinished {
        action: TaskAction,
        previous: Box<Task>,
        optimistic: Box<Task>,
        result: Result<Box<Task>, ClientError>,
    },
    /// `tasks.get` completed for the open detail.
    TaskLoaded { task: Box<Task> },
    /// `tasks.get` failed for the open detail.
    TaskLoadFailed { task_id: Uuid, error: ClientError },
    /// A tolerant `tasks.events` backfill completed.
    EventsLoaded {
        task_id: Uuid,
        result: Box<DetailEventsResult>,
    },
    /// `tasks.events` failed.
    EventsFailed { task_id: Uuid, error: ClientError },
    /// `tasks.runs` completed.
    RunsLoaded { task_id: Uuid, runs: Vec<TaskRun> },
    /// `tasks.runs` failed.
    RunsFailed { task_id: Uuid, error: ClientError },
    /// `agents.reply` completed for one correlated request.
    ReplyFinished {
        session_id: String,
        request_id: String,
        reply: InputReply,
        result: Result<AgentReplyResult, ClientError>,
    },
}

/// The runtime thread handle plus both ends of the bridge.
pub struct Runtime {
    commands: Sender<Command>,
    updates: Receiver<Update>,
    thread: Option<JoinHandle<()>>,
}

impl Runtime {
    /// Start the tokio thread.
    ///
    /// Fails only when the OS refuses to spawn the thread; connection errors are
    /// reported later as [`Update::Connection`] values.
    pub fn spawn(config: ClientConfig) -> Result<Self, String> {
        let (commands, command_rx) = async_channel::bounded(16);
        let (update_tx, updates) = async_channel::bounded(512);

        let thread = std::thread::Builder::new()
            .name("favetto-gpui-runtime".to_string())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .thread_name("favetto-gpui-tokio")
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(err) => {
                        let _ =
                            update_tx.try_send(Update::Connection(ConnectionState::Disconnected {
                                message: format!("could not start the tokio runtime: {err}"),
                            }));
                        return;
                    }
                };
                runtime.block_on(worker(config, command_rx, update_tx));
            })
            .map_err(|err| format!("could not start the runtime thread: {err}"))?;

        Ok(Self {
            commands,
            updates,
            thread: Some(thread),
        })
    }

    /// Queue a command without blocking the UI.
    pub fn send(&self, command: Command) {
        let _ = self.commands.try_send(command);
    }

    /// A clone of the update stream for the foreground drain loop.
    pub fn updates(&self) -> Receiver<Update> {
        self.updates.clone()
    }

    /// Ask for one ping round trip.
    pub fn ping(&self) {
        self.send(Command::Ping);
    }

    /// Restart the SSE stream and re-validate the connection.
    pub fn reconnect(&self) {
        self.send(Command::Reconnect);
    }

    /// Ask for a fresh task snapshot. Coalesced by the loader.
    pub fn refresh_snapshot(&self) {
        self.send(Command::RefreshSnapshot);
    }

    /// Ask for a fresh catalog list. Coalesced by the loader.
    pub fn refresh_catalog(&self) {
        self.send(Command::RefreshCatalog);
    }

    /// Fetch one definition's raw Markdown source.
    pub fn load_catalog_entry(&self, name: String) {
        self.send(Command::LoadCatalogEntry { name });
    }

    /// Enqueue a catalog definition with its collected input.
    pub fn start_catalog_task(&self, name: String, input: serde_json::Value) {
        self.send(Command::StartCatalogTask { name, input });
    }

    /// Open an inline one-shot agent session.
    pub fn start_oneshot(&self, agent: Option<String>, cwd: Option<String>) {
        self.send(Command::StartOneshot { agent, cwd });
    }

    /// Run `tasks.cancel`/`tasks.retry` for a row the UI already changed.
    pub fn task_action(&self, action: TaskAction, previous: Task, optimistic: Task) {
        self.send(Command::TaskAction {
            action,
            previous: Box::new(previous),
            optimistic: Box::new(optimistic),
        });
    }

    /// Load every part of one task's detail surface.
    pub fn load_task_detail(&self, task_id: Uuid) {
        self.send(Command::LoadTaskDetail { task_id });
    }

    /// Re-fetch the persisted output for the open detail.
    pub fn load_task(&self, task_id: Uuid) {
        self.send(Command::LoadTask { task_id });
    }

    /// Re-run the structured-event backfill for the open detail.
    pub fn load_task_events(&self, task_id: Uuid) {
        self.send(Command::LoadTaskEvents { task_id });
    }

    /// Re-fetch the attempt table for the open detail.
    pub fn load_task_runs(&self, task_id: Uuid) {
        self.send(Command::LoadTaskRuns { task_id });
    }

    /// Answer a blocked prompt through `agents.reply`.
    pub fn reply(&self, session_id: String, request_id: String, reply: InputReply) {
        self.send(Command::Reply {
            session_id,
            request_id,
            reply,
        });
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        // Use the blocking send so Shutdown cannot be dropped by a full queue
        // (which would make the `join` below wait forever).
        let _ = self.commands.send_blocking(Command::Shutdown);
        if let Some(thread) = self.thread.take() {
            // The worker aborts the SSE task on shutdown, so this is prompt.
            let _ = thread.join();
        }
    }
}

/// The runtime thread's main loop.
async fn worker(config: ClientConfig, commands: Receiver<Command>, updates: Sender<Update>) {
    let _ = updates.try_send(Update::Connection(ConnectionState::Connecting));

    let client = match RpcClient::new(&config) {
        Ok(client) => Arc::new(client),
        Err(err) => {
            let _ = updates.try_send(Update::Connection(ConnectionState::Disconnected {
                message: err.to_string(),
            }));
            return;
        }
    };

    let token_hint = config.token_source.label();
    let mut sse_task = Some(spawn_sse(&config, &updates, token_hint.clone()));
    spawn_ping(client.clone(), updates.clone());
    let (inbox_trigger, inbox_triggers) = async_channel::bounded(1);
    spawn_inbox_loader(client.clone(), updates.clone(), inbox_triggers);
    let (catalog_trigger, catalog_triggers) = async_channel::bounded(1);
    spawn_catalog_loader(client.clone(), updates.clone(), catalog_triggers);

    while let Ok(command) = commands.recv().await {
        match command {
            Command::Shutdown => break,
            Command::Ping => spawn_ping(client.clone(), updates.clone()),
            Command::Reconnect => {
                if let Some(task) = sse_task.take() {
                    task.abort();
                }
                let _ = updates.try_send(Update::Connection(ConnectionState::Connecting));
                sse_task = Some(spawn_sse(&config, &updates, token_hint.clone()));
                spawn_ping(client.clone(), updates.clone());
                // The UI also asks on the Connected transition; this covers a
                // reconnect whose state transition is not observed.
                let _ = inbox_trigger.try_send(());
                let _ = catalog_trigger.try_send(());
            }
            Command::RefreshSnapshot => {
                // Capacity one: if a load is already queued/running, this is a
                // no-op rather than a second overlapping RPC pair.
                let _ = inbox_trigger.try_send(());
            }
            Command::RefreshCatalog => {
                let _ = catalog_trigger.try_send(());
            }
            Command::LoadCatalogEntry { name } => {
                spawn_catalog_entry(client.clone(), updates.clone(), name);
            }
            Command::StartCatalogTask { name, input } => {
                spawn_start_catalog(client.clone(), updates.clone(), name, input);
            }
            Command::StartOneshot { agent, cwd } => {
                spawn_start_oneshot(client.clone(), updates.clone(), agent, cwd);
            }
            Command::TaskAction {
                action,
                previous,
                optimistic,
            } => {
                spawn_task_action(
                    client.clone(),
                    updates.clone(),
                    action,
                    *previous,
                    *optimistic,
                );
            }
            Command::LoadTaskDetail { task_id } => {
                spawn_task_load(client.clone(), updates.clone(), task_id);
                spawn_events_load(client.clone(), updates.clone(), task_id);
                spawn_runs_load(client.clone(), updates.clone(), task_id);
            }
            Command::LoadTask { task_id } => {
                spawn_task_load(client.clone(), updates.clone(), task_id);
            }
            Command::LoadTaskEvents { task_id } => {
                spawn_events_load(client.clone(), updates.clone(), task_id);
            }
            Command::LoadTaskRuns { task_id } => {
                spawn_runs_load(client.clone(), updates.clone(), task_id);
            }
            Command::Reply {
                session_id,
                request_id,
                reply,
            } => {
                spawn_reply(
                    client.clone(),
                    updates.clone(),
                    session_id,
                    request_id,
                    reply,
                );
            }
        }
    }

    if let Some(task) = sse_task {
        task.abort();
    }
}

/// Spawn one ping round trip; the reply is an [`Update::Ping`].
fn spawn_ping(client: Arc<RpcClient>, updates: Sender<Update>) {
    tokio::spawn(async move {
        let started = Instant::now();
        let result = client.ping(PingParams {}).await;
        let snapshot = PingSnapshot {
            result,
            round_trip: started.elapsed(),
        };
        let _ = updates.try_send(Update::Ping(snapshot));
    });
}

/// Drain catalog refresh triggers into one `catalog.list`, coalescing a burst
/// of `catalog.updated` pushes into a single round trip.
fn spawn_catalog_loader(client: Arc<RpcClient>, updates: Sender<Update>, triggers: Receiver<()>) {
    tokio::spawn(async move {
        while triggers.recv().await.is_ok() {
            while triggers.try_recv().is_ok() {}
            let update = match client.catalog_list(EmptyParams {}).await {
                Ok(entries) => Update::CatalogLoaded { entries },
                Err(error) => Update::CatalogFailed { error },
            };
            let _ = updates.try_send(update);
        }
    });
}

/// Spawn one `catalog.get` for the preview pane.
fn spawn_catalog_entry(client: Arc<RpcClient>, updates: Sender<Update>, name: String) {
    tokio::spawn(async move {
        let result = client
            .catalog_get(CatalogGetParams { name: name.clone() })
            .await;
        let update = match result {
            Ok(result) => Update::CatalogPreviewLoaded {
                name,
                markdown: result.markdown,
            },
            Err(error) => Update::CatalogPreviewFailed { name, error },
        };
        let _ = updates.try_send(update);
    });
}

/// Spawn one `tasks.start` for a catalog definition.
fn spawn_start_catalog(
    client: Arc<RpcClient>,
    updates: Sender<Update>,
    name: String,
    input: serde_json::Value,
) {
    tokio::spawn(async move {
        let result = client
            .tasks_start(TasksStartParams {
                name: name.clone(),
                input: Some(input),
                // The desktop client runs the task headlessly and renders its
                // structured events; the embedded TUI stays a TUI feature.
                interactive: None,
            })
            .await
            .map(Box::new);
        let _ = updates.try_send(Update::CatalogLaunchFinished { name, result });
    });
}

/// Spawn one `tasks.start_oneshot` for the toolbar's secondary action.
fn spawn_start_oneshot(
    client: Arc<RpcClient>,
    updates: Sender<Update>,
    agent: Option<String>,
    cwd: Option<String>,
) {
    tokio::spawn(async move {
        let result = client
            .tasks_start_oneshot(StartOneshotParams {
                agent,
                provider: None,
                model: None,
                cwd,
                rows: None,
                cols: None,
            })
            .await
            .map(Box::new);
        let _ = updates.try_send(Update::OneshotFinished { result });
    });
}

/// Drain Inbox refresh triggers into `tasks.list` + `agents.list` snapshots.
///
/// The trigger channel has capacity one and is drained before each fetch, so a
/// burst of `task.updated` / `agent.state` / `agent.event` pushes (or several
/// `r` presses) collapses into at most one follow-up load. That keeps the SSE
/// tick responsive without letting a chatty agent starve the daemon's RPC path.
fn spawn_inbox_loader(client: Arc<RpcClient>, updates: Sender<Update>, triggers: Receiver<()>) {
    tokio::spawn(async move {
        while triggers.recv().await.is_ok() {
            // Fold every trigger already waiting into this load.
            while triggers.try_recv().is_ok() {}

            // The two reads are independent; run them concurrently so the
            // snapshot lands within one UI tick on a local daemon.
            let (tasks, agents) = tokio::join!(
                client.tasks_list(TasksListParams {
                    limit: Some(TASK_LIST_LIMIT),
                }),
                client.agents_list(EmptyParams {})
            );

            let update = match tasks {
                Ok(tasks) => {
                    // Losing the agents list costs the request text, agent
                    // names and the one-shot roster, not the task rows, so it
                    // is not fatal.
                    let (sessions, agents) = match agents {
                        Ok(agents) => {
                            let sessions = agents
                                .iter()
                                .flat_map(|agent| agent.sessions.iter().cloned())
                                .collect();
                            (sessions, agents)
                        }
                        Err(_) => (Vec::new(), Vec::new()),
                    };
                    Update::SnapshotLoaded {
                        tasks,
                        sessions,
                        agents,
                    }
                }
                Err(error) => Update::SnapshotFailed { error },
            };
            let _ = updates.try_send(update);
        }
    });
}

/// Spawn one `tasks.cancel` / `tasks.retry`; the reply is a
/// [`Update::TaskActionFinished`] so the optimistic row can be confirmed or
/// rolled back.
fn spawn_task_action(
    client: Arc<RpcClient>,
    updates: Sender<Update>,
    action: TaskAction,
    previous: Task,
    optimistic: Task,
) {
    tokio::spawn(async move {
        let params = TaskIdParams { id: previous.id };
        let result = match action {
            TaskAction::Cancel => client.tasks_cancel(params).await,
            TaskAction::Retry => client.tasks_retry(params).await,
        }
        .map(Box::new);
        let _ = updates.try_send(Update::TaskActionFinished {
            action,
            previous: Box::new(previous),
            optimistic: Box::new(optimistic),
            result,
        });
    });
}

/// Spawn one `tasks.get` for the detail surface.
fn spawn_task_load(client: Arc<RpcClient>, updates: Sender<Update>, task_id: Uuid) {
    tokio::spawn(async move {
        let result = client.tasks_get(TaskIdParams { id: task_id }).await;
        let update = match result {
            Ok(task) => Update::TaskLoaded {
                task: Box::new(task),
            },
            Err(error) => Update::TaskLoadFailed { task_id, error },
        };
        let _ = updates.try_send(update);
    });
}

/// Spawn the structured-event backfill.
///
/// The daemon caps one page at 2000 events, so a long-running session is paged
/// forward from `seq` zero. Ten pages (20 000 events) is the ceiling: the detail
/// surface is a live view, and a run with more history than that is better
/// served by the persisted output. The pages are merged into one update so the
/// UI never renders a half-filled gap.
fn spawn_events_load(client: Arc<RpcClient>, updates: Sender<Update>, task_id: Uuid) {
    tokio::spawn(async move {
        const PAGE: u64 = 2000;
        const MAX_PAGES: usize = 10;

        let mut session_id: Option<String> = None;
        let mut events = Vec::new();
        let mut ignored = 0usize;
        let mut next_cursor = 0u64;
        let mut after = 0u64;

        for _ in 0..MAX_PAGES {
            let page = client
                .call::<DetailEventsCall>(TasksEventsParams {
                    id: task_id,
                    after: Some(after),
                    limit: Some(PAGE),
                })
                .await;
            match page {
                Ok(page) => {
                    if session_id.is_none() {
                        session_id = page.session_id.clone();
                    }
                    ignored += page.ignored;
                    next_cursor = next_cursor.max(page.next_cursor);
                    let count = page.events.len();
                    let last_seq = page.events.last().map(|event| event.seq);
                    let cursor = page.next_cursor.max(last_seq.unwrap_or(after));
                    events.extend(page.events);
                    // A short page is the end; a cursor that does not advance
                    // would otherwise page forever.
                    if count < PAGE as usize || cursor <= after {
                        break;
                    }
                    after = cursor;
                }
                Err(error) => {
                    if events.is_empty() {
                        let _ = updates.try_send(Update::EventsFailed { task_id, error });
                        return;
                    }
                    // Keep the page(s) already fetched; the next open/retry can
                    // fill the rest rather than blanking a readable feed.
                    break;
                }
            }
        }

        events.sort_by_key(|event| event.seq);
        let result = DetailEventsResult {
            session_id,
            events,
            next_cursor,
            ignored,
        };
        let _ = updates.try_send(Update::EventsLoaded {
            task_id,
            result: Box::new(result),
        });
    });
}

/// Spawn one `tasks.runs` fetch for the detail surface.
fn spawn_runs_load(client: Arc<RpcClient>, updates: Sender<Update>, task_id: Uuid) {
    tokio::spawn(async move {
        let result = client.tasks_runs(TaskIdParams { id: task_id }).await;
        let update = match result {
            Ok(runs) => Update::RunsLoaded { task_id, runs },
            Err(error) => Update::RunsFailed { task_id, error },
        };
        let _ = updates.try_send(update);
    });
}

/// Spawn one `agents.reply` for a correlated prompt.
fn spawn_reply(
    client: Arc<RpcClient>,
    updates: Sender<Update>,
    session_id: String,
    request_id: String,
    reply: InputReply,
) {
    tokio::spawn(async move {
        let result = client
            .agents_reply(AgentReplyParams {
                session_id: session_id.clone(),
                request_id: request_id.clone(),
                reply: reply.clone(),
            })
            .await;
        let _ = updates.try_send(Update::ReplyFinished {
            session_id,
            request_id,
            reply,
            result,
        });
    });
}

/// Spawn the SSE reader, wiring its callbacks to the update channel.
fn spawn_sse(
    config: &ClientConfig,
    updates: &Sender<Update>,
    token_hint: String,
) -> tokio::task::JoinHandle<()> {
    let url = config.events_url();
    let token = config.token.clone();
    let push_tx = updates.clone();
    let status_tx = updates.clone();

    tokio::spawn(async move {
        let mut sent_cursor: Option<i64> = None;
        sse::run(
            url,
            token,
            None,
            move |event: SseEvent, cursor: Option<i64>| {
                if cursor != sent_cursor {
                    sent_cursor = cursor;
                    let _ = push_tx.try_send(Update::Cursor(cursor));
                }
                match sse::decode_push(&event) {
                    Ok(Some(push)) => {
                        let _ = push_tx.try_send(Update::Push(Box::new(push)));
                    }
                    Ok(None) => {}
                    Err(err) => {
                        let _ = push_tx.try_send(Update::Malformed(err.to_string()));
                    }
                }
            },
            move |status| {
                let connection = connection_for(status, &token_hint);
                let _ = status_tx.try_send(Update::Connection(connection));
            },
        )
        .await;
    })
}

/// Map an SSE transition to the UI's connection state.
fn connection_for(status: SseStatus, token_path: &str) -> ConnectionState {
    match status {
        SseStatus::Connected => ConnectionState::Connected,
        SseStatus::Reconnecting { attempt, delay_ms } => {
            ConnectionState::Reconnecting { attempt, delay_ms }
        }
        SseStatus::Unauthorized => ConnectionState::Unauthorized {
            token_path: token_path.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_statuses_map_to_connection_states() {
        assert_eq!(
            connection_for(SseStatus::Connected, "token"),
            ConnectionState::Connected
        );
        assert_eq!(
            connection_for(
                SseStatus::Reconnecting {
                    attempt: 2,
                    delay_ms: 4_000
                },
                "token"
            ),
            ConnectionState::Reconnecting {
                attempt: 2,
                delay_ms: 4_000
            }
        );
        assert_eq!(
            connection_for(
                SseStatus::Unauthorized,
                "/home/u/.local/share/favetto/token"
            ),
            ConnectionState::Unauthorized {
                token_path: "/home/u/.local/share/favetto/token".to_string()
            }
        );
    }
}
