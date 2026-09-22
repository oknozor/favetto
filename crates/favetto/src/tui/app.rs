//! TUI application state and the logic for folding server pushes into it.

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
    pub dir: String,
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
    pub values: Vec<String>,
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
        if self.tasks.iter().any(|t| t.status == TaskStatus::Running) {
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
        // The embedded agent owns the keyboard as soon as the panel is opened.
        self.agent_capture = true;
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

    /// Route a keypress.
    ///
    /// Ctrl+Y toggles keyboard focus between the embedded agent and favetto. With
    /// the agent focused (the default when the panel is opened), every other key is
    /// forwarded to the agent PTY. With favetto focused, an open popup owns the
    /// keyboard, Ctrl+P toggles the menu, and the Agent tab handles its own keys.
    pub fn handle_key(&mut self, key: KeyEvent) -> UiAction {
        if is_focus_toggle(&key) {
            self.agent_capture = !self.agent_capture;
            return UiAction::None;
        }

        // Ctrl+P closes an open popup, or opens the menu under favetto focus. While
        // the agent captures keys it is forwarded to the agent instead.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('p') | KeyCode::Char('P'))
        {
            if matches!(self.popup, Popup::None) {
                if self.tab == Tab::Agent && self.agent_capture {
                    return self.forward_agent_key(&key);
                }
                self.popup = Popup::Menu { selected: 0 };
            } else {
                self.popup = Popup::None;
            }
            return UiAction::None;
        }

        // A popup owns the keyboard while it is open.
        match &self.popup {
            Popup::Form(_) => return self.handle_form_key(key.code),
            Popup::Menu { .. } => return self.handle_menu_key(key.code),
            Popup::Wizard(_) => return self.handle_wizard_key(key),
            Popup::TaskVars(_) => return self.handle_task_vars_key(key),
            Popup::Help { .. } => return self.handle_help_key(key.code),
            Popup::Workflow { .. } => return self.handle_workflow_key(key.code),
            Popup::None => {}
        }

        // Agent keyboard focus: everything else is forwarded to the PTY.
        if self.tab == Tab::Agent && self.agent_capture {
            return self.forward_agent_key(&key);
        }

        // `?` opens help when no popup owns the keyboard and the embedded agent is
        // not capturing. Popup routing and agent forwarding above take precedence.
        if key.code == KeyCode::Char('?') {
            self.popup = Popup::Help { scroll: 0 };
            return UiAction::None;
        }

        // `M` mutes/unmutes sound. Like `?`, it is literal inside a popup and is
        // forwarded to the agent while the embedded agent captures the keyboard.
        if key.code == KeyCode::Char('m') || key.code == KeyCode::Char('M') {
            self.toggle_sound_muted();
            return UiAction::None;
        }

        // `w` toggles the floating workflow graph, like `?`/`M`.
        if key.code == KeyCode::Char('w') || key.code == KeyCode::Char('W') {
            self.popup = Popup::Workflow {
                scroll: 0,
                hscroll: 0,
            };
            return UiAction::OpenWorkflow;
        }

        // `p` toggles the Catalog preview pane, but only on the Catalog tab.
        if self.tab == Tab::Catalog
            && (key.code == KeyCode::Char('p') || key.code == KeyCode::Char('P'))
        {
            self.toggle_catalog_preview();
            return UiAction::None;
        }

        if self.tab == Tab::Agent {
            return self.handle_agent_favetto_key(&key);
        }

        self.handle_normal_key(&key)
    }

    /// Keys for the Agent tab while favetto (not the agent) has the keyboard.
    fn handle_agent_favetto_key(&mut self, key: &KeyEvent) -> UiAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(key.code, KeyCode::Char('q') | KeyCode::Char('Q')) {
            self.tab = Tab::Tasks;
            return UiAction::None;
        }
        if ctrl && matches!(key.code, KeyCode::Char('n') | KeyCode::Char('N')) {
            if let Some(task_id) = self.agent_task_id.clone() {
                return UiAction::NewAgent(task_id);
            }
            return UiAction::None;
        }
        match key.code {
            KeyCode::Tab | KeyCode::Right => {
                self.next_tab();
                UiAction::None
            }
            KeyCode::Left | KeyCode::BackTab => {
                self.prev_tab();
                UiAction::None
            }
            KeyCode::Esc => {
                self.tab = Tab::Tasks;
                UiAction::None
            }
            _ => UiAction::None,
        }
    }

    /// Encode a keypress and forward it to the active agent session.
    fn forward_agent_key(&self, key: &KeyEvent) -> UiAction {
        let bytes = encode_key(key);
        if bytes.is_empty() {
            UiAction::None
        } else {
            UiAction::AgentInput(bytes)
        }
    }

    /// Handle mouse input.
    ///
    /// Favetto's own regions (the tab bar) are handled first so a click switches
    /// tabs. A click on a list row selects it, or runs the row's primary action
    /// when it was already selected. Any other click on the Agent tab is forwarded
    /// to the embedded agent as a terminal mouse report when the agent has enabled
    /// mouse reporting.
    pub fn handle_mouse(&mut self, mouse: MouseEvent) -> UiAction {
        // A popup owns the screen; ignore clicks so we don't select under it.
        if !matches!(self.popup, Popup::None) {
            return UiAction::None;
        }

        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
            for region in &self.click_regions {
                if mouse.row == region.row
                    && mouse.column >= region.col_start
                    && mouse.column < region.col_end
                {
                    let ClickAction::Tab(tab) = region.action;
                    self.tab = tab;
                    return UiAction::None;
                }
            }
            if let Some(action) = self.handle_list_click(mouse.column, mouse.row) {
                return action;
            }
        }

        // The wheel over a list scrolls that list's selection.
        let wheel_delta = match mouse.kind {
            MouseEventKind::ScrollUp => Some(-3),
            MouseEventKind::ScrollDown => Some(3),
            _ => None,
        };
        if let Some(delta) = wheel_delta {
            if self.scroll_list(mouse.column, mouse.row, delta) {
                return UiAction::None;
            }
        }

        // The wheel scrolls the Catalog preview when the pointer is over it.
        if self.tab == Tab::Catalog {
            if let Some(area) = self.catalog_preview_area {
                let over = mouse.column >= area.x
                    && mouse.column < area.x + area.width
                    && mouse.row >= area.y
                    && mouse.row < area.y + area.height;
                if over {
                    match mouse.kind {
                        MouseEventKind::ScrollUp => {
                            self.scroll_catalog_preview(-3);
                            return UiAction::None;
                        }
                        MouseEventKind::ScrollDown => {
                            self.scroll_catalog_preview(3);
                            return UiAction::None;
                        }
                        _ => {}
                    }
                }
            }
        }

        if self.tab == Tab::Agent && self.agent_running && self.agent_session_id.is_some() {
            if let Some(area) = self.agent_area {
                if let Some(bytes) = encode_mouse(mouse, area, self.term.screen()) {
                    return UiAction::AgentInput(bytes);
                }
            }
        }
        UiAction::None
    }

    /// The list geometry of the active tab, if that tab has a list.
    fn active_list_geometry(&self) -> Option<(Tab, ListGeometry)> {
        match self.tab {
            Tab::Tasks => Some((Tab::Tasks, self.tasks_geom)),
            Tab::Catalog => Some((Tab::Catalog, self.catalog_geom)),
            Tab::Events => Some((Tab::Events, self.events_geom)),
            Tab::Scheduler => Some((Tab::Scheduler, self.schedules_geom)),
            Tab::Notifications => Some((Tab::Notifications, self.notifications_geom)),
            Tab::Agent => None,
        }
    }

    /// Handle a left click on a list row. Returns `Some` when the click hit a row
    /// (the inner action may be `UiAction::None` for selection-only panels).
    fn handle_list_click(&mut self, col: u16, row: u16) -> Option<UiAction> {
        let (tab, geom) = self.active_list_geometry()?;
        let index = geom.row_at(row, col)?;
        Some(self.select_row(tab, index))
    }

    /// Select a data row and run the tab's primary action when it was already
    /// selected (mouse equivalent of Enter).
    fn select_row(&mut self, tab: Tab, index: usize) -> UiAction {
        match tab {
            Tab::Tasks => {
                if self.tasks.is_empty() {
                    return UiAction::None;
                }
                let already = self.tasks_selected == index;
                self.tasks_selected = index.min(self.tasks.len() - 1);
                if already {
                    if let Some(t) = self.tasks.get(index) {
                        return UiAction::OpenAgent(t.id.to_string());
                    }
                }
                UiAction::None
            }
            Tab::Catalog => {
                let row = self.catalog_rows().get(index).cloned();
                match row {
                    // Clicking a folder selects it and folds/unfolds it.
                    Some(CatalogRow::Folder { .. }) => {
                        self.select_catalog_row(index);
                        self.toggle_catalog_folder();
                    }
                    Some(CatalogRow::Task {
                        index: task_index, ..
                    }) => {
                        let already = self.catalog_selected == index;
                        self.select_catalog_row(index);
                        if already {
                            if let Some(entry) = self.catalog.get(task_index).cloned() {
                                return self.begin_catalog_task(&entry);
                            }
                        }
                    }
                    None => {}
                }
                UiAction::None
            }
            Tab::Events => {
                if !self.events.is_empty() {
                    self.events_selected = index.min(self.events.len() - 1);
                }
                UiAction::None
            }
            Tab::Scheduler => {
                if !self.schedules.is_empty() {
                    self.schedules_selected = index.min(self.schedules.len() - 1);
                }
                UiAction::None
            }
            Tab::Notifications => {
                if !self.notifications.is_empty() {
                    self.notifications_selected = index.min(self.notifications.len() - 1);
                }
                UiAction::None
            }
            Tab::Agent => UiAction::None,
        }
    }

    /// Scroll the active list's selection by `delta` rows when the pointer is over
    /// its rectangle. Returns `true` when the wheel was consumed.
    fn scroll_list(&mut self, col: u16, row: u16, delta: i32) -> bool {
        let Some((tab, geom)) = self.active_list_geometry() else {
            return false;
        };
        let over = col >= geom.inner.x
            && col < geom.inner.x + geom.inner.width
            && row >= geom.inner.y
            && row < geom.inner.y + geom.inner.height;
        if !over {
            return false;
        }
        match tab {
            Tab::Tasks => {
                self.tasks_selected = shift_index(self.tasks_selected, delta, self.tasks.len());
            }
            Tab::Catalog => {
                let len = self.catalog_rows().len();
                let next = shift_index(self.catalog_selected, delta, len);
                self.select_catalog_row(next);
            }
            Tab::Events => {
                self.events_selected = shift_index(self.events_selected, delta, self.events.len());
            }
            Tab::Scheduler => {
                self.schedules_selected =
                    shift_index(self.schedules_selected, delta, self.schedules.len());
            }
            Tab::Notifications => {
                self.notifications_selected =
                    shift_index(self.notifications_selected, delta, self.notifications.len());
            }
            Tab::Agent => {}
        }
        true
    }

    /// Keys while the Help overlay is open: `?`/`Esc` close, arrows and page keys
    /// scroll the content.
    fn handle_help_key(&mut self, code: KeyCode) -> UiAction {
        match code {
            KeyCode::Esc | KeyCode::Char('?') => self.popup = Popup::None,
            KeyCode::Up => {
                if let Popup::Help { scroll } = &mut self.popup {
                    *scroll = scroll.saturating_sub(1);
                }
            }
            KeyCode::PageUp => {
                if let Popup::Help { scroll } = &mut self.popup {
                    *scroll = scroll.saturating_sub(10);
                }
            }
            KeyCode::Down => {
                if let Popup::Help { scroll } = &mut self.popup {
                    *scroll = scroll.saturating_add(1);
                }
            }
            KeyCode::PageDown => {
                if let Popup::Help { scroll } = &mut self.popup {
                    *scroll = scroll.saturating_add(10);
                }
            }
            _ => {}
        }
        UiAction::None
    }

    /// Keys while the workflow overlay is open: `w`/`Esc` close, arrows and page
    /// keys scroll the cached graph (or DOT fallback) in both axes.
    fn handle_workflow_key(&mut self, code: KeyCode) -> UiAction {
        match code {
            KeyCode::Esc | KeyCode::Char('w') | KeyCode::Char('W') => self.popup = Popup::None,
            KeyCode::Up => {
                if let Popup::Workflow { scroll, .. } = &mut self.popup {
                    *scroll = scroll.saturating_sub(1);
                }
            }
            KeyCode::PageUp => {
                if let Popup::Workflow { scroll, .. } = &mut self.popup {
                    *scroll = scroll.saturating_sub(10);
                }
            }
            KeyCode::Down => {
                if let Popup::Workflow { scroll, .. } = &mut self.popup {
                    *scroll = scroll.saturating_add(1);
                }
            }
            KeyCode::PageDown => {
                if let Popup::Workflow { scroll, .. } = &mut self.popup {
                    *scroll = scroll.saturating_add(10);
                }
            }
            KeyCode::Left => {
                if let Popup::Workflow { hscroll, .. } = &mut self.popup {
                    *hscroll = hscroll.saturating_sub(1);
                }
            }
            KeyCode::Right => {
                if let Popup::Workflow { hscroll, .. } = &mut self.popup {
                    *hscroll = hscroll.saturating_add(1);
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
                self.open_form(selected)
            }
            _ => UiAction::None,
        }
    }

    fn open_form(&mut self, selected: usize) -> UiAction {
        // The first entry opens the one-shot wizard (which needs an async fetch of
        // the agent list first).
        if selected == 0 {
            return UiAction::OpenWizard;
        }
        let kind = match selected {
            1 => FormKind::AddTask,
            2 => FormKind::CreateSchedule,
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
        UiAction::None
    }

    /// Open the one-shot wizard with the configured agents.
    pub fn open_wizard(&mut self, agents: Vec<AgentCatalogEntry>) {
        let choices: Vec<(String, String)> = agents
            .iter()
            .filter(|a| !a.command.trim().is_empty())
            .map(|a| (a.name.clone(), a.name.clone()))
            .collect();
        let agent_caps: BTreeMap<String, AgentCapabilities> = agents
            .iter()
            .map(|a| (a.name.clone(), a.capabilities))
            .collect();
        let available: BTreeMap<String, bool> = agents
            .iter()
            .map(|a| (a.name.clone(), a.available))
            .collect();
        self.popup = Popup::Wizard(Wizard {
            step: WizardStep::Agent,
            selected: 0,
            choices,
            providers: Vec::new(),
            capabilities: AgentCapabilities::default(),
            agent_caps,
            available,
            agent: None,
            provider: None,
            model: None,
            dir: self.daemon_cwd.clone().unwrap_or_default(),
            loading: false,
            error: None,
        });
    }

    /// The agent currently selected in an open wizard, if any.
    pub fn wizard_agent(&self) -> Option<String> {
        match &self.popup {
            Popup::Wizard(w) => w.agent.clone(),
            _ => None,
        }
    }

    /// Populate the wizard with the configured providers and advance to the
    /// provider step, skipping straight to the directory step for an agent that
    /// has no provider catalog.
    pub fn wizard_set_providers(&mut self, providers: Vec<Provider>) {
        let Popup::Wizard(w) = &mut self.popup else {
            return;
        };
        w.choices = providers
            .iter()
            .map(|p| (p.name.clone(), p.id.clone()))
            .collect();
        w.providers = providers;
        w.loading = false;
        w.error = None;
        w.selected = 0;
        if !w.capabilities.providers {
            w.step = WizardStep::Dir;
            return;
        }
        w.step = WizardStep::Provider;
    }

    /// Surface an error in the wizard (e.g. the model list could not be fetched).
    pub fn wizard_error(&mut self, message: String) {
        if let Popup::Wizard(w) = &mut self.popup {
            w.loading = false;
            w.error = Some(message);
        }
    }

    fn handle_wizard_key(&mut self, key: KeyEvent) -> UiAction {
        // Take the wizard out so the borrow checker lets us mutate `self.popup`.
        let mut popup = std::mem::replace(&mut self.popup, Popup::None);
        let Popup::Wizard(w) = &mut popup else {
            self.popup = popup;
            return UiAction::None;
        };

        if key.code == KeyCode::Esc {
            return UiAction::None; // popup already cleared
        }

        let action = match key.code {
            KeyCode::Up => {
                if w.step == WizardStep::Agent {
                    w.move_selection(false);
                } else {
                    w.selected = w.selected.saturating_sub(1);
                }
                UiAction::None
            }
            KeyCode::Down => {
                if w.step == WizardStep::Agent {
                    w.move_selection(true);
                } else if !w.choices.is_empty() {
                    w.selected = (w.selected + 1).min(w.choices.len() - 1);
                }
                UiAction::None
            }
            KeyCode::Backspace if w.step == WizardStep::Dir => {
                w.dir.pop();
                UiAction::None
            }
            KeyCode::Char(c) if w.step == WizardStep::Dir => {
                w.dir.push(c);
                UiAction::None
            }
            KeyCode::Enter if !w.loading => match w.step {
                WizardStep::Agent => match w.choices.get(w.selected).cloned() {
                    Some((_, name)) => {
                        if w.available.get(&name) == Some(&false) {
                            w.error = Some(format!("agent '{name}' is not installed"));
                            UiAction::None
                        } else {
                            w.agent = Some(name.clone());
                            w.capabilities = w.agent_caps.get(&name).copied().unwrap_or_default();
                            w.error = None;
                            w.choices.clear();
                            if !w.capabilities.providers {
                                // No provider catalog: skip straight to the directory.
                                w.step = WizardStep::Dir;
                                w.selected = 0;
                                UiAction::None
                            } else {
                                w.loading = true;
                                UiAction::WizardLoadProviders
                            }
                        }
                    }
                    None => UiAction::None,
                },
                WizardStep::Provider => match w.choices.get(w.selected).cloned() {
                    Some((_, provider_id)) => {
                        w.provider = Some(provider_id.clone());
                        w.selected = 0;
                        if !w.capabilities.model_selection {
                            // No model selection: keep the agent default and go on.
                            w.choices.clear();
                            w.step = WizardStep::Dir;
                            UiAction::None
                        } else {
                            w.step = WizardStep::Model;
                            w.choices = w
                                .providers
                                .iter()
                                .find(|p| p.id == provider_id)
                                .map(|p| {
                                    p.models
                                        .iter()
                                        .map(|m| (m.name.clone(), m.id.clone()))
                                        .collect()
                                })
                                .unwrap_or_default();
                            UiAction::None
                        }
                    }
                    None => UiAction::None,
                },
                WizardStep::Model => match w.choices.get(w.selected).cloned() {
                    Some((_, model_id)) => {
                        w.model = Some(model_id);
                        w.step = WizardStep::Dir;
                        UiAction::None
                    }
                    None => UiAction::None,
                },
                WizardStep::Dir => {
                    let cwd = if w.dir.trim().is_empty() {
                        None
                    } else {
                        Some(w.dir.trim().to_string())
                    };
                    UiAction::WizardStart {
                        agent: w.agent.clone().unwrap_or_default(),
                        provider: w.provider.clone(),
                        model: w.model.clone(),
                        cwd,
                    }
                }
            },
            _ => UiAction::None,
        };

        // A completed wizard closes; otherwise keep it open.
        if !matches!(action, UiAction::WizardStart { .. }) {
            self.popup = popup;
        }
        action
    }

    /// Start a catalog task, opening the variable form when it declares `[[vars]]`.
    fn begin_catalog_task(&mut self, entry: &CatalogEntry) -> UiAction {
        if entry.vars.is_empty() {
            return UiAction::StartTask(entry.name.clone());
        }
        let values = entry
            .vars
            .iter()
            .map(|v| v.default.clone().unwrap_or_default())
            .collect();
        let choice_selected = entry
            .vars
            .iter()
            .map(|v| match (&v.choices, &v.default) {
                (Some(choices), Some(default)) => {
                    choices.iter().position(|c| c == default).unwrap_or(0)
                }
                _ => 0,
            })
            .collect();
        self.popup = Popup::TaskVars(TaskVarsForm {
            task: entry.name.clone(),
            vars: entry.vars.clone(),
            current: 0,
            values,
            choice_selected,
            error: None,
        });
        UiAction::None
    }

    fn handle_task_vars_key(&mut self, key: KeyEvent) -> UiAction {
        // Take the form out so the borrow checker lets us restore/clear `self.popup`.
        let mut popup = std::mem::replace(&mut self.popup, Popup::None);
        let Popup::TaskVars(form) = &mut popup else {
            self.popup = popup;
            return UiAction::None;
        };

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let last = form.vars.len().saturating_sub(1);
        let has_choices = form
            .vars
            .get(form.current)
            .is_some_and(|v| v.choices.is_some());

        let mut submit = false;
        match key.code {
            KeyCode::Esc => return UiAction::None, // popup already cleared
            KeyCode::Tab => {
                if form.current >= last {
                    submit = true;
                } else {
                    form.current += 1;
                }
            }
            KeyCode::BackTab => {
                form.current = form.current.saturating_sub(1);
            }
            KeyCode::Down if has_choices => {
                if let Some(choices) = form.vars[form.current].choices.as_ref() {
                    let selected = &mut form.choice_selected[form.current];
                    if !choices.is_empty() {
                        *selected = (*selected + 1).min(choices.len() - 1);
                    }
                }
            }
            KeyCode::Up if has_choices => {
                let selected = &mut form.choice_selected[form.current];
                *selected = selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Up => {
                if key.code == KeyCode::Down {
                    if form.current < last {
                        form.current += 1;
                    } else {
                        submit = true;
                    }
                } else {
                    form.current = form.current.saturating_sub(1);
                }
            }
            KeyCode::Enter if ctrl || alt => submit = true,
            KeyCode::Enter if form.vars.get(form.current).is_some_and(|v| v.multiline) => {
                if let Some(value) = form.values.get_mut(form.current) {
                    value.push('\n');
                }
            }
            KeyCode::Enter => {
                if form.current < last {
                    form.current += 1;
                } else {
                    submit = true;
                }
            }
            KeyCode::Backspace if !has_choices => {
                if let Some(value) = form.values.get_mut(form.current) {
                    value.pop();
                }
            }
            KeyCode::Char(c) if !ctrl && !alt && !has_choices => {
                if let Some(value) = form.values.get_mut(form.current) {
                    value.push(c);
                }
            }
            _ => {}
        }

        if !submit {
            self.popup = popup;
            return UiAction::None;
        }

        match submit_task_vars(form) {
            Ok(input) => {
                let name = form.task.clone();
                UiAction::StartTaskWithInput { name, input }
            }
            Err(e) => {
                form.error = Some(e);
                self.popup = popup;
                UiAction::None
            }
        }
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
                    Tab::Catalog => {
                        let prev = self.catalog_selected.saturating_sub(1);
                        self.select_catalog_row(prev);
                    }
                    Tab::Events => self.events_selected = self.events_selected.saturating_sub(1),
                    Tab::Scheduler => {
                        self.schedules_selected = self.schedules_selected.saturating_sub(1)
                    }
                    Tab::Notifications => {
                        self.notifications_selected = self.notifications_selected.saturating_sub(1)
                    }
                    _ => {}
                }
                UiAction::None
            }
            KeyCode::Down => {
                match self.tab {
                    Tab::Tasks => self.select_next(),
                    Tab::Catalog if !self.catalog.is_empty() => {
                        let next = self.catalog_selected + 1;
                        self.select_catalog_row(next);
                    }
                    Tab::Events if !self.events.is_empty() => {
                        self.events_selected =
                            (self.events_selected + 1).min(self.events.len() - 1);
                    }
                    Tab::Scheduler if !self.schedules.is_empty() => {
                        self.schedules_selected =
                            (self.schedules_selected + 1).min(self.schedules.len() - 1);
                    }
                    Tab::Notifications if !self.notifications.is_empty() => {
                        self.notifications_selected =
                            (self.notifications_selected + 1).min(self.notifications.len() - 1);
                    }
                    _ => {}
                }
                UiAction::None
            }
            KeyCode::PageUp => {
                match self.tab {
                    Tab::Events => {
                        self.events_selected = self.events_selected.saturating_sub(10);
                    }
                    Tab::Catalog => {
                        let page = self.catalog_preview_page();
                        self.scroll_catalog_preview(-page);
                    }
                    _ => {}
                }
                UiAction::None
            }
            KeyCode::PageDown => {
                match self.tab {
                    Tab::Events => {
                        if !self.events.is_empty() {
                            self.events_selected =
                                (self.events_selected + 10).min(self.events.len() - 1);
                        }
                    }
                    Tab::Catalog => {
                        let page = self.catalog_preview_page();
                        self.scroll_catalog_preview(page);
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
                    // Enter on a folder folds/unfolds it; only a task row starts.
                    if !self.toggle_catalog_folder() {
                        if let Some(index) = self.selected_catalog_task() {
                            if let Some(entry) = self.catalog.get(index).cloned() {
                                return self.begin_catalog_task(&entry);
                            }
                        }
                    }
                }
                UiAction::None
            }
            // Space folds/unfolds the selected Catalog folder (Left/Right already
            // switch tabs, so they would conflict with tree navigation).
            KeyCode::Char(' ') if self.tab == Tab::Catalog => {
                self.toggle_catalog_folder();
                UiAction::None
            }
            // `e` edits the selected Catalog task; a folder row is a no-op.
            KeyCode::Char('e') if self.tab == Tab::Catalog => {
                match self
                    .selected_catalog_task()
                    .and_then(|i| self.catalog.get(i))
                {
                    Some(entry) => UiAction::EditCatalog(entry.name.clone()),
                    None => UiAction::None,
                }
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
            push::CATALOG_UPDATED => {
                // The list is re-fetched by the session loop, which owns the client.
                self.catalog_dirty = true;
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

/// Build the `input` object for a completed variable form. Required empty fields
/// and failed type coercions return an error string that keeps the form open.
fn submit_task_vars(form: &TaskVarsForm) -> Result<Value, String> {
    let mut map = serde_json::Map::new();
    for (i, var) in form.vars.iter().enumerate() {
        let raw = match &var.choices {
            Some(choices) => choices
                .get(form.choice_selected.get(i).copied().unwrap_or(0))
                .cloned()
                .unwrap_or_default(),
            None => form.values.get(i).cloned().unwrap_or_default(),
        };
        if raw.is_empty() {
            if var.required {
                return Err(format!("{} is required", var.prompt));
            }
            continue;
        }
        let value = var
            .coerce(&raw)
            .map_err(|e| format!("{}: {e}", var.prompt))?;
        map.insert(var.name.clone(), value);
    }
    Ok(Value::Object(map))
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use favetto_core::model::{EventKind, TaskStatus};

    use crate::tasks::VarType;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    fn click(col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: col,
            row,
            modifiers: KeyModifiers::empty(),
        }
    }

    fn geom(inner: Rect, offset: usize, len: usize) -> ListGeometry {
        ListGeometry { inner, offset, len }
    }

    fn task(name: &str) -> Task {
        Task {
            id: uuid::Uuid::new_v4(),
            name: name.to_string(),
            status: TaskStatus::Pending,
            input: serde_json::json!({}),
            output: None,
            dedupe_key: None,
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
            error: None,
            session_id: None,
            session_title: None,
        }
    }

    fn catalog_entry(name: &str) -> CatalogEntry {
        CatalogEntry {
            name: name.to_string(),
            agent: None,
            provider: None,
            model: None,
            cwd: None,
            needs: None,
            vars: Vec::new(),
            prompt: String::new(),
        }
    }

    fn event(id: i64, kind: EventKind, payload: serde_json::Value) -> Event {
        Event {
            id,
            kind,
            payload,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn task_finished_enqueues_success_or_failure_cue() {
        let mut app = App::new();
        app.ingest_event(event(
            1,
            EventKind::TaskFinished,
            serde_json::json!({ "success": true }),
        ));
        assert_eq!(app.take_sound_cues(), vec![SoundCue::TaskFinished]);

        app.ingest_event(event(
            2,
            EventKind::TaskFinished,
            serde_json::json!({ "success": false }),
        ));
        assert_eq!(app.take_sound_cues(), vec![SoundCue::TaskFailed]);
    }

    #[test]
    fn only_task_finished_maps_to_finish_cues() {
        let mut app = App::new();
        app.ingest_event(event(1, EventKind::TaskCompleted, serde_json::json!({})));
        app.ingest_event(event(2, EventKind::TaskFailed, serde_json::json!({})));
        // `task_started` is mapped; whether it sounds is a player-level concern.
        app.ingest_event(event(3, EventKind::TaskStarted, serde_json::json!({})));
        assert_eq!(app.take_sound_cues(), vec![SoundCue::TaskStarted]);
    }

    #[test]
    fn agent_exit_push_enqueues_attention() {
        let mut app = App::new();
        app.handle_notification(Notification {
            method: push::AGENT_EXIT.to_string(),
            params: serde_json::json!({ "session_id": "s1", "code": 0 }),
        });
        assert_eq!(app.take_sound_cues(), vec![SoundCue::Attention]);
    }

    #[test]
    fn suppressed_replay_does_not_enqueue_cues() {
        let mut app = App::new();
        app.sound_suppressed = true;
        app.ingest_event(event(
            1,
            EventKind::TaskFinished,
            serde_json::json!({ "success": true }),
        ));
        assert!(app.take_sound_cues().is_empty());
        // The event is still recorded, just not sounded.
        assert_eq!(app.events.len(), 1);
    }

    #[test]
    fn m_key_toggles_mute_unless_agent_captures() {
        let mut app = App::new();
        app.handle_key(key(KeyCode::Char('m'), KeyModifiers::empty()));
        assert!(app.sound_muted);
        app.handle_key(key(KeyCode::Char('M'), KeyModifiers::empty()));
        assert!(!app.sound_muted);

        // Under agent capture the key goes to the PTY instead.
        app.tab = Tab::Agent;
        app.agent_capture = true;
        match app.handle_key(key(KeyCode::Char('m'), KeyModifiers::empty())) {
            UiAction::AgentInput(bytes) => assert_eq!(bytes, b"m".to_vec()),
            _ => panic!("expected AgentInput"),
        }
        assert!(!app.sound_muted);
    }

    #[test]
    fn agent_capture_forwards_keys_favetto_would_otherwise_use() {
        let mut app = App::new();
        app.tab = Tab::Agent;
        app.agent_capture = true;
        // Ctrl+P is normally the favetto menu, but the agent owns the keyboard.
        match app.handle_key(key(KeyCode::Char('p'), KeyModifiers::CONTROL)) {
            UiAction::AgentInput(bytes) => assert_eq!(bytes, vec![0x10]),
            _ => panic!("expected AgentInput"),
        }
    }

    #[test]
    fn focus_toggle_flips_and_is_never_forwarded() {
        let mut app = App::new();
        app.tab = Tab::Agent;
        app.agent_capture = true;
        assert!(matches!(
            app.handle_key(key(KeyCode::Char('y'), KeyModifiers::CONTROL)),
            UiAction::None
        ));
        assert!(!app.agent_capture);
        assert!(matches!(
            app.handle_key(key(KeyCode::Char('y'), KeyModifiers::CONTROL)),
            UiAction::None
        ));
        assert!(app.agent_capture);
    }

    #[test]
    fn favetto_focus_handles_agent_tab_shortcuts() {
        let mut app = App::new();
        app.tab = Tab::Agent;
        app.agent_capture = false;
        assert!(matches!(
            app.handle_key(key(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            UiAction::None
        ));
        assert_eq!(app.tab, Tab::Tasks);
    }

    #[test]
    fn question_mark_opens_and_closes_help() {
        let mut app = App::new();
        assert!(matches!(
            app.handle_key(key(KeyCode::Char('?'), KeyModifiers::empty())),
            UiAction::None
        ));
        assert!(matches!(app.popup, Popup::Help { scroll: 0 }));

        // `?` closes it again.
        app.handle_key(key(KeyCode::Char('?'), KeyModifiers::empty()));
        assert!(matches!(app.popup, Popup::None));

        // `Esc` also closes it.
        app.handle_key(key(KeyCode::Char('?'), KeyModifiers::empty()));
        app.handle_key(key(KeyCode::Esc, KeyModifiers::empty()));
        assert!(matches!(app.popup, Popup::None));

        // Arrows and page keys scroll the content.
        app.handle_key(key(KeyCode::Char('?'), KeyModifiers::empty()));
        app.handle_key(key(KeyCode::Down, KeyModifiers::empty()));
        assert!(matches!(app.popup, Popup::Help { scroll: 1 }));
        app.handle_key(key(KeyCode::PageDown, KeyModifiers::empty()));
        assert!(matches!(app.popup, Popup::Help { scroll: 11 }));
        app.handle_key(key(KeyCode::Up, KeyModifiers::empty()));
        assert!(matches!(app.popup, Popup::Help { scroll: 10 }));
        app.handle_key(key(KeyCode::PageUp, KeyModifiers::empty()));
        assert!(matches!(app.popup, Popup::Help { scroll: 0 }));
        // Scrolling up from the top saturates at zero.
        app.handle_key(key(KeyCode::Up, KeyModifiers::empty()));
        assert!(matches!(app.popup, Popup::Help { scroll: 0 }));
    }

    #[test]
    fn question_mark_is_forwarded_to_captured_agent() {
        let mut app = App::new();
        app.tab = Tab::Agent;
        app.agent_capture = true;
        match app.handle_key(key(KeyCode::Char('?'), KeyModifiers::empty())) {
            UiAction::AgentInput(bytes) => assert_eq!(bytes, b"?".to_vec()),
            _ => panic!("expected AgentInput"),
        }
        assert!(matches!(app.popup, Popup::None));
    }

    #[test]
    fn question_mark_opens_help_on_agent_tab_with_favetto_focus() {
        let mut app = App::new();
        app.tab = Tab::Agent;
        app.agent_capture = false;
        app.handle_key(key(KeyCode::Char('?'), KeyModifiers::empty()));
        assert!(matches!(app.popup, Popup::Help { scroll: 0 }));
    }

    #[test]
    fn question_mark_is_literal_inside_form() {
        let mut app = App::new();
        app.handle_key(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
        app.handle_key(key(KeyCode::Down, KeyModifiers::empty()));
        app.handle_key(key(KeyCode::Enter, KeyModifiers::empty()));
        let Popup::Form(form) = &app.popup else {
            panic!("expected form");
        };
        assert!(matches!(form.kind, FormKind::AddTask));

        app.handle_key(key(KeyCode::Char('?'), KeyModifiers::empty()));
        let Popup::Form(form) = &app.popup else {
            panic!("expected form");
        };
        assert_eq!(form.input, "?");
    }

    #[test]
    fn question_mark_does_not_open_over_menu() {
        let mut app = App::new();
        app.handle_key(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert!(matches!(app.popup, Popup::Menu { .. }));
        app.handle_key(key(KeyCode::Char('?'), KeyModifiers::empty()));
        assert!(matches!(app.popup, Popup::Menu { .. }));
    }

    #[test]
    fn workflow_key_opens_and_closes() {
        let mut app = App::new();
        assert!(matches!(
            app.handle_key(key(KeyCode::Char('w'), KeyModifiers::empty())),
            UiAction::OpenWorkflow
        ));
        assert!(matches!(app.popup, Popup::Workflow { scroll: 0, .. }));

        // `w` closes it again.
        app.handle_key(key(KeyCode::Char('w'), KeyModifiers::empty()));
        assert!(matches!(app.popup, Popup::None));

        // `Esc` also closes it.
        app.handle_key(key(KeyCode::Char('w'), KeyModifiers::empty()));
        app.handle_key(key(KeyCode::Esc, KeyModifiers::empty()));
        assert!(matches!(app.popup, Popup::None));
    }

    #[test]
    fn workflow_key_is_forwarded_to_captured_agent() {
        let mut app = App::new();
        app.tab = Tab::Agent;
        app.agent_capture = true;
        match app.handle_key(key(KeyCode::Char('w'), KeyModifiers::empty())) {
            UiAction::AgentInput(bytes) => assert_eq!(bytes, b"w".to_vec()),
            _ => panic!("expected AgentInput"),
        }
        assert!(matches!(app.popup, Popup::None));
    }

    #[test]
    fn workflow_key_is_literal_inside_another_popup() {
        let mut app = App::new();
        app.popup = Popup::Help { scroll: 0 };
        app.handle_key(key(KeyCode::Char('w'), KeyModifiers::empty()));
        assert!(matches!(app.popup, Popup::Help { scroll: 0 }));
    }

    #[test]
    fn set_workflow_stores_dot_and_path() {
        let mut app = App::new();
        assert!(app.workflow_dot.is_none());
        assert!(app.workflow_path.is_none());
        assert!(app.workflow_lines.is_none());
        let graph = crate::workflow::WorkflowGraph {
            nodes: vec![crate::workflow::WorkflowNode {
                name: "a".to_string(),
                scheduled: false,
                external: false,
            }],
            edges: Vec::new(),
        };
        app.set_workflow(
            "digraph workflow {}".to_string(),
            Some("/tmp/workflow.dot".to_string()),
            Some(graph),
        );
        assert_eq!(app.workflow_dot.as_deref(), Some("digraph workflow {}"));
        assert_eq!(app.workflow_path.as_deref(), Some("/tmp/workflow.dot"));
        assert!(app.workflow_lines.is_some());
        assert!(app.workflow_note.is_none());
    }

    #[test]
    fn set_workflow_without_graph_falls_back_to_dot() {
        let mut app = App::new();
        app.set_workflow("digraph workflow {}".to_string(), None, None);
        assert_eq!(app.workflow_dot.as_deref(), Some("digraph workflow {}"));
        assert!(app.workflow_lines.is_none());
        assert!(
            app.workflow_note.as_deref().unwrap_or("").contains("DOT"),
            "{:?}",
            app.workflow_note
        );
    }

    #[test]
    fn workflow_scroll_via_arrow_keys() {
        let mut app = App::new();
        app.popup = Popup::Workflow {
            scroll: 0,
            hscroll: 0,
        };
        app.handle_key(key(KeyCode::Down, KeyModifiers::empty()));
        assert!(matches!(app.popup, Popup::Workflow { scroll: 1, .. }));
        app.handle_key(key(KeyCode::PageDown, KeyModifiers::empty()));
        assert!(matches!(app.popup, Popup::Workflow { scroll: 11, .. }));
        app.handle_key(key(KeyCode::Up, KeyModifiers::empty()));
        assert!(matches!(app.popup, Popup::Workflow { scroll: 10, .. }));
        app.handle_key(key(KeyCode::PageUp, KeyModifiers::empty()));
        assert!(matches!(app.popup, Popup::Workflow { scroll: 0, .. }));
        // Scrolling up from the top saturates at zero.
        app.handle_key(key(KeyCode::Up, KeyModifiers::empty()));
        assert!(matches!(app.popup, Popup::Workflow { scroll: 0, .. }));

        // Horizontal scrolling uses the same saturation rules.
        app.handle_key(key(KeyCode::Left, KeyModifiers::empty()));
        assert!(matches!(app.popup, Popup::Workflow { hscroll: 0, .. }));
        app.handle_key(key(KeyCode::Right, KeyModifiers::empty()));
        app.handle_key(key(KeyCode::Right, KeyModifiers::empty()));
        assert!(matches!(app.popup, Popup::Workflow { hscroll: 2, .. }));
    }

    #[test]
    fn catalog_preview_scrolls_with_keys_and_wheel() {
        let mut app = App::new();
        app.tab = Tab::Catalog;
        app.catalog = vec![
            CatalogEntry {
                name: "a".to_string(),
                agent: None,
                provider: None,
                model: None,
                cwd: None,
                needs: None,
                vars: Vec::new(),
                prompt: String::new(),
            },
            CatalogEntry {
                name: "b".to_string(),
                agent: None,
                provider: None,
                model: None,
                cwd: None,
                needs: None,
                vars: Vec::new(),
                prompt: String::new(),
            },
        ];
        app.catalog_preview_max_scroll = 100;
        app.catalog_preview_area = Some(Rect {
            x: 40,
            y: 1,
            width: 40,
            height: 20,
        });

        // PageDown/PageUp move by the pane's inner height.
        app.handle_key(key(KeyCode::PageDown, KeyModifiers::empty()));
        assert_eq!(app.catalog_preview_scroll, 18);
        app.handle_key(key(KeyCode::PageUp, KeyModifiers::empty()));
        assert_eq!(app.catalog_preview_scroll, 0);

        // The wheel scrolls three lines at a time when over the pane.
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 50,
            row: 5,
            modifiers: KeyModifiers::empty(),
        });
        assert_eq!(app.catalog_preview_scroll, 3);

        // The scroll is clamped to the content.
        app.handle_key(key(KeyCode::PageDown, KeyModifiers::empty()));
        for _ in 0..20 {
            app.handle_key(key(KeyCode::PageDown, KeyModifiers::empty()));
        }
        assert_eq!(app.catalog_preview_scroll, 100);

        // Changing the selected task resets the scroll to the top.
        app.handle_key(key(KeyCode::Down, KeyModifiers::empty()));
        assert_eq!(app.catalog_preview_scroll, 0);
    }

    #[test]
    fn catalog_preview_toggle_key_flips_and_clears_area() {
        let mut app = App::new();
        app.tab = Tab::Catalog;
        app.catalog_preview_area = Some(Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        });
        assert!(app.catalog_preview_visible);

        app.handle_key(key(KeyCode::Char('p'), KeyModifiers::empty()));
        assert!(!app.catalog_preview_visible);
        assert!(app.catalog_preview_area.is_none());

        app.handle_key(key(KeyCode::Char('p'), KeyModifiers::empty()));
        assert!(app.catalog_preview_visible);

        // Uppercase works too.
        app.handle_key(key(KeyCode::Char('P'), KeyModifiers::empty()));
        assert!(!app.catalog_preview_visible);
    }

    #[test]
    fn catalog_preview_toggle_only_applies_to_catalog_tab() {
        let mut app = App::new();
        app.tab = Tab::Tasks;
        app.handle_key(key(KeyCode::Char('p'), KeyModifiers::empty()));
        assert!(app.catalog_preview_visible);

        // With the agent capturing keys, `p` is forwarded instead.
        app.tab = Tab::Agent;
        app.agent_capture = true;
        match app.handle_key(key(KeyCode::Char('p'), KeyModifiers::empty())) {
            UiAction::AgentInput(bytes) => assert_eq!(bytes, b"p".to_vec()),
            _ => panic!("expected AgentInput"),
        }
        assert!(app.catalog_preview_visible);
    }

    #[test]
    fn catalog_preview_target_is_none_while_hidden() {
        let mut app = App::new();
        app.tab = Tab::Catalog;
        app.set_catalog(vec![catalog_entry("demo")]);

        assert_eq!(app.catalog_preview_target().as_deref(), Some("demo"));
        app.catalog_preview_visible = false;
        assert_eq!(app.catalog_preview_target(), None);
    }

    #[test]
    fn catalog_preview_is_literal_inside_form() {
        let mut app = App::new();
        app.handle_key(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
        app.handle_key(key(KeyCode::Down, KeyModifiers::empty()));
        app.handle_key(key(KeyCode::Enter, KeyModifiers::empty()));
        let Popup::Form(form) = &app.popup else {
            panic!("expected form");
        };
        assert!(matches!(form.kind, FormKind::AddTask));

        app.handle_key(key(KeyCode::Char('p'), KeyModifiers::empty()));
        let Popup::Form(form) = &app.popup else {
            panic!("expected form");
        };
        assert_eq!(form.input, "p");
        assert!(app.catalog_preview_visible);
    }

    #[test]
    fn catalog_edit_key_maps_to_edit_action() {
        let mut app = App::new();
        app.tab = Tab::Catalog;
        app.set_catalog(vec![catalog_entry("demo")]);

        match app.handle_key(key(KeyCode::Char('e'), KeyModifiers::empty())) {
            UiAction::EditCatalog(name) => assert_eq!(name, "demo"),
            _ => panic!("expected EditCatalog"),
        }

        // The binding is Catalog-only: `e` does nothing on the Tasks tab.
        app.tab = Tab::Tasks;
        assert!(matches!(
            app.handle_key(key(KeyCode::Char('e'), KeyModifiers::empty())),
            UiAction::None
        ));
    }

    #[test]
    fn catalog_edit_key_is_noop_on_folder() {
        let mut app = App::new();
        app.tab = Tab::Catalog;
        app.set_catalog(nested_catalog());
        // Row 1 is the `pipelines` folder header.
        app.catalog_selected = 1;
        assert!(matches!(
            app.catalog_rows().get(1),
            Some(CatalogRow::Folder { .. })
        ));

        assert!(matches!(
            app.handle_key(key(KeyCode::Char('e'), KeyModifiers::empty())),
            UiAction::None
        ));
    }

    #[test]
    fn catalog_edit_key_literal_inside_form() {
        let mut app = App::new();
        app.handle_key(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
        app.handle_key(key(KeyCode::Down, KeyModifiers::empty()));
        app.handle_key(key(KeyCode::Enter, KeyModifiers::empty()));
        let Popup::Form(form) = &app.popup else {
            panic!("expected form");
        };
        assert!(matches!(form.kind, FormKind::AddTask));

        app.handle_key(key(KeyCode::Char('e'), KeyModifiers::empty()));
        let Popup::Form(form) = &app.popup else {
            panic!("expected form");
        };
        assert_eq!(form.input, "e");
    }

    #[test]
    fn catalog_updated_push_marks_catalog_dirty() {
        let mut app = App::new();
        assert!(!app.catalog_dirty);
        app.handle_notification(Notification {
            method: push::CATALOG_UPDATED.to_string(),
            params: serde_json::json!({}),
        });
        assert!(app.catalog_dirty);
    }

    #[test]
    fn set_catalog_clamps_selection_and_refreshes_preview() {
        let mut app = App::new();
        app.catalog_selected = 5;
        app.catalog_preview = Some(("gone".to_string(), "md".to_string()));

        app.set_catalog(vec![CatalogEntry {
            name: "a".to_string(),
            agent: None,
            provider: None,
            model: None,
            cwd: None,
            needs: None,
            vars: Vec::new(),
            prompt: String::new(),
        }]);

        assert_eq!(app.catalog_selected, 0);
        assert!(app.catalog_preview.is_none());

        app.set_catalog(Vec::new());
        assert_eq!(app.catalog_selected, 0);
    }

    /// A catalog with a root task, nested tasks, and a nested folder.
    fn nested_catalog() -> Vec<CatalogEntry> {
        vec![
            catalog_entry("b"),
            catalog_entry("pipelines/sub/deep"),
            catalog_entry("pipelines/triage"),
            catalog_entry("pipelines/plan"),
        ]
    }

    #[test]
    fn catalog_rows_groups_folders_and_tasks() {
        let mut app = App::new();
        app.set_catalog(nested_catalog());
        assert_eq!(
            app.catalog_rows(),
            vec![
                CatalogRow::Task { index: 0, depth: 0 },
                CatalogRow::Folder {
                    path: "pipelines".to_string(),
                    depth: 0,
                    collapsed: false,
                },
                CatalogRow::Task { index: 3, depth: 1 },
                CatalogRow::Folder {
                    path: "pipelines/sub".to_string(),
                    depth: 1,
                    collapsed: false,
                },
                CatalogRow::Task { index: 1, depth: 2 },
                CatalogRow::Task { index: 2, depth: 1 },
            ]
        );
    }

    #[test]
    fn collapsed_folder_hides_descendants_and_nested_folders() {
        let mut app = App::new();
        app.set_catalog(nested_catalog());

        // Collapsing the nested folder keeps its own row but hides its task.
        app.catalog_collapsed.insert("pipelines/sub".to_string());
        let rows = app.catalog_rows();
        assert!(rows.contains(&CatalogRow::Folder {
            path: "pipelines/sub".to_string(),
            depth: 1,
            collapsed: true,
        }));
        assert!(!rows
            .iter()
            .any(|r| matches!(r, CatalogRow::Task { index, .. } if *index == 1)));

        // Collapsing the parent hides the nested folder too.
        app.catalog_collapsed.insert("pipelines".to_string());
        assert_eq!(
            app.catalog_rows(),
            vec![
                CatalogRow::Task { index: 0, depth: 0 },
                CatalogRow::Folder {
                    path: "pipelines".to_string(),
                    depth: 0,
                    collapsed: true,
                },
            ]
        );
    }

    #[test]
    fn enter_on_folder_toggles_and_never_starts() {
        let mut app = App::new();
        app.tab = Tab::Catalog;
        app.set_catalog(vec![catalog_entry("pipelines/plan")]);
        assert_eq!(app.catalog_selected, 0);

        // Selected row is the folder: Enter folds, it never starts a task.
        assert!(matches!(
            app.handle_key(key(KeyCode::Enter, KeyModifiers::empty())),
            UiAction::None
        ));
        assert!(app.catalog_collapsed.contains("pipelines"));

        // A second Enter unfolds it.
        assert!(matches!(
            app.handle_key(key(KeyCode::Enter, KeyModifiers::empty())),
            UiAction::None
        ));
        assert!(!app.catalog_collapsed.contains("pipelines"));

        // On the task row, Enter starts the folder-qualified task.
        app.select_catalog_row(1);
        match app.handle_key(key(KeyCode::Enter, KeyModifiers::empty())) {
            UiAction::StartTask(name) => assert_eq!(name, "pipelines/plan"),
            _ => panic!("expected StartTask"),
        }
    }

    #[test]
    fn space_toggles_selected_folder_only() {
        let mut app = App::new();
        app.tab = Tab::Catalog;
        app.set_catalog(vec![catalog_entry("pipelines/plan")]);

        app.handle_key(key(KeyCode::Char(' '), KeyModifiers::empty()));
        assert!(app.catalog_collapsed.contains("pipelines"));
        app.handle_key(key(KeyCode::Char(' '), KeyModifiers::empty()));
        assert!(!app.catalog_collapsed.contains("pipelines"));

        // On a task row Space is a no-op.
        app.select_catalog_row(1);
        app.handle_key(key(KeyCode::Char(' '), KeyModifiers::empty()));
        assert!(app.catalog_collapsed.is_empty());
    }

    #[test]
    fn catalog_navigation_walks_folder_and_task_rows() {
        let mut app = App::new();
        app.tab = Tab::Catalog;
        app.set_catalog(nested_catalog());
        assert_eq!(app.catalog_selected, 0);

        for expected in [1, 2, 3, 4, 5, 5] {
            app.handle_key(key(KeyCode::Down, KeyModifiers::empty()));
            assert_eq!(app.catalog_selected, expected);
        }
        for expected in [4, 3, 2, 1, 0, 0] {
            app.handle_key(key(KeyCode::Up, KeyModifiers::empty()));
            assert_eq!(app.catalog_selected, expected);
        }

        // Selecting a folder clears the (stale) preview so the pane shows its
        // placeholder instead of the previously highlighted task.
        app.catalog_preview = Some(("pipelines/triage".to_string(), "md".to_string()));
        app.select_catalog_row(1); // the `pipelines` folder
        assert!(app.catalog_preview.is_none());
    }

    #[test]
    fn set_catalog_clamps_selection_against_visible_rows() {
        let mut app = App::new();
        app.catalog_selected = 99;
        app.set_catalog(vec![catalog_entry("a"), catalog_entry("pipelines/plan")]);
        // Three visible rows (`a`, `pipelines` folder, `pipelines/plan`) even
        // though the catalog holds two entries.
        assert_eq!(app.catalog_rows().len(), 3);
        assert_eq!(app.catalog_selected, 2);
    }

    #[test]
    fn catalog_preview_target_is_none_on_folder_row() {
        let mut app = App::new();
        app.tab = Tab::Catalog;
        app.set_catalog(vec![catalog_entry("pipelines/plan")]);

        app.select_catalog_row(0); // folder
        assert_eq!(app.catalog_preview_target(), None);

        app.select_catalog_row(1); // task
        assert_eq!(
            app.catalog_preview_target().as_deref(),
            Some("pipelines/plan")
        );
    }

    #[test]
    fn clicking_catalog_folder_toggles_it() {
        let mut app = App::new();
        app.tab = Tab::Catalog;
        app.set_catalog(vec![catalog_entry("pipelines/plan")]);
        let inner = Rect {
            x: 0,
            y: 2,
            width: 20,
            height: 5,
        };
        app.catalog_geom = geom(inner, 0, 2);

        // First click on the folder row (index 0) folds it.
        assert!(matches!(app.handle_mouse(click(5, 2)), UiAction::None));
        assert!(app.catalog_collapsed.contains("pipelines"));

        // A second click unfolds it.
        assert!(matches!(app.handle_mouse(click(5, 2)), UiAction::None));
        assert!(!app.catalog_collapsed.contains("pipelines"));
    }

    #[test]
    fn encode_mouse_sgr_reports_left_press() {
        let mut parser = vt100::Parser::new(24, 80, 0);
        parser.process(b"\x1b[?1000h\x1b[?1006h");
        let area = Rect {
            x: 2,
            y: 1,
            width: 80,
            height: 24,
        };
        let ev = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 7,
            row: 4,
            modifiers: KeyModifiers::empty(),
        };
        let bytes = encode_mouse(ev, area, parser.screen()).unwrap();
        assert_eq!(String::from_utf8(bytes).unwrap(), "\x1b[<0;6;4M");
    }

    #[test]
    fn encode_mouse_is_ignored_when_agent_disabled_reporting() {
        let parser = vt100::Parser::new(24, 80, 0);
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 24,
        };
        let ev = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 1,
            row: 1,
            modifiers: KeyModifiers::empty(),
        };
        assert!(encode_mouse(ev, area, parser.screen()).is_none());
    }

    #[test]
    fn mouse_mode_survives_daemon_formatted_frames() {
        // The daemon streams `state_formatted` frames (screen contents plus input
        // modes); the client parses them, so the agent's enabled mouse mode must be
        // preserved across the wire.
        let mut daemon = vt100::Parser::new(24, 80, 0);
        daemon.process(b"\x1b[?1000h\x1b[?1006h");
        let frame = daemon.screen().state_formatted();
        let mut client = vt100::Parser::new(24, 80, 0);
        client.process(&frame);
        assert_ne!(
            client.screen().mouse_protocol_mode(),
            vt100::MouseProtocolMode::None
        );
        assert_eq!(
            client.screen().mouse_protocol_encoding(),
            vt100::MouseProtocolEncoding::Sgr
        );
    }

    fn agent_entry(name: &str) -> AgentCatalogEntry {
        AgentCatalogEntry {
            name: name.to_string(),
            display_name: name.to_string(),
            command: "opencode".to_string(),
            default: false,
            available: true,
            capabilities: AgentCapabilities {
                interactive: true,
                providers: true,
                model_selection: true,
                ..Default::default()
            },
            sessions: Vec::new(),
        }
    }

    #[test]
    fn wizard_skips_provider_model_for_narrow_agent() {
        let mut app = App::new();
        app.daemon_cwd = Some("/code/che".to_string());
        let narrow = AgentCatalogEntry {
            name: "pi".to_string(),
            display_name: "Pi".to_string(),
            command: "pi".to_string(),
            default: false,
            available: true,
            capabilities: AgentCapabilities {
                interactive: true,
                ..Default::default()
            },
            sessions: Vec::new(),
        };
        app.open_wizard(vec![narrow]);

        // A narrow agent advances straight from Agent to Dir; no provider fetch.
        assert!(matches!(
            app.handle_key(key(KeyCode::Enter, KeyModifiers::empty())),
            UiAction::None
        ));
        {
            let Popup::Wizard(w) = &app.popup else {
                panic!("wizard")
            };
            assert_eq!(w.step, WizardStep::Dir);
            assert_eq!(w.agent.as_deref(), Some("pi"));
        }

        match app.handle_key(key(KeyCode::Enter, KeyModifiers::empty())) {
            UiAction::WizardStart {
                agent,
                provider,
                model,
                cwd,
            } => {
                assert_eq!(agent, "pi");
                assert!(provider.is_none());
                assert!(model.is_none());
                assert_eq!(cwd.as_deref(), Some("/code/che"));
            }
            _ => panic!("expected WizardStart"),
        }
    }

    #[test]
    fn wizard_walks_agent_provider_model_dir() {
        let mut app = App::new();
        app.daemon_cwd = Some("/code/che".to_string());
        app.open_wizard(vec![agent_entry("opencode")]);

        // Agent step -> request the provider catalog.
        assert!(matches!(
            app.handle_key(key(KeyCode::Enter, KeyModifiers::empty())),
            UiAction::WizardLoadProviders
        ));

        // Catalog arrives; the provider step lists display names.
        app.wizard_set_providers(vec![
            Provider {
                id: "deepseek".to_string(),
                name: "DeepSeek".to_string(),
                models: vec![
                    favetto_providers::Model {
                        id: "deepseek-v4-flash".to_string(),
                        name: "DeepSeek V4 Flash".to_string(),
                    },
                    favetto_providers::Model {
                        id: "deepseek-v4-pro".to_string(),
                        name: "DeepSeek V4 Pro".to_string(),
                    },
                ],
            },
            Provider {
                id: "mistral".to_string(),
                name: "Mistral".to_string(),
                models: vec![favetto_providers::Model {
                    id: "mistral-large".to_string(),
                    name: "Mistral Large".to_string(),
                }],
            },
        ]);
        {
            let Popup::Wizard(w) = &app.popup else {
                panic!("wizard")
            };
            assert_eq!(w.step, WizardStep::Provider);
            assert_eq!(
                w.choices,
                vec![
                    ("DeepSeek".to_string(), "deepseek".to_string()),
                    ("Mistral".to_string(), "mistral".to_string()),
                ]
            );
        }

        // Pick DeepSeek -> model step lists its models by display name.
        app.handle_key(key(KeyCode::Enter, KeyModifiers::empty()));
        {
            let Popup::Wizard(w) = &app.popup else {
                panic!("wizard")
            };
            assert_eq!(w.step, WizardStep::Model);
            assert_eq!(
                w.choices[0],
                (
                    "DeepSeek V4 Flash".to_string(),
                    "deepseek-v4-flash".to_string()
                )
            );
        }

        // Pick the first model -> directory step.
        app.handle_key(key(KeyCode::Enter, KeyModifiers::empty()));
        {
            let Popup::Wizard(w) = &app.popup else {
                panic!("wizard")
            };
            assert_eq!(w.step, WizardStep::Dir);
            assert_eq!(w.model.as_deref(), Some("deepseek-v4-flash"));
            assert_eq!(w.dir, "/code/che");
        }

        match app.handle_key(key(KeyCode::Enter, KeyModifiers::empty())) {
            UiAction::WizardStart {
                agent,
                provider,
                model,
                cwd,
            } => {
                assert_eq!(agent, "opencode");
                assert_eq!(provider.as_deref(), Some("deepseek"));
                assert_eq!(model.as_deref(), Some("deepseek-v4-flash"));
                assert_eq!(cwd.as_deref(), Some("/code/che"));
            }
            _ => panic!("expected WizardStart"),
        }
        assert!(matches!(app.popup, Popup::None));
    }

    #[test]
    fn row_index_at_maps_visible_rows_with_offset() {
        let inner = Rect {
            x: 0,
            y: 3,
            width: 20,
            height: 5,
        };
        assert_eq!(row_index_at(inner, 0, 10, 3, 0), Some(0));
        assert_eq!(row_index_at(inner, 0, 10, 7, 0), Some(4));
        assert_eq!(row_index_at(inner, 4, 10, 3, 0), Some(4));
        assert_eq!(row_index_at(inner, 4, 10, 7, 0), Some(8));
        // Offset pushes the index past `len`.
        assert_eq!(row_index_at(inner, 8, 10, 5, 0), None);
        // Empty table.
        assert_eq!(row_index_at(inner, 0, 0, 3, 0), None);
        // Last visible row is past a short list.
        assert_eq!(row_index_at(inner, 0, 3, 7, 0), None);
        // Column outside the data area (border/header rows are outside `inner`).
        assert_eq!(row_index_at(inner, 0, 10, 3, 20), None);
        assert_eq!(row_index_at(inner, 0, 10, 2, 0), None);
    }

    #[test]
    fn table_rows_area_offsets_border_and_header() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 24,
        };
        assert_eq!(
            table_rows_area(area),
            Rect {
                x: 1,
                y: 2,
                width: 78,
                height: 21,
            }
        );
    }

    #[test]
    fn clicking_tasks_row_selects_then_opens_agent() {
        let mut app = App::new();
        app.tab = Tab::Tasks;
        let tasks = vec![task("a"), task("b")];
        let second_id = tasks[1].id.to_string();
        app.tasks = tasks;
        app.tasks_selected = 0;
        let inner = Rect {
            x: 0,
            y: 2,
            width: 20,
            height: 5,
        };
        app.tasks_geom = geom(inner, 0, 2);

        // First click selects index 1 (no activation).
        assert!(matches!(app.handle_mouse(click(5, 3)), UiAction::None));
        assert_eq!(app.tasks_selected, 1);

        // Second click on the now-selected row opens its agent.
        match app.handle_mouse(click(5, 3)) {
            UiAction::OpenAgent(id) => assert_eq!(id, second_id),
            _ => panic!("expected OpenAgent"),
        }
    }

    #[test]
    fn clicking_catalog_row_selects_resets_scroll_then_starts_task() {
        let mut app = App::new();
        app.tab = Tab::Catalog;
        app.catalog = vec![catalog_entry("a"), catalog_entry("b")];
        app.catalog_selected = 1;
        app.catalog_preview_scroll = 7;
        let inner = Rect {
            x: 0,
            y: 2,
            width: 20,
            height: 5,
        };
        app.catalog_geom = geom(inner, 0, 2);

        assert!(matches!(app.handle_mouse(click(5, 2)), UiAction::None));
        assert_eq!(app.catalog_selected, 0);
        assert_eq!(app.catalog_preview_scroll, 0);

        match app.handle_mouse(click(5, 2)) {
            UiAction::StartTask(name) => assert_eq!(name, "a"),
            _ => panic!("expected StartTask"),
        }
    }

    #[test]
    fn clicking_events_row_selects_newest_first() {
        let mut app = App::new();
        app.tab = Tab::Events;
        for id in 1..=2 {
            app.ingest_event(Event {
                id,
                kind: EventKind::Synthetic,
                payload: serde_json::json!({}),
                created_at: Utc::now(),
            });
        }
        let inner = Rect {
            x: 0,
            y: 2,
            width: 20,
            height: 5,
        };
        app.events_geom = geom(inner, 0, 2);

        assert!(matches!(app.handle_mouse(click(5, 2)), UiAction::None));
        assert_eq!(app.events_selected, 0);
        assert_eq!(app.selected_event().map(|e| e.id), Some(2));

        assert!(matches!(app.handle_mouse(click(5, 3)), UiAction::None));
        assert_eq!(app.events_selected, 1);
        assert_eq!(app.selected_event().map(|e| e.id), Some(1));
    }

    #[test]
    fn clicking_scheduler_and_notifications_rows_selects_only() {
        let mut app = App::new();
        app.schedules = vec![Schedule {
            id: "s1".to_string(),
            cron: "0 * * * *".to_string(),
            task: "t".to_string(),
            input: serde_json::json!({}),
            enabled: true,
            last_run: None,
        }];
        app.notifications = vec![NotificationRecord {
            id: 1,
            channel: "cli".to_string(),
            subject: "hi".to_string(),
            body: "body".to_string(),
            status: "ok".to_string(),
            sent_at: Utc::now(),
        }];
        let inner = Rect {
            x: 0,
            y: 2,
            width: 20,
            height: 5,
        };

        app.tab = Tab::Scheduler;
        app.schedules_geom = geom(inner, 0, 1);
        assert!(matches!(app.handle_mouse(click(5, 2)), UiAction::None));
        assert_eq!(app.schedules_selected, 0);
        assert!(matches!(app.handle_mouse(click(5, 2)), UiAction::None));

        app.tab = Tab::Notifications;
        app.notifications_geom = geom(inner, 0, 1);
        assert!(matches!(app.handle_mouse(click(5, 2)), UiAction::None));
        assert_eq!(app.notifications_selected, 0);
        assert!(matches!(app.handle_mouse(click(5, 2)), UiAction::None));
    }

    #[test]
    fn click_is_ignored_while_popup_open() {
        let mut app = App::new();
        app.tab = Tab::Tasks;
        app.tasks = vec![task("a"), task("b")];
        app.tasks_selected = 0;
        let inner = Rect {
            x: 0,
            y: 2,
            width: 20,
            height: 5,
        };
        app.tasks_geom = geom(inner, 0, 2);
        app.popup = Popup::Menu { selected: 0 };

        assert!(matches!(app.handle_mouse(click(5, 3)), UiAction::None));
        assert_eq!(app.tasks_selected, 0);
    }

    #[test]
    fn wheel_over_list_moves_selection() {
        let mut app = App::new();
        app.tab = Tab::Tasks;
        app.tasks = (0..10).map(|i| task(&format!("t{i}"))).collect();
        let inner = Rect {
            x: 0,
            y: 2,
            width: 20,
            height: 5,
        };
        app.tasks_geom = geom(inner, 0, 10);

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 5,
            row: 3,
            modifiers: KeyModifiers::empty(),
        });
        assert_eq!(app.tasks_selected, 3);

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 5,
            row: 3,
            modifiers: KeyModifiers::empty(),
        });
        assert_eq!(app.tasks_selected, 0);

        // The wheel over the Catalog list still resets the preview scroll.
        app.tab = Tab::Catalog;
        app.catalog = vec![catalog_entry("a"), catalog_entry("b")];
        app.catalog_geom = geom(inner, 0, 2);
        app.catalog_preview_scroll = 5;
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 5,
            row: 3,
            modifiers: KeyModifiers::empty(),
        });
        assert_eq!(app.catalog_preview_scroll, 0);
    }

    #[test]
    fn click_on_border_or_below_rows_is_noop() {
        let mut app = App::new();
        app.tab = Tab::Tasks;
        app.tasks = vec![task("a"), task("b")];
        app.tasks_selected = 0;
        let inner = Rect {
            x: 1,
            y: 2,
            width: 20,
            height: 5,
        };
        app.tasks_geom = geom(inner, 0, 2);

        // Left border, header row, and below the rows all leave the selection be.
        for (col, row) in [(0, 3), (1, 1), (1, 7)] {
            assert!(matches!(app.handle_mouse(click(col, row)), UiAction::None));
            assert_eq!(app.tasks_selected, 0);
        }
    }

    #[test]
    fn keyboard_up_down_moves_scheduler_and_notification_selection() {
        let mut app = App::new();
        app.schedules = vec![
            Schedule {
                id: "s1".to_string(),
                cron: "0 * * * *".to_string(),
                task: "t".to_string(),
                input: serde_json::json!({}),
                enabled: true,
                last_run: None,
            },
            Schedule {
                id: "s2".to_string(),
                cron: "0 0 * * *".to_string(),
                task: "u".to_string(),
                input: serde_json::json!({}),
                enabled: false,
                last_run: None,
            },
        ];
        app.notifications = vec![
            NotificationRecord {
                id: 1,
                channel: "cli".to_string(),
                subject: "a".to_string(),
                body: String::new(),
                status: "ok".to_string(),
                sent_at: Utc::now(),
            },
            NotificationRecord {
                id: 2,
                channel: "cli".to_string(),
                subject: "b".to_string(),
                body: String::new(),
                status: "ok".to_string(),
                sent_at: Utc::now(),
            },
        ];

        app.tab = Tab::Scheduler;
        app.handle_key(key(KeyCode::Down, KeyModifiers::empty()));
        assert_eq!(app.schedules_selected, 1);
        app.handle_key(key(KeyCode::Up, KeyModifiers::empty()));
        assert_eq!(app.schedules_selected, 0);

        app.tab = Tab::Notifications;
        app.handle_key(key(KeyCode::Down, KeyModifiers::empty()));
        assert_eq!(app.notifications_selected, 1);
        app.handle_key(key(KeyCode::Up, KeyModifiers::empty()));
        assert_eq!(app.notifications_selected, 0);
    }

    fn vars_entry(name: &str) -> CatalogEntry {
        CatalogEntry {
            name: name.to_string(),
            agent: None,
            provider: None,
            model: None,
            cwd: None,
            needs: None,
            vars: vec![
                TaskVar {
                    name: "description".to_string(),
                    prompt: "Describe".to_string(),
                    default: Some("draft".to_string()),
                    required: true,
                    multiline: false,
                    var_type: VarType::String,
                    choices: None,
                },
                TaskVar {
                    name: "count".to_string(),
                    prompt: "Count".to_string(),
                    default: None,
                    required: false,
                    multiline: false,
                    var_type: VarType::Int,
                    choices: None,
                },
            ],
            prompt: String::new(),
        }
    }

    fn open_vars_form(app: &mut App, entry: CatalogEntry) {
        app.tab = Tab::Catalog;
        app.catalog = vec![entry];
        app.handle_key(key(KeyCode::Enter, KeyModifiers::empty()));
        assert!(matches!(app.popup, Popup::TaskVars(_)));
    }

    #[test]
    fn enter_on_catalog_entry_without_vars_returns_start_task() {
        let mut app = App::new();
        app.tab = Tab::Catalog;
        app.catalog = vec![catalog_entry("a")];
        match app.handle_key(key(KeyCode::Enter, KeyModifiers::empty())) {
            UiAction::StartTask(name) => assert_eq!(name, "a"),
            _ => panic!("expected StartTask"),
        }
        assert!(matches!(app.popup, Popup::None));
    }

    #[test]
    fn enter_on_vars_task_opens_form_with_defaults() {
        let mut app = App::new();
        open_vars_form(&mut app, vars_entry("issue"));
        let Popup::TaskVars(form) = &app.popup else {
            panic!("expected TaskVars");
        };
        assert_eq!(form.task, "issue");
        assert_eq!(form.values, vec!["draft".to_string(), String::new()]);
        assert_eq!(form.current, 0);
        assert!(form.error.is_none());
    }

    #[test]
    fn required_var_blocks_submit_until_filled() {
        let mut app = App::new();
        open_vars_form(&mut app, vars_entry("issue"));
        for _ in 0.."draft".len() {
            app.handle_key(key(KeyCode::Backspace, KeyModifiers::empty()));
        }
        assert!(matches!(
            app.handle_key(key(KeyCode::Enter, KeyModifiers::CONTROL)),
            UiAction::None
        ));
        match &app.popup {
            Popup::TaskVars(form) => assert!(form.error.is_some()),
            _ => panic!("form should stay open with an error"),
        }

        for c in "hello".chars() {
            app.handle_key(key(KeyCode::Char(c), KeyModifiers::empty()));
        }
        app.handle_key(key(KeyCode::Tab, KeyModifiers::empty()));
        match app.handle_key(key(KeyCode::Enter, KeyModifiers::CONTROL)) {
            UiAction::StartTaskWithInput { name, input } => {
                assert_eq!(name, "issue");
                assert_eq!(input, serde_json::json!({ "description": "hello" }));
            }
            _ => panic!("expected StartTaskWithInput"),
        }
        assert!(matches!(app.popup, Popup::None));
    }

    #[test]
    fn int_var_coerces_and_non_numeric_blocks() {
        let mut app = App::new();
        open_vars_form(&mut app, vars_entry("issue"));
        app.handle_key(key(KeyCode::Tab, KeyModifiers::empty()));
        app.handle_key(key(KeyCode::Char('x'), KeyModifiers::empty()));
        assert!(matches!(
            app.handle_key(key(KeyCode::Enter, KeyModifiers::empty())),
            UiAction::None
        ));
        assert!(matches!(&app.popup, Popup::TaskVars(form) if form.error.is_some()));

        app.handle_key(key(KeyCode::Backspace, KeyModifiers::empty()));
        app.handle_key(key(KeyCode::Char('3'), KeyModifiers::empty()));
        match app.handle_key(key(KeyCode::Enter, KeyModifiers::empty())) {
            UiAction::StartTaskWithInput { input, .. } => {
                assert_eq!(
                    input,
                    serde_json::json!({ "description": "draft", "count": 3 })
                );
            }
            _ => panic!("expected StartTaskWithInput"),
        }
    }

    #[test]
    fn multiline_enter_inserts_newline_and_ctrl_enter_submits() {
        let mut app = App::new();
        let mut entry = vars_entry("issue");
        entry.vars = vec![TaskVar {
            name: "body".to_string(),
            prompt: "Body".to_string(),
            default: None,
            required: true,
            multiline: true,
            var_type: VarType::String,
            choices: None,
        }];
        open_vars_form(&mut app, entry);
        app.handle_key(key(KeyCode::Char('a'), KeyModifiers::empty()));
        app.handle_key(key(KeyCode::Enter, KeyModifiers::empty()));
        app.handle_key(key(KeyCode::Char('b'), KeyModifiers::empty()));
        match &app.popup {
            Popup::TaskVars(form) => assert_eq!(form.values[0], "a\nb"),
            _ => panic!("form closed on Enter"),
        }
        match app.handle_key(key(KeyCode::Enter, KeyModifiers::CONTROL)) {
            UiAction::StartTaskWithInput { input, .. } => {
                assert_eq!(input, serde_json::json!({ "body": "a\nb" }));
            }
            _ => panic!("expected StartTaskWithInput"),
        }
    }

    #[test]
    fn esc_cancels_var_form_without_starting() {
        let mut app = App::new();
        open_vars_form(&mut app, vars_entry("issue"));
        assert!(matches!(
            app.handle_key(key(KeyCode::Esc, KeyModifiers::empty())),
            UiAction::None
        ));
        assert!(matches!(app.popup, Popup::None));
    }

    #[test]
    fn choices_arrows_change_selection_and_submit_selected() {
        let mut app = App::new();
        let mut entry = vars_entry("issue");
        entry.vars = vec![TaskVar {
            name: "flavor".to_string(),
            prompt: "Flavor".to_string(),
            default: None,
            required: false,
            multiline: false,
            var_type: VarType::String,
            choices: Some(vec!["vanilla".to_string(), "mint".to_string()]),
        }];
        open_vars_form(&mut app, entry);
        app.handle_key(key(KeyCode::Down, KeyModifiers::empty()));
        match &app.popup {
            Popup::TaskVars(form) => {
                assert_eq!(form.choice_selected, vec![1]);
                assert_eq!(
                    form.current, 0,
                    "Down must change the choice, not the field"
                );
            }
            _ => panic!("expected TaskVars"),
        }
        match app.handle_key(key(KeyCode::Enter, KeyModifiers::empty())) {
            UiAction::StartTaskWithInput { input, .. } => {
                assert_eq!(input, serde_json::json!({ "flavor": "mint" }));
            }
            _ => panic!("expected StartTaskWithInput"),
        }
    }

    #[test]
    fn clicking_vars_catalog_row_opens_form() {
        let mut app = App::new();
        app.tab = Tab::Catalog;
        app.catalog = vec![vars_entry("issue")];
        let inner = Rect {
            x: 0,
            y: 2,
            width: 20,
            height: 5,
        };
        app.catalog_geom = geom(inner, 0, 1);

        // First click selects the row; the second opens the variable form.
        assert!(matches!(app.handle_mouse(click(5, 2)), UiAction::None));
        assert!(matches!(app.handle_mouse(click(5, 2)), UiAction::None));
        assert!(matches!(app.popup, Popup::TaskVars(_)));
    }

    #[test]
    fn needs_animation_tracks_activity() {
        let mut app = App::new();
        app.conn = ConnState::Connected;
        assert!(!app.needs_animation());

        // Reconnecting always animates.
        app.conn = ConnState::Disconnected;
        assert!(app.needs_animation());

        // A running task animates.
        app.conn = ConnState::Connected;
        let mut running = task("r");
        running.status = TaskStatus::Running;
        app.tasks = vec![running];
        assert!(app.needs_animation());

        // A running agent animates.
        app.tasks.clear();
        app.agent_running = true;
        assert!(app.needs_animation());

        // A loading wizard animates, then settles.
        app.agent_running = false;
        app.open_wizard(vec![agent_entry("opencode")]);
        if let Popup::Wizard(w) = &mut app.popup {
            w.loading = true;
        }
        assert!(app.needs_animation());
        if let Popup::Wizard(w) = &mut app.popup {
            w.loading = false;
        }
        assert!(!app.needs_animation());
    }

    #[test]
    fn wizard_lists_unavailable_agent_but_skips_it() {
        let mut app = App::new();
        let mut unavailable = agent_entry("claude");
        unavailable.available = false;
        app.open_wizard(vec![agent_entry("opencode"), unavailable]);

        {
            let Popup::Wizard(w) = &app.popup else {
                panic!("wizard")
            };
            assert_eq!(
                w.choices,
                vec![
                    ("opencode".to_string(), "opencode".to_string()),
                    ("claude".to_string(), "claude".to_string()),
                ]
            );
            assert_eq!(w.available.get("claude"), Some(&false));
            assert_eq!(w.available.get("opencode"), Some(&true));
            assert_eq!(w.selected, 0);
        }

        // Down skips the unavailable `claude` entry and stays on `opencode`.
        app.handle_key(key(KeyCode::Down, KeyModifiers::empty()));
        {
            let Popup::Wizard(w) = &app.popup else {
                panic!("wizard")
            };
            assert_eq!(w.selected, 0);
        }

        // Even a forced selection on the unavailable entry cannot be entered.
        if let Popup::Wizard(w) = &mut app.popup {
            w.selected = 1;
        }
        assert!(matches!(
            app.handle_key(key(KeyCode::Enter, KeyModifiers::empty())),
            UiAction::None
        ));
        {
            let Popup::Wizard(w) = &app.popup else {
                panic!("wizard")
            };
            assert!(w.agent.is_none());
            assert_eq!(w.step, WizardStep::Agent);
            assert!(w.error.is_some());
        }
    }
}
