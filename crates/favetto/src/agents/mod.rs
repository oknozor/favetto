//! External-agent sessions.
//!
//! Favetto does not implement an agent loop: it launches an Agent Client
//! Protocol (ACP) server in a child process, speaks JSON-RPC over its
//! stdin/stdout, and normalizes the agent's structured updates into
//! [`AgentStateEvent`]s. There is no PTY and no per-vendor adapter: choosing
//! another agent is a `[agents.<name>]` configuration change.
//!
//! One prompt turn (`session/prompt`) is the unit of task execution. A headless
//! session closes itself when its first turn ends; an interactive session stays
//! open for follow-up prompts. See [`acp`] for the transport and success rules.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::broadcast;

use favetto_core::model::{
    AgentActivity, AgentBackend, AgentCapabilities, AgentSessionInfo, AgentStateEvent, AgentUsage,
    AwaitingInputReason, InputReply, PermissionPolicy,
};
use favetto_core::rpc::AgentEventPush;

use crate::config::{AgentConfig, FavettoConfig, GitSettings};
use crate::event_bus::{EventBus, ServerPush};

pub use acp::AcpOutcome;
pub use registry::AgentRegistry;

mod acp;
mod registry;

/// How many normalized events are kept per session for feed backfill
/// (`tasks.events`). The live feed arrives over the bus; this is only the
/// bounded replay window after a reload or reconnect.
const FEED_CAP: usize = 512;

/// Identity and resolved executable of an agent.
#[derive(Debug, Clone)]
pub struct AgentDescriptor {
    /// Configured/built-in key, e.g. `opencode`.
    pub id: String,
    /// Human-readable name, e.g. `OpenCode (ACP)`.
    pub name: String,
    /// Resolved executable.
    pub command: String,
    /// Whether the executable was found on the daemon's PATH.
    pub available: bool,
    /// What the agent can do.
    pub capabilities: AgentCapabilities,
}

/// A configured ACP server.
#[derive(Debug, Clone)]
pub struct Agent {
    descriptor: AgentDescriptor,
    config: AgentConfig,
}

impl Agent {
    /// Build an agent from its `[agents.<name>]` entry.
    pub fn from_config(name: &str, config: &AgentConfig) -> Self {
        let descriptor = AgentDescriptor {
            id: name.to_string(),
            name: format!("{name} (ACP)"),
            command: config.command.clone(),
            available: true,
            capabilities: AgentCapabilities {
                acp: true,
                interactive: true,
                headless: true,
                structured_output: true,
                reports_state: true,
                permission_channel: true,
                interactive_prompt: true,
                ..Default::default()
            },
        };
        Self {
            descriptor,
            config: config.clone(),
        }
    }

    pub fn descriptor(&self) -> &AgentDescriptor {
        &self.descriptor
    }

    fn set_available(&mut self, available: bool) {
        self.descriptor.available = available;
    }

    /// The server command, without any prompt: prompts travel over ACP.
    fn launch_spec(&self) -> anyhow::Result<LaunchSpec> {
        if self.config.command.trim().is_empty() {
            anyhow::bail!(
                "agent '{}' has no command configured (it must launch an ACP server)",
                self.descriptor.id
            );
        }
        Ok(LaunchSpec {
            program: PathBuf::from(&self.config.command),
            args: self.config.args.clone(),
            env: self.config.env.clone(),
            cwd: self.config.cwd.clone(),
        })
    }
}

/// The process to spawn for one session.
struct LaunchSpec {
    program: PathBuf,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    cwd: Option<PathBuf>,
}

/// Everything a launch needs beyond the agent's command.
pub struct Launch {
    /// Catalog task this session is attached to, if any.
    pub task_id: Option<String>,
    /// Prompt to send once the session is ready.
    pub prompt: Option<String>,
    /// Working directory for the agent process and its session.
    pub cwd: PathBuf,
    /// Per-task permission policy; `None` inherits the agent's, then `ask`.
    pub permission: Option<PermissionPolicy>,
    /// Per-task `provider/model` selection, applied when the agent exposes a
    /// model config option over ACP.
    pub model: Option<String>,
    /// Unattended run: close the session when the first turn ends.
    pub headless: bool,
    /// `[git]` provisioning variables (signing identity, no interactive
    /// signers) for the agent's child environment.
    pub git_env: BTreeMap<String, String>,
}

/// A session event broadcast to attached connections.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// The session's process ended.
    Exit {
        session_id: String,
        code: Option<i32>,
    },
    /// The session's folded live state changed (activity/usage).
    State {
        session_id: String,
        live: AgentLiveState,
    },
}

impl AgentEvent {
    pub fn session_id(&self) -> &str {
        match self {
            AgentEvent::Exit { session_id, .. } | AgentEvent::State { session_id, .. } => {
                session_id
            }
        }
    }
}

/// The folded live snapshot of a session: what it is doing and what it cost.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AgentLiveState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<AgentActivity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<AgentUsage>,
}

impl AgentLiveState {
    /// Fold one normalized event into the snapshot.
    pub fn apply(&mut self, event: &AgentStateEvent) {
        match event {
            AgentStateEvent::TurnStarted | AgentStateEvent::ReasoningDelta { .. } => {
                self.activity = Some(AgentActivity::Thinking);
            }
            AgentStateEvent::TextDelta { .. } => {
                self.activity = Some(AgentActivity::Responding);
            }
            AgentStateEvent::ToolStarted { name, .. } => {
                self.activity = Some(AgentActivity::Tool {
                    name: name.clone(),
                    description: None,
                });
            }
            AgentStateEvent::ToolUpdated { .. } => {}
            AgentStateEvent::ToolFinished { .. } => {
                self.activity = Some(AgentActivity::Thinking);
            }
            AgentStateEvent::InputRequested { request } => {
                self.activity = Some(AgentActivity::Waiting {
                    request: request.clone(),
                });
            }
            AgentStateEvent::InputResolved { .. } | AgentStateEvent::Idle { .. } => {
                self.activity = Some(AgentActivity::Idle);
            }
            AgentStateEvent::Usage { usage } => {
                self.usage
                    .get_or_insert_with(AgentUsage::default)
                    .merge(usage);
            }
            AgentStateEvent::Error { .. } => {}
            AgentStateEvent::Session { .. } | AgentStateEvent::Title { .. } => {}
        }
    }
}

/// A live ACP session.
struct Session {
    id: String,
    agent: String,
    task_id: Option<String>,
    running: Arc<AtomicBool>,
    exit_code: Arc<AtomicI32>,
    external_session_id: Arc<Mutex<Option<String>>>,
    headless: bool,
    last_activity: Arc<Mutex<std::time::Instant>>,
    awaiting_input: Arc<Mutex<Option<AwaitingInputReason>>>,
    live: Arc<Mutex<AgentLiveState>>,
    responder: Arc<acp::AcpResponder>,
    state_tx: broadcast::Sender<AgentStateEvent>,
    feed: Arc<Mutex<VecDeque<(u64, AgentStateEvent)>>>,
    next_seq: Arc<AtomicU64>,
    order: u64,
    handles: acp::AcpHandles,
}

impl Session {
    fn info(&self) -> AgentSessionInfo {
        let live = self.live.lock();
        AgentSessionInfo {
            id: self.id.clone(),
            agent: self.agent.clone(),
            backend: AgentBackend::Acp,
            task_id: self.task_id.clone(),
            running: self.running.load(Ordering::SeqCst),
            headless: self.headless,
            session_id: self.external_session_id.lock().clone(),
            awaiting_input: self.awaiting_input.lock().clone(),
            activity: live.activity.clone(),
            usage: live.usage.clone(),
        }
    }
}

/// `[git]` settings resolved once at startup, keyed by agent name.
struct GitRuntime {
    data_dir: PathBuf,
    default: GitSettings,
    per_agent: HashMap<String, GitSettings>,
}

impl Default for GitRuntime {
    fn default() -> Self {
        Self {
            data_dir: std::env::temp_dir(),
            default: GitSettings::default(),
            per_agent: HashMap::new(),
        }
    }
}

/// Owns every live ACP session on the daemon.
pub struct AgentManager {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    tx: broadcast::Sender<AgentEvent>,
    next_order: AtomicU64,
    git: GitRuntime,
    /// The daemon event bus, attached in `State::new` so normalized events are
    /// published as `agent.event` pushes for the web feed.
    bus: Option<EventBus>,
}

impl Default for AgentManager {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for AgentManager {
    fn drop(&mut self) {
        let mut sessions = self.sessions.lock();
        for (_, session) in sessions.drain() {
            session.running.store(false, Ordering::SeqCst);
            session.handles.shutdown();
        }
    }
}

impl AgentManager {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(256);
        Self {
            sessions: Mutex::new(HashMap::new()),
            tx,
            next_order: AtomicU64::new(0),
            git: GitRuntime::default(),
            bus: None,
        }
    }

    /// Attach the daemon event bus so normalized events are published as
    /// `agent.event` pushes. Called once from `State::new`.
    pub fn set_bus(&mut self, bus: EventBus) {
        self.bus = Some(bus);
    }

    /// Resolve the global and per-agent `[git]` settings once at startup, so a
    /// bad `[git]` section fails daemon startup rather than the first launch.
    pub fn configure_git(
        &mut self,
        config: &FavettoConfig,
        data_dir: PathBuf,
    ) -> anyhow::Result<()> {
        self.git.data_dir = data_dir;
        self.git.default = config.git.clone();
        self.git.per_agent.clear();

        crate::git::resolve_env(&self.git.default, &self.git.data_dir)?;
        for (name, agent) in &config.agents {
            let mut merged = config.git.clone();
            if let Some(over) = &agent.git {
                merged = crate::git::overlay(&merged, over);
            }
            crate::git::resolve_env(&merged, &self.git.data_dir)?;
            self.git.per_agent.insert(name.clone(), merged);
        }
        Ok(())
    }

    /// The git environment for a launch, resolved without touching the
    /// operator's real git config.
    fn git_env(
        &self,
        name: &str,
        git_signing: Option<crate::config::GitSigning>,
    ) -> anyhow::Result<BTreeMap<String, String>> {
        let mut settings = self
            .git
            .per_agent
            .get(name)
            .cloned()
            .unwrap_or_else(|| self.git.default.clone());
        if let Some(mode) = git_signing {
            settings.signing = Some(mode);
        }
        Ok(crate::git::resolve_env(&settings, &self.git.data_dir)?
            .into_iter()
            .collect())
    }

    /// Subscribe to every session's lifecycle events; callers filter by id.
    pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.tx.subscribe()
    }

    /// Snapshot of all live (and recently exited) sessions.
    pub fn sessions(&self) -> Vec<AgentSessionInfo> {
        let sessions = self.sessions.lock();
        let mut out: Vec<_> = sessions.values().map(|s| s.info()).collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// The most recently created session attached to `task_id`, running or not.
    pub fn find_latest_by_task(&self, task_id: &str) -> Option<AgentSessionInfo> {
        self.sessions
            .lock()
            .values()
            .filter(|s| s.task_id.as_deref() == Some(task_id))
            .max_by_key(|s| s.order)
            .map(|s| s.info())
    }

    /// The backfill feed for `task_id`'s most recent session.
    pub fn events_for_task(
        &self,
        task_id: &str,
        after: u64,
        limit: usize,
    ) -> (Option<String>, Vec<AgentEventPush>, u64) {
        let sessions = self.sessions.lock();
        let Some(session) = sessions
            .values()
            .filter(|s| s.task_id.as_deref() == Some(task_id))
            .max_by_key(|s| s.order)
        else {
            return (None, Vec::new(), after);
        };
        let session_id = session.id.clone();
        let feed = session.feed.lock();
        let mut events: Vec<AgentEventPush> = feed
            .iter()
            .filter(|(seq, _)| *seq > after)
            .map(|(seq, event)| AgentEventPush {
                session_id: session_id.clone(),
                task_id: session.task_id.clone(),
                seq: *seq,
                event: event.clone(),
            })
            .collect();
        if events.len() > limit {
            let excess = events.len() - limit;
            events.drain(0..excess);
        }
        let next_cursor = events.last().map_or(after, |event| event.seq);
        (Some(session_id), events, next_cursor)
    }

    /// Whether a live interactive session is attached to `task_id`.
    pub fn has_live_interactive(&self, task_id: &str) -> bool {
        self.sessions.lock().values().any(|s| {
            s.task_id.as_deref() == Some(task_id) && !s.headless && s.running.load(Ordering::SeqCst)
        })
    }

    /// Spawn an ACP session.
    pub fn start(
        &self,
        name: &str,
        agent: Arc<Agent>,
        launch: Launch,
    ) -> anyhow::Result<AgentSessionInfo> {
        let mut spec = agent.launch_spec()?;
        spec.env.extend(self.git_env(name, None)?);
        spec.env.extend(launch.git_env.clone());

        let mut program = spec.program.clone();
        let mut args = spec.args.clone();
        // Keep the daemon's process-group supervision: a daemon SIGTERM becomes
        // a group kill, so no ACP server outlives it. If the running binary has
        // been replaced or removed (e.g. mid `cargo install`), `current_exe()`
        // may point at a path that no longer exists; degrade to a direct launch
        // rather than failing every task.
        if wrap_agent() {
            match agent_exec_program().ok().filter(|path| is_executable(path)) {
                Some(wrapper) => {
                    let mut wrapped = vec![
                        "__agent-exec".to_string(),
                        "--".to_string(),
                        program.to_string_lossy().into_owned(),
                    ];
                    wrapped.extend(args.iter().cloned());
                    program = wrapper;
                    args = wrapped;
                }
                None => tracing::warn!(
                    "cannot locate the running favetto binary; launching agent '{name}' without \
                     the process-group supervisor"
                ),
            }
        }

        let cwd = spec.cwd.clone().unwrap_or(launch.cwd);
        let (handles, events) = acp::spawn(acp::AcpLaunch {
            agent: name.to_string(),
            program,
            args,
            env: spec.env,
            cwd,
            // The task's own policy wins; the agent entry is only a default.
            policy: launch
                .permission
                .or(agent.config.permission)
                .unwrap_or_default(),
            model: launch.model,
            prompt: launch.prompt,
            close_after_turn: launch.headless,
            permission_timeout: acp::PERMISSION_TIMEOUT,
        })?;

        let id = uuid::Uuid::new_v4().to_string();
        let order = self.next_order.fetch_add(1, Ordering::SeqCst);
        let (state_tx, _) = broadcast::channel(256);
        let session = Arc::new(Session {
            id: id.clone(),
            agent: name.to_string(),
            task_id: launch.task_id,
            running: handles.running.clone(),
            exit_code: handles.exit_code.clone(),
            external_session_id: handles.external_session_id.clone(),
            headless: launch.headless,
            last_activity: handles.last_activity.clone(),
            awaiting_input: Arc::new(Mutex::new(None)),
            live: Arc::new(Mutex::new(AgentLiveState {
                activity: Some(AgentActivity::Starting),
                usage: None,
            })),
            responder: handles.responder.clone(),
            state_tx: state_tx.clone(),
            feed: Arc::new(Mutex::new(VecDeque::new())),
            next_seq: Arc::new(AtomicU64::new(1)),
            order,
            handles,
        });
        spawn_state_task(session.clone(), self.tx.clone(), events, self.bus.clone());
        let info = session.info();
        self.sessions.lock().insert(id, session);
        Ok(info)
    }

    /// A (re)attaching client's current view: the session plus its plain-text
    /// transcript. Clients render structured `agent.event` updates for the live
    /// conversation; this is the replay seed and the captured run output.
    pub fn attach(&self, session_id: &str) -> anyhow::Result<(AgentSessionInfo, Vec<u8>)> {
        let session = self.get(session_id)?;
        let frame = session
            .handles
            .transcript
            .lock()
            .replace('\n', "\r\n")
            .into_bytes();
        Ok((session.info(), frame))
    }

    /// Wait for a session to stop running (the turn ended and the session
    /// closed, or the server exited); returns its exit code.
    pub async fn wait(&self, session_id: &str) -> Option<i32> {
        loop {
            let (running, code) = match self.get(session_id) {
                Ok(s) => (
                    s.running.load(Ordering::SeqCst),
                    s.exit_code.load(Ordering::SeqCst),
                ),
                Err(_) => return None,
            };
            if !running {
                return Some(code);
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    /// The session's captured transcript (assistant text plus tool summaries).
    pub fn output(&self, session_id: &str) -> String {
        self.get(session_id)
            .map(|s| s.handles.transcript.lock().clone())
            .unwrap_or_default()
    }

    /// The structured turn outcome for a session.
    pub fn acp_outcome(&self, session_id: &str) -> Option<AcpOutcome> {
        Some(self.get(session_id).ok()?.handles.outcome.lock().clone())
    }

    /// The agent's own session id, if it reported one.
    pub fn external_session_id(&self, session_id: &str) -> Option<String> {
        self.get(session_id)
            .ok()
            .and_then(|s| s.external_session_id.lock().clone())
    }

    /// Send a follow-up prompt.
    pub fn prompt(&self, session_id: &str, prompt: &str) -> anyhow::Result<()> {
        self.get(session_id)?
            .handles
            .prompts
            .send(prompt.to_string())
            .map_err(|_| {
                anyhow::anyhow!("agent session '{session_id}' is no longer accepting prompts")
            })
    }

    /// Interrupt the in-flight turn.
    pub fn cancel(&self, session_id: &str) -> anyhow::Result<()> {
        self.get(session_id)?
            .handles
            .commands
            .send(acp::AcpCommand::Cancel)
            .map_err(|_| anyhow::anyhow!("agent session '{session_id}' is no longer running"))
    }

    /// Answer a structured input (permission) request.
    pub async fn reply(
        &self,
        session_id: &str,
        request_id: &str,
        reply: InputReply,
    ) -> anyhow::Result<()> {
        self.get(session_id)?
            .responder
            .reply(request_id, reply)
            .await
    }

    /// Terminate a session and drop it from the registry.
    pub fn close(&self, session_id: &str) -> anyhow::Result<()> {
        if let Some(session) = self.sessions.lock().remove(session_id) {
            session.running.store(false, Ordering::SeqCst);
            session.handles.shutdown();
        }
        Ok(())
    }

    /// Terminate every live session attached to `task_id`.
    pub fn close_by_task(&self, task_id: &str) {
        let ids: Vec<String> = self
            .sessions
            .lock()
            .values()
            .filter(|session| session.task_id.as_deref() == Some(task_id))
            .map(|session| session.id.clone())
            .collect();
        for id in ids {
            let _ = self.close(&id);
        }
    }

    fn get(&self, session_id: &str) -> anyhow::Result<Arc<Session>> {
        self.sessions
            .lock()
            .get(session_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no agent session '{session_id}'"))
    }
}

/// The favetto binary itself, used as the `__agent-exec` supervisor wrapper.
fn agent_exec_program() -> anyhow::Result<PathBuf> {
    std::env::current_exe().map_err(|e| anyhow::anyhow!("cannot locate favetto binary: {e}"))
}

/// The ACP model selector for a task: `provider/model` when both are given,
/// the bare model id otherwise.
pub fn model_selector(provider: Option<&str>, model: Option<&str>) -> Option<String> {
    let model = model?;
    Some(match provider.filter(|provider| !provider.is_empty()) {
        Some(provider) => format!("{provider}/{model}"),
        None => model.to_string(),
    })
}

/// Whether `path` is a file the current user may execute.
fn is_executable(path: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Whether to launch agents through the `__agent-exec` wrapper. Disabled under
/// unit tests (where the current exe is the test harness) and when
/// `FAVETTO_NO_AGENT_WRAP` is set.
fn wrap_agent() -> bool {
    !cfg!(test) && std::env::var_os("FAVETTO_NO_AGENT_WRAP").is_none()
}

/// Consume a session's normalized events: fold live state, mirror structured
/// awaiting-input, record the feed, and publish `agent.event` pushes.
fn spawn_state_task(
    session: Arc<Session>,
    tx: broadcast::Sender<AgentEvent>,
    mut events: tokio::sync::mpsc::UnboundedReceiver<AgentStateEvent>,
    bus: Option<EventBus>,
) {
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            {
                let mut live = session.live.lock();
                live.apply(&event);
            }
            match &event {
                AgentStateEvent::InputRequested { request } => {
                    *session.awaiting_input.lock() = Some(AwaitingInputReason {
                        kind: request.kind,
                        message: request.message.clone(),
                        request_id: Some(request.id.clone()),
                        options: request.options.clone(),
                        allow_always: request.allow_always,
                    });
                }
                AgentStateEvent::InputResolved { .. } | AgentStateEvent::Idle { .. } => {
                    *session.awaiting_input.lock() = None;
                }
                AgentStateEvent::Session {
                    session_id: Some(session_id),
                    ..
                } if !session_id.is_empty() => {
                    let mut slot = session.external_session_id.lock();
                    if slot.is_none() {
                        *slot = Some(session_id.clone());
                    }
                }
                _ => {}
            }
            // A completed turn is an observation point for the transcript.
            *session.last_activity.lock() = std::time::Instant::now();
            let seq = session.next_seq.fetch_add(1, Ordering::SeqCst);
            {
                let mut feed = session.feed.lock();
                feed.push_back((seq, event.clone()));
                while feed.len() > FEED_CAP {
                    feed.pop_front();
                }
            }
            if let Some(bus) = &bus {
                bus.publish(ServerPush::AgentEvent(AgentEventPush {
                    session_id: session.id.clone(),
                    task_id: session.task_id.clone(),
                    seq,
                    event: event.clone(),
                }));
            }
            let _ = session.state_tx.send(event);
            let live = session.live.lock().clone();
            let _ = tx.send(AgentEvent::State {
                session_id: session.id.clone(),
                live,
            });
        }
        // The event stream only closes when the ACP runtime ends.
        let _ = tx.send(AgentEvent::Exit {
            session_id: session.id.clone(),
            code: Some(session.exit_code.load(Ordering::SeqCst)),
        });
    });
}

#[cfg(test)]
mod model_selector_tests {
    use super::model_selector;

    #[test]
    fn composes_provider_and_model() {
        assert_eq!(
            model_selector(Some("deepseek"), Some("deepseek-flash")).as_deref(),
            Some("deepseek/deepseek-flash")
        );
        assert_eq!(
            model_selector(None, Some("deepseek-flash")).as_deref(),
            Some("deepseek-flash")
        );
        assert_eq!(
            model_selector(Some("deepseek"), None),
            None,
            "a provider without a model selects nothing"
        );
        assert_eq!(model_selector(Some(""), Some("m")).as_deref(), Some("m"));
    }
}
