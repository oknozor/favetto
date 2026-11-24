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
use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::schema::{
    AgentCapabilities, AuthMethod, AuthenticateRequest, AvailableCommand as AcpAvailableCommand,
    AvailableCommandInput as AcpAvailableCommandInput, BooleanConfigOptionCapabilities,
    CancelNotification, ClientCapabilities, ClientSessionCapabilities, ContentBlock,
    CreateElicitationRequest, CreateElicitationResponse, DeleteSessionRequest,
    ElicitationAcceptAction, ElicitationAction, ElicitationCapabilities, ElicitationContentValue,
    ElicitationFormCapabilities, ElicitationMode, ElicitationPropertySchema, ElicitationSchema,
    ElicitationScope, EmbeddedResourceResource, EnvVariable, HttpHeader, InitializeRequest,
    ListSessionsRequest, LoadSessionRequest, LogoutRequest, McpServer, McpServerHttp, McpServerSse,
    McpServerStdio, NewSessionRequest, PermissionOptionKind, PlanEntry as AcpPlanEntry,
    PlanEntryPriority as AcpPlanPriority, PlanEntryStatus as AcpPlanStatus, PromptRequest,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    ResumeSessionRequest, SelectedPermissionOutcome, SessionConfigId as AcpSessionConfigId,
    SessionConfigKind as AcpSessionConfigKind, SessionConfigOption as AcpSessionConfigOption,
    SessionConfigOptionCategory as AcpSessionConfigCategory,
    SessionConfigOptionValue as AcpSessionConfigValue, SessionConfigOptionsCapabilities,
    SessionConfigSelectOptions as AcpSelectOptions,
    SessionConfigValueId as AcpSessionConfigValueId, SessionId, SessionModeId as AcpSessionModeId,
    SessionModeState as AcpSessionModeState, SessionNotification, SessionUpdate,
    SetSessionConfigOptionRequest, SetSessionModeRequest, StringFormat, TextContent,
    ToolCall as AcpToolCall, ToolCallContent as AcpToolCallContent,
    ToolCallLocation as AcpToolCallLocation, ToolCallStatus as AcpToolCallStatus,
    ToolCallUpdateFields as AcpToolCallUpdateFields, ToolKind as AcpToolKind, Usage as AcpUsage,
};
use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::{Agent, Client, Lines};
use futures_util::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use futures_util::StreamExt;
use parking_lot::Mutex;
use tokio::sync::{mpsc, oneshot};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use favetto_core::elicitation::{
    ElicitationField, ElicitationFieldKind, ElicitationForm, ElicitationOption, ElicitationOutcome,
    ElicitationReply, ElicitationRequest, ElicitationStringFormat, ElicitationValidationError,
    ElicitationValue, MAX_ELICITATION_FIELDS, MAX_ELICITATION_OPTIONS,
};
use favetto_core::mcp::{McpSessionState, McpTransport};
use favetto_core::model::{
    AgentAuthMethod, AgentAuthMethodKind, AgentAuthState, AgentAuthStatus, AgentCommand,
    AgentLoadState, AgentPlanEntry, AgentPlanPriority, AgentPlanStatus, AgentStateEvent,
    AwaitingInputKind, ContentRole, InputReply, InputRequest, MessageContent, MessageRole,
    PermissionPolicy, RunSummary, SessionConfigChoice, SessionConfigChoices, SessionConfigGroup,
    SessionConfigKind, SessionConfigOption, SessionConfigValue, SessionMode, SessionModes,
    ToolCallDetail, ToolCallStatus, ToolContent, ToolKind, ToolLocation,
};

use super::session_store::SessionBinding;
use super::SessionLifecycle;

/// Deadline for an `ask`-policy permission answered by a remote client.
pub(crate) const PERMISSION_TIMEOUT: Duration = Duration::from_secs(300);

/// Largest newline-delimited ACP message accepted from an agent, in bytes.
///
/// ACP frames one JSON-RPC message per line. The limit excludes the trailing
/// newline and is enforced while reading, before the whole line is buffered or
/// parsed, so a broken or hostile agent cannot grow the daemon's memory without
/// bound. Exceeding it fails the session and tears the child process down.
pub(crate) const MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;

/// Deadline for the ACP `initialize` handshake and each session-setup request
/// (`session/new`, `session/load`, `session/set_config_option`).
///
/// Prompt turns are deliberately never bounded: a task may legitimately run for
/// hours. Only startup may hang forever, stranding the session in
/// `Starting`/`Loading`.
const SETUP_TIMEOUT: Duration = Duration::from_secs(30);

/// Environment override for [`SETUP_TIMEOUT`], in milliseconds. It exists for
/// operators whose agent has a long cold start and for the regression tests that
/// exercise a hung agent without waiting the production deadline.
const SETUP_TIMEOUT_ENV: &str = "FAVETTO_ACP_SETUP_TIMEOUT_MS";

/// The configured startup deadline for one ACP session.
pub(crate) fn setup_timeout() -> Duration {
    std::env::var(SETUP_TIMEOUT_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|millis| *millis > 0)
        .map(Duration::from_millis)
        .unwrap_or(SETUP_TIMEOUT)
}

/// How long to wait for `session/close` before ending the session anyway. The
/// method is optional in practice, and a completed turn must never leave the
/// task `running`.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

/// How long a client-requested config/mode change may take before the RPC fails
/// instead of holding the connection open.
const CONFIG_CHANGE_TIMEOUT: Duration = Duration::from_secs(10);

/// Upper bound on one ACP `authenticate` round trip. The daemon keeps the agent
/// process alive and the UI can retry, so a stuck authentication must not pin a
/// session forever.
pub(crate) const AUTH_TIMEOUT: Duration = Duration::from_secs(300);

/// The safe message shown when an agent only offers terminal sign-in. The
/// daemon preserves the method for display but never advertises or runs it.
pub(crate) const TERMINAL_AUTH_UNSUPPORTED: &str =
    "terminal authentication is not supported by the Favetto daemon; use an agent-handled \
     sign-in method";

/// Whether the initial ACP setup creates a session or restores a known one.
#[derive(Debug, Clone)]
pub(crate) enum AcpSessionMode {
    /// Create a new session with `session/new`.
    New,
    /// Restore a session Favetto already knows by its agent-assigned ACP id.
    Load { external_session_id: String },
    /// Resume a provider-owned session discovered through `session/list`.
    ///
    /// The method is negotiated after `initialize`: `session/load` is preferred
    /// because it replays the transcript, with `session/resume` as the fallback
    /// for agents that can resume but cannot load. Both keep the provider ACP id
    /// separate from the Favetto-local routing id.
    ProviderResume { external_session_id: String },
}

impl AcpSessionMode {
    /// Whether this mode restores an existing provider conversation.
    fn is_restore(&self) -> bool {
        !matches!(self, AcpSessionMode::New)
    }
}

/// The provider session metadata returned by one `session/list` page.
#[derive(Debug, Clone)]
pub(crate) struct AcpProviderSession {
    pub session_id: String,
    pub cwd: PathBuf,
    pub title: Option<String>,
    pub updated_at: Option<String>,
}

/// One `session/list` page plus the capabilities needed to gate actions.
#[derive(Debug)]
pub(crate) struct AcpProviderList {
    /// Whether the agent advertised `sessionCapabilities.list`.
    pub supported: bool,
    pub sessions: Vec<AcpProviderSession>,
    pub next_cursor: Option<String>,
    pub capabilities: AgentCapabilities,
}

/// A short-lived ACP connection used for provider history operations.
pub(crate) struct AcpProviderLaunch {
    pub agent: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// The process working directory.
    pub cwd: PathBuf,
    /// Optional `cwd` filter passed to `session/list`.
    pub filter_cwd: Option<PathBuf>,
    /// Opaque cursor from a previous page.
    pub cursor: Option<String>,
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
    /// Deadline for initialization and session setup. Prompt turns are never
    /// bounded by this.
    pub setup_timeout: Duration,
    /// Durable metadata writer for the Favetto-local session record.
    pub session: Option<SessionBinding>,
    /// For a load, wait for the manager to register the local session before
    /// issuing `session/load`. Replay notifications must have a stable owner
    /// (and a snapshot clients can attach to) before they can arrive.
    pub start_gate: Option<oneshot::Receiver<()>>,
    /// The restore lifecycle shared with the manager's session snapshot.
    pub lifecycle: Option<Arc<Mutex<SessionLifecycle>>>,
    /// Resolved MCP definitions to forward once the agent advertises which
    /// transports it supports.
    pub mcp_servers: Vec<crate::mcp::McpCandidate>,
    /// What the session selected and actually received, shared with the manager
    /// snapshot.
    pub mcp_state: Arc<Mutex<McpSessionState>>,
}

/// Convert one resolved candidate into an ACP `mcpServers` entry.
fn acp_mcp_server(candidate: &crate::mcp::McpCandidate) -> Option<McpServer> {
    match candidate.transport {
        McpTransport::Stdio => {
            let command = candidate.command.clone()?;
            Some(McpServer::Stdio(
                McpServerStdio::new(candidate.name.clone(), command)
                    .args(candidate.args.clone())
                    .env(
                        candidate
                            .env
                            .iter()
                            .map(|(name, value)| EnvVariable::new(name.clone(), value.clone()))
                            .collect(),
                    ),
            ))
        }
        McpTransport::Http => {
            let url = candidate.url.clone()?;
            Some(McpServer::Http(
                McpServerHttp::new(candidate.name.clone(), url).headers(
                    candidate
                        .headers
                        .iter()
                        .map(|(name, value)| HttpHeader::new(name.clone(), value.clone()))
                        .collect(),
                ),
            ))
        }
        McpTransport::Sse => {
            let url = candidate.url.clone()?;
            Some(McpServer::Sse(
                McpServerSse::new(candidate.name.clone(), url).headers(
                    candidate
                        .headers
                        .iter()
                        .map(|(name, value)| HttpHeader::new(name.clone(), value.clone()))
                        .collect(),
                ),
            ))
        }
    }
}

/// Filter the resolved definitions by the agent's advertised `mcpCapabilities`
/// and record what was actually forwarded.
///
/// ACP v1 mandates stdio support; HTTP and SSE are opt-in. Unsupported entries
/// are omitted with an actionable warning rather than failing the session.
fn filter_mcp_servers(
    candidates: &[crate::mcp::McpCandidate],
    capabilities: &AgentCapabilities,
    state: &mut McpSessionState,
) -> Vec<McpServer> {
    let mut servers = Vec::new();
    let mut active = Vec::new();
    for candidate in candidates {
        let supported = match candidate.transport {
            McpTransport::Stdio => true,
            McpTransport::Http => capabilities.mcp_capabilities.http,
            McpTransport::Sse => capabilities.mcp_capabilities.sse,
        };
        if !supported {
            state.warnings.push(format!(
                "MCP server '{}' uses the {} transport, which this agent did not advertise; \
                 omitting it",
                candidate.name,
                candidate.transport.as_str()
            ));
            continue;
        }
        match acp_mcp_server(candidate) {
            Some(server) => {
                active.push(candidate.name.clone());
                servers.push(server);
            }
            None => state.warnings.push(format!(
                "MCP server '{}' is missing its {} endpoint; omitting it",
                candidate.name,
                candidate.transport.as_str()
            )),
        }
    }
    state.active = active;
    servers
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
    /// The runtime has stopped. The manager publishes the terminal `Exited`
    /// live state and exit event exactly once from this marker.
    Exited {
        code: Option<i32>,
    },
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
pub(crate) enum AcpCommand {
    /// Interrupt the current turn (`session/cancel`).
    Cancel,
    /// Close the session and stop the server.
    Shutdown,
    /// Change one advertised config option (`session/set_config_option`).
    SetConfigOption {
        option_id: String,
        value: SessionConfigValue,
        reply: oneshot::Sender<Result<Vec<SessionConfigOption>, String>>,
    },
    /// Switch to an advertised mode (`session/set_mode`).
    SetMode {
        mode_id: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Sign out through ACP `logout`, gated on the advertised capability.
    Logout {
        reply: oneshot::Sender<Result<(), String>>,
    },
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
/// A setup-time sign-in request forwarded to the connection task.
pub(crate) struct AcpAuthCommand {
    pub method_id: String,
    pub reply: oneshot::Sender<Result<(), String>>,
}

/// Progress of a load-mode setup, as observed by the manager.
#[derive(Debug, Clone)]
pub(crate) enum LoadUpdate {
    /// The agent requires sign-in; the load is paused but retryable.
    AuthRequired,
    /// `session/load` succeeded and the replay is applied.
    Ready,
    /// The load failed terminally.
    Failed(String),
}

/// Why one session-setup attempt did not complete.
enum SessionSetup {
    AuthRequired,
    Fatal(String),
}

/// Classify a session setup error: authentication is retryable, everything else
/// is terminal for this launch attempt.
fn setup_error(
    context: impl FnOnce() -> String,
    error: agent_client_protocol::Error,
) -> SessionSetup {
    if error.code == agent_client_protocol::ErrorCode::AUTH_REQUIRED {
        SessionSetup::AuthRequired
    } else {
        SessionSetup::Fatal(format!("{}: {error}", context()))
    }
}

/// Await one session-setup request under the startup deadline.
///
/// A hung server becomes a fatal setup failure naming the method and the
/// deadline instead of stranding the session in `Starting`/`Loading`; a real
/// protocol error is classified by [`setup_error`] so `auth_required` keeps the
/// retryable sign-in flow.
async fn setup_request<T, F>(
    context: impl FnOnce() -> String,
    what: &str,
    deadline: Duration,
    future: F,
) -> Result<T, SessionSetup>
where
    F: Future<Output = Result<T, agent_client_protocol::Error>>,
{
    match tokio::time::timeout(deadline, future).await {
        Ok(result) => result.map_err(|error| setup_error(context, error)),
        Err(_) => Err(SessionSetup::Fatal(format!(
            "{}: {what} did not complete within {deadline:?}; the ACP server may be hung",
            context()
        ))),
    }
}

/// Convert ACP auth methods into the wire/UI shape. Terminal methods are kept
/// but explicitly unsupported: Favetto's daemon cannot bridge an interactive
/// login process to a remote client.
fn wire_auth_methods(methods: &[AuthMethod]) -> Vec<AgentAuthMethod> {
    methods
        .iter()
        .map(|method| {
            let (kind, supported) = match method {
                AuthMethod::Agent(_) => (AgentAuthMethodKind::Agent, true),
                AuthMethod::Terminal(_) => (AgentAuthMethodKind::Terminal, false),
                _ => (AgentAuthMethodKind::Agent, false),
            };
            AgentAuthMethod {
                id: method.id().0.to_string(),
                name: method.name().to_string(),
                description: method.description().map(str::to_string),
                kind,
                supported,
            }
        })
        .collect()
}

/// Publish one session-scoped auth snapshot to the manager snapshot and clients.
fn publish_auth(
    state: AgentAuthState,
    auth: &Mutex<Option<AgentAuthState>>,
    events: &mpsc::UnboundedSender<AcpEvent>,
) {
    *auth.lock() = Some(state.clone());
    let _ = events.send(AcpEvent::State(AgentStateEvent::Auth { auth: state }));
}

/// A safe auth snapshot preserving advertised method metadata.
fn auth_snapshot(
    methods: &[AgentAuthMethod],
    logout_supported: bool,
    status: AgentAuthStatus,
    method_id: Option<&str>,
    message: Option<String>,
) -> AgentAuthState {
    AgentAuthState {
        status,
        methods: methods.to_vec(),
        logout_supported,
        method_id: method_id.map(str::to_string),
        message,
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
    /// Durable structured log folded from every normalized event. Unlike the
    /// bounded live feed this keeps the whole run (text, reasoning, tools,
    /// usage, outcome and error), so a finished task can render it after a
    /// reload or daemon restart.
    pub summary: RunSummary,
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
    /// Sign-in requests consumed while session setup is paused on auth.
    pub authenticate: mpsc::UnboundedSender<AcpAuthCommand>,
    /// Current session-scoped auth state, shared with the manager snapshot.
    pub auth: Arc<Mutex<Option<AgentAuthState>>>,
    pub responder: Arc<AcpResponder>,
    pub running: Arc<AtomicBool>,
    pub exit_code: Arc<AtomicI32>,
    pub external_session_id: Arc<Mutex<Option<String>>>,
    /// Capabilities advertised during `initialize`, retained for callers that
    /// need to decide whether an operation is supported.
    #[allow(dead_code)] // read by later session-history steps and capability-aware callers
    pub capabilities: Arc<Mutex<Option<AgentCapabilities>>>,
    /// Completion of a load-mode startup, when `mode` was [`AcpSessionMode::Load`].
    /// `None` means the load has not settled yet; `Ready` is sent only after the
    /// agent acknowledged `session/load` and the complete replay has been applied
    /// to the feed. `AuthRequired` lets the manager return a retryable session
    /// snapshot while the process stays alive. The receiver closes if the server
    /// exits before either way.
    pub load: Option<tokio::sync::watch::Receiver<Option<LoadUpdate>>>,
    pub last_activity: Arc<Mutex<Instant>>,
    /// Plain-text transcript (assistant text plus tool summaries) used as the
    /// run's captured output.
    pub transcript: Arc<Mutex<String>>,
    pub outcome: Arc<Mutex<AcpOutcome>>,
    /// MCP servers selected for this session and what the agent actually
    /// received after capability filtering.
    pub mcp: Arc<Mutex<McpSessionState>>,
    /// Kept so a forced shutdown can still publish the terminal exit marker,
    /// even after the runtime task is aborted.
    events: mpsc::UnboundedSender<AcpEvent>,
    /// Whether the terminal exit marker has been published; the runtime and a
    /// forced shutdown race here, and only the first one wins.
    terminated: Arc<AtomicBool>,
    /// The connection task. Aborting it drops the runtime's child-reaper signal;
    /// the dedicated reaper then kills and reaps the server.
    task: tokio::task::JoinHandle<()>,
}

impl AcpHandles {
    /// Close the session: request a clean stop, then abort the connection task
    /// so a stuck server is killed rather than leaking. The terminal exit state
    /// is published here too, because an aborted runtime cannot do it itself.
    pub(crate) fn shutdown(&self) {
        let _ = self.commands.send(AcpCommand::Shutdown);
        self.task.abort();
        self.publish_exit(None);
    }

    fn publish_exit(&self, code: Option<i32>) {
        if !self.terminated.swap(true, Ordering::SeqCst) {
            let _ = self.events.send(AcpEvent::Exited { code });
        }
    }

    /// Change one advertised config option and return the agent's full snapshot.
    pub(crate) async fn set_config_option(
        &self,
        option_id: String,
        value: SessionConfigValue,
    ) -> anyhow::Result<Vec<SessionConfigOption>> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(AcpCommand::SetConfigOption {
                option_id,
                value,
                reply,
            })
            .map_err(|_| anyhow::anyhow!("agent session is no longer running"))?;
        tokio::time::timeout(CONFIG_CHANGE_TIMEOUT, response)
            .await
            .map_err(|_| anyhow::anyhow!("agent did not answer the config change in time"))?
            .map_err(|_| anyhow::anyhow!("agent session ended during the config change"))?
            .map_err(anyhow::Error::msg)
    }

    /// Switch to an advertised mode.
    pub(crate) async fn set_mode(&self, mode_id: String) -> anyhow::Result<()> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(AcpCommand::SetMode { mode_id, reply })
            .map_err(|_| anyhow::anyhow!("agent session is no longer running"))?;
        tokio::time::timeout(CONFIG_CHANGE_TIMEOUT, response)
            .await
            .map_err(|_| anyhow::anyhow!("agent did not answer the mode change in time"))?
            .map_err(|_| anyhow::anyhow!("agent session ended during the mode change"))?
            .map_err(anyhow::Error::msg)
    }
}

/// A spawned ACP server with its protocol pipes.
struct AcpProcess {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: tokio::process::ChildStdout,
}

/// Spawn one ACP server process.
///
/// A missing working directory makes `execve` fail with ENOENT too, which would
/// otherwise be reported as if the program were missing; name the directory
/// explicitly because a stale repo/cwd is the usual cause.
fn spawn_process(
    agent: &str,
    program: &std::path::Path,
    args: &[String],
    env: &BTreeMap<String, String>,
    cwd: &std::path::Path,
) -> anyhow::Result<AcpProcess> {
    if !cwd.is_dir() {
        anyhow::bail!(
            "agent '{agent}': working directory '{}' does not exist or is not a directory; \
             check the task's `cwd`/repo path",
            cwd.display()
        );
    }

    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .envs(env)
        .current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(|e| {
        anyhow::anyhow!(
            "agent '{agent}': cannot launch ACP server '{}' in '{}': {e}",
            program.display(),
            cwd.display()
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
    Ok(AcpProcess {
        child,
        stdin,
        stdout,
    })
}

/// Kill and reap a one-shot operation's server.
async fn finish_process(mut child: tokio::process::Child) {
    let _ = child.start_kill();
    let _ = child.wait().await;
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

    let AcpProcess {
        child,
        stdin,
        stdout,
    } = spawn_process(
        &launch.agent,
        &launch.program,
        &launch.args,
        &launch.env,
        &launch.cwd,
    )?;

    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let (prompts_tx, mut prompts_rx) = mpsc::unbounded_channel();
    let (commands_tx, mut commands_rx) = mpsc::unbounded_channel();
    let (auth_tx, mut auth_rx) = mpsc::unbounded_channel::<AcpAuthCommand>();

    let running = Arc::new(AtomicBool::new(true));
    let exit_code = Arc::new(AtomicI32::new(-1));
    let external_session_id = Arc::new(Mutex::new(None));
    let capabilities = Arc::new(Mutex::new(None));
    let auth_state = Arc::new(Mutex::new(None));
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
        agent: launch.agent.clone(),
        session_id: external_session_id.clone(),
        pending_elicitations: Mutex::new(HashMap::new()),
        next_elicitation: AtomicU64::new(1),
    });

    let transport = Lines::new(sink_from_stdin(stdin), bounded_lines(stdout.compat()));

    let notification_events = events_tx.clone();
    let notification_transcript = transcript.clone();
    let notification_outcome = outcome.clone();
    let notification_tools = tools.clone();
    let replay_transcript = transcript.clone();
    let replay_outcome = outcome.clone();
    let replay_tools = tools.clone();
    let permission_responder = responder.clone();
    let elicitation_responder = responder.clone();

    let permission_events = events_tx.clone();
    // Loading restores a conversation; it must never submit a prompt as part of
    // the restore, even if one was supplied to the launch descriptor.
    let initial_prompt = match &launch.mode {
        AcpSessionMode::New => launch.prompt.clone(),
        AcpSessionMode::Load { .. } | AcpSessionMode::ProviderResume { .. } => None,
    };
    let requested_model = launch.model.clone();
    let agent_name = launch.agent.clone();
    let setup_deadline = launch.setup_timeout;
    let close_after_turn = launch.close_after_turn;
    let cwd = launch.cwd.clone();
    let exit_task = exit_code.clone();
    let terminated = Arc::new(AtomicBool::new(false));
    let terminated_task = terminated.clone();
    let session_id_slot = external_session_id.clone();
    let ready_binding = launch.session.clone();
    let end_binding = launch.session;
    let mcp_candidates = launch.mcp_servers;
    let mcp_state = launch.mcp_state;
    let handle_mcp = mcp_state.clone();
    let mut start_gate = launch.start_gate;
    let lifecycle = launch.lifecycle.clone();

    let result_outcome = outcome.clone();
    let result_events = events_tx.clone();
    let handle_events = events_tx.clone();
    let connection_outcome = outcome.clone();
    let capability_slot = capabilities.clone();
    let prompt_turn = turn.clone();
    let control_responder = responder.clone();
    let mode = launch.mode;
    let (load_tx, load_rx) = match &mode {
        AcpSessionMode::Load { .. } | AcpSessionMode::ProviderResume { .. } => {
            let (tx, rx) = tokio::sync::watch::channel(None);
            (Some(tx), Some(rx))
        }
        AcpSessionMode::New => (None, None),
    };
    // A load's replay arrives before `session/load` returns. Hold it until the
    // response proves the restore succeeded, so a failure cannot expose a
    // partial transcript. A provider resume prefers `session/load` and falls
    // back to `session/resume`, so it uses the same gate; the resume path
    // simply has no buffered replay to release.
    let replay_gate = match &mode {
        AcpSessionMode::Load { .. } | AcpSessionMode::ProviderResume { .. } => {
            Some(Arc::new(Mutex::new(ReplayGate::new())))
        }
        AcpSessionMode::New => None,
    };
    // The terminal error path must be able to fail the load even when setup
    // aborted before reaching the explicit load-request branch.
    let failure_mode = mode.clone();
    let failure_lifecycle = lifecycle.clone();
    let failure_load_tx = load_tx.clone();
    let failure_replay = replay_gate.clone();
    let notification_replay = replay_gate.clone();
    let auth_slot = auth_state.clone();
    let handles_auth_tx = auth_tx.clone();
    // The reaper owns the child. The runtime holds the `teardown_tx` sender: a
    // normal finish sends it, and an aborted runtime drops it, so either way the
    // server is killed and its SIGCHLD is collected instead of leaking a zombie.
    let (teardown_tx, teardown_rx) = oneshot::channel::<()>();
    let mut child = child;
    tokio::spawn(async move {
        let _ = teardown_rx.await;
        reap_child(&mut child).await;
    });
    let task = tokio::spawn(async move {
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
            .on_receive_request(
                async move |request: CreateElicitationRequest, responder_handle, _cx| {
                    match elicitation_responder.decide_elicitation(&request).await {
                        Ok(response) => {
                            let _ = responder_handle.respond(response);
                        }
                        Err(reason) => {
                            // A form Favetto cannot render must fail explicitly;
                            // it must never fall back to a permission response.
                            let _ = responder_handle.respond_with_error(
                                agent_client_protocol::Error::new(
                                    agent_client_protocol::ErrorCode::INVALID_PARAMS,
                                    reason.clone(),
                                )
                                .data(reason),
                            );
                        }
                    }
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(
                transport,
                move |connection: ConnectionTo<Agent>| async move {
                    // Plain ACP v1 plan checklists and available-command updates
                    // are baseline and need no client capability. The `plan`
                    // capability opts into plan operations (`plan_update` /
                    // `plan_removed`, identified/Markdown plans), which Favetto
                    // intentionally does not advertise or implement.
                    let init = with_deadline(
                        "initialize",
                        setup_deadline,
                        connection
                            .send_request(
                                InitializeRequest::new(ProtocolVersion::V1)
                                    .client_capabilities(client_capabilities()),
                            )
                            .block_task(),
                    )
                    .await
                    .map_err(|error| anyhow::anyhow!("agent '{agent_name}': {error}"))?;
                    *capability_slot.lock() = Some(init.agent_capabilities.clone());
                    // Resolve which MCP transports this agent supports before
                    // any session setup request carries the list.
                    let mut mcp_snapshot = mcp_state.lock().clone();
                    let mcp_servers = filter_mcp_servers(
                        &mcp_candidates,
                        &init.agent_capabilities,
                        &mut mcp_snapshot,
                    );
                    *mcp_state.lock() = mcp_snapshot;
                    for warning in &mcp_state.lock().warnings {
                        tracing::warn!(
                            target: "favetto::acp",
                            agent = %agent_name,
                            warning = %warning,
                            "omitting MCP server from ACP session setup"
                        );
                    }
                    // `session/close` is optional. Only call it when the agent
                    // advertised `sessionCapabilities.close`; process teardown
                    // below is the cleanup path otherwise.
                    let close_supported =
                        init.agent_capabilities.session_capabilities.close.is_some();
                    let auth_methods = wire_auth_methods(&init.auth_methods);
                    let logout_supported = init.agent_capabilities.auth.logout.is_some();
                    if !auth_methods.is_empty() || logout_supported {
                        publish_auth(
                            auth_snapshot(
                                &auth_methods,
                                logout_supported,
                                AgentAuthStatus::NotRequired,
                                None,
                                None,
                            ),
                            &auth_slot,
                            &events_tx,
                        );
                    }
                    tracing::debug!(
                        target: "favetto::acp",
                        agent = ?init.agent_info,
                        load_session = init.agent_capabilities.load_session,
                        close_supported,
                        auth_methods = auth_methods.len(),
                        logout = logout_supported,
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

                    // Start the control task before session setup: it must
                    // forward a sign-in command while `session/new`/`load` is
                    // paused on auth-required, and honour shutdown throughout.
                    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
                    let control_connection = connection.clone();
                    let session_slot: Arc<Mutex<Option<SessionId>>> = Arc::new(Mutex::new(None));
                    let control_session = session_slot.clone();
                    let control_auth = auth_slot.clone();
                    let control_events = events_tx.clone();
                    let control_shutdown = shutdown_tx.clone();
                    let control_turn = prompt_turn.clone();
                    tokio::spawn(async move {
                        while let Some(command) = commands_rx.recv().await {
                            match command {
                                AcpCommand::Cancel => {
                                    let Some(session) = control_session.lock().clone() else {
                                        continue;
                                    };
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
                                    let _ = control_connection
                                        .send_notification(CancelNotification::new(session));
                                }
                                AcpCommand::SetConfigOption {
                                    option_id,
                                    value,
                                    reply,
                                } => {
                                    let Some(session) = control_session.lock().clone() else {
                                        let _ = reply.send(Err(
                                            "the ACP session is not ready".to_string()
                                        ));
                                        continue;
                                    };
                                    let request = SetSessionConfigOptionRequest::new(
                                        session,
                                        AcpSessionConfigId::new(option_id),
                                        acp_config_value(value),
                                    );
                                    let result = control_connection
                                        .send_request(request)
                                        .block_task()
                                        .await;
                                    match result {
                                        Ok(response) => {
                                            let options = normalized_config_options(Some(
                                                &response.config_options,
                                            ));
                                            let _ = control_events.send(AcpEvent::State(
                                                AgentStateEvent::ConfigOptions {
                                                    options: options.clone(),
                                                },
                                            ));
                                            let _ = reply.send(Ok(options));
                                        }
                                        Err(error) => {
                                            let _ = reply.send(Err(error.to_string()));
                                        }
                                    }
                                }
                                AcpCommand::SetMode { mode_id, reply } => {
                                    let Some(session) = control_session.lock().clone() else {
                                        let _ = reply.send(Err(
                                            "the ACP session is not ready".to_string()
                                        ));
                                        continue;
                                    };
                                    let request = SetSessionModeRequest::new(
                                        session,
                                        AcpSessionModeId::new(mode_id.clone()),
                                    );
                                    let result = control_connection
                                        .send_request(request)
                                        .block_task()
                                        .await;
                                    match result {
                                        Ok(_) => {
                                            let _ = control_events.send(AcpEvent::State(
                                                AgentStateEvent::CurrentMode {
                                                    mode_id: mode_id.clone(),
                                                },
                                            ));
                                            let _ = reply.send(Ok(()));
                                        }
                                        Err(error) => {
                                            let _ = reply.send(Err(error.to_string()));
                                        }
                                    }
                                }
                                AcpCommand::Shutdown => {
                                    if let Some(session) = control_session.lock().clone() {
                                        let _ = control_connection
                                            .send_notification(CancelNotification::new(session));
                                    }
                                    let _ = control_shutdown.send(true);
                                    break;
                                }
                                AcpCommand::Logout { reply } => {
                                    let advertised = control_auth
                                        .lock()
                                        .as_ref()
                                        .is_some_and(|state| state.logout_supported);
                                    if !advertised {
                                        let _ = reply.send(Err(
                                            "the agent did not advertise ACP logout".to_string(),
                                        ));
                                        continue;
                                    }
                                    let result = control_connection
                                        .send_request(LogoutRequest::new())
                                        .block_task()
                                        .await;
                                    match result {
                                        Ok(_) => {
                                            let mut state =
                                                control_auth.lock().clone().unwrap_or_default();
                                            state.status = AgentAuthStatus::NotRequired;
                                            state.method_id = None;
                                            state.message = Some("signed out".to_string());
                                            *control_auth.lock() = Some(state.clone());
                                            let _ = control_events.send(AcpEvent::State(
                                                AgentStateEvent::Auth { auth: state },
                                            ));
                                            let _ = reply.send(Ok(()));
                                        }
                                        Err(error) => {
                                            let _ = reply.send(Err(format!(
                                                "agent logout failed: {error}"
                                            )));
                                        }
                                    }
                                }
                            }
                        }
                    });

                    // Session setup differs only here: a new conversation is
                    // created, while a load restores the agent's own session id.
                    // A load never falls back to `session/new`, because a fresh
                    // session would be presented as a restored conversation.
                    // Auth-required is not a failure: the process stays alive and
                    // the loop waits for an agent-handled sign-in before retrying.
                    let mut pending_auth_reply: Option<oneshot::Sender<Result<(), String>>> = None;
                    let (
                        session_id,
                        external_session_id,
                        chosen_model,
                        config_options,
                        modes,
                    ) = loop {
                        if *shutdown_rx.borrow() {
                            if let Some(reply) = pending_auth_reply.take() {
                                let _ = reply.send(Err(
                                    "the session was closed before sign-in completed".to_string(),
                                ));
                            }
                            return Ok(());
                        }
                        let setup = async {
                            match &mode {
                            AcpSessionMode::Load {
                                external_session_id: known,
                            } => {
                                if !init.agent_capabilities.load_session {
                                    Err(SessionSetup::Fatal(format!(
                                        "agent '{agent_name}' does not support loading ACP sessions \
                                         (session/load); cannot restore session '{known}'"
                                    )))
                                } else {
                                    let loaded = setup_request(
                                        || {
                                            format!(
                                                "agent '{agent_name}' could not load ACP session \
                                                 '{known}'"
                                            )
                                        },
                                        "session/load",
                                        setup_deadline,
                                        connection
                                            .send_request(
                                                LoadSessionRequest::new(
                                                    SessionId::new(known.clone()),
                                                    cwd.clone(),
                                                )
                                                .mcp_servers(mcp_servers.clone()),
                                            )
                                            .block_task(),
                                    )
                                    .await?;
                                    // The local id is the routing key; the ACP
                                    // id stays separate metadata, unchanged by
                                    // the restore.
                                    let recorded = match &ready_binding {
                                        Some(binding) => binding
                                            .store
                                            .mark_ready(
                                                &binding.id,
                                                known,
                                                binding.run_id.as_deref(),
                                            )
                                            .await
                                            .map_err(|error| {
                                                format!(
                                                    "agent '{agent_name}': cannot record loaded \
                                                     ACP session id: {error}"
                                                )
                                            }),
                                        None => Ok(()),
                                    };
                                    match recorded {
                                        Err(message) => Err(SessionSetup::Fatal(message)),
                                        Ok(()) => {
                                            *session_id_slot.lock() = Some(known.clone());
                                            Ok((
                                                SessionId::new(known.clone()),
                                                known.clone(),
                                                current_model(loaded.config_options.as_deref()),
                                                normalized_config_options(
                                                    loaded.config_options.as_deref(),
                                                ),
                                                loaded.modes.as_ref().map(normalized_modes),
                                            ))
                                        }
                                    }
                                }
                            }
                            AcpSessionMode::ProviderResume {
                                external_session_id: known,
                            } => {
                                // Prefer `session/load`: it replays the transcript.
                                // `session/resume` is the v1 fallback for agents that
                                // advertise resume without load.
                                if init.agent_capabilities.load_session {
                                    let loaded = setup_request(
                                        || {
                                            format!(
                                                "agent '{agent_name}' could not load provider \
                                                 session '{known}'"
                                            )
                                        },
                                        "session/load",
                                        setup_deadline,
                                        connection
                                            .send_request(
                                                LoadSessionRequest::new(
                                                    SessionId::new(known.clone()),
                                                    cwd.clone(),
                                                )
                                                .mcp_servers(mcp_servers.clone()),
                                            )
                                            .block_task(),
                                    )
                                    .await?;
                                    let recorded = match &ready_binding {
                                        Some(binding) => binding
                                            .store
                                            .mark_ready(
                                                &binding.id,
                                                known,
                                                binding.run_id.as_deref(),
                                            )
                                            .await
                                            .map_err(|error| {
                                                format!(
                                                    "agent '{agent_name}': cannot record resumed \
                                                     provider session id: {error}"
                                                )
                                            }),
                                        None => Ok(()),
                                    };
                                    match recorded {
                                        Err(message) => Err(SessionSetup::Fatal(message)),
                                        Ok(()) => {
                                            *session_id_slot.lock() = Some(known.clone());
                                            Ok((
                                                SessionId::new(known.clone()),
                                                known.clone(),
                                                current_model(loaded.config_options.as_deref()),
                                                normalized_config_options(
                                                    loaded.config_options.as_deref(),
                                                ),
                                                loaded.modes.as_ref().map(normalized_modes),
                                            ))
                                        }
                                    }
                                } else if init.agent_capabilities.session_capabilities.resume.is_some() {
                                    let resumed = setup_request(
                                        || {
                                            format!(
                                                "agent '{agent_name}' could not resume provider \
                                                 session '{known}'"
                                            )
                                        },
                                        "session/resume",
                                        setup_deadline,
                                        connection
                                            .send_request(
                                                ResumeSessionRequest::new(
                                                    SessionId::new(known.clone()),
                                                    cwd.clone(),
                                                )
                                                .mcp_servers(mcp_servers.clone()),
                                            )
                                            .block_task(),
                                    )
                                    .await?;
                                    let recorded = match &ready_binding {
                                        Some(binding) => binding
                                            .store
                                            .mark_ready(
                                                &binding.id,
                                                known,
                                                binding.run_id.as_deref(),
                                            )
                                            .await
                                            .map_err(|error| {
                                                format!(
                                                    "agent '{agent_name}': cannot record resumed \
                                                     provider session id: {error}"
                                                )
                                            }),
                                        None => Ok(()),
                                    };
                                    match recorded {
                                        Err(message) => Err(SessionSetup::Fatal(message)),
                                        Ok(()) => {
                                            *session_id_slot.lock() = Some(known.clone());
                                            Ok((
                                                SessionId::new(known.clone()),
                                                known.clone(),
                                                current_model(resumed.config_options.as_deref()),
                                                normalized_config_options(
                                                    resumed.config_options.as_deref(),
                                                ),
                                                resumed.modes.as_ref().map(normalized_modes),
                                            ))
                                        }
                                    }
                                } else {
                                    Err(SessionSetup::Fatal(format!(
                                        "agent '{agent_name}' cannot resume provider session \
                                         '{known}': neither session/load nor session/resume is \
                                         supported"
                                    )))
                                }
                            }
                            AcpSessionMode::New => {
                                let session = setup_request(
                                    || {
                                        format!(
                                            "agent '{agent_name}' could not create ACP session"
                                        )
                                    },
                                    "session/new",
                                    setup_deadline,
                                    connection
                                        .send_request(
                                            NewSessionRequest::new(cwd.clone())
                                                .mcp_servers(mcp_servers.clone()),
                                        )
                                        .block_task(),
                                )
                                .await?;
                                let session_id = session.session_id.clone();
                                let external_session_id = session_id.0.to_string();
                                // Record the agent's identity immediately: a
                                // task run can be finalized (or the daemon can
                                // crash) long before the first turn ends. A
                                // write fault aborts the session rather than
                                // letting an untracked conversation continue.
                                let recorded = match &ready_binding {
                                    Some(binding) => binding
                                        .store
                                        .mark_ready(
                                            &binding.id,
                                            &external_session_id,
                                            binding.run_id.as_deref(),
                                        )
                                        .await
                                        .map_err(|error| {
                                            format!(
                                                "agent '{agent_name}': cannot record ACP session \
                                                 id: {error}"
                                            )
                                        }),
                                    None => Ok(()),
                                };
                                match recorded {
                                    Err(message) => Err(SessionSetup::Fatal(message)),
                                    Ok(()) => {
                                        // Expose the id to the manager even if
                                        // model selection fails below, so a
                                        // failed run still reports its identity.
                                        *session_id_slot.lock() =
                                            Some(external_session_id.clone());
                                        // A task names its model explicitly;
                                        // apply it instead of relying on the
                                        // agent's last-used default (which may
                                        // be stale or unavailable here).
                                        let chosen = if let Some(wanted) = &requested_model {
                                            apply_model(
                                                &connection,
                                                &session,
                                                wanted,
                                                setup_deadline,
                                            )
                                            .await
                                            .map(|(model, options)| (Some(model), options))
                                            .map_err(|error| {
                                                SessionSetup::Fatal(format!(
                                                    "agent '{agent_name}': {error}"
                                                ))
                                            })
                                        } else {
                                            Ok((
                                                current_model(session.config_options.as_deref()),
                                                normalized_config_options(
                                                    session.config_options.as_deref(),
                                                ),
                                            ))
                                        };
                                        match chosen {
                                            Ok((chosen_model, options)) => Ok((
                                                session_id,
                                                external_session_id,
                                                chosen_model,
                                                options,
                                                session.modes.as_ref().map(normalized_modes),
                                            )),
                                            Err(failure) => Err(failure),
                                        }
                                    }
                                }
                            }
                            }
                        }
                        .await;

                        match setup {
                            Ok(value) => {
                                if let Some(reply) = pending_auth_reply.take() {
                                    let _ = reply.send(Ok(()));
                                }
                                break value;
                            }
                            Err(SessionSetup::AuthRequired) => {
                                publish_auth(
                                    auth_snapshot(
                                        &auth_methods,
                                        logout_supported,
                                        AgentAuthStatus::Required,
                                        None,
                                        Some("the agent requires sign-in".to_string()),
                                    ),
                                    &auth_slot,
                                    &events_tx,
                                );
                                if let Some(tx) = &load_tx {
                                    let _ = tx.send(Some(LoadUpdate::AuthRequired));
                                }
                                // A previous authenticate call that still ended
                                // in auth-required must not hang its caller.
                                if let Some(reply) = pending_auth_reply.take() {
                                    let _ = reply.send(Err(
                                        "the agent still requires authentication".to_string(),
                                    ));
                                }
                                loop {
                                let command = loop {
                                    tokio::select! {
                                        command = auth_rx.recv() => break command,
                                        _ = shutdown_rx.changed() => {
                                            if *shutdown_rx.borrow() {
                                                if let Some(reply) = pending_auth_reply.take() {
                                                    let _ = reply.send(Err(
                                                        "the session was closed before sign-in completed"
                                                            .to_string(),
                                                    ));
                                                }
                                                return Ok(());
                                            }
                                        }
                                    }
                                };
                                let Some(command) = command else {
                                    let message = format!(
                                        "agent '{agent_name}': authentication was requested but \
                                         no sign-in channel is available"
                                    );
                                    if let Some(reply) = pending_auth_reply.take() {
                                        let _ = reply.send(Err(message.clone()));
                                    }
                                    return Err(anyhow::anyhow!(message).into());
                                };
                                let method = auth_methods
                                    .iter()
                                    .find(|method| method.id == command.method_id);
                                let Some(method) = method else {
                                    let message = format!(
                                        "authentication method '{}' was not advertised by the agent",
                                        command.method_id
                                    );
                                    publish_auth(
                                        auth_snapshot(
                                            &auth_methods,
                                            logout_supported,
                                            AgentAuthStatus::Failed,
                                            Some(&command.method_id),
                                            Some(message.clone()),
                                        ),
                                        &auth_slot,
                                        &events_tx,
                                    );
                                    let _ = command.reply.send(Err(message));
                                    continue;
                                };
                                if !method.supported {
                                    let message = if method.kind == AgentAuthMethodKind::Terminal {
                                        TERMINAL_AUTH_UNSUPPORTED.to_string()
                                    } else {
                                        format!(
                                            "authentication method '{}' is not supported by the \
                                             Favetto daemon",
                                            method.id
                                        )
                                    };
                                    publish_auth(
                                        auth_snapshot(
                                            &auth_methods,
                                            logout_supported,
                                            AgentAuthStatus::Failed,
                                            Some(&command.method_id),
                                            Some(message.clone()),
                                        ),
                                        &auth_slot,
                                        &events_tx,
                                    );
                                    let _ = command.reply.send(Err(message));
                                    continue;
                                }

                                publish_auth(
                                    auth_snapshot(
                                        &auth_methods,
                                        logout_supported,
                                        AgentAuthStatus::Authenticating,
                                        Some(&command.method_id),
                                        None,
                                    ),
                                    &auth_slot,
                                    &events_tx,
                                );
                                let result = tokio::time::timeout(
                                    AUTH_TIMEOUT,
                                    connection
                                        .send_request(AuthenticateRequest::new(
                                            command.method_id.clone(),
                                        ))
                                        .block_task(),
                                )
                                .await;
                                match result {
                                    Ok(Ok(_)) => {
                                        publish_auth(
                                            auth_snapshot(
                                                &auth_methods,
                                                logout_supported,
                                                AgentAuthStatus::Authenticated,
                                                Some(&command.method_id),
                                                None,
                                            ),
                                            &auth_slot,
                                            &events_tx,
                                        );
                                        pending_auth_reply = Some(command.reply);
                                        // Retry session setup now that the agent
                                        // accepted the sign-in.
                                        break;
                                    }
                                    Ok(Err(error)) => {
                                        let message =
                                            format!("agent sign-in failed: {error}");
                                        publish_auth(
                                            auth_snapshot(
                                                &auth_methods,
                                                logout_supported,
                                                AgentAuthStatus::Failed,
                                                Some(&command.method_id),
                                                Some(message.clone()),
                                            ),
                                            &auth_slot,
                                            &events_tx,
                                        );
                                        let _ = command.reply.send(Err(message));
                                    }
                                    Err(_) => {
                                        let message =
                                            "agent sign-in timed out".to_string();
                                        publish_auth(
                                            auth_snapshot(
                                                &auth_methods,
                                                logout_supported,
                                                AgentAuthStatus::Failed,
                                                Some(&command.method_id),
                                                Some(message.clone()),
                                            ),
                                            &auth_slot,
                                            &events_tx,
                                        );
                                        let _ = command.reply.send(Err(message));
                                    }
                                }
                                }
                            }
                            Err(SessionSetup::Fatal(message)) => {
                                if let Some(reply) = pending_auth_reply.take() {
                                    let _ = reply.send(Err(message.clone()));
                                }
                                if matches!(&mode, AcpSessionMode::Load { .. }) {
                                    fail_load(&lifecycle, &load_tx, &message);
                                }
                                let _ = events_tx.send(AcpEvent::State(AgentStateEvent::Error {
                                    message: message.clone(),
                                }));
                                return Err(anyhow::anyhow!(message).into());
                            }
                        }
                    };
                    *session_slot.lock() = Some(session_id.clone());
                    // Setup completed: any pending sign-in is no longer blocking.
                    // Keep the advertised method metadata for the session's
                    // lifetime so clients can still sign out when supported.
                    if !auth_methods.is_empty() || logout_supported {
                        publish_auth(
                            auth_snapshot(
                                &auth_methods,
                                logout_supported,
                                AgentAuthStatus::NotRequired,
                                None,
                                None,
                            ),
                            &auth_slot,
                            &events_tx,
                        );
                    }
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
                    let _ = events_tx.send(AcpEvent::State(AgentStateEvent::ConfigOptions {
                        options: config_options,
                    }));
                    if let Some(modes) = modes {
                        let _ = events_tx.send(AcpEvent::State(AgentStateEvent::Modes { modes }));
                    }
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
                        let _ = tx.send(Some(LoadUpdate::Ready));
                    }

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
                                // Emit the prompt-response usage before `Idle`:
                                // the state task folds events in order, so the
                                // durable summary and live snapshot both carry it
                                // before a waiter observes the turn boundary.
                                if let Some(usage) = response.usage.as_ref() {
                                    let _ = events_tx.send(AcpEvent::State(
                                        AgentStateEvent::Usage {
                                            usage: prompt_usage(usage),
                                            snapshot: false,
                                        },
                                    ));
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
                if failure_mode.is_restore() {
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
        // Do not clear `running` here: the state task clears it only after it
        // has folded every queued `State` event (including a final
        // `usage_update`) and processed this exit marker. Clearing it first
        // would let a waiter read the outcome and live usage before the last
        // usage event reached them.
        let code = exit_task.load(Ordering::SeqCst);
        if !terminated_task.swap(true, Ordering::SeqCst) {
            let _ = result_events.send(AcpEvent::Exited {
                code: (code >= 0).then_some(code),
            });
        }
        // Ask the reaper to kill the whole agent process group and collect
        // the child, so a hung or malformed server cannot become a zombie.
        let _ = teardown_tx.send(());
    });

    Ok((
        AcpHandles {
            prompts: prompts_tx,
            commands: commands_tx,
            authenticate: handles_auth_tx,
            auth: auth_state,
            responder,
            running,
            exit_code,
            external_session_id,
            capabilities,
            load: load_rx,
            last_activity,
            transcript,
            outcome,
            mcp: handle_mcp,
            events: handle_events,
            terminated,
            task,
        },
        events_rx,
    ))
}

/// List one page of provider-owned sessions through the advertised
/// `session/list`.
///
/// A short-lived ACP connection is launched for the operation: provider history
/// is queried only from the agent itself, never inferred from Favetto's durable
/// records. An agent without `sessionCapabilities.list` returns
/// `supported: false` rather than receiving an unadvertised request.
pub(crate) async fn list_provider_sessions(
    launch: AcpProviderLaunch,
) -> anyhow::Result<AcpProviderList> {
    let AcpProcess {
        child,
        stdin,
        stdout,
    } = spawn_process(
        &launch.agent,
        &launch.program,
        &launch.args,
        &launch.env,
        &launch.cwd,
    )?;
    let transport = Lines::new(sink_from_stdin(stdin), bounded_lines(stdout.compat()));
    let filter_cwd = launch.filter_cwd;
    let cursor = launch.cursor;

    let result = Client
        .v1()
        .connect_with(
            transport,
            move |connection: ConnectionTo<Agent>| async move {
                let init = connection
                    .send_request(
                        InitializeRequest::new(ProtocolVersion::V1)
                            .client_capabilities(client_capabilities()),
                    )
                    .block_task()
                    .await?;
                let capabilities = init.agent_capabilities;
                if capabilities.session_capabilities.list.is_none() {
                    return Ok(AcpProviderList {
                        supported: false,
                        sessions: Vec::new(),
                        next_cursor: None,
                        capabilities,
                    });
                }
                let mut request = ListSessionsRequest::new();
                if let Some(cwd) = filter_cwd {
                    request = request.cwd(cwd);
                }
                if let Some(cursor) = cursor {
                    request = request.cursor(cursor);
                }
                let response = connection.send_request(request).block_task().await?;
                let sessions = response
                    .sessions
                    .into_iter()
                    .map(|session| AcpProviderSession {
                        session_id: session.session_id.0.to_string(),
                        cwd: session.cwd,
                        title: session.title,
                        updated_at: session.updated_at,
                    })
                    .collect();
                Ok(AcpProviderList {
                    supported: true,
                    sessions,
                    next_cursor: response.next_cursor,
                    capabilities,
                })
            },
        )
        .await;
    finish_process(child).await;
    result.map_err(|error| anyhow::anyhow!(error))
}

/// Delete a provider-owned session through the advertised `session/delete`.
///
/// This removes only the provider transcript. Favetto-local metadata remains a
/// separate lifecycle and is not touched here.
pub(crate) async fn delete_provider_session(
    launch: AcpProviderLaunch,
    session_id: String,
) -> anyhow::Result<()> {
    let AcpProcess {
        child,
        stdin,
        stdout,
    } = spawn_process(
        &launch.agent,
        &launch.program,
        &launch.args,
        &launch.env,
        &launch.cwd,
    )?;
    let transport = Lines::new(sink_from_stdin(stdin), bounded_lines(stdout.compat()));
    let agent = launch.agent;

    let result = Client
        .v1()
        .connect_with(
            transport,
            move |connection: ConnectionTo<Agent>| async move {
                let init = connection
                    .send_request(
                        InitializeRequest::new(ProtocolVersion::V1)
                            .client_capabilities(client_capabilities()),
                    )
                    .block_task()
                    .await?;
                if init
                    .agent_capabilities
                    .session_capabilities
                    .delete
                    .is_none()
                {
                    return Err(anyhow::anyhow!(
                        "agent '{agent}' does not support deleting ACP sessions (session/delete)"
                    )
                    .into());
                }
                connection
                    .send_request(DeleteSessionRequest::new(SessionId::new(session_id)))
                    .block_task()
                    .await?;
                Ok(())
            },
        )
        .await;
    finish_process(child).await;
    result.map_err(|error| anyhow::anyhow!(error))
}

/// Answers permission requests according to the session's policy.
pub(crate) struct AcpResponder {
    policy: PermissionPolicy,
    events: mpsc::UnboundedSender<AcpEvent>,
    pending: Mutex<HashMap<String, PendingPermission>>,
    timeout: Duration,
    turn: Arc<Mutex<TurnState>>,
    agent: String,
    session_id: Arc<Mutex<Option<String>>>,
    pending_elicitations: Mutex<HashMap<String, PendingElicitation>>,
    next_elicitation: AtomicU64,
}

/// One outstanding `ask` permission request, tagged with the turn that owns it.
struct PendingPermission {
    generation: u64,
    reply: oneshot::Sender<InputReply>,
}

/// One outstanding form elicitation, tagged with the turn that owns it.
struct PendingElicitation {
    generation: u64,
    form: ElicitationForm,
    reply: oneshot::Sender<ElicitationReply>,
}

/// Why a client-supplied elicitation reply was not delivered.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ElicitationReplyError {
    /// The accepted values did not satisfy the requested form.
    #[error("{0}")]
    Invalid(ElicitationValidationError),
    /// The request was already answered, timed out or cancelled.
    #[error("no pending ACP elicitation request '{0}'")]
    NoPending(String),
    /// The waiting ACP handler is gone.
    #[error("elicitation request '{0}' is no longer waiting")]
    Closed(String),
}

/// The client capabilities Favetto advertises: form elicitation and boolean
/// session config options. URL elicitation remains unadvertised until a separate
/// user-consented browser flow exists.
fn client_capabilities() -> ClientCapabilities {
    // ACP-CONFIG-003: agents may only include `type: "boolean"` config options
    // when the client advertises `session.configOptions.boolean`. Favetto's
    // option UI already renders and sets boolean values, so declare support.
    let config_options =
        SessionConfigOptionsCapabilities::new().boolean(BooleanConfigOptionCapabilities::new());
    ClientCapabilities::new()
        .elicitation(ElicitationCapabilities::new().form(ElicitationFormCapabilities::new()))
        .session(ClientSessionCapabilities::new().config_options(config_options))
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
                            form: None,
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

    /// Handle one ACP `elicitation/create` request.
    ///
    /// Unsupported modes or schemas return an error string that the caller sends
    /// to the agent as JSON-RPC `-32602`. A timeout or dropped connection is
    /// always [cancelled](ElicitationOutcome::Cancelled), never accepted.
    async fn decide_elicitation(
        &self,
        request: &CreateElicitationRequest,
    ) -> Result<CreateElicitationResponse, String> {
        let ElicitationMode::Form(mode) = &request.mode else {
            return Err(
                "URL and unknown elicitation modes are not advertised by Favetto".to_string(),
            );
        };
        let tool_call_id = match &mode.scope {
            ElicitationScope::Session(scope) => {
                if let Some(expected) = self.session_id.lock().clone() {
                    if scope.session_id.0.as_ref() != expected {
                        return Err(format!(
                            "elicitation is scoped to another ACP session ('{}')",
                            scope.session_id.0
                        ));
                    }
                }
                scope.tool_call_id.as_ref().map(|id| id.0.to_string())
            }
            // Request-scoped elicitation is accepted: it carries no session and
            // is still correlated by its JSON-RPC response.
            ElicitationScope::Request(_) => None,
            _ => return Err("unknown elicitation scope is not supported".to_string()),
        };
        let form = wire_form(&mode.requested_schema)?;
        let (generation, already_cancelled) = {
            let turn = self.turn.lock();
            (turn.generation, turn.is_cancelled(turn.generation))
        };
        if already_cancelled {
            return Ok(CreateElicitationResponse::new(ElicitationAction::Cancel));
        }

        let id = format!(
            "acp-elic-{}",
            self.next_elicitation.fetch_add(1, Ordering::SeqCst)
        );
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending_elicitations.lock();
            if !pending.is_empty() {
                return Err(
                    "another elicitation request is already pending in this session".to_string(),
                );
            }
            pending.insert(
                id.clone(),
                PendingElicitation {
                    generation,
                    form: form.clone(),
                    reply: tx,
                },
            );
        }
        let _ = self
            .events
            .send(AcpEvent::State(AgentStateEvent::ElicitationRequested {
                request: ElicitationRequest {
                    id: id.clone(),
                    agent: self.agent.clone(),
                    tool_call_id,
                    message: request.message.clone(),
                    form,
                },
            }));
        let reply = tokio::time::timeout(self.timeout, rx)
            .await
            .ok()
            .and_then(Result::ok);
        self.pending_elicitations.lock().remove(&id);
        let outcome = match reply.as_ref() {
            Some(ElicitationReply::Accept { .. }) => ElicitationOutcome::Accepted,
            Some(ElicitationReply::Decline) => ElicitationOutcome::Declined,
            Some(ElicitationReply::Cancel) | None => ElicitationOutcome::Cancelled,
        };
        let _ = self
            .events
            .send(AcpEvent::State(AgentStateEvent::ElicitationResolved {
                id,
                outcome,
            }));
        Ok(match reply {
            Some(reply) => Self::elicitation_response(reply),
            // No client answered in time: fail closed.
            None => CreateElicitationResponse::new(ElicitationAction::Cancel),
        })
    }

    /// Settle every permission and elicitation request owned by `generation`
    /// with the ACP `cancelled` outcome. Returns how many requests were resolved.
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
        let elicitations: Vec<oneshot::Sender<ElicitationReply>> = {
            let mut pending = self.pending_elicitations.lock();
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
        let resolved = replies.len() + elicitations.len();
        for reply in replies {
            let _ = reply.send(InputReply::Cancelled);
        }
        for reply in elicitations {
            let _ = reply.send(ElicitationReply::Cancel);
        }
        resolved
    }

    /// Map a validated Favetto form reply onto the ACP action variants.
    fn elicitation_response(reply: ElicitationReply) -> CreateElicitationResponse {
        match reply {
            ElicitationReply::Accept { values } => {
                let content = values
                    .into_iter()
                    .map(|(name, value)| {
                        let value = match value {
                            ElicitationValue::String(value) => {
                                ElicitationContentValue::String(value)
                            }
                            ElicitationValue::Integer(value) => {
                                ElicitationContentValue::Integer(value)
                            }
                            ElicitationValue::Number(value) => {
                                ElicitationContentValue::Number(value)
                            }
                            ElicitationValue::Boolean(value) => {
                                ElicitationContentValue::Boolean(value)
                            }
                        };
                        (name, value)
                    })
                    .collect::<BTreeMap<_, _>>();
                CreateElicitationResponse::new(ElicitationAction::Accept(
                    ElicitationAcceptAction::new().content(content),
                ))
            }
            ElicitationReply::Decline => CreateElicitationResponse::new(ElicitationAction::Decline),
            ElicitationReply::Cancel => CreateElicitationResponse::new(ElicitationAction::Cancel),
        }
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

    /// Answer a pending form elicitation from a client.
    ///
    /// Invalid accepted values are rejected without removing the request, so
    /// the UI can keep the form open and let the user correct it.
    pub(crate) async fn reply_elicitation(
        &self,
        request_id: &str,
        reply: ElicitationReply,
    ) -> Result<(), ElicitationReplyError> {
        let pending = {
            let mut pending = self.pending_elicitations.lock();
            let request = pending
                .get(request_id)
                .ok_or_else(|| ElicitationReplyError::NoPending(request_id.to_string()))?;
            if let ElicitationReply::Accept { values } = &reply {
                request
                    .form
                    .validate(values)
                    .map_err(ElicitationReplyError::Invalid)?;
            }
            pending
                .remove(request_id)
                .ok_or_else(|| ElicitationReplyError::NoPending(request_id.to_string()))?
        };
        pending
            .reply
            .send(reply)
            .map_err(|_| ElicitationReplyError::Closed(request_id.to_string()))
    }
}

/// Convert one ACP elicitation schema into the restricted wire form.
///
/// Any unsupported type, constraint or invalid default is rejected with a
/// reason. The caller returns that reason to the agent as `-32602`; it never
/// silently becomes an empty form or a permission approval.
fn wire_form(schema: &ElicitationSchema) -> Result<ElicitationForm, String> {
    if schema.properties.len() > MAX_ELICITATION_FIELDS {
        return Err(format!(
            "elicitation schema has more than {MAX_ELICITATION_FIELDS} fields"
        ));
    }
    let required = schema.required.clone().unwrap_or_default();
    if let Some(name) = required
        .iter()
        .find(|name| !schema.properties.contains_key(*name))
    {
        return Err(format!(
            "elicitation schema requires unknown property '{name}'"
        ));
    }
    check_label("title", schema.title.as_deref())?;
    check_label("description", schema.description.as_deref())?;
    let mut fields = Vec::with_capacity(schema.properties.len());
    for (name, property) in &schema.properties {
        let field = wire_field(
            name,
            property,
            required.iter().any(|required| required == name),
        )?;
        fields.push(field);
    }
    fields.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(ElicitationForm {
        title: schema.title.clone(),
        description: schema.description.clone(),
        fields,
    })
}

fn wire_field(
    name: &str,
    property: &ElicitationPropertySchema,
    required: bool,
) -> Result<ElicitationField, String> {
    let field = match property {
        ElicitationPropertySchema::String(string) => {
            if string.pattern.is_some() {
                return Err(format!(
                    "elicitation field '{name}' uses an unsupported pattern constraint"
                ));
            }
            if string.enum_values.is_some() && string.one_of.is_some() {
                return Err(format!(
                    "elicitation field '{name}' declares both enum and oneOf"
                ));
            }
            check_label(name, string.title.as_deref())?;
            check_label(name, string.description.as_deref())?;
            let kind = if let Some(options) = &string.enum_values {
                if options.is_empty() {
                    return Err(format!("elicitation field '{name}' has no enum options"));
                }
                if options.len() > MAX_ELICITATION_OPTIONS {
                    return Err(format!(
                        "elicitation field '{name}' has more than {MAX_ELICITATION_OPTIONS} options"
                    ));
                }
                let options = options
                    .iter()
                    .map(|value| ElicitationOption {
                        value: value.clone(),
                        title: value.clone(),
                        description: None,
                    })
                    .collect();
                ElicitationFieldKind::Enum {
                    options,
                    default: string.default.clone(),
                }
            } else if let Some(options) = &string.one_of {
                if options.is_empty() {
                    return Err(format!("elicitation field '{name}' has no enum options"));
                }
                if options.len() > MAX_ELICITATION_OPTIONS {
                    return Err(format!(
                        "elicitation field '{name}' has more than {MAX_ELICITATION_OPTIONS} options"
                    ));
                }
                let options = options
                    .iter()
                    .map(|option| {
                        check_label(name, option.description.as_deref())?;
                        Ok(ElicitationOption {
                            value: option.value.clone(),
                            title: option.title.clone(),
                            description: option.description.clone(),
                        })
                    })
                    .collect::<Result<Vec<_>, String>>()?;
                ElicitationFieldKind::Enum {
                    options,
                    default: string.default.clone(),
                }
            } else {
                if let (Some(min), Some(max)) = (string.min_length, string.max_length) {
                    if min > max {
                        return Err(format!(
                            "elicitation field '{name}' has minLength greater than maxLength"
                        ));
                    }
                }
                ElicitationFieldKind::String {
                    default: string.default.clone(),
                    min_length: string.min_length,
                    max_length: string.max_length,
                    format: match string.format {
                        Some(format) => Some(wire_string_format(name, format)?),
                        None => None,
                    },
                }
            };
            ElicitationField {
                name: name.to_string(),
                title: string.title.clone(),
                description: string.description.clone(),
                required,
                kind,
            }
        }
        ElicitationPropertySchema::Integer(integer) => {
            if let (Some(min), Some(max)) = (integer.minimum, integer.maximum) {
                if min > max {
                    return Err(format!(
                        "elicitation field '{name}' has minimum greater than maximum"
                    ));
                }
            }
            check_label(name, integer.title.as_deref())?;
            check_label(name, integer.description.as_deref())?;
            ElicitationField {
                name: name.to_string(),
                title: integer.title.clone(),
                description: integer.description.clone(),
                required,
                kind: ElicitationFieldKind::Integer {
                    default: integer.default,
                    minimum: integer.minimum,
                    maximum: integer.maximum,
                },
            }
        }
        ElicitationPropertySchema::Number(number) => {
            if number.minimum.is_some_and(|minimum| !minimum.is_finite())
                || number.maximum.is_some_and(|maximum| !maximum.is_finite())
                || number.default.is_some_and(|default| !default.is_finite())
            {
                return Err(format!(
                    "elicitation field '{name}' has a non-finite numeric constraint"
                ));
            }
            if let (Some(min), Some(max)) = (number.minimum, number.maximum) {
                if min > max {
                    return Err(format!(
                        "elicitation field '{name}' has minimum greater than maximum"
                    ));
                }
            }
            check_label(name, number.title.as_deref())?;
            check_label(name, number.description.as_deref())?;
            ElicitationField {
                name: name.to_string(),
                title: number.title.clone(),
                description: number.description.clone(),
                required,
                kind: ElicitationFieldKind::Number {
                    default: number.default,
                    minimum: number.minimum,
                    maximum: number.maximum,
                },
            }
        }
        ElicitationPropertySchema::Boolean(boolean) => {
            check_label(name, boolean.title.as_deref())?;
            check_label(name, boolean.description.as_deref())?;
            ElicitationField {
                name: name.to_string(),
                title: boolean.title.clone(),
                description: boolean.description.clone(),
                required,
                kind: ElicitationFieldKind::Boolean {
                    default: boolean.default,
                },
            }
        }
        ElicitationPropertySchema::Array(_) => {
            return Err(format!(
                "elicitation field '{name}' is a multi-select array, which Favetto does not support"
            ));
        }
        ElicitationPropertySchema::Other(other) => {
            return Err(format!(
                "elicitation field '{name}' uses unknown property type '{}'",
                other.type_
            ));
        }
        _ => {
            return Err(format!(
                "elicitation field '{name}' uses an unknown property type"
            ));
        }
    };
    check_label(name, field.title.as_deref())?;
    check_label(name, field.description.as_deref())?;
    if let Some(default) = default_value(&field.kind) {
        field.validate_value(&default).map_err(|error| {
            format!("elicitation field '{name}' has an invalid default: {error}")
        })?;
    }
    Ok(field)
}

fn wire_string_format(name: &str, format: StringFormat) -> Result<ElicitationStringFormat, String> {
    match format {
        StringFormat::Email => Ok(ElicitationStringFormat::Email),
        StringFormat::Uri => Ok(ElicitationStringFormat::Uri),
        StringFormat::Date => Ok(ElicitationStringFormat::Date),
        StringFormat::DateTime => Ok(ElicitationStringFormat::DateTime),
        _ => Err(format!(
            "elicitation field '{name}' uses an unsupported string format"
        )),
    }
}

fn default_value(kind: &ElicitationFieldKind) -> Option<ElicitationValue> {
    match kind {
        ElicitationFieldKind::String { default, .. } => {
            default.clone().map(ElicitationValue::String)
        }
        ElicitationFieldKind::Enum { default, .. } => default.clone().map(ElicitationValue::String),
        ElicitationFieldKind::Integer { default, .. } => default.map(ElicitationValue::Integer),
        ElicitationFieldKind::Number { default, .. } => default.map(ElicitationValue::Number),
        ElicitationFieldKind::Boolean { default, .. } => default.map(ElicitationValue::Boolean),
    }
}

fn check_label(name: &str, value: Option<&str>) -> Result<(), String> {
    if value.is_some_and(|value| {
        value.chars().count() > favetto_core::elicitation::MAX_ELICITATION_LABEL_LEN
    }) {
        return Err(format!(
            "elicitation field '{name}' has a label longer than {} characters",
            favetto_core::elicitation::MAX_ELICITATION_LABEL_LEN
        ));
    }
    Ok(())
}

/// The current model reported by a session setup response, if any.
fn current_model(options: Option<&[AcpSessionConfigOption]>) -> Option<String> {
    let option = model_option(options.unwrap_or(&[]))?;
    let AcpSessionConfigKind::Select(select) = &option.kind else {
        return None;
    };
    Some(select.current_value.0.to_string())
}

/// Find the model selector among a session's config options.
fn model_option(options: &[AcpSessionConfigOption]) -> Option<&AcpSessionConfigOption> {
    options.iter().find(|option| {
        option.category == Some(AcpSessionConfigCategory::Model) || option.id.0.as_ref() == "model"
    })
}

/// Select `wanted` on the model config option, returning the applied model id and
/// the complete post-selection option snapshot.
async fn apply_model(
    connection: &ConnectionTo<Agent>,
    session: &agent_client_protocol::v1::schema::NewSessionResponse,
    wanted: &str,
    deadline: Duration,
) -> anyhow::Result<(String, Vec<SessionConfigOption>)> {
    let option = session
        .config_options
        .as_deref()
        .and_then(model_option)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "this agent does not expose model selection over ACP; cannot select '{wanted}'"
            )
        })?;
    let AcpSessionConfigKind::Select(select) = &option.kind else {
        anyhow::bail!("the agent's model option is not a select");
    };
    let available: Vec<String> = match &select.options {
        AcpSelectOptions::Ungrouped(list) => {
            list.iter().map(|value| value.value.0.to_string()).collect()
        }
        AcpSelectOptions::Grouped(groups) => groups
            .iter()
            .flat_map(|group| group.options.iter().map(|value| value.value.0.to_string()))
            .collect(),
        _ => Vec::new(),
    };
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

    let response = with_deadline(
        "session/set_config_option",
        deadline,
        connection
            .send_request(SetSessionConfigOptionRequest::new(
                session.session_id.clone(),
                option.id.clone(),
                AcpSessionConfigValue::value_id(wanted.to_string()),
            ))
            .block_task(),
    )
    .await?;
    let applied = model_option(&response.config_options)
        .and_then(|option| match &option.kind {
            AcpSessionConfigKind::Select(select) => Some(select.current_value.0.to_string()),
            _ => None,
        })
        .unwrap_or_else(|| wanted.to_string());
    if applied != wanted {
        anyhow::bail!("agent kept model '{applied}' instead of '{wanted}'");
    }
    Ok((
        applied,
        normalized_config_options(Some(&response.config_options)),
    ))
}

/// Normalize the agent's complete config-option list without dropping unknown
/// categories or option kinds.
fn normalized_config_options(
    options: Option<&[AcpSessionConfigOption]>,
) -> Vec<SessionConfigOption> {
    options
        .unwrap_or(&[])
        .iter()
        .map(|option| SessionConfigOption {
            id: option.id.0.to_string(),
            name: option.name.clone(),
            description: option.description.clone(),
            category: option.category.as_ref().map(config_category_name),
            kind: match &option.kind {
                AcpSessionConfigKind::Select(select) => SessionConfigKind::Select {
                    current_value: select.current_value.0.to_string(),
                    choices: match &select.options {
                        AcpSelectOptions::Ungrouped(list) => SessionConfigChoices::Ungrouped {
                            options: list.iter().map(config_choice).collect(),
                        },
                        AcpSelectOptions::Grouped(groups) => SessionConfigChoices::Grouped {
                            groups: groups
                                .iter()
                                .map(|group| SessionConfigGroup {
                                    id: group.group.0.to_string(),
                                    name: group.name.clone(),
                                    options: group.options.iter().map(config_choice).collect(),
                                })
                                .collect(),
                        },
                        _ => SessionConfigChoices::Ungrouped {
                            options: Vec::new(),
                        },
                    },
                },
                AcpSessionConfigKind::Boolean(boolean) => SessionConfigKind::Boolean {
                    current_value: boolean.current_value,
                },
                _ => SessionConfigKind::Other,
            },
        })
        .collect()
}

fn config_choice(
    choice: &agent_client_protocol::v1::schema::SessionConfigSelectOption,
) -> SessionConfigChoice {
    SessionConfigChoice {
        value: choice.value.0.to_string(),
        name: choice.name.clone(),
        description: choice.description.clone(),
    }
}

/// Keep an unknown category's raw name instead of collapsing it to "other".
fn config_category_name(category: &AcpSessionConfigCategory) -> String {
    match category {
        AcpSessionConfigCategory::Mode => "mode".to_string(),
        AcpSessionConfigCategory::Model => "model".to_string(),
        AcpSessionConfigCategory::ModelConfig => "model_config".to_string(),
        AcpSessionConfigCategory::ThoughtLevel => "thought_level".to_string(),
        AcpSessionConfigCategory::Other(name) => name.clone(),
        _ => "other".to_string(),
    }
}

/// Normalize the complete mode state advertised at session setup.
fn normalized_modes(modes: &AcpSessionModeState) -> SessionModes {
    SessionModes {
        current_mode_id: modes.current_mode_id.0.to_string(),
        available_modes: modes
            .available_modes
            .iter()
            .map(|mode| SessionMode {
                id: mode.id.0.to_string(),
                name: mode.name.clone(),
                description: mode.description.clone(),
            })
            .collect(),
    }
}

fn acp_config_value(value: SessionConfigValue) -> AcpSessionConfigValue {
    match value {
        SessionConfigValue::Boolean { value } => AcpSessionConfigValue::boolean(value),
        SessionConfigValue::ValueId { value } => AcpSessionConfigValue::ValueId {
            value: AcpSessionConfigValueId::new(value),
        },
    }
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

/// Map a `session/prompt` response's token totals into normalized usage.
///
/// ACP v1 documents these counters as cumulative for the session, while the
/// shipped `opencode` adapter returns the usage of the turn that just finished.
/// Headless task runs submit exactly one prompt, so emitting the value as an
/// incremental step is correct for both. Multi-turn interactive sessions with a
/// cumulative adapter are a documented follow-up.
fn prompt_usage(usage: &AcpUsage) -> favetto_core::model::AgentUsage {
    favetto_core::model::AgentUsage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        reasoning_tokens: usage.thought_tokens.unwrap_or(0),
        cache_read_tokens: usage.cached_read_tokens.unwrap_or(0),
        cache_write_tokens: usage.cached_write_tokens.unwrap_or(0),
        ..Default::default()
    }
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
    load: &Option<tokio::sync::watch::Sender<Option<LoadUpdate>>>,
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
            let _ = tx.send(Some(LoadUpdate::Failed(message.to_string())));
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
        SessionUpdate::UserMessageChunk(chunk) => match &chunk.content {
            ContentBlock::Text(text) => vec![AgentStateEvent::TextDelta {
                role: MessageRole::User,
                text: text.text.clone(),
            }],
            block => content_block_event(ContentRole::User, block),
        },
        SessionUpdate::AgentMessageChunk(chunk) => match &chunk.content {
            ContentBlock::Text(text) => {
                transcript.lock().push_str(&text.text);
                {
                    let mut state = outcome.lock();
                    state.assistant_text.push_str(&text.text);
                }
                vec![AgentStateEvent::TextDelta {
                    role: MessageRole::Assistant,
                    text: text.text.clone(),
                }]
            }
            block => content_block_event(ContentRole::Assistant, block),
        },
        SessionUpdate::AgentThoughtChunk(chunk) => match &chunk.content {
            ContentBlock::Text(text) => vec![AgentStateEvent::ReasoningDelta {
                text: text.text.clone(),
            }],
            block => content_block_event(ContentRole::Thought, block),
        },
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
                reported.set_cost(Some(favetto_core::model::AgentCost::new(
                    cost.amount,
                    cost.currency.clone(),
                )));
            }
            // ACP reports the session's current cumulative state, not a delta.
            vec![AgentStateEvent::Usage {
                usage: reported,
                snapshot: true,
            }]
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
        SessionUpdate::Plan(plan) => vec![AgentStateEvent::Plan {
            entries: plan.entries.iter().map(plan_entry).collect(),
        }],
        SessionUpdate::AvailableCommandsUpdate(update) => {
            vec![AgentStateEvent::AvailableCommands {
                commands: update
                    .available_commands
                    .iter()
                    .map(available_command)
                    .collect(),
            }]
        }
        SessionUpdate::ConfigOptionUpdate(update) => vec![AgentStateEvent::ConfigOptions {
            options: normalized_config_options(Some(&update.config_options)),
        }],
        SessionUpdate::CurrentModeUpdate(update) => vec![AgentStateEvent::CurrentMode {
            mode_id: update.current_mode_id.0.to_string(),
        }],
        // Vendor-specific updates that have no normalized representation are
        // harmless. `SessionUpdate` is non-exhaustive, so this arm is required.
        _ => Vec::new(),
    }
}

/// Normalize a non-text message block into one wire event. Binary payloads are
/// never copied; clients receive only the metadata for a safe placeholder.
fn content_block_event(role: ContentRole, block: &ContentBlock) -> Vec<AgentStateEvent> {
    vec![AgentStateEvent::ContentBlock {
        role,
        content: message_content(block),
    }]
}

/// Normalize one ACP plan entry.
fn plan_entry(entry: &AcpPlanEntry) -> AgentPlanEntry {
    AgentPlanEntry {
        content: entry.content.clone(),
        priority: plan_priority(&entry.priority),
        status: plan_status(&entry.status),
    }
}

/// Normalize an ACP plan priority. A future/unknown value maps to `Unknown`.
fn plan_priority(priority: &AcpPlanPriority) -> AgentPlanPriority {
    match priority {
        AcpPlanPriority::High => AgentPlanPriority::High,
        AcpPlanPriority::Medium => AgentPlanPriority::Medium,
        AcpPlanPriority::Low => AgentPlanPriority::Low,
        _ => AgentPlanPriority::Unknown,
    }
}

/// Normalize an ACP plan status. A future/unknown value maps to `Unknown`.
fn plan_status(status: &AcpPlanStatus) -> AgentPlanStatus {
    match status {
        AcpPlanStatus::Pending => AgentPlanStatus::Pending,
        AcpPlanStatus::InProgress => AgentPlanStatus::InProgress,
        AcpPlanStatus::Completed => AgentPlanStatus::Completed,
        _ => AgentPlanStatus::Unknown,
    }
}

/// Normalize one advertised ACP command, keeping only the unstructured input
/// hint clients can display.
fn available_command(command: &AcpAvailableCommand) -> AgentCommand {
    let input_hint = match &command.input {
        Some(AcpAvailableCommandInput::Unstructured(input)) => Some(input.hint.clone()),
        _ => None,
    };
    AgentCommand {
        name: command.name.clone(),
        description: command.description.clone(),
        input_hint,
    }
}

fn message_content(block: &ContentBlock) -> MessageContent {
    match block {
        ContentBlock::Text(_) => MessageContent::Unknown,
        ContentBlock::Image(image) => MessageContent::Image {
            mime_type: image.mime_type.clone(),
            uri: image.uri.clone(),
        },
        ContentBlock::Audio(audio) => MessageContent::Audio {
            mime_type: audio.mime_type.clone(),
        },
        ContentBlock::ResourceLink(link) => MessageContent::ResourceLink {
            uri: link.uri.clone(),
            name: link.name.clone(),
            mime_type: link.mime_type.clone(),
            title: link.title.clone(),
            description: link.description.clone(),
            size: link.size,
        },
        ContentBlock::Resource(resource) => match &resource.resource {
            EmbeddedResourceResource::TextResourceContents(text) => MessageContent::Resource {
                uri: text.uri.clone(),
                mime_type: text.mime_type.clone(),
                binary: false,
            },
            EmbeddedResourceResource::BlobResourceContents(blob) => MessageContent::Resource {
                uri: blob.uri.clone(),
                mime_type: blob.mime_type.clone(),
                binary: true,
            },
            _ => MessageContent::Unknown,
        },
        _ => MessageContent::Unknown,
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

/// Read newline-delimited UTF-8, enforcing [`MAX_MESSAGE_BYTES`].
///
/// Unlike the SDK's stdio transport this never lossily rewrites a protocol
/// message: invalid UTF-8, an oversized line, and EOF in the middle of a line
/// are all surfaced as read errors that fail the session.
fn bounded_lines<R>(
    reader: R,
) -> impl futures_util::Stream<Item = io::Result<String>> + Send + 'static
where
    R: futures_util::io::AsyncRead + Send + 'static,
{
    let reader = Box::pin(BufReader::new(reader));
    futures_util::stream::try_unfold(reader, |mut reader| async move {
        match read_message(&mut reader, MAX_MESSAGE_BYTES).await? {
            Some(line) => Ok::<_, io::Error>(Some((line, reader))),
            None => Ok(None),
        }
    })
}

/// Read one newline-terminated message, without allocating past `max` bytes.
///
/// Blank lines are skipped; CRLF is accepted. A partial line at EOF is treated
/// as a truncated message rather than delivered as if it were complete.
async fn read_message<R>(reader: &mut R, max: usize) -> io::Result<Option<String>>
where
    R: futures_util::io::AsyncBufRead + Unpin,
{
    loop {
        let mut line = Vec::new();
        loop {
            let available = reader.fill_buf().await?;
            if available.is_empty() {
                if line.is_empty() {
                    return Ok(None);
                }
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "ACP message ended before its terminating newline",
                ));
            }
            let newline = available.iter().position(|byte| *byte == b'\n');
            let chunk = newline.unwrap_or(available.len());
            if line.len() + chunk > max {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("ACP message exceeds the {max} byte limit"),
                ));
            }
            match newline {
                Some(position) => {
                    line.extend_from_slice(&available[..position]);
                    reader.consume_unpin(position + 1);
                    break;
                }
                None => {
                    let consumed = available.len();
                    line.extend_from_slice(available);
                    reader.consume_unpin(consumed);
                }
            }
        }
        if line.ends_with(b"\r") {
            line.pop();
        }
        if line.is_empty() {
            continue;
        }
        return String::from_utf8(line).map(Some).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("ACP message is not valid UTF-8: {}", error.utf8_error()),
            )
        });
    }
}

/// Await a setup request, failing with an actionable message on error or timeout.
///
/// Only initialization and session setup use this; prompt turns stay unlimited.
async fn with_deadline<T, F>(what: &str, deadline: Duration, future: F) -> anyhow::Result<T>
where
    F: Future<Output = Result<T, agent_client_protocol::Error>>,
{
    match tokio::time::timeout(deadline, future).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(anyhow::anyhow!("{what} failed: {error}")),
        Err(_) => Err(anyhow::anyhow!(
            "{what} did not complete within {deadline:?}; the ACP server may be hung"
        )),
    }
}

/// Kill the agent's process group and reap the direct child.
///
/// The supervisor created by `__agent-exec` starts its own process group, so
/// the negative pid reaches a hung agent and its descendants. When no wrapper
/// was used the child is not a group leader and the group kill is a harmless
/// no-op; `start_kill` still terminates the child itself.
async fn reap_child(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // SAFETY: `kill` is async-signal-safe and accepts any pid; a negative
        // pid targets the process group `__agent-exec` created.
        unsafe {
            libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
        }
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::v1::schema::{
        AudioContent, BlobResourceContents, Content, ContentChunk, Cost, Diff, EmbeddedResource,
        EmbeddedResourceResource, ImageContent, Plan as AcpPlan, ResourceLink, Terminal,
        TextResourceContents, ToolCall, ToolCallId, ToolCallLocation, ToolCallUpdate,
        ToolCallUpdateFields, UsageUpdate,
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
    fn usage_updates_are_cumulative_snapshots_with_currency() {
        let harness = Harness::new();

        let first = harness.map(&SessionUpdate::UsageUpdate(
            UsageUpdate::new(1_000, 200_000).cost(Cost::new(0.01, "USD")),
        ));
        let second = harness.map(&SessionUpdate::UsageUpdate(
            UsageUpdate::new(2_000, 200_000).cost(Cost::new(0.02, "credits")),
        ));

        match (&first[0], &second[0]) {
            (
                AgentStateEvent::Usage {
                    usage: first,
                    snapshot: true,
                },
                AgentStateEvent::Usage {
                    usage: second,
                    snapshot: true,
                },
            ) => {
                assert_eq!(first.context_used, Some(1_000));
                assert_eq!(first.context_size, Some(200_000));
                assert_eq!(first.cost_usd, Some(0.01));
                assert_eq!(
                    second.reported_cost(),
                    Some(favetto_core::model::AgentCost::new(0.02, "credits"))
                );
                assert_eq!(second.cost_usd, None);
                assert_eq!(second.context_used, Some(2_000));
            }
            other => panic!("expected usage snapshots, got {other:?}"),
        }
    }

    #[test]
    fn prompt_response_usage_maps_every_token_class() {
        let usage = AcpUsage::new(155, 120, 30)
            .thought_tokens(5)
            .cached_read_tokens(4)
            .cached_write_tokens(2);
        let mapped = prompt_usage(&usage);
        assert_eq!(mapped.input_tokens, 120);
        assert_eq!(mapped.output_tokens, 30);
        assert_eq!(mapped.reasoning_tokens, 5);
        assert_eq!(mapped.cache_read_tokens, 4);
        assert_eq!(mapped.cache_write_tokens, 2);
        // The prompt response never fabricates a cost or a context window.
        assert_eq!(mapped.reported_cost(), None);
        assert_eq!(mapped.context_used, None);
    }

    #[test]
    fn non_text_message_chunks_preserve_role_and_metadata() {
        let harness = Harness::new();

        let user = SessionUpdate::UserMessageChunk(ContentChunk::new(ContentBlock::ResourceLink(
            ResourceLink::new("notes.txt", "file:///tmp/notes.txt").mime_type("text/plain"),
        )));
        assert_eq!(
            harness.map(&user),
            vec![AgentStateEvent::ContentBlock {
                role: ContentRole::User,
                content: MessageContent::ResourceLink {
                    uri: "file:///tmp/notes.txt".to_string(),
                    name: "notes.txt".to_string(),
                    mime_type: Some("text/plain".to_string()),
                    title: None,
                    description: None,
                    size: None,
                },
            }]
        );

        let assistant = SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Image(
            ImageContent::new("BASE64_PAYLOAD", "image/png").uri("https://example.com/a.png"),
        )));
        assert_eq!(
            harness.map(&assistant),
            vec![AgentStateEvent::ContentBlock {
                role: ContentRole::Assistant,
                content: MessageContent::Image {
                    mime_type: "image/png".to_string(),
                    uri: Some("https://example.com/a.png".to_string()),
                },
            }]
        );
        // A non-text assistant chunk is not assistant prose for success accounting.
        assert_eq!(&*harness.transcript.lock(), "");
        assert_eq!(harness.outcome.lock().assistant_text, "");

        let thought = SessionUpdate::AgentThoughtChunk(ContentChunk::new(ContentBlock::Audio(
            AudioContent::new("BASE64_AUDIO", "audio/wav"),
        )));
        assert_eq!(
            harness.map(&thought),
            vec![AgentStateEvent::ContentBlock {
                role: ContentRole::Thought,
                content: MessageContent::Audio {
                    mime_type: "audio/wav".to_string(),
                },
            }]
        );
    }

    #[test]
    fn embedded_message_resources_are_metadata_only() {
        let harness = Harness::new();
        let blob = SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Resource(
            EmbeddedResource::new(EmbeddedResourceResource::BlobResourceContents(
                BlobResourceContents::new("SECRET_BLOB", "file:///tmp/a.bin")
                    .mime_type("application/octet-stream"),
            )),
        )));
        let events = harness.map(&blob);
        match &events[0] {
            AgentStateEvent::ContentBlock {
                role: ContentRole::Assistant,
                content:
                    MessageContent::Resource {
                        uri,
                        mime_type,
                        binary: true,
                    },
            } => {
                assert_eq!(uri, "file:///tmp/a.bin");
                assert_eq!(mime_type.as_deref(), Some("application/octet-stream"));
            }
            other => panic!("expected a binary resource block, got {other:?}"),
        }
        let json = serde_json::to_value(&events).unwrap();
        assert!(
            !json.to_string().contains("SECRET_BLOB"),
            "binary payload leaked into the wire event"
        );

        let text = SessionUpdate::AgentThoughtChunk(ContentChunk::new(ContentBlock::Resource(
            EmbeddedResource::new(EmbeddedResourceResource::TextResourceContents(
                TextResourceContents::new("EMBEDDED_TEXT", "file:///tmp/a.txt"),
            )),
        )));
        let events = harness.map(&text);
        assert!(matches!(
            &events[0],
            AgentStateEvent::ContentBlock {
                role: ContentRole::Thought,
                content: MessageContent::Resource { binary: false, .. }
            }
        ));
        let json = serde_json::to_value(&events).unwrap();
        assert!(
            !json.to_string().contains("EMBEDDED_TEXT"),
            "embedded text should stay out of the metadata-only wire event"
        );
    }

    #[test]
    fn plan_update_replaces_entries_and_preserves_order() {
        let harness = Harness::new();
        let first = SessionUpdate::Plan(AcpPlan::new(vec![
            AcpPlanEntry::new("First", AcpPlanPriority::High, AcpPlanStatus::Completed),
            AcpPlanEntry::new("Second", AcpPlanPriority::Low, AcpPlanStatus::Pending),
        ]));
        assert_eq!(
            harness.map(&first),
            vec![AgentStateEvent::Plan {
                entries: vec![
                    AgentPlanEntry {
                        content: "First".to_string(),
                        priority: AgentPlanPriority::High,
                        status: AgentPlanStatus::Completed,
                    },
                    AgentPlanEntry {
                        content: "Second".to_string(),
                        priority: AgentPlanPriority::Low,
                        status: AgentPlanStatus::Pending,
                    },
                ],
            }]
        );

        // A later update is the complete new list, not an append or patch.
        let second = SessionUpdate::Plan(AcpPlan::new(vec![AcpPlanEntry::new(
            "Only",
            AcpPlanPriority::Medium,
            AcpPlanStatus::InProgress,
        )]));
        assert_eq!(
            harness.map(&second),
            vec![AgentStateEvent::Plan {
                entries: vec![AgentPlanEntry {
                    content: "Only".to_string(),
                    priority: AgentPlanPriority::Medium,
                    status: AgentPlanStatus::InProgress,
                }],
            }]
        );
        assert_eq!(&*harness.transcript.lock(), "");
    }

    #[test]
    fn plan_update_with_empty_entries_is_a_replacement() {
        let harness = Harness::new();
        assert_eq!(
            harness.map(&SessionUpdate::Plan(AcpPlan::new(Vec::new()))),
            vec![AgentStateEvent::Plan {
                entries: Vec::new(),
            }]
        );
    }

    #[test]
    fn available_commands_update_replaces_list_and_keeps_hints() {
        use agent_client_protocol::v1::schema::{
            AvailableCommand, AvailableCommandInput, AvailableCommandsUpdate,
            UnstructuredCommandInput,
        };

        let harness = Harness::new();
        let first = SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(vec![
            AvailableCommand::new("web", "Search the web").input(
                AvailableCommandInput::Unstructured(UnstructuredCommandInput::new("query")),
            ),
            AvailableCommand::new("test", "Run tests"),
        ]));
        assert_eq!(
            harness.map(&first),
            vec![AgentStateEvent::AvailableCommands {
                commands: vec![
                    AgentCommand {
                        name: "web".to_string(),
                        description: "Search the web".to_string(),
                        input_hint: Some("query".to_string()),
                    },
                    AgentCommand {
                        name: "test".to_string(),
                        description: "Run tests".to_string(),
                        input_hint: None,
                    },
                ],
            }]
        );

        let second = SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(vec![
            AvailableCommand::new("plan", "Show the plan"),
        ]));
        assert_eq!(
            harness.map(&second),
            vec![AgentStateEvent::AvailableCommands {
                commands: vec![AgentCommand {
                    name: "plan".to_string(),
                    description: "Show the plan".to_string(),
                    input_hint: None,
                }],
            }]
        );
    }

    #[test]
    fn available_commands_update_with_empty_list_clears() {
        use agent_client_protocol::v1::schema::AvailableCommandsUpdate;

        let harness = Harness::new();
        assert_eq!(
            harness.map(&SessionUpdate::AvailableCommandsUpdate(
                AvailableCommandsUpdate::new(Vec::new())
            )),
            vec![AgentStateEvent::AvailableCommands {
                commands: Vec::new(),
            }]
        );
    }

    #[test]
    fn config_option_update_preserves_grouped_choices_and_unknown_categories() {
        use agent_client_protocol::v1::schema::{
            ConfigOptionUpdate, SessionConfigGroupId, SessionConfigOptionCategory,
            SessionConfigSelectGroup, SessionConfigSelectOption, SessionConfigSelectOptions,
        };

        let harness = Harness::new();
        let select = AcpSessionConfigOption::select(
            "thought",
            "Thought level",
            "medium",
            SessionConfigSelectOptions::Grouped(vec![SessionConfigSelectGroup::new(
                SessionConfigGroupId::new("main"),
                "Main",
                vec![
                    SessionConfigSelectOption::new("low", "Low"),
                    SessionConfigSelectOption::new("medium", "Medium"),
                    SessionConfigSelectOption::new("high", "High"),
                ],
            )]),
        )
        .description("Reasoning depth")
        .category(SessionConfigOptionCategory::ThoughtLevel);
        let vendor = AcpSessionConfigOption::boolean("vendor-toggle", "Vendor toggle", true)
            .category(SessionConfigOptionCategory::Other("vendor_custom".into()));
        assert_eq!(
            harness.map(&SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(
                vec![select, vendor]
            ))),
            vec![AgentStateEvent::ConfigOptions {
                options: vec![
                    SessionConfigOption {
                        id: "thought".to_string(),
                        name: "Thought level".to_string(),
                        description: Some("Reasoning depth".to_string()),
                        category: Some("thought_level".to_string()),
                        kind: SessionConfigKind::Select {
                            current_value: "medium".to_string(),
                            choices: SessionConfigChoices::Grouped {
                                groups: vec![SessionConfigGroup {
                                    id: "main".to_string(),
                                    name: "Main".to_string(),
                                    options: vec![
                                        SessionConfigChoice {
                                            value: "low".to_string(),
                                            name: "Low".to_string(),
                                            description: None,
                                        },
                                        SessionConfigChoice {
                                            value: "medium".to_string(),
                                            name: "Medium".to_string(),
                                            description: None,
                                        },
                                        SessionConfigChoice {
                                            value: "high".to_string(),
                                            name: "High".to_string(),
                                            description: None,
                                        },
                                    ],
                                }],
                            },
                        },
                    },
                    SessionConfigOption {
                        id: "vendor-toggle".to_string(),
                        name: "Vendor toggle".to_string(),
                        description: None,
                        category: Some("vendor_custom".to_string()),
                        kind: SessionConfigKind::Boolean {
                            current_value: true,
                        },
                    },
                ],
            }]
        );
    }

    #[test]
    fn current_mode_update_maps_to_a_focused_mode_event() {
        use agent_client_protocol::v1::schema::{CurrentModeUpdate, SessionModeId};

        let harness = Harness::new();
        assert_eq!(
            harness.map(&SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(
                SessionModeId::new("plan")
            ))),
            vec![AgentStateEvent::CurrentMode {
                mode_id: "plan".to_string(),
            }]
        );
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
    async fn framing_rejects_invalid_utf8_instead_of_rewriting_it() {
        let mut reader = BufReader::new(futures_util::io::Cursor::new(vec![
            0x7b, 0xff, 0xfe, 0x7d, b'\n',
        ]));
        let error = read_message(&mut reader, 64)
            .await
            .expect_err("invalid UTF-8 must be a read error");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("not valid UTF-8"), "{error}");
    }

    #[tokio::test]
    async fn framing_rejects_an_oversized_line_before_buffering_it() {
        let mut reader = BufReader::new(futures_util::io::Cursor::new(vec![b'x'; 65]));
        let error = read_message(&mut reader, 64)
            .await
            .expect_err("an oversized line must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(
            error.to_string().contains("exceeds the 64 byte limit"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn framing_accepts_a_line_exactly_at_the_limit() {
        let mut input = vec![b'x'; 64];
        input.push(b'\n');
        let mut reader = BufReader::new(futures_util::io::Cursor::new(input));
        let line = read_message(&mut reader, 64)
            .await
            .expect("a line at the limit is valid")
            .expect("one message");
        assert_eq!(line.len(), 64);
    }

    #[tokio::test]
    async fn framing_rejects_eof_in_the_middle_of_a_message() {
        let mut reader = BufReader::new(futures_util::io::Cursor::new(
            b"{\"jsonrpc\":\"2.0\"".to_vec(),
        ));
        let error = read_message(&mut reader, 1024)
            .await
            .expect_err("a truncated message must fail");
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        assert!(error.to_string().contains("terminating newline"), "{error}");
    }

    #[tokio::test]
    async fn framing_handles_fragmented_reads_crlf_and_blank_lines() {
        let (mut writer, reader) = tokio::io::duplex(8);
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt as _;
            for chunk in [&b"{\"a\""[..], b":1}\r", b"\n\n{\"b\":2}", b"\n"] {
                writer.write_all(chunk).await.unwrap();
            }
            writer.shutdown().await.unwrap();
        });

        let mut lines = Box::pin(bounded_lines(reader.compat()));
        assert_eq!(lines.next().await.unwrap().unwrap(), "{\"a\":1}");
        assert_eq!(lines.next().await.unwrap().unwrap(), "{\"b\":2}");
        assert!(lines.next().await.is_none());
    }

    #[tokio::test]
    async fn a_setup_request_times_out_with_the_method_and_deadline() {
        let pending = std::future::pending::<Result<(), agent_client_protocol::Error>>();
        let error = with_deadline("initialize", Duration::from_millis(10), pending)
            .await
            .expect_err("a hung setup request must time out");
        let message = error.to_string();
        assert!(message.contains("initialize"), "{message}");
        assert!(message.contains("did not complete within"), "{message}");
        assert!(message.contains("10ms"), "{message}");
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
            setup_timeout: Duration::from_secs(1),
            session: None,
            start_gate: None,
            lifecycle: None,
            mcp_servers: Vec::new(),
            mcp_state: Arc::new(Mutex::new(McpSessionState::default())),
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
    fn only_restore_modes_use_the_load_gate() {
        assert!(!AcpSessionMode::New.is_restore());
        assert!(AcpSessionMode::Load {
            external_session_id: "provider-1".to_string()
        }
        .is_restore());
        assert!(AcpSessionMode::ProviderResume {
            external_session_id: "provider-1".to_string()
        }
        .is_restore());
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
    fn terminal_auth_methods_keep_metadata_but_are_never_supported() {
        let methods = vec![
            AuthMethod::Agent(agent_client_protocol::v1::schema::AuthMethodAgent::new(
                "agent-login",
                "Sign in",
            )),
            AuthMethod::Terminal(
                agent_client_protocol::v1::schema::AuthMethodTerminal::new(
                    "terminal-login",
                    "Terminal sign in",
                )
                .env(HashMap::from([(
                    "TERMINAL_AUTH_SECRET".to_string(),
                    "never-expose-this".to_string(),
                )])),
            ),
        ];
        let wire = wire_auth_methods(&methods);
        assert_eq!(wire.len(), 2);
        assert_eq!(wire[0].id, "agent-login");
        assert!(wire[0].supported);
        assert_eq!(wire[0].kind, AgentAuthMethodKind::Agent);
        assert_eq!(wire[1].kind, AgentAuthMethodKind::Terminal);
        assert!(!wire[1].supported);
        let json = serde_json::to_string(&wire).unwrap();
        assert!(!json.contains("never-expose-this"));
        assert!(!json.contains("TERMINAL_AUTH_SECRET"));
    }

    #[test]
    fn terminal_auth_is_not_advertised_to_agents() {
        let request = InitializeRequest::new(ProtocolVersion::V1);
        assert!(!request.client_capabilities.auth.terminal);
    }

    #[test]
    fn auth_required_is_retryable_while_other_setup_errors_are_fatal() {
        let required = setup_error(
            || "create session".to_string(),
            agent_client_protocol::Error::auth_required(),
        );
        assert!(matches!(required, SessionSetup::AuthRequired));
        let fatal = setup_error(
            || "create session".to_string(),
            agent_client_protocol::Error::internal_error(),
        );
        match fatal {
            SessionSetup::Fatal(message) => assert!(message.contains("create session")),
            SessionSetup::AuthRequired => panic!("internal error must be fatal"),
        }
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
            agent: "test-agent".to_string(),
            session_id: Arc::new(Mutex::new(Some("s1".to_string()))),
            pending_elicitations: Mutex::new(HashMap::new()),
            next_elicitation: AtomicU64::new(1),
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

    fn elicitation_schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "title": "Profile",
            "properties": {
                "name": { "type": "string", "minLength": 2 },
                "color": { "type": "string", "enum": ["red", "blue"] },
                "count": { "type": "integer", "minimum": 1, "maximum": 9 },
                "ratio": { "type": "number", "minimum": 0.0, "maximum": 1.0 },
                "enabled": { "type": "boolean", "default": true }
            },
            "required": ["name"]
        })
    }

    fn form_request(schema: serde_json::Value) -> CreateElicitationRequest {
        serde_json::from_value(serde_json::json!({
            "mode": "form",
            "sessionId": "s1",
            "message": "Please fill in the form",
            "requestedSchema": schema
        }))
        .expect("valid form request")
    }

    async fn next_elicitation_requested(
        rx: &mut mpsc::UnboundedReceiver<AcpEvent>,
    ) -> (String, ElicitationRequest) {
        match rx.recv().await.expect("state event") {
            AcpEvent::State(AgentStateEvent::ElicitationRequested { request }) => {
                (request.id.clone(), request)
            }
            other => panic!("expected ElicitationRequested, got {other:?}"),
        }
    }

    async fn next_elicitation_resolved(
        rx: &mut mpsc::UnboundedReceiver<AcpEvent>,
    ) -> (String, ElicitationOutcome) {
        match rx.recv().await.expect("state event") {
            AcpEvent::State(AgentStateEvent::ElicitationResolved { id, outcome }) => (id, outcome),
            other => panic!("expected ElicitationResolved, got {other:?}"),
        }
    }

    #[test]
    fn client_capabilities_advertise_form_elicitation_and_boolean_config_options() {
        let json = serde_json::to_value(client_capabilities()).unwrap();
        assert_eq!(json["elicitation"]["form"], serde_json::json!({}));
        assert!(
            json["elicitation"].get("url").is_none(),
            "URL mode must stay unadvertised: {json}"
        );
        assert_eq!(
            json["session"]["configOptions"]["boolean"],
            serde_json::json!({}),
            "boolean config options must be advertised: {json}"
        );
    }

    #[test]
    fn wire_form_maps_supported_fields_and_rejects_unsupported_schemas() {
        let request = form_request(elicitation_schema());
        let ElicitationMode::Form(mode) = &request.mode else {
            panic!("form mode");
        };
        let form = wire_form(&mode.requested_schema).expect("supported form");
        assert_eq!(form.title.as_deref(), Some("Profile"));
        let names: Vec<_> = form
            .fields
            .iter()
            .map(|field| field.name.as_str())
            .collect();
        assert_eq!(names, ["color", "count", "enabled", "name", "ratio"]);
        assert!(
            form.fields
                .iter()
                .find(|field| field.name == "name")
                .unwrap()
                .required
        );

        for (index, schema) in [
            serde_json::json!({
                "type": "object",
                "properties": { "tags": { "type": "array", "items": { "type": "string", "enum": ["a", "b"] } } }
            }),
            serde_json::json!({
                "type": "object",
                "properties": { "name": { "type": "string", "pattern": "^a" } }
            }),
            serde_json::json!({
                "type": "object",
                "properties": { "name": { "type": "string" } },
                "required": ["missing"]
            }),
            serde_json::json!({
                "type": "object",
                "properties": { "count": { "type": "integer", "default": 99, "maximum": 9 } }
            }),
            serde_json::json!({
                "type": "object",
                "properties": { "future": { "type": "object" } }
            }),
        ]
        .into_iter()
        .enumerate()
        {
            let schema: ElicitationSchema = serde_json::from_value(schema)
                .unwrap_or_else(|error| panic!("schema {index} did not parse: {error}"));
            assert!(
                wire_form(&schema).is_err(),
                "unsupported schema {index} must be rejected"
            );
        }
    }

    #[tokio::test]
    async fn invalid_accept_keeps_the_request_pending_until_corrected() {
        let (responder, mut rx, turn) = ask_responder(Duration::from_secs(60));
        turn.lock().begin();
        let request = form_request(elicitation_schema());
        let handle = {
            let responder = responder.clone();
            tokio::spawn(async move { responder.decide_elicitation(&request).await })
        };
        let (id, emitted) = next_elicitation_requested(&mut rx).await;
        assert_eq!(emitted.agent, "test-agent");
        assert_eq!(emitted.form.fields.len(), 5);

        let mut values = BTreeMap::new();
        values.insert(
            "name".to_string(),
            ElicitationValue::String("x".to_string()),
        );
        let error = responder
            .reply_elicitation(&id, ElicitationReply::Accept { values })
            .await
            .expect_err("short required value must be rejected");
        assert!(matches!(error, ElicitationReplyError::Invalid(_)));
        assert!(
            responder.pending_elicitations.lock().contains_key(&id),
            "an invalid reply must keep the form open"
        );

        let mut values = BTreeMap::new();
        values.insert(
            "name".to_string(),
            ElicitationValue::String("Ada".to_string()),
        );
        values.insert(
            "color".to_string(),
            ElicitationValue::String("blue".to_string()),
        );
        values.insert("count".to_string(), ElicitationValue::Integer(3));
        responder
            .reply_elicitation(&id, ElicitationReply::Accept { values })
            .await
            .expect("corrected reply");
        let response = handle.await.expect("handler").expect("response");
        match response.action {
            ElicitationAction::Accept(accept) => {
                let content = accept.content.expect("content");
                assert_eq!(
                    content.get("name"),
                    Some(&ElicitationContentValue::String("Ada".to_string()))
                );
                assert_eq!(
                    content.get("count"),
                    Some(&ElicitationContentValue::Integer(3))
                );
            }
            other => panic!("expected accept, got {other:?}"),
        }
        assert_eq!(
            next_elicitation_resolved(&mut rx).await,
            (id.clone(), ElicitationOutcome::Accepted)
        );
        assert!(matches!(
            responder
                .reply_elicitation(&id, ElicitationReply::Decline)
                .await,
            Err(ElicitationReplyError::NoPending(_))
        ));
    }

    #[tokio::test]
    async fn timeout_and_cancel_turn_never_accept() {
        let (responder, mut rx, turn) = ask_responder(Duration::from_millis(20));
        turn.lock().begin();
        let response = responder
            .decide_elicitation(&form_request(elicitation_schema()))
            .await
            .expect("response");
        assert!(matches!(response.action, ElicitationAction::Cancel));
        let (_, _) = next_elicitation_requested(&mut rx).await;
        let (_, outcome) = next_elicitation_resolved(&mut rx).await;
        assert_eq!(outcome, ElicitationOutcome::Cancelled);

        let (responder, mut rx, turn) = ask_responder(Duration::from_secs(60));
        let generation = turn.lock().begin();
        let request = form_request(elicitation_schema());
        let handle = {
            let responder = responder.clone();
            tokio::spawn(async move { responder.decide_elicitation(&request).await })
        };
        let (id, _) = next_elicitation_requested(&mut rx).await;
        assert_eq!(responder.cancel_turn(generation), 1);
        let response = handle.await.expect("handler").expect("response");
        assert!(matches!(response.action, ElicitationAction::Cancel));
        assert_eq!(
            next_elicitation_resolved(&mut rx).await,
            (id, ElicitationOutcome::Cancelled)
        );
    }

    #[tokio::test]
    async fn an_elicitation_during_cancellation_is_answered_without_an_event() {
        let (responder, mut rx, turn) = ask_responder(Duration::from_secs(60));
        turn.lock().begin();
        assert!(turn.lock().request_cancel().is_some());
        let response = responder
            .decide_elicitation(&form_request(elicitation_schema()))
            .await
            .expect("response");
        assert!(matches!(response.action, ElicitationAction::Cancel));
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn url_scope_and_wrong_session_are_rejected_explicitly() {
        let (responder, _rx, _turn) = ask_responder(Duration::from_secs(1));
        let url: CreateElicitationRequest = serde_json::from_value(serde_json::json!({
            "mode": "url",
            "sessionId": "s1",
            "message": "Open this",
            "elicitationId": "e1",
            "url": "https://example.com"
        }))
        .unwrap();
        assert!(responder.decide_elicitation(&url).await.is_err());

        let wrong_session: CreateElicitationRequest = serde_json::from_value(serde_json::json!({
            "mode": "form",
            "sessionId": "other",
            "message": "Fill",
            "requestedSchema": elicitation_schema()
        }))
        .unwrap();
        let error = responder
            .decide_elicitation(&wrong_session)
            .await
            .expect_err("another session must not be able to prompt");
        assert!(error.contains("another ACP session"), "{error}");
    }
}
