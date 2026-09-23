//! The `tui` subcommand: a ratatui client that talks to the daemon over the same
//! wire protocol whether attached locally (Unix socket) or remotely (WebSocket).

mod app;
mod editor;
mod json;
mod markdown;
mod sound;
mod term;
mod text_buffer;
mod theme;
mod ui;
mod workflow_view;

use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use base64::Engine as _;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    self, DisableFocusChange, DisableMouseCapture, EnableFocusChange, EnableMouseCapture,
    Event as CEvent, KeyEventKind,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::Terminal;
use tokio::sync::mpsc;

use favetto_core::model::{
    AgentCatalogEntry, AgentSessionInfo, Event, NotificationRecord, Schedule, Task,
};
use favetto_core::rpc::method;
use favetto_providers::Provider;

use crate::cli::TuiArgs;
use crate::client::{Client, Transport};
use crate::config::FavettoConfig;
use app::{App, CatalogEntry, ConnState, Popup, UiAction};
use sound::{CliSound, SoundPlayer};

/// Outcome of a connected session: the user quit, or the link dropped.
enum SessionOutcome {
    Quit,
    Disconnected,
}

/// Initial reconnect backoff after a dropped or failed connection.
const INITIAL_BACKOFF: Duration = Duration::from_millis(250);
/// Upper bound on the reconnect backoff.
const MAX_BACKOFF: Duration = Duration::from_secs(10);
/// Upper bound on the initial snapshot fetch. Six sequential RPCs at the default
/// 5s request timeout could otherwise stall a half-dead peer for ~30s before the
/// interaction loop even starts.
const INITIAL_SYNC_TIMEOUT: Duration = Duration::from_secs(10);

/// Pause/resume handshake for the input-reader thread. Before handing the
/// terminal to an editor we must guarantee the reader is not consuming keys;
/// `pause` blocks (bounded) until the thread acknowledges `paused`.
#[derive(Clone)]
struct InputPause {
    paused: Arc<AtomicBool>,
    acked: Arc<AtomicBool>,
}

impl InputPause {
    fn new() -> Self {
        Self {
            paused: Arc::new(AtomicBool::new(false)),
            acked: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Block (bounded) until the reader thread observes the pause.
    fn pause(&self) {
        self.acked.store(false, Ordering::SeqCst);
        self.paused.store(true, Ordering::SeqCst);
        for _ in 0..100 {
            if self.acked.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
        self.acked.store(false, Ordering::SeqCst);
    }
}

pub async fn run(args: TuiArgs) -> anyhow::Result<()> {
    // Client-local config: CLI --config, else ~/.config/favetto/config.toml. The
    // daemon parses the same file and ignores the [tui] section.
    let config_path = args
        .config
        .clone()
        .unwrap_or_else(crate::cli::default_config_path);
    let config = FavettoConfig::load_from(&config_path).unwrap_or_default();

    // Editor precedence: `[tui].editor` > `$VISUAL` > `$EDITOR` > `vi`.
    let editor = editor::resolve_editor_command(
        config.tui.editor.as_deref(),
        std::env::var("VISUAL").ok().as_deref(),
        std::env::var("EDITOR").ok().as_deref(),
    );

    let cli_sound = CliSound {
        enabled: if args.no_sound {
            Some(false)
        } else if args.sound {
            Some(true)
        } else {
            None
        },
        command: args.sound_command.clone(),
    };
    let sound_cfg = sound::resolve_from_env(config.tui.sound.clone(), &cli_sound);

    // `--test-sound` previews every cue and exits before touching the terminal.
    if args.test_sound {
        sound::test(&sound_cfg)?;
        return Ok(());
    }

    // The player lives across reconnects; its worker never blocks this loop.
    let player = SoundPlayer::start(sound_cfg.clone());

    let transport = resolve_transport(&args).await;

    enable_raw_mode().context("enable raw mode")?;
    // Resolve the palette now, before the input-reader thread exists: the OSC 11
    // query needs to read the terminal's reply from stdin itself.
    let theme = theme::Theme::detect();
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableFocusChange
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("init terminal")?;

    // Input on a dedicated thread so the async loop can select between key presses,
    // pushes, and pings. The thread can be paused while an editor owns the terminal.
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<CEvent>();
    let input = InputPause::new();
    let reader_control = input.clone();
    std::thread::spawn(move || loop {
        if reader_control.paused.load(Ordering::SeqCst) {
            reader_control.acked.store(true, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        reader_control.acked.store(false, Ordering::SeqCst);
        if event::poll(Duration::from_millis(50)).unwrap_or(false) {
            if let Ok(ev) = event::read() {
                if ev_tx.send(ev).is_err() {
                    break;
                }
            }
        }
    });

    let mut app = App::with_theme(theme);
    app.sound_enabled = sound_cfg.enabled;
    let mut backoff = INITIAL_BACKOFF;
    let result = loop {
        app.conn = ConnState::Connecting;
        app.conn_detail = transport_desc(&transport);
        // Paint the connecting state before the (bounded) connect attempt.
        let _ = terminal.draw(|f| ui::draw(f, &mut app));

        // `Client::connect` is bounded by a connect timeout, so a dead or
        // black-holed peer cannot wedge the loop for the OS TCP timeout.
        match Client::connect(transport.clone()).await {
            Ok(client) => {
                app.conn = ConnState::Connected;
                app.conn_detail = transport_desc(&transport);
                if tokio::time::timeout(INITIAL_SYNC_TIMEOUT, sync_initial(&client, &mut app))
                    .await
                    .is_ok()
                {
                    match run_session(
                        &client,
                        &mut app,
                        &mut ev_rx,
                        &mut terminal,
                        &player,
                        &input,
                        &editor,
                    )
                    .await
                    {
                        SessionOutcome::Quit => break Ok(()),
                        SessionOutcome::Disconnected => {
                            app.conn = ConnState::Disconnected;
                            app.conn_detail = "reconnecting…".to_string();
                            // A live session dropped: retry promptly.
                            backoff = INITIAL_BACKOFF;
                        }
                    }
                } else {
                    // The peer accepted the connection but stalled the initial
                    // sync; treat it as a failed attempt and keep backing off.
                    app.conn = ConnState::Disconnected;
                    app.conn_detail = "reconnecting…".to_string();
                }
            }
            Err(e) => {
                app.conn = ConnState::Disconnected;
                app.conn_detail = e.to_string();
            }
        }

        if app.should_quit {
            break Ok(());
        }

        // Wait out the backoff without freezing the UI: keep processing global
        // keys against the last snapshot, quit immediately, and reconnect at
        // once if the user does something that needs the daemon.
        match disconnected_pump(&mut app, &mut ev_rx, &mut terminal, &player, backoff).await {
            PumpOutcome::Quit => break Ok(()),
            // Both a finished wait and an input-triggered retry grow the backoff
            // so a repeated failure cannot hammer the peer.
            PumpOutcome::Retry | PumpOutcome::Reconnect => {
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    };

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        DisableFocusChange,
        LeaveAlternateScreen
    )?;

    result
}

/// Fetch the initial snapshot and subscribe for live pushes.
async fn sync_initial(client: &Client, app: &mut App) {
    if let Ok(resp) = client.request(method::PING, serde_json::json!({})).await {
        if let Some(v) = resp.result {
            app.daemon_cwd = v.get("cwd").and_then(|c| c.as_str()).map(str::to_string);
        }
    }

    if let Ok(resp) = client
        .request(method::TASKS_LIST, serde_json::json!({}))
        .await
    {
        if let Some(v) = resp.result {
            if let Ok(tasks) = serde_json::from_value::<Vec<Task>>(v) {
                app.tasks = tasks;
            }
        }
    }

    if let Ok(resp) = client
        .request(method::EVENTS_TAIL, serde_json::json!({ "limit": 100 }))
        .await
    {
        if let Some(v) = resp.result {
            if let Ok(events) = serde_json::from_value::<Vec<Event>>(v) {
                // Replayed history must not fire a burst of stale sounds.
                app.sound_suppressed = true;
                for ev in events {
                    app.ingest_event(ev);
                }
                app.sound_suppressed = false;
            }
        }
    }

    if let Ok(resp) = client
        .request(method::SCHEDULES_LIST, serde_json::json!({}))
        .await
    {
        if let Some(v) = resp.result {
            if let Ok(schedules) = serde_json::from_value::<Vec<Schedule>>(v) {
                app.schedules = schedules;
            }
        }
    }

    if let Ok(resp) = client
        .request(
            method::NOTIFICATIONS_LIST,
            serde_json::json!({ "limit": 100 }),
        )
        .await
    {
        if let Some(v) = resp.result {
            if let Ok(notifications) = serde_json::from_value::<Vec<NotificationRecord>>(v) {
                app.notifications = notifications;
            }
        }
    }

    let _ = client
        .request(
            method::EVENTS_SUBSCRIBE,
            serde_json::json!({ "last_event_id": app.last_event_id }),
        )
        .await;

    fetch_catalog(client, app).await;
}

/// Fetch the task catalog.
async fn fetch_catalog(client: &Client, app: &mut App) {
    if let Ok(resp) = client
        .request(method::CATALOG_LIST, serde_json::json!({}))
        .await
    {
        if let Some(v) = resp.result {
            if let Ok(catalog) = serde_json::from_value::<Vec<CatalogEntry>>(v) {
                app.set_catalog(catalog);
            }
        }
    }
}

/// Fetch the catalog workflow graph (DOT) for the workflow overlay.
async fn fetch_workflow(client: &Client, app: &mut App) {
    match client
        .request(method::WORKFLOW_GET, serde_json::json!({}))
        .await
    {
        Ok(resp) => match resp.result {
            Some(v) => {
                let dot = v
                    .get("dot")
                    .and_then(|d| d.as_str())
                    .unwrap_or_default()
                    .to_string();
                let path = v.get("path").and_then(|p| p.as_str()).map(str::to_string);
                // Additive field: older daemons omit it, so fall back to DOT.
                let graph = v
                    .get("graph")
                    .cloned()
                    .and_then(|g| serde_json::from_value::<crate::workflow::WorkflowGraph>(g).ok());
                app.set_workflow(dot, path, graph);
            }
            None => app.set_workflow(String::new(), None, None),
        },
        Err(e) => {
            app.logs.push_back(format!("workflow.get failed: {e}"));
            app.set_workflow(format!("<failed to fetch workflow: {e}>"), None, None);
        }
    }
}

/// Load the raw Markdown for the selected catalog task, if the preview needs it.
async fn maybe_load_catalog_preview(client: &Client, app: &mut App) {
    let Some(name) = app.catalog_preview_target() else {
        return;
    };
    app.catalog_preview_pending = Some(name.clone());
    let markdown = match client
        .request(method::CATALOG_GET, serde_json::json!({ "name": name }))
        .await
    {
        Ok(resp) => resp
            .result
            .and_then(|v| {
                v.get("markdown")
                    .and_then(|m| m.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_default(),
        Err(e) => {
            app.logs.push_back(format!("catalog.get failed: {e}"));
            String::new()
        }
    };
    app.catalog_preview = Some((name, markdown));
    app.catalog_preview_pending = None;
}

/// Re-fetch the list-based tabs (used after a form submission).
async fn refresh_lists(client: &Client, app: &mut App) {
    if let Ok(resp) = client
        .request(method::TASKS_LIST, serde_json::json!({}))
        .await
    {
        if let Some(v) = resp.result {
            if let Ok(tasks) = serde_json::from_value::<Vec<Task>>(v) {
                app.tasks = tasks;
            }
        }
    }
    if let Ok(resp) = client
        .request(method::SCHEDULES_LIST, serde_json::json!({}))
        .await
    {
        if let Some(v) = resp.result {
            if let Ok(schedules) = serde_json::from_value::<Vec<Schedule>>(v) {
                app.schedules = schedules;
            }
        }
    }
    if let Ok(resp) = client
        .request(
            method::NOTIFICATIONS_LIST,
            serde_json::json!({ "limit": 100 }),
        )
        .await
    {
        if let Some(v) = resp.result {
            if let Ok(notifications) = serde_json::from_value::<Vec<NotificationRecord>>(v) {
                app.notifications = notifications;
            }
        }
    }
    fetch_catalog(client, app).await;
}

/// Send a `tasks.start` request and refresh the list tabs afterwards.
async fn start_task(client: &Client, app: &mut App, params: serde_json::Value) {
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    match client.request(method::TASKS_START, params).await {
        Ok(resp) => match resp.result {
            Some(_) => app.logs.push_back(format!("started task: {name}")),
            None => app
                .logs
                .push_back(format!("start task error: {:?}", resp.error)),
        },
        Err(e) => app.logs.push_back(format!("start task failed: {e}")),
    }
    refresh_lists(client, app).await;
}

/// Disable raw mode / alt screen / mouse / focus so a child owns the terminal.
fn suspend_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> anyhow::Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableFocusChange
    )?;
    Ok(())
}

/// Restore the TUI after the child exits and force a full repaint.
fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> anyhow::Result<()> {
    enable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableFocusChange
    )?;
    terminal.clear()?;
    Ok(())
}

/// Open the selected catalog task's Markdown in the user's editor, then persist
/// it over `catalog.update` and refresh the list/preview.
///
/// The editor runs on the client with a client-local temp file (so remote attach
/// works without a shared filesystem). The input reader is paused and the
/// terminal suspended for the child's lifetime, and the TUI is restored on every
/// path before this returns.
async fn edit_catalog_task(
    client: &Client,
    app: &mut App,
    name: &str,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    ev_rx: &mut mpsc::UnboundedReceiver<CEvent>,
    input: &InputPause,
    editor: &[String],
) {
    // Fetch the raw Markdown; an empty body is treated as a failure rather than
    // opening a blank editor over a missing file.
    let markdown = match client
        .request(method::CATALOG_GET, serde_json::json!({ "name": name }))
        .await
    {
        Ok(resp) => resp
            .result
            .and_then(|v| {
                v.get("markdown")
                    .and_then(|m| m.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_default(),
        Err(e) => {
            app.logs.push_back(format!("catalog.get failed: {e}"));
            return;
        }
    };
    if markdown.is_empty() {
        app.logs
            .push_back(format!("catalog.get: no markdown for '{name}'"));
        return;
    }

    let tmp = editor::temp_edit_path(name);
    if let Err(e) = std::fs::write(&tmp, &markdown) {
        app.logs
            .push_back(format!("edit: cannot write {}: {e}", tmp.display()));
        return;
    }

    // Stop the reader before disabling raw mode, then drop any key it queued in
    // the ~50 ms between the `e` press and the pause taking effect.
    input.pause();
    while ev_rx.try_recv().is_ok() {}

    if let Err(e) = suspend_terminal(terminal) {
        input.resume();
        let _ = std::fs::remove_file(&tmp);
        app.logs.push_back(format!("edit: suspend failed: {e}"));
        return;
    }

    // Run the editor on the client with a client-local temp file. The terminal
    // is now owned by the child.
    let status = tokio::process::Command::new(&editor[0])
        .args(&editor[1..])
        .arg(&tmp)
        .status()
        .await;

    // Always restore, even when the editor failed to start.
    if let Err(e) = restore_terminal(terminal) {
        app.logs.push_back(format!("edit: restore failed: {e}"));
    }
    input.resume();

    let edited = std::fs::read_to_string(&tmp);
    let _ = std::fs::remove_file(&tmp);
    let edited = match edited {
        Ok(s) => s,
        Err(e) => {
            app.logs
                .push_back(format!("edit: cannot read {}: {e}", tmp.display()));
            return;
        }
    };

    match status {
        Err(e) => app.logs.push_back(format!("editor failed to start: {e}")),
        Ok(s) if !s.success() => app.logs.push_back(format!("editor exited with status {s}")),
        Ok(_) => {}
    }

    // A no-op editor quit (or a failed start) leaves the file untouched.
    if edited == markdown {
        return;
    }
    match client
        .request(
            method::CATALOG_UPDATE,
            serde_json::json!({ "name": name, "markdown": edited }),
        )
        .await
    {
        Ok(resp) => match resp.result {
            Some(_) => {
                app.logs.push_back(format!("updated task: {name}"));
                fetch_catalog(client, app).await;
            }
            None => app
                .logs
                .push_back(format!("catalog.update error: {:?}", resp.error)),
        },
        Err(e) => app.logs.push_back(format!("catalog.update failed: {e}")),
    }
}

/// The connected interaction loop: redraw, handle keys, apply pushes, ping.
async fn run_session(
    client: &Client,
    app: &mut App,
    ev_rx: &mut mpsc::UnboundedReceiver<CEvent>,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    player: &SoundPlayer,
    input: &InputPause,
    editor: &[String],
) -> SessionOutcome {
    let mut push_rx = client.subscribe();
    let mut closed_rx = client.closed();
    let mut ping = tokio::time::interval(Duration::from_secs(5));
    let mut anim = tokio::time::interval(Duration::from_millis(80));
    anim.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        // The tick is only consumed while something on screen animates; an idle
        // TUI draws solely in response to real events.
        let animate = app.needs_animation();
        let _ = terminal.draw(|f| ui::draw(f, app));

        // Keep the daemon-side PTY in sync with the panel size.
        if let (Some(sid), Some((rows, cols))) =
            (app.agent_session_id.clone(), app.agent_resize.take())
        {
            let _ = client
                .request(
                    method::AGENTS_RESIZE,
                    serde_json::json!({ "session_id": sid, "rows": rows, "cols": cols }),
                )
                .await;
        }

        tokio::select! {
            biased;
            _ = anim.tick(), if animate => {
                app.throbber_state.calc_next();
            }
            ev = ev_rx.recv() => {
                match ev {
                    Some(CEvent::Key(k)) if k.kind == KeyEventKind::Press => {
                        match app.handle_key(k) {
                            UiAction::Quit => return SessionOutcome::Quit,
                            UiAction::OpenAgent(task_id) => {
                                open_agent(client, app, task_id, false).await;
                            }
                            UiAction::NewAgent(task_id) => {
                                open_agent(client, app, task_id, true).await;
                            }
                            UiAction::OpenSessions => {
                                fetch_sessions(client, app).await;
                            }
                            UiAction::AttachSession(session_id) => {
                                attach_session(client, app, session_id).await;
                            }
                            UiAction::AgentInput(bytes) => {
                                send_agent_input(client, app, &bytes).await;
                            }
                            UiAction::StartTask(name) => {
                                start_task(client, app, serde_json::json!({ "name": name })).await;
                            }
                            UiAction::StartTaskWithInput { name, input } => {
                                start_task(
                                    client,
                                    app,
                                    serde_json::json!({ "name": name, "input": input }),
                                )
                                .await;
                            }
                            UiAction::EditCatalog(name) => {
                                edit_catalog_task(
                                    client, app, &name, terminal, ev_rx, input, editor,
                                )
                                .await;
                            }
                            UiAction::OpenWizard => {
                                fetch_agents(client, app).await;
                            }
                            UiAction::OpenWorkflow => {
                                fetch_workflow(client, app).await;
                            }
                            UiAction::WizardLoadProviders => {
                                load_wizard_providers(client, app).await;
                            }
                            UiAction::WizardStart { agent, provider, model, cwd } => {
                                start_oneshot(client, app, agent, provider, model, cwd).await;
                            }
                            UiAction::Submit { method, params } => {
                                match client.request(method, params).await {
                                    Ok(resp) => {
                                        match resp.result {
                                            Some(_) => app.logs.push_back(format!("{method}: ok")),
                                            None => app.logs.push_back(format!("{method}: error {:?}", resp.error)),
                                        }
                                    }
                                    Err(e) => app.logs.push_back(format!("{method} failed: {e}")),
                                }
                                refresh_lists(client, app).await;
                            }
                            UiAction::None => {}
                        }
                    }
                    Some(CEvent::Mouse(m)) => {
                        if let UiAction::AgentInput(bytes) = app.handle_mouse(m) {
                            send_agent_input(client, app, &bytes).await;
                        }
                    }
                    Some(CEvent::FocusGained) => player.set_focused(true),
                    Some(CEvent::FocusLost) => player.set_focused(false),
                    Some(_) => {}
                    None => return SessionOutcome::Quit,
                }
            }
            res = push_rx.recv() => {
                match res {
                    Ok(n) => app.handle_notification(n),
                    Err(_) => return SessionOutcome::Disconnected,
                }
            }
            _ = closed_rx.recv() => {
                return SessionOutcome::Disconnected;
            }
            _ = ping.tick() => {
                if client.request(method::PING, serde_json::json!({})).await.is_err() {
                    return SessionOutcome::Disconnected;
                }
            }
        }

        // Keep the worker's mute/focus state in sync, then forward any cues the
        // pushes queued. `play` only sends on a channel, so this never blocks.
        if player.muted() != app.sound_muted {
            player.set_muted(app.sound_muted);
        }
        if app.sound_muted {
            app.take_sound_cues();
        } else {
            for cue in app.take_sound_cues() {
                player.play(cue);
            }
        }

        // Load the Catalog preview for the selected task, if needed.
        if app.catalog_dirty {
            app.catalog_dirty = false;
            fetch_catalog(client, app).await;
            if matches!(app.popup, Popup::Workflow { .. }) {
                fetch_workflow(client, app).await;
            }
        }
        maybe_load_catalog_preview(client, app).await;
    }
}

/// Resolve a task's catalog entry and open its agent session, seeding a new one
/// with the task prompt when `force_new` is set.
async fn open_agent(client: &Client, app: &mut App, task_id: String, force_new: bool) {
    let task = app
        .tasks
        .iter()
        .find(|t| t.id.to_string() == task_id)
        .cloned();
    let entry = task
        .as_ref()
        .and_then(|t| app.catalog.iter().find(|c| c.name == t.name).cloned());

    let (rows, cols) = app.term.size();
    let mut params = serde_json::json!({ "task_id": task_id, "rows": rows, "cols": cols });
    if force_new {
        params["new"] = serde_json::json!(true);
    }
    if let Some(agent) = entry.as_ref().and_then(|e| e.agent.clone()) {
        params["agent"] = serde_json::json!(agent);
    }
    if let Some(provider) = entry.as_ref().and_then(|e| e.provider.clone()) {
        params["provider"] = serde_json::json!(provider);
    }
    if let Some(model) = entry.as_ref().and_then(|e| e.model.clone()) {
        params["model"] = serde_json::json!(model);
    }
    // Reattach to the agent's own session (e.g. opencode) once a run has captured it.
    if let Some(session_id) = task.as_ref().and_then(|t| t.session_id.clone()) {
        params["session_id"] = serde_json::json!(session_id);
    }
    if let Some(prompt) = entry
        .as_ref()
        .map(|e| e.prompt.clone())
        .filter(|p| !p.is_empty())
    {
        params["prompt"] = serde_json::json!(prompt);
    }
    if let Some(cwd) = task
        .as_ref()
        .and_then(|t| t.input.get("cwd"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| entry.as_ref().and_then(|e| e.cwd.clone()))
    {
        params["cwd"] = serde_json::json!(cwd);
    }

    match client.request(method::AGENTS_START, params).await {
        Ok(resp) => apply_agent_response(app, resp),
        Err(e) => agent_request_failed(app, "agents.start", &e.to_string()),
    }
}

/// Apply an `agents.start` / `tasks.start_oneshot` response: open the returned
/// session (with its current frame) in the Agent panel.
fn apply_agent_response(app: &mut App, resp: favetto_core::rpc::Response) {
    match resp.result {
        Some(v) => {
            let session = v
                .get("session")
                .cloned()
                .and_then(|s| serde_json::from_value::<AgentSessionInfo>(s).ok());
            let frame = v
                .get("data")
                .and_then(|d| d.as_str())
                .and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok())
                .unwrap_or_default();
            match session {
                Some(session) => app.open_agent(session, &frame),
                None => agent_request_failed(app, "agent response", "malformed session"),
            }
        }
        None => {
            let message = resp
                .error
                .as_ref()
                .map(|e| e.message.clone())
                .unwrap_or_else(|| "agent request failed".to_string());
            agent_request_failed(app, "agent", &message);
        }
    }
}

/// Surface a failed `agents.start` / `agents.attach` (or malformed response) in
/// the status bar, and keep a copy in the (unrendered) log ring for debugging.
fn agent_request_failed(app: &mut App, context: &str, message: &str) {
    app.agent_error = Some(message.to_string());
    app.logs.push_back(format!("{context}: {message}"));
}

/// Fetch the configured agents and open the one-shot wizard.
async fn fetch_agents(client: &Client, app: &mut App) {
    match client
        .request(method::AGENTS_LIST, serde_json::json!({}))
        .await
    {
        Ok(resp) => match resp.result {
            Some(v) => match serde_json::from_value::<Vec<AgentCatalogEntry>>(v) {
                Ok(agents) => app.open_wizard(agents),
                Err(e) => app.logs.push_back(format!("agents.list: {e}")),
            },
            None => app
                .logs
                .push_back(format!("agents.list error: {:?}", resp.error)),
        },
        Err(e) => app.logs.push_back(format!("agents.list failed: {e}")),
    }
}

/// Fetch `agents.list` and open the Ctrl+O session picker, flattening every
/// configured agent's live and retained sessions.
async fn fetch_sessions(client: &Client, app: &mut App) {
    match client
        .request(method::AGENTS_LIST, serde_json::json!({}))
        .await
    {
        Ok(resp) => match resp.result {
            Some(v) => match serde_json::from_value::<Vec<AgentCatalogEntry>>(v) {
                Ok(agents) => {
                    let sessions = agents
                        .into_iter()
                        .flat_map(|agent| agent.sessions)
                        .collect();
                    app.open_sessions(sessions);
                }
                Err(e) => agent_request_failed(app, "agents.list", &e.to_string()),
            },
            None => {
                let message = resp
                    .error
                    .as_ref()
                    .map(|e| e.message.clone())
                    .unwrap_or_else(|| "agents.list failed".to_string());
                agent_request_failed(app, "agents.list", &message);
            }
        },
        Err(e) => agent_request_failed(app, "agents.list", &e.to_string()),
    }
}

/// Attach the Agent panel to an existing session by id (`agents.attach`).
async fn attach_session(client: &Client, app: &mut App, session_id: String) {
    match client
        .request(
            method::AGENTS_ATTACH,
            serde_json::json!({ "session_id": session_id }),
        )
        .await
    {
        Ok(resp) => apply_agent_response(app, resp),
        Err(e) => agent_request_failed(app, "agents.attach", &e.to_string()),
    }
}

/// Fetch the configured provider/model catalog for the wizard's selected agent.
async fn load_wizard_providers(client: &Client, app: &mut App) {
    let Some(agent) = app.wizard_agent() else {
        return;
    };
    match client
        .request(
            method::PROVIDERS_LIST,
            serde_json::json!({ "agent": agent }),
        )
        .await
    {
        Ok(resp) => match resp.result {
            Some(v) => {
                let providers = v
                    .get("providers")
                    .and_then(|p| serde_json::from_value::<Vec<Provider>>(p.clone()).ok())
                    .unwrap_or_default();
                app.wizard_set_providers(providers);
            }
            None => app.wizard_error(format!("{:?}", resp.error)),
        },
        Err(e) => app.wizard_error(format!("{e}")),
    }
}

/// Start a one-shot interactive task and open its agent session.
async fn start_oneshot(
    client: &Client,
    app: &mut App,
    agent: String,
    provider: Option<String>,
    model: Option<String>,
    cwd: Option<String>,
) {
    let (rows, cols) = app.term.size();
    let params = serde_json::json!({
        "agent": agent,
        "provider": provider,
        "model": model,
        "cwd": cwd,
        "rows": rows,
        "cols": cols,
    });
    match client.request(method::TASKS_START_ONESHOT, params).await {
        Ok(resp) => apply_agent_response(app, resp),
        Err(e) => app.logs.push_back(format!("start one-shot failed: {e}")),
    }
    refresh_lists(client, app).await;
}

/// Forward raw bytes to the active agent session's PTY.
async fn send_agent_input(client: &Client, app: &mut App, bytes: &[u8]) {
    let Some(sid) = app.agent_session_id.clone() else {
        return;
    };
    let data = base64::engine::general_purpose::STANDARD.encode(bytes);
    if let Err(e) = client
        .request(
            method::AGENTS_INPUT,
            serde_json::json!({ "session_id": sid, "data": data }),
        )
        .await
    {
        app.logs.push_back(format!("agent.input failed: {e}"));
    }
}

/// What [`disconnected_pump`] decided to do next.
enum PumpOutcome {
    /// The backoff elapsed; retry the connection.
    Retry,
    /// An input event needs the daemon; retry now instead of waiting it out.
    Reconnect,
    /// The user asked to quit.
    Quit,
}

/// What a single input event means while disconnected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisconnectedAction {
    /// The event was handled locally (or is irrelevant); keep waiting.
    Continue,
    /// The event needs the daemon; reconnect at once.
    Reconnect,
    /// The user asked to quit.
    Quit,
}

/// Apply one input event while disconnected.
///
/// Keeps the TUI interactive offline: the normal global bindings run against the
/// last known snapshot, `q`/`Esc` set [`App::should_quit`], and anything that
/// would need a round-trip is reported so the caller can reconnect immediately
/// rather than drop the key into the backoff sleep.
fn handle_disconnected_event(app: &mut App, event: CEvent) -> DisconnectedAction {
    let action = match event {
        CEvent::Key(k) if k.kind == KeyEventKind::Press => app.handle_key(k),
        CEvent::Mouse(m) => app.handle_mouse(m),
        _ => return DisconnectedAction::Continue,
    };
    match action {
        UiAction::None => DisconnectedAction::Continue,
        UiAction::Quit => {
            app.should_quit = true;
            DisconnectedAction::Quit
        }
        _ => DisconnectedAction::Reconnect,
    }
}

/// Wait out the reconnect backoff while keeping the UI responsive.
///
/// Unlike a raw `sleep`, this services input the whole time: it redraws, applies
/// global keys to the last snapshot, and returns early on `q`/`Esc` or on
/// daemon-backed input (which triggers an immediate reconnect). Key events are
/// never left unread in the channel.
async fn disconnected_pump<B: ratatui::backend::Backend>(
    app: &mut App,
    ev_rx: &mut mpsc::UnboundedReceiver<CEvent>,
    terminal: &mut Terminal<B>,
    player: &SoundPlayer,
    backoff: Duration,
) -> PumpOutcome {
    let deadline = tokio::time::sleep(backoff);
    tokio::pin!(deadline);
    let mut anim = tokio::time::interval(Duration::from_millis(80));
    anim.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        if app.should_quit {
            return PumpOutcome::Quit;
        }

        let animate = app.needs_animation();
        let _ = terminal.draw(|f| ui::draw(f, app));
        if animate {
            app.throbber_state.calc_next();
        }

        tokio::select! {
            biased;
            _ = anim.tick(), if animate => {}
            _ = &mut deadline => return PumpOutcome::Retry,
            ev = ev_rx.recv() => match ev {
                None => return PumpOutcome::Quit,
                Some(CEvent::FocusGained) => player.set_focused(true),
                Some(CEvent::FocusLost) => player.set_focused(false),
                Some(ev) => match handle_disconnected_event(app, ev) {
                    DisconnectedAction::Continue => {}
                    DisconnectedAction::Reconnect => return PumpOutcome::Reconnect,
                    DisconnectedAction::Quit => return PumpOutcome::Quit,
                },
            },
        }

        // Keep the sound worker's mute/focus state in sync while offline.
        if player.muted() != app.sound_muted {
            player.set_muted(app.sound_muted);
        }
        if app.sound_muted {
            app.take_sound_cues();
        } else {
            for cue in app.take_sound_cues() {
                player.play(cue);
            }
        }
    }
}

async fn resolve_transport(args: &TuiArgs) -> Transport {
    // Pairing: exchange a short-lived code for the daemon's bearer token.
    let pair_token = match (&args.remote, &args.pair_code) {
        (Some(remote), Some(code)) => exchange_pair_code(remote, code).await.ok(),
        _ => None,
    };

    if let Some(remote) = &args.remote {
        return Transport::Ws {
            url: remote.clone(),
            token: pair_token.or_else(|| read_token(args)),
        };
    }
    if let Ok(url) = std::env::var("FAVETTO_URL") {
        if !url.is_empty() {
            return Transport::Ws {
                url,
                token: read_token(args),
            };
        }
    }
    let socket = args
        .socket
        .clone()
        .unwrap_or_else(|| PathBuf::from("/tmp/favetto.sock"));
    Transport::Unix(socket)
}

/// Exchange a pairing code for the daemon's token over the HTTP pairing endpoint.
async fn exchange_pair_code(remote: &str, code: &str) -> anyhow::Result<String> {
    let http_url = remote
        .replace("wss://", "https://")
        .replace("ws://", "http://");
    let resp: serde_json::Value = reqwest::Client::new()
        .post(format!("{}/pair/exchange", http_url.trim_end_matches('/')))
        .json(&serde_json::json!({ "code": code }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    resp.get("token")
        .and_then(|t| t.as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("pair/exchange response missing token"))
}

fn read_token(args: &TuiArgs) -> Option<String> {
    let path = args
        .token_file
        .clone()
        .unwrap_or_else(crate::cli::default_token_path);
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

fn transport_desc(t: &Transport) -> String {
    match t {
        Transport::Unix(p) => format!("unix://{}", p.display()),
        Transport::Ws { url, .. } => url.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::collections::BTreeMap;

    fn key(code: KeyCode) -> CEvent {
        CEvent::Key(KeyEvent::new(code, KeyModifiers::empty()))
    }

    /// A player that never touches the terminal or spawns a worker.
    fn silent_player() -> SoundPlayer {
        SoundPlayer::start(sound::ResolvedSound {
            enabled: false,
            player: sound::PlayerMode::Bell,
            sound_dir: None,
            min_interval: Duration::ZERO,
            only_when_unfocused: false,
            events: BTreeMap::new(),
        })
    }

    fn disconnected_app() -> App {
        let mut app = App::new();
        app.conn = ConnState::Disconnected;
        app
    }

    #[test]
    fn disconnected_tab_keys_stay_live() {
        let mut app = disconnected_app();
        let before = app.tab;

        assert_eq!(
            handle_disconnected_event(&mut app, key(KeyCode::Tab)),
            DisconnectedAction::Continue
        );
        assert_ne!(app.tab, before, "Tab must switch tabs while disconnected");

        assert_eq!(
            handle_disconnected_event(&mut app, key(KeyCode::BackTab)),
            DisconnectedAction::Continue
        );
        assert_eq!(app.tab, before, "Shift+Tab must go back while disconnected");
    }

    #[test]
    fn apply_agent_response_surfaces_errors_in_the_status_bar() {
        let mut app = App::new();
        let resp = favetto_core::rpc::Response::err(1, -32603, "task is already running headless");
        apply_agent_response(&mut app, resp);
        assert_eq!(
            app.agent_error.as_deref(),
            Some("task is already running headless")
        );
    }

    #[test]
    fn disconnected_quit_keys_are_honoured() {
        for code in [KeyCode::Char('q'), KeyCode::Esc] {
            let mut app = disconnected_app();
            assert_eq!(
                handle_disconnected_event(&mut app, key(code)),
                DisconnectedAction::Quit
            );
            assert!(app.should_quit, "{code:?} must request a quit");
        }
    }

    #[test]
    fn disconnected_daemon_backed_keys_request_reconnect() {
        let mut app = disconnected_app();
        // `w` opens the workflow overlay, which is fetched over RPC.
        assert_eq!(
            handle_disconnected_event(&mut app, key(KeyCode::Char('w'))),
            DisconnectedAction::Reconnect
        );
    }

    #[tokio::test]
    async fn disconnected_pump_applies_keys_and_quits_without_waiting_out_backoff() {
        let mut app = disconnected_app();
        let before = app.tab;
        let player = silent_player();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let (tx, mut ev_rx) = mpsc::unbounded_channel();
        tx.send(key(KeyCode::Tab)).unwrap();
        tx.send(key(KeyCode::Char('q'))).unwrap();

        let outcome = tokio::time::timeout(
            Duration::from_millis(1000),
            disconnected_pump(&mut app, &mut ev_rx, &mut terminal, &player, MAX_BACKOFF),
        )
        .await
        .expect("q must not wait out the backoff");

        assert!(matches!(outcome, PumpOutcome::Quit));
        assert_ne!(app.tab, before, "Tab was dropped while disconnected");
    }

    #[tokio::test]
    async fn disconnected_pump_reconnects_on_daemon_backed_input() {
        let mut app = disconnected_app();
        let player = silent_player();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let (tx, mut ev_rx) = mpsc::unbounded_channel();
        tx.send(key(KeyCode::Char('w'))).unwrap();

        let outcome = tokio::time::timeout(
            Duration::from_millis(1000),
            disconnected_pump(&mut app, &mut ev_rx, &mut terminal, &player, MAX_BACKOFF),
        )
        .await
        .expect("daemon-backed input must not wait out the backoff");

        assert!(matches!(outcome, PumpOutcome::Reconnect));
    }
}
