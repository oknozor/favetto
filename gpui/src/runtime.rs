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

use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use async_channel::{Receiver, Sender};
use favetto_wire::elicitation::ElicitationReply;
use favetto_wire::model::{
    AgentCatalogEntry, AgentSessionInfo, InputReply, Schedule, SessionConfigValue, Task, TaskRun,
};
use favetto_wire::rpc::{
    AgentAttachResult, AgentAuthenticateParams, AgentAuthenticateResult, AgentCancelParams,
    AgentCancelResult, AgentCloseResult, AgentElicitationReplyParams, AgentElicitationReplyResult,
    AgentLogoutParams, AgentLogoutResult, AgentOptionsResult, AgentPromptParams, AgentPromptResult,
    AgentReplyParams, AgentReplyResult, AgentSetModeParams, AgentSetOptionParams, CatalogEntry,
    CatalogGetParams, EmptyParams, PingParams, PingResult, ReposActivateParams, ReposListResult,
    ScheduleDeleteParams, ScheduleUpsertParams, SchedulesDeleteResult, SessionIdParams,
    StartAgentParams, StartOneshotParams, StartOneshotResult, TaskIdParams, TasksDeleteResult,
    TasksEventsParams, TasksListParams, TasksSessionResult, TasksStartParams,
    WorkflowCancelResultBody, WorkflowGetResult, WorkflowInspectParams, WorkflowRetryParams,
};
use favetto_wire::workflow::WorkflowInspect;
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
    /// Re-fetch `tasks.list` and `agents.list` for the Inbox, Tasks and Settings
    /// views. Loads coalesce, so a burst of pushes costs one round trip, not one
    /// per frame.
    RefreshSnapshot,
    /// Re-fetch `catalog.list` (connect, Retry or a `catalog.updated` push).
    RefreshCatalog,
    /// Fetch one definition's raw Markdown for the preview pane.
    LoadCatalogEntry { name: String },
    /// Enqueue a catalog definition with its collected variables.
    StartCatalogTask {
        name: String,
        input: serde_json::Value,
        agent: Option<String>,
        model: Option<String>,
    },
    /// Open an inline one-shot agent session (the Catalog toolbar's secondary
    /// action).
    StartOneshot {
        agent: Option<String>,
        cwd: Option<String>,
    },
    /// Start a free-standing ACP session (`agents.start`) with no `task_id`.
    StartAgent {
        agent: Option<String>,
        provider: Option<String>,
        model: Option<String>,
        cwd: Option<PathBuf>,
        new: bool,
    },
    /// Close one standalone ACP session (`agents.close`).
    CloseAgent { session_id: String },
    /// Send `tasks.cancel` or `tasks.retry` for one task. `previous` is the row
    /// the UI optimistically replaced and `optimistic` is the replacement, so a
    /// failed RPC can restore the former only if no newer push replaced it.
    TaskAction {
        action: TaskAction,
        previous: Box<Task>,
        optimistic: Box<Task>,
    },
    /// Permanently remove one terminal task and its recorded history.
    DeleteTask { task_id: Uuid },
    /// Fetch `tasks.get`, `tasks.events` and `tasks.runs` for the open detail.
    LoadTaskDetail { task_id: Uuid },
    /// Resolve the agent session associated with one task (`tasks.session`).
    /// The request id lets the UI drop a reply for a superseded open/attempt.
    LoadTaskSession { task_id: Uuid, request_id: u64 },
    /// Fetch only the detail's persisted output (the Output tab's Retry).
    LoadTask { task_id: Uuid },
    /// Re-run the structured-event backfill from `after` (the output tab's
    /// Retry, or the cursor after a restored transcript).
    LoadTaskEvents { task_id: Uuid, after: u64 },
    /// Re-fetch the attempt table (the Runs tab's Retry).
    LoadTaskRuns { task_id: Uuid },
    /// Answer one blocked prompt. The request id comes from the session's
    /// `awaiting_input` reason or the feed's `InputRequested` event.
    Reply {
        session_id: String,
        request_id: String,
        reply: InputReply,
    },
    /// Answer one ACP form elicitation (`agents.reply_elicitation`).
    ElicitationReply {
        session_id: String,
        request_id: String,
        reply: ElicitationReply,
    },
    /// Interrupt the in-flight turn of one live agent session (`agents.cancel`).
    CancelTurn { session_id: String },
    /// Send an ordinary follow-up prompt (`agents.prompt`). Slash commands are
    /// prompt text; the runtime never rewrites the string.
    Prompt { session_id: String, prompt: String },
    /// Restore a persisted ACP session by its Favetto-local id (`agents.load`).
    LoadAgentSession { session_id: String },
    /// Change one advertised ACP config option (`agents.set_option`).
    SetAgentOption {
        session_id: String,
        option_id: String,
        value: SessionConfigValue,
    },
    /// Switch a live ACP session to an advertised mode (`agents.set_mode`).
    SetAgentMode { session_id: String, mode_id: String },
    /// Complete an agent-handled ACP sign-in (`agents.authenticate`).
    Authenticate {
        session_id: String,
        method_id: String,
    },
    /// Sign an ACP session out when the agent advertised logout (`agents.logout`).
    Logout { session_id: String },
    /// Fetch one root's runtime workflow graph (`workflow.inspect`).
    LoadWorkflowInspect { root_id: Uuid },
    /// Fetch the catalog DAG source (`workflow.get`).
    LoadWorkflowGraph,
    /// Cancel every non-terminal instance under a root (`workflow.cancel`).
    CancelWorkflow { root_id: Uuid },
    /// Re-enqueue one terminal instance (`workflow.retry`).
    RetryWorkflowTask { task_id: Uuid },
    /// Re-fetch `schedules.list` (connect, Retry, a save/delete or a cron tick).
    RefreshSchedules,
    /// Create or update one schedule.
    UpsertSchedule {
        id: Option<String>,
        cron: String,
        task: String,
        input: Option<serde_json::Value>,
        enabled: bool,
    },
    /// Delete one schedule by id.
    DeleteSchedule { id: String },
    /// Re-fetch `repos.list` (connect, a manual refresh or a `catalog.updated`
    /// push from another client's add/remove/activate).
    RefreshRepos,
    /// Activate one repo (`repos.activate`), or clear it for the global view.
    ActivateRepo { name: Option<String> },
    /// Run `system.ping` against a draft endpoint/token without touching the
    /// live connection (the Settings "Test connection" action).
    TestConnection {
        endpoint: String,
        token: Option<String>,
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
    /// `agents.start` completed for the Agent panel.
    AgentStartFinished {
        result: Result<Box<AgentAttachResult>, ClientError>,
    },
    /// `agents.close` completed for one standalone session.
    AgentClosed {
        session_id: String,
        result: Result<AgentCloseResult, ClientError>,
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
    /// A `tasks.delete` round trip completed. `deleted` distinguishes an
    /// idempotent no-op from a removal; `task_id` keys the rollback snapshot.
    TaskDeleteFinished {
        task_id: Uuid,
        result: Result<TasksDeleteResult, ClientError>,
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
    /// `tasks.session` completed for one task-scoped restore lookup.
    TaskSessionLoaded {
        task_id: Uuid,
        request_id: u64,
        result: Result<TasksSessionResult, ClientError>,
    },
    /// `agents.reply` completed for one correlated request.
    ReplyFinished {
        session_id: String,
        request_id: String,
        reply: InputReply,
        result: Result<AgentReplyResult, ClientError>,
    },
    /// `agents.reply_elicitation` completed for one correlated request.
    ElicitationReplyFinished {
        session_id: String,
        request_id: String,
        reply: ElicitationReply,
        result: Result<AgentElicitationReplyResult, ClientError>,
    },
    /// `agents.cancel` completed for the live session.
    TurnCancelFinished {
        session_id: String,
        result: Result<AgentCancelResult, ClientError>,
    },
    /// `agents.prompt` completed for one follow-up prompt.
    PromptFinished {
        session_id: String,
        prompt: String,
        result: Result<AgentPromptResult, ClientError>,
    },
    /// `agents.load` completed for the Favetto-local session id.
    AgentSessionLoaded {
        session_id: String,
        result: Result<Box<AgentAttachResult>, ClientError>,
    },
    /// `agents.set_option` / `agents.set_mode` completed for one session.
    AgentOptionsChanged {
        session_id: String,
        result: Result<Box<AgentOptionsResult>, ClientError>,
    },
    /// `agents.authenticate` completed for one session.
    AuthFinished {
        session_id: String,
        method_id: String,
        result: Result<Box<AgentAuthenticateResult>, ClientError>,
    },
    /// `agents.logout` completed for one session.
    LogoutFinished {
        session_id: String,
        result: Result<AgentLogoutResult, ClientError>,
    },
    /// `workflow.inspect` completed for one root.
    WorkflowInspectLoaded {
        root_id: Uuid,
        view: Box<WorkflowInspect>,
    },
    /// `workflow.inspect` failed for one root.
    WorkflowInspectFailed { root_id: Uuid, error: ClientError },
    /// `workflow.get` completed (DOT source, path and structured graph).
    WorkflowGraphLoaded { result: Box<WorkflowGetResult> },
    /// `workflow.get` failed.
    WorkflowGraphFailed { error: ClientError },
    /// `workflow.cancel` completed for the root.
    WorkflowCancelFinished {
        root_id: Uuid,
        result: Result<WorkflowCancelResultBody, ClientError>,
    },
    /// `workflow.retry` completed for one instance.
    WorkflowRetryFinished {
        task_id: Uuid,
        result: Result<Box<Task>, ClientError>,
    },
    /// A fresh `schedules.list` snapshot.
    SchedulesLoaded { schedules: Vec<Schedule> },
    /// `schedules.list` failed.
    SchedulesFailed { error: ClientError },
    /// `schedules.upsert` completed; the schedule carries its computed next run.
    ScheduleSaved {
        result: Result<Box<Schedule>, ClientError>,
    },
    /// `schedules.delete` completed.
    ScheduleDeleted {
        id: String,
        result: Result<SchedulesDeleteResult, ClientError>,
    },
    /// A fresh `repos.list` registry (also fetched after `catalog.updated`).
    ReposLoaded {
        result: Result<ReposListResult, ClientError>,
    },
    /// `repos.activate` completed; the result is the updated registry.
    RepoActivated {
        result: Result<ReposListResult, ClientError>,
    },
    /// A draft-connection ping completed.
    ConnectionTestFinished {
        endpoint: String,
        result: Result<PingResult, ClientError>,
        round_trip: Duration,
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

    /// Enqueue a catalog definition with its collected input and optional
    /// per-launch agent/model overrides.
    pub fn start_catalog_task(
        &self,
        name: String,
        input: serde_json::Value,
        agent: Option<String>,
        model: Option<String>,
    ) {
        self.send(Command::StartCatalogTask {
            name,
            input,
            agent,
            model,
        });
    }

    /// Open an inline one-shot agent session.
    pub fn start_oneshot(&self, agent: Option<String>, cwd: Option<String>) {
        self.send(Command::StartOneshot { agent, cwd });
    }

    /// Start a free-standing agent session in `cwd` (the active repo checkout).
    pub fn start_agent(
        &self,
        agent: Option<String>,
        provider: Option<String>,
        model: Option<String>,
        cwd: Option<PathBuf>,
        new: bool,
    ) {
        self.send(Command::StartAgent {
            agent,
            provider,
            model,
            cwd,
            new,
        });
    }

    /// Close one standalone agent session.
    pub fn close_agent(&self, session_id: String) {
        self.send(Command::CloseAgent { session_id });
    }

    /// Run `tasks.cancel`/`tasks.retry` for a row the UI already changed.
    pub fn task_action(&self, action: TaskAction, previous: Task, optimistic: Task) {
        self.send(Command::TaskAction {
            action,
            previous: Box::new(previous),
            optimistic: Box::new(optimistic),
        });
    }

    /// Permanently delete one terminal task (`tasks.delete`).
    pub fn delete_task(&self, task_id: Uuid) {
        self.send(Command::DeleteTask { task_id });
    }

    /// Load every part of one task's detail surface.
    pub fn load_task_detail(&self, task_id: Uuid) {
        self.send(Command::LoadTaskDetail { task_id });
    }

    /// Re-fetch the persisted output for the open detail.
    pub fn load_task(&self, task_id: Uuid) {
        self.send(Command::LoadTask { task_id });
    }

    /// Resolve the agent session associated with one task (`tasks.session`).
    pub fn load_task_session(&self, task_id: Uuid, request_id: u64) {
        self.send(Command::LoadTaskSession {
            task_id,
            request_id,
        });
    }

    /// Re-run the structured-event backfill for the open detail from `after`.
    pub fn load_task_events(&self, task_id: Uuid, after: u64) {
        self.send(Command::LoadTaskEvents { task_id, after });
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

    /// Answer one ACP form elicitation through `agents.reply_elicitation`.
    pub fn reply_elicitation(
        &self,
        session_id: String,
        request_id: String,
        reply: ElicitationReply,
    ) {
        self.send(Command::ElicitationReply {
            session_id,
            request_id,
            reply,
        });
    }

    /// Interrupt the in-flight turn of one live agent session.
    pub fn cancel_turn(&self, session_id: String) {
        self.send(Command::CancelTurn { session_id });
    }

    /// Send an ordinary follow-up prompt through `agents.prompt`.
    pub fn prompt(&self, session_id: String, prompt: String) {
        self.send(Command::Prompt { session_id, prompt });
    }

    /// Restore a persisted session by its Favetto-local id.
    pub fn load_agent_session(&self, session_id: String) {
        self.send(Command::LoadAgentSession { session_id });
    }

    /// Change one advertised ACP config option.
    pub fn set_agent_option(
        &self,
        session_id: String,
        option_id: String,
        value: SessionConfigValue,
    ) {
        self.send(Command::SetAgentOption {
            session_id,
            option_id,
            value,
        });
    }

    /// Switch a live ACP session to an advertised mode.
    pub fn set_agent_mode(&self, session_id: String, mode_id: String) {
        self.send(Command::SetAgentMode {
            session_id,
            mode_id,
        });
    }

    /// Complete an agent-handled ACP sign-in for a session.
    pub fn authenticate(&self, session_id: String, method_id: String) {
        self.send(Command::Authenticate {
            session_id,
            method_id,
        });
    }

    /// Sign a session out when the agent advertised ACP logout.
    pub fn logout(&self, session_id: String) {
        self.send(Command::Logout { session_id });
    }

    /// Fetch one root's runtime workflow view.
    pub fn load_workflow_inspect(&self, root_id: Uuid) {
        self.send(Command::LoadWorkflowInspect { root_id });
    }

    /// Fetch the catalog DAG source.
    pub fn load_workflow_graph(&self) {
        self.send(Command::LoadWorkflowGraph);
    }

    /// Cancel the non-terminal instances under a root.
    pub fn cancel_workflow(&self, root_id: Uuid) {
        self.send(Command::CancelWorkflow { root_id });
    }

    /// Re-enqueue one terminal workflow instance.
    pub fn retry_workflow_task(&self, task_id: Uuid) {
        self.send(Command::RetryWorkflowTask { task_id });
    }

    /// Ask for a fresh schedule list. Coalesced by the loader.
    pub fn refresh_schedules(&self) {
        self.send(Command::RefreshSchedules);
    }

    /// Create or update one schedule.
    pub fn upsert_schedule(
        &self,
        id: Option<String>,
        cron: String,
        task: String,
        input: Option<serde_json::Value>,
        enabled: bool,
    ) {
        self.send(Command::UpsertSchedule {
            id,
            cron,
            task,
            input,
            enabled,
        });
    }

    /// Delete one schedule.
    pub fn delete_schedule(&self, id: String) {
        self.send(Command::DeleteSchedule { id });
    }

    /// Ask for a fresh repo registry. Coalesced by the loader.
    pub fn refresh_repos(&self) {
        self.send(Command::RefreshRepos);
    }

    /// Activate one repo, or clear the active repo for the global view.
    pub fn activate_repo(&self, name: Option<String>) {
        self.send(Command::ActivateRepo { name });
    }

    /// Ping a draft endpoint without disturbing the live connection.
    pub fn test_connection(&self, endpoint: String, token: Option<String>) {
        self.send(Command::TestConnection { endpoint, token });
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
    let (schedule_trigger, schedule_triggers) = async_channel::bounded(1);
    spawn_schedule_loader(client.clone(), updates.clone(), schedule_triggers);
    let (repo_trigger, repo_triggers) = async_channel::bounded(1);
    spawn_repo_loader(client.clone(), updates.clone(), repo_triggers);
    // Fill the top-bar label as soon as the worker can reach the daemon; the
    // Connected transition also asks, and the queue coalesces the two.
    let _ = repo_trigger.try_send(());

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
                let _ = schedule_trigger.try_send(());
                let _ = repo_trigger.try_send(());
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
            Command::StartCatalogTask {
                name,
                input,
                agent,
                model,
            } => {
                spawn_start_catalog(client.clone(), updates.clone(), name, input, agent, model);
            }
            Command::StartOneshot { agent, cwd } => {
                spawn_start_oneshot(client.clone(), updates.clone(), agent, cwd);
            }
            Command::StartAgent {
                agent,
                provider,
                model,
                cwd,
                new,
            } => {
                spawn_start_agent(
                    client.clone(),
                    updates.clone(),
                    agent,
                    provider,
                    model,
                    cwd,
                    new,
                );
            }
            Command::CloseAgent { session_id } => {
                spawn_close_agent(client.clone(), updates.clone(), session_id);
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
            Command::DeleteTask { task_id } => {
                spawn_delete_task(client.clone(), updates.clone(), task_id);
            }
            Command::LoadTaskDetail { task_id } => {
                spawn_task_load(client.clone(), updates.clone(), task_id);
                spawn_events_load(client.clone(), updates.clone(), task_id, 0);
                spawn_runs_load(client.clone(), updates.clone(), task_id);
            }
            Command::LoadTaskSession {
                task_id,
                request_id,
            } => {
                spawn_task_session(client.clone(), updates.clone(), task_id, request_id);
            }
            Command::LoadTask { task_id } => {
                spawn_task_load(client.clone(), updates.clone(), task_id);
            }
            Command::LoadTaskEvents { task_id, after } => {
                spawn_events_load(client.clone(), updates.clone(), task_id, after);
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
            Command::ElicitationReply {
                session_id,
                request_id,
                reply,
            } => {
                spawn_elicitation_reply(
                    client.clone(),
                    updates.clone(),
                    session_id,
                    request_id,
                    reply,
                );
            }
            Command::CancelTurn { session_id } => {
                spawn_cancel_turn(client.clone(), updates.clone(), session_id);
            }
            Command::Prompt { session_id, prompt } => {
                spawn_prompt(client.clone(), updates.clone(), session_id, prompt);
            }
            Command::LoadAgentSession { session_id } => {
                spawn_agent_load(client.clone(), updates.clone(), session_id);
            }
            Command::SetAgentOption {
                session_id,
                option_id,
                value,
            } => {
                spawn_agent_set_option(
                    client.clone(),
                    updates.clone(),
                    session_id,
                    option_id,
                    value,
                );
            }
            Command::SetAgentMode {
                session_id,
                mode_id,
            } => {
                spawn_agent_set_mode(client.clone(), updates.clone(), session_id, mode_id);
            }
            Command::Authenticate {
                session_id,
                method_id,
            } => {
                spawn_agent_authenticate(client.clone(), updates.clone(), session_id, method_id);
            }
            Command::Logout { session_id } => {
                spawn_agent_logout(client.clone(), updates.clone(), session_id);
            }
            Command::LoadWorkflowInspect { root_id } => {
                spawn_workflow_inspect(client.clone(), updates.clone(), root_id);
            }
            Command::LoadWorkflowGraph => {
                spawn_workflow_graph(client.clone(), updates.clone());
            }
            Command::CancelWorkflow { root_id } => {
                spawn_workflow_cancel(client.clone(), updates.clone(), root_id);
            }
            Command::RetryWorkflowTask { task_id } => {
                spawn_workflow_retry(client.clone(), updates.clone(), task_id);
            }
            Command::RefreshSchedules => {
                let _ = schedule_trigger.try_send(());
            }
            Command::UpsertSchedule {
                id,
                cron,
                task,
                input,
                enabled,
            } => {
                spawn_schedule_upsert(
                    client.clone(),
                    updates.clone(),
                    id,
                    cron,
                    task,
                    input,
                    enabled,
                );
            }
            Command::DeleteSchedule { id } => {
                spawn_schedule_delete(client.clone(), updates.clone(), id);
            }
            Command::RefreshRepos => {
                let _ = repo_trigger.try_send(());
            }
            Command::ActivateRepo { name } => {
                spawn_repo_activate(client.clone(), updates.clone(), name);
            }
            Command::TestConnection { endpoint, token } => {
                spawn_connection_test(updates.clone(), endpoint, token);
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
    agent: Option<String>,
    model: Option<String>,
) {
    tokio::spawn(async move {
        let result = client
            .tasks_start(TasksStartParams {
                name: name.clone(),
                input: Some(input),
                // The desktop client runs the task headlessly and renders its
                // structured events; the embedded TUI stays a TUI feature.
                interactive: None,
                agent,
                provider: None,
                model,
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
                mcp_servers: None,
            })
            .await
            .map(Box::new);
        let _ = updates.try_send(Update::OneshotFinished { result });
    });
}

/// Spawn one free-standing `agents.start` for the Agent panel.
fn spawn_start_agent(
    client: Arc<RpcClient>,
    updates: Sender<Update>,
    agent: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    cwd: Option<PathBuf>,
    new: bool,
) {
    let params = start_agent_params(agent, provider, model, cwd, new);
    tokio::spawn(async move {
        let result = client.agents_start(params).await.map(Box::new);
        let _ = updates.try_send(Update::AgentStartFinished { result });
    });
}

/// Build the free-standing `agents.start` params. No `task_id` is ever set: the
/// Agent panel starts a session in the active repo's main checkout, not a task
/// or a task worktree.
fn start_agent_params(
    agent: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    cwd: Option<PathBuf>,
    new: bool,
) -> StartAgentParams {
    StartAgentParams {
        agent,
        task_id: None,
        prompt: None,
        session_id: None,
        rows: None,
        cols: None,
        new: Some(new),
        provider,
        model,
        cwd,
        mcp_servers: None,
    }
}

/// Spawn one `agents.close` for a standalone session.
fn spawn_close_agent(client: Arc<RpcClient>, updates: Sender<Update>, session_id: String) {
    tokio::spawn(async move {
        let result = client
            .agents_close(SessionIdParams {
                session_id: session_id.clone(),
            })
            .await;
        let _ = updates.try_send(Update::AgentClosed { session_id, result });
    });
}

/// Drain Inbox refresh triggers into `tasks.list` + `agents.list` snapshots.
///
/// The trigger channel has capacity one and is drained before each fetch, so a
/// burst of `task.updated` / `agent.state` / `agent.event` pushes (or several
/// `r` presses) collapses into at most one follow-up load. That keeps the SSE
/// tick responsive without letting a chatty agent starve the daemon's RPC path.
///
/// Durable session history is never fetched here: a task's conversation is
/// restored on demand from its detail with the task-scoped `tasks.session`.
fn spawn_inbox_loader(client: Arc<RpcClient>, updates: Sender<Update>, triggers: Receiver<()>) {
    tokio::spawn(async move {
        while triggers.recv().await.is_ok() {
            // Fold every trigger already waiting into this load.
            while triggers.try_recv().is_ok() {}

            // The reads are independent; run them concurrently so the snapshot
            // lands within one UI tick on a local daemon.
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

/// Spawn one `tasks.delete`; the reply removes the row for good or triggers the
/// rollback snapshot on failure.
fn spawn_delete_task(client: Arc<RpcClient>, updates: Sender<Update>, task_id: Uuid) {
    tokio::spawn(async move {
        let result = client.tasks_delete(TaskIdParams { id: task_id }).await;
        let _ = updates.try_send(Update::TaskDeleteFinished { task_id, result });
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

/// Spawn one `tasks.session` lookup for the open detail.
///
/// This is a single indexed, task-scoped read. It never lists global or
/// provider-owned histories.
fn spawn_task_session(
    client: Arc<RpcClient>,
    updates: Sender<Update>,
    task_id: Uuid,
    request_id: u64,
) {
    tokio::spawn(async move {
        let result = client.tasks_session(TaskIdParams { id: task_id }).await;
        let _ = updates.try_send(Update::TaskSessionLoaded {
            task_id,
            request_id,
            result,
        });
    });
}

/// Spawn the structured-event backfill.
///
/// The daemon caps one page at 2000 events, so a long-running session is paged
/// forward from `seq` zero. Ten pages (20 000 events) is the ceiling: the detail
/// surface is a live view, and a run with more history than that is better
/// served by the persisted output. The pages are merged into one update so the
/// UI never renders a half-filled gap.
fn spawn_events_load(client: Arc<RpcClient>, updates: Sender<Update>, task_id: Uuid, after: u64) {
    tokio::spawn(async move {
        const PAGE: u64 = 2000;
        const MAX_PAGES: usize = 10;

        let mut session_id: Option<String> = None;
        let mut events = Vec::new();
        let mut ignored = 0usize;
        let mut next_cursor = after;
        let mut after = after;
        let mut plan = None;
        let mut commands = None;

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
                    if page.plan.is_some() {
                        plan = page.plan.clone();
                    }
                    if page.commands.is_some() {
                        commands = page.commands.clone();
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
            plan,
            commands,
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

/// Spawn one `agents.reply_elicitation` for a correlated form.
fn spawn_elicitation_reply(
    client: Arc<RpcClient>,
    updates: Sender<Update>,
    session_id: String,
    request_id: String,
    reply: ElicitationReply,
) {
    tokio::spawn(async move {
        let result = client
            .agents_reply_elicitation(AgentElicitationReplyParams {
                session_id: session_id.clone(),
                request_id: request_id.clone(),
                reply: reply.clone(),
            })
            .await;
        let _ = updates.try_send(Update::ElicitationReplyFinished {
            session_id,
            request_id,
            reply,
            result,
        });
    });
}

/// Spawn one `agents.cancel` for a live session's in-flight turn.
fn spawn_cancel_turn(client: Arc<RpcClient>, updates: Sender<Update>, session_id: String) {
    tokio::spawn(async move {
        let result = client
            .agents_cancel(AgentCancelParams {
                session_id: session_id.clone(),
            })
            .await;
        let _ = updates.try_send(Update::TurnCancelFinished { session_id, result });
    });
}

/// Spawn one `agents.prompt` for a follow-up prompt.
fn spawn_prompt(
    client: Arc<RpcClient>,
    updates: Sender<Update>,
    session_id: String,
    prompt: String,
) {
    tokio::spawn(async move {
        let result = client
            .agents_prompt(AgentPromptParams {
                session_id: session_id.clone(),
                prompt: prompt.clone(),
            })
            .await;
        let _ = updates.try_send(Update::PromptFinished {
            session_id,
            prompt,
            result,
        });
    });
}

/// Spawn one `agents.load` for a persisted session identified by its
/// Favetto-local id.
fn spawn_agent_load(client: Arc<RpcClient>, updates: Sender<Update>, session_id: String) {
    tokio::spawn(async move {
        let result = client
            .agents_load(SessionIdParams {
                session_id: session_id.clone(),
            })
            .await
            .map(Box::new);
        let _ = updates.try_send(Update::AgentSessionLoaded { session_id, result });
    });
}

/// Spawn one `agents.set_option` for the live session.
fn spawn_agent_set_option(
    client: Arc<RpcClient>,
    updates: Sender<Update>,
    session_id: String,
    option_id: String,
    value: SessionConfigValue,
) {
    tokio::spawn(async move {
        let result = client
            .agents_set_option(AgentSetOptionParams {
                session_id: session_id.clone(),
                option_id,
                value,
            })
            .await
            .map(Box::new);
        let _ = updates.try_send(Update::AgentOptionsChanged { session_id, result });
    });
}

/// Spawn one `agents.set_mode` for the live session.
fn spawn_agent_set_mode(
    client: Arc<RpcClient>,
    updates: Sender<Update>,
    session_id: String,
    mode_id: String,
) {
    tokio::spawn(async move {
        let result = client
            .agents_set_mode(AgentSetModeParams {
                session_id: session_id.clone(),
                mode_id,
            })
            .await
            .map(Box::new);
        let _ = updates.try_send(Update::AgentOptionsChanged { session_id, result });
    });
}

/// Spawn one `agents.authenticate` for an advertised agent-handled method.
fn spawn_agent_authenticate(
    client: Arc<RpcClient>,
    updates: Sender<Update>,
    session_id: String,
    method_id: String,
) {
    tokio::spawn(async move {
        let result = client
            .agents_authenticate(AgentAuthenticateParams {
                session_id: session_id.clone(),
                method_id: method_id.clone(),
            })
            .await
            .map(Box::new);
        let _ = updates.try_send(Update::AuthFinished {
            session_id,
            method_id,
            result,
        });
    });
}

/// Spawn one `agents.logout`, only reached after the UI saw the advertised
/// capability.
fn spawn_agent_logout(client: Arc<RpcClient>, updates: Sender<Update>, session_id: String) {
    tokio::spawn(async move {
        let result = client
            .agents_logout(AgentLogoutParams {
                session_id: session_id.clone(),
            })
            .await;
        let _ = updates.try_send(Update::LogoutFinished { session_id, result });
    });
}

/// Spawn one `workflow.inspect` for the selected root.
fn spawn_workflow_inspect(client: Arc<RpcClient>, updates: Sender<Update>, root_id: Uuid) {
    tokio::spawn(async move {
        let update = match client
            .workflow_inspect(WorkflowInspectParams { root_id })
            .await
        {
            Ok(view) => Update::WorkflowInspectLoaded {
                root_id,
                view: Box::new(view),
            },
            Err(error) => Update::WorkflowInspectFailed { root_id, error },
        };
        let _ = updates.try_send(update);
    });
}

/// Spawn one `workflow.get` for the catalog DAG source panel.
fn spawn_workflow_graph(client: Arc<RpcClient>, updates: Sender<Update>) {
    tokio::spawn(async move {
        let update = match client.workflow_get(EmptyParams {}).await {
            Ok(result) => Update::WorkflowGraphLoaded {
                result: Box::new(result),
            },
            Err(error) => Update::WorkflowGraphFailed { error },
        };
        let _ = updates.try_send(update);
    });
}

/// Spawn one root-scoped `workflow.cancel`.
fn spawn_workflow_cancel(client: Arc<RpcClient>, updates: Sender<Update>, root_id: Uuid) {
    tokio::spawn(async move {
        let result = client
            .workflow_cancel(WorkflowInspectParams { root_id })
            .await;
        let _ = updates.try_send(Update::WorkflowCancelFinished { root_id, result });
    });
}

/// Spawn one `workflow.retry` for a failed instance.
fn spawn_workflow_retry(client: Arc<RpcClient>, updates: Sender<Update>, task_id: Uuid) {
    tokio::spawn(async move {
        let result = client
            .workflow_retry(WorkflowRetryParams { task_id })
            .await
            .map(Box::new);
        let _ = updates.try_send(Update::WorkflowRetryFinished { task_id, result });
    });
}

/// Drain schedule refresh triggers into one `schedules.list`, coalescing a
/// burst of cron ticks/saves into a single round trip.
fn spawn_schedule_loader(client: Arc<RpcClient>, updates: Sender<Update>, triggers: Receiver<()>) {
    tokio::spawn(async move {
        while triggers.recv().await.is_ok() {
            while triggers.try_recv().is_ok() {}
            let update = match client.schedules_list(EmptyParams {}).await {
                Ok(schedules) => Update::SchedulesLoaded { schedules },
                Err(error) => Update::SchedulesFailed { error },
            };
            let _ = updates.try_send(update);
        }
    });
}

/// Spawn one `schedules.upsert`; the daemon computes `next_run` on the result.
fn spawn_schedule_upsert(
    client: Arc<RpcClient>,
    updates: Sender<Update>,
    id: Option<String>,
    cron: String,
    task: String,
    input: Option<serde_json::Value>,
    enabled: bool,
) {
    tokio::spawn(async move {
        let result = client
            .schedules_upsert(ScheduleUpsertParams {
                id,
                cron,
                task,
                input,
                enabled: Some(enabled),
            })
            .await
            .map(Box::new);
        let _ = updates.try_send(Update::ScheduleSaved { result });
    });
}

/// Spawn one `schedules.delete`.
fn spawn_schedule_delete(client: Arc<RpcClient>, updates: Sender<Update>, id: String) {
    tokio::spawn(async move {
        let result = client
            .schedules_delete(ScheduleDeleteParams { id: id.clone() })
            .await;
        let _ = updates.try_send(Update::ScheduleDeleted { id, result });
    });
}

/// Drain repo refresh triggers into one `repos.list`, coalescing a burst of
/// `catalog.updated` pushes from other clients into a single round trip.
fn spawn_repo_loader(client: Arc<RpcClient>, updates: Sender<Update>, triggers: Receiver<()>) {
    tokio::spawn(async move {
        while triggers.recv().await.is_ok() {
            while triggers.try_recv().is_ok() {}
            let result = client.repos_list(EmptyParams {}).await;
            let _ = updates.try_send(Update::ReposLoaded { result });
        }
    });
}

/// Spawn one `repos.activate`; the result carries the updated registry so the
/// bar and picker update without a reload.
fn spawn_repo_activate(client: Arc<RpcClient>, updates: Sender<Update>, name: Option<String>) {
    tokio::spawn(async move {
        let result = client.repos_activate(ReposActivateParams { name }).await;
        let _ = updates.try_send(Update::RepoActivated { result });
    });
}

/// Spawn a `system.ping` against a draft client, leaving the live connection
/// untouched (a bad draft can never take Settings offline).
fn spawn_connection_test(updates: Sender<Update>, endpoint: String, token: Option<String>) {
    tokio::spawn(async move {
        let started = Instant::now();
        let result = match RpcClient::from_parts(&endpoint, token.as_deref()) {
            Ok(client) => {
                match tokio::time::timeout(Duration::from_secs(5), client.ping(PingParams {})).await
                {
                    Ok(result) => result,
                    // A silent/black-holed address must not leave "Testing…" up
                    // forever; report the deadline like any other transport error.
                    Err(_) => Err(ClientError::Network(
                        "the connection test timed out after 5 s".to_string(),
                    )),
                }
            }
            Err(error) => Err(error),
        };
        let _ = updates.try_send(Update::ConnectionTestFinished {
            endpoint,
            result,
            round_trip: started.elapsed(),
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
    fn a_free_standing_start_sends_the_checkout_and_no_task() {
        let params = start_agent_params(
            Some("opencode".to_string()),
            Some("opencode-go".to_string()),
            Some("deepseek".to_string()),
            Some(PathBuf::from("/code/favetto")),
            true,
        );
        assert_eq!(params.task_id, None);
        assert_eq!(params.agent.as_deref(), Some("opencode"));
        assert_eq!(params.provider.as_deref(), Some("opencode-go"));
        assert_eq!(params.model.as_deref(), Some("deepseek"));
        assert_eq!(params.cwd, Some(PathBuf::from("/code/favetto")));
        assert_eq!(params.new, Some(true));
        assert!(params.prompt.is_none());
        assert!(params.session_id.is_none());
    }

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
