//! External coding-agent sessions.
//!
//! favetto does not implement the agent loop itself: it launches a configured
//! agent CLI (opencode, Claude Code, pi, Mistral Vibe, …) inside a PTY on the
//! daemon, keeps a terminal emulator for each session, forwards keystrokes, and
//! resizes the PTY. The same agents can also be driven non-interactively to
//! execute catalog tasks.
//!
//! The daemon owns emulation and streams **self-contained full-screen frames**
//! (`vt100`'s `state_formatted`) rather than raw PTY bytes. Frames begin with
//! a clear-screen, so a dropped or duplicated frame cannot corrupt the client —
//! the next one repairs it. The daemon also answers terminal queries itself.
//!
//! Frames are broadcast on a dedicated channel rather than the event bus: they
//! are high-volume, non-durable, and only relevant to attached connections.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use tokio::sync::{broadcast, mpsc};

use favetto_core::model::{
    AgentActivity, AgentSessionInfo, AgentStateEvent, AwaitingInputReason, InputReply,
};

use crate::agent_hooks::{AgentHookLaunch, HookRouteError, HookRouter, HookRuntime};
use crate::config::{FavettoConfig, GitSettings};

use agent::{check_arg_sizes, extract_session_id_from_line};

mod agent;
mod claude;
mod configurable;
mod detect;
mod opencode;
mod pi;
mod registry;
mod state;
#[cfg(test)]
pub(crate) mod testing;
mod vibe;

pub(crate) use agent::{resolve_session_title, TITLE_POLL_ATTEMPTS, TITLE_POLL_INTERVAL};
pub use agent::{Agent, AgentContext, Invocation, SubmitStrategy};
// Re-exported for in-crate tests that build a fake `Agent`; the public trait
// signature already names these types, so this is not a new public surface.
#[cfg(test)]
pub(crate) use agent::{AgentDescriptor, CommandSpec};
pub use registry::AgentRegistry;
pub(crate) use state::{
    AgentLiveState, InputResponder, StateContext, StateSourceConfig, StateStart, StateStop,
};

/// Scrollback lines retained by each session's server-side emulator.
const SCROLLBACK: usize = 2000;

/// Maximum raw output bytes retained per session (for capturing task results).
const MAX_RAW: usize = 1024 * 1024;

/// The favetto binary itself. Agent processes are launched through
/// `favetto __agent-exec -- <cmd>`, which sets a parent-death signal so they are
/// killed when the daemon exits (including `SIGKILL`).
pub(crate) fn agent_exec_program() -> anyhow::Result<PathBuf> {
    std::env::current_exe().map_err(|e| anyhow::anyhow!("cannot locate favetto binary: {e}"))
}

/// Whether to launch agents through the `__agent-exec` wrapper. Disabled under
/// unit tests (where the current exe is the test harness) and when
/// `FAVETTO_NO_AGENT_WRAP` is set.
pub(crate) fn wrap_agent() -> bool {
    !cfg!(test) && std::env::var_os("FAVETTO_NO_AGENT_WRAP").is_none()
}

/// Ask an agent's supervisor to tear down its process group. `SIGTERM` is handled
/// by the `__agent-exec` supervisor, which then `SIGKILL`s the whole group.
fn terminate(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        // SAFETY: `kill` with a pid we own; a stale pid simply fails.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
}

/// Build the loopback hook base URL for a daemon listen address. An unspecified
/// host resolves to `127.0.0.1`; an IPv6 host is bracketed.
fn hook_base_url(addr: SocketAddr) -> String {
    let ip = if addr.ip().is_unspecified() {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    } else {
        addr.ip()
    };
    let host = match ip {
        IpAddr::V6(v6) => format!("[{v6}]"),
        IpAddr::V4(v4) => v4.to_string(),
    };
    format!("http://{host}:{}/agent-hooks/claude", addr.port())
}

/// A full-screen terminal frame, a child exit, or a folded live-state snapshot,
/// broadcast to connections that attached to the session.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// `data` is a self-contained `vt100` formatted screen (clears then redraws).
    Output { session_id: String, data: Vec<u8> },
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
            AgentEvent::Output { session_id, .. }
            | AgentEvent::Exit { session_id, .. }
            | AgentEvent::State { session_id, .. } => session_id,
        }
    }
}

/// A live agent process plus the handles needed to drive it.
struct Session {
    id: String,
    agent: String,
    task_id: Option<String>,
    /// Retained for `resize` (the reader is cloned out of it). Behind a mutex so
    /// the session is `Sync` (axum state requires it).
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    /// PID of the `__agent-exec` supervisor (its process group is the agent tree).
    pid: Option<u32>,
    /// Server-side terminal emulator: the source of truth for the screen.
    parser: Arc<Mutex<vt100::Parser>>,
    running: Arc<AtomicBool>,
    /// Raw output, retained so an unattended run's result can be captured.
    raw: Arc<Mutex<Vec<u8>>>,
    /// Child exit code (`-1` until it exits).
    exit_code: Arc<std::sync::atomic::AtomicI32>,
    /// The agent's own session id, captured from line-delimited JSON output.
    external_session_id: Arc<Mutex<Option<String>>>,
    /// Whether this is an unattended run (its output is not a TUI).
    headless: bool,
    /// When the PTY last produced output; used to require quiet before the
    /// generic awaiting-input fallback fires.
    last_activity: Arc<Mutex<std::time::Instant>>,
    /// The current awaiting-input reason, if the detector last saw one.
    awaiting_input: Arc<Mutex<Option<AwaitingInputReason>>>,
    /// Folded live state from a structured channel, if this session has one.
    live: Arc<Mutex<AgentLiveState>>,
    /// Answers structured prompts through the session's state transport.
    responder: Option<Arc<dyn InputResponder>>,
    /// Per-session broadcast of raw normalized events, for `attention`.
    state_tx: Option<broadcast::Sender<AgentStateEvent>>,
    /// Stops the session's detached state transport; taken on exit or close.
    state_stop: Arc<Mutex<Option<StateStop>>>,
    /// Creation order, so "the task's latest session" is well-defined.
    order: u64,
}

impl Session {
    fn info(&self) -> AgentSessionInfo {
        let live = self.live.lock();
        AgentSessionInfo {
            id: self.id.clone(),
            agent: self.agent.clone(),
            task_id: self.task_id.clone(),
            running: self.running.load(Ordering::SeqCst),
            headless: self.headless,
            session_id: self.external_session_id.lock().clone(),
            awaiting_input: self.awaiting_input.lock().clone(),
            activity: live.activity.clone(),
            usage: live.usage.clone(),
        }
    }

    /// Stop the session's detached state transport, if it is still running.
    fn stop_state(&self) {
        let stop = self.state_stop.lock().take();
        if let Some(stop) = stop {
            stop();
        }
    }
}

/// Resolved `[git]` settings for every launch, keyed by agent name.
///
/// Built once at daemon startup by [`AgentManager::configure_git`]; the default
/// (signing off) keeps unit tests that never configure git from writing files.
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

/// Owns every live external-agent session on the daemon.
pub struct AgentManager {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    tx: broadcast::Sender<AgentEvent>,
    next_order: AtomicU64,
    git: GitRuntime,
    /// The daemon's per-launch HTTP hook receiver, once configured.
    hooks: Option<HookRuntime>,
}

impl Default for AgentManager {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for AgentManager {
    fn drop(&mut self) {
        // Don't leave agent children running after the daemon exits.
        let mut sessions = self.sessions.lock();
        for (_, session) in sessions.drain() {
            session.running.store(false, Ordering::SeqCst);
            session.stop_state();
            terminate(session.pid);
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
            hooks: None,
        }
    }

    /// Resolve the daemon's per-launch hook receiver from the HTTP listen
    /// address.
    ///
    /// Claude sessions get an ephemeral `--settings` file pointing at
    /// `http://<loopback>:<port>/agent-hooks/claude/<token>`; the router behind
    /// it routes each payload to the session that registered the token. An
    /// unspecified host (`0.0.0.0`, `::`) resolves to loopback. A malformed
    /// `listen` leaves hooks disabled (sessions keep the screen heuristic).
    pub fn configure_hooks(&mut self, listen: &str, data_dir: PathBuf) {
        let Ok(addr) = listen.parse::<SocketAddr>() else {
            tracing::warn!(
                listen,
                "cannot parse daemon listen address; agent hooks disabled"
            );
            self.hooks = None;
            return;
        };
        self.hooks = Some(HookRuntime {
            router: Arc::new(HookRouter::new()),
            launch: AgentHookLaunch {
                endpoint: hook_base_url(addr),
                dir: data_dir.join("agent-hooks"),
            },
        });
    }

    /// The hook router, when hooks are configured (the axum handler's target).
    pub fn hook_router(&self) -> Option<Arc<HookRouter>> {
        self.hooks.as_ref().map(|hooks| hooks.router.clone())
    }

    /// Route one raw hook delivery to its session's state channel.
    pub fn route_hook(&self, token: &str, body: &[u8]) -> Result<usize, HookRouteError> {
        match self.hook_router() {
            Some(router) => router.route(token, body),
            None => Err(HookRouteError::Disabled),
        }
    }

    /// Resolve the global and per-agent `[git]` settings, materializing any
    /// generated scripts once so a bad `[git]` section (e.g. `signing = "ssh"`
    /// with no key, or an unset `passphrase_env`) fails daemon startup rather
    /// than the first agent launch.
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

    /// Subscribe to every session's output; callers filter by `session_id`.
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
    /// Opening a task reattaches to this so a finished agent isn't re-launched.
    pub fn find_latest_by_task(&self, task_id: &str) -> Option<AgentSessionInfo> {
        self.sessions
            .lock()
            .values()
            .filter(|s| s.task_id.as_deref() == Some(task_id))
            .max_by_key(|s| s.order)
            .map(|s| s.info())
    }

    /// Spawn `agent` in a PTY for `invocation`.
    ///
    /// The agent builds the [`CommandSpec`]; the manager only owns PTY/emulator
    /// mechanics, stdin delivery, and session capture.
    pub fn start(
        &self,
        name: &str,
        agent: Arc<dyn Agent>,
        task_id: Option<String>,
        invocation: Invocation<'_>,
        mut ctx: AgentContext,
    ) -> anyhow::Result<AgentSessionInfo> {
        // Materialize the per-launch values from the invocation so the agent
        // implementation can read them from the owned context.
        match &invocation {
            Invocation::Interactive {
                prompt,
                provider,
                model,
            } => {
                ctx.prompt = prompt.map(str::to_string);
                ctx.provider = provider.map(str::to_string);
                ctx.model = model.map(str::to_string);
            }
            Invocation::Headless {
                prompt,
                provider,
                model,
            } => {
                ctx.prompt = Some((*prompt).to_string());
                ctx.provider = provider.map(str::to_string);
                ctx.model = model.map(str::to_string);
            }
            Invocation::Resume(session_id) => {
                ctx.session_id = Some((*session_id).to_string());
            }
        }

        let mut spec = agent.command(&invocation, &ctx)?;
        // Register per-launch hooks before the args are frozen: claude gets an
        // ephemeral `--settings` file plus endpoint env vars, and the state
        // source below recovers the token from `spec.env`.
        if let Some(hooks) = &self.hooks {
            if let Some(injection) = agent.hook_injection(&hooks.launch) {
                spec.args.splice(0..0, injection.args);
                for (key, value) in injection.env {
                    spec.env.insert(key, value);
                }
            }
        }
        check_arg_sizes(&spec.program, &spec.args)?;
        let headless = matches!(&invocation, Invocation::Headless { .. });

        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows: ctx.rows.max(1),
            cols: ctx.cols.max(1),
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut cmd = if wrap_agent() {
            let mut c = CommandBuilder::new(agent_exec_program()?);
            c.arg("__agent-exec");
            c.arg("--");
            c.arg(&spec.program);
            c
        } else {
            CommandBuilder::new(&spec.program)
        };
        for arg in &spec.args {
            cmd.arg(arg);
        }

        if let Some(cwd) = &spec.cwd {
            cmd.cwd(cwd);
            // The child's real working directory is changed above, but tools that
            // resolve relative paths from `$PWD` (e.g. opencode) would otherwise
            // still see the daemon's directory. Keep the environment in sync.
            if let Some(cwd) = cwd.to_str() {
                cmd.env("PWD", cwd);
            }
        }
        cmd.env("TERM", "xterm-256color");
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }

        // `[git]` provisioning is applied last so it is authoritative: a raw
        // `[agents.x.env]` `commit.gpgsign` cannot silently re-enable an
        // interactive signer. A per-task `sign` wins over both.
        let mut git_settings = self
            .git
            .per_agent
            .get(name)
            .cloned()
            .unwrap_or_else(|| self.git.default.clone());
        if let Some(mode) = ctx.git_signing {
            git_settings.signing = Some(mode);
        }
        for (k, v) in crate::git::resolve_env(&git_settings, &self.git.data_dir)? {
            cmd.env(k, v);
        }

        let mut child = pair.slave.spawn_command(cmd)?;
        // Drop the slave so the PTY reports EOF when the child exits.
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader()?;
        let writer = Arc::new(Mutex::new(pair.master.take_writer()?));
        let pid = child.process_id();

        // If there is a prompt but no prompt_args, feed it over stdin. For an
        // unattended run also send EOT so an agent reading stdin sees EOF (a PTY
        // cannot half-close).
        if let Some(prompt) = &spec.stdin_prompt {
            let mut w = writer.lock();
            let _ = w.write_all(prompt.as_bytes());
            let _ = w.write_all(b"\r");
            if spec.stdin_eof {
                let _ = w.write_all(&[0x04]);
            }
            let _ = w.flush();
        }

        let id = uuid::Uuid::new_v4().to_string();
        let order = self.next_order.fetch_add(1, Ordering::SeqCst);
        let parser = Arc::new(Mutex::new(vt100::Parser::new(
            ctx.rows.max(1),
            ctx.cols.max(1),
            SCROLLBACK,
        )));
        // Counts output chunks; used only to detect activity/quiet for `submit_prompt`.
        let ticks = Arc::new(AtomicU64::new(0));
        let running = Arc::new(AtomicBool::new(true));
        let raw = Arc::new(Mutex::new(Vec::new()));
        let exit_code = Arc::new(std::sync::atomic::AtomicI32::new(-1));
        let probe = agent.session_id_probe();
        // A headless agent that does not report its own session id gets the
        // deterministic one the caller generated (bound via `{session_id}`), so
        // the task row and reattach path see it even if the process dies before
        // emitting anything. Agents with a probe (opencode) capture their real id.
        let seeded_session_id = if headless && probe.is_none() {
            ctx.session_id.clone()
        } else {
            None
        };
        let external_session_id = Arc::new(Mutex::new(seeded_session_id));
        let last_activity = Arc::new(Mutex::new(std::time::Instant::now()));
        let awaiting_input = Arc::new(Mutex::new(None::<AwaitingInputReason>));
        let tx = self.tx.clone();

        // Start the agent's structured state source, if it has one. The stdout
        // parser is fed by the reader thread below; the event receiver is
        // consumed by a per-session task that folds it into the live snapshot,
        // mirrors awaiting-input onto the session, and rebroadcasts raw events
        // for `attention`. A source that fails to start degrades to the screen
        // heuristic without affecting the session.
        let state_cfg = StateSourceConfig {
            headless,
            program: spec.program.clone(),
            args: spec.args.clone(),
            env: spec.env.clone(),
            cwd: spec.cwd.clone(),
            hook_router: self.hooks.as_ref().map(|hooks| hooks.router.clone()),
        };
        let state_ctx = StateContext {
            favetto_session: id.clone(),
            external_session: ctx.session_id.clone(),
            prompt: ctx.prompt.clone(),
            headless,
            cwd: spec.cwd.clone(),
            program: spec.program.clone(),
            args: spec.args.clone(),
            env: spec.env.clone(),
            stdin: writer.clone(),
        };
        let started = if tokio::runtime::Handle::try_current().is_ok() {
            match agent.state_source(&state_cfg) {
                Some(source) => match source.start(state_ctx) {
                    Ok(start) => Some(start),
                    Err(e) => {
                        tracing::warn!(
                            agent = name,
                            session = %id,
                            error = %e,
                            "agent state source failed to start; falling back to screens"
                        );
                        None
                    }
                },
                None => None,
            }
        } else {
            // The state task needs the async runtime; the daemon always has one,
            // but a bare-sync caller must not half-wire a source.
            None
        };
        let (state_events, state_stdout, state_responder, state_stop, state_tx) = match started {
            Some(StateStart {
                events,
                stdout,
                responder,
                stop,
            }) => {
                let (state_tx, _) = broadcast::channel(256);
                (Some(events), stdout, Some(responder), stop, Some(state_tx))
            }
            None => (None, None, None, None, None),
        };
        let state_stop = Arc::new(Mutex::new(state_stop));
        let live = Arc::new(Mutex::new(AgentLiveState {
            activity: state_events.as_ref().map(|_| AgentActivity::Starting),
            usage: None,
        }));

        // Reader thread: feed the emulator, answer terminal queries, broadcast a
        // full-screen frame, then reap the child and announce its exit.
        {
            let id = id.clone();
            let parser = parser.clone();
            let ticks = ticks.clone();
            let running = running.clone();
            let writer = writer.clone();
            let raw_out = raw.clone();
            let exit_code = exit_code.clone();
            let external_session_id = external_session_id.clone();
            let last_activity = last_activity.clone();
            let mut state_stdout = state_stdout;
            let state_stop = state_stop.clone();
            let reader_tx = tx.clone();
            std::thread::spawn(move || {
                let mut chunk = [0u8; 8192];
                let mut line_buf: Vec<u8> = Vec::new();
                loop {
                    match reader.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => {
                            let bytes = &chunk[..n];
                            *last_activity.lock() = std::time::Instant::now();
                            {
                                let mut buf = raw_out.lock();
                                buf.extend_from_slice(bytes);
                                if buf.len() > MAX_RAW {
                                    let excess = buf.len() - MAX_RAW;
                                    buf.drain(..excess);
                                }
                            }
                            // Feed the structured stdout parser, if this session
                            // has one, from the same bytes as the emulator.
                            if let Some(parser) = state_stdout.as_mut() {
                                parser.push(bytes);
                            }
                            // Capture the agent's own session id from line-delimited
                            // JSON output (e.g. `opencode run --format json`).
                            if let Some(probe) = &probe {
                                line_buf.extend_from_slice(bytes);
                                while let Some(pos) = line_buf.iter().position(|&b| b == b'\n') {
                                    let line: Vec<u8> = line_buf.drain(..=pos).collect();
                                    let line = String::from_utf8_lossy(&line);
                                    if let Some(found) = extract_session_id_from_line(&line, probe)
                                    {
                                        let mut slot = external_session_id.lock();
                                        if slot.is_none() {
                                            *slot = Some(found);
                                        }
                                        break;
                                    }
                                }
                                if line_buf.len() > 64 * 1024 {
                                    line_buf.clear();
                                }
                            }
                            let (frame, replies) = {
                                let mut p = parser.lock();
                                p.process(bytes);
                                (
                                    p.screen().state_formatted(),
                                    terminal_replies(p.screen(), bytes),
                                )
                            };
                            ticks.fetch_add(1, Ordering::SeqCst);
                            if !replies.is_empty() {
                                let mut w = writer.lock();
                                let _ = w.write_all(&replies);
                                let _ = w.flush();
                            }
                            let _ = reader_tx.send(AgentEvent::Output {
                                session_id: id.clone(),
                                data: frame,
                            });
                        }
                        Err(_) => break,
                    }
                }
                running.store(false, Ordering::SeqCst);
                let code = child
                    .wait()
                    .ok()
                    .map(|status| status.exit_code() as i32)
                    .unwrap_or(-1);
                exit_code.store(code, Ordering::SeqCst);
                if let Some(parser) = state_stdout.as_mut() {
                    parser.finish(Some(code));
                }
                let stop = state_stop.lock().take();
                if let Some(stop) = stop {
                    stop();
                }
                let _ = reader_tx.send(AgentEvent::Exit {
                    session_id: id,
                    code: Some(code),
                });
            });
        }

        // If the prompt was only pre-filled via `prompt_args`, submit it once the
        // agent's UI has settled (interactive sessions only).
        if matches!(&invocation, Invocation::Interactive { .. }) {
            if let SubmitStrategy::AfterSettle { delay, max_sends } = spec.submit {
                spawn_submit(
                    writer.clone(),
                    ticks.clone(),
                    running.clone(),
                    delay,
                    max_sends,
                );
            }
        }

        let session = Arc::new(Session {
            id: id.clone(),
            agent: name.to_string(),
            task_id,
            master: Mutex::new(pair.master),
            writer,
            pid,
            parser,
            running,
            raw,
            exit_code,
            external_session_id,
            headless,
            last_activity,
            awaiting_input,
            live,
            responder: state_responder,
            state_tx,
            state_stop,
            order,
        });
        if let Some(events) = state_events {
            spawn_state_task(session.clone(), tx, events);
        }
        let info = session.info();
        self.sessions.lock().insert(id, session);
        Ok(info)
    }

    /// The current full-screen frame for a (re)attaching client.
    pub fn attach(&self, session_id: &str) -> anyhow::Result<(AgentSessionInfo, Vec<u8>)> {
        let session = self.get(session_id)?;
        let frame = session.parser.lock().screen().state_formatted();
        Ok((session.info(), frame))
    }

    /// Wait for a session's process to exit; returns its exit code, or `None` if
    /// the session is unknown.
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

    /// Detect whether a running session is blocked on the user. Agent-specific
    /// detection fires immediately; the generic fallback only fires once the PTY
    /// has been quiet for `quiet`.
    pub fn detect_awaiting_input(
        &self,
        session_id: &str,
        agent: &dyn Agent,
        quiet: std::time::Duration,
    ) -> Option<AwaitingInputReason> {
        let session = self.get(session_id).ok()?;
        if !session.running.load(Ordering::SeqCst) {
            return None;
        }
        let quiet_elapsed = session.last_activity.lock().elapsed();
        let (specific, generic) = {
            let parser = session.parser.lock();
            let screen = parser.screen();
            let specific = agent.awaiting_input(screen);
            let generic = if quiet_elapsed >= quiet {
                detect::generic_awaiting_input(&screen.contents())
            } else {
                None
            };
            (specific, generic)
        };
        specific.or(generic)
    }

    /// Record (or clear) a session's awaiting-input reason.
    pub fn set_awaiting_input(&self, session_id: &str, reason: Option<AwaitingInputReason>) {
        if let Ok(session) = self.get(session_id) {
            *session.awaiting_input.lock() = reason;
        }
    }

    /// Whether the session's child process is still running.
    pub fn is_running(&self, session_id: &str) -> bool {
        self.get(session_id)
            .map(|session| session.running.load(Ordering::SeqCst))
            .unwrap_or(false)
    }

    /// The session's exit code, or `None` when the session is unknown.
    pub fn exit_code(&self, session_id: &str) -> Option<i32> {
        self.get(session_id)
            .ok()
            .map(|session| session.exit_code.load(Ordering::SeqCst))
    }

    /// Raw output captured for a session (used to record an unattended task's output).
    pub fn output(&self, session_id: &str) -> String {
        self.get(session_id)
            .map(|s| String::from_utf8_lossy(&s.raw.lock()).to_string())
            .unwrap_or_default()
    }

    /// Plain-text contents of a session's terminal screen (no ANSI, no terminal
    /// queries). Used as the captured output of an interactive task run.
    pub fn screen_text(&self, session_id: &str) -> String {
        self.get(session_id)
            .map(|s| s.parser.lock().screen().contents())
            .unwrap_or_default()
    }

    /// The agent's own session id captured from a run, if any.
    pub fn external_session_id(&self, session_id: &str) -> Option<String> {
        self.get(session_id)
            .ok()
            .and_then(|s| s.external_session_id.lock().clone())
    }

    /// Subscribe to a session's raw normalized state events, if it has a
    /// structured source. `None` means the session relies on the screen
    /// heuristic.
    pub fn subscribe_state(
        &self,
        session_id: &str,
    ) -> Option<broadcast::Receiver<AgentStateEvent>> {
        let session = self.get(session_id).ok()?;
        session.state_tx.as_ref().map(|tx| tx.subscribe())
    }

    /// Write raw bytes to the session's PTY (keystrokes from the TUI).
    pub fn input(&self, session_id: &str, data: &[u8]) -> anyhow::Result<()> {
        let session = self.get(session_id)?;
        let mut writer = session.writer.lock();
        writer.write_all(data)?;
        writer.flush()?;
        Ok(())
    }

    /// Answer a structured input request through the session's state transport.
    ///
    /// This is the typed counterpart to [`Self::input`]: replies are correlated
    /// by `request_id` and validated, rather than typed as raw PTY bytes. Errors
    /// when the session has no structured input channel.
    pub async fn reply(
        &self,
        session_id: &str,
        request_id: &str,
        reply: InputReply,
    ) -> anyhow::Result<()> {
        let session = self.get(session_id)?;
        let responder = session.responder.clone().ok_or_else(|| {
            anyhow::anyhow!("agent session '{session_id}' has no structured input channel")
        })?;
        responder.reply(request_id, reply).await
    }

    /// Resize the session's PTY and server-side emulator.
    pub fn resize(&self, session_id: &str, rows: u16, cols: u16) -> anyhow::Result<()> {
        let session = self.get(session_id)?;
        let (rows, cols) = (rows.max(1), cols.max(1));
        session.master.lock().resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        session.parser.lock().screen_mut().set_size(rows, cols);
        Ok(())
    }

    /// Terminate a session and drop it from the registry.
    pub fn close(&self, session_id: &str) -> anyhow::Result<()> {
        if let Some(session) = self.sessions.lock().remove(session_id) {
            session.running.store(false, Ordering::SeqCst);
            session.stop_state();
            terminate(session.pid);
        }
        Ok(())
    }

    fn get(&self, session_id: &str) -> anyhow::Result<Arc<Session>> {
        self.sessions
            .lock()
            .get(session_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no agent session '{session_id}'"))
    }
}

/// Reply to terminal queries found in `bytes`. A real terminal answers these and
/// apps (opencode, vim, …) wait for them; the daemon answers on the PTY so the
/// client never has to round-trip.
fn terminal_replies(screen: &vt100::Screen, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] != 0x1b {
            i += 1;
            continue;
        }
        match bytes[i + 1] {
            b'[' => {
                if let Some(len) = csi_reply(screen, &bytes[i..], &mut out) {
                    i += len;
                    continue;
                }
                if let Some(len) = decrqm_reply(&bytes[i..], &mut out) {
                    i += len;
                    continue;
                }
            }
            b']' => {
                if let Some(len) = osc_reply(&bytes[i..], &mut out) {
                    i += len;
                    continue;
                }
            }
            _ => {}
        }
        i += 1;
    }
    out
}

fn csi_reply(screen: &vt100::Screen, bytes: &[u8], out: &mut Vec<u8>) -> Option<usize> {
    if bytes.starts_with(b"\x1b[6n") {
        let (row, col) = screen.cursor_position();
        out.extend_from_slice(format!("\x1b[{};{}R", row + 1, col + 1).as_bytes());
        return Some(4);
    }
    if bytes.starts_with(b"\x1b[5n") {
        out.extend_from_slice(b"\x1b[0n");
        return Some(4);
    }
    if bytes.starts_with(b"\x1b[?u") {
        // Kitty keyboard protocol query — report no support so apps use legacy keys.
        out.extend_from_slice(b"\x1b[?0u");
        return Some(4);
    }
    if bytes.starts_with(b"\x1b[>c") {
        out.extend_from_slice(b"\x1b[>0;276;0c");
        return Some(4);
    }
    if bytes.starts_with(b"\x1b[c") {
        out.extend_from_slice(b"\x1b[?62;1;2;6;9;15;22c");
        return Some(3);
    }
    None
}

/// Answer DECRQM mode queries (`CSI [?]Ps$p`) so apps that probe capabilities
/// (synchronized output, bracketed paste, mouse, …) don't wait for a reply.
fn decrqm_reply(bytes: &[u8], out: &mut Vec<u8>) -> Option<usize> {
    let (prefix_len, marker) = if bytes.starts_with(b"\x1b[?") {
        (3, "?")
    } else if bytes.starts_with(b"\x1b[") {
        (2, "")
    } else {
        return None;
    };
    let rest = &bytes[prefix_len..];
    let mut i = 0;
    while i < rest.len() && rest[i].is_ascii_digit() {
        i += 1;
    }
    if i == 0 || rest.get(i) != Some(&b'$') || rest.get(i + 1) != Some(&b'p') {
        return None;
    }
    let n = std::str::from_utf8(&rest[..i]).ok()?;
    // `2` = recognised and reset.
    out.extend_from_slice(format!("\x1b[{marker}{n};2$y").as_bytes());
    Some(prefix_len + i + 2)
}

fn osc_reply(bytes: &[u8], out: &mut Vec<u8>) -> Option<usize> {
    for (prefix, color) in [
        (
            b"\x1b]10;?".as_slice(),
            b"\x1b]10;rgb:ffff/ffff/ffff\x07".as_slice(),
        ),
        (
            b"\x1b]11;?".as_slice(),
            b"\x1b]11;rgb:0000/0000/0000\x07".as_slice(),
        ),
    ] {
        if bytes.starts_with(prefix) {
            let rest = &bytes[prefix.len()..];
            let term_len = if rest.starts_with(b"\x1b\\") {
                2
            } else if rest.starts_with(b"\x07") {
                1
            } else {
                return None;
            };
            out.extend_from_slice(color);
            return Some(prefix.len() + term_len);
        }
    }
    None
}

/// Press Enter to submit a pre-filled prompt once the agent's UI is ready.
///
/// Agents differ wildly in when their input box becomes ready (opencode starts a
/// background server first, with quiet gaps that make "wait for quiet" wrong).
/// Pressing Enter on an empty composer is a no-op, so we send it a few times,
/// stopping early once the agent starts reacting (output ramps up).
fn spawn_submit(
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    ticks: Arc<AtomicU64>,
    running: Arc<AtomicBool>,
    delay: std::time::Duration,
    max_sends: u32,
) {
    use std::time::{Duration, Instant};

    const FIRST_OUTPUT_TIMEOUT: Duration = Duration::from_secs(15);
    const RETRY_INTERVAL: Duration = Duration::from_millis(1200);
    const MIN_SENDS: u32 = 2;
    /// Output chunks after a send that indicate the agent reacted (started a turn).
    const REACTED: u64 = 4;

    std::thread::spawn(move || {
        // Wait for the app to start producing output.
        let wait_start = Instant::now();
        while ticks.load(Ordering::SeqCst) == 0
            && running.load(Ordering::SeqCst)
            && wait_start.elapsed() < FIRST_OUTPUT_TIMEOUT
        {
            std::thread::sleep(Duration::from_millis(75));
        }
        if !running.load(Ordering::SeqCst) {
            return;
        }

        std::thread::sleep(delay);
        let mut sent = 0u32;
        let mut before = ticks.load(Ordering::SeqCst);
        while running.load(Ordering::SeqCst) && sent < max_sends {
            let mut w = writer.lock();
            let _ = w.write_all(b"\r");
            let _ = w.flush();
            sent += 1;
            std::thread::sleep(RETRY_INTERVAL);

            let now = ticks.load(Ordering::SeqCst);
            if sent >= MIN_SENDS && now.saturating_sub(before) >= REACTED {
                break; // the agent reacted; don't send more Enters
            }
            before = now;
        }
    });
}

/// Consume a session's normalized state events.
///
/// Folds each event into the session's live snapshot, mirrors structured
/// awaiting-input onto the session, and rebroadcasts the raw event so
/// `attention` can react to `InputRequested`/`InputResolved` without the screen
/// heuristic. Emits a folded `AgentEvent::State` for attached clients.
fn spawn_state_task(
    session: Arc<Session>,
    tx: broadcast::Sender<AgentEvent>,
    mut events: mpsc::UnboundedReceiver<AgentStateEvent>,
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
                    // Hook/stream identity can arrive after a seeded id; only
                    // fill an empty slot so a probe/source never masks the real id.
                    let mut slot = session.external_session_id.lock();
                    if slot.is_none() {
                        *slot = Some(session_id.clone());
                    }
                }
                _ => {}
            }
            if let Some(state_tx) = &session.state_tx {
                let _ = state_tx.send(event);
            }
            let live = session.live.lock().clone();
            let _ = tx.send(AgentEvent::State {
                session_id: session.id.clone(),
                live,
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::configurable::ConfigurableAgent;
    use super::*;
    use crate::config::{AgentConfig, FavettoConfig, GitSigning};

    fn cfg(command: &str, args: &[&str]) -> AgentConfig {
        AgentConfig {
            command: command.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn template(name: &str, config: AgentConfig) -> Arc<dyn Agent> {
        Arc::new(ConfigurableAgent::from_config(name, &config))
    }

    fn context(cwd: Option<PathBuf>, rows: u16, cols: u16) -> AgentContext {
        AgentContext {
            cwd,
            rows,
            cols,
            ..Default::default()
        }
    }

    #[test]
    fn interactive_with_model_uses_interactive_model_args() {
        let mgr = AgentManager::new();
        let mut config = cfg("sh", &[]);
        config.interactive_model_args = Some(vec![
            "-c".to_string(),
            r#"printf 'model:%s/%s' "$1" "$2""#.to_string(),
            "ignored".to_string(),
            "{provider}".to_string(),
            "{model}".to_string(),
        ]);
        let info = mgr
            .start(
                "sh",
                template("sh", config),
                None,
                Invocation::Interactive {
                    prompt: None,
                    provider: Some("jev"),
                    model: Some("1.13"),
                },
                context(None, 24, 80),
            )
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        let (_info, data) = mgr.attach(&info.id).unwrap();
        assert!(
            String::from_utf8_lossy(&data).contains("model:jev/1.13"),
            "frame: {:?}",
            String::from_utf8_lossy(&data)
        );
        mgr.close(&info.id).unwrap();
    }

    #[test]
    fn attach_returns_current_frame() {
        let mgr = AgentManager::new();
        let agent = template("sh", cfg("sh", &["-c", "printf hello-pty; sleep 1"]));
        let info = mgr
            .start(
                "sh",
                agent,
                None,
                Invocation::Interactive {
                    prompt: None,
                    provider: None,
                    model: None,
                },
                context(None, 24, 80),
            )
            .unwrap();

        std::thread::sleep(std::time::Duration::from_millis(400));
        let (attached, data) = mgr.attach(&info.id).unwrap();
        assert_eq!(attached.id, info.id);
        assert!(
            String::from_utf8_lossy(&data).contains("hello-pty"),
            "frame: {:?}",
            String::from_utf8_lossy(&data)
        );
        mgr.close(&info.id).unwrap();
    }

    #[test]
    fn pty_session_accepts_input() {
        let mgr = AgentManager::new();
        let info = mgr
            .start(
                "cat",
                template("cat", cfg("cat", &[])),
                None,
                Invocation::Interactive {
                    prompt: None,
                    provider: None,
                    model: None,
                },
                context(None, 24, 80),
            )
            .unwrap();

        std::thread::sleep(std::time::Duration::from_millis(200));
        mgr.input(&info.id, b"ping-pty\r").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));

        let (_info, data) = mgr.attach(&info.id).unwrap();
        assert!(
            String::from_utf8_lossy(&data).contains("ping-pty"),
            "frame: {:?}",
            String::from_utf8_lossy(&data)
        );
        mgr.close(&info.id).unwrap();
    }

    #[test]
    fn screen_text_returns_plain_emulator_contents() {
        let mgr = AgentManager::new();
        let agent = template("sh", cfg("sh", &["-c", "printf 'hello-screen'; sleep 1"]));
        let info = mgr
            .start(
                "sh",
                agent,
                Some("task-screen".to_string()),
                Invocation::Interactive {
                    prompt: None,
                    provider: None,
                    model: None,
                },
                context(None, 24, 80),
            )
            .unwrap();

        std::thread::sleep(std::time::Duration::from_millis(400));
        assert!(
            mgr.screen_text(&info.id).contains("hello-screen"),
            "screen text: {:?}",
            mgr.screen_text(&info.id)
        );
        // An unknown session has no screen.
        assert_eq!(mgr.screen_text("missing"), "");
        mgr.close(&info.id).unwrap();
    }

    #[test]
    fn pty_resize_does_not_error() {
        let mgr = AgentManager::new();
        let info = mgr
            .start(
                "cat",
                template("cat", cfg("cat", &[])),
                None,
                Invocation::Interactive {
                    prompt: None,
                    provider: None,
                    model: None,
                },
                context(None, 24, 80),
            )
            .unwrap();
        mgr.resize(&info.id, 40, 120).unwrap();
        mgr.close(&info.id).unwrap();
    }

    #[test]
    fn submit_prompt_presses_enter_after_quiet() {
        let mgr = AgentManager::new();
        let mut config = cfg("sh", &[]);
        config.prompt_args = Some(vec![
            "-c".to_string(),
            "printf ready; read x; echo got:$x".to_string(),
            "{prompt}".to_string(),
        ]);
        config.submit_prompt = Some(true);
        // Prompt goes via prompt_args, so it is not written to stdin; the submit
        // thread must press Enter for `read` to unblock.
        let info = mgr
            .start(
                "sh",
                template("sh", config),
                None,
                Invocation::Interactive {
                    prompt: Some("ignored"),
                    provider: None,
                    model: None,
                },
                context(None, 24, 80),
            )
            .unwrap();

        std::thread::sleep(std::time::Duration::from_millis(2500));
        let (_info, data) = mgr.attach(&info.id).unwrap();
        let text = String::from_utf8_lossy(&data);
        assert!(text.contains("got:"), "frame: {text:?}");
        mgr.close(&info.id).unwrap();
    }

    #[tokio::test]
    async fn headless_session_runs_and_captures_output() {
        let mgr = AgentManager::new();
        let mut config = cfg("sh", &[]);
        config.headless_args = Some(vec!["-c".to_string(), "cat".to_string()]);
        let info = mgr
            .start(
                "sh",
                template("sh", config),
                Some("task-1".to_string()),
                Invocation::Headless {
                    prompt: "hello-headless",
                    provider: None,
                    model: None,
                },
                context(None, 40, 120),
            )
            .unwrap();
        let code = mgr.wait(&info.id).await;
        assert_eq!(code, Some(0));
        assert!(
            mgr.output(&info.id).contains("hello-headless"),
            "output: {:?}",
            mgr.output(&info.id)
        );
        // Session is retained and bound to the task for reattach.
        assert_eq!(
            mgr.find_latest_by_task("task-1").map(|s| s.id),
            Some(info.id)
        );
    }

    #[tokio::test]
    async fn headless_run_uses_run_args_and_captures_session_id() {
        let mgr = AgentManager::new();
        let mut config = cfg("sh", &[]);
        config.run_args = Some(vec![
            "-c".to_string(),
            r#"printf '{"sessionID":"ses_123"}\n'"#.to_string(),
        ]);
        config.session_id_json_key = Some("sessionID".to_string());
        let info = mgr
            .start(
                "sh",
                template("sh", config),
                Some("task-2".to_string()),
                Invocation::Headless {
                    prompt: "hi",
                    provider: Some("jev"),
                    model: Some("1.13"),
                },
                context(None, 40, 120),
            )
            .unwrap();
        assert_eq!(mgr.wait(&info.id).await, Some(0));
        assert_eq!(
            mgr.external_session_id(&info.id).as_deref(),
            Some("ses_123")
        );
    }

    #[tokio::test]
    async fn headless_run_seeds_a_deterministic_session_id() {
        let mgr = AgentManager::new();
        let mut config = cfg("sh", &[]);
        config.headless_args = Some(vec!["-c".to_string(), "printf ok".to_string()]);
        config.resume_args = Some(vec!["--session".to_string(), "{session_id}".to_string()]);
        let mut ctx = context(None, 40, 120);
        ctx.session_id = Some("ses-seeded".to_string());
        let info = mgr
            .start(
                "sh",
                template("sh", config),
                Some("task-seed".to_string()),
                Invocation::Headless {
                    prompt: "hi",
                    provider: None,
                    model: None,
                },
                ctx,
            )
            .unwrap();
        // Visible immediately, before the process exits.
        assert_eq!(
            mgr.external_session_id(&info.id).as_deref(),
            Some("ses-seeded")
        );
        assert_eq!(mgr.wait(&info.id).await, Some(0));
        assert_eq!(
            mgr.external_session_id(&info.id).as_deref(),
            Some("ses-seeded")
        );
        mgr.close(&info.id).unwrap();
    }

    #[tokio::test]
    async fn probe_agent_captures_its_own_id_over_the_seeded_one() {
        let mgr = AgentManager::new();
        let mut config = cfg("sh", &[]);
        config.headless_args = Some(vec![
            "-c".to_string(),
            r#"printf '{"sessionID":"ses_real"}\n'"#.to_string(),
        ]);
        config.session_id_json_key = Some("sessionID".to_string());
        let mut ctx = context(None, 40, 120);
        ctx.session_id = Some("ses-seeded".to_string());
        let info = mgr
            .start(
                "sh",
                template("sh", config),
                None,
                Invocation::Headless {
                    prompt: "hi",
                    provider: None,
                    model: None,
                },
                ctx,
            )
            .unwrap();
        assert_eq!(mgr.wait(&info.id).await, Some(0));
        // The probe's real id wins; the generated one must not mask it.
        assert_eq!(
            mgr.external_session_id(&info.id).as_deref(),
            Some("ses_real")
        );
    }

    #[tokio::test]
    async fn headless_child_gets_pwd_matching_cwd() {
        let dir = std::env::temp_dir().join(format!("favetto-pwd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mgr = AgentManager::new();
        let mut config = cfg("sh", &[]);
        config.headless_args = Some(vec![
            "-c".to_string(),
            r#"printf 'PWD=%s' "$PWD""#.to_string(),
        ]);
        let info = mgr
            .start(
                "sh",
                template("sh", config),
                None,
                Invocation::Headless {
                    prompt: "ignored",
                    provider: None,
                    model: None,
                },
                context(Some(dir.clone()), 40, 120),
            )
            .unwrap();
        assert_eq!(mgr.wait(&info.id).await, Some(0));
        let output = mgr.output(&info.id);
        assert!(
            output.contains(&format!("PWD={}", dir.display())),
            "output: {output:?}"
        );
        let _ = std::fs::remove_dir(&dir);
    }

    #[tokio::test]
    async fn headless_child_gets_git_signing_env() {
        let dir = std::env::temp_dir().join(format!("favetto-git-env-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let mut mgr = AgentManager::new();
        let mut fc = FavettoConfig::default();
        fc.git.signing = Some(GitSigning::Off);
        mgr.configure_git(&fc, dir.clone()).unwrap();

        let mut config = cfg("sh", &[]);
        config.headless_args = Some(vec![
            "-c".to_string(),
            r#"printf 'COUNT=%s K=%s V=%s' "$GIT_CONFIG_COUNT" "$GIT_CONFIG_KEY_0" "$GIT_CONFIG_VALUE_0""#
                .to_string(),
        ]);
        let info = mgr
            .start(
                "sh",
                template("sh", config),
                None,
                Invocation::Headless {
                    prompt: "ignored",
                    provider: None,
                    model: None,
                },
                context(None, 40, 120),
            )
            .unwrap();
        assert_eq!(mgr.wait(&info.id).await, Some(0));
        let output = mgr.output(&info.id);
        assert!(output.contains("commit.gpgsign"), "output: {output:?}");
        assert!(output.contains("false"), "output: {output:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn task_signing_override_wins() {
        let dir = std::env::temp_dir().join(format!("favetto-git-override-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let mut mgr = AgentManager::new();
        let mut fc = FavettoConfig::default();
        fc.git.signing = Some(GitSigning::Ssh);
        fc.git.signing_key = Some("k".to_string());
        mgr.configure_git(&fc, dir.clone()).unwrap();

        let mut config = cfg("sh", &[]);
        config.headless_args = Some(vec![
            "-c".to_string(),
            r#"printf 'V=%s' "$GIT_CONFIG_VALUE_0""#.to_string(),
        ]);
        let mut ctx = context(None, 40, 120);
        ctx.git_signing = Some(GitSigning::Off);
        let info = mgr
            .start(
                "sh",
                template("sh", config),
                None,
                Invocation::Headless {
                    prompt: "ignored",
                    provider: None,
                    model: None,
                },
                ctx,
            )
            .unwrap();
        assert_eq!(mgr.wait(&info.id).await, Some(0));
        let output = mgr.output(&info.id);
        assert!(output.contains("V=false"), "output: {output:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn awaiting_input_slot_is_exposed_on_session_info() {
        use favetto_core::model::AwaitingInputKind;
        let mgr = AgentManager::new();
        let info = mgr
            .start(
                "sh",
                template("sh", cfg("sh", &["-c", "sleep 1"])),
                Some("task-a".to_string()),
                Invocation::Interactive {
                    prompt: None,
                    provider: None,
                    model: None,
                },
                context(None, 24, 80),
            )
            .unwrap();
        let find = |id: &str| mgr.sessions().into_iter().find(|s| s.id == id).unwrap();
        assert!(find(&info.id).awaiting_input.is_none());

        let reason = AwaitingInputReason {
            kind: AwaitingInputKind::Permission,
            message: "Allow once?".to_string(),
            request_id: None,
            options: Vec::new(),
            allow_always: false,
        };
        mgr.set_awaiting_input(&info.id, Some(reason.clone()));
        assert_eq!(find(&info.id).awaiting_input, Some(reason.clone()));
        assert_eq!(
            mgr.find_latest_by_task("task-a").unwrap().awaiting_input,
            Some(reason)
        );

        mgr.set_awaiting_input(&info.id, None);
        assert!(find(&info.id).awaiting_input.is_none());
        mgr.close(&info.id).unwrap();
    }

    #[test]
    fn detect_awaiting_input_requires_quiet() {
        let mgr = AgentManager::new();
        let agent = template(
            "sh",
            cfg(
                "sh",
                &["-c", "printf 'Permission required: allow? '; sleep 1"],
            ),
        );
        let info = mgr
            .start(
                "sh",
                agent.clone(),
                None,
                Invocation::Interactive {
                    prompt: None,
                    provider: None,
                    model: None,
                },
                context(None, 24, 80),
            )
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(250));

        // Fresh output means the quiet gate is not satisfied yet.
        assert!(mgr
            .detect_awaiting_input(&info.id, agent.as_ref(), std::time::Duration::from_secs(60))
            .is_none());
        // Once quiet, the generic fallback classifies the prompt.
        let reason = mgr
            .detect_awaiting_input(
                &info.id,
                agent.as_ref(),
                std::time::Duration::from_millis(1),
            )
            .expect("prompt should be detected");
        assert_eq!(
            reason.kind,
            favetto_core::model::AwaitingInputKind::Permission
        );
        mgr.close(&info.id).unwrap();
    }

    #[tokio::test]
    async fn detect_awaiting_input_is_none_after_exit() {
        let mgr = AgentManager::new();
        let agent = template(
            "sh",
            cfg(
                "sh",
                &["-c", "printf 'Permission required: allow? '; sleep 0.1"],
            ),
        );
        let info = mgr
            .start(
                "sh",
                agent.clone(),
                None,
                Invocation::Interactive {
                    prompt: None,
                    provider: None,
                    model: None,
                },
                context(None, 24, 80),
            )
            .unwrap();
        assert_eq!(mgr.wait(&info.id).await, Some(0));
        assert!(mgr
            .detect_awaiting_input(&info.id, agent.as_ref(), std::time::Duration::ZERO)
            .is_none());
        mgr.close(&info.id).unwrap();
    }

    /// A panic while holding the sessions lock must not poison it: with
    /// `parking_lot` the manager stays usable, unlike the old `std::sync::Mutex`
    /// whose every later `lock().unwrap()` would also panic and take down the
    /// daemon.
    #[test]
    fn sessions_lock_survives_a_panicking_holder() {
        let mgr = Arc::new(AgentManager::new());
        let holder = mgr.clone();
        let panicked = std::thread::spawn(move || {
            let _guard = holder.sessions.lock();
            panic!("simulated panic while holding the sessions lock");
        });
        assert!(panicked.join().is_err());

        // Still lockable, readable, and usable after the panic.
        assert!(mgr.sessions().is_empty());
        assert!(mgr.get("missing").is_err());
        assert!(mgr.close("missing").is_ok());
    }

    /// Poll a session's info until `check` passes.
    async fn wait_for_session(
        mgr: &AgentManager,
        id: &str,
        mut check: impl FnMut(&AgentSessionInfo) -> bool,
    ) -> AgentSessionInfo {
        for _ in 0..200 {
            if let Some(info) = mgr.sessions().into_iter().find(|s| s.id == id) {
                if check(&info) {
                    return info;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("session {id} never reached the expected state");
    }

    /// A fake `StateSource` folds into the session snapshot and answers
    /// `agents.reply` through its typed responder; a session without a source
    /// keeps the screen-only surface and rejects replies.
    #[tokio::test]
    async fn state_source_folds_live_state_and_reply_round_trips() {
        let fake = crate::agents::testing::fake_state_agent("while true; do sleep 1; done");
        let mgr = AgentManager::new();
        let info = mgr
            .start(
                "fake",
                fake.agent.clone(),
                Some("task-state".to_string()),
                Invocation::Interactive {
                    prompt: None,
                    provider: None,
                    model: None,
                },
                context(None, 24, 80),
            )
            .unwrap();

        // The source seeds `Starting`; `agents.list` would surface it via info().
        assert_eq!(info.activity, Some(AgentActivity::Starting));
        assert!(info.usage.is_none());

        // A source-reported session id is recorded on the session so the panel
        // and reattach path can see an interactive agent's own id.
        fake.events
            .send(AgentStateEvent::Session {
                session_id: Some("ses_x".to_string()),
                title: None,
                model: None,
            })
            .unwrap();
        wait_for_session(&mgr, &info.id, |s| s.session_id.as_deref() == Some("ses_x")).await;
        assert_eq!(mgr.external_session_id(&info.id).as_deref(), Some("ses_x"));

        let request = favetto_core::model::InputRequest {
            id: "perm_1".to_string(),
            kind: favetto_core::model::AwaitingInputKind::Permission,
            message: "Allow?".to_string(),
            options: vec!["Allow once".to_string()],
            allow_always: true,
        };
        fake.events
            .send(AgentStateEvent::InputRequested {
                request: request.clone(),
            })
            .unwrap();
        let info = wait_for_session(&mgr, &info.id, |s| {
            s.activity
                == Some(AgentActivity::Waiting {
                    request: request.clone(),
                })
        })
        .await;
        let awaiting = info.awaiting_input.expect("awaiting-input slot");
        assert_eq!(awaiting.request_id.as_deref(), Some("perm_1"));
        assert_eq!(awaiting.options, vec!["Allow once".to_string()]);
        assert!(awaiting.allow_always);

        fake.events
            .send(AgentStateEvent::Usage {
                usage: favetto_core::model::AgentUsage {
                    input_tokens: 7,
                    ..Default::default()
                },
            })
            .unwrap();
        let info = wait_for_session(&mgr, &info.id, |s| {
            s.usage.as_ref().is_some_and(|u| u.input_tokens == 7)
        })
        .await;
        assert_eq!(info.usage.expect("usage").input_tokens, 7);

        // `agents.reply` reaches the session's responder with the exact reply.
        mgr.reply(&info.id, "perm_1", InputReply::Once)
            .await
            .unwrap();
        assert_eq!(
            fake.responder.replies(),
            vec![("perm_1".to_string(), InputReply::Once)]
        );

        fake.events
            .send(AgentStateEvent::InputResolved {
                id: "perm_1".to_string(),
            })
            .unwrap();
        wait_for_session(&mgr, &info.id, |s| s.awaiting_input.is_none()).await;

        // A session started without a source keeps the screen fallback and has
        // no structured channel to reply to.
        let plain = mgr
            .start(
                "sh",
                template("sh", cfg("sh", &["-c", "sleep 1"])),
                None,
                Invocation::Interactive {
                    prompt: None,
                    provider: None,
                    model: None,
                },
                context(None, 24, 80),
            )
            .unwrap();
        assert!(mgr.subscribe_state(&plain.id).is_none());
        assert!(mgr.subscribe_state(&info.id).is_some());
        assert!(mgr
            .reply(&plain.id, "perm_1", InputReply::Once)
            .await
            .is_err());
        assert!(mgr
            .reply("missing", "perm_1", InputReply::Once)
            .await
            .is_err());

        mgr.close(&info.id).unwrap();
        mgr.close(&plain.id).unwrap();
    }

    #[test]
    fn hook_base_url_uses_loopback_for_unspecified_hosts() {
        assert_eq!(
            hook_base_url("127.0.0.1:7878".parse().unwrap()),
            "http://127.0.0.1:7878/agent-hooks/claude"
        );
        assert_eq!(
            hook_base_url("0.0.0.0:9000".parse().unwrap()),
            "http://127.0.0.1:9000/agent-hooks/claude"
        );
        assert_eq!(
            hook_base_url("[::1]:9000".parse().unwrap()),
            "http://[::1]:9000/agent-hooks/claude"
        );
    }

    #[test]
    fn configure_hooks_enables_a_router_and_route_hook() {
        let mut mgr = AgentManager::new();
        assert!(mgr.hook_router().is_none());
        assert_eq!(mgr.route_hook("tok", b"{}"), Err(HookRouteError::Disabled));

        let dir = std::env::temp_dir().join(format!("favetto-hooks-{}", uuid::Uuid::new_v4()));
        mgr.configure_hooks("0.0.0.0:7878", dir.clone());
        let router = mgr.hook_router().expect("router configured");
        assert!(Arc::ptr_eq(&router, &mgr.hooks.as_ref().unwrap().router));
        // The route is live but no session has registered the token yet.
        assert_eq!(
            mgr.route_hook("tok", b"{}"),
            Err(HookRouteError::UnknownToken)
        );
        assert!(matches!(
            mgr.route_hook("tok", b"not json"),
            Err(HookRouteError::BadRequest(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_listen_disables_hooks() {
        let mut mgr = AgentManager::new();
        mgr.configure_hooks("not-an-address", std::env::temp_dir());
        assert!(mgr.hook_router().is_none());
    }

    /// A probe adapter records whether the `StateSourceConfig` it receives knows
    /// about the hook router, so the manager wiring is asserted directly.
    struct CfgProbeAgent {
        descriptor: AgentDescriptor,
        seen_hook_router: Arc<parking_lot::Mutex<Option<bool>>>,
    }

    impl Agent for CfgProbeAgent {
        fn descriptor(&self) -> &AgentDescriptor {
            &self.descriptor
        }

        fn command(&self, _: &Invocation<'_>, _: &AgentContext) -> anyhow::Result<CommandSpec> {
            Ok(CommandSpec {
                program: PathBuf::from("sh"),
                args: vec!["-c".to_string(), "sleep 1".to_string()],
                env: Default::default(),
                cwd: None,
                stdin_prompt: None,
                stdin_eof: false,
                submit: SubmitStrategy::None,
            })
        }

        fn state_source(
            &self,
            cfg: &StateSourceConfig,
        ) -> Option<Box<dyn crate::agents::state::StateSource>> {
            *self.seen_hook_router.lock() = Some(cfg.hook_router.is_some());
            None
        }
    }

    #[tokio::test]
    async fn state_source_config_sees_hooks_only_after_configure() {
        let seen = Arc::new(parking_lot::Mutex::new(None));
        let descriptor = AgentDescriptor {
            id: "probe".to_string(),
            name: "Probe".to_string(),
            command: "sh".to_string(),
            available: true,
            capabilities: Default::default(),
        };
        let agent: Arc<dyn Agent> = Arc::new(CfgProbeAgent {
            descriptor,
            seen_hook_router: seen.clone(),
        });

        let mgr = AgentManager::new();
        let info = mgr
            .start(
                "probe",
                agent.clone(),
                None,
                Invocation::Interactive {
                    prompt: None,
                    provider: None,
                    model: None,
                },
                context(None, 24, 80),
            )
            .unwrap();
        assert_eq!(*seen.lock(), Some(false));
        mgr.close(&info.id).unwrap();

        let mut mgr = AgentManager::new();
        mgr.configure_hooks("127.0.0.1:7878", std::env::temp_dir());
        let info = mgr
            .start(
                "probe",
                agent,
                None,
                Invocation::Interactive {
                    prompt: None,
                    provider: None,
                    model: None,
                },
                context(None, 24, 80),
            )
            .unwrap();
        assert_eq!(*seen.lock(), Some(true));
        mgr.close(&info.id).unwrap();
    }
}
