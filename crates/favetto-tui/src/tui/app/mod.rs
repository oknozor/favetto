//! TUI application state and the logic for folding server pushes into it.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use serde_json::Value;
use uuid::Uuid;

use base64::Engine as _;

use favetto_core::model::{
    AgentActivity, AgentCapabilities, AgentCatalogEntry, AgentSessionInfo, AgentUsage,
    AwaitingInputKind, Event, EventKind, InputReply, InputRequest, NotificationRecord, Schedule,
    Task, TaskStatus,
};
use favetto_core::rpc::{method, push, Notification};
use favetto_core::workflow::WorkflowInspect;
use favetto_providers::Provider;

use crate::tasks::TaskVar;

use super::sound::SoundCue;
use super::term::TerminalView;
use super::text_buffer::TextBuffer;
use super::theme::Theme;

mod input;
mod task_vars;
mod wizard;

#[cfg(test)]
mod tests;

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
    /// Fetch the runtime workflow view (`workflow.inspect`) for a root and show
    /// the runtime inspector overlay.
    OpenWorkflowInspect(Uuid),
    /// Cancel every non-terminal task in a workflow root (`workflow.cancel`).
    CancelWorkflow(Uuid),
    /// Retry a terminal task instance (`workflow.retry`).
    RetryWorkflowTask(Uuid),
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
    /// Answer the attached session's structured input request (`agents.reply`).
    Reply {
        session_id: String,
        request_id: String,
        reply: InputReply,
    },
    /// Cancel or retry a task, or cancel a whole workflow root, over RPC. The
    /// daemon pushes `task.updated` for every change, so the client never
    /// mutates a row optimistically.
    TaskCommand {
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
    /// Live runtime workflow inspector (`workflow.inspect`), opened with `i` on
    /// the Tasks tab for the selected task's root.
    WorkflowRuntime(WorkflowRuntime),
    /// The Ctrl+O session picker: the daemon's live/retained agent sessions,
    /// listed so the Agent panel can hop between concurrent runs.
    Sessions {
        selected: usize,
        sessions: Vec<AgentSessionInfo>,
    },
    /// The Ctrl+R reply prompt: answer a session's structured input request
    /// (`AgentActivity::Waiting { request }`) through `agents.reply`.
    Reply(ReplyPrompt),
    /// A destructive task action (`c` cancel / `C` cancel root / `r` retry)
    /// awaiting explicit confirmation before its RPC is submitted.
    Confirm(ConfirmPrompt),
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

/// State for the Ctrl+R prompt that answers a session's structured input request.
///
/// Option prompts select a row; a free-form request (empty `options`) edits
/// [`ReplyPrompt::input`]. Either way the typed reply is derived kind-aware by
/// [`ReplyPrompt::reply`].
pub struct ReplyPrompt {
    /// The daemon session id the reply is sent to.
    pub session_id: String,
    /// The precise prompt as reported by the agent's state channel.
    pub request: InputRequest,
    /// Selected option index (0 when the request is free-form).
    pub selected: usize,
    /// Free-form answer buffer, used when `request.options` is empty.
    pub input: TextBuffer,
}

impl ReplyPrompt {
    pub fn new(session_id: String, request: InputRequest) -> Self {
        Self {
            session_id,
            request,
            selected: 0,
            input: TextBuffer::default(),
        }
    }

    /// Whether the prompt offers selectable options (vs free-form text).
    pub fn has_options(&self) -> bool {
        !self.request.options.is_empty()
    }

    /// The typed reply for the current selection/buffer.
    pub fn reply(&self) -> InputReply {
        reply_for(&self.request, self.selected, self.input.value())
    }
}

/// A destructive task action awaiting explicit confirmation.
///
/// Holds the RPC to submit on confirm so the popup stays a pure view of an
/// intent already decided by [`App::handle_key`]. Nothing here mutates a task
/// row: the daemon's `task.updated` push is the single source of truth.
pub struct ConfirmPrompt {
    /// Overlay title, e.g. ` Cancel task `.
    pub title: &'static str,
    /// Consequence lines, one per rendered row.
    pub message: Vec<String>,
    /// The RPC submitted when the prompt is confirmed.
    pub method: &'static str,
    /// The RPC params submitted when the prompt is confirmed.
    pub params: Value,
}

/// State for the runtime workflow inspector (`i` on the Tasks tab), backed by
/// `workflow.inspect`: the root's state, the ready/running/failed/blocked
/// buckets, and a per-instance status/attempt/summary table.
///
/// The view is summary-only — `workflow.inspect` never carries `output` blobs.
pub struct WorkflowRuntime {
    /// The root being inspected (the selected task's `root_or_self`).
    pub root_id: Uuid,
    /// The latest fetched view, or `None` while the initial fetch is in flight.
    pub view: Option<WorkflowInspect>,
    /// Fetch/cancel/retry error surfaced in the overlay (a stale view stays put).
    pub error: Option<String>,
    /// Selected instance row (index into `view.tasks`).
    pub selected: usize,
    /// True while the initial `workflow.inspect` is in flight.
    pub loading: bool,
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
    /// Set when a `task.updated`/`event` push arrives while the runtime workflow
    /// inspector is open; the session loop re-fetches `workflow.inspect` and
    /// clears it.
    pub workflow_inspect_dirty: bool,

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
    /// True when the panel renders the structured state view for a headless run
    /// instead of the machine PTY's screen (which carries raw JSON events).
    pub agent_structured: bool,
    /// The last `agents.start` / `agents.attach` error, shown in the status bar.
    pub agent_error: Option<String>,
    /// The last task-command error (`tasks.cancel` / `tasks.retry` /
    /// `workflow.cancel`), shown in the status bar. Cleared by a later success.
    pub notice: Option<String>,
    /// Cached `agents.list` sessions keyed by daemon session id. The source of
    /// the Tasks-table Activity/Usage cells and the Ctrl+R reply target.
    pub agent_sessions: HashMap<String, AgentSessionInfo>,
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
            workflow_inspect_dirty: false,
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
            agent_structured: false,
            agent_error: None,
            notice: None,
            agent_sessions: HashMap::new(),
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

    /// The task currently selected on the Tasks tab, if any.
    pub fn selected_task(&self) -> Option<&Task> {
        self.tasks.get(self.tasks_selected)
    }

    /// Open the confirmation popup to cancel the selected task (`tasks.cancel`).
    ///
    /// No-op on a terminal task (nothing to cancel) or an empty list.
    fn confirm_cancel_task(&mut self) -> UiAction {
        let (name, attempt, id) = match self.selected_task() {
            Some(t) if !task_status_is_terminal(t.status) => (t.name.clone(), t.attempt, t.id),
            _ => return UiAction::None,
        };
        self.popup = Popup::Confirm(ConfirmPrompt {
            title: " Cancel task ",
            message: vec![
                format!("Cancel task '{name}' (attempt {attempt})?"),
                "A live agent is stopped; the task cannot be resumed.".to_string(),
            ],
            method: method::TASKS_CANCEL,
            params: serde_json::json!({ "id": id }),
        });
        UiAction::None
    }

    /// Open the confirmation popup to cancel every non-terminal task in the
    /// selected task's workflow root (`workflow.cancel`).
    ///
    /// Only offered for a spawned child, i.e. a task whose `root_id` is set; a
    /// directly-started task has no separate root to abandon.
    fn confirm_cancel_root(&mut self) -> UiAction {
        let (name, root_id) = match self
            .selected_task()
            .and_then(|t| t.root_id.map(|r| (&t.name, r)))
        {
            Some((name, root_id)) => (name.clone(), root_id),
            None => return UiAction::None,
        };
        self.popup = Popup::Confirm(ConfirmPrompt {
            title: " Cancel workflow ",
            message: vec![
                format!("Cancel every non-terminal task in workflow {root_id}?"),
                format!("Selected task: '{name}'"),
            ],
            method: method::WORKFLOW_CANCEL,
            params: serde_json::json!({ "root_id": root_id }),
        });
        UiAction::None
    }

    /// Open the confirmation popup to retry the selected terminal task
    /// (`tasks.retry`). No-op while the task is live: the daemon rejects a retry
    /// with an active run.
    fn confirm_retry_task(&mut self) -> UiAction {
        let (name, attempt, id) = match self.selected_task() {
            Some(t) if task_status_is_terminal(t.status) => (t.name.clone(), t.attempt, t.id),
            _ => return UiAction::None,
        };
        self.popup = Popup::Confirm(ConfirmPrompt {
            title: " Retry task ",
            message: vec![
                format!("Retry task '{name}' (attempt {attempt})?"),
                "A new attempt is recorded; prior runs are kept.".to_string(),
            ],
            method: method::TASKS_RETRY,
            params: serde_json::json!({ "id": id }),
        });
        UiAction::None
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

    /// Open the runtime workflow inspector for the selected Tasks-tab row's root
    /// (`root_id` when spawned, else the task's own id).
    fn open_workflow_inspect(&mut self) -> UiAction {
        let Some(task) = self.tasks.get(self.tasks_selected) else {
            return UiAction::None;
        };
        let root_id = task.root_or_self();
        self.popup = Popup::WorkflowRuntime(WorkflowRuntime {
            root_id,
            view: None,
            error: None,
            selected: 0,
            loading: true,
        });
        UiAction::OpenWorkflowInspect(root_id)
    }

    /// The root of the open runtime inspector, if any.
    pub fn workflow_inspect_root(&self) -> Option<Uuid> {
        match &self.popup {
            Popup::WorkflowRuntime(rt) => Some(rt.root_id),
            _ => None,
        }
    }

    /// Store a fetched `workflow.inspect` view, clamping the row selection.
    pub fn set_workflow_inspect(&mut self, view: WorkflowInspect) {
        let Popup::WorkflowRuntime(rt) = &mut self.popup else {
            return;
        };
        rt.selected = if view.tasks.is_empty() {
            0
        } else {
            rt.selected.min(view.tasks.len() - 1)
        };
        rt.root_id = view.root_id;
        rt.view = Some(view);
        rt.loading = false;
        rt.error = None;
    }

    /// Surface an error in the runtime inspector, leaving any stale view in place.
    pub fn set_workflow_inspect_error(&mut self, message: String) {
        if let Popup::WorkflowRuntime(rt) = &mut self.popup {
            rt.loading = false;
            rt.error = Some(message);
        }
    }

    /// The selected instance id in the runtime inspector, if any.
    pub fn workflow_runtime_selected_task(&self) -> Option<Uuid> {
        let Popup::WorkflowRuntime(rt) = &self.popup else {
            return None;
        };
        rt.view.as_ref()?.tasks.get(rt.selected).map(|t| t.id)
    }

    /// Request a refresh of the open runtime inspector. Called for pushes
    /// (`task.updated`/`event`) while the overlay is open; a no-op otherwise.
    pub fn mark_workflow_inspect_dirty(&mut self) {
        if matches!(&self.popup, Popup::WorkflowRuntime(_)) {
            self.workflow_inspect_dirty = true;
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
    /// attaching is what lets the answer reach its prompt. A headless run whose
    /// panel has no interactive view is rendered as a structured state view, so
    /// its raw JSON event stream is never shown.
    pub fn open_agent(&mut self, session: AgentSessionInfo, frame: &[u8]) {
        // Cache the session so its activity/usage show in the Tasks table and the
        // Ctrl+R reply sees the latest prompt without waiting for an `agents.list`.
        self.agent_sessions
            .insert(session.id.clone(), session.clone());
        let structured = session.headless && session.awaiting_input.is_none();
        let read_only = structured;
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
        self.agent_structured = structured;
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

    /// Replace the cached `agents.list` sessions (the Activity/Usage source for
    /// the Tasks table and the Ctrl+O picker).
    pub fn set_agent_sessions(&mut self, sessions: Vec<AgentSessionInfo>) {
        self.agent_sessions = sessions.into_iter().map(|s| (s.id.clone(), s)).collect();
    }

    /// Fold a `push::agent.state` frame into the cached session. A state frame
    /// for an unknown session is ignored: the next `agents.list` re-syncs it.
    pub fn apply_agent_state(
        &mut self,
        session_id: &str,
        activity: Option<AgentActivity>,
        usage: Option<AgentUsage>,
    ) {
        if let Some(session) = self.agent_sessions.get_mut(session_id) {
            session.activity = activity;
            session.usage = usage;
        }
    }

    /// The cached session attached to a task, preferring a running one. Used to
    /// join Activity/Usage into the Tasks table.
    pub fn task_session(&self, task_id: &str) -> Option<&AgentSessionInfo> {
        self.agent_sessions
            .values()
            .filter(|s| s.task_id.as_deref() == Some(task_id))
            .max_by_key(|s| (s.running, s.activity.is_some(), s.id.as_str()))
    }

    /// The structured input request the Ctrl+R keybind can answer: the attached
    /// session's `AgentActivity::Waiting`, or its structured `awaiting_input`
    /// fallback. A screen-detected prompt (no `request_id`) yields `None`, so
    /// sessions without a state channel keep their existing behaviour.
    pub fn pending_request(&self) -> Option<(String, InputRequest)> {
        let session = self.agent_sessions.get(self.agent_session_id.as_deref()?)?;
        if let Some(AgentActivity::Waiting { request }) = &session.activity {
            return Some((session.id.clone(), request.clone()));
        }
        let awaiting = session.awaiting_input.as_ref()?;
        let request_id = awaiting.request_id.clone()?;
        Some((
            session.id.clone(),
            InputRequest {
                id: request_id,
                kind: awaiting.kind,
                message: awaiting.message.clone(),
                options: awaiting.options.clone(),
                allow_always: awaiting.allow_always,
            },
        ))
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
            input: TextBuffer::default(),
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
            dir: TextBuffer::new(self.daemon_cwd.clone().unwrap_or_default()),
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

    /// Start a catalog task, opening the variable form when it declares `[[vars]]`.
    fn begin_catalog_task(&mut self, entry: &CatalogEntry) -> UiAction {
        if entry.vars.is_empty() {
            return UiAction::StartTask(entry.name.clone());
        }
        let values = entry
            .vars
            .iter()
            .map(|v| TextBuffer::new(v.default.clone().unwrap_or_default()))
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

    fn handle_form_key(&mut self, key: KeyEvent) -> UiAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Esc => {
                self.popup = Popup::None;
                UiAction::None
            }
            KeyCode::Backspace if alt => {
                if let Popup::Form(form) = &mut self.popup {
                    form.input.delete_word_before();
                }
                UiAction::None
            }
            KeyCode::Backspace => {
                if let Popup::Form(form) = &mut self.popup {
                    form.input.backspace();
                }
                UiAction::None
            }
            KeyCode::Delete => {
                if let Popup::Form(form) = &mut self.popup {
                    form.input.delete();
                }
                UiAction::None
            }
            KeyCode::Left if alt || ctrl => {
                if let Popup::Form(form) = &mut self.popup {
                    form.input.move_word_left();
                }
                UiAction::None
            }
            KeyCode::Left => {
                if let Popup::Form(form) = &mut self.popup {
                    form.input.move_left();
                }
                UiAction::None
            }
            KeyCode::Right if alt || ctrl => {
                if let Popup::Form(form) = &mut self.popup {
                    form.input.move_word_right();
                }
                UiAction::None
            }
            KeyCode::Right => {
                if let Popup::Form(form) = &mut self.popup {
                    form.input.move_right();
                }
                UiAction::None
            }
            KeyCode::Home => {
                if let Popup::Form(form) = &mut self.popup {
                    form.input.home();
                }
                UiAction::None
            }
            KeyCode::End => {
                if let Popup::Form(form) = &mut self.popup {
                    form.input.end();
                }
                UiAction::None
            }
            KeyCode::Char('a') if ctrl => {
                if let Popup::Form(form) = &mut self.popup {
                    form.input.home();
                }
                UiAction::None
            }
            KeyCode::Char('e') if ctrl => {
                if let Popup::Form(form) = &mut self.popup {
                    form.input.end();
                }
                UiAction::None
            }
            KeyCode::Char('u') if ctrl => {
                if let Popup::Form(form) = &mut self.popup {
                    form.input.delete_to_line_start();
                }
                UiAction::None
            }
            KeyCode::Char('k') if ctrl => {
                if let Popup::Form(form) = &mut self.popup {
                    form.input.delete_to_line_end();
                }
                UiAction::None
            }
            KeyCode::Char('w') if ctrl => {
                if let Popup::Form(form) = &mut self.popup {
                    form.input.delete_word_before();
                }
                UiAction::None
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                if let Popup::Form(form) = &mut self.popup {
                    form.input.insert_char(c);
                }
                UiAction::None
            }
            KeyCode::Enter => {
                let Popup::Form(form) = &mut self.popup else {
                    return UiAction::None;
                };
                form.values
                    .push(std::mem::take(&mut form.input).into_string());
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

    /// Fold a server push into app state. Events are deduped by their monotonic id.
    pub fn handle_notification(&mut self, n: Notification) {
        match n.method.as_str() {
            push::EVENT => {
                if let Ok(ev) = serde_json::from_value::<Event>(n.params) {
                    self.ingest_event(ev);
                }
                self.mark_workflow_inspect_dirty();
            }
            push::TASK_UPDATED => {
                if let Ok(t) = serde_json::from_value::<Task>(n.params) {
                    match self.tasks.iter().position(|x| x.id == t.id) {
                        Some(i) => self.tasks[i] = t,
                        None => self.tasks.insert(0, t),
                    }
                }
                self.mark_workflow_inspect_dirty();
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
            push::AGENT_STATE => {
                let sid = n.params.get("session_id").and_then(|v| v.as_str());
                let activity = n
                    .params
                    .get("activity")
                    .cloned()
                    .and_then(|v| serde_json::from_value::<AgentActivity>(v).ok());
                let usage = n
                    .params
                    .get("usage")
                    .cloned()
                    .and_then(|v| serde_json::from_value::<AgentUsage>(v).ok());
                if let Some(sid) = sid {
                    self.apply_agent_state(sid, activity, usage);
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

/// Map a selected option (or free-form value) to a typed [`InputReply`].
///
/// The mapping is kind-aware so a permission prompt answers with
/// `Once`/`Always`/`Reject`, a confirmation with `Confirmed`, and everything
/// else (choices, free-form text) with `Value`.
fn reply_for(request: &InputRequest, selected: usize, value: &str) -> InputReply {
    if request.options.is_empty() {
        return InputReply::Value {
            value: value.to_string(),
        };
    }
    let label = request.options.get(selected).cloned().unwrap_or_default();
    let lower = label.to_ascii_lowercase();
    match request.kind {
        AwaitingInputKind::Permission => {
            if lower.contains("always") || lower.contains("remember") {
                InputReply::Always
            } else if lower.contains("reject")
                || lower.contains("deny")
                || lower.contains("block")
                || lower.contains("cancel")
            {
                InputReply::Reject
            } else if request.allow_always && selected == 1 && request.options.len() > 2 {
                InputReply::Always
            } else {
                InputReply::Once
            }
        }
        AwaitingInputKind::Confirmation => {
            let confirmed = !(lower.contains("no")
                || lower.contains("deny")
                || lower.contains("reject")
                || lower.contains("cancel"));
            InputReply::Confirmed { confirmed }
        }
        AwaitingInputKind::Choice | AwaitingInputKind::Pinentry | AwaitingInputKind::Other => {
            InputReply::Value { value: label }
        }
    }
}

/// Short human label for an activity, used by the Tasks table and picker.
pub fn format_activity(activity: &AgentActivity) -> String {
    match activity {
        AgentActivity::Starting => "starting".to_string(),
        AgentActivity::Thinking => "thinking".to_string(),
        AgentActivity::Responding => "responding".to_string(),
        AgentActivity::Tool { name, .. } => format!("tool: {name}"),
        AgentActivity::Waiting { .. } => "waiting".to_string(),
        AgentActivity::Idle => "idle".to_string(),
        AgentActivity::Exited { code } => match code {
            Some(code) => format!("exited({code})"),
            None => "exited".to_string(),
        },
    }
}

/// Activity cell value, or an em dash when no state channel reported one.
pub fn activity_cell(activity: Option<&AgentActivity>) -> String {
    activity
        .map(format_activity)
        .unwrap_or_else(|| "—".to_string())
}

/// Compact `input/output` token counts plus cost, used by the Tasks table and
/// picker.
pub fn format_usage(usage: &AgentUsage) -> String {
    let mut out = format!(
        "{}/{} tok",
        format_tokens(usage.input_tokens),
        format_tokens(usage.output_tokens)
    );
    if let Some(cost) = usage.cost_usd {
        out.push_str(&format!(" ${cost:.4}"));
    }
    out
}

/// Usage cell value, or an em dash when nothing has been reported yet.
pub fn usage_cell(usage: Option<&AgentUsage>) -> String {
    match usage {
        Some(usage) if !usage.is_empty() => format_usage(usage),
        _ => "—".to_string(),
    }
}

fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
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

/// Whether a task lifecycle status is terminal: cancel/retry eligibility is
/// decided client-side from this, and re-validated by the daemon.
fn task_status_is_terminal(status: TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Succeeded | TaskStatus::Failed | TaskStatus::Cancelled
    )
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
            None => form
                .values
                .get(i)
                .map(|buffer| buffer.value().to_string())
                .unwrap_or_default(),
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
