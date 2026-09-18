//! The `tui` subcommand: a ratatui client that talks to the daemon over the same
//! wire protocol whether attached locally (Unix socket) or remotely (WebSocket).

mod app;
mod ui;

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event as CEvent, KeyCode, KeyEventKind};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::Terminal;
use tokio::sync::mpsc;

use favetto_core::model::{ChatSession, Event, NotificationRecord, Schedule, Task};
use favetto_core::rpc::method;

use crate::cli::TuiArgs;
use crate::client::{Client, Transport};
use app::{App, ConnState, UiAction};

/// Outcome of a connected session: the user quit, or the link dropped.
enum SessionOutcome {
    Quit,
    Disconnected,
}

pub async fn run(args: TuiArgs) -> anyhow::Result<()> {
    let transport = resolve_transport(&args).await;

    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
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
    let mut backoff = Duration::from_millis(250);
    let result = loop {
        app.conn = ConnState::Connecting;
        app.conn_detail = transport_desc(&transport);

        match Client::connect(transport.clone()).await {
            Ok(client) => {
                app.conn = ConnState::Connected;
                sync_initial(&client, &mut app).await;
                match run_session(&client, &mut app, &mut ev_rx, &mut terminal).await {
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

        let _ = terminal.draw(|f| ui::draw(f, &app));
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(10));
    };

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

    result
}

/// Fetch the initial snapshot and subscribe for live pushes.
async fn sync_initial(client: &Client, app: &mut App) {
    if let Ok(resp) = client.request(method::TASKS_LIST, serde_json::json!({})).await {
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
                for ev in events {
                    app.ingest_event(ev);
                }
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
}

/// The connected interaction loop: redraw, handle keys, apply pushes, ping.
async fn run_session(
    client: &Client,
    app: &mut App,
    ev_rx: &mut mpsc::UnboundedReceiver<CEvent>,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> SessionOutcome {
    let mut push_rx = client.subscribe();
    let mut ping = tokio::time::interval(Duration::from_secs(5));

    loop {
        let _ = terminal.draw(|f| ui::draw(f, app));

        tokio::select! {
            biased;
            ev = ev_rx.recv() => {
                match ev {
                    Some(CEvent::Key(k)) if k.kind == KeyEventKind::Press => {
                        match app.handle_key(k.code) {
                            UiAction::Quit => return SessionOutcome::Quit,
                            UiAction::OpenChat(task_id) => {
                                match client.request(method::CHAT_OPEN, serde_json::json!({ "task_id": task_id })).await {
                                    Ok(resp) => {
                                        if let Some(v) = resp.result {
                                            match serde_json::from_value::<ChatSession>(v) {
                                                Ok(session) => app.open_chat(session),
                                                Err(e) => app.logs.push_back(format!("chat.open: {e}")),
                                            }
                                        }
                                    }
                                    Err(e) => app.logs.push_back(format!("chat.open failed: {e}")),
                                }
                            }
                            UiAction::SendChat => {
                                let Some(session_id) = app.chat_session_id.clone() else {
                                    app.logs.push_back("no chat session".to_string());
                                    continue;
                                };
                                let message = std::mem::take(&mut app.input);
                                if message.trim().is_empty() {
                                    continue;
                                }
                                match client.request(method::CHAT_SEND, serde_json::json!({ "session_id": session_id, "message": message })).await {
                                    Ok(resp) => {
                                        if let Some(v) = resp.result {
                                            match serde_json::from_value::<ChatSession>(v) {
                                                Ok(session) => app.update_chat(session),
                                                Err(e) => app.logs.push_back(format!("chat.send: {e}")),
                                            }
                                        }
                                    }
                                    Err(e) => app.logs.push_back(format!("chat.send failed: {e}")),
                                }
                            }
                            UiAction::None => {}
                        }
                    }
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
            _ = ping.tick() => {
                if client.request(method::PING, serde_json::json!({})).await.is_err() {
                    return SessionOutcome::Disconnected;
                }
            }
        }
    }
}

/// Non-blocking check for a quit key while disconnected (between reconnect attempts).
fn drain_quit(ev_rx: &mut mpsc::UnboundedReceiver<CEvent>, app: &mut App) {
    while let Ok(ev) = ev_rx.try_recv() {
        if let CEvent::Key(k) = ev {
            if k.kind == KeyEventKind::Press && matches!(k.code, KeyCode::Char('q') | KeyCode::Esc) {
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
