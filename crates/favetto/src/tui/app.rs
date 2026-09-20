//! TUI application state and the logic for folding server pushes into it.

use std::collections::VecDeque;

use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use serde_json::Value;

use base64::Engine as _;

use favetto_core::model::{AgentSessionInfo, Event, NotificationRecord, Schedule, Task};
use favetto_core::rpc::{method, push, Notification};

use super::term::TerminalView;

/// Tabs shown in the header. Agent hosts the embedded external-agent terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Tasks,
    Catalog,
    Agent,
    Events,
    Scheduler,
    Notifications,
}

impl Tab {
    pub const ALL: [Tab; 6] = [
        Tab::Tasks,
        Tab::Catalog,
        Tab::Agent,
        Tab::Events,
        Tab::Scheduler,
        Tab::Notifications,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Tab::Tasks => "Tasks",
            Tab::Catalog => "Catalog",
            Tab::Agent => "Agent",
            Tab::Events => "Events",
            Tab::Scheduler => "Scheduler",
            Tab::Notifications => "Notifications",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnState {
    Connecting,
    Connected,
    Disconnected,
}

/// An action produced by [`App::handle_key`] for the session loop to execute.
pub enum UiAction {
    None,
    Quit,
    /// Start (and focus) an embedded agent session for a task id.
    OpenAgent(String),
    /// Start a brand-new agent session for a task, ignoring any existing one.
    NewAgent(String),
    /// Forward raw keystrokes to the active agent session.
    AgentInput(Vec<u8>),
    /// Start a catalog task by name (runs headlessly through the configured agent).
    StartTask(String),
    /// Submit a completed form via an RPC call.
    Submit { method: &'static str, params: Value },
}

/// The Ctrl+P popup: either the top-level menu or a step-by-step form.
pub enum Popup {
    None,
    Menu { selected: usize },
    Form(Form),
}

pub struct Form {
    pub kind: FormKind,
    pub title: &'static str,
    pub fields: Vec<&'static str>,
    /// Index of the field currently being edited.
    pub current: usize,
    /// Completed field values (one per completed field).
    pub values: Vec<String>,
    /// Typing buffer for the current field.
    pub input: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FormKind {
    AddTask,
    CreateSchedule,
    CreateNotification,
}

/// A task-definition entry in the catalog (as returned by `catalog.list`).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct CatalogEntry {
    pub name: String,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub schedule: Option<String>,
    #[serde(default)]
    pub needs: Option<String>,
    #[serde(default)]
    pub prompt: String,
}

impl CatalogEntry {
    /// The `provider/model` selector shown in the catalog, or `—` when unset.
    pub fn model_display(&self) -> String {
        match (&self.provider, &self.model) {
            (Some(p), Some(m)) => format!("{p}/{m}"),
            (None, Some(m)) => m.clone(),
            (Some(p), None) => p.clone(),
            (None, None) => "—".to_string(),
        }
    }
}

/// The Ctrl+P menu entries, in order.
pub const MENU_OPTIONS: [&str; 3] = [
    "Add task to catalog",
    "Create schedule",
    "Create notification hook",
];

/// A clickable screen region (absolute coordinates), populated during draw.
pub struct ClickRegion {
    pub row: u16,
    pub col_start: u16,
    pub col_end: u16,
    pub action: ClickAction,
}

#[derive(Clone, Copy)]
pub enum ClickAction {
    /// Switch to a tab.
    Tab(Tab),
}

pub struct App {
    pub tab: Tab,
    pub conn: ConnState,
    pub conn_detail: String,
    pub tasks: Vec<Task>,
    pub events: VecDeque<Event>,
    pub last_event_id: i64,
    pub logs: VecDeque<String>,
    pub should_quit: bool,

    // Task-list selection.
    pub tasks_selected: usize,

    // Task catalog.
    pub catalog: Vec<CatalogEntry>,
    pub catalog_selected: usize,

    // Embedded agent terminal.
    pub agent_session_id: Option<String>,
    pub agent_task_id: Option<String>,
    pub agent_name: Option<String>,
    pub agent_running: bool,
    pub agent_status: String,
    /// Set when the terminal was resized during draw; the session loop forwards
    /// the new size to the daemon and clears it.
    pub agent_resize: Option<(u16, u16)>,
    pub term: TerminalView,

    // Clickable regions (tab bar), populated during draw.
    pub click_regions: Vec<ClickRegion>,
    pub throbber_state: throbber_widgets_tui::ThrobberState,

    // Scheduler + notifications.
    pub schedules: Vec<Schedule>,
    pub notifications: Vec<NotificationRecord>,

    // Ctrl+P popup.
    pub popup: Popup,
}

impl App {
    pub fn new() -> Self {
        Self {
            tab: Tab::Tasks,
            conn: ConnState::Connecting,
            conn_detail: String::new(),
            tasks: Vec::new(),
            events: VecDeque::new(),
            last_event_id: 0,
            logs: VecDeque::new(),
            should_quit: false,
            tasks_selected: 0,
            catalog: Vec::new(),
            catalog_selected: 0,
            agent_session_id: None,
            agent_task_id: None,
            agent_name: None,
            agent_running: false,
            agent_status: String::new(),
            agent_resize: None,
            term: TerminalView::default(),
            click_regions: Vec::new(),
            throbber_state: throbber_widgets_tui::ThrobberState::default(),
            schedules: Vec::new(),
            notifications: Vec::new(),
            popup: Popup::None,
        }
    }

    pub fn next_tab(&mut self) {
        let idx = Tab::ALL.iter().position(|t| *t == self.tab).unwrap_or(0);
        self.tab = Tab::ALL[(idx + 1) % Tab::ALL.len()];
    }

    pub fn prev_tab(&mut self) {
        let idx = Tab::ALL.iter().position(|t| *t == self.tab).unwrap_or(0);
        self.tab = Tab::ALL[(idx + Tab::ALL.len() - 1) % Tab::ALL.len()];
    }

    pub fn select_next(&mut self) {
        if !self.tasks.is_empty() {
            self.tasks_selected = (self.tasks_selected + 1).min(self.tasks.len() - 1);
        }
    }

    pub fn select_prev(&mut self) {
        self.tasks_selected = self.tasks_selected.saturating_sub(1);
    }

    /// Adopt a newly started agent session (with its current screen frame) and
    /// switch to the Agent tab. Re-opening the same session keeps the existing
    /// emulator (and its size) rather than resetting it.
    pub fn open_agent(&mut self, session: AgentSessionInfo, frame: &[u8]) {
        if self.agent_session_id.as_deref() != Some(session.id.as_str()) {
            self.term = TerminalView::default();
        }
        self.term.process(frame);
        self.agent_session_id = Some(session.id);
        self.agent_task_id = session.task_id;
        self.agent_name = Some(session.agent);
        self.agent_running = session.running;
        self.agent_status.clear();
        self.tab = Tab::Agent;
    }

    /// Feed a full-screen frame to the active session's terminal.
    pub fn agent_output(&mut self, session_id: &str, frame: &[u8]) {
        if self.agent_session_id.as_deref() == Some(session_id) {
            self.term.process(frame);
        }
    }

    /// Mark the active session as exited.
    pub fn agent_exit(&mut self, session_id: &str, code: Option<i32>) {
        if self.agent_session_id.as_deref() == Some(session_id) {
            self.agent_running = false;
            self.agent_status = match code {
                Some(c) => format!("exited ({c}) — Ctrl+N for a new session"),
                None => "exited — Ctrl+N for a new session".to_string(),
            };
        }
    }

    /// Route a keypress. Ctrl+P toggles the menu; while a popup is open, keys go to
    /// it; the Agent tab forwards everything else to the agent PTY. Returns the
    /// async action to perform.
    pub fn handle_key(&mut self, key: KeyEvent) -> UiAction {
        if key.code == KeyCode::Char('p') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.popup = match self.popup {
                Popup::None => Popup::Menu { selected: 0 },
                Popup::Menu { .. } | Popup::Form(_) => Popup::None,
            };
            return UiAction::None;
        }

        if matches!(self.popup, Popup::None) && self.tab == Tab::Agent {
            return self.handle_agent_key(&key);
        }

        match &self.popup {
            Popup::Form(_) => return self.handle_form_key(key.code),
            Popup::Menu { .. } => return self.handle_menu_key(key.code),
            Popup::None => {}
        }

        self.handle_normal_key(&key)
    }

    /// Keys while the Agent tab is focused: Ctrl+Q detaches (leaving the session
    /// running on the daemon); everything else is encoded and forwarded to the PTY.
    fn handle_agent_key(&mut self, key: &KeyEvent) -> UiAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(key.code, KeyCode::Char('q')) {
            self.tab = Tab::Tasks;
            return UiAction::None;
        }
        if ctrl && matches!(key.code, KeyCode::Char('n')) {
            if let Some(task_id) = self.agent_task_id.clone() {
                return UiAction::NewAgent(task_id);
            }
            return UiAction::None;
        }

        let bytes = encode_key(key);
        if bytes.is_empty() {
            UiAction::None
        } else {
            UiAction::AgentInput(bytes)
        }
    }

    /// Handle mouse input: click the tab bar.
    pub fn handle_mouse(&mut self, mouse: MouseEvent) -> UiAction {
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
            for region in &self.click_regions {
                if mouse.row == region.row
                    && mouse.column >= region.col_start
                    && mouse.column < region.col_end
                {
                    let ClickAction::Tab(tab) = region.action;
                    self.tab = tab;
                    break;
                }
            }
        }
        UiAction::None
    }

    fn handle_menu_key(&mut self, code: KeyCode) -> UiAction {
        match code {
            KeyCode::Esc => {
                self.popup = Popup::None;
                UiAction::None
            }
            KeyCode::Up => {
                if let Popup::Menu { selected } = &mut self.popup {
                    *selected = selected.saturating_sub(1);
                }
                UiAction::None
            }
            KeyCode::Down => {
                if let Popup::Menu { selected } = &mut self.popup {
                    *selected = (*selected + 1).min(MENU_OPTIONS.len() - 1);
                }
                UiAction::None
            }
            KeyCode::Enter => {
                let selected = match self.popup {
                    Popup::Menu { selected } => selected,
                    _ => 0,
                };
                self.open_form(selected);
                UiAction::None
            }
            _ => UiAction::None,
        }
    }

    fn open_form(&mut self, selected: usize) {
        let kind = match selected {
            0 => FormKind::AddTask,
            1 => FormKind::CreateSchedule,
            _ => FormKind::CreateNotification,
        };
        let (title, fields) = match kind {
            FormKind::AddTask => (
                "Add task to catalog",
                vec![
                    "Task name",
                    "Agent (optional)",
                    "Provider (optional)",
                    "Model (optional)",
                    "Working dir (optional)",
                    "Schedule cron (optional)",
                    "Needs (optional)",
                    "Prompt",
                ],
            ),
            FormKind::CreateSchedule => (
                "Create schedule",
                vec!["Cron expression", "Task name", "Input JSON (optional)"],
            ),
            FormKind::CreateNotification => (
                "Create notification hook",
                vec!["Event kind", "Channel", "Config JSON (optional)"],
            ),
        };
        self.popup = Popup::Form(Form {
            kind,
            title,
            fields,
            current: 0,
            values: Vec::new(),
            input: String::new(),
        });
    }

    fn handle_form_key(&mut self, code: KeyCode) -> UiAction {
        match code {
            KeyCode::Esc => {
                self.popup = Popup::None;
                UiAction::None
            }
            KeyCode::Backspace => {
                if let Popup::Form(form) = &mut self.popup {
                    form.input.pop();
                }
                UiAction::None
            }
            KeyCode::Char(c) => {
                if let Popup::Form(form) = &mut self.popup {
                    form.input.push(c);
                }
                UiAction::None
            }
            KeyCode::Enter => {
                let Popup::Form(form) = &mut self.popup else {
                    return UiAction::None;
                };
                form.values.push(std::mem::take(&mut form.input));
                form.current += 1;
                if form.current < form.fields.len() {
                    return UiAction::None;
                }
                let kind = form.kind;
                let values = std::mem::take(&mut form.values);
                self.popup = Popup::None;
                self.build_submit(kind, values)
            }
            _ => UiAction::None,
        }
    }

    /// Map a completed form's values to an RPC submission.
    fn build_submit(&self, kind: FormKind, values: Vec<String>) -> UiAction {
        let v = |i: usize| -> String { values.get(i).cloned().unwrap_or_default() };
        let opt = |i: usize| -> Option<String> {
            let s = values.get(i).cloned().unwrap_or_default();
            if s.trim().is_empty() {
                None
            } else {
                Some(s)
            }
        };
        let json = |s: &str| -> Value { serde_json::from_str(s).unwrap_or(Value::Null) };

        match kind {
            FormKind::AddTask => UiAction::Submit {
                method: method::CATALOG_ADD,
                params: serde_json::json!({
                    "name": v(0),
                    "agent": opt(1),
                    "provider": opt(2),
                    "model": opt(3),
                    "cwd": opt(4),
                    "schedule": opt(5),
                    "needs": opt(6),
                    "prompt": v(7),
                }),
            },
            FormKind::CreateSchedule => UiAction::Submit {
                method: method::SCHEDULES_UPSERT,
                params: serde_json::json!({
                    "cron": v(0),
                    "task": v(1),
                    "input": json(&v(2)),
                }),
            },
            FormKind::CreateNotification => UiAction::Submit {
                method: method::HOOKS_UPSERT,
                params: serde_json::json!({
                    "event": v(0),
                    "channel": v(1),
                    "config": json(&v(2)),
                }),
            },
        }
    }

    fn handle_normal_key(&mut self, key: &KeyEvent) -> UiAction {
        let code = key.code;

        match code {
            KeyCode::Char('q') | KeyCode::Esc => UiAction::Quit,
            KeyCode::Tab | KeyCode::Right => {
                self.next_tab();
                UiAction::None
            }
            KeyCode::Left | KeyCode::BackTab => {
                self.prev_tab();
                UiAction::None
            }
            KeyCode::Up => {
                match self.tab {
                    Tab::Tasks => self.select_prev(),
                    Tab::Catalog => self.catalog_selected = self.catalog_selected.saturating_sub(1),
                    _ => {}
                }
                UiAction::None
            }
            KeyCode::Down => {
                match self.tab {
                    Tab::Tasks => self.select_next(),
                    Tab::Catalog if !self.catalog.is_empty() => {
                        self.catalog_selected =
                            (self.catalog_selected + 1).min(self.catalog.len() - 1);
                    }
                    _ => {}
                }
                UiAction::None
            }
            KeyCode::Enter => {
                if self.tab == Tab::Tasks {
                    if let Some(t) = self.tasks.get(self.tasks_selected) {
                        return UiAction::OpenAgent(t.id.to_string());
                    }
                }
                if self.tab == Tab::Catalog {
                    if let Some(entry) = self.catalog.get(self.catalog_selected) {
                        return UiAction::StartTask(entry.name.clone());
                    }
                }
                UiAction::None
            }
            _ => UiAction::None,
        }
    }

    /// Fold a server push into app state. Events are deduped by their monotonic id.
    pub fn handle_notification(&mut self, n: Notification) {
        match n.method.as_str() {
            push::EVENT => {
                if let Ok(ev) = serde_json::from_value::<Event>(n.params) {
                    self.ingest_event(ev);
                }
            }
            push::TASK_UPDATED => {
                if let Ok(t) = serde_json::from_value::<Task>(n.params) {
                    match self.tasks.iter().position(|x| x.id == t.id) {
                        Some(i) => self.tasks[i] = t,
                        None => self.tasks.insert(0, t),
                    }
                }
            }
            push::LOG_LINE => {
                if let Some(msg) = n.params.get("message").and_then(|m| m.as_str()) {
                    self.logs.push_back(msg.to_string());
                }
            }
            push::AGENT_OUTPUT => {
                let sid = n.params.get("session_id").and_then(|v| v.as_str());
                let data = n
                    .params
                    .get("data")
                    .and_then(|v| v.as_str())
                    .and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok());
                if let (Some(sid), Some(data)) = (sid, data) {
                    self.agent_output(sid, &data);
                }
            }
            push::AGENT_EXIT => {
                let sid = n.params.get("session_id").and_then(|v| v.as_str());
                let code = n.params.get("code").and_then(|v| v.as_i64()).map(|c| c as i32);
                if let Some(sid) = sid {
                    self.agent_exit(sid, code);
                }
            }
            _ => {}
        }

        while self.events.len() > 1000 {
            self.events.pop_front();
        }
        while self.logs.len() > 200 {
            self.logs.pop_front();
        }
    }

    /// Add an event if it is newer than the last seen cursor, advancing the cursor.
    pub fn ingest_event(&mut self, ev: Event) {
        if ev.id > self.last_event_id {
            self.last_event_id = ev.id;
            self.events.push_back(ev);
        }
    }
}

/// Encode a keypress as the byte sequence a terminal would emit for it.
fn encode_key(key: &KeyEvent) -> Vec<u8> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let mut out = Vec::new();
    if alt {
        out.push(0x1b);
    }
    match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                // Ctrl+A..Z / Ctrl+@.._ map to control codes.
                let b = c.to_ascii_lowercase() as u8;
                if b.is_ascii_lowercase() {
                    out.push(b & 0x1f);
                } else if c == ' ' {
                    out.push(0x00);
                } else {
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                }
            } else {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
        KeyCode::Enter => out.push(b'\r'),
        KeyCode::Backspace => out.push(0x7f),
        KeyCode::Tab => out.push(b'\t'),
        KeyCode::BackTab => out.extend_from_slice(b"\x1b[Z"),
        KeyCode::Esc => out.push(0x1b),
        KeyCode::Up => out.extend_from_slice(b"\x1b[A"),
        KeyCode::Down => out.extend_from_slice(b"\x1b[B"),
        KeyCode::Right => out.extend_from_slice(b"\x1b[C"),
        KeyCode::Left => out.extend_from_slice(b"\x1b[D"),
        KeyCode::Home => out.extend_from_slice(b"\x1b[H"),
        KeyCode::End => out.extend_from_slice(b"\x1b[F"),
        KeyCode::PageUp => out.extend_from_slice(b"\x1b[5~"),
        KeyCode::PageDown => out.extend_from_slice(b"\x1b[6~"),
        KeyCode::Insert => out.extend_from_slice(b"\x1b[2~"),
        KeyCode::Delete => out.extend_from_slice(b"\x1b[3~"),
        KeyCode::F(n) => {
            let seq: &[u8] = match n {
                1 => b"\x1bOP",
                2 => b"\x1bOQ",
                3 => b"\x1bOR",
                4 => b"\x1bOS",
                5 => b"\x1b[15~",
                6 => b"\x1b[17~",
                7 => b"\x1b[18~",
                8 => b"\x1b[19~",
                9 => b"\x1b[20~",
                10 => b"\x1b[21~",
                11 => b"\x1b[23~",
                12 => b"\x1b[24~",
                _ => b"",
            };
            out.extend_from_slice(seq);
        }
        _ => {}
    }
    out
}
