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
use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::schema::{
    CancelNotification, ContentBlock, InitializeRequest, NewSessionRequest, PermissionOptionKind,
    PromptRequest, RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionNotification, SessionUpdate, TextContent, ToolCallStatus,
};
use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::{Agent, Client, Lines};
use futures_util::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use futures_util::StreamExt;
use parking_lot::Mutex;
use tokio::sync::{mpsc, oneshot};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use favetto_core::model::{
    AgentStateEvent, AwaitingInputKind, InputReply, InputRequest, MessageRole, PermissionPolicy,
};

/// Deadline for an `ask`-policy permission answered by a remote client.
pub(crate) const PERMISSION_TIMEOUT: Duration = Duration::from_secs(300);

/// How long to wait for `session/close` before ending the session anyway. The
/// method is optional in practice, and a completed turn must never leave the
/// task `running`.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

/// What to launch and how to drive it.
pub(crate) struct AcpLaunch {
    /// The configured agent name, used in diagnostics.
    pub agent: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: PathBuf,
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
}

/// Commands that do not need the prompt turn to be in flight.
#[derive(Debug)]
pub(crate) enum AcpCommand {
    /// Interrupt the current turn (`session/cancel`).
    Cancel,
    /// Close the session and stop the server.
    Shutdown,
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
) -> anyhow::Result<(AcpHandles, mpsc::UnboundedReceiver<AgentStateEvent>)> {
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
    let last_activity = Arc::new(Mutex::new(Instant::now()));
    let transcript = Arc::new(Mutex::new(String::new()));
    let outcome = Arc::new(Mutex::new(AcpOutcome::default()));

    let responder = Arc::new(AcpResponder {
        policy: launch.policy,
        events: events_tx.clone(),
        pending: Mutex::new(HashMap::new()),
        timeout: launch.permission_timeout,
    });

    let transport = Lines::new(sink_from_stdin(stdin), lossy_lines(stdout.compat()));

    let notification_events = events_tx.clone();
    let notification_transcript = transcript.clone();
    let notification_outcome = outcome.clone();
    let permission_responder = responder.clone();

    let permission_events = events_tx.clone();
    let initial_prompt = launch.prompt.clone();
    let requested_model = launch.model.clone();
    let agent_name = launch.agent.clone();
    let close_after_turn = launch.close_after_turn;
    let cwd = launch.cwd.clone();
    let running_task = running.clone();
    let exit_task = exit_code.clone();
    let session_id_slot = external_session_id.clone();

    let result_outcome = outcome.clone();
    let result_events = events_tx.clone();
    let connection_outcome = outcome.clone();
    let mut child = child;
    let task = tokio::spawn(async move {
        let notification_transcript = Arc::clone(&notification_transcript);
        let result = Client
            .v1()
            .on_receive_notification(
                async move |notification: SessionNotification, _cx| {
                    let mapped = map_update(
                        &notification.update,
                        &notification_transcript,
                        &notification_outcome,
                    );
                    for event in mapped {
                        let _ = notification_events.send(event);
                    }
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
                    tracing::debug!(
                        target: "favetto::acp",
                        agent = ?init.agent_info,
                        "ACP agent initialized"
                    );

                    let session = connection
                        .send_request(NewSessionRequest::new(cwd))
                        .block_task()
                        .await?;
                    let session_id = session.session_id.clone();
                    // A task names its model explicitly; apply it instead of
                    // relying on the agent's last-used default (which may be
                    // stale or unavailable on this account).
                    let chosen_model = if let Some(wanted) = &requested_model {
                        match apply_model(&connection, &session, wanted).await {
                            Ok(model) => Some(model),
                            Err(error) => {
                                let message = format!("agent '{agent_name}': {error}");
                                let _ = events_tx.send(AgentStateEvent::Error {
                                    message: message.clone(),
                                });
                                return Err(anyhow::anyhow!(message).into());
                            }
                        }
                    } else {
                        current_model(&session)
                    };
                    *session_id_slot.lock() = Some(session_id.0.to_string());
                    let _ = events_tx.send(AgentStateEvent::Session {
                        session_id: Some(session_id.0.to_string()),
                        title: None,
                        model: chosen_model,
                    });

                    // A control task turns external commands into protocol
                    // notifications, so the prompt loop below stays linear.
                    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
                    let control_connection = connection.clone();
                    let control_session = session_id.clone();
                    let mut cancel_pending = false;
                    tokio::spawn(async move {
                        while let Some(command) = commands_rx.recv().await {
                            match command {
                                AcpCommand::Cancel => {
                                    if !cancel_pending {
                                        cancel_pending = true;
                                        let _ = control_connection.send_notification(
                                            CancelNotification::new(control_session.clone()),
                                        );
                                    }
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

                        let response = connection
                            .send_request(PromptRequest::new(
                                session_id.clone(),
                                vec![ContentBlock::Text(TextContent::new(prompt))],
                            ))
                            .block_task()
                            .await;
                        turns += 1;

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
                                let _ = events_tx.send(AgentStateEvent::Error {
                                    message: message.clone(),
                                });
                                (None, favetto_core::model::IdleOutcome::Failed)
                            }
                        };

                        let _ = events_tx.send(AgentStateEvent::Idle {
                            outcome: turn_outcome,
                        });
                        tracing::debug!(target: "favetto::acp", ?stop_reason, "ACP turn finished");

                        if close_after_turn || *shutdown_rx.borrow() {
                            break;
                        }
                    }

                    // Closing is best-effort. `session/close` is not part of the
                    // v1 session capabilities every agent advertises, and an
                    // agent that ignores it never answers: awaiting it would
                    // hold the session `running` forever after the final turn.
                    let close = connection
                        .send_request(agent_client_protocol::v1::schema::CloseSessionRequest::new(
                            session_id,
                        ))
                        .block_task();
                    if tokio::time::timeout(CLOSE_TIMEOUT, close).await.is_err() {
                        tracing::debug!(
                            target: "favetto::acp",
                            agent = %agent_name,
                            "agent did not acknowledge session/close; ending the session anyway"
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
                {
                    let mut state = result_outcome.lock();
                    if state.error.is_none() {
                        state.error = Some(message.clone());
                    }
                }
                let _ = result_events.send(AgentStateEvent::Error { message });
                exit_task.store(1, Ordering::SeqCst);
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
    events: mpsc::UnboundedSender<AgentStateEvent>,
    pending: Mutex<HashMap<String, oneshot::Sender<InputReply>>>,
    timeout: Duration,
}

impl AcpResponder {
    async fn decide(&self, request: &RequestPermissionRequest) -> RequestPermissionResponse {
        match self.policy {
            PermissionPolicy::Allow => Self::response_for(request, InputReply::Once),
            PermissionPolicy::Deny => Self::response_for(request, InputReply::Reject),
            PermissionPolicy::Ask => {
                let id = format!("acp-perm-{}", request.tool_call.tool_call_id.0);
                let title = request
                    .tool_call
                    .fields
                    .title
                    .clone()
                    .or_else(|| request.tool_call.fields.name.clone())
                    .unwrap_or_else(|| "tool call".to_string());
                let (tx, rx) = oneshot::channel();
                self.pending.lock().insert(id.clone(), tx);
                let _ = self.events.send(AgentStateEvent::InputRequested {
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
                });
                let reply = tokio::time::timeout(self.timeout, rx)
                    .await
                    .ok()
                    .and_then(Result::ok);
                self.pending.lock().remove(&id);
                let _ = self.events.send(AgentStateEvent::InputResolved { id });
                match reply {
                    Some(reply) => Self::response_for(request, reply),
                    // No client answered in time: fail closed.
                    None => Self::response_for(request, InputReply::Reject),
                }
            }
        }
    }

    /// Map a Favetto reply onto one of the agent's advertised options. Options
    /// are matched by `kind`, never by position or name.
    fn response_for(
        request: &RequestPermissionRequest,
        reply: InputReply,
    ) -> RequestPermissionResponse {
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
            InputReply::Reject
            | InputReply::Cancelled
            | InputReply::Confirmed { confirmed: false } => &[
                PermissionOptionKind::RejectOnce,
                PermissionOptionKind::RejectAlways,
            ],
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
        let sender =
            self.pending.lock().remove(request_id).ok_or_else(|| {
                anyhow::anyhow!("no pending ACP permission request '{request_id}'")
            })?;
        sender
            .send(reply)
            .map_err(|_| anyhow::anyhow!("permission request '{request_id}' is no longer waiting"))
    }
}

/// The current model reported by a `session/new` response, if any.
fn current_model(
    session: &agent_client_protocol::v1::schema::NewSessionResponse,
) -> Option<String> {
    let option = model_option(session.config_options.as_deref().unwrap_or(&[]))?;
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

/// Map one ACP update into normalized events, folding text into `transcript`
/// and counts into `outcome`.
fn map_update(
    update: &SessionUpdate,
    transcript: &Mutex<String>,
    outcome: &Mutex<AcpOutcome>,
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
            {
                let mut state = outcome.lock();
                state.tool_calls += 1;
            }
            vec![AgentStateEvent::ToolStarted {
                id: tool.tool_call_id.0.to_string(),
                name: tool.name.clone().unwrap_or_else(|| tool.title.clone()),
                input: tool.raw_input.clone().unwrap_or(serde_json::Value::Null),
            }]
        }
        SessionUpdate::ToolCallUpdate(update) => {
            let id = update.tool_call_id.0.to_string();
            let name = update
                .fields
                .name
                .clone()
                .or_else(|| update.fields.title.clone())
                .unwrap_or_else(|| "tool".to_string());
            match update.fields.status {
                Some(ToolCallStatus::Completed) => vec![AgentStateEvent::ToolFinished {
                    id,
                    name,
                    ok: true,
                    output: update.fields.raw_output.clone(),
                }],
                Some(ToolCallStatus::Failed) => {
                    {
                        let mut state = outcome.lock();
                        state.failed_tools += 1;
                    }
                    vec![AgentStateEvent::ToolFinished {
                        id,
                        name,
                        ok: false,
                        output: update.fields.raw_output.clone(),
                    }]
                }
                _ => vec![AgentStateEvent::ToolUpdated {
                    id,
                    partial: serde_json::json!({
                        "title": update.fields.title,
                        "kind": update.fields.kind,
                    }),
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
        ContentChunk, ToolCall, ToolCallId, ToolCallUpdateFields,
    };

    #[test]
    fn assistant_text_is_folded_into_the_transcript_and_outcome() {
        let transcript = Mutex::new(String::new());
        let outcome = Mutex::new(AcpOutcome::default());
        let update = SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
            TextContent::new("pong"),
        )));
        let events = map_update(&update, &transcript, &outcome);
        assert_eq!(
            events,
            vec![AgentStateEvent::TextDelta {
                role: MessageRole::Assistant,
                text: "pong".to_string()
            }]
        );
        assert_eq!(&*transcript.lock(), "pong");
        assert_eq!(outcome.lock().assistant_text, "pong");
    }

    #[test]
    fn tool_updates_report_start_and_finish_with_failure_counts() {
        let transcript = Mutex::new(String::new());
        let outcome = Mutex::new(AcpOutcome::default());
        let call = SessionUpdate::ToolCall(
            ToolCall::new(ToolCallId::new("call_1"), "write hello.txt")
                .name("write")
                .raw_input(serde_json::json!({ "path": "hello.txt" })),
        );
        let started = map_update(&call, &transcript, &outcome);
        assert!(matches!(
            &started[0],
            AgentStateEvent::ToolStarted { id, name, .. } if id == "call_1" && name == "write"
        ));

        let failed =
            SessionUpdate::ToolCallUpdate(agent_client_protocol::v1::schema::ToolCallUpdate::new(
                ToolCallId::new("call_1"),
                ToolCallUpdateFields::default().status(ToolCallStatus::Failed),
            ));
        let finished = map_update(&failed, &transcript, &outcome);
        assert!(matches!(
            &finished[0],
            AgentStateEvent::ToolFinished { ok: false, .. }
        ));
        assert_eq!(outcome.lock().failed_tools, 1);
        assert_eq!(outcome.lock().tool_calls, 1);
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
            policy: PermissionPolicy::Deny,
            prompt: None,
            close_after_turn: true,
            permission_timeout: Duration::from_secs(1),
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
    }
}
