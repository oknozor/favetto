//! The `tui` subcommand: a ratatui client that talks to the daemon over the same
//! wire protocol whether attached locally (Unix socket) or remotely (WebSocket).

mod app;
mod json;
mod markdown;
mod sound;
mod term;
mod ui;

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use base64::Engine as _;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    self, DisableFocusChange, DisableMouseCapture, EnableFocusChange, EnableMouseCapture,
    Event as CEvent, KeyCode, KeyEventKind,
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
use app::{App, CatalogEntry, ConnState, UiAction};
use sound::{CliSound, SoundPlayer};

/// Outcome of a connected session: the user quit, or the link dropped.
enum SessionOutcome {
    Quit,
    Disconnected,
}

pub async fn run(args: TuiArgs) -> anyhow::Result<()> {
    // Client-local config: CLI --config, else ~/.config/favetto/config.toml. The
    // daemon parses the same file and ignores the [tui] section.
    let config_path = args
        .config
        .clone()
        .unwrap_or_else(crate::cli::default_config_path);
    let config = FavettoConfig::load_from(&config_path).unwrap_or_default();

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
    // pushes, and pings.
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<CEvent>();
    std::thread::spawn(move || loop {
        if event::poll(Duration::from_millis(50)).unwrap_or(false) {
            if let Ok(ev) = event::read() {
                if ev_tx.send(ev).is_err() {
                    break;
                }
            }
        }
    });

    let mut app = App::new();
    app.sound_enabled = sound_cfg.enabled;
    let mut backoff = Duration::from_millis(250);
    let result = loop {
        app.conn = ConnState::Connecting;
        app.conn_detail = transport_desc(&transport);

        match Client::connect(transport.clone()).await {
            Ok(client) => {
                app.conn = ConnState::Connected;
                sync_initial(&client, &mut app).await;
                match run_session(&client, &mut app, &mut ev_rx, &mut terminal, &player).await {
                    SessionOutcome::Quit => break Ok(()),
                    SessionOutcome::Disconnected => {
                        app.conn = ConnState::Disconnected;
                        app.conn_detail = "reconnecting…".to_string();
                    }
                }
                backoff = Duration::from_millis(250);
            }
            Err(e) => {
                app.conn = ConnState::Disconnected;
                app.conn_detail = e.to_string();
            }
        }

        // Allow quitting while disconnected.
        drain_quit(&mut ev_rx, &mut app);
        if app.should_quit {
            break Ok(());
        }

        let _ = terminal.draw(|f| ui::draw(f, &mut app));
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(10));
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

/// The connected interaction loop: redraw, handle keys, apply pushes, ping.
async fn run_session(
    client: &Client,
    app: &mut App,
    ev_rx: &mut mpsc::UnboundedReceiver<CEvent>,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    player: &SoundPlayer,
) -> SessionOutcome {
    let mut push_rx = client.subscribe();
    let mut closed_rx = client.closed();
    let mut ping = tokio::time::interval(Duration::from_secs(5));

    loop {
        app.throbber_state.calc_next();
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
                            UiAction::OpenWizard => {
                                fetch_agents(client, app).await;
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
        Err(e) => app.logs.push_back(format!("agents.start failed: {e}")),
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
                None => app
                    .logs
                    .push_back("agent response: malformed session".to_string()),
            }
        }
        None => app.logs.push_back(format!("agent error: {:?}", resp.error)),
    }
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

/// Non-blocking check for a quit key while disconnected (between reconnect attempts).
fn drain_quit(ev_rx: &mut mpsc::UnboundedReceiver<CEvent>, app: &mut App) {
    while let Ok(ev) = ev_rx.try_recv() {
        if let CEvent::Key(k) = ev {
            if k.kind == KeyEventKind::Press && matches!(k.code, KeyCode::Char('q') | KeyCode::Esc)
            {
                app.should_quit = true;
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
