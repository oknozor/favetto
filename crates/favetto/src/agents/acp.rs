//! Generic Agent Client Protocol runtime.
//!
//! One implementation drives every ACP server: Favetto launches the configured
//! command, speaks JSON-RPC over its stdin/stdout, and normalizes the agent's
//! structured updates into [`AgentStateEvent`]s. There is no per-vendor adapter:
//! choosing another agent is a configuration change, not code.
//!
//! The daemon owns the child process (not the SDK's process helper) so the same
//! supervision, cancellation and process-group teardown used by PTY sessions
//! applies here too.
//!
//! A *turn* (`session/prompt`) is the unit of task execution. A headless session
//! closes itself once its first turn finishes; an interactive session stays open
//! and accepts follow-up prompts through [`AcpHandles::prompts`]. Task success is
//! deliberately stricter than ACP's `stop_reason`: see [`AcpOutcome::succeeded`].

use std::collections::BTreeMap;
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::schema::{
    AgentCapabilities, CancelNotification, ContentBlock, EmbeddedResourceResource,
    InitializeRequest, LoadSessionRequest, NewSessionRequest, PermissionOptionKind, PromptRequest,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionConfigOption, SessionId, SessionNotification, SessionUpdate,
    TextContent, ToolCall as AcpToolCall, ToolCallContent as AcpToolCallContent,
    ToolCallLocation as AcpToolCallLocation, ToolCallStatus as AcpToolCallStatus,
    ToolCallUpdateFields as AcpToolCallUpdateFields, ToolKind as AcpToolKind,
};
use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::{Agent, Client, Lines};
use futures_util::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use futures_util::StreamExt;
use parking_lot::Mutex;
use tokio::sync::{mpsc, oneshot};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use favetto_core::model::{
    AgentLoadState, AgentStateEvent, AwaitingInputKind, InputReply, InputRequest, MessageRole,
    PermissionPolicy, ToolCallDetail, ToolCallStatus, ToolContent, ToolKind, ToolLocation,
};

use super::session_store::SessionBinding;
use super::SessionLifecycle;

/// Deadline for an `ask`-policy permission answered by a remote client.
pub(crate) const PERMISSION_TIMEOUT: Duration = Duration::from_secs(300);

/// How long to wait for `session/close` before ending the session anyway. The
/// method is optional in practice, and a completed turn must never leave the
/// task `running`.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

/// Whether the initial ACP setup creates a session or restores a known one.
#[derive(Debug, Clone)]
pub(crate) enum AcpSessionMode {
    /// Create a new session with `session/new`.
    New,
    /// Restore a session Favetto already knows by its agent-assigned ACP id.
    Load { external_session_id: String },
}

/// What to launch and how to drive it.
pub(crate) struct AcpLaunch {
    /// The configured agent name, used in diagnostics.
    pub agent: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: PathBuf,
    /// Create a new ACP session or load an existing one.
    pub mode: AcpSessionMode,
    /// How permission requests that reach the client are answered.
    pub policy: PermissionPolicy,
    /// The `provider/model` the task requested, applied through the agent's
    /// model config option when it exposes one.
    pub model: Option<String>,
    /// A prompt to send as soon as the session is ready.
    pub prompt: Option<String>,
    /// Close the session (and kill the server) after the first turn.
    pub close_after_turn: bool,
    /// How long an `ask` request waits for a client answer.
    pub permission_timeout: Duration,
    /// Durable metadata writer for the Favetto-local session record.
    pub session: Option<SessionBinding>,
    /// For a load, wait for the manager to register the local session before
    /// issuing `session/load`. Replay notifications must have a stable owner
    /// (and a snapshot clients can attach to) before they can arrive.
    pub start_gate: Option<oneshot::Receiver<()>>,
    /// The restore lifecycle shared with the manager's session snapshot.
    pub lifecycle: Option<Arc<Mutex<SessionLifecycle>>>,
}

/// Internal messages from the ACP runtime to the manager's state task.
///
/// Only [`AcpEvent::State`] is wire-visible. The marker keeps readiness ordered
/// behind replay: the runtime waits for the state task to fold every replayed
/// update into the feed (and event sequence) before reporting the load complete.
#[derive(Debug)]
pub(crate) enum AcpEvent {
    State(AgentStateEvent),
    /// Replay is fully emitted; acknowledge once it is in the feed.
    ReplayApplied(oneshot::Sender<()>),
}

/// How replay notifications are handled before `session/load` settles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplayPhase {
    /// Hold updates; nothing is exposed to clients yet.
    Buffering,
    /// The load succeeded; updates can be delivered in order.
    Forwarding,
    /// The load failed; discard buffered updates instead of exposing a partial
    /// transcript as if the restore had succeeded.
    Discarded,
}

#[derive(Debug)]
struct ReplayGate {
    phase: ReplayPhase,
    buffered: Vec<SessionNotification>,
}

impl ReplayGate {
    fn new() -> Self {
        Self {
            phase: ReplayPhase::Buffering,
            buffered: Vec::new(),
        }
    }

    /// Stop buffering and hand over the replay in arrival order.
    fn take_buffered(&mut self) -> Vec<SessionNotification> {
        self.phase = ReplayPhase::Forwarding;
        std::mem::take(&mut self.buffered)
    }

    /// Drop any buffered replay and ignore later updates.
    fn discard(&mut self) {
        self.phase = ReplayPhase::Discarded;
        self.buffered.clear();
    }
}

/// Commands that do not need the prompt turn to be in flight.
#[derive(Debug)]
pub(crate) enum AcpCommand {
    /// Interrupt the current turn (`session/cancel`).
    Cancel,
    /// Close the session and stop the server.
    Shutdown,
}

/// Cancellation state for the one prompt turn that can be in flight at a time.
///
/// ACP sessions are long-lived and accept successive prompts, so cancellation
/// must be scoped to a turn rather than to the connection. `generation` changes
/// at every turn boundary; permission requests carry the generation that was
/// active when they arrived, which lets the cancel path settle exactly the
/// requests that belong to the cancelled turn.
#[derive(Debug, Default)]
struct TurnState {
    generation: u64,
    active: bool,
    cancel_requested: bool,
}

impl TurnState {
    /// Start a new turn. Cancellation from the previous turn never leaks into
    /// the next one.
    fn begin(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.active = true;
        self.cancel_requested = false;
        self.generation
    }

    /// The turn finished (or failed): later cancels must not send another
    /// notification until a new prompt starts.
    fn finish(&mut self) {
        self.active = false;
        self.cancel_requested = false;
    }

    /// Claim cancellation for the active turn. Returns the generation to settle,
    /// or `None` when the connection is idle or this turn was already cancelled.
    fn request_cancel(&mut self) -> Option<u64> {
        if !self.active || self.cancel_requested {
            return None;
        }
        self.cancel_requested = true;
        Some(self.generation)
    }

    /// Whether `generation` is the active turn and already cancelled. A
    /// permission request that arrives during cancellation is answered
    /// `cancelled` immediately instead of blocking until the timeout.
    fn is_cancelled(&self, generation: u64) -> bool {
        self.active && self.cancel_requested && self.generation == generation
    }
}

/// The structured result of the session's turns so far.
#[derive(Debug, Default, Clone)]
pub struct AcpOutcome {
    /// The last turn's ACP stop reason, e.g. `end_turn`.
    pub stop_reason: Option<String>,
    /// The first fatal error (protocol, transport, or agent-reported).
    pub error: Option<String>,
    /// Whether at least one turn ran to a stop reason.
    pub finished: bool,
    /// Assistant text accumulated across the session.
    pub assistant_text: String,
    pub tool_calls: usize,
    pub failed_tools: usize,
}

impl AcpOutcome {
    /// Whether the run counts as a success.
    ///
    /// Agreed rule: `stop_reason` alone is not enough. Some adapters report
    /// `end_turn` after an upstream model failure (observed with pi-acp), so a
    /// run succeeds only when the turn ended normally, no error was recorded,
    /// and the agent actually produced assistant text.
    pub fn succeeded(&self) -> bool {
        self.finished
            && self.error.is_none()
            && self.stop_reason.as_deref() == Some("end_turn")
            && !self.assistant_text.trim().is_empty()
    }

    /// A one-line explanation for a failed run, for the task failure record.
    pub fn failure_reason(&self, agent: &str) -> String {
        if let Some(error) = &self.error {
            return format!("agent '{agent}' failed: {error}");
        }
        if !self.finished {
            return format!("agent '{agent}' ended without completing its turn");
        }
        match self.stop_reason.as_deref() {
            Some("end_turn") if self.assistant_text.trim().is_empty() => {
                format!("agent '{agent}' finished its turn without producing an answer")
            }
            Some(reason) => format!("agent '{agent}' stopped with '{reason}'"),
            None => format!("agent '{agent}' did not report a stop reason"),
        }
    }
}

/// Handles to a live ACP session.
pub(crate) struct AcpHandles {
    pub prompts: mpsc::UnboundedSender<String>,
    pub commands: mpsc::UnboundedSender<AcpCommand>,
    pub responder: Arc<AcpResponder>,
    pub running: Arc<AtomicBool>,
    pub exit_code: Arc<AtomicI32>,
    pub external_session_id: Arc<Mutex<Option<String>>>,
    /// Capabilities advertised during `initialize`, retained for callers that
    /// need to decide whether an operation is supported.
    #[allow(dead_code)] // read by later session-history steps and capability-aware callers
    pub capabilities: Arc<Mutex<Option<AgentCapabilities>>>,
    /// Completion of a load-mode startup, when `mode` was [`AcpSessionMode::Load`].
    /// `None` means the load has not settled yet; `Ok` is sent only after the
    /// agent acknowledged `session/load` and the complete replay has been applied
    /// to the feed. The receiver closes if the server exits before either way.
    pub load: Option<tokio::sync::watch::Receiver<Option<Result<(), String>>>>,
    pub last_activity: Arc<Mutex<Instant>>,
    /// Plain-text transcript (assistant text plus tool summaries) used as the
    /// run's captured output.
    pub transcript: Arc<Mutex<String>>,
    pub outcome: Arc<Mutex<AcpOutcome>>,
    /// The connection task. Aborting it drops the child handle, whose
    /// `kill_on_drop` setting terminates the server.
    task: tokio::task::JoinHandle<()>,
}

impl AcpHandles {
    /// Close the session: request a clean stop, then abort the connection task
    /// so a stuck server is killed rather than leaking.
    pub(crate) fn shutdown(&self) {
        let _ = self.commands.send(AcpCommand::Shutdown);
        self.task.abort();
    }
}

/// Spawn an ACP server and drive one session.
///
/// Returns as soon as the child is running: initialization, session creation and
/// the initial prompt happen in the background, and their failures surface as
/// state events plus a non-zero exit code. This matches the PTY backend, whose
/// `start` is also synchronous.
pub(crate) fn spawn(
    launch: AcpLaunch,
) -> anyhow::Result<(AcpHandles, mpsc::UnboundedReceiver<AcpEvent>)> {
    if tokio::runtime::Handle::try_current().is_err() {
        anyhow::bail!("ACP sessions require the daemon's async runtime");
    }

    // A missing working directory makes `execve` fail with ENOENT too, which
    // would otherwise be reported as if the program were missing. Name the
    // directory explicitly: a stale repo/cwd is the usual cause.
    if !launch.cwd.is_dir() {
        anyhow::bail!(
            "agent '{}': working directory '{}' does not exist or is not a directory;              check the task's `cwd`/repo path",
            launch.agent,
            launch.cwd.display()
        );
    }

    let mut command = tokio::process::Command::new(&launch.program);
    command
        .args(&launch.args)
        .envs(&launch.env)
        .current_dir(&launch.cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(|e| {
        anyhow::anyhow!(
            "agent '{}': cannot launch ACP server '{}' in '{}': {e}",
            launch.agent,
            launch.program.display(),
            launch.cwd.display()
        )
    })?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("ACP server stdin is unavailable"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("ACP server stdout is unavailable"))?;
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr.compat()).lines();
            while let Some(Ok(line)) = lines.next().await {
                tracing::debug!(target: "favetto::acp", "{line}");
            }
        });
    }

    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let (prompts_tx, mut prompts_rx) = mpsc::unbounded_channel();
    let (commands_tx, mut commands_rx) = mpsc::unbounded_channel();

    let running = Arc::new(AtomicBool::new(true));
    let exit_code = Arc::new(AtomicI32::new(-1));
    let external_session_id = Arc::new(Mutex::new(None));
    let capabilities = Arc::new(Mutex::new(None));
    let last_activity = Arc::new(Mutex::new(Instant::now()));
    let transcript = Arc::new(Mutex::new(String::new()));
    let outcome = Arc::new(Mutex::new(AcpOutcome::default()));
    let tools = Arc::new(Mutex::new(ToolTracker::default()));
    let turn = Arc::new(Mutex::new(TurnState::default()));

    let responder = Arc::new(AcpResponder {
        policy: launch.policy,
        events: events_tx.clone(),
        pending: Mutex::new(HashMap::new()),
        timeout: launch.permission_timeout,
        turn: turn.clone(),
    });

    let transport = Lines::new(sink_from_stdin(stdin), lossy_lines(stdout.compat()));

    let notification_events = events_tx.clone();
    let notification_transcript = transcript.clone();
    let notification_outcome = outcome.clone();
    let notification_tools = tools.clone();
    let replay_transcript = transcript.clone();
    let replay_outcome = outcome.clone();
    let replay_tools = tools.clone();
    let permission_responder = responder.clone();

    let permission_events = events_tx.clone();
    // Loading restores a conversation; it must never submit a prompt as part of
    // the restore, even if one was supplied to the launch descriptor.
    let initial_prompt = match &launch.mode {
        AcpSessionMode::New => launch.prompt.clone(),
        AcpSessionMode::Load { .. } => None,
    };
    let requested_model = launch.model.clone();
    let agent_name = launch.agent.clone();
    let close_after_turn = launch.close_after_turn;
    let cwd = launch.cwd.clone();
    let running_task = running.clone();
    let exit_task = exit_code.clone();
    let session_id_slot = external_session_id.clone();
    let ready_binding = launch.session.clone();
    let end_binding = launch.session;
    let mut start_gate = launch.start_gate;
    let lifecycle = launch.lifecycle.clone();

    let result_outcome = outcome.clone();
    let result_events = events_tx.clone();
    let connection_outcome = outcome.clone();
    let capability_slot = capabilities.clone();
    let prompt_turn = turn.clone();
    let control_responder = responder.clone();
    let mode = launch.mode;
    let (load_tx, load_rx) = match &mode {
        AcpSessionMode::Load { .. } => {
            let (tx, rx) = tokio::sync::watch::channel(None);
            (Some(tx), Some(rx))
        }
        AcpSessionMode::New => (None, None),
    };
    // A load's replay arrives before `session/load` returns. Hold it until the
    // response proves the restore succeeded, so a failure cannot expose a
    // partial transcript.
    let replay_gate = match &mode {
        AcpSessionMode::Load { .. } => Some(Arc::new(Mutex::new(ReplayGate::new()))),
        AcpSessionMode::New => None,
    };
    // The terminal error path must be able to fail the load even when setup
    // aborted before reaching the explicit load-request branch.
    let failure_mode = mode.clone();
    let failure_lifecycle = lifecycle.clone();
    let failure_load_tx = load_tx.clone();
    let failure_replay = replay_gate.clone();
    let notification_replay = replay_gate.clone();
    let mut child = child;
    let task =
        tokio::spawn(async move {
            let notification_transcript = Arc::clone(&notification_transcript);
            let result = Client
            .v1()
            .on_receive_notification(
                async move |notification: SessionNotification, _cx| {
                    // While a restore is in flight, buffer raw updates rather
                    // than folding them into the transcript: readiness must be
                    // published only after the whole replay is applied.
                    if let Some(replay) = &notification_replay {
                        let mut replay = replay.lock();
                        match replay.phase {
                            ReplayPhase::Buffering => {
                                replay.buffered.push(notification);
                                return Ok(());
                            }
                            ReplayPhase::Discarded => return Ok(()),
                            ReplayPhase::Forwarding => {}
                        }
                    }
                    emit_update(
                        &notification.update,
                        &notification_events,
                        &notification_transcript,
                        &notification_outcome,
                        &notification_tools,
                    );
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_request(
                async move |request: RequestPermissionRequest, responder_handle, _cx| {
                    let _ = &permission_events;
                    let response = permission_responder.decide(&request).await;
                    let _ = responder_handle.respond(response);
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(
                transport,
                move |connection: ConnectionTo<Agent>| async move {
                    let init = connection
                        .send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;
                    *capability_slot.lock() = Some(init.agent_capabilities.clone());
                    // `session/close` is optional. Only call it when the agent
                    // advertised `sessionCapabilities.close`; process teardown
                    // below is the cleanup path otherwise.
                    let close_supported =
                        init.agent_capabilities.session_capabilities.close.is_some();
                    tracing::debug!(
                        target: "favetto::acp",
                        agent = ?init.agent_info,
                        load_session = init.agent_capabilities.load_session,
                        close_supported,
                        "ACP agent initialized"
                    );

                    // The manager registers the local session before the gate
                    // opens, so replay notifications and lifecycle pushes have a
                    // stable owner and snapshot from their first update.
                    if let Some(gate) = start_gate.take() {
                        if gate.await.is_err() {
                            let message = format!(
                                "agent '{agent_name}': session registration ended before the \
                                 restore could start"
                            );
                            fail_load(&lifecycle, &load_tx, &message);
                            let _ = events_tx.send(AcpEvent::State(AgentStateEvent::Error {
                                message: message.clone(),
                            }));
                            return Err(anyhow::anyhow!(message).into());
                        }
                    }

                    // Session setup differs only here: a new conversation is
                    // created, while a load restores the agent's own session id.
                    // A load never falls back to `session/new`, because a fresh
                    // session would be presented as a restored conversation.
                    let (session_id, external_session_id, chosen_model) = match &mode {
                        AcpSessionMode::Load {
                            external_session_id: known,
                        } => {
                            if !init.agent_capabilities.load_session {
                                let message = format!(
                                    "agent '{agent_name}' does not support loading ACP sessions \
                                     (session/load); cannot restore session '{known}'"
                                );
                                fail_load(&lifecycle, &load_tx, &message);
                                let _ = events_tx.send(AcpEvent::State(AgentStateEvent::Error {
                                    message: message.clone(),
                                }));
                                return Err(anyhow::anyhow!(message).into());
                            }
                            let loaded = connection
                                .send_request(LoadSessionRequest::new(
                                    SessionId::new(known.clone()),
                                    cwd.clone(),
                                ))
                                .block_task()
                                .await
                                .map_err(|error| {
                                    let message = format!(
                                        "agent '{agent_name}' could not load ACP session \
                                         '{known}': {error}"
                                    );
                                    fail_load(&lifecycle, &load_tx, &message);
                                    let _ = events_tx.send(AcpEvent::State(AgentStateEvent::Error {
                                        message: message.clone(),
                                    }));
                                    anyhow::anyhow!(message)
                                })?;
                            // The local id is the routing key; the ACP id stays
                            // separate metadata, unchanged by the restore.
                            if let Some(binding) = &ready_binding {
                                if let Err(error) = binding
                                    .store
                                    .mark_ready(&binding.id, known, binding.run_id.as_deref())
                                    .await
                                {
                                    let message = format!(
                                        "agent '{agent_name}': cannot record loaded ACP session \
                                         id: {error}"
                                    );
                                    fail_load(&lifecycle, &load_tx, &message);
                                    let _ = events_tx.send(AcpEvent::State(AgentStateEvent::Error {
                                        message: message.clone(),
                                    }));
                                    return Err(anyhow::anyhow!(message).into());
                                }
                            }
                            *session_id_slot.lock() = Some(known.clone());
                            (
                                SessionId::new(known.clone()),
                                known.clone(),
                                current_model(loaded.config_options.as_deref()),
                            )
                        }
                        AcpSessionMode::New => {
                            let session = connection
                                .send_request(NewSessionRequest::new(cwd.clone()))
                                .block_task()
                                .await?;
                            let session_id = session.session_id.clone();
                            let external_session_id = session_id.0.to_string();
                            // Record the agent's identity immediately: a task
                            // run can be finalized (or the daemon can crash)
                            // long before the first turn ends. A write fault
                            // aborts the session rather than letting an
                            // untracked conversation continue.
                            if let Some(binding) = &ready_binding {
                                if let Err(error) = binding
                                    .store
                                    .mark_ready(
                                        &binding.id,
                                        &external_session_id,
                                        binding.run_id.as_deref(),
                                    )
                                    .await
                                {
                                    let message = format!(
                                        "agent '{agent_name}': cannot record ACP session id: \
                                         {error}"
                                    );
                                    let _ = events_tx.send(AcpEvent::State(AgentStateEvent::Error {
                                        message: message.clone(),
                                    }));
                                    return Err(anyhow::anyhow!(message).into());
                                }
                            }
                            // Expose the id to the manager even if model
                            // selection fails below, so a failed run still
                            // reports its identity.
                            *session_id_slot.lock() = Some(external_session_id.clone());
                            // A task names its model explicitly; apply it
                            // instead of relying on the agent's last-used
                            // default (which may be stale or unavailable on
                            // this account).
                            let chosen_model = if let Some(wanted) = &requested_model {
                                match apply_model(&connection, &session, wanted).await {
                                    Ok(model) => Some(model),
                                    Err(error) => {
                                        let message = format!("agent '{agent_name}': {error}");
                                        let _ = events_tx.send(AcpEvent::State(
                                            AgentStateEvent::Error {
                                                message: message.clone(),
                                            },
                                        ));
                                        return Err(anyhow::anyhow!(message).into());
                                    }
                                }
                            } else {
                                current_model(session.config_options.as_deref())
                            };
                            (session_id, external_session_id, chosen_model)
                        }
                    };
                    // A successful load releases the buffered replay in arrival
                    // order, then waits for the manager to fold it into the feed
                    // before readiness is published. Nothing here runs for a
                    // failed load: its buffered updates are discarded.
                    if let Some(replay) = &replay_gate {
                        let mut replay = replay.lock();
                        let buffered = replay.take_buffered();
                        for notification in buffered {
                            emit_update(
                                &notification.update,
                                &events_tx,
                                &replay_transcript,
                                &replay_outcome,
                                &replay_tools,
                            );
                        }
                    }
                    let _ = events_tx.send(AcpEvent::State(AgentStateEvent::Session {
                        session_id: Some(external_session_id),
                        title: None,
                        model: chosen_model,
                    }));
                    if replay_gate.is_some() {
                        let (applied_tx, applied_rx) = oneshot::channel();
                        let _ = events_tx.send(AcpEvent::ReplayApplied(applied_tx));
                        // The state task acknowledges after the replay has a
                        // sequence and is in the feed, so attach snapshots and
                        // `tasks.events` cursors agree with this readiness.
                        let _ = applied_rx.await;
                        if let Some(lifecycle) = &lifecycle {
                            let mut lifecycle = lifecycle.lock();
                            lifecycle.state = AgentLoadState::Ready;
                            lifecycle.error = None;
                        }
                    }
                    // A load is only reported as restored after the replay is
                    // applied and the record is ready.
                    if let Some(tx) = &load_tx {
                        let _ = tx.send(Some(Ok(())));
                    }

                    // A control task turns external commands into protocol
                    // notifications, so the prompt loop below stays linear.
                    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
                    let control_connection = connection.clone();
                    let control_session = session_id.clone();
                    let control_turn = prompt_turn.clone();
                    tokio::spawn(async move {
                        while let Some(command) = commands_rx.recv().await {
                            match command {
                                AcpCommand::Cancel => {
                                    let Some(generation) = control_turn.lock().request_cancel()
                                    else {
                                        continue;
                                    };
                                    // ACP requires every pending permission request
                                    // for the cancelled turn to be answered
                                    // `cancelled` before the agent is left to
                                    // unwind. Settle them first, then notify.
                                    let settled = control_responder.cancel_turn(generation);
                                    tracing::debug!(
                                        target: "favetto::acp",
                                        generation,
                                        settled,
                                        "cancelling ACP turn"
                                    );
                                    let _ = control_connection.send_notification(
                                        CancelNotification::new(control_session.clone()),
                                    );
                                }
                                AcpCommand::Shutdown => {
                                    let _ = control_connection.send_notification(
                                        CancelNotification::new(control_session.clone()),
                                    );
                                    let _ = shutdown_tx.send(true);
                                    break;
                                }
                            }
                        }
                    });

                    let mut pending = initial_prompt;
                    let mut turns = 0u32;
                    loop {
                        let prompt = match pending.take() {
                            Some(prompt) => prompt,
                            None => {
                                if close_after_turn && turns > 0 {
                                    break;
                                }
                                tokio::select! {
                                    prompt = prompts_rx.recv() => match prompt {
                                        Some(prompt) => prompt,
                                        None => break,
                                    },
                                    _ = shutdown_rx.changed() => break,
                                }
                            }
                        };

                        // A new prompt is a new turn: cancellation reset here,
                        // never at connection lifetime.
                        prompt_turn.lock().begin();
                        let response = connection
                            .send_request(PromptRequest::new(
                                session_id.clone(),
                                vec![ContentBlock::Text(TextContent::new(prompt))],
                            ))
                            .block_task()
                            .await;
                        turns += 1;
                        // The turn boundary closes immediately when the prompt
                        // response arrives (or the request fails), so a cancel
                        // that races the end cannot poison the next turn.
                        prompt_turn.lock().finish();

                        let (stop_reason, turn_outcome) = match response {
                            Ok(response) => {
                                let reason = stop_reason_name(&response.stop_reason).to_string();
                                let idle = match response.stop_reason {
                                    agent_client_protocol::v1::schema::StopReason::EndTurn => {
                                        favetto_core::model::IdleOutcome::Succeeded
                                    }
                                    agent_client_protocol::v1::schema::StopReason::Cancelled => {
                                        favetto_core::model::IdleOutcome::Interrupted
                                    }
                                    _ => favetto_core::model::IdleOutcome::Failed,
                                };
                                {
                                    let mut state = connection_outcome.lock();
                                    state.stop_reason = Some(reason.clone());
                                    state.finished = true;
                                }
                                (Some(reason), idle)
                            }
                            Err(error) => {
                                let message = error.to_string();
                                {
                                    let mut state = connection_outcome.lock();
                                    state.error = Some(message.clone());
                                    state.stop_reason = None;
                                }
                                let _ = events_tx.send(AcpEvent::State(AgentStateEvent::Error {
                                    message: message.clone(),
                                }));
                                (None, favetto_core::model::IdleOutcome::Failed)
                            }
                        };

                        let _ = events_tx.send(AcpEvent::State(AgentStateEvent::Idle {
                            outcome: turn_outcome,
                        }));
                        tracing::debug!(target: "favetto::acp", ?stop_reason, "ACP turn finished");

                        if close_after_turn || *shutdown_rx.borrow() {
                            break;
                        }
                    }

                    // Closing is best-effort and optional in ACP v1: only ask
                    // when the agent advertised support. Even then an agent may
                    // ignore the request, so bound the wait; the child process
                    // is killed and reaped below regardless.
                    if close_supported {
                        let close = connection
                            .send_request(
                                agent_client_protocol::v1::schema::CloseSessionRequest::new(
                                    session_id,
                                ),
                            )
                            .block_task();
                        if tokio::time::timeout(CLOSE_TIMEOUT, close).await.is_err() {
                            tracing::debug!(
                                target: "favetto::acp",
                                agent = %agent_name,
                                "agent did not acknowledge session/close; ending the session anyway"
                            );
                        }
                    } else {
                        tracing::debug!(
                            target: "favetto::acp",
                            agent = %agent_name,
                            "agent does not advertise session/close; ending the session without it"
                        );
                    }
                    Ok(())
                },
            )
            .await;

            match result {
                Ok(()) => {
                    if exit_task.load(Ordering::SeqCst) < 0 {
                        exit_task.store(0, Ordering::SeqCst);
                    }
                }
                Err(error) => {
                    let message = error.to_string();
                    // Setup can fail before reaching the explicit load branches
                    // (initialize, transport, registration). A load that has not
                    // succeeded must expose the failure, not a partial transcript.
                    if matches!(failure_mode, AcpSessionMode::Load { .. }) {
                        if let Some(replay) = &failure_replay {
                            replay.lock().discard();
                        }
                        fail_load(&failure_lifecycle, &failure_load_tx, &message);
                    }
                    {
                        let mut state = result_outcome.lock();
                        if state.error.is_none() {
                            state.error = Some(message.clone());
                        }
                    }
                    let _ = result_events.send(AcpEvent::State(AgentStateEvent::Error { message }));
                    exit_task.store(1, Ordering::SeqCst);
                }
            }
            // Record the terminal state before the session reports itself stopped, so
            // a waiter (and the headless executor) never observes a finished session
            // without its durable outcome. The row itself is kept for history.
            if let Some(binding) = &end_binding {
                let error = result_outcome.lock().error.clone();
                let recorded = match error.as_deref() {
                    Some(error) => binding.store.mark_failed(&binding.id, error).await,
                    None => binding.store.mark_ended(&binding.id).await,
                };
                if let Err(write) = recorded {
                    tracing::warn!(
                        target: "favetto::acp",
                        session = %binding.id,
                        error = %write,
                        "failed to record the terminal session state"
                    );
                }
            }
            running_task.store(false, Ordering::SeqCst);
            // Kill and reap the server: a clean `close_session` normally ends it,
            // but a stuck or crashed server must not outlive the daemon or become a
            // zombie.
            let _ = child.start_kill();
            let _ = child.wait().await;
        });

    Ok((
        AcpHandles {
            prompts: prompts_tx,
            commands: commands_tx,
            responder,
            running,
            exit_code,
            external_session_id,
            capabilities,
            load: load_rx,
            last_activity,
            transcript,
            outcome,
            task,
        },
        events_rx,
    ))
}

/// Answers permission requests according to the session's policy.
pub(crate) struct AcpResponder {
    policy: PermissionPolicy,
    events: mpsc::UnboundedSender<AcpEvent>,
    pending: Mutex<HashMap<String, PendingPermission>>,
    timeout: Duration,
    turn: Arc<Mutex<TurnState>>,
}

/// One outstanding `ask` permission request, tagged with the turn that owns it.
struct PendingPermission {
    generation: u64,
    reply: oneshot::Sender<InputReply>,
}

impl AcpResponder {
    async fn decide(&self, request: &RequestPermissionRequest) -> RequestPermissionResponse {
        match self.policy {
            PermissionPolicy::Allow => Self::response_for(request, InputReply::Once),
            PermissionPolicy::Deny => Self::response_for(request, InputReply::Reject),
            PermissionPolicy::Ask => {
                let (generation, already_cancelled) = {
                    let turn = self.turn.lock();
                    (turn.generation, turn.is_cancelled(turn.generation))
                };
                if already_cancelled {
                    return Self::response_for(request, InputReply::Cancelled);
                }
                let id = format!("acp-perm-{}", request.tool_call.tool_call_id.0);
                let title = request
                    .tool_call
                    .fields
                    .title
                    .clone()
                    .or_else(|| request.tool_call.fields.name.clone())
                    .unwrap_or_else(|| "tool call".to_string());
                let (tx, rx) = oneshot::channel();
                self.pending.lock().insert(
                    id.clone(),
                    PendingPermission {
                        generation,
                        reply: tx,
                    },
                );
                let _ = self
                    .events
                    .send(AcpEvent::State(AgentStateEvent::InputRequested {
                        request: InputRequest {
                            id: id.clone(),
                            kind: AwaitingInputKind::Permission,
                            message: title,
                            options: request
                                .options
                                .iter()
                                .map(|option| option.name.clone())
                                .collect(),
                            allow_always: request
                                .options
                                .iter()
                                .any(|option| option.kind == PermissionOptionKind::AllowAlways),
                        },
                    }));
                let reply = tokio::time::timeout(self.timeout, rx)
                    .await
                    .ok()
                    .and_then(Result::ok);
                self.pending.lock().remove(&id);
                let _ = self
                    .events
                    .send(AcpEvent::State(AgentStateEvent::InputResolved { id }));
                match reply {
                    Some(reply) => Self::response_for(request, reply),
                    // No client answered in time: fail closed.
                    None => Self::response_for(request, InputReply::Reject),
                }
            }
        }
    }

    /// Settle every permission request owned by `generation` with the ACP
    /// `cancelled` outcome. Returns how many requests were resolved.
    fn cancel_turn(&self, generation: u64) -> usize {
        let replies: Vec<oneshot::Sender<InputReply>> = {
            let mut pending = self.pending.lock();
            let owned: Vec<String> = pending
                .iter()
                .filter(|(_, request)| request.generation == generation)
                .map(|(id, _)| id.clone())
                .collect();
            owned
                .into_iter()
                .filter_map(|id| pending.remove(&id).map(|request| request.reply))
                .collect()
        };
        let resolved = replies.len();
        for reply in replies {
            let _ = reply.send(InputReply::Cancelled);
        }
        resolved
    }

    /// Map a Favetto reply onto one of the agent's advertised options. Options
    /// are matched by `kind`, never by position or name.
    fn response_for(
        request: &RequestPermissionRequest,
        reply: InputReply,
    ) -> RequestPermissionResponse {
        // A cancelled turn (or a dismissed prompt) must use ACP's dedicated
        // outcome; selecting a reject option is a different protocol answer.
        if reply == InputReply::Cancelled {
            return RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled);
        }
        let kinds: &[PermissionOptionKind] = match reply {
            InputReply::Once
            | InputReply::Value { .. }
            | InputReply::Confirmed { confirmed: true } => &[
                PermissionOptionKind::AllowOnce,
                PermissionOptionKind::AllowAlways,
            ],
            InputReply::Always => &[
                PermissionOptionKind::AllowAlways,
                PermissionOptionKind::AllowOnce,
            ],
            InputReply::Reject | InputReply::Confirmed { confirmed: false } => &[
                PermissionOptionKind::RejectOnce,
                PermissionOptionKind::RejectAlways,
            ],
            // Handled above: cancellation never selects an advertised option.
            InputReply::Cancelled => &[],
        };
        let picked = kinds.iter().find_map(|kind| {
            request
                .options
                .iter()
                .find(|option| option.kind == *kind)
                .map(|option| option.option_id.clone())
        });
        match picked {
            Some(option_id) => RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
                SelectedPermissionOutcome::new(option_id),
            )),
            None => RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled),
        }
    }
}

impl AcpResponder {
    /// Answer a pending permission request from a client.
    pub(crate) async fn reply(&self, request_id: &str, reply: InputReply) -> anyhow::Result<()> {
        let sender = self
            .pending
            .lock()
            .remove(request_id)
            .map(|request| request.reply)
            .ok_or_else(|| anyhow::anyhow!("no pending ACP permission request '{request_id}'"))?;
        sender
            .send(reply)
            .map_err(|_| anyhow::anyhow!("permission request '{request_id}' is no longer waiting"))
    }
}

/// The current model reported by a session setup response, if any.
fn current_model(options: Option<&[SessionConfigOption]>) -> Option<String> {
    let option = model_option(options.unwrap_or(&[]))?;
    let agent_client_protocol::v1::schema::SessionConfigKind::Select(select) = &option.kind else {
        return None;
    };
    Some(select.current_value.0.to_string())
}

/// Find the model selector among a session's config options.
fn model_option(
    options: &[agent_client_protocol::v1::schema::SessionConfigOption],
) -> Option<&agent_client_protocol::v1::schema::SessionConfigOption> {
    use agent_client_protocol::v1::schema::SessionConfigOptionCategory;
    options.iter().find(|option| {
        option.category == Some(SessionConfigOptionCategory::Model)
            || option.id.0.as_ref() == "model"
    })
}

/// Select `wanted` on the model config option, returning the applied model id.
async fn apply_model(
    connection: &ConnectionTo<Agent>,
    session: &agent_client_protocol::v1::schema::NewSessionResponse,
    wanted: &str,
) -> anyhow::Result<String> {
    use agent_client_protocol::v1::schema::{
        SessionConfigKind, SessionConfigOptionValue, SessionConfigSelectOptions,
        SetSessionConfigOptionRequest,
    };

    let option = session
        .config_options
        .as_deref()
        .and_then(model_option)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "this agent does not expose model selection over ACP; cannot select '{wanted}'"
            )
        })?;
    let SessionConfigKind::Select(select) = &option.kind else {
        anyhow::bail!("the agent's model option is not a select");
    };
    let mut available: Vec<String> = Vec::new();
    match &select.options {
        SessionConfigSelectOptions::Ungrouped(list) => {
            available.extend(list.iter().map(|value| value.value.0.to_string()))
        }
        SessionConfigSelectOptions::Grouped(groups) => available.extend(
            groups
                .iter()
                .flat_map(|group| group.options.iter().map(|value| value.value.0.to_string())),
        ),
        _ => {}
    }
    if !available.iter().any(|value| value == wanted) {
        anyhow::bail!(
            "model '{wanted}' is not offered by this agent; available: {}",
            if available.is_empty() {
                "(none)".to_string()
            } else {
                available.join(", ")
            }
        );
    }

    let response = connection
        .send_request(SetSessionConfigOptionRequest::new(
            session.session_id.clone(),
            option.id.clone(),
            SessionConfigOptionValue::value_id(wanted.to_string()),
        ))
        .block_task()
        .await?;
    let applied = model_option(&response.config_options)
        .and_then(|option| match &option.kind {
            SessionConfigKind::Select(select) => Some(select.current_value.0.to_string()),
            _ => None,
        })
        .unwrap_or_else(|| wanted.to_string());
    if applied != wanted {
        anyhow::bail!("agent kept model '{applied}' instead of '{wanted}'");
    }
    Ok(applied)
}

/// Cumulative per-session tool-call state.
///
/// ACP `ToolCallUpdate` fields are a patch: omitted fields keep their previous
/// value, while `content`/`locations` replace the whole collection. Keeping the
/// merged snapshot here lets every emitted event carry the full picture and
/// makes sparse updates (including updates that arrive before their start)
/// harmless.
#[derive(Default)]
struct ToolTracker {
    calls: HashMap<String, ToolCallDetail>,
    failed: HashSet<String>,
}

impl ToolTracker {
    /// Fold a tool-call start into the call's snapshot; returns whether this
    /// call id was seen for the first time.
    fn start(&mut self, tool: &AcpToolCall) -> (bool, ToolCallDetail) {
        let id = tool.tool_call_id.0.to_string();
        let is_new = !self.calls.contains_key(&id);
        let detail = self.calls.entry(id).or_default();
        merge_start(detail, tool);
        (is_new, detail.clone())
    }

    /// Fold a sparse update into the call's snapshot; returns whether this
    /// call id was seen for the first time.
    fn update(&mut self, id: &str, fields: &AcpToolCallUpdateFields) -> (bool, ToolCallDetail) {
        let is_new = !self.calls.contains_key(id);
        let detail = self.calls.entry(id.to_string()).or_default();
        merge_update(detail, fields);
        (is_new, detail.clone())
    }

    /// Record a failure once per call id; returns whether it was the first.
    fn count_failure(&mut self, id: &str) -> bool {
        self.failed.insert(id.to_string())
    }
}

/// Merge a (possibly out-of-order) start into an existing snapshot.
///
/// A start's title is always present and authoritative. Its kind/status have
/// protocol defaults (`other`/`pending`) that are indistinguishable from
/// "absent", so they must not downgrade a more specific kind or a terminal
/// status supplied by an update that arrived first.
fn merge_start(detail: &mut ToolCallDetail, tool: &AcpToolCall) {
    detail.title = Some(tool.title.clone());
    if let Some(name) = &tool.name {
        detail.name = Some(name.clone());
    }
    let kind = tool_kind(tool.kind);
    if detail.kind.is_none() || kind != ToolKind::Other {
        detail.kind = Some(kind);
    }
    // A start's status has a protocol default (`pending`) that cannot be told
    // apart from "absent", so it may only fill a gap. A late start must never
    // downgrade an in-progress or terminal status learned from an update.
    if detail.status.is_none() {
        detail.status = Some(tool_status(tool.status));
    }
    if let Some(input) = &tool.raw_input {
        detail.input = Some(input.clone());
    }
    if let Some(output) = &tool.raw_output {
        detail.output = Some(output.clone());
    }
    let content: Vec<ToolContent> = tool.content.iter().filter_map(tool_content).collect();
    if !content.is_empty() {
        detail.content = content;
    }
    if !tool.locations.is_empty() {
        detail.locations = tool.locations.iter().map(tool_location).collect();
    }
}

/// Apply ACP patch semantics to a snapshot: `None` leaves a field unchanged;
/// present `content`/`locations` replace the collection, including with empty.
fn merge_update(detail: &mut ToolCallDetail, fields: &AcpToolCallUpdateFields) {
    if let Some(name) = &fields.name {
        detail.name = Some(name.clone());
    }
    if let Some(title) = &fields.title {
        detail.title = Some(title.clone());
    }
    if let Some(kind) = fields.kind {
        detail.kind = Some(tool_kind(kind));
    }
    if let Some(status) = fields.status {
        detail.status = Some(tool_status(status));
    }
    if let Some(input) = &fields.raw_input {
        detail.input = Some(input.clone());
    }
    if let Some(output) = &fields.raw_output {
        detail.output = Some(output.clone());
    }
    if let Some(content) = &fields.content {
        detail.content = content.iter().filter_map(tool_content).collect();
    }
    if let Some(locations) = &fields.locations {
        detail.locations = locations.iter().map(tool_location).collect();
    }
}

/// The display name for a tool, falling back from programmatic name to title.
fn tool_name(detail: &ToolCallDetail) -> String {
    detail
        .name
        .clone()
        .or_else(|| detail.title.clone())
        .unwrap_or_else(|| "tool".to_string())
}

/// Fold one update into the transcript/outcome and queue its normalized events.
fn emit_update(
    update: &SessionUpdate,
    events: &mpsc::UnboundedSender<AcpEvent>,
    transcript: &Mutex<String>,
    outcome: &Mutex<AcpOutcome>,
    tools: &Mutex<ToolTracker>,
) {
    for event in map_update(update, transcript, outcome, tools) {
        let _ = events.send(AcpEvent::State(event));
    }
}

/// Fail an in-flight load exactly once. A load that already became ready (a
/// later prompt/transport error) is not overwritten with a restore failure.
fn fail_load(
    lifecycle: &Option<Arc<Mutex<SessionLifecycle>>>,
    load: &Option<tokio::sync::watch::Sender<Option<Result<(), String>>>>,
    message: &str,
) {
    let was_loading = match lifecycle {
        Some(lifecycle) => {
            let mut lifecycle = lifecycle.lock();
            if lifecycle.state == AgentLoadState::Loading {
                lifecycle.state = AgentLoadState::Failed;
                lifecycle.error = Some(message.to_string());
                true
            } else {
                false
            }
        }
        None => true,
    };
    if was_loading {
        if let Some(tx) = load {
            let _ = tx.send(Some(Err(message.to_string())));
        }
    }
}
/// Map one ACP update into normalized events, folding text into `transcript`
/// and counts into `outcome`.
fn map_update(
    update: &SessionUpdate,
    transcript: &Mutex<String>,
    outcome: &Mutex<AcpOutcome>,
    tools: &Mutex<ToolTracker>,
) -> Vec<AgentStateEvent> {
    match update {
        SessionUpdate::UserMessageChunk(chunk) => content_text(&chunk.content)
            .map(|text| {
                vec![AgentStateEvent::TextDelta {
                    role: MessageRole::User,
                    text: text.to_string(),
                }]
            })
            .unwrap_or_default(),
        SessionUpdate::AgentMessageChunk(chunk) => {
            let Some(text) = content_text(&chunk.content) else {
                return Vec::new();
            };
            transcript.lock().push_str(text);
            {
                let mut state = outcome.lock();
                state.assistant_text.push_str(text);
            }
            vec![AgentStateEvent::TextDelta {
                role: MessageRole::Assistant,
                text: text.to_string(),
            }]
        }
        SessionUpdate::AgentThoughtChunk(chunk) => content_text(&chunk.content)
            .map(|text| {
                vec![AgentStateEvent::ReasoningDelta {
                    text: text.to_string(),
                }]
            })
            .unwrap_or_default(),
        SessionUpdate::ToolCall(tool) => {
            transcript.lock().push_str(&format!(
                "\n[tool] {}: {}\n",
                tool.name.as_deref().unwrap_or(&tool.title),
                tool.title
            ));
            let id = tool.tool_call_id.0.to_string();
            let (is_new, failed_now, detail) = {
                let mut tracker = tools.lock();
                let (is_new, detail) = tracker.start(tool);
                let failed_now =
                    detail.status == Some(ToolCallStatus::Failed) && tracker.count_failure(&id);
                (is_new, failed_now, detail)
            };
            if is_new {
                outcome.lock().tool_calls += 1;
            }
            if failed_now {
                outcome.lock().failed_tools += 1;
            }
            vec![AgentStateEvent::ToolStarted {
                id,
                name: tool_name(&detail),
                input: detail.input.clone().unwrap_or(serde_json::Value::Null),
                detail: Some(Box::new(detail)),
            }]
        }
        SessionUpdate::ToolCallUpdate(update) => {
            let id = update.tool_call_id.0.to_string();
            let (is_new, failed_now, detail) = {
                let mut tracker = tools.lock();
                let (is_new, detail) = tracker.update(&id, &update.fields);
                let failed_now =
                    detail.status == Some(ToolCallStatus::Failed) && tracker.count_failure(&id);
                (is_new, failed_now, detail)
            };
            if is_new {
                outcome.lock().tool_calls += 1;
            }
            if failed_now {
                outcome.lock().failed_tools += 1;
            }

            match update.fields.status {
                Some(AcpToolCallStatus::Completed) => vec![AgentStateEvent::ToolFinished {
                    name: tool_name(&detail),
                    id,
                    ok: true,
                    output: detail.output.clone(),
                    detail: Some(Box::new(detail)),
                }],
                Some(AcpToolCallStatus::Failed) => vec![AgentStateEvent::ToolFinished {
                    name: tool_name(&detail),
                    id,
                    ok: false,
                    output: detail.output.clone(),
                    detail: Some(Box::new(detail)),
                }],
                _ => vec![AgentStateEvent::ToolUpdated {
                    id,
                    partial: serde_json::json!({
                        "title": update.fields.title,
                        "kind": update.fields.kind,
                    }),
                    detail: Some(Box::new(detail)),
                }],
            }
        }
        SessionUpdate::UsageUpdate(usage) => {
            let mut reported = favetto_core::model::AgentUsage {
                context_used: Some(usage.used),
                context_size: Some(usage.size),
                ..Default::default()
            };
            if let Some(cost) = &usage.cost {
                reported.cost_usd = Some(cost.amount);
            }
            vec![AgentStateEvent::Usage { usage: reported }]
        }
        SessionUpdate::SessionInfoUpdate(info) => info
            .title
            .value()
            .map(|title| {
                vec![AgentStateEvent::Title {
                    title: title.clone(),
                }]
            })
            .unwrap_or_default(),
        // Plan, mode, command-list and vendor-specific updates are not part of
        // the normalized model yet. `SessionUpdate` is non-exhaustive, so this
        // arm is required and keeps unknown updates harmless.
        _ => Vec::new(),
    }
}

fn content_text(block: &ContentBlock) -> Option<&str> {
    match block {
        ContentBlock::Text(text) => Some(&text.text),
        _ => None,
    }
}

/// Normalize an ACP tool kind. A future/unknown kind maps to `Other`.
fn tool_kind(kind: AcpToolKind) -> ToolKind {
    match kind {
        AcpToolKind::Read => ToolKind::Read,
        AcpToolKind::Edit => ToolKind::Edit,
        AcpToolKind::Delete => ToolKind::Delete,
        AcpToolKind::Move => ToolKind::Move,
        AcpToolKind::Search => ToolKind::Search,
        AcpToolKind::Execute => ToolKind::Execute,
        AcpToolKind::Think => ToolKind::Think,
        AcpToolKind::Fetch => ToolKind::Fetch,
        AcpToolKind::SwitchMode => ToolKind::SwitchMode,
        _ => ToolKind::Other,
    }
}

/// Normalize an ACP tool status. A future/unknown status maps to `Unknown`.
fn tool_status(status: AcpToolCallStatus) -> ToolCallStatus {
    match status {
        AcpToolCallStatus::Pending => ToolCallStatus::Pending,
        AcpToolCallStatus::InProgress => ToolCallStatus::InProgress,
        AcpToolCallStatus::Completed => ToolCallStatus::Completed,
        AcpToolCallStatus::Failed => ToolCallStatus::Failed,
        _ => ToolCallStatus::Unknown,
    }
}

/// Normalize one structured tool result, dropping binary payloads.
fn tool_content(content: &AcpToolCallContent) -> Option<ToolContent> {
    match content {
        AcpToolCallContent::Content(content) => content_block(&content.content),
        AcpToolCallContent::Diff(diff) => Some(ToolContent::Diff {
            path: diff.path.to_string_lossy().into_owned(),
            old_text: diff.old_text.clone(),
            new_text: diff.new_text.clone(),
        }),
        AcpToolCallContent::Terminal(terminal) => Some(ToolContent::Terminal {
            terminal_id: terminal.terminal_id.0.to_string(),
        }),
        _ => None,
    }
}

/// Normalize one ACP content block. Images, audio and blobs keep metadata only:
/// the daemon never decodes or forwards their base64 payloads.
fn content_block(block: &ContentBlock) -> Option<ToolContent> {
    match block {
        ContentBlock::Text(text) => Some(ToolContent::Text {
            text: text.text.clone(),
        }),
        ContentBlock::Image(image) => Some(ToolContent::Image {
            mime_type: image.mime_type.clone(),
            uri: image.uri.clone(),
        }),
        ContentBlock::Audio(audio) => Some(ToolContent::Audio {
            mime_type: audio.mime_type.clone(),
        }),
        ContentBlock::ResourceLink(link) => Some(ToolContent::ResourceLink {
            uri: link.uri.clone(),
            name: link.name.clone(),
            mime_type: link.mime_type.clone(),
            title: link.title.clone(),
            description: link.description.clone(),
            size: link.size,
        }),
        ContentBlock::Resource(resource) => match &resource.resource {
            EmbeddedResourceResource::TextResourceContents(text) => Some(ToolContent::Resource {
                uri: text.uri.clone(),
                mime_type: text.mime_type.clone(),
                text: Some(text.text.clone()),
                binary: false,
            }),
            EmbeddedResourceResource::BlobResourceContents(blob) => Some(ToolContent::Resource {
                uri: blob.uri.clone(),
                mime_type: blob.mime_type.clone(),
                text: None,
                binary: true,
            }),
            _ => None,
        },
        _ => None,
    }
}

/// Normalize a file location touched by a tool call.
fn tool_location(location: &AcpToolCallLocation) -> ToolLocation {
    ToolLocation {
        path: location.path.to_string_lossy().into_owned(),
        line: location.line,
    }
}

/// The wire name of a stop reason, independent of Rust's `Debug` formatting.
fn stop_reason_name(reason: &agent_client_protocol::v1::schema::StopReason) -> &'static str {
    use agent_client_protocol::v1::schema::StopReason;
    match reason {
        StopReason::EndTurn => "end_turn",
        StopReason::MaxTokens => "max_tokens",
        StopReason::MaxTurnRequests => "max_turn_requests",
        StopReason::Refusal => "refusal",
        StopReason::Cancelled => "cancelled",
        _ => "unknown",
    }
}

/// A `Sink<String>` that writes one line per message, matching ACP's framing.
fn sink_from_stdin(
    stdin: tokio::process::ChildStdin,
) -> impl futures_util::Sink<String, Error = io::Error> + Send + 'static {
    let writer = stdin.compat_write();
    futures_util::sink::unfold(writer, |mut writer, line: String| async move {
        let mut bytes = line.into_bytes();
        bytes.push(b'\n');
        writer.write_all(&bytes).await?;
        writer.flush().await?;
        Ok::<_, io::Error>(writer)
    })
}

/// Read newline-delimited UTF-8 lossily, like the SDK's stdio transport.
fn lossy_lines<R>(
    reader: R,
) -> impl futures_util::Stream<Item = io::Result<String>> + Send + 'static
where
    R: futures_util::io::AsyncRead + Send + 'static,
{
    let reader = Box::pin(BufReader::new(reader));
    futures_util::stream::try_unfold(reader, |mut reader| async move {
        let mut line = Vec::new();
        if reader.read_until(b'\n', &mut line).await? == 0 {
            return Ok::<_, io::Error>(None);
        }
        if line.ends_with(b"\n") {
            line.pop();
            if line.ends_with(b"\r") {
                line.pop();
            }
        }
        Ok(Some((String::from_utf8_lossy(&line).into_owned(), reader)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::v1::schema::{
        Content, ContentChunk, Diff, ImageContent, Terminal, TextResourceContents, ToolCall,
        ToolCallId, ToolCallLocation, ToolCallUpdate, ToolCallUpdateFields,
    };

    /// A `map_update` fixture with isolated transcript/outcome/tool state.
    struct Harness {
        transcript: Mutex<String>,
        outcome: Mutex<AcpOutcome>,
        tools: Mutex<ToolTracker>,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                transcript: Mutex::new(String::new()),
                outcome: Mutex::new(AcpOutcome::default()),
                tools: Mutex::new(ToolTracker::default()),
            }
        }

        fn map(&self, update: &SessionUpdate) -> Vec<AgentStateEvent> {
            map_update(update, &self.transcript, &self.outcome, &self.tools)
        }
    }

    fn update(fields: ToolCallUpdateFields) -> SessionUpdate {
        SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            agent_client_protocol::v1::schema::ToolCallId::new("call_1"),
            fields,
        ))
    }

    fn started(events: &[AgentStateEvent]) -> (&str, &str, &serde_json::Value, &ToolCallDetail) {
        match &events[0] {
            AgentStateEvent::ToolStarted {
                id,
                name,
                input,
                detail,
                ..
            } => (
                id,
                name,
                input,
                detail.as_deref().expect("tool start carries detail"),
            ),
            other => panic!("expected ToolStarted, got {other:?}"),
        }
    }

    fn updated(events: &[AgentStateEvent]) -> (&str, &ToolCallDetail) {
        match &events[0] {
            AgentStateEvent::ToolUpdated { id, detail, .. } => {
                (id, detail.as_deref().expect("tool update carries detail"))
            }
            other => panic!("expected ToolUpdated, got {other:?}"),
        }
    }

    fn finished(
        events: &[AgentStateEvent],
    ) -> (
        &str,
        &str,
        bool,
        &Option<serde_json::Value>,
        &ToolCallDetail,
    ) {
        match &events[0] {
            AgentStateEvent::ToolFinished {
                id,
                name,
                ok,
                output,
                detail,
            } => (
                id,
                name,
                *ok,
                output,
                detail.as_deref().expect("tool finish carries detail"),
            ),
            other => panic!("expected ToolFinished, got {other:?}"),
        }
    }

    #[test]
    fn assistant_text_is_folded_into_the_transcript_and_outcome() {
        let harness = Harness::new();
        let update = SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
            TextContent::new("pong"),
        )));
        let events = harness.map(&update);
        assert_eq!(
            events,
            vec![AgentStateEvent::TextDelta {
                role: MessageRole::Assistant,
                text: "pong".to_string()
            }]
        );
        assert_eq!(&*harness.transcript.lock(), "pong");
        assert_eq!(harness.outcome.lock().assistant_text, "pong");
    }

    #[test]
    fn tool_start_preserves_kind_status_title_input_output_content_and_locations() {
        let harness = Harness::new();
        let call = SessionUpdate::ToolCall(
            ToolCall::new(
                agent_client_protocol::v1::schema::ToolCallId::new("call_1"),
                "Run tests",
            )
            .name("bash")
            .kind(AcpToolKind::Execute)
            .status(AcpToolCallStatus::InProgress)
            .raw_input(serde_json::json!({ "command": "cargo test" }))
            .raw_output(serde_json::json!({ "exit": 0 }))
            .content(vec![
                AcpToolCallContent::Content(Content::new(ContentBlock::Text(TextContent::new(
                    "done",
                )))),
                AcpToolCallContent::Diff(Diff::new("src/lib.rs", "new body").old_text("old body")),
                AcpToolCallContent::Terminal(Terminal::new("term_1")),
            ])
            .locations(vec![ToolCallLocation::new("src/lib.rs").line(7)]),
        );
        let events = harness.map(&call);
        let (id, name, input, detail) = started(&events);
        assert_eq!(id, "call_1");
        assert_eq!(name, "bash");
        assert_eq!(input, &serde_json::json!({ "command": "cargo test" }));
        assert_eq!(detail.name.as_deref(), Some("bash"));
        assert_eq!(detail.title.as_deref(), Some("Run tests"));
        assert_eq!(detail.kind, Some(ToolKind::Execute));
        assert_eq!(detail.status, Some(ToolCallStatus::InProgress));
        assert_eq!(
            detail.input,
            Some(serde_json::json!({ "command": "cargo test" }))
        );
        assert_eq!(detail.output, Some(serde_json::json!({ "exit": 0 })));
        assert_eq!(
            detail.locations,
            vec![ToolLocation {
                path: "src/lib.rs".to_string(),
                line: Some(7)
            }]
        );
        assert!(matches!(
            &detail.content[0],
            ToolContent::Text { text } if text == "done"
        ));
        assert!(matches!(
            &detail.content[1],
            ToolContent::Diff { path, old_text, new_text }
                if path == "src/lib.rs" && old_text.as_deref() == Some("old body") && new_text == "new body"
        ));
        assert!(matches!(
            &detail.content[2],
            ToolContent::Terminal { terminal_id } if terminal_id == "term_1"
        ));
        assert_eq!(harness.outcome.lock().tool_calls, 1);
        assert_eq!(harness.outcome.lock().failed_tools, 0);
    }

    #[test]
    fn title_only_start_falls_back_to_title_and_stays_sparse() {
        let harness = Harness::new();
        let call = SessionUpdate::ToolCall(ToolCall::new(
            agent_client_protocol::v1::schema::ToolCallId::new("call_1"),
            "Read a file",
        ));
        let events = harness.map(&call);
        let (id, name, input, detail) = started(&events);
        assert_eq!(id, "call_1");
        assert_eq!(name, "Read a file");
        assert_eq!(input, &serde_json::Value::Null);
        assert_eq!(detail.name, None);
        assert_eq!(detail.title.as_deref(), Some("Read a file"));
        assert_eq!(detail.kind, Some(ToolKind::Other));
        assert_eq!(detail.status, Some(ToolCallStatus::Pending));
        assert_eq!(detail.input, None);
        assert!(detail.content.is_empty());
        assert!(detail.locations.is_empty());
        assert_eq!(harness.outcome.lock().tool_calls, 1);
    }

    #[test]
    fn sparse_update_keeps_prior_input_metadata_and_content() {
        let harness = Harness::new();
        let call = SessionUpdate::ToolCall(
            ToolCall::new(
                agent_client_protocol::v1::schema::ToolCallId::new("call_1"),
                "Read a",
            )
            .name("read")
            .kind(AcpToolKind::Read)
            .raw_input(serde_json::json!({ "path": "a" }))
            .content(vec![AcpToolCallContent::Content(Content::new(
                ContentBlock::Text(TextContent::new("first")),
            ))]),
        );
        harness.map(&call);

        let events = harness.map(&update(
            ToolCallUpdateFields::default()
                .title("Reading a")
                .status(AcpToolCallStatus::InProgress),
        ));
        let (id, detail) = updated(&events);
        assert_eq!(id, "call_1");
        assert_eq!(detail.name.as_deref(), Some("read"));
        assert_eq!(detail.title.as_deref(), Some("Reading a"));
        assert_eq!(detail.kind, Some(ToolKind::Read));
        assert_eq!(detail.status, Some(ToolCallStatus::InProgress));
        assert_eq!(detail.input, Some(serde_json::json!({ "path": "a" })));
        assert!(matches!(
            &detail.content[0],
            ToolContent::Text { text } if text == "first"
        ));
        assert_eq!(harness.outcome.lock().tool_calls, 1);
        assert_eq!(harness.outcome.lock().failed_tools, 0);
    }

    #[test]
    fn streaming_update_exposes_partial_output_without_finishing() {
        let harness = Harness::new();
        harness.map(&SessionUpdate::ToolCall(
            ToolCall::new(
                agent_client_protocol::v1::schema::ToolCallId::new("call_1"),
                "Bash",
            )
            .name("bash"),
        ));

        let events = harness.map(&update(
            ToolCallUpdateFields::default()
                .status(AcpToolCallStatus::InProgress)
                .raw_output(serde_json::json!({ "chunk": "a" }))
                .locations(vec![ToolCallLocation::new("a.txt").line(1)]),
        ));
        let (id, detail) = updated(&events);
        assert_eq!(id, "call_1");
        assert_eq!(detail.status, Some(ToolCallStatus::InProgress));
        assert_eq!(detail.output, Some(serde_json::json!({ "chunk": "a" })));
        assert_eq!(
            detail.locations,
            vec![ToolLocation {
                path: "a.txt".to_string(),
                line: Some(1)
            }]
        );
        assert_eq!(harness.outcome.lock().failed_tools, 0);

        // A later update replaces (never merges) the locations collection.
        let replaced = harness.map(&update(
            ToolCallUpdateFields::default().locations(vec![ToolCallLocation::new("b.txt")]),
        ));
        assert_eq!(
            updated(&replaced).1.locations,
            vec![ToolLocation {
                path: "b.txt".to_string(),
                line: None
            }]
        );
    }

    #[test]
    fn empty_content_update_clears_but_absent_content_keeps_it() {
        let harness = Harness::new();
        let call = SessionUpdate::ToolCall(
            ToolCall::new(
                agent_client_protocol::v1::schema::ToolCallId::new("call_1"),
                "Read a",
            )
            .content(vec![AcpToolCallContent::Content(Content::new(
                ContentBlock::Text(TextContent::new("first")),
            ))]),
        );
        harness.map(&call);

        let kept = harness.map(&update(ToolCallUpdateFields::default().title("renamed")));
        assert_eq!(updated(&kept).1.content.len(), 1);

        let cleared = harness.map(&update(ToolCallUpdateFields::default().content(Vec::new())));
        assert!(updated(&cleared).1.content.is_empty());
    }

    #[test]
    fn status_only_completion_keeps_start_name_and_output() {
        let harness = Harness::new();
        let call = SessionUpdate::ToolCall(
            ToolCall::new(
                agent_client_protocol::v1::schema::ToolCallId::new("call_1"),
                "Write hello",
            )
            .name("write")
            .raw_output(serde_json::json!({ "bytes": 5 })),
        );
        harness.map(&call);

        let events = harness.map(&update(
            ToolCallUpdateFields::default().status(AcpToolCallStatus::Completed),
        ));
        let (id, name, ok, output, detail) = finished(&events);
        assert_eq!(id, "call_1");
        assert_eq!(name, "write");
        assert!(ok);
        assert_eq!(output, &Some(serde_json::json!({ "bytes": 5 })));
        assert_eq!(detail.status, Some(ToolCallStatus::Completed));
        assert_eq!(detail.output, Some(serde_json::json!({ "bytes": 5 })));
    }

    #[test]
    fn completion_preserves_diff_terminal_and_resource_metadata() {
        let harness = Harness::new();
        harness.map(&SessionUpdate::ToolCall(ToolCall::new(
            agent_client_protocol::v1::schema::ToolCallId::new("call_1"),
            "Edit",
        )));

        let events = harness.map(&update(
            ToolCallUpdateFields::default()
                .status(AcpToolCallStatus::Completed)
                .content(vec![
                    AcpToolCallContent::Diff(Diff::new("/tmp/a.rs", "new").old_text("old")),
                    AcpToolCallContent::Terminal(Terminal::new("term_9")),
                    AcpToolCallContent::Content(Content::new(ContentBlock::Image(
                        ImageContent::new("base64-payload", "image/png").uri("file:///tmp/a.png"),
                    ))),
                    AcpToolCallContent::Content(Content::new(ContentBlock::Resource(
                        agent_client_protocol::v1::schema::EmbeddedResource::new(
                            EmbeddedResourceResource::TextResourceContents(
                                TextResourceContents::new("body", "file:///tmp/a.txt"),
                            ),
                        ),
                    ))),
                ]),
        ));
        let (_, _, ok, _, detail) = finished(&events);
        assert!(ok);
        assert_eq!(detail.content.len(), 4);
        assert!(matches!(
            &detail.content[0],
            ToolContent::Diff { path, old_text, new_text }
                if path == "/tmp/a.rs" && old_text.as_deref() == Some("old") && new_text == "new"
        ));
        assert!(matches!(
            &detail.content[1],
            ToolContent::Terminal { terminal_id } if terminal_id == "term_9"
        ));
        // Binary payloads are metadata-only; the base64 body is never forwarded.
        assert!(matches!(
            &detail.content[2],
            ToolContent::Image { mime_type, uri }
                if mime_type == "image/png" && uri.as_deref() == Some("file:///tmp/a.png")
        ));
        assert!(matches!(
            &detail.content[3],
            ToolContent::Resource { uri, text, binary, .. }
                if uri == "file:///tmp/a.txt" && text.as_deref() == Some("body") && !binary
        ));
    }

    #[test]
    fn interleaved_tool_ids_do_not_cross_talk() {
        let harness = Harness::new();
        harness.map(&SessionUpdate::ToolCall(
            ToolCall::new(
                agent_client_protocol::v1::schema::ToolCallId::new("c1"),
                "Read a",
            )
            .name("read")
            .raw_input(serde_json::json!({ "path": "a" })),
        ));
        harness.map(&SessionUpdate::ToolCall(
            ToolCall::new(
                agent_client_protocol::v1::schema::ToolCallId::new("c2"),
                "Write b",
            )
            .name("write")
            .raw_input(serde_json::json!({ "path": "b" })),
        ));

        let update_c2 = harness.map(&SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            agent_client_protocol::v1::schema::ToolCallId::new("c2"),
            ToolCallUpdateFields::default()
                .title("Writing b")
                .raw_output(serde_json::json!({ "bytes": 1 })),
        )));
        assert_eq!(updated(&update_c2).0, "c2");
        assert_eq!(
            updated(&update_c2).1.input,
            Some(serde_json::json!({ "path": "b" }))
        );

        let finish_c1 = harness.map(&SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            agent_client_protocol::v1::schema::ToolCallId::new("c1"),
            ToolCallUpdateFields::default()
                .status(AcpToolCallStatus::Completed)
                .raw_output(serde_json::json!({ "text": "contents" })),
        )));
        let (id, name, ok, output, detail) = finished(&finish_c1);
        assert_eq!(id, "c1");
        assert_eq!(name, "read");
        assert!(ok);
        assert_eq!(output, &Some(serde_json::json!({ "text": "contents" })));
        assert_eq!(detail.input, Some(serde_json::json!({ "path": "a" })));

        let finish_c2 = harness.map(&SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            agent_client_protocol::v1::schema::ToolCallId::new("c2"),
            ToolCallUpdateFields::default().status(AcpToolCallStatus::Failed),
        )));
        let (id, name, ok, output, detail) = finished(&finish_c2);
        assert_eq!(id, "c2");
        assert_eq!(name, "write");
        assert!(!ok);
        assert_eq!(output, &Some(serde_json::json!({ "bytes": 1 })));
        assert_eq!(detail.title.as_deref(), Some("Writing b"));
        assert_eq!(harness.outcome.lock().tool_calls, 2);
        assert_eq!(harness.outcome.lock().failed_tools, 1);
    }

    #[test]
    fn finish_before_start_is_correlated_and_not_reset_by_the_late_start() {
        let harness = Harness::new();
        let early = harness.map(&update(
            ToolCallUpdateFields::default()
                .title("Run tests")
                .status(AcpToolCallStatus::Completed)
                .raw_output(serde_json::json!("ok"))
                .content(vec![AcpToolCallContent::Content(Content::new(
                    ContentBlock::Text(TextContent::new("ok")),
                ))]),
        ));
        let (id, name, ok, output, detail) = finished(&early);
        assert_eq!(id, "call_1");
        assert_eq!(name, "Run tests");
        assert!(ok);
        assert_eq!(output, &Some(serde_json::json!("ok")));
        assert_eq!(detail.status, Some(ToolCallStatus::Completed));
        assert_eq!(harness.outcome.lock().tool_calls, 1);

        let late = harness.map(&SessionUpdate::ToolCall(
            ToolCall::new(
                agent_client_protocol::v1::schema::ToolCallId::new("call_1"),
                "cargo test",
            )
            .name("bash")
            .raw_input(serde_json::json!({ "command": "cargo test" })),
        ));
        let (_, name, input, detail) = started(&late);
        assert_eq!(name, "bash");
        assert_eq!(input, &serde_json::json!({ "command": "cargo test" }));
        // The late start must not downgrade the terminal status or erase output.
        assert_eq!(detail.status, Some(ToolCallStatus::Completed));
        assert_eq!(detail.output, Some(serde_json::json!("ok")));
        assert_eq!(detail.title.as_deref(), Some("cargo test"));
        assert_eq!(harness.outcome.lock().tool_calls, 1);
        assert_eq!(harness.outcome.lock().failed_tools, 0);
    }

    #[test]
    fn a_late_start_does_not_downgrade_an_in_progress_status() {
        let harness = Harness::new();
        harness.map(&update(
            ToolCallUpdateFields::default().status(AcpToolCallStatus::InProgress),
        ));
        let late = harness.map(&SessionUpdate::ToolCall(ToolCall::new(
            agent_client_protocol::v1::schema::ToolCallId::new("call_1"),
            "Read",
        )));
        assert_eq!(started(&late).3.status, Some(ToolCallStatus::InProgress));
        assert_eq!(harness.outcome.lock().tool_calls, 1);
    }

    #[test]
    fn input_from_an_early_update_survives_a_later_start() {
        let harness = Harness::new();
        let early = harness.map(&update(
            ToolCallUpdateFields::default().raw_input(serde_json::json!({ "path": "early" })),
        ));
        assert_eq!(
            updated(&early).1.input,
            Some(serde_json::json!({ "path": "early" }))
        );

        let late = harness.map(&SessionUpdate::ToolCall(ToolCall::new(
            agent_client_protocol::v1::schema::ToolCallId::new("call_1"),
            "Read",
        )));
        let (_, _, input, detail) = started(&late);
        assert_eq!(input, &serde_json::json!({ "path": "early" }));
        assert_eq!(detail.input, Some(serde_json::json!({ "path": "early" })));
        assert_eq!(harness.outcome.lock().tool_calls, 1);
    }

    #[test]
    fn duplicate_starts_and_failures_are_counted_once() {
        let harness = Harness::new();
        let call = SessionUpdate::ToolCall(
            ToolCall::new(
                agent_client_protocol::v1::schema::ToolCallId::new("call_1"),
                "Write",
            )
            .name("write"),
        );
        harness.map(&call);
        harness.map(&call);
        let failed = update(ToolCallUpdateFields::default().status(AcpToolCallStatus::Failed));
        harness.map(&failed);
        harness.map(&failed);

        assert_eq!(harness.outcome.lock().tool_calls, 1);
        assert_eq!(harness.outcome.lock().failed_tools, 1);
    }

    #[test]
    fn initial_failed_start_is_counted_as_one_failed_call() {
        let harness = Harness::new();
        let call = SessionUpdate::ToolCall(
            ToolCall::new(
                agent_client_protocol::v1::schema::ToolCallId::new("call_1"),
                "Write",
            )
            .name("write")
            .status(AcpToolCallStatus::Failed),
        );
        let events = harness.map(&call);
        assert_eq!(started(&events).3.status, Some(ToolCallStatus::Failed));
        harness.map(&call);
        assert_eq!(harness.outcome.lock().tool_calls, 1);
        assert_eq!(harness.outcome.lock().failed_tools, 1);
    }

    #[tokio::test]
    async fn a_missing_working_directory_is_named_in_the_error() {
        let launch = AcpLaunch {
            agent: "demo".to_string(),
            model: None,
            program: "/bin/true".into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: "/nonexistent/favetto-acp-cwd".into(),
            mode: AcpSessionMode::New,
            policy: PermissionPolicy::Deny,
            prompt: None,
            close_after_turn: true,
            permission_timeout: Duration::from_secs(1),
            session: None,
            start_gate: None,
            lifecycle: None,
        };
        let error = match spawn(launch) {
            Ok(_) => panic!("a missing cwd must not spawn"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("working directory"), "{error}");
        assert!(error.contains("/nonexistent/favetto-acp-cwd"), "{error}");
        assert!(error.contains("'demo'"), "{error}");
    }

    #[test]
    fn success_requires_end_turn_no_error_and_an_answer() {
        let mut outcome = AcpOutcome {
            stop_reason: Some("end_turn".to_string()),
            finished: true,
            assistant_text: "done".to_string(),
            ..Default::default()
        };
        assert!(outcome.succeeded());

        // The pi-acp failure mode: end_turn with no model output.
        outcome.assistant_text.clear();
        assert!(!outcome.succeeded());

        outcome.assistant_text = "done".to_string();
        outcome.error = Some("credit balance is too low".to_string());
        assert!(!outcome.succeeded());

        outcome.error = None;
        outcome.stop_reason = Some("cancelled".to_string());
        assert!(!outcome.succeeded());
    }

    #[test]
    fn reply_maps_to_the_advertised_option_kind() {
        let request = RequestPermissionRequest::new(
            "ses_1",
            agent_client_protocol::v1::schema::ToolCallUpdate::new(
                ToolCallId::new("call_1"),
                ToolCallUpdateFields::default().title("write"),
            ),
            vec![
                agent_client_protocol::v1::schema::PermissionOption::new(
                    "reject-1",
                    "Reject",
                    PermissionOptionKind::RejectOnce,
                ),
                agent_client_protocol::v1::schema::PermissionOption::new(
                    "allow-1",
                    "Allow",
                    PermissionOptionKind::AllowOnce,
                ),
            ],
        );
        let response = AcpResponder::response_for(&request, InputReply::Reject);
        assert!(matches!(
            response.outcome,
            RequestPermissionOutcome::Selected(ref selected) if selected.option_id.0.as_ref() == "reject-1"
        ));
        let response = AcpResponder::response_for(&request, InputReply::Once);
        assert!(matches!(
            response.outcome,
            RequestPermissionOutcome::Selected(ref selected) if selected.option_id.0.as_ref() == "allow-1"
        ));
        // Cancellation uses the dedicated protocol outcome, never a reject
        // option that may not even exist.
        let response = AcpResponder::response_for(&request, InputReply::Cancelled);
        assert!(matches!(
            response.outcome,
            RequestPermissionOutcome::Cancelled
        ));
    }

    /// A permission request fixture with both allow and reject options.
    fn permission_request() -> RequestPermissionRequest {
        RequestPermissionRequest::new(
            "ses_1",
            agent_client_protocol::v1::schema::ToolCallUpdate::new(
                ToolCallId::new("call_1"),
                ToolCallUpdateFields::default().title("write"),
            ),
            vec![
                agent_client_protocol::v1::schema::PermissionOption::new(
                    "reject-1",
                    "Reject",
                    PermissionOptionKind::RejectOnce,
                ),
                agent_client_protocol::v1::schema::PermissionOption::new(
                    "allow-1",
                    "Allow",
                    PermissionOptionKind::AllowOnce,
                ),
            ],
        )
    }

    fn ask_responder(
        timeout: Duration,
    ) -> (
        Arc<AcpResponder>,
        mpsc::UnboundedReceiver<AcpEvent>,
        Arc<Mutex<TurnState>>,
    ) {
        let (events, rx) = mpsc::unbounded_channel();
        let turn = Arc::new(Mutex::new(TurnState::default()));
        let responder = Arc::new(AcpResponder {
            policy: PermissionPolicy::Ask,
            events,
            pending: Mutex::new(HashMap::new()),
            timeout,
            turn: turn.clone(),
        });
        (responder, rx, turn)
    }

    async fn next_input_requested(rx: &mut mpsc::UnboundedReceiver<AcpEvent>) -> String {
        match rx.recv().await.expect("state event") {
            AcpEvent::State(AgentStateEvent::InputRequested { request }) => request.id,
            other => panic!("expected InputRequested, got {other:?}"),
        }
    }

    #[test]
    fn cancellation_is_scoped_to_one_turn() {
        let mut turn = TurnState::default();

        // Idle: a cancel must not send a notification.
        assert_eq!(turn.request_cancel(), None);

        let first = turn.begin();
        assert_eq!(turn.request_cancel(), Some(first));
        // Duplicate cancels for the same turn are ignored.
        assert_eq!(turn.request_cancel(), None);
        assert!(turn.is_cancelled(first));

        turn.finish();
        assert_eq!(turn.request_cancel(), None);

        // A later turn on the same connection is cancellable again.
        let second = turn.begin();
        assert_ne!(first, second);
        assert_eq!(turn.request_cancel(), Some(second));
        assert!(turn.is_cancelled(second));
        assert!(!turn.is_cancelled(first));
    }

    #[tokio::test]
    async fn cancelling_a_turn_settles_its_pending_permission_as_cancelled() {
        let (responder, mut rx, turn) = ask_responder(Duration::from_secs(60));
        let generation = turn.lock().begin();
        let request = permission_request();

        let handle = {
            let responder = responder.clone();
            tokio::spawn(async move { responder.decide(&request).await })
        };
        let id = next_input_requested(&mut rx).await;
        assert_eq!(id, "acp-perm-call_1");

        assert_eq!(responder.cancel_turn(generation), 1);
        let response = handle.await.expect("permission handler");
        assert!(matches!(
            response.outcome,
            RequestPermissionOutcome::Cancelled
        ));

        // The request is resolved exactly once, and a second cancel has nothing
        // left to settle.
        match rx.recv().await.expect("state event") {
            AcpEvent::State(AgentStateEvent::InputResolved { id: resolved }) => {
                assert_eq!(resolved, id);
            }
            other => panic!("expected InputResolved, got {other:?}"),
        }
        assert_eq!(responder.cancel_turn(generation), 0);
    }

    #[tokio::test]
    async fn a_client_answer_racing_cancellation_wins_once() {
        let (responder, mut rx, turn) = ask_responder(Duration::from_secs(60));
        let generation = turn.lock().begin();
        let request = permission_request();

        let handle = {
            let responder = responder.clone();
            tokio::spawn(async move { responder.decide(&request).await })
        };
        let id = next_input_requested(&mut rx).await;

        responder
            .reply(&id, InputReply::Once)
            .await
            .expect("answer the request");
        // The answer already removed the pending request; the racing cancel
        // must not resolve it a second time.
        assert_eq!(responder.cancel_turn(generation), 0);

        let response = handle.await.expect("permission handler");
        assert!(matches!(
            response.outcome,
            RequestPermissionOutcome::Selected(ref selected) if selected.option_id.0.as_ref() == "allow-1"
        ));
        match rx.recv().await.expect("state event") {
            AcpEvent::State(AgentStateEvent::InputResolved { .. }) => {}
            other => panic!("expected InputResolved, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_permission_arriving_during_cancellation_is_answered_immediately() {
        let (responder, mut rx, turn) = ask_responder(Duration::from_secs(60));
        turn.lock().begin();
        assert!(turn.lock().request_cancel().is_some());

        let response = responder.decide(&permission_request()).await;
        assert!(matches!(
            response.outcome,
            RequestPermissionOutcome::Cancelled
        ));
        // It never reaches the client as a blocking InputRequested event.
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }
}
