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
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use tokio::sync::broadcast;

use favetto_core::model::AgentSessionInfo;

use crate::config::AgentConfig;

/// Scrollback lines retained by each session's server-side emulator.
const SCROLLBACK: usize = 2000;

/// Maximum raw output bytes retained per session (for capturing task results).
const MAX_RAW: usize = 1024 * 1024;

/// The favetto binary itself. Agent processes are launched through
/// `favetto __agent-exec -- <cmd>`, which sets a parent-death signal so they are
/// killed when the daemon exits (including `SIGKILL`).
fn agent_exec_program() -> anyhow::Result<PathBuf> {
    std::env::current_exe().map_err(|e| anyhow::anyhow!("cannot locate favetto binary: {e}"))
}

/// Whether to launch agents through the `__agent-exec` wrapper. Disabled under
/// unit tests (where the current exe is the test harness) and when
/// `FAVETTO_NO_AGENT_WRAP` is set.
fn wrap_agent() -> bool {
    !cfg!(test) && std::env::var_os("FAVETTO_NO_AGENT_WRAP").is_none()
}

/// Substitute `{key}` placeholders in an argument template.
fn substitute(template: &[String], vars: &[(&str, &str)]) -> Vec<String> {
    template
        .iter()
        .map(|a| {
            let mut s = a.clone();
            for (k, v) in vars {
                s = s.replace(k, v);
            }
            s
        })
        .collect()
}

/// Extract an agent session id from one line of line-delimited JSON output.
fn session_id_from_line(line: &str, key: &str) -> Option<String> {
    let line = line.trim();
    if !line.starts_with('{') {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    value.get(key)?.as_str().map(str::to_string)
}

/// Append a rendered argument template, reporting whether it carried `{prompt}`.
fn append_template(cmd: &mut CommandBuilder, template: &[String], vars: &[(&str, &str)]) -> bool {
    let saw_prompt = template.iter().any(|a| a.contains("{prompt}"));
    for a in substitute(template, vars) {
        cmd.arg(a);
    }
    saw_prompt
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

/// A full-screen terminal frame or a child exit, broadcast to connections that
/// attached to the session.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// `data` is a self-contained `vt100` formatted screen (clears then redraws).
    Output { session_id: String, data: Vec<u8> },
    Exit {
        session_id: String,
        code: Option<i32>,
    },
}

impl AgentEvent {
    pub fn session_id(&self) -> &str {
        match self {
            AgentEvent::Output { session_id, .. } | AgentEvent::Exit { session_id, .. } => {
                session_id
            }
        }
    }
}

/// How an agent is invoked for a session.
pub enum Invocation<'a> {
    /// Interactive TUI: base `args` + `prompt_args` (or the prompt on stdin).
    Interactive(Option<&'a str>),
    /// Unattended run: `run_args` when a model is given, else `headless_args`.
    Headless {
        prompt: &'a str,
        provider: Option<&'a str>,
        model: Option<&'a str>,
    },
    /// Interactive reattach to an existing agent session (`resume_args`).
    Resume(&'a str),
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
    /// Creation order, so "the task's latest session" is well-defined.
    order: u64,
}

impl Session {
    fn info(&self) -> AgentSessionInfo {
        AgentSessionInfo {
            id: self.id.clone(),
            agent: self.agent.clone(),
            task_id: self.task_id.clone(),
            running: self.running.load(Ordering::SeqCst),
            headless: self.headless,
            session_id: self.external_session_id.lock().unwrap().clone(),
        }
    }
}

/// Owns every live external-agent session on the daemon.
pub struct AgentManager {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    tx: broadcast::Sender<AgentEvent>,
    next_order: AtomicU64,
}

impl Default for AgentManager {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for AgentManager {
    fn drop(&mut self) {
        // Don't leave agent children running after the daemon exits.
        if let Ok(mut sessions) = self.sessions.lock() {
            for (_, session) in sessions.drain() {
                session.running.store(false, Ordering::SeqCst);
                terminate(session.pid);
            }
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
        }
    }

    /// Subscribe to every session's output; callers filter by `session_id`.
    pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.tx.subscribe()
    }

    /// Snapshot of all live (and recently exited) sessions.
    pub fn sessions(&self) -> Vec<AgentSessionInfo> {
        let sessions = self.sessions.lock().unwrap();
        let mut out: Vec<_> = sessions.values().map(|s| s.info()).collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// The most recently created session attached to `task_id`, running or not.
    /// Opening a task reattaches to this so a finished agent isn't re-launched.
    pub fn find_latest_by_task(&self, task_id: &str) -> Option<AgentSessionInfo> {
        self.sessions
            .lock()
            .unwrap()
            .values()
            .filter(|s| s.task_id.as_deref() == Some(task_id))
            .max_by_key(|s| s.order)
            .map(|s| s.info())
    }

    /// Spawn `cfg` in a PTY for the given invocation.
    pub fn start(
        &self,
        name: &str,
        cfg: &AgentConfig,
        task_id: Option<String>,
        invocation: Invocation<'_>,
        rows: u16,
        cols: u16,
    ) -> anyhow::Result<AgentSessionInfo> {
        if cfg.command.trim().is_empty() {
            anyhow::bail!("agent '{name}' has no command configured");
        }

        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows: rows.max(1),
            cols: cols.max(1),
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut cmd = if wrap_agent() {
            let mut c = CommandBuilder::new(agent_exec_program()?);
            c.arg("__agent-exec");
            c.arg("--");
            c.arg(&cfg.command);
            c
        } else {
            CommandBuilder::new(&cfg.command)
        };

        // Build the argument list for the invocation. `prompt_via_args` records
        // whether the prompt was placed on the command line; otherwise it is fed
        // over stdin. `headless` controls the EOT sent so a run sees EOF.
        let mut prompt_via_args = false;
        let mut stdin_prompt: Option<&str> = None;
        let mut headless = false;
        match invocation {
            Invocation::Interactive(prompt) => {
                for a in &cfg.args {
                    cmd.arg(a);
                }
                if let (Some(p), Some(prompt_args)) = (prompt, cfg.prompt_args.as_ref()) {
                    let vars = [("{prompt}", p)];
                    prompt_via_args = append_template(&mut cmd, prompt_args, &vars);
                }
                if !prompt_via_args {
                    stdin_prompt = prompt;
                }
            }
            Invocation::Headless {
                prompt,
                provider,
                model,
            } => {
                headless = true;
                let template = if model.is_some() {
                    cfg.run_args.as_deref().ok_or_else(|| {
                        anyhow::anyhow!(
                            "agent '{name}' has no `run_args`, so it cannot run a task with a \
                             model; add one, e.g. run_args = [\"run\", \"--model\", \
                             \"{{provider}}/{{model}}\", \"--format\", \"json\", \"{{prompt}}\"]"
                        )
                    })?
                } else {
                    cfg.headless_args.as_deref().ok_or_else(|| {
                        anyhow::anyhow!(
                            "agent '{name}' has no `headless_args`, so it cannot run a task \
                             unattended; add one, e.g. headless_args = [\"run\", \"--auto\", \
                             \"{{prompt}}\"]"
                        )
                    })?
                };
                let vars = [
                    ("{prompt}", prompt),
                    ("{provider}", provider.unwrap_or("")),
                    ("{model}", model.unwrap_or("")),
                ];
                prompt_via_args = append_template(&mut cmd, template, &vars);
                if !prompt_via_args {
                    stdin_prompt = Some(prompt);
                }
            }
            Invocation::Resume(session_id) => {
                let template = cfg.resume_args.as_deref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "agent '{name}' has no `resume_args`, so it cannot reattach to a session"
                    )
                })?;
                let vars = [("{session_id}", session_id)];
                append_template(&mut cmd, template, &vars);
            }
        }

        if let Some(cwd) = &cfg.cwd {
            cmd.cwd(cwd);
        }
        cmd.env("TERM", "xterm-256color");
        for (k, v) in &cfg.env {
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
        if let Some(prompt) = stdin_prompt {
            let mut w = writer.lock().unwrap();
            let _ = w.write_all(prompt.as_bytes());
            let _ = w.write_all(b"\r");
            if headless {
                let _ = w.write_all(&[0x04]);
            }
            let _ = w.flush();
        }

        let id = uuid::Uuid::new_v4().to_string();
        let order = self.next_order.fetch_add(1, Ordering::SeqCst);
        let parser = Arc::new(Mutex::new(vt100::Parser::new(
            rows.max(1),
            cols.max(1),
            SCROLLBACK,
        )));
        // Counts output chunks; used only to detect activity/quiet for `submit_prompt`.
        let ticks = Arc::new(AtomicU64::new(0));
        let running = Arc::new(AtomicBool::new(true));
        let raw = Arc::new(Mutex::new(Vec::new()));
        let exit_code = Arc::new(std::sync::atomic::AtomicI32::new(-1));
        let external_session_id = Arc::new(Mutex::new(None::<String>));
        let session_key = cfg.session_id_json_key.clone();
        let tx = self.tx.clone();

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
            std::thread::spawn(move || {
                let mut chunk = [0u8; 8192];
                let mut line_buf: Vec<u8> = Vec::new();
                loop {
                    match reader.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => {
                            let bytes = &chunk[..n];
                            {
                                let mut buf = raw_out.lock().unwrap();
                                buf.extend_from_slice(bytes);
                                if buf.len() > MAX_RAW {
                                    let excess = buf.len() - MAX_RAW;
                                    buf.drain(..excess);
                                }
                            }
                            // Capture the agent's own session id from line-delimited
                            // JSON output (e.g. `opencode run --format json`).
                            if let Some(key) = &session_key {
                                line_buf.extend_from_slice(bytes);
                                while let Some(pos) = line_buf.iter().position(|&b| b == b'\n') {
                                    let line: Vec<u8> = line_buf.drain(..=pos).collect();
                                    let line = String::from_utf8_lossy(&line);
                                    if let Some(found) = session_id_from_line(&line, key) {
                                        let mut slot = external_session_id.lock().unwrap();
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
                                let mut p = parser.lock().unwrap();
                                p.process(bytes);
                                (p.screen().state_formatted(), terminal_replies(p.screen(), bytes))
                            };
                            ticks.fetch_add(1, Ordering::SeqCst);
                            if !replies.is_empty() {
                                if let Ok(mut w) = writer.lock() {
                                    let _ = w.write_all(&replies);
                                    let _ = w.flush();
                                }
                            }
                            let _ = tx.send(AgentEvent::Output {
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
                let _ = tx.send(AgentEvent::Exit {
                    session_id: id,
                    code: Some(code),
                });
            });
        }

        // If the prompt was only pre-filled via `prompt_args`, submit it once the
        // agent's UI has settled (interactive sessions only).
        if !headless && cfg.submit_prompt && prompt_via_args {
            spawn_submit(writer.clone(), ticks.clone(), running.clone());
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
            order,
        });
        let info = session.info();
        self.sessions.lock().unwrap().insert(id, session);
        Ok(info)
    }

    /// The current full-screen frame for a (re)attaching client.
    pub fn attach(&self, session_id: &str) -> anyhow::Result<(AgentSessionInfo, Vec<u8>)> {
        let session = self.get(session_id)?;
        let frame = session
            .parser
            .lock()
            .unwrap()
            .screen()
            .state_formatted();
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

    /// Raw output captured for a session (used to record an unattended task's output).
    pub fn output(&self, session_id: &str) -> String {
        self.get(session_id)
            .map(|s| String::from_utf8_lossy(&s.raw.lock().unwrap()).to_string())
            .unwrap_or_default()
    }

    /// The agent's own session id captured from a run, if any.
    pub fn external_session_id(&self, session_id: &str) -> Option<String> {
        self.get(session_id)
            .ok()
            .and_then(|s| s.external_session_id.lock().unwrap().clone())
    }

    /// Write raw bytes to the session's PTY (keystrokes from the TUI).
    pub fn input(&self, session_id: &str, data: &[u8]) -> anyhow::Result<()> {
        let session = self.get(session_id)?;
        let mut writer = session.writer.lock().unwrap();
        writer.write_all(data)?;
        writer.flush()?;
        Ok(())
    }

    /// Resize the session's PTY and server-side emulator.
    pub fn resize(&self, session_id: &str, rows: u16, cols: u16) -> anyhow::Result<()> {
        let session = self.get(session_id)?;
        let (rows, cols) = (rows.max(1), cols.max(1));
        session.master.lock().unwrap().resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        session.parser.lock().unwrap().screen_mut().set_size(rows, cols);
        Ok(())
    }

    /// Terminate a session and drop it from the registry.
    pub fn close(&self, session_id: &str) -> anyhow::Result<()> {
        if let Some(session) = self.sessions.lock().unwrap().remove(session_id) {
            session.running.store(false, Ordering::SeqCst);
            terminate(session.pid);
        }
        Ok(())
    }


    fn get(&self, session_id: &str) -> anyhow::Result<Arc<Session>> {
        self.sessions
            .lock()
            .unwrap()
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
        (b"\x1b]10;?".as_slice(), b"\x1b]10;rgb:ffff/ffff/ffff\x07".as_slice()),
        (b"\x1b]11;?".as_slice(), b"\x1b]11;rgb:0000/0000/0000\x07".as_slice()),
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
) {
    use std::time::{Duration, Instant};

    const FIRST_OUTPUT_TIMEOUT: Duration = Duration::from_secs(15);
    const INITIAL_DELAY: Duration = Duration::from_millis(1200);
    const RETRY_INTERVAL: Duration = Duration::from_millis(1200);
    const MAX_SENDS: u32 = 8;
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

        std::thread::sleep(INITIAL_DELAY);
        let mut sent = 0u32;
        let mut before = ticks.load(Ordering::SeqCst);
        while running.load(Ordering::SeqCst) && sent < MAX_SENDS {
            if let Ok(mut w) = writer.lock() {
                let _ = w.write_all(b"\r");
                let _ = w.flush();
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(command: &str, args: &[&str]) -> AgentConfig {
        AgentConfig {
            command: command.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn substitute_renders_placeholders() {
        let template = vec!["run".to_string(), "--model".to_string(), "{provider}/{model}".to_string()];
        let rendered = substitute(
            &template,
            &[("{provider}", "jev"), ("{model}", "1.13")],
        );
        assert_eq!(rendered, vec!["run", "--model", "jev/1.13"]);
    }

    #[test]
    fn session_id_from_line_extracts_key() {
        let line = r#"{"type":"text","timestamp":1,"sessionID":"ses_abc","part":{}}"#;
        assert_eq!(
            session_id_from_line(line, "sessionID").as_deref(),
            Some("ses_abc")
        );
        assert_eq!(session_id_from_line("not json", "sessionID"), None);
        assert_eq!(session_id_from_line(r#"{"other":1}"#, "sessionID"), None);
    }

    #[test]
    fn attach_returns_current_frame() {
        let mgr = AgentManager::new();
        let agent = cfg("sh", &["-c", "printf hello-pty; sleep 1"]);
        let info = mgr.start("sh", &agent, None, Invocation::Interactive(None), 24, 80).unwrap();

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
            .start("cat", &cfg("cat", &[]), None, Invocation::Interactive(None), 24, 80)
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
    fn pty_resize_does_not_error() {
        let mgr = AgentManager::new();
        let info = mgr
            .start("cat", &cfg("cat", &[]), None, Invocation::Interactive(None), 24, 80)
            .unwrap();
        mgr.resize(&info.id, 40, 120).unwrap();
        mgr.close(&info.id).unwrap();
    }

    #[test]
    fn submit_prompt_presses_enter_after_quiet() {
        let mgr = AgentManager::new();
        let mut agent = cfg("sh", &[]);
        agent.prompt_args = Some(vec![
            "-c".to_string(),
            "printf ready; read x; echo got:$x".to_string(),
            "{prompt}".to_string(),
        ]);
        agent.submit_prompt = true;
        // Prompt goes via prompt_args, so it is not written to stdin; the submit
        // thread must press Enter for `read` to unblock.
        let info = mgr
            .start("sh", &agent, None, Invocation::Interactive(Some("ignored")), 24, 80)
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
        let mut agent = cfg("sh", &[]);
        agent.headless_args = Some(vec!["-c".to_string(), "cat".to_string()]);
        let info = mgr
            .start(
                "sh",
                &agent,
                Some("task-1".to_string()),
                Invocation::Headless {
                    prompt: "hello-headless",
                    provider: None,
                    model: None,
                },
                40,
                120,
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
        let mut agent = cfg("sh", &[]);
        agent.run_args = Some(vec![
            "-c".to_string(),
            r#"printf '{"sessionID":"ses_123"}\n'"#.to_string(),
        ]);
        agent.session_id_json_key = Some("sessionID".to_string());
        let info = mgr
            .start(
                "sh",
                &agent,
                Some("task-2".to_string()),
                Invocation::Headless {
                    prompt: "hi",
                    provider: Some("jev"),
                    model: Some("1.13"),
                },
                40,
                120,
            )
            .unwrap();
        assert_eq!(mgr.wait(&info.id).await, Some(0));
        assert_eq!(
            mgr.external_session_id(&info.id).as_deref(),
            Some("ses_123")
        );
    }
}
