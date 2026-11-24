//! The Agent panel's session switcher model.
//!
//! The menu is a pure merge of the live/retained `agents.list` sessions and the
//! on-demand `agents.history` records. It never starts, closes or replaces a
//! session: choosing a row only asks the caller to attach (`agents.attach`) or
//! load (`agents.load`) the explicitly selected Favetto-local id.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use favetto_wire::model::{
    AgentLoadState, AgentSessionHistoryEntry, AgentSessionInfo, AgentSessionState, Task,
};

/// The lifecycle label shown for one menu row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionMenuState {
    /// A live process is available to attach to.
    Active,
    /// A restore/launch is still in flight.
    Loading,
    /// The session ended and may be loaded.
    Ended,
    /// The launch, load or agent failed.
    Failed,
    /// The record cannot be opened (missing ACP id or unavailable agent).
    Unsupported,
}

/// What choosing one row does. Never `agents.start` or `agents.close`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionMenuAction {
    /// Reattach the live/retained in-process session.
    Attach,
    /// Restore an ended, loadable session by its Favetto-local id.
    Load,
    /// The row is informational; it cannot be opened.
    None,
}

/// One row in the Agent panel's session switcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMenuRow {
    /// Favetto-local routing key, never the provider's ACP id.
    pub id: String,
    /// Configured agent that owns the session.
    pub agent: String,
    /// Agent-reported title, when it reported one.
    pub title: Option<String>,
    /// Working directory, when known.
    pub cwd: Option<PathBuf>,
    /// Human-readable linked task name/id, when task-backed.
    pub task: Option<String>,
    /// Lifecycle label.
    pub state: SessionMenuState,
    /// What choosing the row does.
    pub action: SessionMenuAction,
    /// Why the row cannot be opened, when it cannot.
    pub detail: Option<String>,
    /// Whether the session is waiting for the user.
    pub awaiting_input: bool,
    /// Whether this is the conversation currently on screen.
    pub current: bool,
}

impl SessionMenuRow {
    /// Whether the row can become the displayed conversation.
    pub fn selectable(&self) -> bool {
        !self.current && self.action != SessionMenuAction::None
    }

    /// The short lifecycle label.
    pub fn state_label(&self) -> &'static str {
        if self.current {
            return "current";
        }
        if self.awaiting_input {
            return "awaiting input";
        }
        match self.state {
            SessionMenuState::Active => "running",
            SessionMenuState::Loading => "loading",
            SessionMenuState::Ended => "ended",
            SessionMenuState::Failed => "failed",
            SessionMenuState::Unsupported => "cannot load",
        }
    }

    /// The primary label. It never exposes the raw Favetto-local session id.
    pub fn display_title(&self) -> String {
        if let Some(title) = self
            .title
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            return title.to_string();
        }
        match self.cwd.as_deref().and_then(repo_label) {
            Some(repo) => format!("{} · {repo}", self.agent),
            None => self.agent.clone(),
        }
    }

    /// The secondary metadata line.
    pub fn location_label(&self) -> Option<String> {
        self.cwd
            .as_deref()
            .map(|cwd| cwd.display().to_string())
            .filter(|cwd| !cwd.is_empty())
    }

    /// Why Enter/click cannot open this row.
    pub fn blocked_reason(&self) -> Option<String> {
        if self.current {
            return Some("This session is already open.".to_string());
        }
        match self.action {
            SessionMenuAction::None => {
                Some(self.detail.clone().unwrap_or_else(|| match self.state {
                    SessionMenuState::Loading => {
                        "This session is still loading; wait for it to finish.".to_string()
                    }
                    SessionMenuState::Failed => {
                        "This session failed and cannot be loaded.".to_string()
                    }
                    SessionMenuState::Unsupported => "This session cannot be restored.".to_string(),
                    SessionMenuState::Active | SessionMenuState::Ended => {
                        "This session cannot be opened.".to_string()
                    }
                }))
            }
            SessionMenuAction::Attach | SessionMenuAction::Load => None,
        }
    }
}

fn repo_label(cwd: &Path) -> Option<String> {
    cwd.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .or_else(|| {
            let text = cwd.display().to_string();
            (!text.is_empty()).then_some(text)
        })
}

fn task_label(task_id: &str, tasks: &HashMap<String, &Task>) -> String {
    tasks
        .get(task_id)
        .map(|task| task.name.clone())
        .unwrap_or_else(|| format!("task {task_id}"))
}

fn row_from_live(
    session: &AgentSessionInfo,
    title: Option<String>,
    cwd: Option<PathBuf>,
    tasks: &HashMap<String, &Task>,
    current: Option<&str>,
) -> SessionMenuRow {
    let (state, action, detail) = if session.load_state == AgentLoadState::Failed {
        (
            SessionMenuState::Failed,
            SessionMenuAction::None,
            session.load_error.clone(),
        )
    } else if session.load_state == AgentLoadState::Loading {
        (SessionMenuState::Loading, SessionMenuAction::None, None)
    } else if session.running {
        (SessionMenuState::Active, SessionMenuAction::Attach, None)
    } else {
        (SessionMenuState::Ended, SessionMenuAction::Attach, None)
    };
    SessionMenuRow {
        id: session.id.clone(),
        agent: session.agent.clone(),
        title,
        cwd,
        task: session
            .task_id
            .as_deref()
            .map(|task_id| task_label(task_id, tasks)),
        state,
        action,
        detail,
        awaiting_input: session.awaiting_input.is_some(),
        current: current == Some(session.id.as_str()),
    }
}

fn row_from_history(
    entry: &AgentSessionHistoryEntry,
    tasks: &HashMap<String, &Task>,
    current: Option<&str>,
) -> SessionMenuRow {
    let (state, action, detail) = match entry.state {
        AgentSessionState::Active => (SessionMenuState::Active, SessionMenuAction::Attach, None),
        AgentSessionState::Loading => (SessionMenuState::Loading, SessionMenuAction::None, None),
        AgentSessionState::Ended if entry.loadable => {
            (SessionMenuState::Ended, SessionMenuAction::Load, None)
        }
        AgentSessionState::Ended => (
            SessionMenuState::Unsupported,
            SessionMenuAction::None,
            entry.load_error.clone(),
        ),
        AgentSessionState::Failed => (
            SessionMenuState::Failed,
            SessionMenuAction::None,
            entry.load_error.clone(),
        ),
    };
    SessionMenuRow {
        id: entry.id.clone(),
        agent: entry.agent.clone(),
        title: entry.title.clone(),
        cwd: Some(entry.cwd.clone()),
        task: entry
            .task_id
            .as_deref()
            .map(|task_id| task_label(task_id, tasks)),
        state,
        action,
        detail,
        awaiting_input: false,
        current: current == Some(entry.id.as_str()),
    }
}

/// Merge live/retained sessions first, then durable history newest first.
///
/// A history row already present in `agents.list` is not duplicated. The
/// daemon's history order is preserved.
pub fn merge_rows(
    live: &[AgentSessionInfo],
    history: &[AgentSessionHistoryEntry],
    tasks: &[Task],
    current: Option<&str>,
) -> Vec<SessionMenuRow> {
    let task_names: HashMap<String, &Task> = tasks
        .iter()
        .map(|task| (task.id.to_string(), task))
        .collect();
    let titles: HashMap<&str, &str> = history
        .iter()
        .filter_map(|entry| {
            entry
                .title
                .as_deref()
                .map(|title| (entry.id.as_str(), title))
        })
        .collect();
    let cwds: HashMap<&str, &PathBuf> = history
        .iter()
        .map(|entry| (entry.id.as_str(), &entry.cwd))
        .collect();
    let live_ids: HashSet<&str> = live.iter().map(|session| session.id.as_str()).collect();

    let mut rows: Vec<SessionMenuRow> = live
        .iter()
        .map(|session| {
            row_from_live(
                session,
                titles
                    .get(session.id.as_str())
                    .map(|title| title.to_string()),
                cwds.get(session.id.as_str()).map(|cwd| (*cwd).clone()),
                &task_names,
                current,
            )
        })
        .collect();
    rows.extend(
        history
            .iter()
            .filter(|entry| !live_ids.contains(entry.id.as_str()))
            .map(|entry| row_from_history(entry, &task_names, current)),
    );
    rows
}

/// The on-demand session switcher state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionMenu {
    /// Whether the menu is visible.
    open: bool,
    /// The highlighted row index.
    highlighted: usize,
    /// The last `agents.history` result. Kept while a refresh is in flight.
    pub history: Vec<AgentSessionHistoryEntry>,
    /// Whether an `agents.history` read is in flight.
    pub history_loading: bool,
    /// The last history failure, shown inline.
    pub history_error: Option<String>,
}

impl SessionMenu {
    /// Whether the menu is open.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// The highlighted row index.
    pub fn highlighted(&self) -> usize {
        self.highlighted
    }

    /// Open the menu and request a fresh history read from the caller.
    pub fn open(&mut self) {
        self.open = true;
        self.begin_history_load();
    }

    /// Close the menu without changing the conversation.
    pub fn close(&mut self) {
        self.open = false;
    }

    /// Begin a history read while keeping any previous rows visible.
    pub fn begin_history_load(&mut self) {
        self.history_loading = true;
        self.history_error = None;
    }

    /// Fold a history result in.
    pub fn set_history(&mut self, history: Vec<AgentSessionHistoryEntry>) {
        self.history = history;
        self.history_loading = false;
        self.history_error = None;
    }

    /// Record a history failure. Existing rows stay usable.
    pub fn fail_history(&mut self, message: String) {
        self.history_loading = false;
        self.history_error = Some(message);
    }

    /// Clamp or move the highlight onto the first selectable row.
    pub fn ensure_highlight(&mut self, rows: &[SessionMenuRow]) {
        if rows
            .get(self.highlighted)
            .is_some_and(|row| row.selectable())
        {
            return;
        }
        if let Some(index) = rows.iter().position(SessionMenuRow::selectable) {
            self.highlighted = index;
        } else {
            self.highlighted = self.highlighted.min(rows.len().saturating_sub(1));
        }
    }

    /// Move to the next/previous selectable row, wrapping at the ends.
    pub fn move_selection(&mut self, delta: isize, rows: &[SessionMenuRow]) {
        let selectable: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| row.selectable().then_some(index))
            .collect();
        if selectable.is_empty() {
            return;
        }
        let current = selectable
            .iter()
            .position(|index| *index == self.highlighted)
            .unwrap_or(0);
        let next = (current as isize + delta).rem_euclid(selectable.len() as isize) as usize;
        self.highlighted = selectable[next];
    }

    /// The highlighted row, when it is selectable.
    pub fn highlighted_row<'a>(&self, rows: &'a [SessionMenuRow]) -> Option<&'a SessionMenuRow> {
        rows.get(self.highlighted).filter(|row| row.selectable())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use favetto_wire::model::{
        AgentBackend, AgentLoadState, AwaitingInputKind, AwaitingInputReason,
    };
    use favetto_wire::Timestamp;
    use uuid::Uuid;

    fn session(id: &str, running: bool, task_id: Option<&str>) -> AgentSessionInfo {
        AgentSessionInfo {
            id: id.to_string(),
            agent: "opencode".to_string(),
            backend: AgentBackend::Acp,
            task_id: task_id.map(str::to_string),
            running,
            headless: false,
            session_id: None,
            awaiting_input: None,
            activity: None,
            usage: None,
            load_state: AgentLoadState::Ready,
            load_error: None,
            config_options: Vec::new(),
            modes: None,
            auth: None,
            mcp: None,
        }
    }

    fn history(
        id: &str,
        state: AgentSessionState,
        loadable: bool,
        title: Option<&str>,
        task_id: Option<&str>,
    ) -> AgentSessionHistoryEntry {
        AgentSessionHistoryEntry {
            id: id.to_string(),
            agent: "opencode".to_string(),
            title: title.map(str::to_string),
            cwd: PathBuf::from(format!("/code/{id}")),
            task_id: task_id.map(str::to_string),
            state,
            loadable,
            load_error: (!loadable).then(|| "no ACP session id was recorded".to_string()),
            created_at: Timestamp::now(),
            updated_at: Timestamp::now(),
        }
    }

    fn task(name: &str) -> Task {
        Task {
            id: Uuid::from_u128(42),
            name: name.to_string(),
            status: favetto_wire::model::TaskStatus::Running,
            attempt: 0,
            input: serde_json::Value::Null,
            output: None,
            dedupe_key: None,
            created_at: Timestamp::now(),
            started_at: None,
            finished_at: None,
            error: None,
            failure: None,
            session_id: None,
            session_title: None,
            parent_id: None,
            root_id: None,
            interactive: false,
            agent: None,
            provider: None,
            model: None,
            latest_usage: None,
        }
    }

    #[test]
    fn live_rows_come_first_and_history_is_deduplicated_newest_first() {
        let task = task("open_github_issue");
        let live = vec![session("live-a", true, Some(&task.id.to_string()))];
        let history = vec![
            history(
                "live-a",
                AgentSessionState::Active,
                true,
                Some("Live title"),
                None,
            ),
            history(
                "past-b",
                AgentSessionState::Ended,
                true,
                Some("Past title"),
                Some(&task.id.to_string()),
            ),
            history("past-c", AgentSessionState::Ended, true, None, None),
        ];

        let rows = merge_rows(&live, &history, std::slice::from_ref(&task), Some("live-a"));
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].id, "live-a");
        assert_eq!(rows[0].title.as_deref(), Some("Live title"));
        assert!(rows[0].current);
        assert_eq!(rows[0].state_label(), "current");
        assert_eq!(rows[1].id, "past-b");
        assert_eq!(rows[1].task.as_deref(), Some("open_github_issue"));
        assert_eq!(rows[1].action, SessionMenuAction::Load);
        assert_eq!(rows[2].id, "past-c");
        assert_eq!(rows[2].display_title(), "opencode · past-c");
    }

    #[test]
    fn failed_and_unsupported_history_rows_explain_why_they_cannot_open() {
        let history = vec![
            history("failed", AgentSessionState::Failed, false, None, None),
            history("unloadable", AgentSessionState::Ended, false, None, None),
            history("loading", AgentSessionState::Loading, false, None, None),
        ];
        let rows = merge_rows(&[], &history, &[], None);
        assert_eq!(rows[0].state_label(), "failed");
        assert!(!rows[0].selectable());
        assert!(rows[0]
            .blocked_reason()
            .unwrap()
            .contains("no ACP session id"));
        assert_eq!(rows[1].state_label(), "cannot load");
        assert!(!rows[1].selectable());
        assert_eq!(rows[2].state_label(), "loading");
        assert!(rows[2].blocked_reason().unwrap().contains("still loading"));
    }

    #[test]
    fn a_retained_live_session_attaches_and_awaiting_input_is_visible() {
        let mut waiting = session("waiting", true, None);
        waiting.awaiting_input = Some(AwaitingInputReason {
            kind: AwaitingInputKind::Permission,
            message: "Allow?".to_string(),
            request_id: Some("perm".to_string()),
            options: Vec::new(),
            allow_always: false,
            form: None,
        });
        let ended = session("retained", false, None);
        let rows = merge_rows(&[waiting, ended], &[], &[], None);
        assert_eq!(rows[0].state_label(), "awaiting input");
        assert_eq!(rows[0].action, SessionMenuAction::Attach);
        assert_eq!(rows[1].state_label(), "ended");
        assert_eq!(rows[1].action, SessionMenuAction::Attach);
    }

    #[test]
    fn navigation_skips_the_current_and_disabled_rows() {
        let history = vec![
            history("current", AgentSessionState::Active, true, None, None),
            history("failed", AgentSessionState::Failed, false, None, None),
            history("past", AgentSessionState::Ended, true, None, None),
        ];
        let rows = merge_rows(&[], &history, &[], Some("current"));
        let mut menu = SessionMenu::default();
        menu.ensure_highlight(&rows);
        assert_eq!(menu.highlighted(), 2);
        menu.move_selection(1, &rows);
        assert_eq!(menu.highlighted(), 2, "one selectable row wraps to itself");
        menu.move_selection(-1, &rows);
        assert_eq!(menu.highlighted(), 2);
    }

    #[test]
    fn a_current_history_row_is_not_selectable_even_when_loadable() {
        let history = vec![history("ended", AgentSessionState::Ended, true, None, None)];
        let rows = merge_rows(&[], &history, &[], Some("ended"));
        assert!(rows[0].current);
        assert!(!rows[0].selectable());
        assert_eq!(rows[0].action, SessionMenuAction::Load);
    }
}
