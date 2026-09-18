//! TUI application state and the logic for folding server pushes into it.

use std::collections::VecDeque;

use ratatui::crossterm::event::KeyCode;

use favetto_core::model::{ChatMessage, ChatSession, Event, Task};
use favetto_core::rpc::{push, Notification};

/// Tabs shown in the header. Tasks and Events are live; the rest are placeholders
/// that fill in over later milestones (Chat is the per-task LLM conversation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Tasks,
    Chat,
    Events,
    Scheduler,
    Notifications,
    Repos,
    Agents,
    Mcp,
}

impl Tab {
    pub const ALL: [Tab; 8] = [
        Tab::Tasks,
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

    // Chat state.
    pub chat_session_id: Option<String>,
    pub chat_task_id: Option<String>,
    pub chat_messages: Vec<ChatMessage>,
    pub input: String,
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
            chat_session_id: None,
            chat_task_id: None,
            chat_messages: Vec::new(),
            input: String::new(),
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
        self.input.clear();
        self.tab = Tab::Chat;
    }

    /// Replace the conversation with an updated session (after a send).
    pub fn update_chat(&mut self, session: ChatSession) {
        self.chat_messages = session.messages;
        if self.chat_session_id.is_none() {
            self.chat_session_id = Some(session.id);
        }
    }

    /// Route a keypress to the active tab. Returns the async action to perform.
    pub fn handle_key(&mut self, code: KeyCode) -> UiAction {
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
                KeyCode::Left => {
                    self.prev_tab();
                    UiAction::None
                }
                KeyCode::Enter => UiAction::SendChat,
                KeyCode::Backspace => {
                    self.input.pop();
                    UiAction::None
                }
                KeyCode::Char(c) => {
                    self.input.push(c);
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
            KeyCode::Left => {
                self.prev_tab();
                UiAction::None
            }
            KeyCode::Up => {
                if self.tab == Tab::Tasks {
                    self.select_prev();
                }
                UiAction::None
            }
            KeyCode::Down => {
                if self.tab == Tab::Tasks {
                    self.select_next();
                }
                UiAction::None
            }
            KeyCode::Enter => {
                if self.tab == Tab::Tasks {
                    if let Some(t) = self.tasks.get(self.tasks_selected) {
                        return UiAction::OpenChat(t.id.to_string());
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
