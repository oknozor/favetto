//! TUI application state and the logic for folding server pushes into it.

mod input;
mod task_vars;
#[cfg(test)]
mod tests;
mod wizard;

use std::collections::{BTreeMap, HashSet, VecDeque};

use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use serde_json::Value;

use base64::Engine as _;

use favetto_core::model::{
    AgentCapabilities, AgentCatalogEntry, AgentSessionInfo, Event, EventKind, NotificationRecord,
    Schedule, Task, TaskStatus,
};
use favetto_core::rpc::{method, push, Notification};
use favetto_providers::Provider;

use crate::tasks::TaskVar;

use super::sound::SoundCue;
use super::term::TerminalView;
use super::text_buffer::TextBuffer;
use super::theme::Theme;

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
    /// Open the Ctrl+O agent-session picker (needs an async `agents.list`).
    OpenSessions,
    /// Attach the Agent panel to an existing session id (`agents.attach`).
    AttachSession(String),
    /// Forward raw keystrokes to the active agent session.
    AgentInput(Vec<u8>),
    /// Start a catalog task by name (runs headlessly through the configured agent).
    StartTask(String),
    /// Start a catalog task with collected input variables.
    StartTaskWithInput {
        name: String,
        input: Value,
    },
    /// Open the selected Catalog task's Markdown in the user's editor.
    EditCatalog(String),
    /// Open the one-shot task wizard.
    OpenWizard,
    /// Fetch the catalog workflow graph (`workflow.get`) and show the overlay.
    OpenWorkflow,
    /// The wizard needs the configured provider/model catalog (`providers.list`).
    WizardLoadProviders,
    /// The wizard completed: start an inline, interactive one-shot task.
    WizardStart {
        agent: String,
        provider: Option<String>,
        model: Option<String>,
        cwd: Option<String>,
    },
    /// Submit a completed form via an RPC call.
    Submit {
        method: &'static str,
        params: Value,
    },
}

/// The Ctrl+P popup: either the top-level menu, a step-by-step form, the
/// one-shot wizard, or the `?` keybinding reference.
pub enum Popup {
    None,
    Menu {
        selected: usize,
    },
    Form(Form),
    Wizard(Wizard),
    /// Prompt for a catalog task's declared `[[vars]]`.
    TaskVars(TaskVarsForm),
    /// Scrollable keybinding reference, toggled with `?`.
    Help {
        scroll: u16,
    },
    /// Scrollable floating workflow graph, toggled with `w`.
    Workflow {
        scroll: u16,
        hscroll: u16,
    },
    /// The Ctrl+O session picker: the daemon's live/retained agent sessions,
    /// listed so the Agent panel can hop between concurrent runs.
    Sessions {
        selected: usize,
        sessions: Vec<AgentSessionInfo>,
    },
}

pub struct Form {
    pub kind: FormKind,
    pub title: &'static str,
    pub fields: Vec<&'static str>,
    /// Index of the field currently being edited.
    pub current: usize,
    /// Completed field values (one per completed field).
    pub values: Vec<String>,
    /// Caret-aware typing buffer for the current field.
    pub input: TextBuffer,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FormKind {
    AddTask,
    CreateSchedule,
    CreateNotification,
}

/// A step in the one-shot task wizard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WizardStep {
    Agent,
    Provider,
    Model,
    Dir,
}

impl WizardStep {
    pub fn title(self) -> &'static str {
        match self {
            WizardStep::Agent => "One-shot task — select agent",
            WizardStep::Provider => "One-shot task — select provider",
            WizardStep::Model => "One-shot task — select model",
            WizardStep::Dir => "One-shot task — working directory",
        }
    }
}

/// State for the capability-driven one-shot wizard (agent → provider → model →
/// directory; provider/model steps are skipped for agents that lack them).
pub struct Wizard {
    pub step: WizardStep,
    pub selected: usize,
    /// `(label, value)` pairs for the current step.
    pub choices: Vec<(String, String)>,
    /// The configured provider/model catalog (fetched once).
    pub providers: Vec<Provider>,
    /// Capabilities of the selected agent, used to skip unsupported steps.
    pub capabilities: AgentCapabilities,
    /// Agent name → capabilities, so a selection can skip steps.
    pub agent_caps: BTreeMap<String, AgentCapabilities>,
    /// Agent name → whether its executable was found on the daemon's PATH.
    pub available: BTreeMap<String, bool>,
    pub agent: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    /// Working directory buffer (with caret).
    pub dir: TextBuffer,
    pub loading: bool,
    pub error: Option<String>,
}

impl Wizard {
    /// Move the selection one row in `forward` (Down) or backward (Up) order,
    /// skipping choices marked unavailable. Stays put when there is no
    /// selectable choice further in that direction.
    fn move_selection(&mut self, forward: bool) {
        let len = self.choices.len();
        if len == 0 {
            return;
        }
        let mut idx = self.selected;
        loop {
            if forward {
                if idx + 1 >= len {
                    return;
                }
                idx += 1;
            } else {
                if idx == 0 {
                    return;
                }
                idx -= 1;
            }
            let available = self
                .available
                .get(&self.choices[idx].1)
                .copied()
                .unwrap_or(true);
            if available {
                self.selected = idx;
                return;
            }
        }
    }
}

/// State for the floating form that collects a catalog task's `[[vars]]`.
pub struct TaskVarsForm {
    /// The catalog task name being started.
    pub task: String,
    pub vars: Vec<TaskVar>,
    /// Index of the variable currently being edited.
    pub current: usize,
    /// Typed values, one per variable (initialized from each var's `default`).
    /// Each value carries its own caret so fields remember the position.
    pub values: Vec<TextBuffer>,
    /// Selected index into each variable's `choices` (0 when absent).
    pub choice_selected: Vec<usize>,
    pub error: Option<String>,
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
    pub needs: Option<String>,
    #[serde(default)]
    pub vars: Vec<TaskVar>,
    #[serde(default)]
    pub prompt: String,
}

/// One visible row of the Catalog tree: a folder header or a task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogRow {
    Folder {
        /// Folder path relative to the tasks root, `/`-separated.
        path: String,
        depth: usize,
        collapsed: bool,
    },
    Task {
        /// Index into [`App::catalog`].
        index: usize,
        depth: usize,
    },
}

impl CatalogEntry {
    /// The last `/`-separated segment of the task's relative path (its file
    /// stem), shown under its folder row.
    pub fn stem(&self) -> &str {
        self.name.rsplit('/').next().unwrap_or(self.name.as_str())
    }

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
pub const MENU_OPTIONS: [&str; 4] = [
    "New one-shot task",
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

/// The last-rendered geometry of a table, used to map a click to a data row.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ListGeometry {
    /// The data-rows rectangle (inside the border, below the one-line header).
    pub inner: Rect,
    /// `TableState::offset()` after the last render: first visible data-row index.
    pub offset: usize,
    /// Number of data rows at the last render.
    pub len: usize,
}

impl ListGeometry {
    /// The data-row index under an absolute `(row, col)`, if any.
    pub fn row_at(&self, row: u16, col: u16) -> Option<usize> {
        row_index_at(self.inner, self.offset, self.len, row, col)
    }
}

pub struct App {
    /// The colour palette every widget paints through.
    pub theme: Theme,
    pub tab: Tab,
    pub conn: ConnState,
    pub conn_detail: String,
    pub tasks: Vec<Task>,
    pub events: VecDeque<Event>,
    /// Selected event, counted from the newest (0 = newest).
    pub events_selected: usize,
    pub last_event_id: i64,
    pub logs: VecDeque<String>,
    pub should_quit: bool,

    // Task-list selection.
    pub tasks_selected: usize,

    // Task catalog.
    pub catalog: Vec<CatalogEntry>,
    /// Index into the *visible* rows of the Catalog tree (see
    /// [`App::catalog_rows`]), not into [`App::catalog`].
    pub catalog_selected: usize,
    /// Folder paths (relative, `/`-separated) collapsed in the Catalog tree.
    /// Session-scoped: deliberately not cleared by [`App::set_catalog`], so it
    /// survives `catalog.updated` reloads.
    pub catalog_collapsed: HashSet<String>,
    /// Raw Markdown of the selected catalog task as `(name, source)`.
    pub catalog_preview: Option<(String, String)>,
    /// Name currently being fetched for the preview (avoids duplicate requests).
    pub catalog_preview_pending: Option<String>,
    /// Vertical scroll offset (in rendered lines) of the catalog preview pane.
    pub catalog_preview_scroll: u16,
    /// Maximum useful scroll offset of the preview (set during draw).
    pub catalog_preview_max_scroll: u16,
    /// The preview pane's screen rectangle (set during draw) for mouse hit tests.
    pub catalog_preview_area: Option<Rect>,
    /// Whether the Catalog preview pane is shown. Toggled with `p` on the
    /// Catalog tab; when hidden the tree takes the full width and no preview is
    /// fetched.
    pub catalog_preview_visible: bool,
    /// Set when the daemon pushes `catalog.updated`; the session loop re-fetches
    /// the catalog and clears it.
    pub catalog_dirty: bool,

    /// The catalog workflow graph in DOT, fetched for the workflow overlay.
    pub workflow_dot: Option<String>,
    /// Path of the daemon's persisted `<data_dir>/workflow.dot`, if reported.
    pub workflow_path: Option<String>,
    /// Rendered box-drawing rows, when the structured graph was available.
    pub workflow_lines: Option<Vec<String>>,
    /// Why the overlay is showing raw DOT (fallback explanation), if it is.
    pub workflow_note: Option<String>,

    // Embedded agent terminal.
    pub agent_session_id: Option<String>,
    pub agent_task_id: Option<String>,
    pub agent_name: Option<String>,
    pub agent_running: bool,
    pub agent_status: String,
    /// When true, keystrokes are forwarded to the embedded agent; when false they
    /// are handled by the favetto TUI. Toggled with Ctrl+Y.
    pub agent_capture: bool,
    /// True when the panel is showing a retained headless PTY that must not
    /// receive keystrokes (a read-only replay or a live unattended run).
    pub agent_read_only: bool,
    /// The last `agents.start` / `agents.attach` error, shown in the status bar.
    pub agent_error: Option<String>,
    /// The embedded terminal's inner screen rectangle (set during draw), used to
    /// translate mouse events into the agent's coordinate space.
    pub agent_area: Option<Rect>,
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

    // Last-rendered list geometry, for mouse row hit-testing.
    pub tasks_geom: ListGeometry,
    pub catalog_geom: ListGeometry,
    pub events_geom: ListGeometry,

    // Scheduler + notifications selection (no primary action).
    pub schedules_selected: usize,
    pub schedules_geom: ListGeometry,
    pub notifications_selected: usize,
    pub notifications_geom: ListGeometry,

    /// The daemon's working directory (from `system.ping`), used to prefill the
    /// one-shot wizard's directory step.
    pub daemon_cwd: Option<String>,

    // Ctrl+P popup.
    pub popup: Popup,

    /// Queued sound cues, drained by the session loop.
    pub sound_cues: Vec<SoundCue>,
    /// Session-only mute toggle (`M`).
    pub sound_muted: bool,
    /// Suppresses cue enqueueing during the initial event replay.
    pub sound_suppressed: bool,
    /// Whether sound is enabled by configuration (drives the status badge).
    pub sound_enabled: bool,
}

impl App {
    /// The default test/initial palette. Production resolves [`Theme::detect`].
    #[cfg(test)]
    pub fn new() -> Self {
        Self::with_theme(Theme::dark())
    }

    pub fn with_theme(theme: Theme) -> Self {
        Self {
            theme,
            tab: Tab::Tasks,
            conn: ConnState::Connecting,
            conn_detail: String::new(),
            tasks: Vec::new(),
            events: VecDeque::new(),
            events_selected: 0,
            last_event_id: 0,
            logs: VecDeque::new(),
            should_quit: false,
            tasks_selected: 0,
            catalog: Vec::new(),
            catalog_selected: 0,
            catalog_collapsed: HashSet::new(),
            catalog_preview: None,
            catalog_preview_pending: None,
            catalog_preview_scroll: 0,
            catalog_preview_max_scroll: 0,
            catalog_preview_area: None,
            catalog_preview_visible: true,
            catalog_dirty: false,
            workflow_dot: None,
            workflow_path: None,
            workflow_lines: None,
            workflow_note: None,
            agent_session_id: None,
            agent_task_id: None,
            agent_name: None,
            agent_running: false,
            agent_status: String::new(),
            agent_capture: false,
            agent_read_only: false,
            agent_error: None,
            agent_area: None,
            agent_resize: None,
            term: TerminalView::default(),
            click_regions: Vec::new(),
            throbber_state: throbber_widgets_tui::ThrobberState::default(),
            schedules: Vec::new(),
            notifications: Vec::new(),
            tasks_geom: ListGeometry::default(),
            catalog_geom: ListGeometry::default(),
            events_geom: ListGeometry::default(),
            schedules_selected: 0,
            schedules_geom: ListGeometry::default(),
            notifications_selected: 0,
            notifications_geom: ListGeometry::default(),
            daemon_cwd: None,
            popup: Popup::None,
            sound_cues: Vec::new(),
            sound_muted: false,
            sound_suppressed: false,
            sound_enabled: true,
        }
    }

    /// True while something on screen is animating (throbber) or reconnecting.
    ///
    /// The session loop only enables its redraw tick while this holds, so an idle
    /// TUI never redraws without an event.
    pub fn needs_animation(&self) -> bool {
        if self.conn != ConnState::Connected {
            return true;
        }
        if self.agent_running {
            return true;
        }
        if self
            .tasks
            .iter()
            .any(|t| matches!(t.status, TaskStatus::Running | TaskStatus::AwaitingInput))
        {
            return true;
        }
        matches!(&self.popup, Popup::Wizard(w) if w.loading)
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

    /// Replace the catalog, clamping the selection to the new visible rows and
    /// dropping the cached preview so it is re-fetched (the selected task's
    /// source may have changed). Collapsed-folder state is kept, so it survives a
    /// live reload.
    pub fn set_catalog(&mut self, catalog: Vec<CatalogEntry>) {
        self.catalog = catalog;
        let rows = self.catalog_rows().len();
        self.catalog_selected = if rows == 0 {
            0
        } else {
            self.catalog_selected.min(rows - 1)
        };
        self.catalog_preview = None;
    }

    /// Store the fetched workflow graph (DOT + persisted path) and, when the
    /// structured `graph` field was available, its cached box-drawing render.
    pub fn set_workflow(
        &mut self,
        dot: String,
        path: Option<String>,
        graph: Option<crate::workflow::WorkflowGraph>,
    ) {
        self.workflow_dot = Some(dot);
        self.workflow_path = path;
        match graph {
            Some(graph) => match crate::tui::workflow_view::render(&graph) {
                Ok(text) => {
                    self.workflow_lines = Some(text.lines().map(str::to_string).collect());
                    self.workflow_note = None;
                }
                Err(e) => {
                    self.workflow_lines = None;
                    self.workflow_note = Some(format!("graph render failed: {e}"));
                }
            },
            None => {
                self.workflow_lines = None;
                self.workflow_note =
                    Some("structured graph unavailable; showing raw DOT".to_string());
            }
        }
    }

    /// The visible rows of the Catalog tree: folder headers and tasks, with every
    /// descendant of a collapsed folder hidden. Folders sort before a same-named
    /// task (so `a.md` and `a/` render folder-first) and `depth` is the number of
    /// `/` segments above the row.
    pub fn catalog_rows(&self) -> Vec<CatalogRow> {
        // (key, is_task, catalog index) records for folders and tasks.
        let folders: HashSet<&str> = self
            .catalog
            .iter()
            .flat_map(|e| {
                let mut out = Vec::new();
                let mut cur = e.name.as_str();
                while let Some((prefix, _)) = cur.rsplit_once('/') {
                    out.push(prefix);
                    cur = prefix;
                }
                out
            })
            .collect();

        let mut records: Vec<(String, bool, usize)> = folders
            .iter()
            .map(|f| ((*f).to_string(), false, 0))
            .collect();
        records.extend(
            self.catalog
                .iter()
                .enumerate()
                .map(|(i, e)| (e.name.clone(), true, i)),
        );
        records.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));

        let mut rows = Vec::with_capacity(records.len());
        for (key, is_task, index) in records {
            // Hide a row when it is a strict descendant of a collapsed folder.
            let hidden = self
                .catalog_collapsed
                .iter()
                .any(|c| key.starts_with(&format!("{c}/")));
            if hidden {
                continue;
            }
            let depth = key.matches('/').count();
            if is_task {
                rows.push(CatalogRow::Task { index, depth });
            } else {
                let collapsed = self.catalog_collapsed.contains(&key);
                rows.push(CatalogRow::Folder {
                    path: key,
                    depth,
                    collapsed,
                });
            }
        }
        rows
    }

    /// Index into [`App::catalog`] for the selected row, or `None` when a folder
    /// (or nothing) is selected.
    pub fn selected_catalog_task(&self) -> Option<usize> {
        match self.catalog_rows().get(self.catalog_selected) {
            Some(CatalogRow::Task { index, .. }) => Some(*index),
            _ => None,
        }
    }

    /// Move the Catalog selection to a visible row, clamping it, resetting the
    /// preview scroll, and clearing the preview when a folder is selected.
    fn select_catalog_row(&mut self, index: usize) {
        let rows = self.catalog_rows();
        if rows.is_empty() {
            self.catalog_selected = 0;
            return;
        }
        let index = index.min(rows.len() - 1);
        self.catalog_selected = index;
        self.reset_catalog_preview_scroll();
        if matches!(rows.get(index), Some(CatalogRow::Folder { .. })) {
            self.catalog_preview = None;
        }
    }

    /// Fold/unfold the selected folder row. Returns `true` when a folder was
    /// selected (and toggled), `false` for a task row.
    fn toggle_catalog_folder(&mut self) -> bool {
        let rows = self.catalog_rows();
        let Some(CatalogRow::Folder { path, .. }) = rows.get(self.catalog_selected) else {
            return false;
        };
        let path = path.clone();
        if !self.catalog_collapsed.remove(&path) {
            self.catalog_collapsed.insert(path);
        }
        // Collapsing can shrink the row list below the selection; re-clamp.
        let rows = self.catalog_rows().len();
        self.catalog_selected = if rows == 0 {
            0
        } else {
            self.catalog_selected.min(rows - 1)
        };
        true
    }

    /// The catalog task whose preview should be loaded, if the Catalog tab is
    /// showing a task that isn't already loaded or being fetched. Folder rows
    /// never fetch a preview.
    pub fn catalog_preview_target(&self) -> Option<String> {
        if !self.catalog_preview_visible {
            return None;
        }
        if self.tab != Tab::Catalog {
            return None;
        }
        let name = self
            .catalog
            .get(self.selected_catalog_task()?)?
            .name
            .clone();
        if self
            .catalog_preview
            .as_ref()
            .is_some_and(|(loaded, _)| loaded == &name)
        {
            return None;
        }
        if self.catalog_preview_pending.as_deref() == Some(name.as_str()) {
            return None;
        }
        Some(name)
    }

    /// Toggle the Catalog preview pane (`p`). Hiding it drops the recorded pane
    /// rectangle so the mouse wheel/PageUp/PageDown stop targeting it; the tree
    /// expands to the full width. Re-showing lets [`App::catalog_preview_target`]
    /// re-request the preview lazily.
    pub fn toggle_catalog_preview(&mut self) {
        self.catalog_preview_visible = !self.catalog_preview_visible;
        if !self.catalog_preview_visible {
            self.catalog_preview_area = None;
        }
    }

    /// Scroll the catalog preview by `delta` rendered lines (negative scrolls up),
    /// clamped to the available content.
    pub fn scroll_catalog_preview(&mut self, delta: i32) {
        let max = self.catalog_preview_max_scroll as i32;
        self.catalog_preview_scroll =
            (self.catalog_preview_scroll as i32 + delta).clamp(0, max) as u16;
    }

    /// A page-sized scroll step for the preview: its inner height, or 10 lines
    /// before the pane has been drawn once.
    fn catalog_preview_page(&self) -> i32 {
        self.catalog_preview_area
            .map_or(10, |a| a.height.saturating_sub(2).max(1)) as i32
    }

    /// Reset the preview scroll to the top (e.g. when the selected task changes).
    fn reset_catalog_preview_scroll(&mut self) {
        self.catalog_preview_scroll = 0;
    }

    /// Adopt a newly started agent session (with its current screen frame) and
    /// switch to the Agent tab. Re-opening the same session keeps the existing
    /// emulator (and its size) rather than resetting it.
    ///
    /// A retained headless PTY (an unattended run, or a finished run being
    /// replayed) is shown **read-only**: it is displayed but never receives
    /// keystrokes. A headless run that is blocked on the user is the exception —
    /// attaching is what lets the answer reach its prompt.
    pub fn open_agent(&mut self, session: AgentSessionInfo, frame: &[u8]) {
        let read_only = session.headless && session.awaiting_input.is_none();
        let status = if !session.headless {
            String::new()
        } else if session.awaiting_input.is_some() {
            "headless run (awaiting input)".to_string()
        } else if session.running {
            "headless run (read-only)".to_string()
        } else {
            "headless run (replay, read-only)".to_string()
        };

        if self.agent_session_id.as_deref() != Some(session.id.as_str()) {
            self.term = TerminalView::default();
        }
        self.term.process(frame);
        self.agent_session_id = Some(session.id);
        self.agent_task_id = session.task_id;
        self.agent_name = Some(session.agent);
        self.agent_running = session.running;
        self.agent_read_only = read_only;
        self.agent_status = status;
        self.agent_error = None;
        // The embedded agent owns the keyboard as soon as the panel is opened,
        // unless the PTY is read-only.
        self.agent_capture = !read_only;
        self.tab = Tab::Agent;
    }

    /// Replace the popup with the Ctrl+O agent-session picker. An empty list is
    /// reported in the status bar rather than opening an empty overlay.
    pub fn open_sessions(&mut self, sessions: Vec<AgentSessionInfo>) {
        if sessions.is_empty() {
            self.popup = Popup::None;
            self.agent_error = Some("no agent sessions to switch to".to_string());
            return;
        }
        self.popup = Popup::Sessions {
            selected: 0,
            sessions,
        };
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
            push::CATALOG_UPDATED => {
                // The list is re-fetched by the session loop, which owns the client.
                self.catalog_dirty = true;
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
                let code = n
                    .params
                    .get("code")
                    .and_then(|v| v.as_i64())
                    .map(|c| c as i32);
                if let Some(sid) = sid {
                    self.agent_exit(sid, code);
                }
                if !self.sound_suppressed {
                    self.enqueue_sound(SoundCue::Attention);
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
            if !self.sound_suppressed {
                if let Some(cue) = cue_for_event(&ev) {
                    self.enqueue_sound(cue);
                }
            }
            self.events.push_back(ev);
        }
    }

    /// Queue a cue, keeping the buffer bounded.
    fn enqueue_sound(&mut self, cue: SoundCue) {
        const MAX_SOUND_CUES: usize = 64;
        if self.sound_cues.len() >= MAX_SOUND_CUES {
            self.sound_cues.remove(0);
        }
        self.sound_cues.push(cue);
    }

    /// Drain the pending sound cues (played by the session loop).
    pub fn take_sound_cues(&mut self) -> Vec<SoundCue> {
        std::mem::take(&mut self.sound_cues)
    }

    /// Toggle sound mute for this session.
    pub fn toggle_sound_muted(&mut self) {
        self.sound_muted = !self.sound_muted;
    }

    /// The event at the current selection (0 = newest).
    pub fn selected_event(&self) -> Option<&Event> {
        let n = self.events.len();
        if n == 0 {
            return None;
        }
        let offset = self.events_selected.min(n - 1);
        self.events.get(n - 1 - offset)
    }
}

/// Derive the sound cue for an event, if any.
///
/// Only `task_finished` is sounded (success/failure), so `task_completed` /
/// `task_failed` never double up; `task_started` is mapped but defaults off.
/// `task_awaiting_input` maps to the attention cue (defaults on).
fn cue_for_event(ev: &Event) -> Option<SoundCue> {
    match &ev.kind {
        EventKind::TaskFinished => {
            let success = ev.payload.get("success").and_then(|v| v.as_bool());
            Some(if success == Some(true) {
                SoundCue::TaskFinished
            } else {
                SoundCue::TaskFailed
            })
        }
        EventKind::TaskStarted => Some(SoundCue::TaskStarted),
        EventKind::TaskAwaitingInput => Some(SoundCue::AwaitingInput),
        _ => None,
    }
}

/// The rectangle occupied by a bordered table's data rows: inside the border and
/// below the single-line header.
pub(super) fn table_rows_area(area: Rect) -> Rect {
    Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(2),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(3),
    }
}

/// Map an absolute mouse position to a data-row index of a table.
///
/// `inner` is the data-rows rectangle, `offset` the first visible data-row index
/// recorded from `TableState::offset()`, `len` the row count. Clicks on the
/// border, the header, empty space below the rows, or past `len` return `None`.
fn row_index_at(inner: Rect, offset: usize, len: usize, row: u16, col: u16) -> Option<usize> {
    if len == 0 || inner.width == 0 || inner.height == 0 {
        return None;
    }
    if col < inner.x
        || col >= inner.x + inner.width
        || row < inner.y
        || row >= inner.y + inner.height
    {
        return None;
    }
    let index = offset + (row - inner.y) as usize;
    (index < len).then_some(index)
}

/// Clamp a selection moved by `delta` (negative = up) to `0..len`.
fn shift_index(current: usize, delta: i32, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let max = len - 1;
    if delta >= 0 {
        current.saturating_add(delta as usize).min(max)
    } else {
        current.saturating_sub(delta.unsigned_abs() as usize)
    }
}

/// Ctrl+Y: toggle keyboard focus between the embedded agent and favetto. Chosen
/// because it is not bound by common agent TUIs (opencode's defaults do not use it).
fn is_focus_toggle(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y'))
}

/// The kind of mouse report to emit.
#[derive(Clone, Copy)]
enum MouseKind {
    Press,
    Release,
    Motion,
}

/// Encode a mouse event as a terminal mouse report for the embedded agent.
///
/// Returns `None` when the agent has not enabled mouse reporting, when the event
/// falls outside the terminal area, or when it is not relevant to the active mode.
fn encode_mouse(mouse: MouseEvent, area: Rect, screen: &vt100::Screen) -> Option<Vec<u8>> {
    use vt100::{MouseProtocolEncoding, MouseProtocolMode};

    let mode = screen.mouse_protocol_mode();
    if mode == MouseProtocolMode::None {
        return None;
    }

    // Translate absolute screen coordinates into the agent's 1-based grid.
    let col = mouse.column.checked_sub(area.x)? + 1;
    let row = mouse.row.checked_sub(area.y)? + 1;
    if col > area.width || row > area.height {
        return None;
    }

    let (button, kind) = match mouse.kind {
        MouseEventKind::Down(b) => (button_code(b), MouseKind::Press),
        MouseEventKind::Up(_) => (0, MouseKind::Release),
        MouseEventKind::Drag(b) => (button_code(b), MouseKind::Motion),
        MouseEventKind::Moved => (0, MouseKind::Motion),
        MouseEventKind::ScrollUp => (64, MouseKind::Press),
        MouseEventKind::ScrollDown => (65, MouseKind::Press),
        MouseEventKind::ScrollLeft => (66, MouseKind::Press),
        MouseEventKind::ScrollRight => (67, MouseKind::Press),
    };

    // Respect the enabled mode: X10 reports presses only; motion needs a motion mode.
    match kind {
        MouseKind::Release if mode == MouseProtocolMode::Press => return None,
        MouseKind::Motion
            if !matches!(
                mode,
                MouseProtocolMode::ButtonMotion | MouseProtocolMode::AnyMotion
            ) =>
        {
            return None
        }
        _ => {}
    }

    let mut code = button;
    if mouse.modifiers.contains(KeyModifiers::SHIFT) {
        code |= 4;
    }
    if mouse.modifiers.contains(KeyModifiers::ALT) {
        code |= 8;
    }
    if mouse.modifiers.contains(KeyModifiers::CONTROL) {
        code |= 16;
    }
    if matches!(kind, MouseKind::Motion) {
        code |= 32;
    }

    if screen.mouse_protocol_encoding() == MouseProtocolEncoding::Sgr {
        let final_byte = if matches!(kind, MouseKind::Release) {
            'm'
        } else {
            'M'
        };
        Some(format!("\x1b[<{code};{col};{row}{final_byte}").into_bytes())
    } else {
        // Default / UTF-8 single-byte encoding; releases report button 3.
        let code = if matches!(kind, MouseKind::Release) {
            3
        } else {
            code
        };
        let cb = (32u16 + code).min(255) as u8;
        let cx = (32u16 + col).min(255) as u8;
        let cy = (32u16 + row).min(255) as u8;
        Some(vec![0x1b, b'[', b'M', cb, cx, cy])
    }
}

fn button_code(button: MouseButton) -> u16 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
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
