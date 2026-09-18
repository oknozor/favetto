//! TUI application state and the logic for folding server pushes into it.

use std::collections::VecDeque;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use serde_json::Value;

use favetto_core::model::{ChatMessage, ChatSession, Event, MessageRole, NotificationRecord, Schedule, Task};
use favetto_core::rpc::{method, push, Notification};

/// Tabs shown in the header. Tasks and Events are live; the rest are placeholders
/// that fill in over later milestones (Chat is the per-task LLM conversation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Tasks,
    Catalog,
    Chat,
    Events,
    Scheduler,
    Notifications,
    Repos,
    Agents,
    Mcp,
}

impl Tab {
    pub const ALL: [Tab; 9] = [
        Tab::Tasks,
        Tab::Catalog,
        Tab::Chat,
        Tab::Events,
        Tab::Scheduler,
        Tab::Notifications,
        Tab::Repos,
        Tab::Agents,
        Tab::Mcp,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Tab::Tasks => "Tasks",
            Tab::Catalog => "Catalog",
            Tab::Chat => "Chat",
            Tab::Events => "Events",
            Tab::Scheduler => "Scheduler",
            Tab::Notifications => "Notifications",
            Tab::Repos => "Repos",
            Tab::Agents => "Agents",
            Tab::Mcp => "MCP",
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
    /// Open (or switch to) the chat for a task id.
    OpenChat(String),
    /// Send the current input buffer.
    SendChat,
    /// Start a catalog task by name.
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
    AddProvider,
    AddTask,
    CreateSchedule,
    CreateNotification,
}

/// A task-definition entry in the catalog (as returned by `catalog.list`).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct CatalogEntry {
    pub name: String,
    pub model: String,
    pub schedule: Option<String>,
    pub needs: Option<String>,
}

/// The Ctrl+P menu entries, in order.
pub const MENU_OPTIONS: [&str; 4] = [
    "Add provider / model",
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
    /// Toggle the collapse of the i-th assistant message's thinking block.
    ToggleThinking(usize),
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

    // Chat state.
    pub chat_session_id: Option<String>,
    pub chat_task_id: Option<String>,
    pub chat_messages: Vec<ChatMessage>,
    pub input: String,
    /// Lines scrolled up from the bottom of the chat (0 = pinned to bottom).
    pub chat_scroll: usize,
    /// In-progress streamed answer text (not yet committed to the conversation).
    pub streaming: Option<String>,
    /// In-progress streamed reasoning text for the current turn.
    pub streaming_reasoning: String,
    /// Per-assistant-message collapse state (aligned to assistant messages).
    pub thinking_collapsed: Vec<bool>,
    /// Clickable regions (tab bar + thinking headers), populated during draw.
    pub click_regions: Vec<ClickRegion>,
    /// A chat turn is in flight (show the "Thinking" throbber).
    pub thinking: bool,
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
            chat_session_id: None,
            chat_task_id: None,
            chat_messages: Vec::new(),
            input: String::new(),
            chat_scroll: 0,
            streaming: None,
            streaming_reasoning: String::new(),
            thinking_collapsed: Vec::new(),
            click_regions: Vec::new(),
            thinking: false,
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

    /// Adopt a freshly opened chat session and switch to the Chat tab.
    pub fn open_chat(&mut self, session: ChatSession) {
        self.chat_session_id = Some(session.id);
        self.chat_task_id = session.task_id;
        self.chat_messages = session.messages;
        self.sync_collapse();
        self.input.clear();
        self.chat_scroll = 0;
        self.streaming = None;
        self.streaming_reasoning.clear();
        self.thinking = false;
        self.tab = Tab::Chat;
    }

    /// Replace the conversation with an updated session (after a send, or when
    /// resuming after a reconnect). Does not switch tabs or reset input.
    pub fn update_chat(&mut self, session: ChatSession) {
        self.chat_session_id = Some(session.id);
        self.chat_task_id = session.task_id;
        self.chat_messages = session.messages;
        self.sync_collapse();
    }

    /// Adopt a freshly opened chat session without switching to the Chat tab,
    /// used to re-sync the conversation after a reconnect.
    pub fn resume_chat(&mut self, session: ChatSession) {
        self.update_chat(session);
    }

    /// Keep `thinking_collapsed` aligned with the number of assistant messages,
    /// preserving existing state and defaulting new ones to collapsed.
    pub(crate) fn sync_collapse(&mut self) {
        let n = self
            .chat_messages
            .iter()
            .filter(|m| m.role == MessageRole::Assistant)
            .count();
        self.thinking_collapsed.resize(n, true);
    }

    /// Route a keypress. Ctrl+P toggles the menu; while a popup is open, keys go to
    /// it; otherwise they go to the active tab. Returns the async action to perform.
    pub fn handle_key(&mut self, key: KeyEvent) -> UiAction {
        if key.code == KeyCode::Char('p') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.popup = match self.popup {
                Popup::None => Popup::Menu { selected: 0 },
                Popup::Menu { .. } | Popup::Form(_) => Popup::None,
            };
            return UiAction::None;
        }

        match &self.popup {
            Popup::Form(_) => return self.handle_form_key(key.code),
            Popup::Menu { .. } => return self.handle_menu_key(key.code),
            Popup::None => {}
        }

        self.handle_normal_key(&key)
    }

    /// Handle mouse input: scroll the chat and click the tab bar / thinking headers.
    pub fn handle_mouse(&mut self, mouse: MouseEvent) -> UiAction {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                if self.tab == Tab::Chat {
                    self.chat_scroll += 3;
                }
            }
            MouseEventKind::ScrollDown => {
                if self.tab == Tab::Chat {
                    self.chat_scroll = self.chat_scroll.saturating_sub(3);
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                for region in &self.click_regions {
                    if mouse.row == region.row
                        && mouse.column >= region.col_start
                        && mouse.column < region.col_end
                    {
                        match region.action {
                            ClickAction::Tab(tab) => self.tab = tab,
                            ClickAction::ToggleThinking(i) => {
                                if let Some(c) = self.thinking_collapsed.get_mut(i) {
                                    *c = !*c;
                                }
                            }
                        }
                        break;
                    }
                }
            }
            _ => {}
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
            0 => FormKind::AddProvider,
            1 => FormKind::AddTask,
            2 => FormKind::CreateSchedule,
            _ => FormKind::CreateNotification,
        };
        let (title, fields) = match kind {
            FormKind::AddProvider => (
                "Add provider",
                vec!["Provider name", "Kind (openai)", "API key env var", "Model", "Base URL"],
            ),
            FormKind::AddTask => (
                "Add task to catalog",
                vec![
                    "Task name",
                    "Model (provider:model)",
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
            FormKind::AddProvider => UiAction::Submit {
                method: method::CONFIG_SET_PROVIDER,
                params: serde_json::json!({
                    "name": v(0),
                    "kind": v(1),
                    "api_key_env": v(2),
                    "model": v(3),
                    "base_url": v(4),
                }),
            },
            FormKind::AddTask => UiAction::Submit {
                method: method::CATALOG_ADD,
                params: serde_json::json!({
                    "name": v(0),
                    "model": v(1),
                    "schedule": opt(2),
                    "needs": opt(3),
                    "prompt": v(4),
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
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        if self.tab == Tab::Chat {
            return match code {
                KeyCode::Esc => {
                    self.tab = Tab::Tasks;
                    UiAction::None
                }
                KeyCode::Tab | KeyCode::Right => {
                    self.next_tab();
                    UiAction::None
                }
                KeyCode::Left | KeyCode::BackTab => {
                    self.prev_tab();
                    UiAction::None
                }
                KeyCode::Enter => UiAction::SendChat,
                KeyCode::Backspace => {
                    self.input.pop();
                    UiAction::None
                }
                KeyCode::Char('b') if ctrl => {
                    self.chat_scroll += 3;
                    UiAction::None
                }
                KeyCode::Char('f') if ctrl => {
                    self.chat_scroll = self.chat_scroll.saturating_sub(3);
                    UiAction::None
                }
                KeyCode::Char('r') if ctrl => {
                    let expand = self.thinking_collapsed.iter().any(|c| *c);
                    for c in &mut self.thinking_collapsed {
                        *c = expand;
                    }
                    UiAction::None
                }
                KeyCode::Char(c) => {
                    self.input.push(c);
                    UiAction::None
                }
                KeyCode::Up => {
                    self.chat_scroll += 1;
                    UiAction::None
                }
                KeyCode::Down => {
                    self.chat_scroll = self.chat_scroll.saturating_sub(1);
                    UiAction::None
                }
                KeyCode::PageUp => {
                    self.chat_scroll += 10;
                    UiAction::None
                }
                KeyCode::PageDown => {
                    self.chat_scroll = self.chat_scroll.saturating_sub(10);
                    UiAction::None
                }
                _ => UiAction::None,
            };
        }

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
                        return UiAction::OpenChat(t.id.to_string());
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
            push::CHAT_DELTA => {
                if let Some(text) = n.params.get("text").and_then(|t| t.as_str()) {
                    self.streaming
                        .get_or_insert_with(String::new)
                        .push_str(text);
                    self.thinking = false;
                }
            }
            push::CHAT_REASONING => {
                if let Some(text) = n.params.get("text").and_then(|t| t.as_str()) {
                    self.streaming_reasoning.push_str(text);
                    self.thinking = true;
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
