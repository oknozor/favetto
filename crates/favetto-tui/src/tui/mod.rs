//! The `tui` subcommand: a ratatui client that talks to the daemon over the same
//! wire protocol whether attached locally (Unix socket) or remotely (WebSocket).

mod app;
mod clipboard;
mod editor;
mod feed;
mod json;
mod markdown;
mod sanitize;
mod sound;
mod syntax;
mod term;
mod text_buffer;
mod theme;
mod ui;
mod workflow_view;

use std::collections::{HashMap, HashSet};
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
use serde::de::DeserializeOwned;
use tokio::sync::mpsc;

use favetto_core::model::{
    AgentCatalogEntry, AgentCommand, AgentPlanEntry, AgentSessionHistoryEntry, AgentSessionInfo,
    Event, NotificationRecord, ProviderSessionsResult, RepoInfo, Schedule, Task, UsageStats,
};
use favetto_core::rpc::{method, ConfigValidateResult, TasksEventsResult};
use favetto_core::workflow::WorkflowInspect;
use favetto_providers::Provider;
use uuid::Uuid;

use crate::cli::TuiArgs;
use crate::client::{normalize_ws_url, pair_http_base, Client, Transport};
use crate::config::FavettoConfig;
use app::{
    App, CatalogEntry, ConnState, Popup, SessionPickerEntry, SessionPickerState, UiAction, DOCS_URL,
};
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
    let system_mode = theme::Theme::detect_mode();
    let theme_name = match std::env::var("FAVETTO_THEME_NAME") {
        Ok(name) if !name.trim().is_empty() => name.trim().to_string(),
        _ => config.tui.theme.clone(),
    };
    let theme_mode = std::env::var("FAVETTO_THEME")
        .ok()
        .and_then(|value| theme::ThemeMode::from_key(&value))
        .or_else(|| theme::ThemeMode::from_key(&config.tui.theme_mode))
        .unwrap_or(theme::ThemeMode::System);
    let theme_dirs = theme_directories(&config_path);
    let user_themes = theme::Theme::discover_user_themes(&theme_dirs);
    let theme = theme::Theme::resolve(&theme_name, theme_mode, &theme_dirs, system_mode);
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
    app.theme_name = theme_name;
    app.theme_mode = theme_mode;
    app.system_mode = system_mode;
    app.theme_dirs = theme_dirs;
    app.user_themes = user_themes;
    app.show_header = config.tui.show_header;
    app.show_footer = config.tui.show_footer;
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

/// Best-effort RPC fetch: decode the result, logging (and swallowing) any
/// transport, RPC, or decode failure with the method name.
///
/// The initial sync is deliberately tolerant — one failing method must not abort
/// the snapshot — but a partial snapshot used to be indistinguishable from an
/// empty daemon. Logging every failure with its method keeps that visible.
async fn fetch_rpc<T: DeserializeOwned>(
    client: &Client,
    method: &str,
    params: serde_json::Value,
) -> Option<T> {
    let resp = match client.request(method, params).await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::warn!(method, error = %e, "rpc request failed");
            return None;
        }
    };
    let value = match resp.result {
        Some(v) => v,
        None => {
            match resp.error {
                Some(e) => {
                    tracing::warn!(method, code = e.code, message = %e.message, "rpc returned an error")
                }
                None => tracing::warn!(method, "rpc returned no result"),
            }
            return None;
        }
    };
    match serde_json::from_value::<T>(value) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(method, error = %e, "rpc result decode failed");
            None
        }
    }
}

/// Fetch the initial snapshot and subscribe for live pushes.
async fn sync_initial(client: &Client, app: &mut App) {
    if let Some(v) =
        fetch_rpc::<serde_json::Value>(client, method::PING, serde_json::json!({})).await
    {
        app.daemon_cwd = v.get("cwd").and_then(|c| c.as_str()).map(str::to_string);
    }

    // Report a malformed daemon config through a dismissible popup; a valid (or
    // absent) file, or an older daemon without `config.validate`, stays silent.
    if let Some(validation) =
        fetch_rpc::<ConfigValidateResult>(client, method::CONFIG_VALIDATE, serde_json::json!({}))
            .await
    {
        if !validation.valid {
            app.popup = Popup::ConfigError {
                path: validation.path,
                errors: validation.errors,
            };
        }
    }

    if let Some(tasks) =
        fetch_rpc::<Vec<Task>>(client, method::TASKS_LIST, serde_json::json!({})).await
    {
        app.tasks = tasks;
    }

    if let Some(events) = fetch_rpc::<Vec<Event>>(
        client,
        method::EVENTS_TAIL,
        serde_json::json!({ "limit": 100 }),
    )
    .await
    {
        // Replayed history must not fire a burst of stale sounds.
        app.sound_suppressed = true;
        for ev in events {
            app.ingest_event(ev);
        }
        app.sound_suppressed = false;
    }

    fetch_schedules(client, app).await;

    if let Some(notifications) = fetch_rpc::<Vec<NotificationRecord>>(
        client,
        method::NOTIFICATIONS_LIST,
        serde_json::json!({ "limit": 100 }),
    )
    .await
    {
        app.notifications = notifications;
    }

    let _ = fetch_rpc::<serde_json::Value>(
        client,
        method::EVENTS_SUBSCRIBE,
        serde_json::json!({ "last_event_id": app.last_event_id }),
    )
    .await;

    fetch_catalog(client, app).await;
    fetch_repos(client, app).await;
    fetch_agent_sessions(client, app).await;
}

/// Fetch the task catalog.
async fn fetch_catalog(client: &Client, app: &mut App) {
    if let Some(catalog) =
        fetch_rpc::<Vec<CatalogEntry>>(client, method::CATALOG_LIST, serde_json::json!({})).await
    {
        app.set_catalog(catalog);
    }
}

/// Fetch the known-repos registry and the daemon-wide active repo (`repos.list`).
async fn fetch_repos(client: &Client, app: &mut App) {
    if let Some(v) =
        fetch_rpc::<serde_json::Value>(client, method::REPOS_LIST, serde_json::json!({})).await
    {
        let active = v.get("active").and_then(|a| a.as_str()).map(str::to_string);
        let repos = v
            .get("repos")
            .cloned()
            .and_then(|r| serde_json::from_value::<Vec<RepoInfo>>(r).ok())
            .unwrap_or_default();
        app.set_repos(repos, active);
    }
}

/// Fetch the cron schedules (`schedules.list`), including the daemon-computed
/// `next_run` for enabled entries.
async fn fetch_schedules(client: &Client, app: &mut App) {
    if let Some(schedules) =
        fetch_rpc::<Vec<Schedule>>(client, method::SCHEDULES_LIST, serde_json::json!({})).await
    {
        app.schedules = schedules;
    }
}

/// Set the daemon-wide active repo (`repos.activate`) and refresh the client
/// lists so the status badge and catalog grouping update.
async fn activate_repo(client: &Client, app: &mut App, name: Option<String>) {
    let label = name.as_deref().unwrap_or("global");
    match client
        .request(method::REPOS_ACTIVATE, serde_json::json!({ "name": name }))
        .await
    {
        Ok(resp) => match resp.result {
            Some(_) => app.logs.push_back(format!("active repo: {label}")),
            None => app
                .logs
                .push_back(format!("repos.activate error: {:?}", resp.error)),
        },
        Err(e) => app.logs.push_back(format!("repos.activate failed: {e}")),
    }
    refresh_lists(client, app).await;
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

/// Fetch the runtime workflow view for `root_id` (`workflow.inspect`) and fold
/// it into the open runtime inspector. The view is summary-only: no `output`
/// blobs are carried.
async fn fetch_workflow_inspect(client: &Client, app: &mut App, root_id: Uuid) {
    match client
        .request(
            method::WORKFLOW_INSPECT,
            serde_json::json!({ "root_id": root_id }),
        )
        .await
    {
        Ok(resp) => match resp.result {
            Some(v) => match serde_json::from_value::<WorkflowInspect>(v) {
                Ok(view) => app.set_workflow_inspect(view),
                Err(e) => app.set_workflow_inspect_error(format!("workflow.inspect: {e}")),
            },
            None => {
                let message = resp
                    .error
                    .as_ref()
                    .map(|e| e.message.clone())
                    .unwrap_or_else(|| "workflow.inspect failed".to_string());
                app.set_workflow_inspect_error(message);
            }
        },
        Err(e) => app.set_workflow_inspect_error(format!("workflow.inspect failed: {e}")),
    }
}

/// `workflow.cancel` every non-terminal task in `root_id`, then refresh the
/// runtime view so the cancelled instances show immediately.
async fn cancel_root_workflow(client: &Client, app: &mut App, root_id: Uuid) {
    match client
        .request(
            method::WORKFLOW_CANCEL,
            serde_json::json!({ "root_id": root_id }),
        )
        .await
    {
        Ok(resp) => match resp.result {
            Some(_) => app.logs.push_back("workflow.cancel: ok".to_string()),
            None => {
                let message = resp
                    .error
                    .as_ref()
                    .map(|e| e.message.clone())
                    .unwrap_or_else(|| "workflow.cancel failed".to_string());
                app.set_workflow_inspect_error(message);
            }
        },
        Err(e) => app.set_workflow_inspect_error(format!("workflow.cancel failed: {e}")),
    }
    fetch_workflow_inspect(client, app, root_id).await;
}

/// `workflow.retry` one terminal instance, then refresh the list tabs and the
/// runtime view so its new run attempt shows.
async fn retry_instance_workflow(client: &Client, app: &mut App, task_id: Uuid) {
    let root_id = app.workflow_inspect_root();
    match client
        .request(
            method::WORKFLOW_RETRY,
            serde_json::json!({ "task_id": task_id }),
        )
        .await
    {
        Ok(resp) => match resp.result {
            Some(_) => app.logs.push_back("workflow.retry: ok".to_string()),
            None => {
                let message = resp
                    .error
                    .as_ref()
                    .map(|e| e.message.clone())
                    .unwrap_or_else(|| "workflow.retry failed".to_string());
                app.set_workflow_inspect_error(message);
            }
        },
        Err(e) => app.set_workflow_inspect_error(format!("workflow.retry failed: {e}")),
    }
    refresh_lists(client, app).await;
    if let Some(root_id) = root_id {
        fetch_workflow_inspect(client, app, root_id).await;
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

/// Fetch `usage.stats` for the selected period and fold it into the Usage tab.
async fn fetch_usage_stats(client: &Client, app: &mut App) {
    let period = app.usage_period;
    match client
        .request(method::USAGE_STATS, serde_json::json!({ "period": period }))
        .await
    {
        Ok(resp) => match resp.result {
            Some(value) => match serde_json::from_value::<UsageStats>(value) {
                Ok(stats) => app.set_usage_stats(stats),
                Err(e) => app.set_usage_error(format!("usage.stats: {e}")),
            },
            None => {
                let message = resp
                    .error
                    .as_ref()
                    .map(|e| e.message.clone())
                    .unwrap_or_else(|| "usage.stats failed".to_string());
                app.set_usage_error(message);
            }
        },
        Err(e) => app.set_usage_error(format!("usage.stats failed: {e}")),
    }
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
    fetch_schedules(client, app).await;
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
    fetch_repos(client, app).await;
    fetch_agent_sessions(client, app).await;
}

/// Send a `tasks.start` request and refresh the list tabs afterwards.
///
/// Every call is user-initiated (Catalog **Enter** / the variable popup), so the
/// run is marked `interactive`: the executor launches the agent's real TUI and
/// the Agent panel attaches to it live. Programmatic starts (scheduler, hooks,
/// webhooks, `needs`, `spawn`) stay headless.
async fn start_task(client: &Client, app: &mut App, mut params: serde_json::Value) {
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    if let Some(obj) = params.as_object_mut() {
        obj.insert("interactive".to_string(), serde_json::json!(true));
    }
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

/// Copy-on-select: always emit OSC 52 (remote/SSH/tmux fallback), then a
/// best-effort local clipboard tool, and report the result in the status bar.
fn copy_selection_to_clipboard(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    text: String,
) {
    let chars = text.chars().count();
    let _ = clipboard::write_osc52(terminal.backend_mut(), &text);
    let local = clipboard::write_local(&text);
    app.copy_notice = Some(if local {
        format!("copied {chars} chars")
    } else {
        format!("copied {chars} chars (OSC 52)")
    });
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
    let mut agents_refresh = tokio::time::interval(Duration::from_secs(3));
    agents_refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        // The tick is only consumed while something on screen animates; an idle
        // TUI draws solely in response to real events.
        let animate = app.needs_animation();
        let _ = terminal.draw(|f| ui::draw(f, app));

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
                            UiAction::OpenTaskResult(task_id) => {
                                open_task_result(client, app, task_id).await;
                            }
                            UiAction::NewAgent(task_id) => {
                                open_agent(client, app, task_id, true).await;
                            }
                            UiAction::OpenSessions => {
                                fetch_sessions(client, app).await;
                            }
                            UiAction::OpenRepoPicker => {
                                fetch_repos(client, app).await;
                            }
                            UiAction::OpenTaskPicker => {
                                fetch_catalog(client, app).await;
                            }
                            UiAction::OpenDocs => {
                                open_docs(app);
                            }
                            UiAction::ActivateRepo(name) => {
                                activate_repo(client, app, name).await;
                            }
                            UiAction::AttachSession(session_id) => {
                                attach_session(client, app, session_id).await;
                            }
                            UiAction::LoadSession(session_id) => {
                                load_session(client, app, session_id).await;
                            }
                            UiAction::ResumeProviderSession {
                                agent,
                                provider_session_id,
                                cwd,
                            } => {
                                resume_provider_session(
                                    client,
                                    app,
                                    agent,
                                    provider_session_id,
                                    cwd,
                                )
                                .await;
                            }
                            UiAction::DeleteProviderSession {
                                agent,
                                provider_session_id,
                            } => {
                                delete_provider_session(
                                    client,
                                    app,
                                    agent,
                                    provider_session_id,
                                )
                                .await;
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
                            UiAction::OpenWorkflowInspect(root_id) => {
                                fetch_workflow_inspect(client, app, root_id).await;
                            }
                            UiAction::CancelWorkflow(root_id) => {
                                cancel_root_workflow(client, app, root_id).await;
                            }
                            UiAction::RetryWorkflowTask(task_id) => {
                                retry_instance_workflow(client, app, task_id).await;
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
                            UiAction::Reply { session_id, request_id, reply } => {
                                send_agent_reply(client, app, session_id, request_id, reply).await;
                            }
                            UiAction::SetSessionOption {
                                session_id,
                                option_id,
                                value,
                            } => {
                                send_agent_option(client, app, session_id, option_id, value).await;
                            }
                            UiAction::SetSessionMode { session_id, mode_id } => {
                                send_agent_mode(client, app, session_id, mode_id).await;
                            }
                            UiAction::CancelTurn(session_id) => {
                                cancel_agent_turn(client, app, session_id).await;
                            }
                            UiAction::Authenticate { session_id, method_id } => {
                                send_agent_authenticate(client, app, session_id, method_id).await;
                            }
                            UiAction::Logout { session_id } => {
                                send_agent_logout(client, app, session_id).await;
                            }
                            UiAction::TaskCommand { method, params } => {
                                task_command(client, app, method, params).await;
                            }
                            // Copy-on-select is mouse-driven; a key never yields it.
                            UiAction::CopySelection(_) => {}
                            UiAction::None => {}
                        }
                    }
                    Some(CEvent::Mouse(m)) => match app.handle_mouse(m) {
                        UiAction::AgentInput(bytes) => send_agent_input(client, app, &bytes).await,
                        UiAction::CopySelection(text) => {
                            copy_selection_to_clipboard(terminal, app, text)
                        }
                        _ => {}
                    },
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
            _ = agents_refresh.tick() => {
                if app.wants_agent_refresh() {
                    fetch_agent_sessions(client, app).await;
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
        // A repo add/remove/activate also publishes `catalog.updated`; keep the
        // picker and status badge in sync.
        if app.repos_dirty {
            app.repos_dirty = false;
            fetch_repos(client, app).await;
        }
        // A `cron_tick` recomputes the schedule's next run; re-fetch so the
        // countdown resets after each fire.
        if app.schedules_dirty {
            app.schedules_dirty = false;
            fetch_schedules(client, app).await;
        }
        // A `task.updated`/`event` push while the runtime inspector is open
        // re-fetches `workflow.inspect` to keep its table live.
        if app.workflow_inspect_dirty {
            app.workflow_inspect_dirty = false;
            if let Some(root_id) = app.workflow_inspect_root() {
                fetch_workflow_inspect(client, app, root_id).await;
            }
        }
        maybe_load_catalog_preview(client, app).await;
        // The Usage tab fetches on focus, on period change, and when a task push
        // marks it dirty.
        if app.usage_dirty {
            app.usage_dirty = false;
            fetch_usage_stats(client, app).await;
        }
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
        Ok(resp) => adopt_agent_response(client, app, resp).await,
        Err(e) => agent_request_failed(app, "agents.start", &e.to_string()),
    }
}

/// Open an action task's result summary: fetch the durable row with
/// `tasks.get` (its `output.envelope.summary` is dropped from `tasks.list`) and
/// show it in the result popup. No agent session or PTY is started.
async fn open_task_result(client: &Client, app: &mut App, task_id: String) {
    match client
        .request(method::TASKS_GET, serde_json::json!({ "id": task_id }))
        .await
    {
        Ok(resp) => match resp.result {
            Some(v) => match serde_json::from_value::<Task>(v) {
                Ok(task) => app.open_task_result(task),
                Err(e) => agent_request_failed(app, "tasks.get", &e.to_string()),
            },
            None => {
                let message = resp
                    .error
                    .as_ref()
                    .map(|e| e.message.clone())
                    .unwrap_or_else(|| "task not found".to_string());
                agent_request_failed(app, "tasks.get", &message);
            }
        },
        Err(e) => agent_request_failed(app, "tasks.get", &e.to_string()),
    }
}

/// Apply an `agents.start` / `tasks.start_oneshot` response: open the returned
/// session (with its current frame) in the Agent panel.
fn apply_agent_response(
    app: &mut App,
    resp: favetto_core::rpc::Response,
) -> Option<AgentSessionInfo> {
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
            let plan = v
                .get("plan")
                .cloned()
                .and_then(|value| serde_json::from_value::<Vec<AgentPlanEntry>>(value).ok())
                .unwrap_or_default();
            let commands = v
                .get("commands")
                .cloned()
                .and_then(|value| serde_json::from_value::<Vec<AgentCommand>>(value).ok())
                .unwrap_or_default();
            match session {
                Some(session) => {
                    app.open_agent(session.clone(), &frame);
                    app.seed_agent_plan(plan, commands);
                    Some(session)
                }
                None => {
                    app.finish_session_load();
                    agent_request_failed(app, "agent response", "malformed session");
                    None
                }
            }
        }
        None => {
            app.finish_session_load();
            let message = resp
                .error
                .as_ref()
                .map(|e| e.message.clone())
                .unwrap_or_else(|| "agent request failed".to_string());
            agent_request_failed(app, "agent", &message);
            None
        }
    }
}

/// Open an agent response, then replay the adopted session's existing feed.
///
/// `agent.event` pushes are non-durable and an ACP session may have streamed
/// events before the panel opened, so a task-backed session is backfilled from
/// `tasks.events` once. The feed cursor deduplicates this with any live pushes
/// that were queued while the request was in flight.
async fn adopt_agent_response(client: &Client, app: &mut App, resp: favetto_core::rpc::Response) {
    let Some(session) = apply_agent_response(app, resp) else {
        return;
    };
    let Some(task_id) = session.task_id.as_deref() else {
        return;
    };
    if session.backend != favetto_core::model::AgentBackend::Acp {
        return;
    }
    let Ok(task_id) = Uuid::parse_str(task_id) else {
        return;
    };
    let response = match client
        .request(
            method::TASKS_EVENTS,
            serde_json::json!({ "id": task_id, "after": 0, "limit": 2000 }),
        )
        .await
    {
        Ok(response) => response,
        Err(e) => {
            app.logs.push_back(format!("tasks.events failed: {e}"));
            return;
        }
    };
    let Some(value) = response.result else {
        if let Some(error) = response.error {
            app.logs
                .push_back(format!("tasks.events unavailable: {}", error.message));
        }
        return;
    };
    match serde_json::from_value::<TasksEventsResult>(value) {
        Ok(result) => {
            // `tasks.events` resolves the task's most recent session; only fold
            // it when that is the one the panel just adopted.
            if result.session_id.as_deref() == Some(session.id.as_str()) {
                app.apply_agent_backfill(&session.id, &result.events, result.next_cursor);
            }
        }
        Err(e) => app.logs.push_back(format!("tasks.events: {e}")),
    }
}

/// Surface a failed `agents.start` / `agents.attach` / `tasks.get` (or malformed
/// response) in the status bar, and keep a copy in the (unrendered) log ring for
/// debugging.
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

/// Merge live `agents.list` sessions and persisted `agents.history` records into
/// the Ctrl+O picker rows.
///
/// Live/retained sessions come first so they keep the existing in-process
/// `agents.attach` behavior. Persisted rows not currently live are load targets;
/// a row is never synthesized as a fresh session.
fn merge_session_picker(
    agents: Vec<AgentCatalogEntry>,
    history: Vec<AgentSessionHistoryEntry>,
    provider: Vec<ProviderSessionsResult>,
) -> Vec<SessionPickerEntry> {
    let titles: HashMap<String, String> = history
        .iter()
        .filter_map(|entry| entry.title.clone().map(|title| (entry.id.clone(), title)))
        .collect();
    let live: Vec<AgentSessionInfo> = agents.into_iter().flat_map(|a| a.sessions).collect();
    let live_ids: HashSet<String> = live.iter().map(|s| s.id.clone()).collect();
    let live_provider_ids: HashSet<(String, String)> = live
        .iter()
        .filter_map(|session| {
            session
                .session_id
                .clone()
                .map(|provider_id| (session.agent.clone(), provider_id))
        })
        .collect();

    let mut rows: Vec<SessionPickerEntry> = live
        .iter()
        .map(|session| SessionPickerEntry::from_live(session, titles.get(&session.id).cloned()))
        .collect();
    let local_ids: HashSet<String> = live_ids
        .iter()
        .chain(history.iter().map(|entry| &entry.id))
        .cloned()
        .collect();
    rows.extend(
        history
            .iter()
            .filter(|entry| !live_ids.contains(&entry.id))
            .map(SessionPickerEntry::from_history),
    );

    // Provider-owned rows are distinct from Favetto-local records. Rows already
    // imported or attached locally are not duplicated; unsupported/error agents
    // contribute a single explanatory, non-actionable row.
    for result in provider {
        if let Some(error) = &result.error {
            rows.push(SessionPickerEntry::provider_status(&result.agent, error));
        }
        for entry in &result.sessions {
            if entry
                .local_session_id
                .as_ref()
                .is_some_and(|id| local_ids.contains(id))
            {
                continue;
            }
            if live_provider_ids.contains(&(entry.agent.clone(), entry.session_id.clone())) {
                continue;
            }
            rows.push(SessionPickerEntry::from_provider(
                entry,
                result.can_resume,
                result.can_delete,
            ));
        }
    }

    // Active sessions are the most likely target; keep their relative order and
    // put them before past rows. Everything else keeps history's newest-first
    // order, with provider-only rows after Favetto's own records.
    rows.sort_by_key(|row| match row.state {
        SessionPickerState::Active => 0,
        SessionPickerState::Loading => 1,
        _ => 2,
    });
    rows
}

/// Fetch live sessions (`agents.list`) and persisted history (`agents.history`)
/// for the Ctrl+O picker. History is optional: an older daemon without the
/// method still gets a working live-session picker.
async fn fetch_sessions(client: &Client, app: &mut App) {
    let list = client
        .request(method::AGENTS_LIST, serde_json::json!({}))
        .await;
    let history = client
        .request(method::AGENTS_HISTORY, serde_json::json!({ "limit": 100 }))
        .await;

    let agents = match list {
        Ok(resp) => match resp.result {
            Some(v) => match serde_json::from_value::<Vec<AgentCatalogEntry>>(v) {
                Ok(agents) => agents,
                Err(e) => {
                    agent_request_failed(app, "agents.list", &e.to_string());
                    return;
                }
            },
            None => {
                let message = resp
                    .error
                    .as_ref()
                    .map(|e| e.message.clone())
                    .unwrap_or_else(|| "agents.list failed".to_string());
                agent_request_failed(app, "agents.list", &message);
                return;
            }
        },
        Err(e) => {
            agent_request_failed(app, "agents.list", &e.to_string());
            return;
        }
    };

    let history = match history {
        Ok(resp) => match resp.result {
            Some(v) => {
                serde_json::from_value::<Vec<AgentSessionHistoryEntry>>(v).unwrap_or_else(|e| {
                    app.logs.push_back(format!("agents.history: {e}"));
                    Vec::new()
                })
            }
            None => {
                // Older daemons do not know `agents.history`; the live list still
                // works, so this is a log line rather than a blocking error.
                if let Some(error) = resp.error {
                    app.logs
                        .push_back(format!("agents.history unavailable: {}", error.message));
                }
                Vec::new()
            }
        },
        Err(e) => {
            app.logs.push_back(format!("agents.history failed: {e}"));
            Vec::new()
        }
    };

    let provider = fetch_provider_history(client, &agents).await;
    let live_sessions: Vec<AgentSessionInfo> = agents
        .iter()
        .flat_map(|agent| agent.sessions.iter().cloned())
        .collect();
    let sessions = merge_session_picker(agents, history, provider);
    app.set_agent_sessions(live_sessions);
    app.open_sessions(sessions);
}

/// How many provider pages the picker follows for one agent. A provider is free
/// to return a small page; this bounds startup latency without hiding a normal
/// history behind a "load more" interaction.
const PROVIDER_PAGE_LIMIT: usize = 20;

/// Fetch provider-owned sessions for every available ACP agent.
///
/// Each agent's opaque cursor is followed (bounded by [`PROVIDER_PAGE_LIMIT`])
/// and its pages are merged into one result. A failing/unsupported agent yields
/// a structured row with its explanation; the local `agents.history` list is
/// unaffected.
async fn fetch_provider_history(
    client: &Client,
    agents: &[AgentCatalogEntry],
) -> Vec<ProviderSessionsResult> {
    let mut results = Vec::new();
    for agent in agents
        .iter()
        .filter(|agent| agent.available && agent.capabilities.acp)
    {
        let mut merged: Option<ProviderSessionsResult> = None;
        let mut cursor: Option<String> = None;
        let mut seen_cursors: HashSet<String> = HashSet::new();
        for _ in 0..PROVIDER_PAGE_LIMIT {
            let mut params = serde_json::json!({ "agent": agent.name });
            if let Some(cursor) = &cursor {
                params["cursor"] = serde_json::json!(cursor);
            }
            let response = match client
                .request(method::AGENTS_PROVIDER_SESSIONS, params)
                .await
            {
                Ok(response) => response,
                Err(error) => {
                    results.push(ProviderSessionsResult {
                        agent: agent.name.clone(),
                        supported: false,
                        can_resume: false,
                        can_delete: false,
                        sessions: Vec::new(),
                        next_cursor: None,
                        error: Some(format!("agents.provider_sessions failed: {error}")),
                    });
                    merged = None;
                    break;
                }
            };
            let Some(value) = response.result else {
                let message = response
                    .error
                    .map(|error| error.message)
                    .unwrap_or_else(|| "agents.provider_sessions failed".to_string());
                results.push(ProviderSessionsResult {
                    agent: agent.name.clone(),
                    supported: false,
                    can_resume: false,
                    can_delete: false,
                    sessions: Vec::new(),
                    next_cursor: None,
                    error: Some(message),
                });
                merged = None;
                break;
            };
            let pages = match serde_json::from_value::<Vec<ProviderSessionsResult>>(value) {
                Ok(pages) => pages,
                Err(error) => {
                    results.push(ProviderSessionsResult {
                        agent: agent.name.clone(),
                        supported: false,
                        can_resume: false,
                        can_delete: false,
                        sessions: Vec::new(),
                        next_cursor: None,
                        error: Some(format!("agents.provider_sessions: {error}")),
                    });
                    merged = None;
                    break;
                }
            };
            let Some(page) = pages.into_iter().find(|page| page.agent == agent.name) else {
                break;
            };
            let next = page.next_cursor.clone();
            match &mut merged {
                Some(aggregate) => {
                    aggregate.sessions.extend(page.sessions);
                    aggregate.next_cursor = next.clone();
                }
                None => merged = Some(page),
            }
            let Some(next) = next else { break };
            if !seen_cursors.insert(next.clone()) {
                break;
            }
            cursor = Some(next);
        }
        if let Some(result) = merged {
            results.push(result);
        }
    }
    results
}

/// Fetch `agents.list` and cache every live/retained session so the Tasks table
/// and the Ctrl+O picker can show activity/usage. Best-effort: a failure leaves
/// the previous cache in place.
async fn fetch_agent_sessions(client: &Client, app: &mut App) {
    match client
        .request(method::AGENTS_LIST, serde_json::json!({}))
        .await
    {
        Ok(resp) => {
            if let Some(v) = resp.result {
                match serde_json::from_value::<Vec<AgentCatalogEntry>>(v) {
                    Ok(agents) => {
                        let sessions = agents.into_iter().flat_map(|a| a.sessions).collect();
                        app.set_agent_sessions(sessions);
                    }
                    Err(e) => app.logs.push_back(format!("agents.list: {e}")),
                }
            }
        }
        Err(e) => app.logs.push_back(format!("agents.list failed: {e}")),
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
        Ok(resp) => adopt_agent_response(client, app, resp).await,
        Err(e) => agent_request_failed(app, "agents.attach", &e.to_string()),
    }
}

/// Restore a persisted session by its Favetto-local id (`agents.load`).
///
/// The daemon resolves the provider ACP id from its durable record. A failure —
/// including an agent that does not support `session/load` — is reported as an
/// error; the client never falls back to starting a new session.
async fn load_session(client: &Client, app: &mut App, session_id: String) {
    app.begin_session_load(&session_id);
    match client
        .request(
            method::AGENTS_LOAD,
            serde_json::json!({ "session_id": session_id }),
        )
        .await
    {
        Ok(resp) => adopt_agent_response(client, app, resp).await,
        Err(e) => {
            app.finish_session_load();
            agent_request_failed(app, "agents.load", &e.to_string());
        }
    }
}

/// Resume a provider-owned session by its ACP id.
///
/// The daemon imports it into a Favetto-local record and returns the local id;
/// the attached session is then opened like any other restore. An agent that
/// supports neither `session/load` nor `session/resume` reports an error and no
/// fresh session is created.
async fn resume_provider_session(
    client: &Client,
    app: &mut App,
    agent: String,
    provider_session_id: String,
    cwd: Option<PathBuf>,
) {
    app.begin_session_load(&provider_session_id);
    match client
        .request(
            method::AGENTS_PROVIDER_RESUME,
            serde_json::json!({
                "agent": agent,
                "provider_session_id": provider_session_id,
                "cwd": cwd,
            }),
        )
        .await
    {
        Ok(resp) => {
            let _ = apply_agent_response(app, resp);
        }
        Err(e) => {
            app.finish_session_load();
            agent_request_failed(app, "agents.provider_resume", &e.to_string());
        }
    }
}

/// Delete a provider-owned session and re-open the picker with fresh history.
async fn delete_provider_session(
    client: &Client,
    app: &mut App,
    agent: String,
    provider_session_id: String,
) {
    match client
        .request(
            method::AGENTS_PROVIDER_DELETE,
            serde_json::json!({
                "agent": agent,
                "provider_session_id": provider_session_id,
            }),
        )
        .await
    {
        Ok(resp) => {
            if let Some(error) = resp.error {
                app.logs
                    .push_back(format!("agents.provider_delete: {}", error.message));
            } else {
                app.logs
                    .push_back("provider session deleted; local metadata is unchanged".to_string());
            }
        }
        Err(e) => app
            .logs
            .push_back(format!("agents.provider_delete failed: {e}")),
    }
    // The provider list is the source of truth; refresh it instead of removing
    // the row optimistically.
    fetch_sessions(client, app).await;
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
        Ok(resp) => adopt_agent_response(client, app, resp).await,
        Err(e) => app.logs.push_back(format!("start one-shot failed: {e}")),
    }
    refresh_lists(client, app).await;
}

/// Type into the active ACP session's prompt line. Enter sends the composed
/// prompt as `agents.prompt`; Backspace edits it. There is no child TUI, so
/// other control keys are ignored.
async fn send_agent_input(client: &Client, app: &mut App, bytes: &[u8]) {
    let Some(sid) = app.agent_session_id.clone() else {
        return;
    };
    for byte in bytes {
        match byte {
            b'\r' | b'\n' => {
                let prompt = std::mem::take(&mut app.agent_prompt);
                app.reset_agent_command_selection();
                if prompt.trim().is_empty() {
                    continue;
                }
                app.agent_status.clear();
                if let Err(e) = client
                    .request(
                        method::AGENTS_PROMPT,
                        serde_json::json!({ "session_id": sid, "prompt": prompt }),
                    )
                    .await
                {
                    app.logs.push_back(format!("agents.prompt failed: {e}"));
                }
            }
            0x7f | 0x08 => {
                app.agent_prompt.pop();
                app.reset_agent_command_selection();
            }
            b if b.is_ascii_graphic() || *b == b' ' => {
                app.agent_prompt.push(*b as char);
                app.reset_agent_command_selection();
            }
            _ => {}
        }
    }
}

/// Answer a session's structured input request through `agents.reply`, then
/// re-sync the cached sessions so the resolved prompt disappears promptly.
async fn send_agent_reply(
    client: &Client,
    app: &mut App,
    session_id: String,
    request_id: String,
    reply: favetto_core::model::InputReply,
) {
    let params = serde_json::json!({
        "session_id": session_id,
        "request_id": request_id,
        "reply": reply,
    });
    match client.request(method::AGENTS_REPLY, params).await {
        Ok(resp) => match resp.result {
            Some(_) => app.logs.push_back("agents.reply: ok".to_string()),
            None => {
                let message = resp
                    .error
                    .as_ref()
                    .map(|e| e.message.clone())
                    .unwrap_or_else(|| "agents.reply failed".to_string());
                app.agent_error = Some(message);
            }
        },
        Err(e) => app.agent_error = Some(e.to_string()),
    }
    fetch_agent_sessions(client, app).await;
}

/// Change one advertised session config option (`agents.set_option`) and adopt
/// the complete reconciled snapshot the daemon returns.
async fn send_agent_option(
    client: &Client,
    app: &mut App,
    session_id: String,
    option_id: String,
    value: favetto_core::model::SessionConfigValue,
) {
    let params = serde_json::json!({
        "session_id": session_id,
        "option_id": option_id,
        "value": value,
    });
    apply_session_options_response(
        client,
        app,
        method::AGENTS_SET_OPTION,
        "agents.set_option",
        params,
    )
    .await;
}

/// Switch the attached ACP session to an advertised mode (`agents.set_mode`).
async fn send_agent_mode(client: &Client, app: &mut App, session_id: String, mode_id: String) {
    let params = serde_json::json!({ "session_id": session_id, "mode_id": mode_id });
    apply_session_options_response(
        client,
        app,
        method::AGENTS_SET_MODE,
        "agents.set_mode",
        params,
    )
    .await;
}

async fn apply_session_options_response(
    client: &Client,
    app: &mut App,
    method: &'static str,
    label: &str,
    params: serde_json::Value,
) {
    match client.request(method, params).await {
        Ok(resp) => match resp.result {
            Some(value) => {
                match serde_json::from_value::<favetto_core::rpc::AgentOptionsResult>(value) {
                    Ok(snapshot) => {
                        app.seed_agent_options(snapshot.config_options, snapshot.modes);
                        app.agent_error = None;
                        app.logs.push_back(format!("{label}: ok"));
                    }
                    Err(error) => {
                        app.agent_error = Some(format!("{label}: malformed response: {error}"));
                    }
                }
            }
            None => {
                let message = resp
                    .error
                    .as_ref()
                    .map(|error| error.message.clone())
                    .unwrap_or_else(|| format!("{label} failed"));
                app.agent_error = Some(message);
            }
        },
        Err(error) => app.agent_error = Some(error.to_string()),
    }
    fetch_agent_sessions(client, app).await;
}

/// Complete an agent-handled ACP sign-in for a session.
///
/// The daemon performs the flow; the TUI only chooses an advertised method id.
/// No credential is collected, sent, or displayed here.
async fn send_agent_authenticate(
    client: &Client,
    app: &mut App,
    session_id: String,
    method_id: String,
) {
    let params = serde_json::json!({
        "session_id": session_id,
        "method_id": method_id,
    });
    match client.request(method::AGENTS_AUTHENTICATE, params).await {
        Ok(resp) => match resp.result {
            Some(_) => {
                app.agent_error = None;
                app.logs.push_back("agents.authenticate: ok".to_string());
            }
            None => {
                let message = resp
                    .error
                    .as_ref()
                    .map(|error| error.message.clone())
                    .unwrap_or_else(|| "agents.authenticate failed".to_string());
                app.agent_error = Some(message);
            }
        },
        Err(error) => app.agent_error = Some(error.to_string()),
    }
    fetch_agent_sessions(client, app).await;
}

/// Interrupt the attached ACP session's in-flight turn (`agents.cancel`).
///
/// This is distinct from `tasks.cancel`: the task stays alive and a follow-up
/// prompt can be sent once the turn settles. The daemon pushes the resulting
/// `Idle { Interrupted }` event, which the feed renders as "Turn interrupted".
async fn cancel_agent_turn(client: &Client, app: &mut App, session_id: String) {
    match client
        .request(
            method::AGENTS_CANCEL,
            serde_json::json!({ "session_id": session_id }),
        )
        .await
    {
        Ok(resp) => match resp.result {
            Some(_) => {
                app.agent_status = "cancelling turn…".to_string();
                app.logs.push_back("agents.cancel: ok".to_string());
            }
            None => {
                let message = resp
                    .error
                    .as_ref()
                    .map(|e| e.message.clone())
                    .unwrap_or_else(|| "agents.cancel failed".to_string());
                app.agent_error = Some(message);
            }
        },
        Err(e) => {
            app.agent_error = Some(e.to_string());
            app.logs.push_back(format!("agents.cancel failed: {e}"));
        }
    }
}

/// Sign a session out through ACP `logout` when the agent advertised it.
async fn send_agent_logout(client: &Client, app: &mut App, session_id: String) {
    let params = serde_json::json!({ "session_id": session_id });
    match client.request(method::AGENTS_LOGOUT, params).await {
        Ok(resp) => match resp.result {
            Some(_) => {
                app.agent_error = None;
                app.logs.push_back("agents.logout: ok".to_string());
            }
            None => {
                let message = resp
                    .error
                    .as_ref()
                    .map(|error| error.message.clone())
                    .unwrap_or_else(|| "agents.logout failed".to_string());
                app.agent_error = Some(message);
            }
        },
        Err(error) => app.agent_error = Some(error.to_string()),
    }
    fetch_agent_sessions(client, app).await;
}

/// Apply a task command (`tasks.cancel`, `tasks.retry`, `workflow.cancel`).
///
/// The daemon pushes `task.updated` (and `task.cancelled` events) for every
/// change, so a success never mutates a row here; it only re-syncs the list as a
/// safety net. A failure is surfaced in the status bar, where the TUI also shows
/// connection and agent errors, instead of only the internal log ring.
async fn task_command(client: &Client, app: &mut App, method: &str, params: serde_json::Value) {
    match client.request(method, params).await {
        Ok(resp) => match resp.result {
            Some(_) => {
                app.notice = None;
                refresh_lists(client, app).await;
            }
            None => {
                let message = resp
                    .error
                    .as_ref()
                    .map(|e| e.message.clone())
                    .unwrap_or_else(|| format!("{method} failed"));
                app.notice = Some(format!("{method}: {message}"));
            }
        },
        Err(e) => app.notice = Some(format!("{method} failed: {e}")),
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
        // A copy is served locally; it never needs a reconnect (and the offline
        // pump has no terminal handle to emit OSC 52 on, so it is skipped).
        UiAction::CopySelection(_) => DisconnectedAction::Continue,
        // The docs site is opened locally, so no reconnect is needed either.
        UiAction::OpenDocs => {
            open_docs(app);
            DisconnectedAction::Continue
        }
        _ => DisconnectedAction::Reconnect,
    }
}

/// Open the favetto documentation site in the user's browser, reporting a launch
/// failure in the status bar. A local action: it never needs the daemon.
fn open_docs(app: &mut App) {
    if let Err(e) = webbrowser::open(DOCS_URL) {
        app.notice = Some(format!("could not open docs: {e}"));
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
    // Remote credential resolution, in precedence order: an explicit pairing
    // code, then a username/password login, then the token file.
    let remote_token = match &args.remote {
        Some(remote) => {
            if let Some(code) = &args.pair_code {
                exchange_pair_code(remote, code).await.ok()
            } else if let Some(user) = &args.user {
                login(remote, user, args.password.as_deref()).await.ok()
            } else {
                None
            }
        }
        None => None,
    };

    if let Some(remote) = &args.remote {
        return Transport::Ws {
            url: normalize_ws_url(remote),
            token: remote_token.or_else(|| read_token(args)),
        };
    }
    if let Ok(url) = std::env::var("FAVETTO_URL") {
        if !url.is_empty() {
            return Transport::Ws {
                url: normalize_ws_url(&url),
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
///
/// The pairing endpoint is derived from the remote WebSocket URL by
/// [`pair_http_base`], so both a base URL and an explicit `/rpc` URL reach the
/// daemon's HTTP router.
async fn exchange_pair_code(remote: &str, code: &str) -> anyhow::Result<String> {
    let http_url = pair_http_base(remote);
    let resp: serde_json::Value = reqwest::Client::new()
        .post(format!("{http_url}/pair/exchange"))
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

/// Log in with a username/password and return the daemon's bearer token.
///
/// The login endpoint is derived from the remote WebSocket URL by
/// [`pair_http_base`]. When `password` is `None`, the user is prompted on stderr.
async fn login(remote: &str, user: &str, password: Option<&str>) -> anyhow::Result<String> {
    let password = match password {
        Some(password) => password.to_string(),
        None => prompt_password(user)?,
    };
    let http_url = pair_http_base(remote);
    let resp: serde_json::Value = reqwest::Client::new()
        .post(format!("{http_url}/auth/login"))
        .json(&login_body(user, &password))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    resp.get("token")
        .and_then(|t| t.as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("auth/login response missing token"))
}

/// The JSON body of `POST /auth/login` (pure, so it can be unit-tested).
fn login_body(username: &str, password: &str) -> serde_json::Value {
    serde_json::json!({ "username": username, "password": password })
}

/// Prompt for a password on stderr, reading one line from stdin.
fn prompt_password(user: &str) -> anyhow::Result<String> {
    use std::io::{BufRead, Write};

    eprint!("Password for {user}: ");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line)? == 0 {
        anyhow::bail!("no password provided on stdin; pass --password");
    }
    Ok(line.trim_end_matches(['\n', '\r']).to_string())
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

/// Directories searched for user theme files, in order: the config-local
/// `themes/`, the XDG/`FAVETTO_THEME_DIR` themes dir, then the repo-local
/// `.favetto/themes/`.
fn theme_directories(config_path: &std::path::Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(parent) = config_path.parent() {
        let local = parent.join("themes");
        if !dirs.contains(&local) {
            dirs.push(local);
        }
    }
    let default = favetto_core::paths::default_themes_dir();
    if !dirs.contains(&default) {
        dirs.push(default);
    }
    if let Ok(cwd) = std::env::current_dir() {
        let repo = cwd.join(".favetto").join("themes");
        if !dirs.contains(&repo) {
            dirs.push(repo);
        }
    }
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use std::collections::BTreeMap;

    fn key(code: KeyCode) -> CEvent {
        CEvent::Key(KeyEvent::new(code, KeyModifiers::empty()))
    }

    fn mouse(kind: MouseEventKind, col: u16, row: u16) -> CEvent {
        CEvent::Mouse(MouseEvent {
            kind,
            column: col,
            row,
            modifiers: KeyModifiers::empty(),
        })
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
        let resp = favetto_core::rpc::Response::error(
            1,
            favetto_core::rpc::RpcError::Internal("task is already running headless".to_string()),
        );
        assert!(apply_agent_response(&mut app, resp).is_none());
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
    fn disconnected_mouse_drag_does_not_reconnect() {
        let mut app = disconnected_app();
        app.tab = app::Tab::Agent;
        app.agent_session_id = Some("s".to_string());
        app.agent_running = true;
        app.agent_area = Some(ratatui::layout::Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 24,
        });
        app.term.process(b"hello");

        // Down → Drag → Up drives a full selection; the release yields
        // `CopySelection`, which must stay local (no reconnect).
        for ev in [
            mouse(MouseEventKind::Down(MouseButton::Left), 0, 0),
            mouse(MouseEventKind::Drag(MouseButton::Left), 4, 0),
            mouse(MouseEventKind::Up(MouseButton::Left), 4, 0),
        ] {
            assert_eq!(
                handle_disconnected_event(&mut app, ev),
                DisconnectedAction::Continue
            );
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

    #[test]
    fn session_picker_merge_prefers_live_attach_and_lists_past_loads() {
        let live = AgentSessionInfo {
            id: "local-live".to_string(),
            agent: "opencode".to_string(),
            backend: favetto_core::model::AgentBackend::Acp,
            task_id: None,
            running: true,
            headless: false,
            session_id: None,
            awaiting_input: None,
            activity: None,
            usage: None,
            load_state: Default::default(),
            load_error: None,

            config_options: Vec::new(),
            modes: None,
            auth: None,
        };
        let agents = vec![AgentCatalogEntry {
            name: "opencode".to_string(),
            display_name: "OpenCode".to_string(),
            command: "opencode".to_string(),
            default: true,
            available: true,
            capabilities: Default::default(),
            sessions: vec![live],
        }];
        let now = chrono::Utc::now();
        let past = AgentSessionHistoryEntry {
            id: "local-past".to_string(),
            agent: "opencode".to_string(),
            title: Some("Fix the widget".to_string()),
            cwd: PathBuf::from("/work"),
            task_id: None,
            state: favetto_core::model::AgentSessionState::Ended,
            loadable: true,
            load_error: None,
            created_at: now,
            updated_at: now,
        };
        let mut duplicate = past.clone();
        duplicate.id = "local-live".to_string();
        duplicate.title = Some("Live title".to_string());
        duplicate.state = favetto_core::model::AgentSessionState::Active;

        let rows = merge_session_picker(agents, vec![past, duplicate], Vec::new());
        assert_eq!(rows.len(), 2, "a live session is not listed twice");
        assert_eq!(rows[0].id, "local-live");
        assert_eq!(rows[0].title.as_deref(), Some("Live title"));
        assert_eq!(rows[0].action, app::SessionPickerAction::Attach);
        assert_eq!(rows[1].id, "local-past");
        assert_eq!(rows[1].state, SessionPickerState::Ended);
        assert_eq!(rows[1].action, app::SessionPickerAction::Load);
        assert_eq!(rows[1].cwd.as_deref(), Some(std::path::Path::new("/work")));
    }

    fn provider_entry(agent: &str, session_id: &str) -> favetto_core::model::ProviderSessionEntry {
        favetto_core::model::ProviderSessionEntry {
            agent: agent.to_string(),
            session_id: session_id.to_string(),
            cwd: PathBuf::from("/work"),
            title: Some(format!("{agent} provider")),
            updated_at: None,
            local_session_id: None,
        }
    }

    #[test]
    fn provider_rows_are_capability_gated_and_scoped_per_agent() {
        let result = |agent: &str, can_resume, can_delete, sessions| {
            favetto_core::model::ProviderSessionsResult {
                agent: agent.to_string(),
                supported: true,
                can_resume,
                can_delete,
                sessions,
                next_cursor: None,
                error: None,
            }
        };
        let rows = merge_session_picker(
            Vec::new(),
            Vec::new(),
            vec![
                result(
                    "opencode",
                    true,
                    true,
                    vec![provider_entry("opencode", "session-1")],
                ),
                result("pi", false, false, vec![provider_entry("pi", "session-1")]),
            ],
        );
        assert_eq!(rows.len(), 2, "colliding provider ids stay separate");
        let opencode = rows.iter().find(|row| row.agent == "opencode").unwrap();
        assert_eq!(opencode.state, SessionPickerState::Provider);
        assert_eq!(opencode.action, app::SessionPickerAction::ResumeProvider);
        assert!(opencode.deletable);
        assert_eq!(opencode.provider_session_id.as_deref(), Some("session-1"));
        let pi = rows.iter().find(|row| row.agent == "pi").unwrap();
        assert_eq!(pi.action, app::SessionPickerAction::None);
        assert!(!pi.deletable);
        assert!(pi.blocked_reason().is_some());
    }

    #[test]
    fn provider_rows_already_imported_or_unsupported_are_not_duplicated() {
        let now = chrono::Utc::now();
        let local = AgentSessionHistoryEntry {
            id: "local-1".to_string(),
            agent: "opencode".to_string(),
            title: Some("Imported".to_string()),
            cwd: PathBuf::from("/work"),
            task_id: None,
            state: favetto_core::model::AgentSessionState::Ended,
            loadable: true,
            load_error: None,
            created_at: now,
            updated_at: now,
        };
        let mut imported = provider_entry("opencode", "provider-1");
        imported.local_session_id = Some("local-1".to_string());
        let rows = merge_session_picker(
            Vec::new(),
            vec![local],
            vec![
                favetto_core::model::ProviderSessionsResult {
                    agent: "opencode".to_string(),
                    supported: true,
                    can_resume: true,
                    can_delete: true,
                    sessions: vec![imported],
                    next_cursor: None,
                    error: None,
                },
                favetto_core::model::ProviderSessionsResult {
                    agent: "pi".to_string(),
                    supported: false,
                    can_resume: false,
                    can_delete: false,
                    sessions: Vec::new(),
                    next_cursor: None,
                    error: Some("session/list is not advertised".to_string()),
                },
            ],
        );
        assert_eq!(
            rows.iter().filter(|row| row.id == "local-1").count(),
            1,
            "the already-imported provider row is not duplicated"
        );
        assert!(
            rows.iter()
                .any(|row| row.state == SessionPickerState::Unsupported
                    && row.detail.as_deref() == Some("session/list is not advertised")),
            "unsupported provider history is explained inline"
        );
    }

    #[test]
    fn login_body_carries_the_credentials() {
        let body = login_body("alice", "s3cret");
        assert_eq!(
            body,
            serde_json::json!({ "username": "alice", "password": "s3cret" })
        );
    }
}
